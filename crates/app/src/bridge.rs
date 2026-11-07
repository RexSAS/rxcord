//! How data reaches the UI: view rows -> Slint structs/models, pushed via `invoke_from_event_loop`.
use crate::markdown::{Block, Span};
use crate::nav::Nav;
use crate::settings::Settings;
use crate::voice::Bar;
use crate::images::ThumbUpdate;
use crate::view::{self, ChannelRow, MessageRow, Thumb, TreeRow};
use crate::{AppWindow, FileItem, MessageItem, TabItem, ThumbItem, TreeItem};
use rxcord_api::id::Id;
use rxcord_api::remote_auth::QrStatus;
use rxcord_store::Store;
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, StyledText, VecModel, Weak};

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
        ui.set_tree(model(vec![])); // must stay a VecModel, see `set_tree`
        ui.set_tabs(ModelRc::default());
        ui.set_messages(model(vec![])); // must stay a VecModel, see `set_messages`
        ui.set_selected_channel("".into());
        ui.set_me_name("".into());
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
    ui.set_row_error_ok(false);
}

/// Opens an http(s) URL in the default browser (through the UI's open-url handler, which checks the scheme).
pub fn open_url(ui: &Weak<AppWindow>, url: String) {
    with_ui(ui, move |ui| ui.invoke_open_url(url.into()));
}

/// A note under message `id` (`ok`: green), e.g. the result of joining an invite. `open`: also open
/// this https URL in the browser.
pub fn row_note(ui: &Weak<AppWindow>, id: Id, msg: String, ok: bool, open: Option<String>) {
    with_ui(ui, move |ui| {
        ui.set_row_error_id(id.to_string().into());
        ui.set_row_error(msg.into());
        ui.set_row_error_ok(ok);
        if let Some(u) = open {
            ui.invoke_open_url(u.into());
        }
    });
}

/// Tree and tabs. Rows that didn't change aren't touched, so a busy server doesn't redraw the tree.
pub fn push_tree(ui: &Weak<AppWindow>, s: &Store, nav: &mut Nav) {
    let rows = view::tree_rows(s, &nav.expanded, &nav.forbidden, &nav.speaking, &nav.settings);
    let tabs: Vec<TabItem> = view::tab_rows(s, nav.tabs.ids()).into_iter().map(tab_item).collect();
    let edits: Option<Vec<(usize, TreeRow)>> = (rows.len() == nav.tree.len())
        .then(|| rows.iter().enumerate().filter(|(i, r)| nav.tree[*i] != **r).map(|(i, r)| (i, r.clone())).collect());
    let full = edits.is_none().then(|| rows.clone());
    nav.tree = rows;
    with_ui(ui, move |ui| {
        let m = ui.get_tree();
        match (full, m.as_any().downcast_ref::<VecModel<TreeItem>>()) {
            (None, Some(vm)) => edits.into_iter().flatten().for_each(|(i, r)| vm.set_row_data(i, tree_item(r))),
            (Some(rows), Some(vm)) => vm.set_vec(rows.into_iter().map(tree_item).collect::<Vec<_>>()),
            _ => {}
        }
        if ui.get_tabs().iter().ne(tabs.iter().cloned()) {
            ui.set_tabs(model(tabs));
        }
    });
}

/// The voice bar (text "" hides it).
pub fn voice_bar(ui: &Weak<AppWindow>, bar: Bar) {
    with_ui(ui, move |ui| {
        ui.set_voice(bar.text.into());
        ui.set_voice_muted(bar.muted);
        ui.set_voice_deafened(bar.deafened);
        ui.set_voice_level(bar.level);
    });
}

