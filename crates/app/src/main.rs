mod bridge;
mod compose;
#[cfg(debug_assertions)]
mod demo;
mod markdown;
mod nav;
mod runtime;
mod token;
mod view;

use compose::OutgoingError;
use rxcord_api::id::Id;
use runtime::Command;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();
    let ui = AppWindow::new()?;
    bridge::clear(&ui.as_weak()); // queued: runs once the event loop starts
    // Window focus, shared with the runtime (acks now, notifications in Task 13). Starts false: winit
    // reports Focused(true) when the window gets focus.
    let focused = Arc::new(AtomicBool::new(false));
    let tx = runtime::spawn_runtime(ui.as_weak(), focused.clone());
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
    ui.on_select_guild(move |g| drop(t.send(Command::SelectGuild(id(g)))));
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
    ui.on_retry_send(move |n| drop(t.send(Command::Retry(n.into()))));
    let drafts = Rc::new(RefCell::new(compose::Drafts::default()));
    let d = drafts.clone();
    ui.on_switch_draft(move |text, to| d.borrow_mut().switch(text.into(), id(to)).into());
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
