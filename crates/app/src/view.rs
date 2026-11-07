//! Plain `Send` view structs built from the store on the runtime thread.
use crate::markdown::{Block, parse};
use chrono::{DateTime, FixedOffset, Local};
use rxcord_api::id::Id;
use rxcord_api::model::{Channel, Message};
use rxcord_store::Store;

#[derive(Debug, Clone, PartialEq)]
pub struct GuildRow {
    pub id: Id,
    pub name: String,
    pub unread: bool,
    pub mentions: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelRow {
    pub id: Id,
    pub name: String,
    pub kind: u8,
    pub unread: bool,
    pub mentions: u32,
    pub enabled: bool, // false for voice
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub id: Option<Id>,
    pub nonce: Option<String>,
    pub author: String,
    pub time: String, // local HH:MM
    pub blocks: Vec<Block>,
    pub thumbs: Vec<String>,
    pub files: Vec<(String, String)>,
    pub reply_preview: Option<String>,
    pub edited: bool,
    pub mine: bool,
    pub pending: bool,
    pub failed: bool,
}

pub fn format_time(ts: &str, offset: FixedOffset) -> String {
    DateTime::parse_from_rfc3339(ts).map(|t| t.with_timezone(&offset).format("%H:%M").to_string()).unwrap_or_default()
}

fn channel_row(s: &Store, c: &Channel) -> ChannelRow {
    let name = c.name.clone().unwrap_or_else(|| {
        let names: Vec<_> = c.recipient_ids.iter().filter_map(|u| s.user(*u)).map(|u| u.display_name()).collect();
        names.join(", ")
    });
    ChannelRow {
        id: c.id,
        name,
        kind: c.kind,
        unread: s.is_unread(c.id),
        mentions: s.mention_count(c.id),
        enabled: !matches!(c.kind, 2 | 13), // voice, stage
    }
}

pub fn guild_rows(s: &Store) -> Vec<GuildRow> {
    s.guilds()
        .iter()
        .map(|g| {
            let chans = s.channels_of(g.id);
            GuildRow {
                id: g.id,
                name: g.name.clone(),
                unread: chans.iter().any(|c| s.is_unread(c.id)),
                mentions: chans.iter().map(|c| s.mention_count(c.id)).sum(),
            }
        })
        .collect()
}

pub fn channel_rows(s: &Store, guild: Id) -> Vec<ChannelRow> {
    s.channels_of(guild).into_iter().map(|c| channel_row(s, c)).collect()
}

pub fn dm_rows(s: &Store) -> Vec<ChannelRow> {
    s.dms().into_iter().map(|c| channel_row(s, c)).collect()
}

fn preview(m: &Message) -> String {
    let text: String = m.content.chars().take(100).collect();
    format!("{}: {text}", m.author.display_name())
}

/// Stored messages, then pending ones. Times use the current local offset.
pub fn message_rows(s: &Store, channel: Id) -> Vec<MessageRow> {
    let offset = *Local::now().offset();
    let me = s.me();
    let msgs = s.messages(channel);
    let mut rows: Vec<MessageRow> = msgs
        .iter()
        .map(|m| {
            let (img, other): (Vec<_>, Vec<_>) =
                m.attachments.iter().partition(|a| a.content_type.as_deref().is_some_and(|t| t.starts_with("image/")));
            MessageRow {
                id: Some(m.id),
                nonce: None,
                author: m.author.display_name().to_string(),
                time: format_time(&m.timestamp, offset),
                blocks: parse(&m.content),
                thumbs: img.into_iter().map(|a| a.proxy_url.clone()).collect(),
                files: other.into_iter().map(|a| (a.filename.clone(), a.url.clone())).collect(),
                reply_preview: m.referenced_message.as_deref().map(preview),
                edited: m.edited_timestamp.is_some(),
                mine: me.is_some_and(|u| u.id == m.author.id),
                pending: false,
                failed: false,
            }
        })
        .collect();
    rows.extend(s.pending(channel).into_iter().map(|p| MessageRow {
        id: None,
        nonce: Some(p.nonce.clone()),
        author: me.map(|u| u.display_name().to_string()).unwrap_or_default(),
        time: String::new(),
        blocks: parse(&p.content),
        thumbs: vec![],
        files: vec![],
        reply_preview: p.reply_to.and_then(|r| msgs.iter().find(|m| m.id == r)).map(preview),
        edited: false,
        mine: true,
        pending: true,
        failed: p.failed,
    }));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxcord_api::event::Event;
    use rxcord_store::Pending;
    use serde_json::json;

    fn store() -> Store {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [
                {"id": "100", "type": 0, "name": "general"},
                {"id": "101", "type": 2, "name": "voice"}
            ]}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        s
    }

    #[test]
    fn message_rows_include_pending_last() {
        let mut s = store();
        let ch = Id(100);
        s.open_channel(ch);
        let m = json!({"id": "500", "channel_id": "100", "author": {"id": "2", "username": "bob"},
            "content": "hi", "timestamp": "2026-11-07T12:00:00+00:00"});
        s.apply(&Event::MessageCreate(Box::new(serde_json::from_value(m).unwrap())));
        s.add_pending(Pending { nonce: "n1".into(), channel: ch, content: "yo".into(), reply_to: None, failed: false });
        let rows = message_rows(&s, ch);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, Some(Id(500)));
        assert_eq!(rows[0].author, "bob");
        assert!(!rows[0].pending && !rows[0].mine);
        let p = &rows[1];
        assert_eq!((p.id, p.nonce.as_deref(), p.pending, p.mine), (None, Some("n1"), true, true));
        assert_eq!(p.author, "me");
    }

    #[test]
    fn voice_channel_disabled() {
        let rows = channel_rows(&store(), Id(10));
        let v = rows.iter().find(|r| r.id == Id(101)).unwrap();
        assert_eq!((v.kind, v.enabled), (2, false));
        assert!(rows.iter().find(|r| r.id == Id(100)).unwrap().enabled);
    }

    #[test]
    fn time_is_local_hhmm() {
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(format_time("2026-11-07T21:05:09.123000+00:00", plus2), "23:05");
        assert_eq!(format_time("2026-11-07T23:30:00+00:00", plus2), "01:30");
        assert_eq!(format_time("garbage", plus2), "");
    }
}
