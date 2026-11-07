use rxcord_api::event::Event;
use rxcord_api::id::Id;
use rxcord_api::model::{Message, Ready, User};
use rxcord_store::{Change, MESSAGE_CAP, Pending, Store};

fn fx(name: &str) -> String {
    std::fs::read_to_string(format!("{}/../api/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

// Above fixture read-state ids so new messages count as unread.
const BASE: u64 = 900_000_000_000_000_000;
const ME: Id = Id(100000000000000001);
const GCH: Id = Id(300000000000000001);

fn ready_store() -> Store {
    let r: Ready = serde_json::from_str(&fx("ready_legacy.json")).unwrap();
    let mut s = Store::new();
    s.apply(&Event::Ready(Box::new(r)));
    s
}

fn dm_id(s: &Store) -> Id {
    s.dms()[0].id
}

fn msg(id: u64, ch: Id, author: Id) -> Message {
    let mut m: Message = serde_json::from_str(&fx("message_create.json")).unwrap();
    m.id = Id(BASE + id);
    m.channel_id = ch;
    m.author = User { id: author, ..m.author };
    m.mentions.clear();
    m.mention_everyone = false;
    m.nonce = None;
    m
}

fn create(s: &mut Store, m: Message) -> Vec<Change> {
    s.apply(&Event::MessageCreate(Box::new(m)))
}

#[test]
fn ready_populates() {
    let s = ready_store();
    assert_eq!(s.me().unwrap().id, ME);
    assert_eq!(s.guilds().len(), 1);
    assert_eq!(s.channels_of(s.guilds()[0].id).len(), 2);
    assert_eq!(s.dms().len(), 1);
}

#[test]
fn duplicate_create_once() {
    let mut s = ready_store();
    s.open_channel(GCH);
    create(&mut s, msg(500, GCH, Id(7)));
    create(&mut s, msg(500, GCH, Id(7)));
    assert_eq!(s.messages(GCH).len(), 1);
}

#[test]
fn unknown_channel_ignored() {
    let mut s = ready_store();
    assert_eq!(create(&mut s, msg(500, Id(999), Id(7))), vec![]);
}

#[test]
fn cap_evicts_oldest() {
    let mut s = ready_store();
    s.open_channel(GCH);
    for i in 1..=201 {
        create(&mut s, msg(i, GCH, Id(7)));
    }
    let q = s.messages(GCH);
    assert_eq!(q.len(), MESSAGE_CAP);
    assert!(q.iter().all(|m| m.id != Id(BASE + 1)));
    assert_eq!(q.back().unwrap().id, Id(BASE + 201));
}

#[test]
fn pending_cleared_by_nonce() {
    let mut s = ready_store();
    s.open_channel(GCH);
    s.add_pending(Pending { nonce: "n1".into(), channel: GCH, content: "hi".into(), reply_to: None, failed: false });
    assert_eq!(s.pending(GCH).len(), 1);
    let mut m = msg(500, GCH, ME);
    m.nonce = Some("n1".into());
    create(&mut s, m);
    assert!(s.pending(GCH).is_empty());
    assert_eq!(s.messages(GCH).len(), 1);
}

#[test]
fn dm_counts_as_mention() {
    let mut s = ready_store();
    let dm = dm_id(&s);
    let ch = create(&mut s, msg(500, dm, Id(7)));
    assert!(ch.contains(&Change::Unread(dm)));
    assert_eq!(s.mention_count(dm), 1);
    assert!(s.is_unread(dm));
    // plain guild message without mention does not count
    let before = s.mention_count(GCH);
    create(&mut s, msg(501, GCH, Id(7)));
    assert_eq!(s.mention_count(GCH), before);
    assert!(s.is_unread(GCH));
}

#[test]
fn own_message_marks_read() {
    let mut s = ready_store();
    create(&mut s, msg(500, GCH, Id(7)));
    assert!(s.is_unread(GCH));
    create(&mut s, msg(501, GCH, ME));
    assert!(!s.is_unread(GCH));
    assert_eq!(s.mention_count(GCH), 0);
}

#[test]
fn ack_clears_unread() {
    let mut s = ready_store();
    let dm = dm_id(&s);
    create(&mut s, msg(500, dm, Id(7)));
    let ch = s.apply(&Event::MessageAck { channel_id: dm, message_id: Id(BASE + 500) });
    assert_eq!(ch, vec![Change::Unread(dm)]);
    assert!(!s.is_unread(dm));
    assert_eq!(s.mention_count(dm), 0);
}

#[test]
fn history_prepend_dedupes() {
    let mut s = ready_store();
    assert!(s.open_channel(GCH));
    create(&mut s, msg(30, GCH, Id(7)));
    let ch = s.prepend_history(GCH, vec![msg(30, GCH, Id(7)), msg(20, GCH, Id(7)), msg(10, GCH, Id(7))]);
    assert_eq!(ch, vec![Change::Messages(GCH)]);
    let ids: Vec<u64> = s.messages(GCH).iter().map(|m| m.id.0 - BASE).collect();
    assert_eq!(ids, vec![10, 20, 30]);
    assert!(!s.open_channel(GCH));
}

#[test]
fn close_trims_to_cap() {
    let mut s = ready_store();
    s.open_channel(GCH);
    let hist: Vec<_> = (1..=250).rev().map(|i| msg(i, GCH, Id(7))).collect();
    s.prepend_history(GCH, hist);
    assert_eq!(s.messages(GCH).len(), 250);
    s.close_channel(GCH);
    let q = s.messages(GCH);
    assert_eq!(q.len(), MESSAGE_CAP);
    assert_eq!(q.front().unwrap().id, Id(BASE + 51));
    assert_eq!(q.back().unwrap().id, Id(BASE + 250));
}

#[test]
fn duplicate_dm_create_counts_once() {
    let mut s = ready_store();
    let dm = dm_id(&s);
    for opened in [false, true] {
        if opened {
            s.open_channel(dm);
        }
        let base = s.mention_count(dm);
        create(&mut s, msg(700 + opened as u64, dm, Id(7)));
        let again = create(&mut s, msg(700 + opened as u64, dm, Id(7)));
        assert_eq!(s.mention_count(dm), base + 1);
        assert!(!again.contains(&Change::Unread(dm)));
    }
}

fn pend(n: &str, ch: Id) -> Pending {
    Pending { nonce: n.into(), channel: ch, content: "x".into(), reply_to: None, failed: false }
}

#[test]
fn fail_and_take_pending() {
    let mut s = ready_store();
    s.add_pending(pend("a", GCH));
    assert_eq!(s.fail_pending("a"), Some(Change::Messages(GCH)));
    assert_eq!(s.fail_pending("zz"), None);
    assert!(s.pending(GCH)[0].failed);
    assert_eq!(s.take_pending("a").unwrap().nonce, "a");
    assert!(s.take_pending("a").is_none());
}

#[test]
fn retry_pending_only_failed() {
    let mut s = ready_store();
    s.add_pending(pend("a", GCH));
    s.add_pending(pend("b", GCH));
    assert_eq!(s.retry_pending("a"), None, "still in flight: no double send");
    s.fail_pending("a");
    assert_eq!(s.retry_pending("a"), Some(pend("a", GCH)));
    let p = s.pending(GCH);
    assert_eq!((p[0].nonce.as_str(), p[0].failed), ("a", false), "same nonce, same place");
    assert_eq!(s.retry_pending("zz"), None);
}

#[test]
fn guild_and_channel_delete_clean_up_but_keep_pending() {
    let mut s = ready_store();
    let g = s.guilds()[0].id;
    s.open_channel(GCH);
    create(&mut s, msg(1, GCH, Id(7)));
    s.add_pending(pend("a", GCH));
    s.apply(&Event::GuildDelete { id: g, unavailable: false });
    assert!(s.channels_of(g).is_empty());
    assert!(s.messages(GCH).is_empty());
    assert_eq!(s.mention_count(GCH), 0);
    // unsent text is never dropped silently: it stays until the session ends (then it becomes a draft)
    assert_eq!(s.all_pending(), [pend("a", GCH)]);

    let mut s = ready_store();
    let ch = s.channel(GCH).unwrap().clone();
    s.open_channel(GCH);
    create(&mut s, msg(1, GCH, Id(7)));
    s.add_pending(pend("a", GCH));
    s.apply(&Event::ChannelDelete(ch));
    assert!(s.channel(GCH).is_none());
    assert!(s.messages(GCH).is_empty());
    assert_eq!(s.all_pending(), [pend("a", GCH)]);
}

#[test]
fn unavailable_guild_delete_ignored() {
    let mut s = ready_store();
    let g = s.guilds()[0].id;
    s.open_channel(GCH);
    create(&mut s, msg(1, GCH, Id(7)));
    assert_eq!(s.apply(&Event::GuildDelete { id: g, unavailable: true }), vec![]);
    assert_eq!(s.guilds().len(), 1);
    assert_eq!(s.channels_of(g).len(), 2);
    assert_eq!(s.messages(GCH).len(), 1);
}

#[test]
fn re_ready_keeps_pending_drops_messages() {
    let mut s = ready_store();
    s.open_channel(GCH);
    create(&mut s, msg(1, GCH, Id(7)));
    s.add_pending(pend("a", GCH));
    let r: Ready = serde_json::from_str(&fx("ready_legacy.json")).unwrap();
    s.apply(&Event::Ready(Box::new(r)));
    assert_eq!(s.pending(GCH).len(), 1);
    assert!(s.messages(GCH).is_empty());
    assert!(s.open_channel(GCH));
}

#[test]
fn prepend_dedupes_within_batch() {
    let mut s = ready_store();
    s.prepend_history(GCH, vec![msg(2, GCH, Id(7)), msg(2, GCH, Id(7)), msg(1, GCH, Id(7))]);
    assert_eq!(s.messages(GCH).len(), 2);
}

#[test]
fn channel_create_dm_with_full_recipients_names_the_dm() {
    let mut s = ready_store();
    let c = serde_json::from_str(r#"{"id":"777","type":1,"recipients":[{"id":"555","username":"bob","global_name":"Bob"}]}"#).unwrap();
    s.apply(&Event::ChannelCreate(c));
    let ch = s.channel(Id(777)).unwrap();
    assert_eq!(ch.recipient_ids, vec![Id(555)]);
    assert_eq!(s.user(Id(555)).unwrap().display_name(), "Bob");
}
