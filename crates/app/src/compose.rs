//! Composer rules: what may be sent, and which message "Edit" / Up-arrow opens.
use rxcord_api::event::Event;
use rxcord_api::id::Id;
use rxcord_api::model::{Message, MessageUpdate};
use rxcord_api::nonce::make_nonce;
use rxcord_store::{Pending, Store};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Discord's content limit, in chars.
pub const MAX_CHARS: usize = 2000;

#[derive(Debug, PartialEq)]
pub enum OutgoingError {
    Empty,
    TooLong(usize),
}

/// Trailing whitespace is trimmed; leading is kept (code blocks, indentation).
pub fn validate_outgoing(s: &str) -> Result<&str, OutgoingError> {
    let t = s.trim_end();
    match t.chars().count() {
        0 => Err(OutgoingError::Empty),
        n if n > MAX_CHARS => Err(OutgoingError::TooLong(n)),
        _ => Ok(t),
    }
}

/// `(id, content)` of own message `id` in `ch`, or of the newest own one for `None`.
pub fn edit_target(s: &Store, ch: Id, id: Option<Id>) -> Option<(Id, String)> {
    let me = s.me()?.id;
    let m = s.messages(ch).iter().rev().find(|m| m.author.id == me && id.is_none_or(|i| i == m.id))?;
    Some((m.id, m.content.clone()))
}

/// A new outgoing message with a fresh nonce.
pub fn new_pending(channel: Id, content: String, reply_to: Option<Id>) -> Pending {
    let nonce = make_nonce(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64));
    Pending { nonce, channel, content, reply_to, failed: false }
}

/// The edit REST response as the update the gateway would send, to apply without waiting for it.
pub fn edit_event(m: Message) -> Event {
    let (id, channel_id, edited_timestamp) = (m.id, m.channel_id, m.edited_timestamp);
    Event::MessageUpdate(MessageUpdate { id, channel_id, content: Some(m.content), edited_timestamp, attachments: None })
}

/// Unsent text of `pending` per channel (first-seen order), messages in send order joined by blank lines.
pub fn unsent_drafts(pending: &[Pending]) -> Vec<(Id, String)> {
    let mut out: Vec<(Id, String)> = vec![];
    for p in pending {
        match out.iter_mut().find(|(c, _)| *c == p.channel) {
            Some((_, t)) => *t = join(t, &p.content),
            None => out.push((p.channel, p.content.clone())),
        }
    }
    out
}

/// `a`, a blank line, then `b`; just `b` when `a` is blank.
fn join(a: &str, b: &str) -> String {
    if a.trim().is_empty() { b.to_string() } else { format!("{a}\n\n{b}") }
}

/// Per-channel composer drafts (UI thread). `current` is the channel whose draft the composer shows.
#[derive(Default)]
pub struct Drafts {
    /// The user the saved drafts belong to.
    owner: Option<Id>,
    current: Option<Id>,
    saved: HashMap<Id, String>,
}

/// Composer text after a successful edit: the draft stashed when the edit began, unless the user typed
/// after submitting; then nothing is overwritten (the stash goes in front of what they typed).
pub fn after_edit(composer: &str, submitted: &str, stash: &str) -> String {
    if composer == submitted { stash.to_string() } else { join(stash, composer) }
}

impl Drafts {
    /// A session for user `me` started: another account's drafts are dropped, the same user's kept.
    pub fn set_user(&mut self, me: Id) {
        if self.owner.replace(me) != Some(me) {
            self.saved.clear();
        }
    }

    /// The composer moves to channel `to`: `text` is kept as the old channel's draft; returns `to`'s draft.
    pub fn switch(&mut self, text: String, to: Option<Id>) -> String {
        if to == self.current {
            return text;
        }
        if let Some(c) = self.current {
            self.saved.insert(c, text);
        }
        self.current = to;
        to.and_then(|c| self.saved.remove(&c)).unwrap_or_default()
    }

