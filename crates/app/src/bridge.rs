//! How data reaches the UI: view rows -> Slint structs/models, pushed via `invoke_from_event_loop`.
use crate::markdown::{Block, Span};
use crate::view::{self, ChannelRow, GuildRow, MessageRow};
use crate::{AppWindow, BlockItem, ChannelItem, GuildItem, MessageItem};
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
    });
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

/// Sends the selected channel's rows as a minimal splice against `shown` (what the UI holds).
pub fn push_messages(ui: &Weak<AppWindow>, s: &Store, selected: Option<Id>, shown: &mut Vec<MessageRow>) {
    let rows = selected.map(|c| view::message_rows(s, c)).unwrap_or_default();
    let (start, old_end, new_end) = splice(shown, &rows);
    if (start, start) != (old_end, new_end) {
        set_messages(ui, start, old_end - start, rows[start..new_end].to_vec());
    }
    *shown = rows;
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

/// Replaces `remove` rows at `start` with `rows`, touching only those rows of the message model.
fn set_messages(ui: &Weak<AppWindow>, start: usize, remove: usize, rows: Vec<MessageRow>) {
    with_ui(ui, move |ui| {
        let m = ui.get_messages();
        let Some(vm) = m.as_any().downcast_ref::<VecModel<MessageItem>>() else { return };
        let items = rows.into_iter().map(message_item);
        if start == 0 && remove == vm.row_count() {
            // other channel (or first fill): start at the newest message
            vm.set_vec(items.collect::<Vec<_>>());
            ui.invoke_scroll_to_bottom();
            return;
        }
        for _ in 0..remove {
            vm.remove(start);
        }
        let n = items.enumerate().map(|(i, it)| vm.insert(start + i, it)).count();
        if start == 0 && remove == 0 {
            ui.invoke_messages_prepended(n as i32);
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
        thumbs: model(r.thumbs.into_iter().map(Into::into).collect()),
        files: model(r.files.into_iter().map(|(name, _)| name.into()).collect()),
        edited: r.edited,
        pending: r.pending,
        failed: r.failed,
    }
}

fn styled(spans: &[Span]) -> StyledText {
    StyledText::from_markdown(&to_markdown(spans))
        .unwrap_or_else(|_| StyledText::from_plain_text(&spans.iter().map(|s| s.text.as_str()).collect::<String>()))
}

/// Smallest replaced range: `old[start..old_end]` becomes `new[start..new_end]`.
pub fn splice(old: &[MessageRow], new: &[MessageRow]) -> (usize, usize, usize) {
    let p = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let s = old[p..].iter().rev().zip(new[p..].iter().rev()).take_while(|(a, b)| a == b).count();
    (p, old.len() - s, new.len() - s)
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
    let mut out = String::new();
    for sp in spans {
        let body = sp.text.trim();
        if body.is_empty() || !(sp.bold || sp.italic || sp.strike || sp.code || sp.link.is_some()) {
            esc(&mut out, &sp.text);
            continue;
        }
        // Delimiters must touch non-space text (CommonMark flanking rules), so keep whitespace outside.
        let open = [(sp.strike, "~~"), (sp.bold, "**"), (sp.italic, "*")];
        esc(&mut out, &sp.text[..sp.text.len() - sp.text.trim_start().len()]);
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
        esc(&mut out, &sp.text[sp.text.trim_end().len()..]);
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
        assert_eq!(md("  x\n\ty"), "\u{a0}\u{a0}x\n\u{a0}y");
        assert_eq!(md("see https://e.com/a_b"), r"see [https\:\/\/e\.com\/a\_b](<https://e.com/a_b>)");
    }

    #[test]
    fn markdown_round_trips_through_slint() {
        for s in ["**b** *i* ~~s~~ `c`", "    indented", "> quote", "- item", "# head", "<b>x</b>", "a\n\nb"] {
            let m = md(s);
            assert!(slint::StyledText::from_markdown(&m).is_ok(), "{s:?} -> {m:?}");
        }
    }
}
