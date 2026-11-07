//! Per-session navigation state and the pure rules for history paging and read acks.
use crate::view::MessageRow;
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_store::{Change, Store};
use std::collections::{HashMap, HashSet};
use tokio::time::{Duration, Instant};

pub const HISTORY_PAGE: u8 = 50;
pub const ACK_EVERY: Duration = Duration::from_secs(3);

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
    pub guild: Option<Id>, // None = Direct Messages
    pub selected: Option<Id>,
    pub at_bottom: bool,
    pub shown: Vec<MessageRow>, // what the UI's message model holds
    pub shown_channel: Option<Id>,
    /// Bumped on READY: history responses from an older generation are ignored entirely.
    pub generation: u64,
    pub in_flight: HashSet<Id>,
    pub history_end: HashMap<Id, Id>,
    pub acked: HashMap<Id, Instant>,
    pub ack_later: HashSet<Id>,
}

impl Nav {
    /// Which UI models a batch of store changes touches: (guilds, channels, messages).
    pub fn affected(&self, s: &Store, changes: &[Change]) -> (bool, bool, bool) {
        let (mut guilds, mut chans, mut msgs) = (false, false, false);
        for c in changes {
            match *c {
                Change::Me => {}
                Change::GuildList => guilds = true,
                Change::Channels { guild } => chans |= self.guild.is_some() && (guild.is_none() || guild == self.guild),
                Change::DmList => chans |= self.guild.is_none(),
                Change::Messages(c) => msgs |= Some(c) == self.selected,
                Change::Unread(c) => {
                    guilds = true;
                    chans |= s.channel(c).is_some_and(|c| c.guild_id == self.guild);
                }
            }
        }
        (guilds, chans, msgs)
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
        let s = store();
        let mut n = Nav { guild: Some(Id(10)), selected: Some(Id(100)), ..Default::default() };
        assert_eq!(n.affected(&s, &[Change::Messages(Id(100))]), (false, false, true));
        assert_eq!(n.affected(&s, &[Change::Messages(Id(101))]), (false, false, false));
        assert_eq!(n.affected(&s, &[Change::Unread(Id(100))]), (true, true, false));
        assert_eq!(n.affected(&s, &[Change::Unread(Id(200))]), (true, false, false));
        assert_eq!(n.affected(&s, &[Change::DmList]), (false, false, false));
        assert_eq!(n.affected(&s, &[Change::Channels { guild: None }]), (false, true, false));
        n.guild = None;
        assert_eq!(n.affected(&s, &[Change::DmList, Change::Unread(Id(200))]), (true, true, false));
        assert_eq!(n.affected(&s, &[Change::Channels { guild: Some(Id(10)) }]), (false, false, false));
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