/// Fill the voice settings popup and show it. "System default" is entry 0; a saved device that isn't
/// plugged in is still listed so the choice shows.
pub fn voice_settings(ui: &Weak<AppWindow>, s: &Settings, inputs: Vec<String>, outputs: Vec<String>) {
    let list = |mut devs: Vec<String>, saved: &Option<String>| {
        if let Some(d) = saved.as_ref().filter(|d| !devs.contains(d)) {
            devs.push(d.clone());
        }
        let at = saved.as_ref().and_then(|d| devs.iter().position(|x| x == d)).map_or(0, |i| i as i32 + 1);
        let names: Vec<SharedString> = std::iter::once("System default".to_string()).chain(devs).map(Into::into).collect();
        (names, at)
    };
    let ((ins, i), (outs, o)) = (list(inputs, &s.input_device), list(outputs, &s.output_device));
    let (threshold, noise) = (s.vad_threshold, s.noise_suppression);
    with_ui(ui, move |ui| {
        ui.set_vs_inputs(model(ins));
        ui.set_vs_input(i);
        ui.set_vs_outputs(model(outs));
        ui.set_vs_output(o);
        ui.set_vs_threshold(threshold);
        ui.set_vs_noise(noise);
        ui.invoke_show_voice_settings();
    });
}

/// A status bar notice for a few seconds (a newer one isn't cut short by an older timer).
pub fn voice_notice(ui: &Weak<AppWindow>, text: String) {
    with_ui(ui, move |ui| {
        ui.set_notice(text.clone().into());
        let w = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_secs(5), move || {
            if let Some(ui) = w.upgrade()
                && ui.get_notice() == text.as_str()
            {
                ui.set_notice("".into());
            }
        });
    });
}

fn tree_item(r: TreeRow) -> TreeItem {
    let m = r.member.as_ref();
    TreeItem {
        member: m.is_some(),
        speaking: m.is_some_and(|m| m.speaking),
        muted: m.is_some_and(|m| m.muted),
        deafened: m.is_some_and(|m| m.deafened),
        volume: m.map_or(1.0, |m| m.volume),
        muted_for_me: m.is_some_and(|m| m.muted_for_me),
        id: r.row.id.to_string().into(),
        name: if r.row.kind == 4 && !r.node { r.row.name.to_uppercase() } else { r.row.name }.into(),
        node: r.node,
        expanded: r.expanded,
        depth: r.depth.into(),
        kind: r.row.kind.into(),
        unread: r.row.unread,
        mentions: r.row.mentions as i32,
        enabled: r.row.enabled,
    }
}

fn tab_item(c: ChannelRow) -> TabItem {
    TabItem { id: c.id.to_string().into(), name: c.name.into(), kind: c.kind.into(), unread: c.unread, mentions: c.mentions as i32 }
}

/// The active tab. `carry`: the previous one was deleted, its composer text moves along.
pub fn push_selection(ui: &Weak<AppWindow>, selected: Option<Id>, carry: bool) {
    let id = selected.map(|c| c.to_string()).unwrap_or_default();
    with_ui(ui, move |ui| {
        if carry {
            ui.set_carry_draft(true);
        }
        ui.set_selected_channel(id.into());
    });
}

/// The status bar's update entry: `label` ("" hides it) and what clicking does (see `update::parse_click`).
pub fn update_state(ui: &Weak<AppWindow>, label: String, action: &str) {
    let action = action.to_string();
    with_ui(ui, move |ui| {
        ui.set_update_label(label.into());
        ui.set_update_action(action.into());
    });
}

/// Status bar name.
pub fn push_me(ui: &Weak<AppWindow>, s: &Store) {
    let name = s.me().map(|u| u.display_name().to_string()).unwrap_or_default();
    with_ui(ui, move |ui| ui.set_me_name(name.into()));
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
    MessageItem {
        id: r.id.map(|i| i.to_string()).or(r.nonce.clone()).unwrap_or_default().into(),
        body: styled(&body_spans(&r)),
        author: r.author.into(),
        time: r.time.into(),
        reply: r.reply_preview.unwrap_or_default().into(),
        thumbs: model(r.thumbs.into_iter().map(thumb_item).collect()),
        files: model(r.files.into_iter().map(|(name, url)| FileItem { name: name.into(), url: url.into() }).collect()),
        invites: model(r.invites.into_iter().map(Into::into).collect()),
        edited: r.edited,
        pending: r.pending,
        failed: r.failed,
        mine: r.mine,
    }
}

