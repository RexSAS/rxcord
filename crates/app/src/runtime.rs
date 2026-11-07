//! Tokio runtime thread: owns the store, the gateway and REST, and pushes finished data to the UI.
use crate::{AppWindow, token};
use rxcord_api::event::Event;
use rxcord_api::gateway::driver::{ConnStatus, GatewayEvent, run_gateway};
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_api::nonce::make_nonce;
use rxcord_api::remote_auth::{QrStatus, qr_login};
use rxcord_api::{ApiError, Rest};
use rxcord_store::{Pending, Store};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinSet;

const HISTORY_PAGE: u8 = 50;

#[allow(dead_code)] // channel/message variants are sent by the UI from Task 10
pub enum Command {
    LoginToken(String),
    StartQr,
    Logout,
    OpenChannel(Id),
    LoadOlder(Id),
    Send { channel: Id, content: String, reply_to: Option<Id> },
    Retry(String /*nonce*/),
    Edit { channel: Id, message: Id, content: String },
    Delete { channel: Id, message: Id },
    ThumbVisible(String /*url*/),
}

/// Results of tasks spawned on the runtime, handled back on the loop.
enum Done {
    Nothing,
    History(Id, Result<Vec<Message>, ApiError>),
    Sent(String, Result<Box<Message>, ApiError>),
    Op(Result<(), ApiError>),
    QrToken(Result<String, ApiError>),
}

enum Input {
    Cmd(Command),
    Gw(Option<GatewayEvent>),
    Qr(Option<QrStatus>),
    Done(Done),
}

pub fn spawn_runtime(ui: slint::Weak<AppWindow>) -> UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("rxcord-runtime".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            rt.block_on(Runtime::new(ui).run(rx));
        })
        .expect("spawn runtime thread");
    tx
}

struct Runtime {
    ui: slint::Weak<AppWindow>,
    store: Store,
    rest: Option<Arc<Rest>>,
    gw: Option<mpsc::Receiver<GatewayEvent>>,
    qr: Option<mpsc::Receiver<QrStatus>>,
    /// Session tasks; replacing the set aborts them all (logout, new login).
    tasks: JoinSet<Done>,
    selected: Option<Id>,
}

