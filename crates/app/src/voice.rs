//! Runtime-side voice. `Controller` is the join handshake (op 4, then our VOICE_STATE_UPDATE and the
//! VOICE_SERVER_UPDATE) as pure state; `Conn` is what runs while connected: the driver task and the
//! audio thread. Nothing here opens a device or spawns a thread unless a `Conn` exists.
use rxcord_api::event::Event;
use rxcord_api::gateway::GatewayCmd;
use rxcord_api::id::Id;
use rxcord_voice::audio::capture::{Capture, CaptureOut, SILENCE, SILENCE_FRAMES};
use rxcord_voice::audio::playback::Playback;
use crate::settings::Settings;
use rxcord_voice::driver::{VoiceCmd, VoiceEvent, VoiceStatus, run_voice};
use rxcord_voice::session::{Connect, LeaveReason};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// After a close 4014 the main gateway may still bring a move or a new server; we wait this long.
const FOLLOW_WINDOW: Duration = Duration::from_secs(5);
/// An unanswered incoming call stops ringing here.
pub const RING_TIMEOUT: Duration = Duration::from_secs(45);
/// How often a lost or missing device is tried again.
const REOPEN_EVERY: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(10);
/// The level dot changes at most this often.
const LEVEL_EVERY: Duration = Duration::from_millis(100);

/// What the runtime must do after a controller call, in order.
#[derive(Debug)]
pub enum Effect {
    /// Main gateway op 4.
    Gateway(GatewayCmd),
    /// Start a voice connection; a running one restarts its driver there and keeps its devices.
    Connect { connect: Connect, channel: Id },
    /// Stop the running voice connection and its devices.
    Disconnect,
    /// Shown in the status bar for a few seconds.
    Notice(&'static str),
    /// Ring the other people of this DM call (REST).
    Ring(Id),
}

struct Target {
    /// None for a DM call.
    guild: Option<Id>,
    channel: Id,
    /// We started this call: ring the others once our voice state is in.
    ring: bool,
    session: Option<String>,
    /// (token, endpoint)
    server: Option<(String, String)>,
    /// A `Connect` was issued (the driver may still be handshaking).
    connected: bool,
    /// Join gives up at this time unless connected.
    deadline: Instant,
    /// The voice session went invalid and we already asked Discord again (Ruling R3: once until it works).
    rejoined: bool,
    /// The voice server closed with 4014: waiting `FOLLOW_WINDOW` for a move or a new server.
    dropped: bool,
}

#[derive(Default)]
pub struct Controller {
    target: Option<Target>,
    /// Self mute/deafen (kept across joins); deafened implies muted.
    mute: bool,
    deaf: bool,
    /// The mute from before deafening, restored on undeafen.
    mute_before_deaf: bool,
}

impl Controller {
    fn op4(&self, guild: Option<Id>, channel: Option<Id>) -> Effect {
        Effect::Gateway(GatewayCmd::VoiceState { guild_id: guild, channel_id: channel, self_mute: self.mute, self_deaf: self.deaf })
    }

    pub fn muted(&self) -> bool {
        self.mute
    }

    pub fn deafened(&self) -> bool {
        self.deaf
    }

    /// The 🎤 button. Unmuting while deafened undeafens too.
    pub fn toggle_mute(&mut self) -> Vec<Effect> {
        self.mute = !self.mute;
        self.deaf &= self.mute;
        self.resend()
    }

    /// The 🎧 button: deafen mutes; undeafen restores the mute from before.
    pub fn toggle_deafen(&mut self) -> Vec<Effect> {
        if self.deaf {
            self.mute = self.mute_before_deaf;
        } else {
            self.mute_before_deaf = self.mute;
            self.mute = true;
        }
        self.deaf = !self.deaf;
        self.resend()
    }

    /// Op 4 with the new flags, same channel; nothing when not in voice.
    fn resend(&self) -> Vec<Effect> {
        self.target.iter().map(|t| self.op4(t.guild, Some(t.channel))).collect()
    }

    /// (guild, channel) we're in or joining; guild None for a DM call.
    pub fn channel(&self) -> Option<(Option<Id>, Id)> {
        self.target.as_ref().map(|t| (t.guild, t.channel))
    }

    pub fn connected(&self) -> bool {
        self.target.as_ref().is_some_and(|t| t.connected)
    }

    /// When `expire` must be called, while a join is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.target.as_ref().filter(|t| !t.connected).map(|t| t.deadline)
    }

    /// Join `channel` (guild None: a DM call), leaving (moving out of) any current one: one op 4.
    pub fn join(&mut self, guild: Option<Id>, channel: Id, now: Instant) -> Vec<Effect> {
        if self.target.as_ref().is_some_and(|t| t.channel == channel) {
            return vec![];
        }
        let mut fx = vec![];
        if self.connected() {
            fx.push(Effect::Disconnect);
        }
        self.target = Some(Target { guild, channel, ring: false, session: None, server: None, connected: false, deadline: now + JOIN_TIMEOUT, rejoined: false, dropped: false });
        fx.push(self.op4(guild, Some(channel)));
        fx
    }

    /// Start a call in DM `channel`: join it, then ring the others once Discord accepted the join.
    pub fn call(&mut self, channel: Id, now: Instant) -> Vec<Effect> {
        let fx = self.join(None, channel, now);
        if let Some(t) = self.target.as_mut().filter(|_| !fx.is_empty()) {
            t.ring = true;
        }
        fx
    }

    /// Hang up: op 4 with no channel, once.
    pub fn hang_up(&mut self) -> Vec<Effect> {
        let Some(t) = self.target.take() else { return vec![] };
        let mut fx = vec![];
        if t.connected {
            fx.push(Effect::Disconnect);
        }
        fx.push(self.op4(t.guild, None));
        fx
    }