/// Compact row text: "HH:MM Name: " then every block (code as inline code, line by line), blank-line separated.
fn body_spans(r: &MessageRow) -> Vec<Span> {
    let plain = |t: &str| Span { text: t.into(), ..Default::default() };
    let mut out = vec![plain(&format!("{} ", r.time)), Span { text: r.author.clone(), bold: true, ..Default::default() }, plain(": ")];
    for (i, b) in r.blocks.iter().enumerate() {
        if i > 0 || matches!(b, Block::Code { .. }) {
            out.push(plain(if i > 0 { "\n\n" } else { "\n" }));
        }
        match b {
            Block::Para(spans) => out.extend(spans.iter().cloned()),
            Block::Code { text, .. } => {
                for (j, l) in text.trim_end_matches('\n').split('\n').enumerate() {
                    if j > 0 {
                        out.push(plain("\n"));
                    }
                    out.push(Span { text: l.into(), code: true, ..Default::default() });
                }
            }
        }
    }
    if r.edited {
        out.push(Span { text: " (edited)".into(), italic: true, ..Default::default() });
    }
    out
}

fn thumb_item(t: Thumb) -> ThumbItem {
    let (width, height) = (t.width as i32, t.height as i32);
    ThumbItem { url: t.url.into(), full: t.full.into(), width, height, image: Default::default() }
}

/// Puts a thumbnail into the shown rows with its URL and no image yet, and drops the images the cache
/// evicted, so rows never hold more than the cache's byte budget.
pub fn push_thumb(ui: &Weak<AppWindow>, u: ThumbUpdate) {
    if u.show.is_none() && u.evicted.is_empty() {
        return;
    }
    with_ui(ui, move |ui| {
        let show = u.show.map(|(url, px)| (url, slint::Image::from_rgba8(px)));
        for row in ui.get_messages().iter() {
            for (i, mut t) in row.thumbs.iter().enumerate() {
                let loaded = t.image.size().width != 0;
                match &show {
                    Some((url, img)) if !loaded && t.url == url.as_str() => t.image = img.clone(),
                    _ if loaded && u.evicted.iter().any(|e| t.url == e.as_str()) => t.image = Default::default(),
                    _ => continue,
                }
                row.thumbs.set_row_data(i, t);
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
            with_ui(ui, move |ui| {
                ui.set_login_error("".into()); // a new login attempt
                ui.set_login_status(format!("Scanned by {name}. Confirm on your phone.").into());
            });
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

    #[test]
    fn body_has_header_code_lines_and_edited_mark() {
        let r = MessageRow {
            id: None,
            nonce: None,
            author: "bob".into(),
            time: "12:01".into(),
            blocks: vec![
                Block::Para(vec![Span { text: "hi".into(), ..Default::default() }]),
                Block::Code { lang: None, text: "a\n b\n".into() },
            ],
            thumbs: vec![],
            files: vec![],
            invites: vec![],
            reply_preview: None,
            edited: true,
            mine: false,
            pending: false,
            failed: false,
        };
        let s = body_spans(&r);
        let text: String = s.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(text, "12:01 bob: hi\n\na\n b (edited)");
        assert!(s[1].bold && s.iter().filter(|x| x.code).count() == 2);
        let empty = MessageRow { blocks: vec![], edited: false, ..r };
        assert_eq!(body_spans(&empty).len(), 3, "attachment-only message: just the header");
    }

    fn row(id: u64, text: &str) -> MessageRow {
        MessageRow {
            id: Some(Id(id)),
            nonce: None,
            author: "a".into(),
            time: String::new(),
            blocks: crate::markdown::parse(text),
            thumbs: vec![],
            files: vec![],
            invites: vec![],
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
