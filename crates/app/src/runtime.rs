//! Tokio runtime thread: owns the store, the gateway and REST, and pushes finished data to the UI.
use crate::nav::{Ack, DMS_NODE, HISTORY_PAGE, Nav};
use crate::voice::{AudioCfg, AudioNote, Bar, Conn, ConnIn, Controller, Effect, Incoming, ToAudio};
use crate::{AppWindow, bridge, compose, images, notify, settings, token};
use rxcord_api::event::Event;
use rxcord_api::gateway::GatewayCmd;
use rxcord_api::gateway::driver::{ConnStatus, GatewayEvent, run_gateway};
use rxcord_api::id::Id;
use rxcord_api::model::Message;
use rxcord_api::remote_auth::{QrStatus, qr_login};
use rxcord_api::rest::{InviteContext, JoinedGuild};
use rxcord_api::{ApiError, Rest};
use rxcord_store::{Change, Pending, Store};
use rxcord_voice::driver::{VoiceEvent, VoiceStatus};
use rxcord_voice::audio::tone::Ringer;
use rxcord_voice::session::LeaveReason;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Duration, Instant};

pub enum Command {
    LoginToken(String),
    StartQr,
    Logout,
    /// Expand or collapse a tree node (guild id or `DMS_NODE`).
    ToggleNode(Id),
    /// Open the channel's tab, or activate it.
    OpenChannel(Id),
    CloseTab(Id),
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
    /// The status bar's "Update" was clicked.
    InstallUpdate,
    /// Join button of a server invite in message `message` of the active channel.
    JoinInvite { message: Id, code: String },
    /// Join this guild voice channel (moving out of any other).
    JoinVoice(Id),
    /// A DM tab's 📞: join the DM's call and ring the others.
    StartCall(Id),
    /// The incoming-call prompt's buttons.
    AcceptCall,
    DeclineCall,
    /// The voice bar's hang-up.
    LeaveVoice,
    /// The voice bar's 🎤 and 🎧.
    ToggleMute,
    ToggleDeafen,
    /// Our volume for a user (0.0..=2.0) and "mute for me".
    UserVolume(Id, f32),
    UserMuted(Id, bool),
    /// ⚙ → "Voice settings…": list the devices and show the popup.
    VoiceSettings,
    /// Device name; None = system default.
    SetInput(Option<String>),
    SetOutput(Option<String>),
    SetThreshold(f32),
    SetNoiseSuppression(bool),
    /// The window closed: leave voice, let the gateway send that, then stop (main.rs waits for it).
    Quit,
}

/// Results of tasks spawned on the runtime, handled back on the loop.
enum Done {
    History { channel: Id, generation: u64, before: Option<Id>, result: Result<Vec<Message>, ApiError> },
    Acked(Id, Id, Result<(), ApiError>),
    AckDue(Id),
    Sent(String, Result<Box<Message>, ApiError>),
    Edited(Id, Result<Box<Message>, ApiError>),
    Deleted { channel_id: Id, id: Id, result: Result<(), ApiError> },
    QrToken(Result<String, ApiError>),
    /// `captcha`: this attempt already carried a solved captcha (don't ask again).
    Joined { message: Id, code: String, ctx: InviteContext, captcha: bool, result: Result<Option<JoinedGuild>, ApiError> },
    /// The local captcha page failed or nobody solved it.
    CaptchaFailed { message: Id, code: String, error: String },
}

enum Input {
    Cmd(Command),
    Gw(Option<GatewayEvent>),
    Qr(Option<QrStatus>),
    Done(Done),
    Thumb(images::Loaded),
    Voice(ConnIn),
    VoiceTimeout,
    RingTimeout,
    SaveSettings,
    /// The gateway's queue has room for the unsent op 4 (None: the gateway is gone).
    Op4Room(Option<mpsc::OwnedPermit<GatewayCmd>>),
}

/// `focused` is true while the window has focus; written by the UI thread (main.rs), read here and by notifications.
/// `last_guild` (0 = none): expanded on login, updated as guilds are used; main.rs saves it on exit.
pub fn spawn_runtime(
    ui: slint::Weak<AppWindow>,
    focused: Arc<AtomicBool>,
    last_guild: Arc<AtomicU64>,
) -> (UnboundedSender<Command>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let thread = std::thread::Builder::new()
        .name("rxcord-runtime".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            let (store, tasks, thumbs) = (Store::new(), JoinSet::new(), images::Thumbs::new());
            let nav = Nav { settings: settings::load(), ..Default::default() };
            let (update_tx, update_rx) = mpsc::unbounded_channel();
            if crate::update::enabled() {
                rt.spawn(crate::update::run(ui.clone(), update_rx));
            }
            let session_id = String::new();
            let (voice, conn, bar, save_at) = (Controller::default(), None, Bar::default(), None);
            let (calls, tone) = (Incoming::default(), None);
            rt.block_on(Runtime { ui, store, rest: None, gw: None, gw_cmds: None, gw_task: None, qr: None, tasks, nav, focused, thumbs, last_guild, session_id, update_tx, voice, conn, bar, save_at, calls, tone, op4_unsent: None }.run(rx));
            rt.shutdown_timeout(Duration::from_millis(200)); // don't wait out blocking tasks on exit
        })
        .expect("spawn runtime thread");
    (tx, thread)
}

