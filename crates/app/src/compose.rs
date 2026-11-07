//! Composer rules: what may be sent, and which message "Edit" / Up-arrow opens.
use rxcord_api::event::Event;
use rxcord_api::id::Id;
use rxcord_api::model::{Message, MessageUpdate};
use rxcord_api::nonce::make_nonce;
use rxcord_store::{Pending, Store};
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

#[cfg(test)]
mod tests {
    use super::*;
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
