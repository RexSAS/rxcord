mod bridge;
mod demo;
mod markdown;
mod nav;
mod runtime;
mod token;
mod view;

use rxcord_api::id::Id;
use runtime::Command;
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();
    let ui = AppWindow::new()?;
    bridge::clear(&ui.as_weak()); // queued: runs once the event loop starts
    let tx = runtime::spawn_runtime(ui.as_weak());
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
    let (t, w) = (tx.clone(), ui.as_weak());
    ui.on_load_older(move || {
        if let Some(c) = w.upgrade().and_then(|u| id(u.get_selected_channel())) {
            let _ = t.send(Command::LoadOlder(c));
        }
    });
    let t = tx.clone();
    ui.on_bottom_visible(move |b| drop(t.send(Command::BottomVisible(b))));
    ui.on_thumb_visible(move |u| drop(tx.send(Command::ThumbVisible(u.into()))));
    ui.run()
}