    /// Appends restored text to `ch`'s draft; returns the new composer text when `ch` is the one shown.
    pub fn restore(&mut self, ch: Id, text: &str, composer: &str) -> Option<String> {
        if self.current == Some(ch) {
            return Some(join(composer, text));
        }
        let d = self.saved.entry(ch).or_default();
        *d = join(d, text);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsent_grouped_per_channel() {
        let p = |n: &str, ch: u64, content: &str, failed| Pending {
            nonce: n.into(),
            channel: Id(ch),
            content: content.into(),
            reply_to: None,
            failed,
        };
        let pending = [p("1", 7, "a", true), p("2", 8, "b", false), p("3", 7, "c\nd", false)];
        assert_eq!(unsent_drafts(&pending), vec![(Id(7), "a\n\nc\nd".to_string()), (Id(8), "b".to_string())]);
        assert_eq!(unsent_drafts(&[]), vec![]);
    }

    #[test]
    fn edit_success_keeps_typed_text() {
        assert_eq!(after_edit("fixed", "fixed", "my draft"), "my draft");
        assert_eq!(after_edit("fixed", "fixed", ""), "");
        assert_eq!(after_edit("fixed more", "fixed", ""), "fixed more", "typed during the PATCH");
        assert_eq!(after_edit("fixed more", "fixed", "my draft"), "my draft\n\nfixed more");
    }

    #[test]
    fn drafts_scoped_to_user() {
        let mut d = Drafts::default();
        d.set_user(Id(1));
        d.switch(String::new(), Some(Id(7)));
        d.switch("mine".into(), None); // logout
        d.set_user(Id(1)); // same user again: kept
        assert_eq!(d.switch(String::new(), Some(Id(7))), "mine");
        d.switch("mine".into(), None);
        d.set_user(Id(2)); // another account: dropped
        assert_eq!(d.switch(String::new(), Some(Id(7))), "");
    }

    #[test]
    fn drafts_follow_channel() {
        let mut d = Drafts::default();
        assert_eq!(d.switch(String::new(), Some(Id(1))), "");
        assert_eq!(d.switch("for one".into(), Some(Id(2))), "", "new channel starts empty");
        assert_eq!(d.switch("for two".into(), Some(Id(1))), "for one");
        assert_eq!(d.switch("for one!".into(), Some(Id(1))), "for one!", "same channel: unchanged");
        assert_eq!(d.switch("kept".into(), None), "", "logged out: nothing shown");
        assert_eq!(d.switch(String::new(), Some(Id(1))), "kept", "draft survives a session change");
        // restore: shown channel gets it in the composer, others in their saved draft
        assert_eq!(d.restore(Id(1), "lost", "kept"), Some("kept\n\nlost".into()));
        assert_eq!(d.restore(Id(1), "lost", " \n"), Some("lost".into()));
        assert_eq!(d.restore(Id(2), "lost", "x"), None);
        assert_eq!(d.switch(String::new(), Some(Id(2))), "for two\n\nlost");
        assert_eq!(d.restore(Id(3), "only", ""), None);
        assert_eq!(d.switch(String::new(), Some(Id(3))), "only");
    }

    use serde_json::json;

    #[test]
    fn outgoing_validation() {
        assert_eq!(validate_outgoing(""), Err(OutgoingError::Empty));
        assert_eq!(validate_outgoing(" \n\t "), Err(OutgoingError::Empty));
        assert_eq!(validate_outgoing("  hi \n"), Ok("  hi"));
        assert_eq!(validate_outgoing("```\n  x\n```\n"), Ok("```\n  x\n```"));
        let max = "é".repeat(MAX_CHARS); // 2 bytes each: chars are counted, not bytes
        assert_eq!(validate_outgoing(&max), Ok(max.as_str()));
        assert_eq!(validate_outgoing(&format!("{max}x")), Err(OutgoingError::TooLong(MAX_CHARS + 1)));
        assert_eq!(validate_outgoing(&format!("{max}   \n")), Ok(max.as_str()), "trailing space doesn't count");
    }

    #[test]
    fn edit_target_is_own_message() {
        let ready = json!({"user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [{"id": "100", "type": 0, "name": "a"}]}]});
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        let ch = Id(100);
        assert_eq!(edit_target(&s, ch, None), None, "nothing loaded");
        s.open_channel(ch);
        for (id, author) in [(500, "1"), (501, "2"), (502, "1"), (503, "2")] {
            let m = json!({"id": id.to_string(), "channel_id": "100", "author": {"id": author, "username": "u"},
                "content": format!("m{id}"), "timestamp": "2026-11-07T12:00:00+00:00"});
            s.apply(&Event::MessageCreate(Box::new(serde_json::from_value(m).unwrap())));
        }
        assert_eq!(edit_target(&s, ch, None), Some((Id(502), "m502".into())));
        assert_eq!(edit_target(&s, ch, Some(Id(500))), Some((Id(500), "m500".into())));
        assert_eq!(edit_target(&s, ch, Some(Id(501))), None, "not mine");
        assert_eq!(edit_target(&s, Id(999), None), None);
    }
}
