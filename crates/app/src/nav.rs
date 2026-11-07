//! Per-session navigation state and the pure rules for history paging and read acks.
use crate::settings::Settings;
use crate::view::{ChannelRow, MessageRow, TreeRow};
use rxcord_api::ApiError;
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_store::{Change, Store};
use std::collections::{HashMap, HashSet};
use tokio::time::{Duration, Instant};

pub const HISTORY_PAGE: u8 = 50;
pub const ACK_EVERY: Duration = Duration::from_secs(3);
pub const MAX_TABS: usize = 8;
/// Tree node id of "Direct Messages" (guild ids are never 0).
pub const DMS_NODE: Id = Id(0);

/// Open chat tabs in display order, with least-recently-used eviction.
#[derive(Default)]
pub struct Tabs {
    open: Vec<Id>,
    used: HashMap<Id, u64>,
    tick: u64,
}

impl Tabs {
    pub fn ids(&self) -> &[Id] {
        &self.open
    }

    /// Adds `id` (if new) and marks it used; returns the tab closed to stay within `MAX_TABS`.
    pub fn open(&mut self, id: Id) -> Option<Id> {
        self.tick += 1;
        self.used.insert(id, self.tick);
        if self.open.contains(&id) {
            return None;
        }
        self.open.push(id);
        if self.open.len() <= MAX_TABS {
            return None;
        }
        let lru = *self.open.iter().min_by_key(|t| self.used.get(t).copied().unwrap_or(0))?;
        self.remove(lru);
        Some(lru)
    }

    /// The tab shown after `id` closes: its right neighbour, else its left one.
    pub fn neighbour(&self, id: Id) -> Option<Id> {
        let i = self.open.iter().position(|t| *t == id)?;
        self.open.get(i + 1).or(i.checked_sub(1).and_then(|j| self.open.get(j))).copied()
    }

    /// True when `id` was open.
    pub fn remove(&mut self, id: Id) -> bool {
        self.used.remove(&id);
        let before = self.open.len();
        self.open.retain(|t| *t != id);
        self.open.len() != before
    }
}

#[derive(Debug, PartialEq)]
pub enum Ack {
    No,
    Now,
    At(Instant),
}

/// Ack only an unread channel whose newest message is on screen, at most once per `ACK_EVERY`.
pub fn ack_decision(unread: bool, newest_visible: bool, last: Option<Instant>, now: Instant) -> Ack {
    if !unread || !newest_visible {
        return Ack::No;
    }
    match last.map(|t| t + ACK_EVERY) {
        Some(due) if now < due => Ack::At(due),
        _ => Ack::Now,
    }
}

pub fn history_exhausted(got: usize) -> bool {
    got < HISTORY_PAGE as usize
}

/// `end` is the oldest stored id when a page last came back short (history exhausted).
pub fn should_load_older(front: Option<Id>, end: Option<Id>, in_flight: bool) -> bool {
    front.is_some() && front != end && !in_flight
}

/// Navigation state of one session.
#[derive(Default)]
pub struct Nav {
    pub tabs: Tabs,
    /// Expanded tree nodes: guild ids and `DMS_NODE`.
    pub expanded: HashSet<Id>,
    /// The active tab.
    pub selected: Option<Id>,
    pub at_bottom: bool,
    pub tree: Vec<TreeRow>, // what the UI's tree model holds
    pub tab_rows: Vec<ChannelRow>, // what the UI's tabs model holds
    pub shown: Vec<MessageRow>, // what the UI's message model holds
    pub shown_channel: Option<Id>,
    /// Bumped on READY: history responses from an older generation are ignored entirely.
    pub generation: u64,
    pub in_flight: HashSet<Id>,
    pub history_end: HashMap<Id, Id>,
    pub acked: HashMap<Id, Instant>,
    pub ack_later: HashSet<Id>,
    /// Channels whose history returned 403; they don't light the guild's unread dot.
    pub forbidden: HashSet<Id>,
    /// Voice members highlighted as talking (from the voice connection).
    pub speaking: HashSet<Id>,
    /// Voice settings and per-person volumes (not per session: a logout keeps them).
    pub settings: Settings,
}