struct Runtime {
    ui: slint::Weak<AppWindow>,
    store: Store,
    rest: Option<Arc<Rest>>,
    gw: Option<mpsc::Receiver<GatewayEvent>>,
    /// Main-gateway commands (voice state).
    gw_cmds: Option<mpsc::Sender<GatewayCmd>>,
    /// `run_gateway`, outside `tasks`: ending a session lets it flush `gw_cmds` first.
    gw_task: Option<JoinHandle<()>>,
    qr: Option<mpsc::Receiver<QrStatus>>,
    /// Session tasks; replacing the set aborts them all (logout, new login).
    tasks: JoinSet<Done>,
    nav: Nav,
    focused: Arc<AtomicBool>,
    thumbs: images::Thumbs,
    last_guild: Arc<AtomicU64>,
    /// Gateway session (from READY); joining an invite sends it.
    session_id: String,
    /// Asks the updater (update.rs) to install the release it found.
    update_tx: UnboundedSender<()>,
    /// The voice channel we're in or joining.
    voice: Controller,
    /// While connected to voice: the driver and the audio thread.
    conn: Option<Conn>,
    /// What the voice bar shows now.
    bar: Bar,
    /// Voice settings changed: written to settings.json at this time (at most once per second).
    save_at: Option<Instant>,
    /// The incoming call we're prompting for.
    calls: Incoming,
    /// The ring's own output, while ringing outside voice (in voice it's mixed into the call).
    tone: Option<Ringer>,
    /// The latest voice state (op 4) the gateway's full queue didn't take; sent once it has room.
    op4_unsent: Option<GatewayCmd>,
}

/// Op 4 to the gateway without waiting (it may be blocked handing us events; a wait would deadlock). A
/// full queue keeps the latest one for later; an older unsent one is replaced, never sent after it.
fn queue_op4(tx: Option<&mpsc::Sender<GatewayCmd>>, unsent: &mut Option<GatewayCmd>, c: GatewayCmd) {
    let Some(tx) = tx else { return };
    if unsent.is_some() {
        *unsent = Some(c);
    } else if let Err(e) = tx.try_send(c) {
        tracing::debug!("voice state queued: gateway busy");
        *unsent = Some(e.into_inner());
    }
}

/// Room in the gateway's queue for the unsent op 4; never resolves when there's nothing to send.
async fn op4_room(tx: Option<mpsc::Sender<GatewayCmd>>, pending: bool) -> Option<mpsc::OwnedPermit<GatewayCmd>> {
    match tx.filter(|_| pending) {
        Some(tx) => tx.reserve_owned().await.ok(),
        None => std::future::pending().await,
    }
}

