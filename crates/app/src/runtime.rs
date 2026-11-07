//! Tokio runtime thread: owns the store, the gateway and REST, and pushes finished data to the UI.
use crate::nav::{Ack, HISTORY_PAGE, Nav};
use crate::{AppWindow, bridge, compose, images, token};
use rxcord_api::event::Event;
use rxcord_api::gateway::driver::{ConnStatus, GatewayEvent, run_gateway};
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_api::remote_auth::{QrStatus, qr_login};
use rxcord_api::{ApiError, Rest};
use rxcord_store::{Change, Pending, Store};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinSet;
use tokio::time::Instant;

pub enum Command {
    LoginToken(String),
    StartQr,
    Logout,
    /// `None` = Direct Messages
    SelectGuild(Option<Id>),
    OpenChannel(Id),
    BottomVisible(bool),
    /// The window gained or lost focus (state is in the shared `focused` flag).
    FocusChanged,
    LoadOlder(Id),
    Send { channel: Id, content: String, reply_to: Option<Id> },
    Retry(String /*nonce*/),
    /// Put own message `Some(id)` (or the newest own one) into the composer for editing.
    StartEdit(Option<Id>),
    Edit { channel: Id, message: Id, content: String },
    Delete { channel: Id, message: Id },
    ThumbVisible(String /*url*/),
}

/// Results of tasks spawned on the runtime, handled back on the loop.
enum Done {
    Nothing,
    History { channel: Id, generation: u64, before: Option<Id>, result: Result<Vec<Message>, ApiError> },
    Acked(Id, Id, Result<(), ApiError>),
    AckDue(Id),
    Sent(String, Result<Box<Message>, ApiError>),
    Edited(Id, Result<Box<Message>, ApiError>),
    Deleted { channel_id: Id, id: Id, result: Result<(), ApiError> },
    QrToken(Result<String, ApiError>),
}

enum Input {
    Cmd(Command),
    Gw(Option<GatewayEvent>),
    Qr(Option<QrStatus>),
    Done(Done),
    Thumb(images::Loaded),
}

/// `focused` is true while the window has focus; written by the UI thread (main.rs), read here and by notifications.
pub fn spawn_runtime(ui: slint::Weak<AppWindow>, focused: Arc<AtomicBool>) -> UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("rxcord-runtime".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            let (store, tasks, nav, thumbs) = (Store::new(), JoinSet::new(), Nav::default(), images::Thumbs::new());
            rt.block_on(Runtime { ui, store, rest: None, gw: None, qr: None, tasks, nav, focused, thumbs }.run(rx));
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
    nav: Nav,
    focused: Arc<AtomicBool>,
    thumbs: images::Thumbs,
}

