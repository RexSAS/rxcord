//! How data reaches the UI: view rows -> Slint structs/models, pushed via `invoke_from_event_loop`.
use crate::markdown::{Block, Span};
use crate::nav::Nav;
use crate::images::Pixels;
use crate::view::{self, ChannelRow, GuildRow, MessageRow, Thumb};
use crate::{AppWindow, BlockItem, ChannelItem, FileItem, GuildItem, MessageItem, ThumbItem};
use rxcord_api::id::Id;
use rxcord_api::remote_auth::QrStatus;
use rxcord_store::Store;
use slint::{Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, StyledText, VecModel, Weak};

/// Guild id the UI uses for the "Direct Messages" entry.
pub const DMS: &str = "@me";

pub fn with_ui(ui: &Weak<AppWindow>, f: impl FnOnce(AppWindow) + Send + 'static) {
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui.upgrade() {
            f(ui);
        }
    });
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

/// Empties every navigation model (new session).
pub fn clear(ui: &Weak<AppWindow>) {
    with_ui(ui, |ui| {
        ui.set_guilds(ModelRc::default());
        ui.set_channels(ModelRc::default());
        ui.set_messages(model(vec![])); // must stay a VecModel, see `set_messages`
        ui.set_selected_guild(DMS.into());
        ui.set_selected_channel("".into());
        ui.set_guild_title("".into());
        ui.set_channel_title("".into());
        ui.set_messages_error("".into());
        // Composer text and reply/edit mode are left alone: the selected-channel change saves the draft.
        ui.set_composer_error("".into());
        ui.set_row_error_id("".into());
    });
}

/// Puts own message `id` into the composer in edit mode.
pub fn start_edit(ui: &Weak<AppWindow>, id: Id, text: String) {
    with_ui(ui, move |ui| {
        if ui.get_edit_id().is_empty() {
            ui.set_edit_stash(ui.get_composer_text()); // the draft comes back after the edit
        }
        ui.set_reply_id("".into());
        ui.set_edit_id(id.to_string().into());
        ui.set_composer_text(text.into());
        ui.set_composer_error("".into());
        ui.invoke_focus_composer();
    });
}

/// An edit finished. Success leaves edit mode; failure keeps the text and says so (on the row if the user moved on).
pub fn edit_done(ui: &Weak<AppWindow>, id: Id, ok: bool) {
    with_ui(ui, move |ui| {
        let editing = ui.get_edit_id() == id.to_string().as_str();
        match (ok, editing) {
            (true, true) => {
                ui.set_edit_id("".into());
                let (now, sent) = (ui.get_composer_text(), ui.get_edit_submitted());
                ui.set_composer_text(crate::compose::after_edit(&now, &sent, &ui.get_edit_stash()).into());
                ui.set_edit_stash("".into());
                ui.set_composer_error("".into());
            }
            (true, false) => {}
            (false, true) => ui.set_composer_error("Couldn't save the edit. Press Enter to try again.".into()),
            (false, false) => row_error(&ui, id, "Couldn't edit message"),
        }
    });
}

/// READY: show the app; drafts belong to user `me`.
pub fn logged_in(ui: &Weak<AppWindow>, me: Option<Id>) {
    with_ui(ui, move |ui| {
        ui.set_logged_in(true);
        if let Some(me) = me {
            ui.invoke_session_user(me.to_string().into());
        }
    });
}

/// Text of an ended session's unsent messages, per channel; main.rs adds it to that channel's draft.
pub fn restore_drafts(ui: &Weak<AppWindow>, drafts: Vec<(Id, String)>) {
    if !drafts.is_empty() {
        with_ui(ui, move |ui| {
            for (c, t) in drafts {
                ui.invoke_restore_draft(c.to_string().into(), t.into());
            }
        });
    }
}

pub fn delete_failed(ui: &Weak<AppWindow>, id: Id) {
    with_ui(ui, move |ui| row_error(&ui, id, "Couldn't delete message"));
}

/// Inline error under one message row; replaced by the next one, cleared on channel switch.
fn row_error(ui: &AppWindow, id: Id, msg: &str) {
    ui.set_row_error_id(id.to_string().into());
    ui.set_row_error(msg.into());
}

