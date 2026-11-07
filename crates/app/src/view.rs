//! Plain `Send` view structs built from the store on the runtime thread.
use crate::images::{THUMB_MAX, fit, thumb_url};
use crate::markdown::{Block, Mention, parse};
use crate::nav::DMS_NODE;
use chrono::{DateTime, FixedOffset, Local};
use rxcord_api::id::Id;
use rxcord_api::model::{Attachment, Channel, Message, User, VoiceState};
use rxcord_store::Store;
use std::collections::HashSet;

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
    pub enabled: bool, // false for stage
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
    /// Server invite codes found in the text (a Join button each).
    pub invites: Vec<String>,
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
        enabled: c.kind != 13, // stage; voice joins on double-click
    }
}

/// Whether `c`'s unread state counts for its guild's dot: only channels the user can open and read.
pub fn counts_for_guild(c: &Channel, forbidden: &HashSet<Id>) -> bool {
    !matches!(c.kind, 2 | 13 | 4) && !forbidden.contains(&c.id) // voice, stage, category; history 403
}

/// `forbidden`: channels whose history returned 403 this session.
pub fn guild_rows(s: &Store, forbidden: &HashSet<Id>) -> Vec<GuildRow> {
    s.guilds()
        .iter()
        .map(|g| {
            let mut chans = s.channels_of(g.id);
            chans.retain(|c| counts_for_guild(c, forbidden));
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

/// One line of the tree: a node (DMs or a guild, `depth` 0) or a channel/category under an expanded node.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub node: bool,
    pub depth: u8,
    pub expanded: bool,
    /// For nodes: everything inside (the UI shows it while collapsed). For members: id and name only.
    pub row: ChannelRow,
    /// Someone in the voice channel above.
    pub member: Option<VoiceMember>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VoiceMember {
    pub user_id: Id,
    pub name: String,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
}

/// Server nick, else display name; GUILD_CREATE states carry no member, so the store's user if known.
fn voice_member(s: &Store, v: &VoiceState, speaking: &HashSet<Id>) -> VoiceMember {
    let m = v.member.as_ref();
    let name = m
        .and_then(|m| m.nick.clone())
        .or_else(|| m.and_then(|m| m.user.as_ref()).or(s.user(v.user_id)).map(|u| u.display_name().to_string()))
        .unwrap_or_else(|| "Unknown user".into());
    VoiceMember {
        user_id: v.user_id,
        name,
        speaking: speaking.contains(&v.user_id),
        muted: v.self_mute || v.mute,
        deafened: v.self_deaf || v.deaf,
    }
}

/// DMs first, then guilds in READY order; expanded nodes list their channels in Discord order, voice
/// channels followed by who's in them. `speaking`: users highlighted as talking.
pub fn tree_rows(s: &Store, expanded: &HashSet<Id>, forbidden: &HashSet<Id>, speaking: &HashSet<Id>) -> Vec<TreeRow> {
    let node = |id: Id, name: String, unread: bool, mentions: u32, children: Vec<ChannelRow>| {
        let open = expanded.contains(&id);
        let row = ChannelRow { id, name, kind: 0, unread, mentions, enabled: true };
        let mut out = vec![TreeRow { node: true, depth: 0, expanded: open, row, member: None }];
        if open {
            let mut in_category = false;
            for c in children {
                in_category |= c.kind == 4;
                let depth = if in_category && c.kind != 4 { 2 } else { 1 };
                let members = if matches!(c.kind, 2 | 13) { s.voice_members(c.id) } else { vec![] };
                out.push(TreeRow { node: false, depth, expanded: false, row: c, member: None });
                out.extend(members.into_iter().map(|v| {
                    let m = voice_member(s, v, speaking);
                    let row = ChannelRow { id: m.user_id, name: m.name.clone(), kind: 0, unread: false, mentions: 0, enabled: false };
                    TreeRow { node: false, depth: depth + 1, expanded: false, row, member: Some(m) }
                }));
            }
        }
        out
    };
    let dms = dm_rows(s);
    let (unread, mentions) = (dms.iter().any(|d| d.unread), dms.iter().map(|d| d.mentions).sum());
    let mut out = node(DMS_NODE, "Direct Messages".into(), unread, mentions, dms);
    for g in guild_rows(s, forbidden) {
        let children = if expanded.contains(&g.id) { channel_rows(s, g.id) } else { vec![] };
        out.extend(node(g.id, g.name, g.unread, g.mentions, children));
    }
    out
}

/// Open tabs in order; tabs of channels that vanished are skipped (the runtime closes them).
pub fn tab_rows(s: &Store, tabs: &[Id]) -> Vec<ChannelRow> {
    tabs.iter().filter_map(|t| s.channel(*t)).map(|c| channel_row(s, c)).collect()
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
                m.attachments.iter().partition(|a| {
                    // what `images::decode` handles; other images (svg, avif, ...) stay file links
                    matches!(a.content_type.as_deref(), Some("image/png" | "image/jpeg" | "image/webp" | "image/gif"))
                });
            MessageRow {
                id: Some(m.id),
                nonce: None,
                author: m.author.display_name().to_string(),
                time: format_time(&m.timestamp, offset),
                blocks: resolve_mentions(s, &m.mentions, parse(&m.content)),
                thumbs: img.into_iter().map(thumb).collect(),
                files: other.into_iter().map(|a| (a.filename.clone(), a.url.clone())).collect(),
                invites: crate::invite::invite_codes(&m.content),
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
        invites: vec![], // not sent yet: nothing to join from
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
    use std::collections::HashSet;

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
                {"id": "4", "filename": "d.svg", "url": "https://cdn/d.svg", "proxy_url": "https://media/d.svg",
                 "width": 10, "height": 10, "content_type": "image/svg+xml", "size": 1},
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
            t("https://media/a.png?width=200&height=100", "https://cdn/a.png", 200, 100),
            t("https://media/b.png?width=200&height=200", "https://cdn/b.png", 100, 100), // size unknown
        ]);
        let f = |n: &str| (n.to_string(), format!("https://cdn/{n}"));
        assert_eq!(rows[0].files, [f("d.svg"), f("c.txt")]);
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
    fn guild_aggregate_counts_only_openable_channels() {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [
                {"id": "100", "type": 0, "name": "text"},
                {"id": "101", "type": 2, "name": "voice", "last_message_id": "9"},
                {"id": "102", "type": 13, "name": "stage", "last_message_id": "9"},
                {"id": "103", "type": 4, "name": "cat", "last_message_id": "9"},
                {"id": "104", "type": 0, "name": "secret", "last_message_id": "9"}
            ]}],
            "read_state": [{"id": "101", "mention_count": 1}, {"id": "104", "mention_count": 2}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        let forbidden = HashSet::from([Id(104)]);
        let g = &guild_rows(&s, &forbidden)[0];
        assert_eq!((g.unread, g.mentions), (false, 0), "voice/stage/category/forbidden don't count");
        let g = &guild_rows(&s, &HashSet::new())[0];
        assert_eq!((g.unread, g.mentions), (true, 2), "an openable channel does");
    }

    #[test]
    fn voice_channel_enabled() {
        let rows = channel_rows(&store(), Id(10));
        let v = rows.iter().find(|r| r.id == Id(101)).unwrap();
        assert_eq!((v.kind, v.enabled), (2, true));
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
    fn tree_lists_voice_members() {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [{"id": "10", "name": "G", "channels": [
                {"id": "50", "type": 4, "name": "Cat", "position": 0},
                {"id": "100", "type": 0, "name": "text", "position": 0, "parent_id": "50"},
                {"id": "101", "type": 2, "name": "voice", "position": 1, "parent_id": "50"},
                {"id": "102", "type": 2, "name": "empty", "position": 2}],
                "voice_states": [{"user_id": "2", "channel_id": "101", "self_mute": true},
                    {"user_id": "9", "channel_id": "101", "deaf": true}]}],
            "users": [{"id": "2", "username": "bob", "global_name": "Bob"}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        let vsu = json!({"user_id": "1", "channel_id": "101", "guild_id": "10", "session_id": "x",
            "member": {"nick": "Nick", "user": {"id": "1", "username": "me"}}});
        s.apply(&Event::VoiceStateUpdate(serde_json::from_value(vsu).unwrap()));
        let open: HashSet<Id> = [Id(10)].into();
        let rows = tree_rows(&s, &open, &HashSet::new(), &[Id(9)].into());
        let shape: Vec<_> = rows.iter().map(|r| (r.row.name.as_str(), r.depth, r.member.is_some())).collect();
        assert_eq!(shape, [
            ("Direct Messages", 0, false), ("G", 0, false), ("empty", 1, false), ("Cat", 1, false), ("text", 2, false),
            ("voice", 2, false), ("Nick", 3, true), ("Bob", 3, true), ("Unknown user", 3, true)]);
        let m = |i: usize| rows[i].member.clone().unwrap();
        assert_eq!(m(6), VoiceMember { user_id: Id(1), name: "Nick".into(), speaking: false, muted: false, deafened: false });
        assert_eq!((m(7).muted, m(7).deafened, m(7).speaking), (true, false, false));
        assert_eq!((m(8).muted, m(8).deafened, m(8).speaking), (false, true, true));
        assert!(rows[5].row.enabled, "voice channels can be joined");
        let collapsed = tree_rows(&s, &HashSet::new(), &HashSet::new(), &HashSet::new());
        assert!(collapsed.iter().all(|r| r.member.is_none()));
    }

    #[test]
    fn time_is_local_hhmm() {
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(format_time("2026-11-07T21:05:09.123000+00:00", plus2), "23:05");
        assert_eq!(format_time("2026-11-07T23:30:00+00:00", plus2), "01:30");
        assert_eq!(format_time("garbage", plus2), "");
    }

    #[test]
    fn tree_rows_nodes_children_and_totals() {
        let ready = json!({
            "user": {"id": "1", "username": "me"},
            "guilds": [
                {"id": "10", "name": "G", "channels": [
                    {"id": "50", "type": 4, "name": "Cat", "position": 0},
                    {"id": "101", "type": 0, "name": "b", "position": 1, "parent_id": "50", "last_message_id": "9"},
                    {"id": "100", "type": 0, "name": "a", "position": 0, "last_message_id": "9"}]},
                {"id": "11", "name": "H", "channels": [{"id": "110", "type": 0, "name": "c", "last_message_id": "9"}]}],
            "private_channels": [{"id": "200", "type": 1, "recipient_ids": ["2"]}],
            "users": [{"id": "2", "username": "bob"}],
            "read_state": [{"id": "101", "last_message_id": "1", "mention_count": 2}, {"id": "100", "last_message_id": "9"},
                {"id": "110", "last_message_id": "9"}],
        });
        let mut s = Store::new();
        s.apply(&Event::Ready(Box::new(serde_json::from_value(ready).unwrap())));
        let none = HashSet::new();
        let shape = |rows: &[TreeRow]| rows.iter().map(|r| (r.row.name.clone(), r.depth, r.node)).collect::<Vec<_>>();
        let rows = tree_rows(&s, &none, &none, &none);
        assert_eq!(shape(&rows), [("Direct Messages".into(), 0, true), ("G".into(), 0, true), ("H".into(), 0, true)]);
        assert!(rows[1].row.unread && rows[1].row.mentions == 2 && !rows[1].expanded, "collapsed guild totals");
        assert!(!rows[2].row.unread);
        let open: HashSet<Id> = [DMS_NODE, Id(10)].into();
        let rows = tree_rows(&s, &open, &none, &none);
        assert_eq!(shape(&rows), [
            ("Direct Messages".into(), 0, true), ("bob".into(), 1, false),
            ("G".into(), 0, true), ("a".into(), 1, false), ("Cat".into(), 1, false), ("b".into(), 2, false),
            ("H".into(), 0, true)]);
        assert!(rows[2].expanded && rows[5].row.unread);
        let forbidden: HashSet<Id> = [Id(101)].into();
        assert!(!tree_rows(&s, &none, &forbidden, &none)[1].row.unread, "403 channels don't light the node");
        let tabs = tab_rows(&s, &[Id(200), Id(999), Id(100)]);
        assert_eq!(tabs.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["bob", "a"]);
    }
}
