#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod bridge;
mod captcha;
mod compose;
#[cfg(debug_assertions)]
mod demo;
mod images;
mod invite;
mod markdown;
mod nav;
mod notify;
mod runtime;
mod settings;
mod token;
mod view;

use compose::OutgoingError;
use rxcord_api::id::Id;
use runtime::Command;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(std::env::var("RUST_LOG").ok().as_deref()))
        .init();
    // Software by default; SLINT_BACKEND set by the user wins. Keep winit: the focus hook needs it.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        let renderer = if std::env::var("RXCORD_RENDERER").is_ok_and(|v| v == "gpu") { "femtovg" } else { "software" };
        tracing::info!(renderer, "selecting renderer");
        slint::BackendSelector::new().backend_name("winit".into()).renderer_name(renderer.into()).select()?;
    }
    let ui = AppWindow::new()?;
    bridge::clear(&ui.as_weak()); // queued: runs once the event loop starts
    let saved = settings::load();
    ui.window().set_size(slint::LogicalSize::new(saved.width, saved.height));
    if let (Some(x), Some(y)) = (saved.x, saved.y) {
        ui.window().set_position(slint::PhysicalPosition::new(x, y)); // ignored on Wayland
    }
    ui.set_tree_height(saved.tree_height);
    let last_guild = Arc::new(AtomicU64::new(saved.last_guild.unwrap_or(0)));
    let (w, lg) = (ui.as_weak(), last_guild.clone());
    ui.window().on_close_requested(move || {
        if let Some(ui) = w.upgrade() {
            let win = ui.window();
            let size = win.size().to_logical(win.scale_factor());
            let pos = win.position();
            let g = lg.load(Ordering::Relaxed);
            settings::save(&settings::Settings {
                width: size.width,
                height: size.height,
                x: Some(pos.x),
                y: Some(pos.y),
                tree_height: ui.get_tree_height(),
                last_guild: (g != 0).then_some(g),
            });
        }
        slint::CloseRequestResponse::HideWindow
    });
    // Window focus, shared with the runtime (acks now, notifications in Task 13). Starts false: winit
    // reports Focused(true) when the window gets focus.
    let focused = Arc::new(AtomicBool::new(false));
    let tx = runtime::spawn_runtime(ui.as_weak(), focused.clone(), last_guild);
    let t = tx.clone();
    ui.window().on_winit_window_event(move |_, ev| {
        if let WindowEvent::Focused(f) = ev {
            focused.store(*f, Ordering::Relaxed);
            let _ = t.send(Command::FocusChanged);
        }
        EventResult::Propagate
    });
    let id = |s: slint::SharedString| s.parse().ok().map(Id);
    let t = tx.clone();
    ui.on_login_token(move |s| drop(t.send(Command::LoginToken(s.into()))));
    let t = tx.clone();
    ui.on_start_qr(move || drop(t.send(Command::StartQr)));
    let t = tx.clone();
    ui.on_logout(move || drop(t.send(Command::Logout)));
    let t = tx.clone();
    ui.on_toggle_node(move |n| drop(id(n).map(|n| t.send(Command::ToggleNode(n)))));
    let t = tx.clone();
    ui.on_close_tab(move |c| drop(id(c).map(|c| t.send(Command::CloseTab(c)))));
    let t = tx.clone();
    ui.on_select_channel(move |c| drop(id(c).map(|c| t.send(Command::OpenChannel(c)))));
    let selected = move |w: &slint::Weak<AppWindow>| w.upgrade().and_then(|u| id(u.get_selected_channel()));
    let (t, w) = (tx.clone(), ui.as_weak());
    ui.on_load_older(move || drop(selected(&w).map(|c| t.send(Command::LoadOlder(c)))));
    let (t, w) = (tx.clone(), ui.as_weak());
    ui.on_retry(move || drop(selected(&w).map(|c| t.send(Command::OpenChannel(c)))));
    let (t, w) = (tx.clone(), ui.as_weak());
    ui.on_send(move |text| {
        if let Some(ui) = w.upgrade() {
            send(&ui, &t, &text);
        }
    });
    let t = tx.clone();
    ui.on_start_edit(move |m| drop(t.send(Command::StartEdit(id(m)))));
    let (t, w) = (tx.clone(), ui.as_weak());
    ui.on_delete_message(move |m| {
        if let (Some(channel), Some(message)) = (selected(&w), id(m)) {
            let _ = t.send(Command::Delete { channel, message });
        }
    });
    let t = tx.clone();
    ui.on_join_invite(move |m, code| {
        if let Some(message) = id(m) {
            let _ = t.send(Command::JoinInvite { message, code: code.into() });
        }
    });
    let t = tx.clone();
    ui.on_retry_send(move |n| drop(t.send(Command::Retry(n.into()))));
    let drafts = Rc::new(RefCell::new(compose::Drafts::default()));
    let d = drafts.clone();
    ui.on_switch_draft(move |text, to| d.borrow_mut().switch(text.into(), id(to)).into());
    let d = drafts.clone();
    ui.on_carry(move |text, to| d.borrow_mut().carry(text.into(), id(to)).into());
    let d = drafts.clone();
    ui.on_session_user(move |me| {
        if let Some(me) = id(me) {
            d.borrow_mut().set_user(me);
        }
    });
    let w = ui.as_weak();
    ui.on_restore_draft(move |ch, text| {
        let (Some(ui), Some(ch)) = (w.upgrade(), id(ch)) else { return };
        // While editing, the draft is in the stash.
        let editing = !ui.get_edit_id().is_empty();
        let draft = if editing { ui.get_edit_stash() } else { ui.get_composer_text() };
        match drafts.borrow_mut().restore(ch, &text, &draft) {
            Some(t) if editing => ui.set_edit_stash(t.into()),
            Some(t) => ui.set_composer_text(t.into()),
            None => {}
        }
    });
    let t = tx.clone();
    ui.on_bottom_visible(move |b| drop(t.send(Command::BottomVisible(b))));
    ui.on_thumb_visible(move |u| drop(tx.send(Command::ThumbVisible(u.into()))));
    ui.on_open_url(|u| {
        // only web links: never hand other schemes from message data to the OS opener
        if u.starts_with("https://") || u.starts_with("http://") {
            let u = u.to_string();
            std::thread::spawn(move || opener::open_browser(&u).map_err(|e| tracing::warn!("open {u}: {e}")));
        }
    });
    ui.run()
}

