//! Runtime-side voice. `Controller` is the join handshake (op 4, then our VOICE_STATE_UPDATE and the
//! VOICE_SERVER_UPDATE) as pure state; `Conn` is what runs while connected: the driver task and the
//! audio thread. Nothing here opens a device or spawns a thread unless a `Conn` exists.
use rxcord_api::event::Event;
use rxcord_api::gateway::GatewayCmd;
use rxcord_api::id::Id;
use rxcord_voice::audio::capture::{Capture, CaptureOut};
use rxcord_voice::audio::playback::Playback;
use rxcord_voice::driver::{VoiceCmd, VoiceEvent, VoiceStatus, run_voice};
use rxcord_voice::session::{Connect, LeaveReason};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a lost or missing device is tried again.
const REOPEN_EVERY: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(10);
/// Until settings exist (Task 10).
const NOISE_SUPPRESSION: bool = true;

/// What the runtime must do after a controller call, in order.
#[derive(Debug)]
pub enum Effect {
    /// Main gateway op 4.
    Gateway(GatewayCmd),
    /// Start a voice connection (the runtime replaces any running one).
    Connect { connect: Connect, channel: Id },
    /// Stop the running voice connection and its devices.
    Disconnect,
    /// Shown in the status bar for a few seconds.
    Notice(&'static str),
}

struct Target {
    guild: Id,
    channel: Id,
    session: Option<String>,
    /// (token, endpoint)
    server: Option<(String, String)>,
    /// A `Connect` was issued (the driver may still be handshaking).
    connected: bool,
    /// Join gives up at this time unless connected.
    deadline: Instant,
}

#[derive(Default)]
pub struct Controller {
    target: Option<Target>,
}

fn op4(guild: Id, channel: Option<Id>) -> Effect {
    Effect::Gateway(GatewayCmd::VoiceState { guild_id: Some(guild), channel_id: channel, self_mute: false, self_deaf: false })
}

impl Controller {
    /// (guild, channel) we're in or joining.
    pub fn channel(&self) -> Option<(Id, Id)> {
        self.target.as_ref().map(|t| (t.guild, t.channel))
    }

    pub fn connected(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.connected)
    }

    /// When `expire` must be called, while a join is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.target.as_ref().filter(|t| !t.connected).map(|t| t.deadline)
    }

    /// Join `channel`, leaving (moving out of) any current one: one op 4.
    pub fn join(&mut self, guild: Id, channel: Id, now: Instant) -> Vec<Effect> {
        if self.target.as_ref().is_some_and(|t| t.channel == channel) {
            return vec![];
        }
        let mut fx = vec![];
        if self.connected() {
            fx.push(Effect::Disconnect);
        }
        self.target = Some(Target { guild, channel, session: None, server: None, connected: false, deadline: now + JOIN_TIMEOUT });
        fx.push(op4(guild, Some(channel)));
        fx
    }

    /// Hang up: op 4 with no channel, once.
    pub fn hang_up(&mut self) -> Vec<Effect> {
        let Some(t) = self.target.take() else { return vec![] };
        let mut fx = vec![];
        if t.connected {
            fx.push(Effect::Disconnect);
        }
        fx.push(op4(t.guild, None));
        fx
    }

    /// Main-gateway events; `me` is our user id.
    pub fn event(&mut self, ev: &Event, me: Id) -> Vec<Effect> {
        let Some(t) = &mut self.target else { return vec![] };
        match ev {
            Event::VoiceStateUpdate(v) if v.user_id == me && v.channel_id == Some(t.channel) => {
                t.session = Some(v.session_id.clone());
            }
            // Disconnected by a moderator or another client of ours. While still joining it may be the
            // echo of leaving the previous channel, so only a live connection counts.
            Event::VoiceStateUpdate(v) if v.user_id == me && v.channel_id.is_none() && t.connected => {
                self.target = None;
                return vec![Effect::Disconnect, Effect::Notice("Disconnected from voice")];
            }
            // endpoint null: the server is still being allocated, another update follows
            Event::VoiceServerUpdate(s) if s.guild_id == Some(t.guild) && s.endpoint.is_some() => {
                let endpoint = s.endpoint.clone().unwrap_or_default();
                // Same endpoint while connected: a main-gateway reconnect replaying op 4; the voice
                // connection keeps going. // VERIFY LIVE
                if t.connected && t.server.as_ref().is_some_and(|(_, e)| *e == endpoint) {
                    return vec![];
                }
                t.server = Some((s.token.clone(), endpoint));
            }
            _ => return vec![],
        }
        let (Some(session), Some((token, endpoint))) = (&t.session, &t.server) else { return vec![] };
        let connect = Connect { server_id: t.guild.0, user_id: me.0, session_id: session.clone(), token: token.clone(), endpoint: endpoint.clone() };
        let mut fx = vec![];
        if t.connected {
            fx.push(Effect::Disconnect); // voice server changed
        }
        t.connected = true;
        fx.push(Effect::Connect { connect, channel: t.channel });
        fx
    }

    /// The pending join timed out (no answer from Discord): give up.
    pub fn expire(&mut self, now: Instant) -> Vec<Effect> {
        if self.deadline().is_none_or(|d| now < d) {
            return vec![];
        }
        let mut fx = self.hang_up();
        fx.push(Effect::Notice("Couldn't join voice: Discord didn't answer"));
        fx
    }

    /// The driver gave up or was told to leave (kicked, moved, channel deleted).
    pub fn left(&mut self, reason: &LeaveReason) -> Vec<Effect> {
        let mut fx = self.hang_up();
        if fx.is_empty() {
            return fx;
        }
        fx.push(Effect::Notice(match reason {
            LeaveReason::Kicked => "Disconnected from voice",
            LeaveReason::BadEncryption => "Voice failed: unsupported encryption",
            LeaveReason::Fatal(_) => "Voice connection lost",
        }));
        fx
    }
}