pub fn push_guilds(ui: &Weak<AppWindow>, s: &Store) {
    let dms = view::dm_rows(s);
    let summary = (dms.iter().any(|d| d.unread), dms.iter().map(|d| d.mentions).sum());
    set_guilds(ui, view::guild_rows(s), summary);
}

/// Channels of `guild`, or the DM list for `None`.
pub fn push_channels(ui: &Weak<AppWindow>, s: &Store, guild: Option<Id>) {
    let rows = match guild {
        Some(g) => view::channel_rows(s, g),
        None => view::dm_rows(s),
    };
    set_channels(ui, rows);
}

pub fn push_selection(ui: &Weak<AppWindow>, s: &Store, guild: Option<Id>, selected: Option<Id>) {
    let (guild, guild_title) = match guild {
        None => (DMS.to_string(), "Direct Messages".to_string()),
        Some(g) => (g.to_string(), s.guilds().iter().find(|x| x.id == g).map(|x| x.name.clone()).unwrap_or_default()),
    };
    let ch = selected.and_then(|c| s.channel(c));
    let title = ch.map(|c| view::channel_row(s, c).name).unwrap_or_default();
    set_selection(ui, guild, ch.map(|c| c.id.to_string()).unwrap_or_default(), guild_title, title);
}

/// Sends the selected channel's rows: a full replace for another channel or a first fill, else row edits.
pub fn push_messages(ui: &Weak<AppWindow>, s: &Store, nav: &mut Nav) {
    let rows = nav.selected.map(|c| view::message_rows(s, c)).unwrap_or_default();
    let reset = nav.shown_channel != nav.selected || nav.shown.is_empty();
    let edits: Vec<_> = if reset {
        vec![(0, 0, rows.clone())]
    } else {
        diff(&nav.shown, &rows).into_iter().map(|(at, remove, r)| (at, remove, rows[r].to_vec())).collect()
    };
    if reset || !edits.is_empty() {
        set_messages(ui, reset, edits);
    }
    nav.shown = rows;
    nav.shown_channel = nav.selected;
}

/// One-line notice in the message pane (empty hides it).
pub fn set_error(ui: &Weak<AppWindow>, msg: String) {
    with_ui(ui, move |ui| ui.set_messages_error(msg.into()));
}

/// `dms` = (any DM unread, total DM mentions) for the "Direct Messages" entry on top.
fn set_guilds(ui: &Weak<AppWindow>, rows: Vec<GuildRow>, dms: (bool, u32)) {
    with_ui(ui, move |ui| {
        let dm =
            GuildItem { id: DMS.into(), name: "Direct Messages".into(), short: "DM".into(), unread: dms.0, mentions: dms.1 as i32 };
        let guilds = rows.into_iter().map(|g| GuildItem {
            id: g.id.to_string().into(),
            short: g.name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect::<String>().into(),
            name: g.name.into(),
            unread: g.unread,
            mentions: g.mentions as i32,
        });
        ui.set_guilds(model(std::iter::once(dm).chain(guilds).collect()));
    });
}

fn set_channels(ui: &Weak<AppWindow>, rows: Vec<ChannelRow>) {
    with_ui(ui, move |ui| {
        let items = rows.into_iter().map(|c| ChannelItem {
            id: c.id.to_string().into(),
            name: if c.kind == 4 { c.name.to_uppercase() } else { c.name }.into(),
            kind: c.kind.into(),
            unread: c.unread,
            mentions: c.mentions as i32,
            enabled: c.enabled,
        });
        ui.set_channels(model(items.collect()));
    });
}

/// Ids and header titles of the selected guild (`DMS` for DMs) and channel.
fn set_selection(ui: &Weak<AppWindow>, guild: String, channel: String, guild_title: String, channel_title: String) {
    with_ui(ui, move |ui| {
        ui.set_selected_guild(guild.into());
        ui.set_selected_channel(channel.into());
        ui.set_guild_title(guild_title.into());
        ui.set_channel_title(channel_title.into());
    });
}