    /// Main-gateway events; `me` is our user id.
    pub fn event(&mut self, ev: &Event, me: Id) -> Vec<Effect> {
        let Some(t) = &mut self.target else { return vec![] };
        // the voice server's id: the guild, or the DM channel for a call
        let server_id = t.guild.unwrap_or(t.channel);
        let mut fx = vec![];
        match ev {
            Event::VoiceStateUpdate(v) if v.user_id == me && v.channel_id == Some(t.channel) => {
                if std::mem::take(&mut t.ring) {
                    fx.push(Effect::Ring(t.channel));
                }
                // replayed after a main-gateway reconnect (READY/RESUMED): the connection keeps going
                if t.connected && t.session.as_ref() == Some(&v.session_id) {
                    return fx;
                }
                // a re-identify gave us a new session: the next server update restarts the driver with it
                if t.connected {
                    (t.server, t.rejoined) = (None, false);
                }
                t.session = Some(v.session_id.clone());
            }
            // Moved by a moderator (or another client of ours) within the guild: follow. Discord sends a
            // VOICE_SERVER_UPDATE with the move; forgetting the server makes it restart the driver for the new
            // channel even on the same endpoint. // VERIFY LIVE
            // After a 4014 the driver is gone already: any server update starts it.
            Event::VoiceStateUpdate(v) if v.user_id == me && (t.connected || t.dropped) && t.guild.is_some() && v.guild_id == t.guild && v.channel_id.is_some_and(|c| c != t.channel) => {
                (t.channel, t.session) = (v.channel_id.unwrap_or(t.channel), Some(v.session_id.clone()));
                if t.connected {
                    t.server = None;
                    return fx;
                }
            }
            // Disconnected by a moderator or another client of ours. While still joining it may be the
            // echo of leaving the previous channel, so only a live connection counts, and only for our
            // guild (or call): leaving a server's voice for a DM call removes the server's state late.
            Event::VoiceStateUpdate(v) if v.user_id == me && v.channel_id.is_none() && v.guild_id == t.guild && (t.connected || t.dropped) => {
                self.target = None;
                return vec![Effect::Disconnect, Effect::Notice("Disconnected from voice")];
            }
            // endpoint null: the server is still being allocated, another update follows
            Event::VoiceServerUpdate(s) if s.guild_id.or(s.channel_id) == Some(server_id) && s.endpoint.is_some() => {
                let endpoint = s.endpoint.clone().unwrap_or_default();
                // Same server while connected: a main-gateway reconnect replaying op 4; the voice
                // connection keeps going. // VERIFY LIVE
                let server = (s.token.clone(), endpoint);
                if t.connected && t.server.as_ref() == Some(&server) {
                    return fx;
                }
                t.server = Some(server);
            }
            _ => return fx,
        }
        let (Some(session), Some((token, endpoint))) = (&t.session, &t.server) else { return fx };
        let connect = Connect { server_id: server_id.0, user_id: me.0, session_id: session.clone(), token: token.clone(), endpoint: endpoint.clone() };
        (t.connected, t.dropped) = (true, false);
        fx.push(Effect::Connect { connect, channel: t.channel });
        fx
    }

    /// The driver reached Ready (or resumed): a later invalid session may rejoin again.
    pub fn voice_up(&mut self) {
        if let Some(t) = &mut self.target {
            t.rejoined = false;
        }
    }

    /// The pending join timed out (no answer from Discord): give up.
    pub fn expire(&mut self, now: Instant) -> Vec<Effect> {
        if self.deadline().is_none_or(|d| now < d) {
            return vec![];
        }
        let dropped = self.target.as_ref().is_some_and(|t| t.dropped);
        let mut fx = self.hang_up();
        fx.push(Effect::Notice(if dropped { "Disconnected from voice" } else { "Couldn't join voice: Discord didn't answer" }));
        fx
    }

    /// The driver gave up or was told to leave (kicked, moved, channel deleted).
    pub fn left(&mut self, reason: &LeaveReason, now: Instant) -> Vec<Effect> {
        // Ruling R3: a fresh Identify failed with 4006 too: ask the main gateway again (op 4, same
        // channel) once; Discord answers with new state/server updates. // VERIFY LIVE
        if let (LeaveReason::SessionInvalid, Some(t)) = (reason, self.target.as_mut())
            && t.connected
            && !t.rejoined
        {
            (t.rejoined, t.session, t.server, t.connected, t.deadline) = (true, None, None, false, now + JOIN_TIMEOUT);
            let (guild, channel) = (t.guild, t.channel);
            return vec![Effect::Disconnect, self.op4(guild, Some(channel))];
        }
        // 4014 is also how a moderator move or a voice server change closes the old socket, and the main
        // gateway's updates for it may come after: no op 4 null yet, `expire` hangs up if nothing comes.
        if let (LeaveReason::Kicked, Some(t)) = (reason, self.target.as_mut())
            && t.connected
        {
            (t.connected, t.dropped, t.server, t.deadline) = (false, true, None, now + FOLLOW_WINDOW);
            return vec![Effect::Disconnect];
        }
        let mut fx = self.hang_up();
        if fx.is_empty() {
            return fx;
        }
        fx.push(Effect::Notice(match reason {
            LeaveReason::Kicked => "Disconnected from voice",
            LeaveReason::BadEncryption => "Voice failed: unsupported encryption",
            LeaveReason::Fatal(_) | LeaveReason::SessionInvalid => "Voice connection lost",
        }));
        fx
    }
}

/// The incoming-call prompt: one call at a time, shown once per call while it rings us.
#[derive(Default)]
pub struct Incoming {
    /// The DM channel and when its prompt appeared.
    prompt: Option<(Id, Instant)>,
    /// Calls we answered (accept, decline, timeout) that still ring us: not shown again.
    answered: HashSet<Id>,
}

impl Incoming {
    /// The call being prompted for.
    pub fn channel(&self) -> Option<Id> {
        self.prompt.map(|p| p.0)
    }

    /// When the prompt gives up (`end` it then).
    pub fn deadline(&self) -> Option<Instant> {
        self.prompt.map(|p| p.1 + RING_TIMEOUT)
    }

    /// Calls changed; `ringing` is `Store::ringing_me`. Returns the call that just got its prompt (notify).
    pub fn sync(&mut self, ringing: &[Id], now: Instant) -> Option<Id> {
        self.answered.retain(|c| ringing.contains(c));
        if self.prompt.is_some_and(|p| !ringing.contains(&p.0)) {
            self.prompt = None;
        }
        if self.prompt.is_some() {
            return None;
        }
        let c = *ringing.iter().find(|c| !self.answered.contains(c))?;
        self.prompt = Some((c, now));
        Some(c)
    }

    /// Accepted, declined or timed out: the prompt goes and isn't shown again while that call rings.
    pub fn end(&mut self) -> Option<Id> {
        let (c, _) = self.prompt.take()?;
        self.answered.insert(c);
        Some(c)
    }
}

/// A DM tab's 📞: answers the call ringing us in `ch` (no re-ring), else starts one and rings the others.
pub fn start_call(voice: &mut Controller, calls: &mut Incoming, ch: Id, now: Instant) -> Vec<Effect> {
    if calls.channel() == Some(ch) {
        calls.end();
        return voice.join(None, ch, now);
    }
    voice.call(ch, now)
}

/// What the voice bar shows; text "" when not in voice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bar {
    pub text: String,
    /// The channel we're in or joining ("" = none): the active DM tab's 📞 becomes ✖.
    pub channel: String,
    pub muted: bool,
    pub deafened: bool,
    /// The level dot: the mic hears speech and we're not muted.
    pub level: bool,
}

