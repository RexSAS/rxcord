//! Per-session navigation state and the pure rules for history paging and read acks.
use crate::view::MessageRow;
use rxcord_api::id::Id;
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
    pub in_flight: HashSet<Id>,
    pub history_end: HashMap<Id, Id>,
    pub acked: HashMap<Id, Instant>,
    pub ack_later: HashSet<Id>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