/// From the audio thread to the runtime.
#[derive(Debug, PartialEq)]
pub enum AudioNote {
    /// Whether an output device is open.
    Speaker(bool),
    /// Whether a microphone is open (none: we're effectively muted).
    Mic(bool),
    /// The mic hears speech right now (only sent when it changes).
    Level(bool),
    /// Users heard recently and whether they're talking now (only sent when it changes).
    Levels(Vec<(u64, bool)>),
}

pub enum ConnIn {
    Event(VoiceEvent),
    Audio(AudioNote),
    /// The driver ended without saying why.
    Closed,
}

/// A running voice connection. Dropping it stops the driver (Leave, then its channels close) and the
/// audio thread (its media channel closes; the thread drops the device and exits).
pub struct Conn {
    cmds: mpsc::Sender<VoiceCmd>,
    events: mpsc::Receiver<VoiceEvent>,
    media: Option<std_mpsc::Sender<(u64, u16, Vec<u8>)>>,
    notes: mpsc::UnboundedReceiver<AudioNote>,
    /// Read by the audio thread every tick.
    muted: Arc<AtomicBool>,
    pub status: VoiceStatus,
    /// An output device is open.
    pub speaker: bool,
    /// A microphone is open.
    pub mic: bool,
    /// The mic hears speech (the voice bar's level dot, Task 10).
    #[allow(dead_code)]
    pub level: bool,
}

impl Conn {
    /// Spawns the driver on the current tokio runtime and the audio thread.
    pub fn start(connect: Connect, channel: Id) -> Conn {
        let (ev_tx, events) = mpsc::channel(256);
        let (cmds, cmd_rx) = mpsc::channel(64);
        tokio::spawn(run_voice(connect, channel.0, ev_tx, cmd_rx));
        let (media, media_rx) = std_mpsc::channel();
        let (note_tx, notes) = mpsc::unbounded_channel();
        let muted = Arc::new(AtomicBool::new(false));
        let thread = Audio { cmds: cmds.clone(), notes: note_tx, muted: muted.clone(), reopen: REOPEN_EVERY };
        let spawned = std::thread::Builder::new().name("rxcord-voice-audio".into()).spawn(move || {
            thread.run(
                || Playback::start(None).map_err(|e| tracing::warn!("audio output: {e}")).ok(),
                || Capture::start(None, NOISE_SUPPRESSION).map_err(|e| tracing::warn!("audio input: {e}")).ok(),
                media_rx,
            )
        });
        if let Err(e) = spawned {
            tracing::warn!("audio thread: {e}");
        }
        Conn { cmds, events, media: Some(media), notes, muted, status: VoiceStatus::Connecting, speaker: true, mic: true, level: false }
    }

