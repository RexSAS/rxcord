//! Debug-only `RXCORD_DEMO=1`: fixture-like data instead of a Discord session (screenshots, layout work).
use rxcord_api::event::{Event, parse_dispatch};
use rxcord_api::id::Id;
use serde_json::{json, value::to_raw_value};

pub const GUILD: u64 = 10;
pub const CHANNEL: u64 = 100;

fn ev(name: &str, d: serde_json::Value) -> Event {
    parse_dispatch(name, &to_raw_value(&d).unwrap()).unwrap()
}

/// READY, then the messages for `CHANNEL` (apply after opening it).
pub fn events() -> (Event, Vec<Event>) {
    let ch = |id: u64, kind: u8, name: &str, pos: i32, parent: Option<&str>| {
        json!({"id": id.to_string(), "type": kind, "name": name, "position": pos, "parent_id": parent, "last_message_id": "900"})
    };
    let ready = ev("READY", json!({
        "user": {"id": "1", "username": "me"},
        "guilds": [
            {"id": GUILD.to_string(), "name": "Rust Users", "channels": [
                ch(50, 4, "Text Channels", 0, None), ch(CHANNEL, 0, "general", 0, Some("50")),
                ch(101, 0, "help", 1, Some("50")), ch(102, 2, "Voice", 2, Some("50")), ch(103, 0, "rules", 0, None)]},
            {"id": "11", "name": "Slint", "channels": [ch(110, 0, "chat", 0, None)]}],
        "private_channels": [{"id": "200", "type": 1, "recipient_ids": ["2"], "last_message_id": "900"}],
        "users": [{"id": "2", "username": "ferris", "global_name": "Ferris"}],
        "read_state": {"entries": [{"id": "101", "last_message_id": "1", "mention_count": 2}]},
    }));
    let texts = [
        "hello **world**, this is *italic* and ~~struck~~ with `inline code`",
        "a link: https://www.rust-lang.org and <@1>, join us: https://discord.gg/rust-lang",
        "```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
        "a long line that should wrap around the message list because it keeps going and going and going on",
    ];
    // Same image in several rows (one fetch); offline it fails and keeps the placeholder.
    let attachments = json!([
        {"id": "1", "filename": "ferris.png", "url": "https://cdn.discordapp.com/attachments/100/1/ferris.png",
         "proxy_url": "https://media.discordapp.net/attachments/100/1/ferris.png", "width": 800, "height": 533,
         "content_type": "image/png", "size": 1},
        {"id": "2", "filename": "notes.txt", "url": "https://cdn.discordapp.com/attachments/100/2/notes.txt",
         "proxy_url": "https://media.discordapp.net/attachments/100/2/notes.txt", "content_type": "text/plain", "size": 1}
    ]);
    let msgs = (0..40)
        .map(|i| {
            ev("MESSAGE_CREATE", json!({"id": (500 + i).to_string(), "channel_id": CHANNEL.to_string(),
                "author": {"id": (1 + i % 2).to_string(), "username": if i % 2 == 0 { "me" } else { "ferris" }},
                "content": texts[i % texts.len()], "timestamp": "2026-11-07T12:00:00+00:00",
                "attachments": if i % 10 == 9 { attachments.clone() } else { json!([]) }}))
        })
        .collect();
    (ready, msgs)
}

/// A send with no session: shows the failed row with its retry action (replying to message 538).
pub fn failed_send() -> crate::runtime::Command {
    let (channel, content) = (Id(CHANNEL), "this send fails (demo has no session)".into());
    crate::runtime::Command::Send { channel, content, reply_to: Some(Id(538)) }
}