async fn recv_opt<T>(r: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match r {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// How long an ending gateway may take to send its queued commands (op 4 null) before it's aborted.
const GATEWAY_FLUSH: Duration = Duration::from_millis(500);

/// Waits (bounded) for the gateway to send what's queued and stop after its commands closed. Its events
/// keep being taken meanwhile: it may be blocked handing one over before it reads its commands.
async fn finish_gateway(mut task: JoinHandle<()>, mut gw: Option<mpsc::Receiver<GatewayEvent>>) {
    let done = tokio::time::timeout(GATEWAY_FLUSH, async {
        loop {
            tokio::select! {
                _ = &mut task => return,
                e = recv_opt(&mut gw) => if e.is_none() { gw = None },
            }
        }
    })
    .await;
    if done.is_err() {
        tracing::warn!("gateway didn't stop in time");
        task.abort();
    }
}

async fn recv_conn(c: &mut Option<Conn>) -> ConnIn {
    match c {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
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
                if !self.nav.expanded.contains(&Id(crate::demo::GUILD)) {
                    self.command(Command::ToggleNode(Id(crate::demo::GUILD))).await;
                }
                self.command(Command::OpenChannel(Id(200))).await;
                self.command(Command::OpenChannel(Id(crate::demo::CHANNEL))).await;
                msgs.into_iter().for_each(|m| self.event(m));
                self.command(crate::demo::failed_send()).await;
                self.nav.speaking.insert(Id(2)); // Ferris talking in the demo voice channel
                bridge::push_tree(&self.ui, &self.store, &mut self.nav);
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
                    Some(Command::Quit) | None => return self.quit().await, // UI gone
                    Some(c) => Input::Cmd(c),
                },
                g = recv_opt(&mut self.gw) => Input::Gw(g),
                q = recv_opt(&mut self.qr) => Input::Qr(q),
                Some(Ok(d)) = self.tasks.join_next() => Input::Done(d),
                Some(Ok(l)) = self.thumbs.tasks.join_next() => Input::Thumb(l),
                v = recv_conn(&mut self.conn) => Input::Voice(v),
                _ = sleep_opt(self.voice.deadline()) => Input::VoiceTimeout,
                _ = sleep_opt(self.calls.deadline()) => Input::RingTimeout,
                _ = sleep_opt(self.save_at) => Input::SaveSettings,
                p = op4_room(self.gw_cmds.clone(), self.op4_unsent.is_some()) => Input::Op4Room(p),
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
                Input::Voice(v) => self.voice_in(v),
                Input::VoiceTimeout => {
                    let fx = self.voice.expire(Instant::now());
                    self.voice_effects(fx);
                }
                Input::RingTimeout => {
                    if let Some(ch) = self.calls.end() {
                        self.stop_ringing(ch); // nobody answered: stop ringing our other clients too
                    }
                    self.push_call();
                }
                Input::SaveSettings => {
                    if let Some(s) = self.unsaved_settings() {
                        tokio::task::spawn_blocking(move || settings::update(|f| f.take_voice(&s)));
                    }
                    // volume drags skip the tree; its rows (the sliders' model) catch up here
                    bridge::push_tree(&self.ui, &self.store, &mut self.nav);
                }
                Input::Op4Room(p) => {
                    if let (Some(p), Some(c)) = (p, self.op4_unsent.take()) {
                        p.send(c);
                    }
                }
            }
        }
    }

    /// Fire-and-forget: Discord stops ringing us (all our clients) for the call in `ch`.
    fn stop_ringing(&self, ch: Id) {
        if let (Some(rest), Some(me)) = (self.rest.clone(), self.store.me().map(|u| u.id)) {
            tokio::spawn(async move {
                if let Err(e) = rest.stop_ringing(ch, me).await {
                    tracing::warn!("stop ringing: {e}");
                }
            });
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
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        self.gw_cmds = Some(cmd_tx);
        self.with_ui(|ui| ui.set_connecting(true)); // until READY shows the app
        self.gw_task = Some(tokio::spawn(run_gateway(token, tx, cmd_rx)));
    }

    /// Closing its commands stops `run_gateway` once it has sent them (bounded, see `finish_gateway`);
    /// returns the wait. Nothing to wait for without a gateway.
    /// Best effort: an op 4 still unsent (queue full) gets one more try, and a gateway that's mid-resume
    /// or backing off sends what's queued only if it's back within `GATEWAY_FLUSH`; otherwise Discord
    /// drops us from voice when our session times out.
    fn close_gateway(&mut self) -> Option<JoinHandle<()>> {
        if let (Some(tx), Some(c)) = (self.gw_cmds.take(), self.op4_unsent.take()) {
            let _ = tx.try_send(c);
        }
        let task = self.gw_task.take()?;
        Some(tokio::spawn(finish_gateway(task, self.gw.take())))
    }

    async fn quit(&mut self) {
        let fx = self.voice.hang_up();
        self.voice_effects(fx);
        if let Some(s) = self.unsaved_settings() {
            settings::update(|f| f.take_voice(&s));
        }
        if let Some(wait) = self.close_gateway() {
            let _ = wait.await;
        }
    }

    /// Hangs up voice (op 4 null is queued first, then the gateway flushes it and stops); dropping the
    /// task set aborts everything else.
    fn end_session(&mut self) {
        let fx = self.voice.hang_up();
        self.voice_effects(fx);
        self.close_gateway();
        self.tasks = JoinSet::new();
        self.gw = None;
        self.qr = None;
        self.rest = None;
        // Unsent text goes back into the drafts, never auto-sent: the next login may be another account.
        bridge::restore_drafts(&self.ui, compose::unsent_drafts(self.store.all_pending()));
        self.store = Store::new();
        self.nav = Nav { settings: std::mem::take(&mut self.nav.settings), ..Default::default() };
        self.thumbs = images::Thumbs::new(); // drops the cache and aborts its fetches
        bridge::clear(&self.ui);
        self.calls = Incoming::default();
        self.push_call();
    }

    /// `error` stays on the login screen (QR updates don't replace it) until the next login attempt.
    fn show_login(&mut self, error: &str) {
        let error = error.to_string();
        self.with_ui(move |ui| {
            ui.set_logged_in(false);
            ui.set_connecting(false);
            ui.set_banner("".into());
            ui.set_login_status("".into());
            ui.set_login_error(error.into());
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
            // A connecting session must not be ended by a stale login-screen action.
            Command::LoginToken(_) | Command::StartQr if self.gw.is_some() => {}
            Command::LoginToken(t) => {
                let t = t.trim().to_string();
                if t.is_empty() {
                    return;
                }
                self.with_ui(|ui| {
                    ui.set_login_error("".into());
                    ui.set_login_status("Logging in…".into());
                });
                self.login(t).await;
            }
            Command::StartQr => self.show_login(""),
            Command::Logout => self.logout("").await,
            Command::ToggleNode(n) => {
                if !self.nav.expanded.remove(&n) {
                    self.nav.expanded.insert(n);
                    self.used_guild(n);
                }
                bridge::push_tree(&self.ui, &self.store, &mut self.nav);
            }
            Command::CloseTab(id) => match self.nav.close_tab(&mut self.store, id) {
                Some(next) => self.show_tab(next, false),
                None => bridge::push_tree(&self.ui, &self.store, &mut self.nav),
            },
            Command::OpenChannel(id) => {
                bridge::set_error(&self.ui, String::new());
                if self.nav.selected == Some(id) {
                    // re-click: retry a failed first load
                    if self.store.messages(id).is_empty() {
                        self.fetch(id, None);
                    }
                    return;
                }
                if let Some(g) = self.store.channel(id).and_then(|c| c.guild_id) {
                    self.used_guild(g);
                }
                self.show_tab(Some(id), false);
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
            Command::JoinInvite { message, code } => {
                let ch = self.nav.selected.and_then(|c| self.store.channel(c));
                let (Some(rest), Some(ch)) = (self.rest.clone(), ch) else { return };
                let ctx = InviteContext { guild: ch.guild_id, channel: ch.id, channel_type: ch.kind, message };
                let session = self.session_id.clone();
                bridge::row_note(&self.ui, message, "Joining…".into(), true, None);
                self.tasks.spawn(async move {
                    let result = rest.join_invite(&code, &session, ctx, None).await;
                    Done::Joined { message, code, ctx, captcha: false, result }
                });
            }
            Command::JoinVoice(ch) => {
                let guild = self.store.channel(ch).filter(|c| c.kind == 2).and_then(|c| c.guild_id);
                if let (Some(g), Some(_)) = (guild, &self.gw_cmds) {
                    let fx = self.voice.join(Some(g), ch, Instant::now());
                    self.voice_effects(fx);
                }
            }
            Command::StartCall(ch) => {
                let dm = self.store.channel(ch).is_some_and(|c| matches!(c.kind, 1 | 3));
                if dm && self.gw_cmds.is_some() {
                    let answering = self.calls.channel() == Some(ch);
                    let fx = crate::voice::start_call(&mut self.voice, &mut self.calls, ch, Instant::now());
                    if answering {
                        self.push_call(); // the ring stops before the call's audio starts
                    }
                    self.voice_effects(fx);
                }
            }
            Command::AcceptCall => {
                let Some(ch) = self.calls.end() else { return };
                self.push_call(); // the ring stops before the call's audio starts
                if self.gw_cmds.is_some() {
                    let fx = self.voice.join(None, ch, Instant::now()); // moves out of any other voice channel
                    self.voice_effects(fx);
                }
            }
            Command::DeclineCall => {
                let Some(ch) = self.calls.end() else { return };
                self.push_call();
                self.stop_ringing(ch);
            }
            Command::LeaveVoice => {
                let fx = self.voice.hang_up();
                self.voice_effects(fx);
            }
            Command::ToggleMute => {
                let fx = self.voice.toggle_mute();
                self.self_state(fx);
            }
            Command::ToggleDeafen => {
                let fx = self.voice.toggle_deafen();
                self.self_state(fx);
            }
            Command::UserVolume(u, v) => {
                self.nav.settings.set_volume(u.to_string(), v);
                self.user_gain(u); // a slider drag: the tree follows with the settings save
            }
            Command::UserMuted(u, on) => {
                let (set, k) = (&mut self.nav.settings.user_muted, u.to_string());
                if on { set.insert(k) } else { set.remove(&k) };
                self.user_gain(u);
                bridge::push_tree(&self.ui, &self.store, &mut self.nav);
            }
            Command::VoiceSettings => {
                let (ui, s) = (self.ui.clone(), self.nav.settings.clone());
                // device enumeration can be slow (ALSA); only done when asked
                tokio::task::spawn_blocking(move || {
                    let (inputs, outputs) = (rxcord_voice::audio::devices::inputs(), rxcord_voice::audio::devices::outputs());
                    bridge::voice_settings(&ui, &s, inputs, outputs);
                });
            }
            Command::SetInput(d) => {
                self.nav.settings.input_device = d;
                self.reopen_input();
            }
            Command::SetNoiseSuppression(on) => {
                self.nav.settings.noise_suppression = on;
                self.reopen_input();
            }
            Command::SetOutput(d) => {
                self.nav.settings.output_device = d.clone();
                self.audio(ToAudio::Output(d));
            }
            Command::SetThreshold(t) => {
                self.nav.settings.set_threshold(t);
                self.audio(ToAudio::Threshold(self.nav.settings.vad_threshold));
            }
            Command::Quit => {} // handled in `run`
            Command::InstallUpdate => drop(self.update_tx.send(())),
            Command::ThumbVisible(url) => bridge::push_thumb(&self.ui, self.thumbs.request(url, &self.nav.shown)),
        }
    }

    /// Activates `next` (opening its tab) or nothing; `carry`: the previous tab's channel was deleted.
    fn show_tab(&mut self, next: Option<Id>, carry: bool) {
        if let Some(id) = next
            && self.nav.open(&mut self.store, id)
        {
            self.fetch(id, None);
        }
        bridge::push_selection(&self.ui, self.nav.selected, carry);
        bridge::push_messages(&self.ui, &self.store, &mut self.nav);
        bridge::push_tree(&self.ui, &self.store, &mut self.nav);
        if let Some(id) = next {
            self.maybe_ack(id);
        }
    }

    /// Last resort for a captcha: the invite in a Firefox private window, where the user logs in as this
    /// account (a normal window would join with whatever account is logged in there).
    async fn captcha_fallback(&mut self, message: Id, code: &str, why: &str) {
        let me = self.store.me().map(|u| u.username.clone()).unwrap_or_default();
        let url = crate::invite::invite_url(code);
        let u = url.clone();
        let private = tokio::task::spawn_blocking(move || crate::invite::open_private(&u)).await.unwrap_or(false);
        let (msg, open) = if private {
            (format!("{why} In the private window that opened, log in as {me}, then join."), None)
        } else {
            (format!("{why} Open the invite in a private window, log in as {me}, then join."), Some(url))
        };
        bridge::row_note(&self.ui, message, msg, false, open);
    }

    fn used_guild(&self, g: Id) {
        if g != DMS_NODE {
            self.last_guild.store(g.0, Ordering::Relaxed);
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
        let (tree, msgs) = self.nav.affected(&changes);
        if changes.contains(&Change::Me) {
            bridge::push_me(&self.ui, &self.store);
        }
        if tree {
            bridge::push_tree(&self.ui, &self.store, &mut self.nav);
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
        if let Event::Ready(r) = &ev {
            self.session_id = r.session_id.clone();
        }
        let changes = self.store.apply(&ev);
        if ready || changes.iter().any(|c| matches!(c, Change::Call(_))) {
            self.calls_changed();
        }
        if matches!(ev, Event::VoiceStateUpdate(_) | Event::VoiceServerUpdate(_))
            && let Some(me) = self.store.me().map(|u| u.id)
        {
            let fx = self.voice.event(&ev, me);
            self.voice_effects(fx);
        }
        if let Some(next) = self.nav.drop_missing_tabs(&mut self.store) {
            bridge::set_error(&self.ui, String::new());
            self.show_tab(next, true);
        }
        // Unread(channel) is only reported for a create the store saw as new, so redeliveries stay silent.
        if let Event::MessageCreate(m) = &ev
            && changes.contains(&Change::Unread(m.channel_id))
            && let Some(me) = self.store.me().map(|u| u.id)
            && let Some(c) = self.store.channel(m.channel_id)
            && notify::should_notify(m, me, matches!(c.kind, 1 | 3), self.nav.selected, self.focused.load(Ordering::Relaxed))
        {
            let (title, body) = notify::notification_text(m, &self.store);
            notify::show(&title, &body);
        }
        if ready {
            tracing::info!(guilds = self.store.guilds().len(), "READY");
            bridge::logged_in(&self.ui, self.store.me().map(|u| u.id));
            if self.nav.expanded.is_empty() {
                self.nav.expanded.insert(DMS_NODE);
                self.nav.expanded.insert(Id(self.last_guild.load(Ordering::Relaxed)));
            }
            // READY drops stored messages; refetch the open channel. Older pages still in flight are stale.
            self.nav.reset_history();
            if let Some(id) = self.nav.selected
                && self.store.open_channel(id)
            {
                self.fetch(id, None);
            }
            bridge::push_selection(&self.ui, self.nav.selected, false);
            bridge::push_messages(&self.ui, &self.store, &mut self.nav);
        }
        self.changed(changes);
    }

    /// Mute/deafen: applied here at once, then op 4 and the bar.
    fn self_state(&mut self, fx: Vec<Effect>) {
        if let Some(k) = &self.conn {
            k.set_muted(self.voice.muted());
            k.audio(ToAudio::Deafen(self.voice.deafened()));
        }
        self.voice_effects(fx);
    }

    /// Per-person volume or "mute for me" changed: playback now, settings.json (and the tree) soon.
    fn user_gain(&mut self, u: Id) {
        if let Some(k) = &self.conn {
            k.audio(ToAudio::Gain(u.0, self.nav.settings.gain(&u.to_string())));
        }
        self.save_soon();
    }

    fn reopen_input(&mut self) {
        let s = &self.nav.settings;
        self.audio(ToAudio::Input(s.input_device.clone(), s.noise_suppression));
    }

    /// A voice setting changed: to the audio thread when in voice, and saved soon.
    fn audio(&mut self, m: ToAudio) {
        if let Some(k) = &self.conn {
            k.audio(m);
        }
        self.save_soon();
    }

    /// Coalesces slider drags: one write per second at most.
    fn save_soon(&mut self) {
        self.save_at.get_or_insert(Instant::now() + Duration::from_secs(1));
    }

    fn unsaved_settings(&mut self) -> Option<settings::Settings> {
        self.save_at.take().map(|_| self.nav.settings.clone())
    }

    /// Calls changed: a new incoming call gets its prompt and one notification; an ended one loses it.
    fn calls_changed(&mut self) {
        if let Some(ch) = self.calls.sync(&self.store.ringing_me(), Instant::now()) {
            notify::show(&notify::call_title(&self.store, ch), "Incoming call");
        }
        self.push_call();
    }

    /// The prompt and the ring follow `calls`.
    fn push_call(&mut self) {
        let text = self.calls.channel().map(|c| notify::call_title(&self.store, c)).unwrap_or_default();
        bridge::call_prompt(&self.ui, text);
        self.sync_ring();
    }

    /// Rings while prompting: mixed into the call when in voice, else on its own output, opened only while
    /// ringing.
    fn sync_ring(&mut self) {
        let on = self.calls.channel().is_some();
        if let Some(k) = &self.conn {
            self.tone = None;
            return k.audio(ToAudio::Ring(on));
        }
        if !on {
            self.tone = None;
        } else if self.tone.is_none() {
            // opened and closed on its own thread; a failed open is logged and not retried while this call rings
            self.tone = Some(Ringer::start(self.nav.settings.output_device.clone()));
        }
    }

    /// Carries out what the voice controller decided. Old devices close before new ones open.
    fn voice_effects(&mut self, fx: Vec<Effect>) {
        let had_conn = self.conn.is_some();
        for e in fx {
            match e {
                Effect::Gateway(c) => queue_op4(self.gw_cmds.as_ref(), &mut self.op4_unsent, c),
                Effect::Connect { connect, channel } => {
                    // a new voice server mid-call: only the driver starts over, the devices stay open
                    let connect = match &mut self.conn {
                        Some(k) => match k.restart(connect, channel) {
                            Ok(()) => continue,
                            Err(c) => c, // its queue is full: start a new connection instead
                        },
                        None => connect,
                    };
                    self.conn = None;
                    self.tone = None; // the ring moves into the call's output
                    let mut cfg = AudioCfg::new(&self.nav.settings, self.voice.deafened());
                    cfg.ringing = self.calls.channel().is_some();
                    let k = Conn::start(connect, channel, cfg);
                    k.set_muted(self.voice.muted());
                    self.conn = Some(k);
                }
                Effect::Disconnect => self.conn = None,
                Effect::Notice(n) => bridge::voice_notice(&self.ui, n.into()),
                Effect::Ring(ch) => {
                    if let Some(rest) = self.rest.clone() {
                        tokio::spawn(async move {
                            if let Err(e) = rest.ring(ch).await {
                                tracing::warn!("ring: {e}");
                            }
                        });
                    }
                }
            }
        }
        if had_conn && self.conn.is_none() {
            self.sync_ring(); // still ringing: back on its own output
        }
        if !self.voice.connected() && !self.nav.speaking.is_empty() {
            self.nav.speaking.clear();
            bridge::push_tree(&self.ui, &self.store, &mut self.nav);
        }
        self.push_voice();
    }

    fn voice_in(&mut self, v: ConnIn) {
        let Some(conn) = &mut self.conn else { return };
        match v {
            ConnIn::Event(VoiceEvent::Media { user_id, seq, opus, .. }) => return conn.audio(ToAudio::Media(user_id, seq, opus)),
            ConnIn::Event(VoiceEvent::Speaking { user_id, on, .. }) => {
                if on { conn.op5.insert(user_id) } else { conn.op5.remove(&user_id) };
            }
            ConnIn::Audio(AudioNote::Levels(l)) => conn.heard = l,
            ConnIn::Audio(AudioNote::Speaker(ok)) => {
                conn.speaker = ok;
                return self.push_voice();
            }
            ConnIn::Audio(AudioNote::Mic(ok)) => {
                conn.mic = ok;
                conn.level &= ok;
                return self.push_voice();
            }
            ConnIn::Audio(AudioNote::Level(on)) => {
                conn.level = on;
                return self.push_voice();
            }
            ConnIn::Event(VoiceEvent::Status(s)) => {
                conn.status = s;
                return self.push_voice();
            }
            ConnIn::Event(VoiceEvent::Left(r)) => {
                let fx = self.voice.left(&r, Instant::now());
                return self.voice_effects(fx);
            }
            ConnIn::Closed => {
                let fx = self.voice.left(&LeaveReason::Fatal(0), Instant::now());
                return self.voice_effects(fx);
            }
            ConnIn::Event(VoiceEvent::PrivacyCode(_)) => return,
        }
        let speaking = conn.speaking();
        if speaking != self.nav.speaking {
            self.nav.speaking = speaking;
            bridge::push_tree(&self.ui, &self.store, &mut self.nav);
        }
    }

    /// The voice bar; only pushed when it changed (voice states of every server come through here).
    fn push_voice(&mut self) {
        let text = self.voice.channel().map(|(g, c)| {
            let place = match g {
                Some(g) => {
                    let g = self.store.guilds().iter().find(|x| x.id == g).map(|x| x.name.as_str()).unwrap_or_default();
                    let c = self.store.channel(c).and_then(|c| c.name.as_deref()).unwrap_or_default();
                    format!("{g} / {c}")
                }
                None => self.store.channel(c).map(|c| crate::view::channel_row(&self.store, c).name).unwrap_or_default(),
            };
            match &self.conn {
                Some(k) if matches!(k.status, VoiceStatus::Reconnecting { .. }) => "Reconnecting voice…".into(),
                k => {
                    let problems = k.as_ref().map_or(vec!["Connecting…"], |k| k.problems());
                    std::iter::once(format!("🔊 {place}")).chain(problems.into_iter().map(String::from)).collect::<Vec<_>>().join(" · ")
                }
            }
        });
        let muted = self.voice.muted();
        let level = !muted && self.conn.as_ref().is_some_and(|k| k.level);
        let channel = self.voice.channel().map_or(String::new(), |(_, c)| c.to_string());
        let bar = Bar { text: text.unwrap_or_default(), channel, muted, deafened: self.voice.deafened(), level };
        if let Some(bar) = crate::voice::changed(&mut self.bar, bar) {
            bridge::voice_bar(&self.ui, bar);
        }
    }

    async fn status(&mut self, s: ConnStatus) {
        tracing::info!(?s, "gateway status");
        if s == ConnStatus::Connected { self.thumbs.forget_failures() } // a network blip shouldn't leave placeholders
        let banner = match s {
            ConnStatus::AuthFailed => return self.logout("Login failed or session expired. Please log in again.").await,
            ConnStatus::Connected => String::new(),
            ConnStatus::Connecting => "Connecting…".into(),
            ConnStatus::Reconnecting { attempt } => format!("Reconnecting (attempt {attempt})…"),
            ConnStatus::Fatal(code) => format!("Discord closed the connection (code {code})"),
            ConnStatus::ReadyFailed => "Couldn't read account data from Discord".into(),
        };
        self.with_ui(move |ui| ui.set_banner(banner.into()));
    }

    async fn done(&mut self, d: Done) {
        let err = match d {
            Done::History { channel, generation, before, result: Ok(msgs) } => {
                let changes = self.nav.history_page(&mut self.store, channel, generation, before, msgs);
                return self.changed(changes.unwrap_or_default());
            }
            Done::History { channel, generation, result: Err(e), .. } => {
                if let Some(msg) = self.nav.history_failed(&self.store, channel, generation, &e) {
                    bridge::set_error(&self.ui, msg.into());
                }
                if self.nav.forbidden.contains(&channel) {
                    bridge::push_tree(&self.ui, &self.store, &mut self.nav);
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
            Done::Joined { message, result: Ok(g), .. } => {
                let name = g.as_ref().map(|g| g.name.clone()).filter(|n| !n.is_empty());
                if let Some(g) = g {
                    // Its channels arrive with GUILD_CREATE; open its node so they show up right away.
                    self.nav.expanded.insert(g.id);
                    self.used_guild(g.id);
                    bridge::push_tree(&self.ui, &self.store, &mut self.nav);
                }
                let msg = name.map_or("Joined the server".into(), |n| format!("Joined {n}"));
                return bridge::row_note(&self.ui, message, msg, true, None);
            }
            Done::Joined { message, code, ctx, captcha: false, result: Err(ApiError::Http { status: 400, body }) }
                if let Some(ch) = crate::captcha::parse_challenge(&body) =>
            {
                // Shown on a local page in the browser; the solved token comes back here and the join is retried.
                let (Some(rest), ui, session) = (self.rest.clone(), self.ui.clone(), self.session_id.clone()) else { return };
                bridge::row_note(&self.ui, message, "Discord wants a captcha: solve it in the browser tab that opened".into(), true, None);
                self.tasks.spawn(async move {
                    let ready = |url: String| bridge::open_url(&ui, url);
                    match crate::captcha::solve(ch, ready).await {
                        Ok(answer) => {
                            bridge::row_note(&ui, message, "Joining…".into(), true, None);
                            let result = rest.join_invite(&code, &session, ctx, Some(&answer)).await;
                            Done::Joined { message, code, ctx, captcha: true, result }
                        }
                        Err(error) => Done::CaptchaFailed { message, code, error },
                    }
                });
                return;
            }
            Done::CaptchaFailed { message, code, error } => {
                tracing::warn!("captcha: {error}");
                return self.captcha_fallback(message, &code, &format!("The captcha didn't work: {error}.")).await;
            }
            Done::Joined { message, code, result: Err(e), .. } if e.is_captcha() => {
                return self.captcha_fallback(message, &code, "Discord still wants a captcha.").await;
            }
            Done::Joined { message, result: Err(e), .. } => {
                let msg = match &e {
                    ApiError::Http { status: 404, .. } => "This invite is invalid or expired".to_string(),
                    ApiError::Http { status: 403, .. } => "You can't join this server (banned, or the server is full)".to_string(),
                    _ => "Couldn't join the server".to_string(),
                };
                bridge::row_note(&self.ui, message, msg, false, None);
                e
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

#[cfg(test)]
mod tests {
    use super::*;

    fn op4(ch: u64) -> GatewayCmd {
        GatewayCmd::VoiceState { guild_id: Some(Id(1)), channel_id: Some(Id(ch)), self_mute: false, self_deaf: false }
    }
    fn ch(c: GatewayCmd) -> Option<Id> {
        let GatewayCmd::VoiceState { channel_id, .. } = c;
        channel_id
    }

    #[tokio::test]
    async fn op4_kept_when_gateway_busy_then_sent() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut unsent = None;
        queue_op4(Some(&tx), &mut unsent, op4(1));
        assert!(unsent.is_none(), "room: sent at once");
        queue_op4(Some(&tx), &mut unsent, op4(2)); // full
        queue_op4(Some(&tx), &mut unsent, op4(3)); // still unsent: replaces 2
        assert_eq!(unsent.map(ch), Some(Some(Id(3))), "the latest is kept");
        assert_eq!(ch(rx.recv().await.unwrap()), Some(Id(1)));
        let p = op4_room(Some(tx.clone()), true).await.expect("room again");
        p.send(unsent.take().unwrap());
        assert_eq!(ch(rx.recv().await.unwrap()), Some(Id(3)), "retried; 2 never goes out after 3");
        let idle = tokio::time::timeout(Duration::from_millis(50), op4_room(Some(tx), false)).await;
        assert!(idle.is_err(), "nothing unsent: the arm never fires");
    }

    #[tokio::test]
    async fn finish_gateway_drains_events_until_the_driver_stops() {
        let (tx, rx) = mpsc::channel(1);
        // a driver with more events to hand over before it gets to its (closed) commands
        let task = tokio::spawn(async move {
            for _ in 0..5 {
                let _ = tx.send(GatewayEvent::Status(ConnStatus::Connected)).await;
            }
        });
        let t0 = std::time::Instant::now();
        finish_gateway(task, Some(rx)).await;
        assert!(t0.elapsed() < GATEWAY_FLUSH, "finished, not timed out");
    }

    #[tokio::test]
    async fn finish_gateway_aborts_a_stuck_driver() {
        let (dropped_tx, dropped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _keep = dropped_tx;
            std::future::pending::<()>().await
        });
        finish_gateway(task, None).await;
        assert!(dropped.await.is_err(), "aborted: its state was dropped");
    }
}