    pub async fn next(&mut self) -> ConnIn {
        tokio::select! {
            e = self.events.recv() => e.map_or(ConnIn::Closed, ConnIn::Event),
            Some(n) = self.notes.recv() => ConnIn::Audio(n),
        }
    }

    /// A received Opus frame, for the audio thread (never blocks).
    pub fn media(&self, user_id: u64, seq: u16, opus: Vec<u8>) {
        if let Some(m) = &self.media {
            let _ = m.send((user_id, seq, opus));
        }
    }

    /// Muted: nothing is sent (the mic stays open so unmuting is instant).
    #[allow(dead_code)] // the mute button: Task 10
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Never blocks: the driver may itself be waiting to hand us an event.
    #[allow(dead_code)] // the audio thread sends on its own clone; Task 10's UI uses this
    pub fn send(&self, cmd: VoiceCmd) {
        if self.cmds.try_send(cmd).is_err() {
            tracing::debug!("voice command dropped: driver busy or gone");
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.cmds.try_send(VoiceCmd::Leave);
    }
}

/// What the audio thread plays through: `Playback`, or a fake in tests.
trait Output {
    fn push(&mut self, user_id: u64, seq: u16, opus: Vec<u8>);
    fn tick(&mut self);
    fn failed(&self) -> bool;
    fn levels(&self) -> Vec<(u64, bool)>;
}

impl Output for Playback {
    fn push(&mut self, user_id: u64, seq: u16, opus: Vec<u8>) {
        Playback::push(self, user_id, seq, opus)
    }
    fn tick(&mut self) {
        Playback::tick(self)
    }
    fn failed(&self) -> bool {
        Playback::failed(self)
    }
    fn levels(&self) -> Vec<(u64, bool)> {
        Playback::levels(self)
    }
}

/// What the audio thread records from: `Capture`, or a fake in tests.
trait Input {
    fn tick(&mut self) -> Vec<CaptureOut>;
    fn failed(&self) -> bool;
    fn set_muted(&mut self, muted: bool);
}

impl Input for Capture {
    fn tick(&mut self) -> Vec<CaptureOut> {
        Capture::tick(self)
    }
    fn failed(&self) -> bool {
        Capture::failed(self)
    }
    fn set_muted(&mut self, muted: bool) {
        Capture::set_muted(self, muted)
    }
}

/// A device the audio thread keeps open: a lost or missing one is reopened every `reopen`.
struct Slot<T> {
    dev: Option<T>,
    retry_at: std::time::Instant,
    /// Last reported open state.
    shown: Option<bool>,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Slot { dev: None, retry_at: std::time::Instant::now(), shown: None }
    }

    /// Drop a failed device, retry a missing one; whether it's open, when that changed.
    fn keep(&mut self, now: std::time::Instant, reopen: Duration, failed: impl Fn(&T) -> bool, open: impl FnOnce() -> Option<T>) -> Option<bool> {
        if self.dev.as_ref().is_some_and(failed) {
            self.dev = None;
            self.retry_at = now + reopen;
        }
        if self.dev.is_none() && now >= self.retry_at {
            self.dev = open();
            self.retry_at = now + reopen;
        }
        let open = self.dev.is_some();
        (self.shown != Some(open)).then(|| *self.shown.insert(open))
    }
}

/// The audio thread's ends of the channels.
struct Audio {
    cmds: mpsc::Sender<VoiceCmd>,
    notes: mpsc::UnboundedSender<AudioNote>,
    muted: Arc<AtomicBool>,
    reopen: Duration,
}