/// `reset`: replace everything and scroll to the newest row. Otherwise apply the `(start, remove, rows)`
/// edits in order, touching only those rows.
fn set_messages(ui: &Weak<AppWindow>, reset: bool, edits: Vec<(usize, usize, Vec<MessageRow>)>) {
    with_ui(ui, move |ui| {
        let m = ui.get_messages();
        let Some(vm) = m.as_any().downcast_ref::<VecModel<MessageItem>>() else { return };
        if reset {
            vm.set_vec(edits.into_iter().flat_map(|e| e.2).map(message_item).collect::<Vec<_>>());
            ui.invoke_scroll_to_bottom();
            return;
        }
        for (start, remove, rows) in edits {
            for _ in 0..remove {
                vm.remove(start);
            }
            let n = rows.into_iter().map(message_item).enumerate().map(|(i, it)| vm.insert(start + i, it)).count();
            if start == 0 && remove == 0 {
                ui.invoke_messages_prepended(n as i32);
            }
        }
    });
}

fn message_item(r: MessageRow) -> MessageItem {
    let blocks = r.blocks.iter().map(|b| match b {
        Block::Para(spans) => BlockItem { code: false, text: styled(spans), raw: Default::default() },
        Block::Code { text, .. } => BlockItem { code: true, text: Default::default(), raw: text.into() },
    });
    MessageItem {
        id: r.id.map(|i| i.to_string()).or(r.nonce).unwrap_or_default().into(),
        author: r.author.into(),
        time: r.time.into(),
        blocks: model(blocks.collect()),
        reply: r.reply_preview.unwrap_or_default().into(),
        thumbs: model(r.thumbs.into_iter().map(thumb_item).collect()),
        files: model(r.files.into_iter().map(|(name, url)| FileItem { name: name.into(), url: url.into() }).collect()),
        edited: r.edited,
        pending: r.pending,
        failed: r.failed,
        mine: r.mine,
    }
}

fn thumb_item(t: Thumb) -> ThumbItem {
    let (width, height) = (t.width as i32, t.height as i32);
    ThumbItem { url: t.url.into(), full: t.full.into(), width, height, image: Default::default() }
}

/// A thumbnail to show (`None`: nothing): it goes into each shown row with `url` and no image yet.
pub fn push_thumb(ui: &Weak<AppWindow>, thumb: Option<(String, Pixels)>) {
    let Some((url, px)) = thumb else { return };
    with_ui(ui, move |ui| {
        let image = slint::Image::from_rgba8(px);
        for row in ui.get_messages().iter() {
            for (i, mut t) in row.thumbs.iter().enumerate() {
                if t.url == url && t.image.size().width == 0 {
                    t.image = image.clone();
                    row.thumbs.set_row_data(i, t);
                }
            }
        }
    });
}

fn styled(spans: &[Span]) -> StyledText {
    StyledText::from_markdown(&to_markdown(spans))
        .unwrap_or_else(|_| StyledText::from_plain_text(&spans.iter().map(|s| s.text.as_str()).collect::<String>()))
}

/// Smallest replaced range: `old[start..old_end]` becomes `new[start..new_end]`.
fn splice(old: &[MessageRow], new: &[MessageRow]) -> (usize, usize, usize) {
    let p = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let s = old[p..].iter().rev().zip(new[p..].iter().rev()).take_while(|(a, b)| a == b).count();
    (p, old.len() - s, new.len() - s)
}

/// Edits turning `old` into `new`, applied in order: `(start, remove, new[range])`.
/// Rows evicted from the head (store cap) and the rest are separate edits, so the tail stays a small splice.
fn diff(old: &[MessageRow], new: &[MessageRow]) -> Vec<(usize, usize, std::ops::Range<usize>)> {
    let same = |a: &MessageRow, b: &MessageRow| a.id == b.id && a.nonce == b.nonce;
    let head = new.first().and_then(|f| old.iter().position(|o| same(o, f))).unwrap_or(0);
    let (p, old_end, new_end) = splice(&old[head..], new);
    let mut edits = vec![];
    if head > 0 {
        edits.push((0, head, 0..0));
    }
    if (p, p) != (old_end, new_end) {
        edits.push((p, old_end - p, p..new_end));
    }
    edits
}