async fn recv_opt<T>(r: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match r {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

impl Runtime {
    async fn run(mut self, mut cmds: UnboundedReceiver<Command>) {
        if cfg!(debug_assertions) && std::env::var_os("RXCORD_DEMO").is_some() {
            #[cfg(debug_assertions)]
            {
                let (ready, msgs) = crate::demo::events();
                self.event(ready);
                self.command(Command::SelectGuild(Some(Id(crate::demo::GUILD)))).await;
                self.command(Command::OpenChannel(Id(crate::demo::CHANNEL))).await;
                msgs.into_iter().for_each(|m| self.event(m));
                self.command(crate::demo::failed_send()).await;
            }
        } else {
            match tokio::task::spawn_blocking(token::load).await.ok().flatten() {
                Some(t) => self.start_session(t),
                None => self.show_login(""),
            }
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
                Some(Ok(l)) = self.thumbs.tasks.join_next() => Input::Thumb(l),
            };
            match input {
                Input::Cmd(c) => self.command(c).await,
                Input::Gw(Some(GatewayEvent::Event(ev))) => self.event(ev),
                Input::Gw(Some(GatewayEvent::Status(s))) => self.status(s).await,
                Input::Gw(None) => self.gw = None,
                Input::Qr(Some(s)) => bridge::qr_status(&self.ui, s),
                Input::Qr(None) => self.qr = None,
                Input::Done(d) => self.done(d).await,
                Input::Thumb(l) => bridge::push_thumb(&self.ui, self.thumbs.loaded(l, &self.nav.shown)),
            }
        }
    }

    fn with_ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        bridge::with_ui(&self.ui, f);
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
        // Unsent text goes back into the drafts, never auto-sent: the next login may be another account.
        bridge::restore_drafts(&self.ui, compose::unsent_drafts(self.store.all_pending()));
        self.store = Store::new();
        self.nav = Nav::default();
        self.thumbs = images::Thumbs::new(); // drops the cache and aborts its fetches
        bridge::clear(&self.ui);
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
            Command::SelectGuild(g) => {
                self.nav.guild = g;
                bridge::push_channels(&self.ui, &self.store, self.nav.guild);
                bridge::push_selection(&self.ui, &self.store, self.nav.guild, self.nav.selected);
            }
            Command::OpenChannel(id) => {
                bridge::set_error(&self.ui, String::new());
                if self.nav.selected == Some(id) {
                    // re-click: retry a failed first load
                    if self.store.messages(id).is_empty() {
                        self.fetch(id, None);
                    }
                    return;
                }
                if self.nav.open(&mut self.store, id) {
                    self.fetch(id, None);
                }
                bridge::push_selection(&self.ui, &self.store, self.nav.guild, self.nav.selected);
                bridge::push_messages(&self.ui, &self.store, &mut self.nav);
                self.maybe_ack(id);
            }
            Command::LoadOlder(id) => {
                if let Some(before) = self.nav.older_before(&self.store, id) {
                    self.fetch(id, Some(before));
                }
            }
            Command::BottomVisible(b) => {
                self.nav.at_bottom = b;
                if let Some(c) = self.nav.selected {
                    self.maybe_ack(c);
                }
            }
            Command::FocusChanged => {
                if let Some(c) = self.nav.selected {
                    self.maybe_ack(c);
                }
            }
            Command::Send { channel, content, reply_to } => {
                let p = compose::new_pending(channel, content, reply_to);
                let c = self.store.add_pending(p.clone());
                self.changed(vec![c]);
                self.post(p);
            }
            Command::Retry(nonce) => {
                if let Some(p) = self.store.retry_pending(&nonce) {
                    self.changed(vec![Change::Messages(p.channel)]);
                    self.post(p);
                }
            }
            Command::StartEdit(id) => {
                if let Some((m, text)) = self.nav.selected.and_then(|ch| compose::edit_target(&self.store, ch, id)) {
                    bridge::start_edit(&self.ui, m, text);
                }
            }
            Command::Edit { channel, message, content } => {
                let Some(rest) = self.rest.clone() else { return bridge::edit_done(&self.ui, message, false) };
                self.tasks.spawn(async move {
                    Done::Edited(message, rest.edit_message(channel, message, &content).await.map(Box::new))
                });
            }
            Command::Delete { channel, message: id } => {
                let Some(rest) = self.rest.clone() else { return bridge::delete_failed(&self.ui, id) };
                self.tasks.spawn(async move {
                    Done::Deleted { channel_id: channel, id, result: rest.delete_message(channel, id).await }
                });
            }
            Command::ThumbVisible(url) => bridge::push_thumb(&self.ui, self.thumbs.request(url, &self.nav.shown)),
        }
    }

    async fn login(&mut self, token: String) {
        let t = token.clone();
        let _ = tokio::task::spawn_blocking(move || token::save(&t)).await;
        self.start_session(token);
    }

    /// One history request per channel at a time.
    fn fetch(&mut self, channel: Id, before: Option<Id>) {
        if let Some(rest) = self.rest.clone()
            && self.nav.in_flight.insert(channel)
        {
            let generation = self.nav.generation;
            self.tasks.spawn(async move {
                let result = rest.messages(channel, before, HISTORY_PAGE).await;
                Done::History { channel, generation, before, result }
            });
        }
    }

    fn maybe_ack(&mut self, ch: Id) {
        match self.nav.plan_ack(&self.store, ch, self.focused.load(Ordering::Relaxed), Instant::now()) {
            Ack::No => {}
            Ack::Now => {
                // The channel's last id, not our newest row: that one may have been deleted.
                let last = self.store.channel(ch).and_then(|c| c.last_message_id);
                if let (Some(rest), Some(last)) = (self.rest.clone(), last) {
                    self.tasks.spawn(async move { Done::Acked(ch, last, rest.ack(ch, last).await) });
                }
            }
            Ack::At(t) => drop(self.tasks.spawn(async move {
                tokio::time::sleep_until(t).await;
                Done::AckDue(ch)
            })),
        }
    }

    /// Pushes only the UI models a batch of store changes affects.
    fn changed(&mut self, changes: Vec<Change>) {
        let (guilds, chans, msgs) = self.nav.affected(&self.store, &changes);
        if guilds {
            bridge::push_guilds(&self.ui, &self.store);
        }
        if chans {
            bridge::push_channels(&self.ui, &self.store, self.nav.guild);
        }
        if msgs {
            bridge::push_messages(&self.ui, &self.store, &mut self.nav);
        }
        if let Some(c) = self.nav.selected {
            self.maybe_ack(c);
        }
    }

    /// Sends a stored pending message; without a session it fails at once, keeping the text with a retry.
    fn post(&mut self, p: Pending) {
        let Some(rest) = self.rest.clone() else {
            let c = self.store.fail_pending(&p.nonce);
            return self.changed(c.into_iter().collect());
        };
        self.tasks.spawn(async move {
            let r = rest.send_message(p.channel, &p.content, &p.nonce, p.reply_to).await.map(Box::new);
            Done::Sent(p.nonce, r)
        });
    }

    fn event(&mut self, ev: Event) {
        let ready = matches!(ev, Event::Ready(_));
        let changes = self.store.apply(&ev);
        if ready {
            tracing::info!(guilds = self.store.guilds().len(), "READY");
            bridge::logged_in(&self.ui, self.store.me().map(|u| u.id));
            // READY drops stored messages; refetch the open channel. Older pages still in flight are stale.
            self.nav.reset_history();
            if let Some(id) = self.nav.selected
                && self.store.open_channel(id)
            {
                self.fetch(id, None);
            }
            bridge::push_selection(&self.ui, &self.store, self.nav.guild, self.nav.selected);
            bridge::push_messages(&self.ui, &self.store, &mut self.nav);
        }
        self.changed(changes);
    }

    async fn status(&mut self, s: ConnStatus) {
        tracing::info!(?s, "gateway status");
        if s == ConnStatus::Connected { self.thumbs.forget_failures() } // a network blip shouldn't leave placeholders
        let banner = match s {
            ConnStatus::AuthFailed => return self.logout("Login failed or session expired. Please log in again.").await,
            ConnStatus::Connected => String::new(),
            ConnStatus::Connecting => "Connecting…".into(),
            ConnStatus::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})…"),
        };
        self.with_ui(move |ui| ui.set_banner(banner.into()));
    }

    async fn done(&mut self, d: Done) {
        let err = match d {
            Done::Nothing => return,
            Done::History { channel, generation, before, result: Ok(msgs) } => {
                let changes = self.nav.history_page(&mut self.store, channel, generation, before, msgs);
                return self.changed(changes.unwrap_or_default());
            }
            Done::History { channel, generation, result: Err(e), .. } => {
                if let Some(msg) = self.nav.history_failed(&self.store, channel, generation, &e) {
                    bridge::set_error(&self.ui, msg.into());
                }
                e
            }
            Done::Acked(channel_id, message_id, Ok(())) => {
                // Clear the unread state now instead of waiting for the gateway's MESSAGE_ACK.
                let changes = self.store.apply(&Event::MessageAck { channel_id, message_id });
                return self.changed(changes);
            }
            Done::AckDue(ch) => {
                self.nav.ack_later.remove(&ch);
                return self.maybe_ack(ch);
            }
            Done::Sent(nonce, Ok(m)) => {
                // Don't wait for the gateway echo; apply dedupes the later MESSAGE_CREATE.
                self.store.take_pending(&nonce);
                let changes = self.store.apply(&Event::MessageCreate(m));
                return self.changed(changes);
            }
            Done::Sent(nonce, Err(e)) => {
                let c = self.store.fail_pending(&nonce);
                self.changed(c.into_iter().collect());
                e
            }
            Done::Edited(id, Ok(m)) => {
                bridge::edit_done(&self.ui, id, true);
                let changes = self.store.apply(&compose::edit_event(*m));
                return self.changed(changes);
            }
            Done::Deleted { channel_id, id, result: Ok(()) } => {
                let changes = self.store.apply(&Event::MessageDelete { id, channel_id });
                return self.changed(changes);
            }
            Done::QrToken(Ok(t)) => return self.login(t).await,
            Done::QrToken(Err(e)) => {
                tracing::warn!("QR login failed: {e}");
                return;
            }
            Done::Edited(id, Err(e)) => {
                bridge::edit_done(&self.ui, id, false);
                e
            }
            Done::Deleted { id, result: Err(e), .. } => {
                bridge::delete_failed(&self.ui, id);
                e
            }
            Done::Acked(_, _, Err(e)) => e,
        };
        match err {
            ApiError::Unauthorized => self.logout("Session expired. Please log in again.").await,
            e => tracing::warn!("request failed: {e}"),
        }
    }
}