impl Nav {
    /// Which UI models a batch of store changes touches: (tree and tabs, messages).
    pub fn affected(&self, changes: &[Change]) -> (bool, bool) {
        let (mut tree, mut msgs) = (false, false);
        for c in changes {
            match *c {
                Change::Me => {}
                Change::Messages(c) => msgs |= Some(c) == self.selected,
                Change::GuildList | Change::Channels { .. } | Change::DmList | Change::Unread(_) | Change::Voice(_) | Change::Call(_) => tree = true,
            }
        }
        (tree, msgs)
    }

    /// Activates channel `id`'s tab, opening it if needed (the previous one is trimmed, an evicted one
    /// forgotten); true when its history must be fetched.
    pub fn open(&mut self, s: &mut Store, id: Id) -> bool {
        if let Some(gone) = self.tabs.open(id) {
            self.forget(s, gone);
        }
        if let Some(prev) = self.selected.replace(id) {
            s.close_channel(prev);
        }
        self.at_bottom = true; // the UI scrolls a newly shown channel to the bottom
        s.open_channel(id)
    }

    /// `before` id for the next older page of `ch`, if one should be requested now.
    pub fn older_before(&self, s: &Store, ch: Id) -> Option<Id> {
        let front = s.messages(ch).front().map(|m| m.id);
        should_load_older(front, self.history_end.get(&ch).copied(), self.in_flight.contains(&ch)).then_some(front?)
    }

    /// READY reset the store: forget paging state; responses still in flight become stale.
    pub fn reset_history(&mut self) {
        self.generation += 1;
        self.in_flight.clear();
        self.history_end.clear();
    }

    /// Applies a history page. `None`: stale (older generation, or the store's front moved since the request).
    pub fn history_page(
        &mut self,
        s: &mut Store,
        ch: Id,
        generation: u64,
        before: Option<Id>,
        msgs: Vec<Message>,
    ) -> Option<Vec<Change>> {
        if !self.finish_request(ch, generation) || before.is_some_and(|b| Some(b) != s.messages(ch).front().map(|m| m.id)) {
            return None;
        }
        let got = msgs.len();
        let changes = s.prepend_history(ch, msgs);
        if history_exhausted(got)
            && let Some(f) = s.messages(ch).front()
        {
            self.history_end.insert(ch, f.id);
        }
        Some(changes)
    }

    /// Marks a history request of `ch` finished; false when it is from an older generation (ignore it).
    pub fn finish_request(&mut self, ch: Id, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.in_flight.remove(&ch);
        true
    }