/// `new` when it differs from what's shown (which then becomes `new`); `None` spares the UI a wakeup.
pub fn changed<T: PartialEq + Clone>(shown: &mut T, new: T) -> Option<T> {
    (*shown != new).then(|| {
        shown.clone_from(&new);
        new
    })
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

/// From the runtime to the audio thread.
#[derive(Debug)]
pub enum ToAudio {
    /// A received Opus frame: (user, seq, packet).
    Media(u64, u16, Vec<u8>),
    Gain(u64, f32),
    Deafen(bool),
    Threshold(f32),
    /// Reopen the mic: (device, noise suppression).
    Input(Option<String>, bool),
    /// Reopen the speaker.
    Output(Option<String>),
    /// Mix the incoming-call ring in, or stop it.
    Ring(bool),
}

/// What the audio thread opens devices with and applies to them (again after a reopen).
#[derive(Debug, Clone, Default)]
pub struct AudioCfg {
    pub input: Option<String>,
    pub output: Option<String>,
    pub noise_suppression: bool,
    pub threshold: f32,
    pub deafened: bool,
    pub gains: HashMap<u64, f32>,
    pub ringing: bool,
}

impl AudioCfg {
    pub fn new(s: &Settings, deafened: bool) -> AudioCfg {
        let users = s.user_volume.keys().chain(&s.user_muted);
        AudioCfg {
            input: s.input_device.clone(),
            output: s.output_device.clone(),
            noise_suppression: s.noise_suppression,
            threshold: s.vad_threshold,
            deafened,
            gains: users.filter_map(|u| Some((u.parse().ok()?, s.gain(u)))).collect(),
            ringing: false,
        }
    }
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
    media: Option<std_mpsc::Sender<ToAudio>>,
    notes: mpsc::UnboundedReceiver<AudioNote>,
    /// Read by the audio thread every tick.
    muted: Arc<AtomicBool>,
    pub status: VoiceStatus,
    /// An output device is open.
    pub speaker: bool,
    /// A microphone is open.
    pub mic: bool,
    /// The mic hears speech (the voice bar's level dot).
    pub level: bool,
    /// Op 5: who said they're speaking.
    pub op5: HashSet<u64>,
    /// The decoded audio: users heard recently and whether they're talking now.
    pub heard: Vec<(u64, bool)>,
    /// DAVE's privacy code for this call, once end-to-end encryption is up.
    pub privacy: Option<String>,
}

impl Conn {
    /// Spawns the driver on the current tokio runtime and the audio thread.
    pub fn start(connect: Connect, channel: Id, cfg: AudioCfg) -> Conn {
        let (ev_tx, events) = mpsc::channel(256);
        let (cmds, cmd_rx) = mpsc::channel(64);
        tokio::spawn(run_voice(connect, channel.0, ev_tx, cmd_rx));
        let (media, media_rx) = std_mpsc::channel();
        let (note_tx, notes) = mpsc::unbounded_channel();
        let muted = Arc::new(AtomicBool::new(false));
        let thread = Audio { cmds: cmds.clone(), notes: note_tx, muted: muted.clone(), reopen: REOPEN_EVERY };
        let spawned = std::thread::Builder::new().name("rxcord-voice-audio".into()).spawn(move || {
            thread.run(
                cfg,
                |c: &AudioCfg| Playback::start(c.output.clone()).map_err(|e| tracing::warn!("audio output: {e}")).ok(),
                |c: &AudioCfg| Capture::start(c.input.clone(), c.noise_suppression).map_err(|e| tracing::warn!("audio input: {e}")).ok(),
                media_rx,
            )
        });
        if let Err(e) = &spawned {
            tracing::warn!("audio thread: {e}");
        }
        Conn::new(cmds, events, media, notes, muted, spawned.is_ok())
    }

    /// `audio`: the audio thread runs (otherwise there's no mic and no speaker, for good).
    fn new(cmds: mpsc::Sender<VoiceCmd>, events: mpsc::Receiver<VoiceEvent>, media: std_mpsc::Sender<ToAudio>, notes: mpsc::UnboundedReceiver<AudioNote>, muted: Arc<AtomicBool>, audio: bool) -> Conn {
        let (op5, heard) = (HashSet::new(), vec![]);
        Conn { cmds, events, media: Some(media), notes, muted, status: VoiceStatus::Connecting, speaker: audio, mic: audio, level: false, op5, heard, privacy: None }
    }

    /// Who's talking: the decoded audio for people we hear, op 5 only for those we haven't heard yet.
    pub fn speaking(&self) -> HashSet<Id> {
        let heard = |u: &u64| self.heard.iter().any(|h| h.0 == *u);
        let talking = self.heard.iter().filter(|h| h.1).map(|h| h.0);
        talking.chain(self.op5.iter().copied().filter(|u| !heard(u))).map(Id).collect()
    }

    /// What's wrong, for the voice bar: all of it, in this order.
    pub fn problems(&self) -> Vec<&'static str> {
        let mut p = vec![];
        if self.status != VoiceStatus::Connected {
            p.push("Connecting…");
        }
        if !self.mic {
            p.push("No microphone");
        }
        if !self.speaker {
            p.push("No speaker");
        }
        p
    }

    pub async fn next(&mut self) -> ConnIn {
        tokio::select! {
            e = self.events.recv() => e.map_or(ConnIn::Closed, ConnIn::Event),
            Some(n) = self.notes.recv() => ConnIn::Audio(n),
        }
    }

    /// For the audio thread (never blocks).
    pub fn audio(&self, m: ToAudio) {
        if let Some(tx) = &self.media {
            let _ = tx.send(m);
        }
    }

    /// Muted: nothing is sent (the mic stays open so unmuting is instant).
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// A new voice server (or channel) mid-call: only the driver starts over, the audio thread and its
    /// devices stay. `Err` gives `connect` back when the driver's queue is full (start a new `Conn`).
    pub fn restart(&mut self, connect: Connect, channel: Id) -> Result<(), Connect> {
        let Ok(slot) = self.cmds.try_reserve() else { return Err(connect) };
        slot.send(VoiceCmd::Restart(connect, channel.0));
        (self.status, self.op5, self.privacy) = (VoiceStatus::Connecting, HashSet::new(), None);
        Ok(())
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
    fn set_gain(&mut self, user_id: u64, gain: f32);
    fn set_deafened(&mut self, deafened: bool);
    fn set_ringing(&mut self, on: bool);
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
    fn set_gain(&mut self, user_id: u64, gain: f32) {
        Playback::set_gain(self, user_id, gain)
    }
    fn set_deafened(&mut self, deafened: bool) {
        Playback::set_deafened(self, deafened)
    }
    fn set_ringing(&mut self, on: bool) {
        Playback::set_ringing(self, on)
    }
}

/// What the audio thread records from: `Capture`, or a fake in tests.
trait Input {
    fn tick(&mut self) -> Vec<CaptureOut>;
    fn failed(&self) -> bool;
    fn set_muted(&mut self, muted: bool);
    fn set_threshold(&mut self, threshold: f32);
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
    fn set_threshold(&mut self, threshold: f32) {
        Capture::set_threshold(self, threshold)
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

    /// Close the device and open it again on the next `keep`.
    fn reopen(&mut self) {
        self.dev = None;
        self.retry_at = std::time::Instant::now();
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
    /// meanwhile). Settings changes apply live. Returns when `media` closes.
    fn run<O: Output, I: Input>(&self, mut cfg: AudioCfg, mut open_out: impl FnMut(&AudioCfg) -> Option<O>, mut open_mic: impl FnMut(&AudioCfg) -> Option<I>, media: std_mpsc::Receiver<ToAudio>) {
        let (mut out, mut mic, mut levels) = (Slot::<O>::new(), Slot::<I>::new(), vec![]);
        // the mic's last Speaking: its silence tail is ours to send if it dies mid-speech
        let mut speaking = false;
        // the level dot: heard now, last reported, next report allowed
        let (mut level, mut level_shown, mut level_at) = (false, false, std::time::Instant::now());
        let mut next_tick = std::time::Instant::now();
        loop {
            loop {
                match media.recv_timeout(next_tick.saturating_duration_since(std::time::Instant::now())) {
                    Ok(ToAudio::Media(user, seq, opus)) => {
                        if let Some(o) = &mut out.dev {
                            o.push(user, seq, opus);
                        }
                    }
                    Ok(ToAudio::Gain(user, gain)) => {
                        cfg.gains.insert(user, gain);
                        out.dev.iter_mut().for_each(|o| o.set_gain(user, gain));
                    }
                    Ok(ToAudio::Deafen(on)) => {
                        cfg.deafened = on;
                        out.dev.iter_mut().for_each(|o| o.set_deafened(on));
                    }
                    Ok(ToAudio::Threshold(t)) => {
                        cfg.threshold = t;
                        mic.dev.iter_mut().for_each(|m| m.set_threshold(t));
                    }
                    Ok(ToAudio::Input(dev, ns)) => {
                        (cfg.input, cfg.noise_suppression) = (dev, ns);
                        self.end_speech(&mut speaking);
                        mic.reopen();
                        level = false; // the new mic lights the dot once it hears speech
                    }
                    Ok(ToAudio::Output(dev)) => {
                        cfg.output = dev;
                        out.reopen();
                    }
                    Ok(ToAudio::Ring(on)) => {
                        cfg.ringing = on;
                        out.dev.iter_mut().for_each(|o| o.set_ringing(on));
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => break,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            let now = std::time::Instant::now();
            next_tick = (next_tick + TICK).max(now);
            let opened_out = || {
                open_out(&cfg).map(|mut o| {
                    o.set_deafened(cfg.deafened);
                    o.set_ringing(cfg.ringing);
                    cfg.gains.iter().for_each(|(u, g)| o.set_gain(*u, *g));
                    o
                })
            };
            if let Some(open) = out.keep(now, self.reopen, O::failed, opened_out) {
                if !open {
                    tracing::warn!("no audio output");
                }
                let _ = self.notes.send(AudioNote::Speaker(open));
            }
            let opened_mic = || {
                open_mic(&cfg).map(|mut m| {
                    m.set_threshold(cfg.threshold);
                    m
                })
            };
            if let Some(open) = mic.keep(now, self.reopen, I::failed, opened_mic) {
                if !open {
                    tracing::warn!("no audio input");
                    self.end_speech(&mut speaking);
                    level = false;
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
                        CaptureOut::Opus(f) => self.cmds.try_send(VoiceCmd::Opus(f)),
                        CaptureOut::Speaking(on) => {
                            speaking = on;
                            self.cmds.try_send(VoiceCmd::Speaking(on))
                        }
                        CaptureOut::Level(on) => {
                            level = on;
                            Ok(())
                        }
                    };
                }
            }
            if level != level_shown && now >= level_at {
                (level_shown, level_at) = (level, now + LEVEL_EVERY);
                let _ = self.notes.send(AudioNote::Level(level));
            }
        }
    }

    /// The mic stopped mid-speech: the silence tail and Speaking(false) it would have sent.
    fn end_speech(&self, speaking: &mut bool) {
        if std::mem::take(speaking) {
            for _ in 0..SILENCE_FRAMES {
                let _ = self.cmds.try_send(VoiceCmd::Opus(SILENCE.to_vec()));
            }
            let _ = self.cmds.try_send(VoiceCmd::Speaking(false));
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
                    let id = |i: Option<Id>| i.map_or("null".into(), |c| c.0.to_string());
                    format!("op4 {} {}", id(guild_id), id(channel_id))
                }
                Effect::Connect { connect: c, channel } => {
                    format!("connect {} {} {} {} {} {}", channel.0, c.server_id, c.user_id, c.session_id, c.token, c.endpoint)
                }
                Effect::Disconnect => "disconnect".into(),
                Effect::Notice(n) => format!("notice {n}"),
                Effect::Ring(c) => format!("ring {}", c.0),
            })
            .collect()
    }

    fn connected_to(c: &mut Controller, ch: Id, now: Instant) {
        assert_eq!(names(c.join(Some(G), ch, now)), [format!("op4 10 {}", ch.0)]);
        assert!(names(c.event(&server(Some("v.discord.media")), ME)).is_empty(), "waits for our state too");
        let other = ev("VOICE_STATE_UPDATE", json!({"user_id": "2", "channel_id": ch.to_string(), "session_id": "x"}));
        assert!(names(c.event(&other, ME)).is_empty(), "someone else's state");
        assert_eq!(names(c.event(&state(ME, Some(ch)), ME)), [format!("connect {} 10 1 sess tok v.discord.media", ch.0)]);
        assert!(c.connected() && c.deadline().is_none());
    }

    const DM: Id = Id(7);

    fn ready_with_dm() -> rxcord_store::Store {
        let mut s = rxcord_store::Store::new();
        s.apply(&ev("READY", json!({"user": {"id": "1", "username": "me"}, "session_id": "s",
            "private_channels": [{"id": "7", "type": 1, "recipient_ids": ["2"]}], "guilds": []})));
        s
    }
    fn call(name: &str, ringing: &[&str]) -> Event {
        ev(name, json!({"channel_id": "7", "ringing": ringing, "voice_states": [{"user_id": "2", "channel_id": "7", "session_id": "x"}]}))
    }

    #[test]
    fn incoming_call_prompt_once() {
        let (mut s, mut inc, t0) = (ready_with_dm(), Incoming::default(), Instant::now());
        s.apply(&call("CALL_CREATE", &["1"]));
        assert_eq!(inc.sync(&s.ringing_me(), t0), Some(DM), "prompt (and notify) once");
        assert_eq!((inc.channel(), inc.deadline()), (Some(DM), Some(t0 + RING_TIMEOUT)));
        s.apply(&call("CALL_UPDATE", &["1"]));
        assert_eq!(inc.sync(&s.ringing_me(), t0), None, "still ringing: no second prompt");
        assert_eq!(inc.channel(), Some(DM));
        s.apply(&call("CALL_UPDATE", &[]));
        assert_eq!(inc.sync(&s.ringing_me(), t0), None);
        assert_eq!((inc.channel(), inc.deadline()), (None, None), "not ringing me any more: prompt cleared");
        assert_eq!(inc.end(), None);
        // rung again, then declined (or timed out): gone, and not shown again while Discord still rings us
        s.apply(&call("CALL_UPDATE", &["1"]));
        assert_eq!(inc.sync(&s.ringing_me(), t0), Some(DM));
        assert_eq!(inc.end(), Some(DM));
        assert_eq!(inc.sync(&s.ringing_me(), t0), None);
        assert_eq!(inc.channel(), None);
        s.apply(&ev("CALL_DELETE", json!({"channel_id": "7"})));
        assert_eq!(inc.sync(&s.ringing_me(), t0), None);
        s.apply(&call("CALL_CREATE", &["1"]));
        assert_eq!(inc.sync(&s.ringing_me(), t0), Some(DM), "a new call prompts again");
        s.apply(&ev("CALL_DELETE", json!({"channel_id": "7"})));
        inc.sync(&s.ringing_me(), t0);
        assert_eq!(inc.channel(), None, "CALL_DELETE clears the prompt");
    }

    #[test]
    fn accept_while_in_voice_leaves_first() {
        let (mut c, mut inc, now) = (Controller::default(), Incoming::default(), Instant::now());
        connected_to(&mut c, A, now);
        inc.sync(&[DM], now);
        let ch = inc.end().unwrap();
        assert_eq!(inc.channel(), None, "prompt (and ring) gone before joining");
        assert_eq!(names(c.join(None, ch, now)), ["disconnect", "op4 null 7"], "guild voice stopped first, then one op 4 for the call");
        assert_eq!(c.channel(), Some((None, DM)));
        assert!(names(c.event(&state(ME, None), ME)).is_empty(), "the echo of leaving A");
        let mine = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": "7", "session_id": "sess"}));
        assert!(names(c.event(&mine, ME)).is_empty(), "accepted: nobody to ring");
        assert!(names(c.event(&server(Some("g")), ME)).is_empty(), "a guild's server update isn't ours");
        let dm_server = ev("VOICE_SERVER_UPDATE", json!({"token": "tok", "guild_id": null, "channel_id": "7", "endpoint": "d"}));
        assert_eq!(names(c.event(&dm_server, ME)), ["connect 7 7 1 sess tok d"], "server id = the DM channel");
    }

    #[test]
    fn late_guild_leave_keeps_the_call() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        c.join(None, DM, now);
        let mine = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": "7", "session_id": "sess"}));
        c.event(&mine, ME);
        let dm_server = ev("VOICE_SERVER_UPDATE", json!({"token": "tok", "guild_id": null, "channel_id": "7", "endpoint": "d"}));
        assert_eq!(names(c.event(&dm_server, ME)), ["connect 7 7 1 sess tok d"]);
        assert!(names(c.event(&state(ME, None), ME)).is_empty(), "the server's removal of our old state, after the call connected");
        assert_eq!((c.channel(), c.connected()), (Some((None, DM)), true));
        let left_dm = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": null, "session_id": "sess"}));
        assert_eq!(names(c.event(&left_dm, ME)), ["disconnect", "notice Disconnected from voice"], "dropped from the call itself");
    }

    #[test]
    fn call_button_on_a_ringing_dm_answers() {
        let (mut c, mut inc, now) = (Controller::default(), Incoming::default(), Instant::now());
        inc.sync(&[DM], now);
        assert_eq!(names(start_call(&mut c, &mut inc, DM, now)), ["op4 null 7"]);
        assert_eq!(inc.channel(), None, "prompt and ring gone");
        let mine = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": "7", "session_id": "sess"}));
        assert!(names(c.event(&mine, ME)).is_empty(), "answered: nobody re-rung");
        let (mut c, mut inc) = (Controller::default(), Incoming::default());
        assert_eq!(names(start_call(&mut c, &mut inc, DM, now)), ["op4 null 7"]);
        assert_eq!(names(c.event(&mine, ME)), ["ring 7"], "not ringing us: a new call rings the others");
    }

    #[test]
    fn start_call_rings_once_joined() {
        let (mut c, now) = (Controller::default(), Instant::now());
        assert_eq!(names(c.call(DM, now)), ["op4 null 7"]);
        let mine = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": "7", "session_id": "sess"}));
        assert_eq!(names(c.event(&mine, ME)), ["ring 7"], "join accepted: ring the others");
        assert!(names(c.event(&mine, ME)).is_empty(), "rung once");
        assert!(names(c.call(DM, now)).is_empty(), "already in it");
    }

    #[test]
    fn double_join_moves() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        assert!(names(c.join(Some(G), A, now)).is_empty(), "already there: no second op 4");
        assert_eq!(names(c.join(Some(G), B, now)), ["disconnect", "op4 10 101"], "old driver stopped, one op 4");
        assert_eq!(c.channel(), Some((Some(G), B)));
        assert!(!c.connected());
        assert!(names(c.event(&state(ME, Some(A)), ME)).is_empty(), "stale state for A");
        assert!(names(c.event(&state(ME, None), ME)).is_empty(), "echo of leaving A doesn't cancel the join");
        assert!(names(c.event(&state(ME, Some(B)), ME)).is_empty(), "the server for B hasn't come yet");
        assert_eq!(names(c.event(&server(Some("w")), ME)), ["connect 101 10 1 sess tok w"]);
        assert!(names(c.event(&server(Some("w")), ME)).is_empty(), "replayed after a gateway reconnect: kept");
        assert_eq!(names(c.event(&server(Some("x")), ME)), ["connect 101 10 1 sess tok x"], "server moved");
    }

    #[test]
    fn voice_server_change_reconnects() {
        // the runtime restarts only the driver on a Connect while connected: no Disconnect, devices stay
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        assert_eq!(names(c.event(&server(Some("other")), ME)), ["connect 100 10 1 sess tok other"]);
        assert!(c.connected());
    }

    #[test]
    fn moved_by_moderator_follows() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        let moved = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": B.to_string(), "guild_id": G.to_string(), "session_id": "sess2"}));
        assert!(names(c.event(&moved, ME)).is_empty(), "no leave, no op 4");
        assert_eq!((c.channel(), c.connected()), (Some((Some(G), B)), true), "the bar follows to B");
        assert_eq!(
            names(c.event(&server(Some("v.discord.media")), ME)),
            ["connect 101 10 1 sess2 tok v.discord.media"],
            "the move's server update restarts the driver for B, even on the same endpoint"
        );
        assert!(names(c.event(&server(Some("v.discord.media")), ME)).is_empty(), "a replay after that is kept");
        let dm_state = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": "7", "session_id": "x"}));
        assert!(names(c.event(&dm_state, ME)).is_empty(), "another place's state isn't a move");
        assert_eq!(c.channel(), Some((Some(G), B)));
    }

    #[test]
    fn main_gateway_ready_keeps_voice() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        let ready = ev("READY", json!({"user": {"id": "1", "username": "me"}, "session_id": "s2", "private_channels": [],
            "guilds": [{"id": G.to_string(), "name": "g", "channels": [{"id": A.to_string(), "type": 2, "name": "v"}],
                "voice_states": [{"user_id": "1", "channel_id": A.to_string(), "session_id": "sess"},
                                 {"user_id": "2", "channel_id": A.to_string(), "session_id": "x"}]}]}));
        let mut s = rxcord_store::Store::new();
        for _ in 0..2 {
            assert!(names(c.event(&ready, ME)).is_empty(), "READY: no leave");
            s.apply(&ready);
            s.apply(&state(ME, Some(A))); // replayed after the reconnect
            s.apply(&server(Some("v.discord.media")));
            assert!(names(c.event(&state(ME, Some(A)), ME)).is_empty() && names(c.event(&server(Some("v.discord.media")), ME)).is_empty());
        }
        assert!(c.connected(), "the voice connection survives");
        let ids: Vec<u64> = s.voice_members(A).iter().map(|v| v.user_id.0).collect();
        assert_eq!(ids, [1, 2], "the tree's members, once each");
    }

    #[test]
    fn session_invalid_rejoins_once() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, t0);
        assert_eq!(names(c.left(&LeaveReason::SessionInvalid, t0)), ["disconnect", "op4 10 100"], "rejoin through the main gateway");
        assert_eq!((c.channel(), c.connected(), c.deadline()), (Some((Some(G), A)), false, Some(t0 + JOIN_TIMEOUT)));
        c.event(&state(ME, Some(A)), ME);
        assert_eq!(names(c.event(&server(Some("v.discord.media")), ME)), ["connect 100 10 1 sess tok v.discord.media"], "same endpoint, new session: connects");
        assert_eq!(
            names(c.left(&LeaveReason::SessionInvalid, t0)),
            ["disconnect", "op4 10 null", "notice Voice connection lost"],
            "failed again: leave with a notice"
        );
    }

    #[test]
    fn audio_cmd_restart_keeps_devices() {
        let (cmds, mut cmd_rx) = mpsc::channel(1);
        let (_ev_tx, events) = mpsc::channel(1);
        let (media, media_rx) = std_mpsc::channel();
        let (_note_tx, notes) = mpsc::unbounded_channel();
        let mut k = Conn::new(cmds, events, media, notes, Arc::new(AtomicBool::new(false)), true);
        k.status = VoiceStatus::Connected;
        let connect = || Connect { server_id: 1, user_id: 2, session_id: "s".into(), token: "t".into(), endpoint: "e".into() };
        assert!(k.restart(connect(), Id(5)).is_ok());
        assert!(matches!(cmd_rx.try_recv(), Ok(VoiceCmd::Restart(c, 5)) if c.endpoint == "e"));
        assert_eq!(k.status, VoiceStatus::Connecting);
        k.send(VoiceCmd::Speaking(true));
        assert!(k.restart(connect(), Id(5)).is_err(), "driver busy: the caller starts a new connection");
        k.audio(ToAudio::Deafen(true));
        assert!(matches!(media_rx.try_recv(), Ok(ToAudio::Deafen(true))), "the audio thread's channel stays open");
    }

    #[test]
    fn mic_change_resets_level_dot() {
        let opens = Arc::new(AtomicUsize::new(0));
        let n = opens.clone();
        let open_mic = move |_: &AudioCfg| {
            let first = n.fetch_add(1, Ordering::Relaxed) == 0;
            let out = if first { vec![CaptureOut::Level(true)] } else { vec![] };
            Some(FakeMic { failed: Default::default(), muted: Default::default(), out, threshold: Default::default() })
        };
        let (media, mut notes, _cmds, _muted, thread) = spawn_audio(|_| None::<Fake>, open_mic);
        let mut next = || loop {
            match notes.blocking_recv() {
                Some(AudioNote::Level(l)) => break l,
                Some(_) => continue,
                None => panic!("audio thread gone"),
            }
        };
        assert!(next(), "the old mic hears speech");
        media.send(ToAudio::Input(Some("usb".into()), true)).unwrap();
        assert!(!next(), "another mic: the dot goes out until it hears something");
        drop(media);
        thread.join().unwrap();
    }

    #[test]
    fn join_timeout() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        c.join(Some(G), A, t0);
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
        c.join(Some(G), A, now);
        c.event(&state(ME, Some(A)), ME);
        assert_eq!(names(c.hang_up()), ["op4 10 null"], "nothing running yet: no disconnect");
        assert!(names(c.hang_up()).is_empty(), "null sent once");
        assert!(names(c.event(&server(Some("v")), ME)).is_empty(), "late server update: no zombie driver");
        connected_to(&mut c, A, now); // driver started, voice WS still handshaking
        assert_eq!(names(c.hang_up()), ["disconnect", "op4 10 null"]);
        assert!(names(c.hang_up()).is_empty());
        assert!(names(c.left(&LeaveReason::Fatal(0), now)).is_empty(), "the stopped driver's last words change nothing");
    }

    #[test]
    fn closed_4014_then_moved_follows() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, t0);
        assert_eq!(names(c.left(&LeaveReason::Kicked, t0)), ["disconnect"], "no op 4 null: the move's updates may still come");
        assert_eq!((c.channel(), c.connected(), c.deadline()), (Some((Some(G), A)), false, Some(t0 + FOLLOW_WINDOW)));
        let moved = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": B.to_string(), "guild_id": G.to_string(), "session_id": "sess2"}));
        assert!(names(c.event(&moved, ME)).is_empty(), "waits for the new server");
        assert_eq!(c.channel(), Some((Some(G), B)));
        assert_eq!(names(c.event(&server(Some("v.discord.media")), ME)), ["connect 101 10 1 sess2 tok v.discord.media"]);
        assert!(c.connected() && c.deadline().is_none());
        // a voice server change: only the server update follows the 4014
        assert_eq!(names(c.left(&LeaveReason::Kicked, t0)), ["disconnect"]);
        assert_eq!(names(c.event(&server(Some("other")), ME)), ["connect 101 10 1 sess2 tok other"]);
        // a real kick: our state goes null within the window
        assert_eq!(names(c.left(&LeaveReason::Kicked, t0)), ["disconnect"]);
        assert_eq!(names(c.event(&state(ME, None), ME)), ["disconnect", "notice Disconnected from voice"]);
        assert_eq!(c.channel(), None);
    }

    #[test]
    fn closed_4014_alone_hangs_up_after_the_window() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, t0);
        assert_eq!(names(c.left(&LeaveReason::Kicked, t0)), ["disconnect"]);
        assert!(names(c.expire(t0 + Duration::from_secs(4))).is_empty());
        assert_eq!(names(c.expire(t0 + FOLLOW_WINDOW)), ["op4 10 null", "notice Disconnected from voice"]);
        assert_eq!(c.channel(), None);
    }

    #[test]
    fn main_gateway_reidentify_restarts_the_driver() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        let fresh = ev("VOICE_STATE_UPDATE", json!({"user_id": "1", "channel_id": A.to_string(), "guild_id": G.to_string(), "session_id": "sess2"}));
        assert!(names(c.event(&fresh, ME)).is_empty(), "new session: waits for its server update");
        let srv = ev("VOICE_SERVER_UPDATE", json!({"token": "tok2", "guild_id": G.to_string(), "endpoint": "v.discord.media"}));
        assert_eq!(names(c.event(&srv, ME)), ["connect 100 10 1 sess2 tok2 v.discord.media"], "same endpoint, new token: restart");
        assert!(names(c.event(&fresh, ME)).is_empty() && names(c.event(&srv, ME)).is_empty(), "replays kept");
        assert!(c.connected());
    }

    #[test]
    fn session_invalid_twice_in_a_call_recovers_both_times() {
        let (mut c, t0) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, t0);
        for _ in 0..2 {
            assert_eq!(names(c.left(&LeaveReason::SessionInvalid, t0)), ["disconnect", "op4 10 100"]);
            c.event(&state(ME, Some(A)), ME);
            assert_eq!(names(c.event(&server(Some("v.discord.media")), ME)), ["connect 100 10 1 sess tok v.discord.media"]);
            c.voice_up();
        }
    }

    #[test]
    fn kicked_or_lost_leaves_once() {
        let (mut c, now) = (Controller::default(), Instant::now());
        connected_to(&mut c, A, now);
        assert_eq!(names(c.left(&LeaveReason::Fatal(0), now)), ["disconnect", "op4 10 null", "notice Voice connection lost"]);
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
        let conn = Conn::new(cmds, events, media, notes, muted, true);
        conn.send(VoiceCmd::Speaking(true));
        conn.send(VoiceCmd::Speaking(false)); // full: dropped, never blocks
        drop(conn);
        assert!(matches!(cmd_rx.try_recv(), Ok(VoiceCmd::Speaking(true))));
        assert!(cmd_rx.try_recv().is_err() && cmd_rx.is_closed(), "driver's commands closed");
        assert!(ev_tx.is_closed(), "driver sees its events closed: it stops");
        assert!(media_rx.recv().is_err(), "audio thread's media closed: it exits");
    }

    #[derive(Default)]
    struct Fake {
        failed: Arc<AtomicBool>,
        pushed: Arc<AtomicUsize>,
        /// set_gain/set_deafened calls, as text
        log: Arc<std::sync::Mutex<Vec<String>>>,
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
        fn set_gain(&mut self, user_id: u64, gain: f32) {
            self.log.lock().unwrap().push(format!("gain {user_id} {gain}"));
        }
        fn set_deafened(&mut self, deafened: bool) {
            self.log.lock().unwrap().push(format!("deafened {deafened}"));
        }
        fn set_ringing(&mut self, on: bool) {
            if on {
                self.log.lock().unwrap().push("ringing".into());
            }
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
            (i != 1).then(|| Fake { failed: f.clone(), pushed: p.clone(), ..Default::default() })
        };
        let (media, mut notes, _cmds, _muted, thread) = spawn_audio(move |_| open(), |_| None::<FakeMic>);
        let mut next = || notes.blocking_recv().unwrap();
        assert_eq!(next(), AudioNote::Speaker(true));
        assert_eq!(next(), AudioNote::Mic(false), "no microphone: the call goes on");
        assert_eq!(next(), AudioNote::Levels(vec![(7, false)]));
        media.send(ToAudio::Media(7, 0, vec![1])).unwrap();
        assert_eq!(next(), AudioNote::Levels(vec![(7, true)]));
        failed.store(true, Ordering::Relaxed); // unplugged
        assert_eq!(next(), AudioNote::Speaker(false), "\"No speaker\", the media channel stays open");
        assert_eq!(next(), AudioNote::Speaker(true), "reopened after the retry interval");
        assert_eq!(opens.load(Ordering::Relaxed), 3);
        drop(media);
        thread.join().unwrap();
    }

    type Spawned = (std_mpsc::Sender<ToAudio>, mpsc::UnboundedReceiver<AudioNote>, mpsc::Receiver<VoiceCmd>, Arc<AtomicBool>, std::thread::JoinHandle<()>);

    /// The audio thread with a 30 ms reopen interval.
    fn spawn_audio<O: Output, I: Input>(open_out: impl FnMut(&AudioCfg) -> Option<O> + Send + 'static, open_mic: impl FnMut(&AudioCfg) -> Option<I> + Send + 'static) -> Spawned {
        let (media, media_rx) = std_mpsc::channel();
        let (note_tx, notes) = mpsc::unbounded_channel();
        let (cmds, cmd_rx) = mpsc::channel(64);
        let muted = Arc::new(AtomicBool::new(false));
        let audio = Audio { cmds, notes: note_tx, muted: muted.clone(), reopen: Duration::from_millis(30) };
        let thread = std::thread::spawn(move || audio.run(AudioCfg::default(), open_out, open_mic, media_rx));
        (media, notes, cmd_rx, muted, thread)
    }

    struct FakeMic {
        failed: Arc<AtomicBool>,
        muted: Arc<AtomicBool>,
        out: Vec<CaptureOut>,
        threshold: Arc<std::sync::Mutex<f32>>,
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
        fn set_threshold(&mut self, threshold: f32) {
            *self.threshold.lock().unwrap() = threshold;
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
            Some(FakeMic { failed: f.clone(), muted: m.clone(), out, threshold: Default::default() })
        };
        let (media, mut notes, mut cmds, muted, thread) = spawn_audio(|_| None::<Fake>, move |_| open_mic());
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
        for i in 0..5 {
            assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Opus(f)) if f == SILENCE), "silence frame {i}");
        }
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(false))), "then we stop speaking");
        assert_eq!(notes.blocking_recv(), Some(AudioNote::Mic(true)), "reopened after the retry interval");
        assert_eq!(opens.load(Ordering::Relaxed), 2);
        drop(media);
        thread.join().unwrap();
        assert!(mic_muted.load(Ordering::Relaxed), "the reopened mic is muted too");
    }

    fn op4_flags(fx: Vec<Effect>) -> Vec<(Option<Id>, bool, bool)> {
        fx.into_iter()
            .filter_map(|e| match e {
                Effect::Gateway(GatewayCmd::VoiceState { channel_id, self_mute, self_deaf, .. }) => Some((channel_id, self_mute, self_deaf)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn unchanged_voice_bar_is_not_pushed() {
        // VOICE_STATE_UPDATEs from any server while we're not in voice: the bar stays empty, no UI wakeup
        let mut shown = Bar::default();
        assert_eq!(changed(&mut shown, Bar::default()), None);
        let bar = Bar { text: "🔊 G / v".into(), ..Default::default() };
        assert_eq!(changed(&mut shown, bar.clone()), Some(bar.clone()));
        assert_eq!(changed(&mut shown, bar.clone()), None, "same again");
        assert!(changed(&mut shown, Bar { level: true, ..bar }).is_some(), "level dot changed");
    }

    #[test]
    fn deafen_implies_mute_and_restores() {
        let (mut c, now) = (Controller::default(), Instant::now());
        assert!(c.toggle_deafen().is_empty(), "not in voice: nothing sent");
        c.toggle_deafen();
        connected_to(&mut c, A, now);
        assert_eq!(op4_flags(c.toggle_mute()), [(Some(A), true, false)], "mute: op 4 same channel");
        assert_eq!(op4_flags(c.toggle_deafen()), [(Some(A), true, true)], "deafen implies mute");
        assert_eq!(op4_flags(c.toggle_deafen()), [(Some(A), true, false)], "undeafen: still muted from before");
        c.toggle_mute();
        assert_eq!(op4_flags(c.toggle_deafen()), [(Some(A), true, true)]);
        assert_eq!(op4_flags(c.toggle_deafen()), [(Some(A), false, false)], "undeafen: unmuted as before");
        c.toggle_deafen();
        assert_eq!(op4_flags(c.toggle_mute()), [(Some(A), false, false)], "unmuting undeafens");
        c.toggle_mute();
        assert_eq!(op4_flags(c.join(Some(G), B, now)), [(Some(B), true, false)], "flags kept across joins");
        assert_eq!(op4_flags(c.hang_up()), [(None, true, false)]);
        assert!((c.muted(), c.deafened()) == (true, false));
    }

    fn test_conn(audio: bool) -> Conn {
        let (cmds, _) = mpsc::channel(1);
        let (_, events) = mpsc::channel(1);
        let (_, notes) = mpsc::unbounded_channel();
        Conn::new(cmds, events, std_mpsc::channel().0, notes, Arc::new(AtomicBool::new(false)), audio)
    }

    #[test]
    fn voice_bar_shows_every_problem() {
        let mut k = test_conn(false);
        assert_eq!(k.problems(), ["Connecting…", "No microphone", "No speaker"], "audio thread didn't spawn: no devices, handshake still shown");
        k.status = VoiceStatus::Connected;
        assert_eq!(k.problems(), ["No microphone", "No speaker"]);
        (k.mic, k.speaker) = (true, true);
        assert!(k.problems().is_empty());
        assert_eq!(test_conn(true).problems(), ["Connecting…"]);
    }

    #[test]
    fn speaking_from_levels_with_op5_fallback() {
        let mut k = test_conn(true);
        k.op5 = HashSet::from([1, 2, 3]);
        k.heard = vec![(1, false), (2, true), (4, true)];
        let mut s: Vec<u64> = k.speaking().into_iter().map(|i| i.0).collect();
        s.sort();
        assert_eq!(s, [2, 3, 4], "1: heard silent wins over op 5; 3: not heard yet, op 5 counts");
    }

    #[test]
    fn device_loss_retry() {
        // the mic's stream errors: "No microphone" (the call goes on), then retried every 5 s
        assert_eq!(REOPEN_EVERY, Duration::from_secs(5));
        let mut slot = Slot::<bool>::new();
        let (t0, opens) = (std::time::Instant::now(), std::cell::Cell::new(0));
        let open = |ok: bool| {
            opens.set(opens.get() + 1);
            ok.then_some(false)
        };
        assert_eq!(slot.keep(t0, REOPEN_EVERY, |f| *f, || open(true)), Some(true));
        slot.dev = Some(true); // stream error callback fired
        assert_eq!(slot.keep(t0, REOPEN_EVERY, |f| *f, || open(true)), Some(false), "dropped, not reopened at once");
        assert_eq!(slot.keep(t0 + Duration::from_secs(4), REOPEN_EVERY, |f| *f, || open(false)), None, "waits");
        assert_eq!(opens.get(), 1);
        assert_eq!(slot.keep(t0 + REOPEN_EVERY, REOPEN_EVERY, |f| *f, || open(false)), None, "still missing");
        assert_eq!(slot.keep(t0 + Duration::from_secs(9), REOPEN_EVERY, |f| *f, || open(true)), None);
        assert_eq!(slot.keep(t0 + REOPEN_EVERY * 2, REOPEN_EVERY, |f| *f, || open(true)), Some(true), "back");
        assert_eq!(opens.get(), 3);
        let mut k = test_conn(true);
        (k.status, k.mic) = (VoiceStatus::Connected, false);
        assert_eq!(k.problems(), ["No microphone"]);
    }

    #[test]
    fn settings_reach_devices_and_survive_reopen() {
        let (log, failed, threshold) = (Arc::new(std::sync::Mutex::new(vec![])), Arc::new(AtomicBool::new(false)), Arc::new(std::sync::Mutex::new(0.0)));
        let (l, f) = (log.clone(), failed.clone());
        let open_out = move |c: &AudioCfg| {
            l.lock().unwrap().push(format!("open {:?}", c.output));
            f.store(false, Ordering::Relaxed);
            Some(Fake { failed: f.clone(), log: l.clone(), ..Default::default() })
        };
        let (l, t) = (log.clone(), threshold.clone());
        let open_mic = move |c: &AudioCfg| {
            l.lock().unwrap().push(format!("mic {:?} {}", c.input, c.noise_suppression));
            let (failed, muted) = Default::default();
            Some(FakeMic { failed, muted, out: vec![CaptureOut::Speaking(true)], threshold: t.clone() })
        };
        let (media, mut notes, mut cmds, _muted, thread) = spawn_audio(open_out, open_mic);
        let mut next = || loop {
            match notes.blocking_recv() {
                Some(AudioNote::Levels(_)) => continue,
                n => break n,
            }
        };
        assert_eq!(next(), Some(AudioNote::Speaker(true)));
        assert_eq!(next(), Some(AudioNote::Mic(true)));
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(true))));
        media.send(ToAudio::Gain(5, 0.0)).unwrap();
        media.send(ToAudio::Deafen(true)).unwrap();
        media.send(ToAudio::Threshold(0.3)).unwrap();
        media.send(ToAudio::Input(Some("usb".into()), false)).unwrap();
        for i in 0..5 {
            assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Opus(f)) if f == SILENCE), "reopening mid-speech: silence {i}");
        }
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(false))));
        assert!(matches!(cmds.blocking_recv(), Some(VoiceCmd::Speaking(true))), "the reopened mic");
        assert_eq!(*threshold.lock().unwrap(), 0.3, "threshold applied to the reopened mic");
        failed.store(true, Ordering::Relaxed); // speaker unplugged
        assert_eq!(next(), Some(AudioNote::Speaker(false)));
        assert_eq!(next(), Some(AudioNote::Speaker(true)));
        media.send(ToAudio::Ring(true)).unwrap();
        media.send(ToAudio::Output(Some("hdmi".into()))).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !log.lock().unwrap().iter().any(|l| l == "open Some(\"hdmi\")") {
            assert!(std::time::Instant::now() < deadline, "output reopened on the chosen device");
            std::thread::yield_now();
        }
        drop(media);
        thread.join().unwrap();
        let log = log.lock().unwrap().join(", ");
        assert_eq!(
            log,
            "open None, deafened false, mic None false, gain 5 0, deafened true, mic Some(\"usb\") false, \
             open None, deafened true, gain 5 0, ringing, open Some(\"hdmi\"), deafened true, ringing, gain 5 0",
            "every reopened speaker gets deafen, the ring and gains"
        );
    }
}