async fn recv_opt<T>(r: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match r {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

impl Runtime {
    fn new(ui: slint::Weak<AppWindow>) -> Self {
        Self { ui, store: Store::new(), rest: None, gw: None, qr: None, tasks: JoinSet::new(), selected: None }
    }

    async fn run(mut self, mut cmds: UnboundedReceiver<Command>) {
        match tokio::task::spawn_blocking(token::load).await.ok().flatten() {
            Some(t) => self.start_session(t),
            None => self.show_login(""),
        }
        loop {
            let input = tokio::select! {
                c = cmds.recv() => match c {
                    Some(c) => Input::Cmd(c),
                    None => return, // UI gone
                },
                g = recv_opt(&mut self.gw) => Input::Gw(g),
                q = recv_opt(&mut self.qr) => Input::Qr(q),
                Some(Ok(d)) = self.tasks.join_next() => Input::Done(d),
            };
            match input {
                Input::Cmd(c) => self.command(c).await,
                Input::Gw(Some(GatewayEvent::Event(ev))) => self.event(ev),
                Input::Gw(Some(GatewayEvent::Status(s))) => self.status(s).await,
                Input::Gw(None) => self.gw = None,
                Input::Qr(Some(s)) => self.qr_status(s),
                Input::Qr(None) => self.qr = None,
                Input::Done(d) => self.done(d).await,
            }
        }
    }

    fn with_ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(ui);
            }
        });
    }

    fn start_session(&mut self, token: String) {
        self.end_session();
        let (tx, rx) = mpsc::channel(256);
        self.rest = Some(Arc::new(Rest::new(token.clone())));
        self.gw = Some(rx);
        self.tasks.spawn(async move {
            run_gateway(token, tx).await;
            Done::Nothing
        });
    }

    /// Dropping the gateway receiver stops `run_gateway`; dropping the task set aborts everything else.
    fn end_session(&mut self) {
        self.tasks = JoinSet::new();
        self.gw = None;
        self.qr = None;
        self.rest = None;
        self.store = Store::new();
        self.selected = None;
    }

    fn show_login(&mut self, status: &str) {
        let status = status.to_string();
        self.with_ui(move |ui| {
            ui.set_logged_in(false);
            ui.set_banner("".into());
            ui.set_login_status(status.into());
        });
        self.start_qr();
    }

    fn start_qr(&mut self) {
        self.end_session();
        let (tx, rx) = mpsc::channel(4);
        self.qr = Some(rx);
        self.tasks.spawn(async move { Done::QrToken(qr_login(tx).await) });
    }

    async fn logout(&mut self, status: &str) {
        let _ = tokio::task::spawn_blocking(token::delete).await;
        self.show_login(status); // ends the session
    }

    async fn command(&mut self, c: Command) {
        match c {
            Command::LoginToken(t) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    return;
                }
                self.with_ui(|ui| ui.set_login_status("Logging in…".into()));
                self.login(t).await;
            }
            Command::StartQr => self.show_login(""),
            Command::Logout => self.logout("").await,
            Command::OpenChannel(id) => {
                if let Some(prev) = self.selected.replace(id) {
                    self.store.close_channel(prev);
                }
                if self.store.open_channel(id) {
                    self.fetch(id, None);
                }
            }
            Command::LoadOlder(id) => {
                let before = self.store.messages(id).front().map(|m| m.id);
                if before.is_some() {
                    self.fetch(id, before);
                }
            }
            Command::Send { channel, content, reply_to } => {
                let nonce = make_nonce(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64));
                self.send(Pending { nonce, channel, content, reply_to, failed: false });
            }
            Command::Retry(nonce) => {
                if let Some(p) = self.store.take_pending(&nonce) {
                    self.send(Pending { failed: false, ..p });
                }
            }
            Command::Edit { channel, message, content } => {
                if let Some(rest) = self.rest.clone() {
                    self.tasks.spawn(async move { Done::Op(rest.edit_message(channel, message, &content).await.map(drop)) });
                }
            }
            Command::Delete { channel, message } => {
                if let Some(rest) = self.rest.clone() {
                    self.tasks.spawn(async move { Done::Op(rest.delete_message(channel, message).await) });
                }
            }
            Command::ThumbVisible(_) => {} // image loading: Task 12
        }
    }

    async fn login(&mut self, token: String) {
        let t = token.clone();
        let _ = tokio::task::spawn_blocking(move || token::save(&t)).await;
        self.start_session(token);
    }

    fn fetch(&mut self, channel: Id, before: Option<Id>) {
        if let Some(rest) = self.rest.clone() {
            self.tasks.spawn(async move { Done::History(channel, rest.messages(channel, before, HISTORY_PAGE).await) });
        }
    }

    fn send(&mut self, p: Pending) {
        let Some(rest) = self.rest.clone() else { return };
        let (channel, nonce, content, reply_to) = (p.channel, p.nonce.clone(), p.content.clone(), p.reply_to);
        self.store.add_pending(p);
        self.tasks.spawn(async move {
            let r = rest.send_message(channel, &content, &nonce, reply_to).await.map(Box::new);
            Done::Sent(nonce, r)
        });
    }

    fn event(&mut self, ev: Event) {
        let ready = matches!(ev, Event::Ready(_));
        let _changes = self.store.apply(&ev); // pushed to the UI from Task 10
        if ready {
            tracing::info!(guilds = self.store.guilds().len(), "READY");
            self.with_ui(|ui| ui.set_logged_in(true));
            // READY drops stored messages; refetch the open channel.
            if let Some(id) = self.selected
                && self.store.open_channel(id)
            {
                self.fetch(id, None);
            }
        }
    }

    async fn status(&mut self, s: ConnStatus) {
        tracing::info!(?s, "gateway status");
        let banner = match s {
            ConnStatus::AuthFailed => return self.logout("Login failed or session expired. Please log in again.").await,
            ConnStatus::Connected => String::new(),
            ConnStatus::Connecting => "Connecting…".into(),
            ConnStatus::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})…"),
        };
        self.with_ui(move |ui| ui.set_banner(banner.into()));
    }

    fn qr_status(&mut self, s: QrStatus) {
        match s {
            QrStatus::Qr(url) => {
                let buf = qr_pixels(&url);
                self.with_ui(move |ui| {
                    ui.set_qr(slint::Image::from_rgba8(buf));
                    ui.set_login_status("Scan with the Discord mobile app".into());
                });
            }
            QrStatus::Scanned(name) => {
                self.with_ui(move |ui| ui.set_login_status(format!("Scanned by {name}. Confirm on your phone.").into()));
            }
            QrStatus::Expired => {
                self.with_ui(|ui| {
                    ui.set_qr(slint::Image::default());
                    ui.set_login_status("QR code expired. Press \"New QR code\".".into());
                });
            }
        }
    }

    async fn done(&mut self, d: Done) {
        let err = match d {
            Done::Nothing => return,
            Done::History(ch, Ok(msgs)) => {
                let _changes = self.store.prepend_history(ch, msgs);
                return;
            }
            Done::Sent(nonce, Ok(m)) => {
                // Don't wait for the gateway echo; apply dedupes the later MESSAGE_CREATE.
                self.store.take_pending(&nonce);
                self.store.apply(&Event::MessageCreate(m));
                return;
            }
            Done::Sent(nonce, Err(e)) => {
                self.store.fail_pending(&nonce);
                e
            }
            Done::Op(Ok(())) => return,
            Done::QrToken(Ok(t)) => return self.login(t).await,
            Done::QrToken(Err(e)) => {
                tracing::warn!("QR login failed: {e}");
                return;
            }
            Done::History(_, Err(e)) | Done::Op(Err(e)) => e,
        };
        match err {
            ApiError::Unauthorized => self.logout("Session expired. Please log in again.").await,
            e => tracing::warn!("request failed: {e}"),
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
    fn qr_has_quiet_zone_and_finder() {
        let buf = qr_pixels("https://discord.com/ra/abc");
        let side = buf.width() as usize;
        assert_eq!(buf.height() as usize, side);
        let px = buf.as_slice();
        assert_eq!(px[0].r, 255); // quiet zone
        assert_eq!(px[24 * side + 24].r, 0); // first module of the top-left finder (4 * 6 px)
    }
}