impl Audio {
    /// Feeds media to the output and the mic's packets to the driver, ticking both every 10 ms, and
    /// reports levels; lost or missing devices are reopened every `reopen` (the connection stays up
    /// meanwhile). Returns when `media` closes.
    fn run<O: Output, I: Input>(&self, mut open_out: impl FnMut() -> Option<O>, mut open_mic: impl FnMut() -> Option<I>, media: std_mpsc::Receiver<(u64, u16, Vec<u8>)>) {
        let (mut out, mut mic, mut levels) = (Slot::<O>::new(), Slot::<I>::new(), vec![]);
        let mut next_tick = std::time::Instant::now();
        loop {
            loop {
                match media.recv_timeout(next_tick.saturating_duration_since(std::time::Instant::now())) {
                    Ok((user, seq, opus)) => {
                        if let Some(o) = &mut out.dev {
                            o.push(user, seq, opus);
                        }
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            let now = std::time::Instant::now();
            next_tick = (next_tick + TICK).max(now);
            if let Some(open) = out.keep(now, self.reopen, O::failed, &mut open_out) {
                if !open {
                    tracing::warn!("no audio output");
                }
                let _ = self.notes.send(AudioNote::Speaker(open));
            }
            if let Some(open) = mic.keep(now, self.reopen, I::failed, &mut open_mic) {
                if !open {
                    tracing::warn!("no audio input");
                    let _ = self.cmds.try_send(VoiceCmd::Speaking(false)); // in case it died mid-sentence
                }
                let _ = self.notes.send(AudioNote::Mic(open));
            }
            if let Some(o) = &mut out.dev {
                o.tick();
                let l = o.levels();
                if l != levels {
                    levels = l;
                    let _ = self.notes.send(AudioNote::Levels(levels.clone()));
                }
            }
            if let Some(m) = &mut mic.dev {
                m.set_muted(self.muted.load(Ordering::Relaxed));
                for o in m.tick() {
                    // never block the audio thread: a busy driver loses the packet
                    let _ = match o {
                        CaptureOut::Opus(f) => self.cmds.try_send(VoiceCmd::Opus(f)).map_err(drop),
                        CaptureOut::Speaking(on) => self.cmds.try_send(VoiceCmd::Speaking(on)).map_err(drop),
                        CaptureOut::Level(on) => self.notes.send(AudioNote::Level(on)).map_err(drop),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rxcord_api::event::parse_dispatch;
    use serde_json::{json, value::to_raw_value};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const ME: Id = Id(1);
    const G: Id = Id(10);
    const A: Id = Id(100);
    const B: Id = Id(101);

    fn ev(name: &str, d: serde_json::Value) -> Event {
        parse_dispatch(name, &to_raw_value(&d).unwrap()).unwrap()
    }
    fn state(user: Id, ch: Option<Id>) -> Event {
        ev("VOICE_STATE_UPDATE", json!({"user_id": user.to_string(), "channel_id": ch.map(|c| c.to_string()),
            "guild_id": G.to_string(), "session_id": "sess"}))
    }
    fn server(endpoint: Option<&str>) -> Event {
        ev("VOICE_SERVER_UPDATE", json!({"token": "tok", "guild_id": G.to_string(), "endpoint": endpoint}))
    }

    /// Effects as short strings.
    fn names(fx: Vec<Effect>) -> Vec<String> {
        fx.into_iter()
            .map(|e| match e {
                Effect::Gateway(GatewayCmd::VoiceState { guild_id, channel_id, .. }) => {
                    format!("op4 {} {}", guild_id.unwrap().0, channel_id.map_or("null".into(), |c| c.0.to_string()))
                }
                Effect::Connect { connect: c, channel } => {
                    format!("connect {} {} {} {} {} {}", channel.0, c.server_id, c.user_id, c.session_id, c.token, c.endpoint)
                }
                Effect::Disconnect => "disconnect".into(),
                Effect::Notice(n) => format!("notice {n}"),
            })
            .collect()
    }

    fn connected_to(c: &mut Controller, ch: Id, now: Instant) {
        assert_eq!(names(c.join(G, ch, now)), [format!("op4 10 {}", ch.0)]);
        assert!(names(c.event(&server(Some("v.discord.media")), ME)).is_empty(), "waits for our state too");
        let other = ev("VOICE_STATE_UPDATE", json!({"user_id": "2", "channel_id": ch.to_string(), "session_id": "x"}));
        assert!(names(c.event(&other, ME)).is_empty(), "someone else's state");
        assert_eq!(names(c.event(&state(ME, Some(ch)), ME)), [format!("connect {} 10 1 sess tok v.discord.media", ch.0)]);
        assert!(c.connected() && c.deadline().is_none());
    }

    #[test]
    fn double_join_moves() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        assert!(names(c.join(G, A, now)).is_empty(), "already there: no second op 4");
        assert_eq!(names(c.join(G, B, now)), ["disconnect", "op4 10 101"], "old driver stopped, one op 4");
        assert_eq!(c.channel(), Some((G, B)));
        assert!(!c.connected());
        assert!(names(c.event(&state(ME, Some(A)), ME)).is_empty(), "stale state for A");
        assert!(names(c.event(&state(ME, None), ME)).is_empty(), "echo of leaving A doesn't cancel the join");
        assert!(names(c.event(&state(ME, Some(B)), ME)).is_empty(), "the server for B hasn't come yet");
        assert_eq!(names(c.event(&server(Some("w")), ME)), ["connect 101 10 1 sess tok w"]);
        assert!(names(c.event(&server(Some("w")), ME)).is_empty(), "replayed after a gateway reconnect: kept");
        assert_eq!(names(c.event(&server(Some("x")), ME)), ["disconnect", "connect 101 10 1 sess tok x"], "server moved");
    }

    #[test]
    fn join_timeout() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        c.join(G, A, t0);
        c.event(&state(ME, Some(A)), ME);
        assert!(names(c.event(&server(None), ME)).is_empty(), "endpoint null: keep waiting");
        assert_eq!(c.deadline(), Some(t0 + JOIN_TIMEOUT));
        assert!(names(c.expire(t0 + Duration::from_secs(9))).is_empty());
        assert_eq!(names(c.expire(t0 + JOIN_TIMEOUT)), ["op4 10 null", "notice Couldn't join voice: Discord didn't answer"]);
        assert_eq!((c.channel(), c.deadline()), (None, None), "state cleared");
        assert!(names(c.event(&server(Some("late")), ME)).is_empty(), "a late answer starts nothing");
        assert!(names(c.expire(t0 + JOIN_TIMEOUT * 2)).is_empty());
    }

    #[test]
    fn hangup_mid_handshake_sends_null_once() {
        let (mut c, now) = (Controller::default(), Instant::now());
        c.join(G, A, now);
        c.event(&state(ME, Some(A)), ME);
        assert_eq!(names(c.hang_up()), ["op4 10 null"], "nothing running yet: no disconnect");
        assert!(names(c.hang_up()).is_empty(), "null sent once");
        assert!(names(c.event(&server(Some("v")), ME)).is_empty(), "late server update: no zombie driver");
        connected_to(&mut c, A, now); // driver started, voice WS still handshaking
        assert_eq!(names(c.hang_up()), ["disconnect", "op4 10 null"]);
        assert!(names(c.hang_up()).is_empty());
        assert!(names(c.left(&LeaveReason::Fatal(0))).is_empty(), "the stopped driver's last words change nothing");
    }

    #[test]
    fn kicked_or_lost_leaves_once() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        assert_eq!(names(c.left(&LeaveReason::Kicked)), ["disconnect", "op4 10 null", "notice Disconnected from voice"]);
        connected_to(&mut c, A, now);
        assert_eq!(names(c.event(&state(ME, None), ME)), ["disconnect", "notice Disconnected from voice"]);
        assert_eq!(c.channel(), None);
    }

    #[test]
    fn conn_drop_stops_driver() {
        let (cmds, mut cmd_rx) = mpsc::channel(1);
        let (ev_tx, events) = mpsc::channel(1);
        let (media, media_rx) = std_mpsc::channel();
        let (_note_tx, notes) = mpsc::unbounded_channel();
        let muted = Arc::new(AtomicBool::new(false));
        let conn = Conn { cmds, events, media: Some(media), notes, muted, status: VoiceStatus::Connected, speaker: true, mic: true, level: false };
        conn.send(VoiceCmd::Speaking(true));
        conn.send(VoiceCmd::Speaking(false)); // full: dropped, never blocks
        drop(conn);
        assert!(matches!(cmd_rx.try_recv(), Ok(VoiceCmd::Speaking(true))));
        assert!(cmd_rx.try_recv().is_err() && cmd_rx.is_closed(), "driver's commands closed");
        assert!(ev_tx.is_closed(), "driver sees its events closed: it stops");
        assert!(media_rx.recv().is_err(), "audio thread's media closed: it exits");
    }

    struct Fake {
        failed: Arc<AtomicBool>,
        pushed: Arc<AtomicUsize>,
    }
    impl Output for Fake {
        fn push(&mut self, _: u64, _: u16, _: Vec<u8>) {
            self.pushed.fetch_add(1, Ordering::Relaxed);
        }
        fn tick(&mut self) {}
        fn failed(&self) -> bool {
            self.failed.load(Ordering::Relaxed)
        }
        fn levels(&self) -> Vec<(u64, bool)> {
            vec![(7, self.pushed.load(Ordering::Relaxed) > 0)]
        }
    }

    #[test]
    fn device_loss_reopens_while_connected() {
        let (failed, pushed, opens) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (f, p, n) = (failed.clone(), pushed.clone(), opens.clone());
        // opens: 1st works, 2nd (right after the loss) fails, 3rd works
        let open = move || {
            let i = n.fetch_add(1, Ordering::Relaxed);
            f.store(false, Ordering::Relaxed);
            (i != 1).then(|| Fake { failed: f.clone(), pushed: p.clone() })
        };
        let (media, mut notes, _cmds, _muted, thread) = spawn_audio(open, || None::<FakeMic>);
        let mut next = || notes.blocking_recv().unwrap();
        assert_eq!(next(), AudioNote::Speaker(true));
        assert_eq!(next(), AudioNote::Mic(false), "no microphone: the call goes on");
        assert_eq!(next(), AudioNote::Levels(vec![(7, false)]));
        media.send((7, 0, vec![1])).unwrap();
        assert_eq!(next(), AudioNote::Levels(vec![(7, true)]));
        failed.store(true, Ordering::Relaxed); // unplugged
        assert_eq!(next(), AudioNote::Speaker(false), "\"No speaker\", the media channel stays open");
        assert_eq!(next(), AudioNote::Speaker(true), "reopened after the retry interval");
        assert_eq!(opens.load(Ordering::Relaxed), 3);
        drop(media);
        thread.join().unwrap();
    }

    type Spawned = (std_mpsc::Sender<(u64, u16, Vec<u8>)>, mpsc::UnboundedReceiver<AudioNote>, mpsc::Receiver<VoiceCmd>, Arc<AtomicBool>, std::thread::JoinHandle<()>);

    /// The audio thread with a 30 ms reopen interval.
    fn spawn_audio<O: Output, I: Input>(open_out: impl FnMut() -> Option<O> + Send + 'static, open_mic: impl FnMut() -> Option<I> + Send + 'static) -> Spawned {
        let (media, media_rx) = std_mpsc::channel();
        let (note_tx, notes) = mpsc::unbounded_channel();
        let (cmds, cmd_rx) = mpsc::channel(64);
        let muted = Arc::new(AtomicBool::new(false));
        let audio = Audio { cmds, notes: note_tx, muted: muted.clone(), reopen: Duration::from_millis(30) };
        let thread = std::thread::spawn(move || audio.run(open_out, open_mic, media_rx));
        (media, notes, cmd_rx, muted, thread)
    }

    struct FakeMic {
        failed: Arc<AtomicBool>,
        muted: Arc<AtomicBool>,
        out: Vec<CaptureOut>,
    }
    impl Input for FakeMic {
        fn tick(&mut self) -> Vec<CaptureOut> {
            std::mem::take(&mut self.out)
        }
        fn failed(&self) -> bool {
            self.failed.load(Ordering::Relaxed)
        }
        fn set_muted(&mut self, muted: bool) {
            self.muted.store(muted, Ordering::Relaxed);
        }
    }

    #[test]
    fn mic_packets_go_to_the_driver_and_loss_reopens() {
        let (failed, mic_muted, opens) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let (f, m, n) = (failed.clone(), mic_muted.clone(), opens.clone());
        let open_mic = move || {
            n.fetch_add(1, Ordering::Relaxed);
            f.store(false, Ordering::Relaxed);
            m.store(false, Ordering::Relaxed);
            let out = vec![CaptureOut::Level(true), CaptureOut::Speaking(true), CaptureOut::Opus(vec![1])];
            Some(FakeMic { failed: f.clone(), muted: m.clone(), out })
        };
        let (media, mut notes, mut cmds, muted, thread) = spawn_audio(|| None::<Fake>, open_mic);
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Speaker(false)));
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Mic(true)));
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Level(true)), "level dot");
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(true))));
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Opus(f)) if f == [1]));
        muted.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !mic_muted.load(Ordering::Relaxed) {
            assert!(std::time::Instant::now() < deadline, "mute reaches the mic");
            std::thread::yield_now();
        }
        failed.store(true, Ordering::Relaxed); // unplugged
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Mic(false)), "\"No microphone\", still connected");
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(false))), "we stop speaking");
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Mic(true)), "reopened after the retry interval");
        assert_eq!(opens.load(Ordering::Relaxed), 2);
        drop(media);
        thread.join().unwrap();
        assert!(mic_muted.load(Ordering::Relaxed), "the reopened mic is muted too");
    }
}
