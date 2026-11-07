//! Plain `Send` view structs built from the store on the runtime thread.
use crate::images::{THUMB_MAX, fit, thumb_url};
use crate::markdown::{Block, Mention, parse};
use chrono::{DateTime, FixedOffset, Local};
use rxcord_api::id::Id;
use rxcord_api::model::{Attachment, Channel, Message, User};
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
    pub thumbs: Vec<Thumb>,
    pub files: Vec<(String, String)>, // (filename, url)
    pub reply_preview: Option<String>,
    pub edited: bool,
    pub mine: bool,
    pub pending: bool,
    pub failed: bool,
}

/// An image attachment; the row reserves `width` x `height` px before the image arrives.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumb {
    pub url: String, // sized proxy URL, also the cache key
    pub full: String,
    pub width: u32,
    pub height: u32,
}

pub fn format_time(ts: &str, offset: FixedOffset) -> String {
    DateTime::parse_from_rfc3339(ts).map(|t| t.with_timezone(&offset).format("%H:%M").to_string()).unwrap_or_default()
}

pub fn channel_row(s: &Store, c: &Channel) -> ChannelRow {
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

/// Discord order: uncategorized channels, then each category followed by its children; text before voice.
pub fn channel_rows(s: &Store, guild: Id) -> Vec<ChannelRow> {
    let all = s.channels_of(guild); // (position, id) order
    let cats: Vec<&Channel> = all.iter().copied().filter(|c| c.kind == 4).collect();
    let under = |p: Option<Id>| {
        let mut v: Vec<&Channel> = all
            .iter()
            .copied()
            .filter(|c| c.kind != 4 && c.parent_id.filter(|id| cats.iter().any(|k| k.id == *id)) == p)
            .collect();
        v.sort_by_key(|c| matches!(c.kind, 2 | 13)); // stable sort keeps position order
        v
    };
    let mut out = under(None);
    for k in &cats {
        out.push(k);
        out.extend(under(Some(k.id)));
    }
    out.into_iter().map(|c| channel_row(s, c)).collect()
}

pub fn dm_rows(s: &Store) -> Vec<ChannelRow> {
    s.dms().into_iter().map(|c| channel_row(s, c)).collect()
}

/// Mention spans show names: `@user` (from the message's `mentions`, then the store), `#channel`, `@role`.
fn resolve_mentions(s: &Store, known: &[User], mut blocks: Vec<Block>) -> Vec<Block> {
    for b in &mut blocks {
        let Block::Para(spans) = b else { continue };
        for sp in spans {
            sp.text = match sp.mention {
                Some(Mention::User(id)) => known
                    .iter()
                    .chain(s.user(id))
                    .find(|u| u.id == id)
                    .map_or("@unknown-user".into(), |u| format!("@{}", u.display_name())),
                Some(Mention::Channel(id)) => {
                    s.channel(id).and_then(|c| c.name.as_deref()).map_or("#unknown-channel".into(), |n| format!("#{n}"))
                }
                Some(Mention::Role(_)) => "@role".into(), // shortcut: roles aren't stored yet; show names once they are
                Some(Mention::Everyone) | None => continue,
            };
        }
    }
    blocks
}

fn preview(m: &Message) -> String {
    // one line: the preview is an elided single-line Text
    let text: String = m.content.chars().take(100).map(|c| if c == '\n' { ' ' } else { c }).collect();
    format!("{}: {text}", m.author.display_name())
}

fn thumb(a: &Attachment) -> Thumb {
    let (width, height) = fit(a.width, a.height, THUMB_MAX).unwrap_or((THUMB_MAX / 2, THUMB_MAX / 2));
    Thumb { url: thumb_url(&a.proxy_url, a.width, a.height, THUMB_MAX), full: a.url.clone(), width, height }
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
                blocks: resolve_mentions(s, &m.mentions, parse(&m.content)),
                thumbs: img.into_iter().map(thumb).collect(),
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
        blocks: resolve_mentions(s, &[], parse(&p.content)),
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
            "content": "hi", "timestamp": "2026-11-07T12:00:00+00:00", "attachments": [
                {"id": "1", "filename": "a.png", "url": "https://cdn/a.png", "proxy_url": "https://media/a.png",
                 "width": 800, "height": 400, "content_type": "image/png", "size": 1},
                {"id": "2", "filename": "b.png", "url": "https://cdn/b.png", "proxy_url": "https://media/b.png",
                 "content_type": "image/png", "size": 1},
                {"id": "3", "filename": "c.txt", "url": "https://cdn/c.txt", "proxy_url": "https://media/c.txt", "size": 1}
            ]});
        s.apply(&Event::MessageCreate(Box::new(serde_json::from_value(m).unwrap())));
        s.add_pending(Pending { nonce: "n1".into(), channel: ch, content: "yo".into(), reply_to: None, failed: false });
        let rows = message_rows(&s, ch);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, Some(Id(500)));
        assert_eq!(rows[0].author, "bob");
        assert!(!rows[0].pending && !rows[0].mine);
        let t = |url: &str, full: &str, width, height| Thumb { url: url.into(), full: full.into(), width, height };
        assert_eq!(rows[0].thumbs, [
            t("https://media/a.png?width=400&height=200", "https://cdn/a.png", 400, 200),
            t("https://media/b.png", "https://cdn/b.png", 200, 200), // size unknown: square placeholder
        ]);
        assert_eq!(rows[0].files, [("c.txt".to_string(), "https://cdn/c.txt".to_string())]);
        let p = &rows[1];
        assert_eq!((p.id, p.nonce.as_deref(), p.pending, p.mine), (None, Some("n1"), true, true));
        assert_eq!(p.author, "me");
    }

    #[test]
    fn mentions_resolved() {
        let mut s = store();
        let ch = Id(100);
        s.open_channel(ch);
        let m = json!({"id": "500", "channel_id": "100", "author": {"id": "2", "username": "bob"},
            "content": "<@1> <@3> <@!9> <#100> <#7> <@&4> @here", "timestamp": "2026-11-07T12:00:00+00:00",
            "mentions": [{"id": "3", "username": "carl", "global_name": "Carl"}]});
        s.apply(&Event::MessageCreate(Box::new(serde_json::from_value(m).unwrap())));
        let Block::Para(spans) = &message_rows(&s, ch)[0].blocks[0] else { panic!() };
        let text: String = spans.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(text, "@me @Carl @unknown-user #general #unknown-channel @role @here");
    }

    #[test]
    fn voice_channel_disabled() {
        let rows = channel_rows(&store(), Id(10));
        let v = rows.iter().find(|r| r.id == Id(101)).unwrap();
        assert_eq!((v.kind, v.enabled), (2, false));
        assert!(rows.iter().find(|r| r.id == Id(100)).unwrap().enabled);
    }

    #[test]
    fn channels_grouped_under_categories() {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [
                {"id": "50", "type": 4, "name": "Cat", "position": 0},
                {"id": "51", "type": 0, "name": "a", "position": 1, "parent_id": "50"},
                {"id": "52", "type": 0, "name": "b", "position": 0},
                {"id": "53", "type": 2, "name": "v", "position": 0, "parent_id": "50"},
                {"id": "54", "type": 0, "name": "c", "position": 2, "parent_id": "50"},
                {"id": "55", "type": 0, "name": "orphan", "position": 5, "parent_id": "99"}
            ]}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        let names: Vec<_> = channel_rows(&s, Id(10)).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["b", "orphan", "Cat", "a", "c", "v"]);
    }

    #[test]
    fn time_is_local_hhmm() {
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(format_time("2026-11-07T21:05:09.123000+00:00", plus2), "23:05");
        assert_eq!(format_time("2026-11-07T23:30:00+00:00", plus2), "01:30");
        assert_eq!(format_time("garbage", plus2), "");
    }
}