    /// A history request of `ch` failed: the notice to show, if the channel is open with nothing loaded.
    pub fn history_failed(&mut self, s: &Store, ch: Id, generation: u64, e: &ApiError) -> Option<&'static str> {
        if matches!(e, ApiError::Http { status: 403, .. }) {
            self.forbidden.insert(ch);
        }
        if !self.finish_request(ch, generation) || self.selected != Some(ch) || !s.messages(ch).is_empty() {
            return None;
        }
        Some(match e {
            ApiError::Http { status: 403, .. } => "You don't have access to this channel",
            _ => "Couldn't load messages — click to retry",
        })
    }

    fn forget(&mut self, s: &mut Store, id: Id) {
        s.forget_channel(id);
        self.history_end.remove(&id);
    }

    /// Closes `id`'s tab. When it was the active one, nothing is selected afterwards and the tab to
    /// activate next is returned as `Some(next)`.
    pub fn close_tab(&mut self, s: &mut Store, id: Id) -> Option<Option<Id>> {
        let next = self.tabs.neighbour(id);
        if !self.tabs.remove(id) {
            return None;
        }
        self.forget(s, id);
        (self.selected == Some(id)).then(|| {
            self.selected = None;
            next
        })
    }

    /// Closes the tabs of channels that no longer exist (deleted, or their guild left). Same return as
    /// `close_tab` for the active tab.
    pub fn drop_missing_tabs(&mut self, s: &mut Store) -> Option<Option<Id>> {
        let gone: Vec<Id> = self.tabs.ids().iter().copied().filter(|c| s.channel(*c).is_none()).collect();
        let active = self.selected.filter(|c| gone.contains(c));
        for c in gone.into_iter().filter(|c| Some(*c) != active) {
            self.tabs.remove(c);
            self.forget(s, c);
        }
        self.close_tab(s, active?)
    }

    /// Decides (and records) an ack for `ch`. Needs the channel selected, loaded, on screen and the window focused.
    pub fn plan_ack(&mut self, s: &Store, ch: Id, focused: bool, now: Instant) -> Ack {
        if self.selected != Some(ch) || s.messages(ch).is_empty() {
            return Ack::No;
        }
        let a = ack_decision(s.is_unread(ch), focused && self.at_bottom, self.acked.get(&ch).copied(), now);
        match a {
            Ack::Now => drop(self.acked.insert(ch, now)),
            Ack::At(_) if !self.ack_later.insert(ch) => return Ack::No,
            _ => {}
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxcord_api::event::Event;
    use serde_json::json;

    fn store() -> Store {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [{"id": "100", "type": 0, "name": "a", "last_message_id": "9"}]}],
            "private_channels": [{"id": "200", "type": 1, "recipient_ids": ["2"]}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        s
    }

    fn msgs(ids: std::ops::Range<u64>) -> Vec<Message> {
        ids.rev() // API order: newest first
            .map(|i| {
                serde_json::from_value(json!({"id": i.to_string(), "channel_id": "100", "author": {"id": "2", "username": "b"},
                    "content": "x", "timestamp": "2026-11-07T12:00:00+00:00"}))
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn history_generation_and_exhaustion() {
        let (mut s, mut n) = (store(), Nav::default());
        let ch = Id(100);
        s.open_channel(ch);
        n.in_flight.insert(ch);
        let old = n.generation;
        n.reset_history(); // READY while the request was in flight
        n.in_flight.insert(ch); // the post-READY request
        assert_eq!(n.history_page(&mut s, ch, old, None, msgs(1..10)), None);
        assert!(n.in_flight.contains(&ch), "pre-READY response must not clear the new request's flag");
        assert!(!n.finish_request(ch, old));
        assert!(n.in_flight.contains(&ch));
        // current generation, short page: applied, history end recorded
        let c = n.history_page(&mut s, ch, n.generation, None, msgs(5..10)).unwrap();
        assert_eq!(c, vec![Change::Messages(ch)]);
        assert!(!n.in_flight.contains(&ch));
        assert_eq!(n.history_end.get(&ch), Some(&Id(5)));
        assert_eq!(n.older_before(&s, ch), None);
        // a page for a `before` that is no longer the front is stale
        n.history_end.clear();
        assert_eq!(n.older_before(&s, ch), Some(Id(5)));
        assert_eq!(n.history_page(&mut s, ch, n.generation, Some(Id(7)), msgs(1..5)), None);
        assert!(n.finish_request(ch, n.generation));
    }

    #[test]
    fn ack_needs_focus_and_bottom() {
        let (mut s, mut n) = (store(), Nav::default());
        let ch = Id(100);
        s.open_channel(ch);
        let t = Instant::now();
        n.selected = Some(ch);
        n.at_bottom = true;
        assert_eq!(n.plan_ack(&s, ch, true, t), Ack::No, "nothing loaded yet");
        s.prepend_history(ch, msgs(8..10));
        assert_eq!(n.plan_ack(&s, ch, false, t), Ack::No, "window unfocused");
        n.at_bottom = false;
        assert_eq!(n.plan_ack(&s, ch, true, t), Ack::No, "scrolled up");
        n.at_bottom = true;
        assert_eq!(n.plan_ack(&s, ch, true, t), Ack::Now);
        let t1 = t + Duration::from_secs(1);
        assert_eq!(n.plan_ack(&s, ch, true, t1), Ack::At(t + ACK_EVERY));
        assert_eq!(n.plan_ack(&s, ch, true, t1), Ack::No, "one deferred ack per channel");
        assert_eq!(n.plan_ack(&s, Id(200), true, t), Ack::No, "not selected");
    }

    #[test]
    fn affected_models() {
        let n = Nav { selected: Some(Id(100)), ..Default::default() };
        assert_eq!(n.affected(&[Change::Messages(Id(100))]), (false, true));
        assert_eq!(n.affected(&[Change::Messages(Id(101))]), (false, false));
        assert_eq!(n.affected(&[Change::Unread(Id(200))]), (true, false));
        assert_eq!(n.affected(&[Change::DmList, Change::Me]), (true, false));
        assert_eq!(n.affected(&[Change::Me]), (false, false));
    }

    #[test]
    fn tabs_lru_eviction_and_neighbours() {
        let mut t = Tabs::default();
        for i in 1..=MAX_TABS as u64 {
            assert_eq!(t.open(Id(i)), None);
        }
        assert_eq!(t.open(Id(1)), None, "reopen: no new tab, marks 1 used");
        assert_eq!(t.open(Id(99)), Some(Id(2)), "least recently used is closed");
        assert_eq!(t.ids().len(), MAX_TABS);
        assert_eq!(t.ids()[0], Id(1), "display order kept");
        assert_eq!(t.neighbour(Id(3)), Some(Id(4)));
        assert_eq!(t.neighbour(Id(99)), Some(Id(8)), "last tab: left neighbour");
        assert!(t.remove(Id(3)) && !t.remove(Id(3)));
        let mut one = Tabs::default();
        one.open(Id(5));
        assert_eq!(one.neighbour(Id(5)), None);
    }

    #[test]
    fn close_and_open_tabs() {
        let (mut s, mut n) = (store(), Nav::default());
        assert!(n.open(&mut s, Id(100)), "first open fetches");
        s.prepend_history(Id(100), msgs(1..3));
        n.open(&mut s, Id(200));
        assert!(!n.open(&mut s, Id(100)), "background tab kept its messages");
        assert_eq!(n.close_tab(&mut s, Id(200)), None, "inactive tab: no switch");
        assert_eq!(n.tabs.ids(), &[Id(100)]);
        n.open(&mut s, Id(200));
        assert_eq!(n.close_tab(&mut s, Id(200)), Some(Some(Id(100))), "active tab: left neighbour next");
        assert_eq!(n.selected, None);
        n.open(&mut s, Id(100));
        assert_eq!(n.close_tab(&mut s, Id(100)), Some(None), "last tab: nothing to show");
        assert!(s.messages(Id(100)).is_empty(), "closing forgets loaded messages");
        assert_eq!(n.close_tab(&mut s, Id(100)), None, "not open");
    }

    #[test]
    fn forbidden_recorded_on_403() {
        let (s, mut n) = (store(), Nav::default());
        let e = |status| ApiError::Http { status, body: String::new() };
        n.history_failed(&s, Id(100), n.generation, &e(500));
        assert!(n.forbidden.is_empty());
        n.history_failed(&s, Id(100), n.generation, &e(403)); // even when not selected
        assert!(n.forbidden.contains(&Id(100)));
    }

    #[test]
    fn deleted_tabs_closed() {
        let mut s = store();
        let mut n = Nav::default();
        n.open(&mut s, Id(200));
        n.open(&mut s, Id(100));
        assert_eq!(n.drop_missing_tabs(&mut s), None);
        let Some(c) = s.channel(Id(100)).cloned() else { panic!() };
        s.apply(&Event::ChannelDelete(c));
        assert_eq!(n.drop_missing_tabs(&mut s), Some(Some(Id(200))));
        assert_eq!((n.selected, n.tabs.ids()), (None, &[Id(200)][..]));
        n.open(&mut s, Id(200));
        let Some(c) = s.channel(Id(200)).cloned() else { panic!() };
        s.apply(&Event::ChannelDelete(c));
        assert_eq!(n.drop_missing_tabs(&mut s), Some(None), "last tab gone");
    }

    #[test]
    fn ack_rules() {
        let t = Instant::now();
        assert_eq!(ack_decision(false, true, None, t), Ack::No);
        assert_eq!(ack_decision(true, false, None, t), Ack::No);
        assert_eq!(ack_decision(true, true, None, t), Ack::Now);
        let s1 = t + Duration::from_secs(1);
        assert_eq!(ack_decision(true, true, Some(t), s1), Ack::At(t + ACK_EVERY));
        assert_eq!(ack_decision(true, true, Some(t), t + ACK_EVERY), Ack::Now);
    }

    #[test]
    fn load_older_rules() {
        assert!(should_load_older(Some(Id(5)), None, false));
        assert!(!should_load_older(Some(Id(5)), None, true)); // in flight
        assert!(!should_load_older(None, None, false)); // nothing to page from
        assert!(!should_load_older(Some(Id(5)), Some(Id(5)), false)); // exhausted
        assert!(should_load_older(Some(Id(5)), Some(Id(9)), false)); // end evicted/stale
        assert!(history_exhausted(49) && !history_exhausted(50));
    }
}
