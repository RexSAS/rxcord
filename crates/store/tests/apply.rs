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
