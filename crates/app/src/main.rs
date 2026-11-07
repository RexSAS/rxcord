mod markdown;
mod runtime;
mod token;
#[allow(dead_code)] // row builders are wired to the UI in Task 10
mod view;

use runtime::Command;
use tracing_subscriber::EnvFilter;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();
    let ui = AppWindow::new()?;
    let tx = runtime::spawn_runtime(ui.as_weak());
    let t = tx.clone();
    ui.on_login_token(move |s| drop(t.send(Command::LoginToken(s.into()))));
    let t = tx.clone();
    ui.on_start_qr(move || drop(t.send(Command::StartQr)));
    ui.on_logout(move || drop(tx.send(Command::Logout)));
    ui.run()
}