/// Spans as CommonMark for `StyledText::from_markdown`; every literal punctuation char is escaped.
fn to_markdown(spans: &[Span]) -> String {
    fn esc(out: &mut String, s: &str) {
        for c in s.chars() {
            // Leading whitespace would start an indented code block; NBSP keeps the indent as text.
            if matches!(c, ' ' | '\t') && (out.is_empty() || out.ends_with(['\n', '\u{a0}'])) {
                out.push('\u{a0}');
                continue;
            }
            if c.is_ascii_punctuation() {
                out.push('\\');
            }
            out.push(c);
        }
    }
    // One span per line: a styled span must not straddle a line break (a blank line ends a CommonMark
    // paragraph, and code spans would turn newlines into spaces).
    let lines = spans.iter().flat_map(|sp| {
        sp.text.split('\n').enumerate().flat_map(move |(i, l)| {
            let nl = (i > 0).then(|| Span { text: "\n".into(), ..Default::default() });
            nl.into_iter().chain([Span { text: l.into(), ..sp.clone() }])
        })
    });
    let mut out = String::new();
    for sp in lines {
        if sp.text.trim().is_empty() || !(sp.bold || sp.italic || sp.strike || sp.code || sp.link.is_some()) {
            esc(&mut out, &sp.text);
            continue;
        }
        // Delimiters must touch non-space text (CommonMark flanking rules), so keep whitespace outside.
        // Code keeps its spaces: the padded fence below preserves them.
        let body = if sp.code { sp.text.as_str() } else { sp.text.trim() };
        let lead = if sp.code { "" } else { &sp.text[..sp.text.len() - sp.text.trim_start().len()] };
        let trail = if sp.code { "" } else { &sp.text[sp.text.trim_end().len()..] };
        let open = [(sp.strike, "~~"), (sp.bold, "**"), (sp.italic, "*")];
        esc(&mut out, lead);
        open.iter().filter(|d| d.0).for_each(|d| out.push_str(d.1));
        if sp.link.is_some() {
            out.push('[');
        }
        if sp.code {
            let mut run = 0;
            let longest = body.chars().fold(0, |m, c| {
                run = if c == '`' { run + 1 } else { 0 };
                m.max(run)
            });
            let fence = "`".repeat(longest + 1);
            out.push_str(&format!("{fence} {body} {fence}"));
        } else {
            esc(&mut out, body);
        }
        if let Some(url) = &sp.link {
            out.push_str("](<");
            for c in url.chars() {
                if matches!(c, '<' | '>' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push_str(">)");
        }
        open.iter().rev().filter(|d| d.0).for_each(|d| out.push_str(d.1));
        esc(&mut out, trail);
    }
    out
}

pub fn qr_status(ui: &Weak<AppWindow>, s: QrStatus) {
    match s {
        QrStatus::Qr(url) => {
            let buf = qr_pixels(&url);
            with_ui(ui, move |ui| {
                ui.set_qr(slint::Image::from_rgba8(buf));
                ui.set_login_status("Scan with the Discord mobile app".into());
            });
        }
        QrStatus::Scanned(name) => {
            with_ui(ui, move |ui| ui.set_login_status(format!("Scanned by {name}. Confirm on your phone.").into()));
        }
        QrStatus::Expired => {
            with_ui(ui, |ui| {
                ui.set_qr(slint::Image::default());
                ui.set_login_status("QR code expired. Press \"New QR code\".".into());
            });
        }
    }
}

/// QR modules scaled up with a 4-module quiet zone, black on white.
fn qr_pixels(url: &str) -> SharedPixelBuffer<Rgba8Pixel> {
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let Ok(code) = qrcode::QrCode::new(url) else { return SharedPixelBuffer::new(1, 1) };
    let w = code.width();
    let colors = code.to_colors();
    let side = (w + 2 * QUIET) * SCALE;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(side as u32, side as u32);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % side) / SCALE, (i / side) / SCALE);
        let dark = x >= QUIET
            && y >= QUIET
            && x < w + QUIET
            && y < w + QUIET
            && colors[(y - QUIET) * w + (x - QUIET)] == qrcode::Color::Dark;
        let v = if dark { 0 } else { 255 };
        *px = Rgba8Pixel { r: v, g: v, b: v, a: 255 };
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, text: &str) -> MessageRow {
        MessageRow {
            id: Some(Id(id)),
            nonce: None,
            author: "a".into(),
            time: String::new(),
            blocks: crate::markdown::parse(text),
            thumbs: vec![],
            files: vec![],
            reply_preview: None,
            edited: false,
            mine: false,
            pending: false,
            failed: false,
        }
    }

    #[test]
    fn qr_has_quiet_zone_and_finder() {
        let buf = qr_pixels("https://discord.com/ra/abc");
        let side = buf.width() as usize;
        assert_eq!(buf.height() as usize, side);
        let px = buf.as_slice();
        assert_eq!(px[0].r, 255); // quiet zone
        assert_eq!(px[24 * side + 24].r, 0); // first module of the top-left finder (4 * 6 px)
    }

    #[test]
    fn splice_ranges() {
        let old = [row(1, "a"), row(2, "b"), row(3, "c")];
        assert_eq!(splice(&old, &old), (3, 3, 3));
        let appended = [row(1, "a"), row(2, "b"), row(3, "c"), row(4, "d")];
        assert_eq!(splice(&old, &appended), (3, 3, 4));
        let prepended = [row(0, "z"), row(1, "a"), row(2, "b"), row(3, "c")];
        assert_eq!(splice(&old, &prepended), (0, 0, 1));
        let edited = [row(1, "a"), row(2, "B"), row(3, "c")];
        assert_eq!(splice(&old, &edited), (1, 2, 2));
        let deleted = [row(1, "a"), row(3, "c")];
        assert_eq!(splice(&old, &deleted), (1, 2, 1));
        let other = [row(7, "a")];
        assert_eq!(splice(&old, &other), (0, 3, 1));
        assert_eq!(splice(&[], &old), (0, 0, 3));
    }

    #[test]
    fn diff_edits() {
        let old: Vec<_> = (1..6).map(|i| row(i, "x")).collect();
        let capped: Vec<_> = (2..7).map(|i| row(i, "x")).collect(); // head evicted at the cap, one appended
        assert_eq!(diff(&old, &capped), vec![(0, 1, 0..0), (4, 0, 4..5)]);
        let mut edited = old.clone();
        edited[0] = row(1, "y");
        assert_eq!(diff(&old, &edited), vec![(0, 1, 0..1)]);
        let prepended: Vec<_> = (0..6).map(|i| row(i, "x")).collect();
        assert_eq!(diff(&old, &prepended), vec![(0, 0, 0..1)]);
        assert_eq!(diff(&old, &old), vec![]);
    }

    fn md(s: &str) -> String {
        match &crate::markdown::parse(s)[..] {
            [crate::markdown::Block::Para(spans)] => to_markdown(spans),
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn markdown_escapes_and_styles() {
        assert_eq!(md("1. a_b # <x> &amp;"), r"1\. a\_b \# \<x\> \&amp\;");
        assert_eq!(md("**bold** *it* ~~s~~"), r"**bold** *it* ~~s~~");
        let bold = Span { text: "bold ".into(), bold: true, ..Default::default() };
        let x = Span { text: "x".into(), ..Default::default() };
        assert_eq!(to_markdown(&[bold, x]), "**bold** x"); // whitespace moved outside delimiters
        assert_eq!(md("a `x*y` b"), "a ` x*y ` b");
        let tick = Span { text: "a`b".into(), code: true, ..Default::default() };
        assert_eq!(to_markdown(&[tick]), "`` a`b ``");
        assert_eq!(md("l1\nl2"), "l1\nl2");
        let code = |t: &str| to_markdown(&[Span { text: t.into(), code: true, ..Default::default() }]);
        assert_eq!(code(" a  b "), "`  a  b  `"); // inner spaces kept (one pad space stripped per side)
        assert_eq!(code("a\nb"), "` a `\n` b `");
        let evil = code("x\n\n[Official](https://evil) **y**");
        assert_eq!(evil, "` x `\n\n` [Official](https://evil) **y** `");
        assert_eq!(md("  x\n\ty"), "\u{a0}\u{a0}x\n\u{a0}y");
        assert_eq!(md("see https://e.com/a_b"), r"see [https\:\/\/e\.com\/a\_b](<https://e.com/a_b>)");
    }

    #[test]
    fn markdown_round_trips_through_slint() {
        for s in ["**b** *i* ~~s~~ `c`", "    indented", "> quote", "- item", "# head", "<b>x</b>", "a\n\nb"] {
            let m = md(s);
            assert!(slint::StyledText::from_markdown(&m).is_ok(), "{s:?} -> {m:?}");
        }
        let evil = Span { text: "x\n\n[Official](https://evil) **y**".into(), code: true, ..Default::default() };
        let m = to_markdown(&[evil]);
        let st = slint::StyledText::from_markdown(&m).unwrap();
        assert!(!format!("{st:?}").contains("Link"), "{st:?}");
    }
}