/// Composer submit: a new message leaves the composer at once (it shows as a pending row); an edit keeps
/// the composer in edit mode until the runtime reports the result.
fn send(ui: &AppWindow, tx: &tokio::sync::mpsc::UnboundedSender<Command>, text: &str) {
    let id = |s: slint::SharedString| s.parse().ok().map(Id);
    let Some(channel) = id(ui.get_selected_channel()) else { return };
    let content = match compose::validate_outgoing(text) {
        Ok(c) => c.to_string(),
        Err(OutgoingError::Empty) => return,
        Err(OutgoingError::TooLong(n)) => {
            return ui.set_composer_error(format!("Message is too long ({n}/{}).", compose::MAX_CHARS).into());
        }
    };
    ui.set_composer_error("".into());
    if let Some(message) = id(ui.get_edit_id()) {
        ui.set_edit_submitted(text.into()); // see `compose::after_edit`
        let _ = tx.send(Command::Edit { channel, message, content });
        return;
    }
    let _ = tx.send(Command::Send { channel, content, reply_to: id(ui.get_reply_id()) });
    ui.set_composer_text("".into());
    ui.set_reply_id("".into());
}

/// RUST_LOG (default `warn`), but websocket frame traces never: they may contain the Identify token.
fn log_filter(env: Option<&str>) -> EnvFilter {
    let f = env.and_then(|e| EnvFilter::try_new(e).ok()).unwrap_or_else(|| EnvFilter::new("warn"));
    let off = |t: &str| t.parse().expect("static directive");
    f.add_directive(off("tungstenite=off")).add_directive(off("tokio_tungstenite=off"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn log_filter_silences_websocket_frames() {
        for env in [None, Some("trace"), Some("trace,tungstenite=trace,tokio_tungstenite=debug"), Some("bad=[")] {
            let f = super::log_filter(env).to_string();
            assert!(f.contains("tungstenite=off") && f.contains("tokio_tungstenite=off"), "{env:?}: {f}");
            assert!(!f.contains("tungstenite=trace") && !f.contains("tungstenite=debug"), "{env:?}: {f}");
        }
    }
}
