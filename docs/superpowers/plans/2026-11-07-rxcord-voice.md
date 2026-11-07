# rxcord Stage 2 (Voice) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Server voice channels and DM/group calls in rxcord with voice activity, mute/deafen, per-person volume and noise suppression, ~1–3% of one core in a call and zero cost outside one.

**Architecture:** New `crates/voice`: pure, sans-IO voice-gateway session + DAVE wrapper + RTP/transport crypto, a thin tokio driver (WS + UDP), and an audio engine (cpal callbacks ↔ lock-free ring buffers ↔ one voice thread doing RNNoise/Opus/mixing). `api` gains outgoing gateway commands (op 4) and voice/call events; `store` tracks voice states and calls; `app` shows them in the tree and drives joins.

**Tech Stack:** Rust 2024, tokio, tokio-tungstenite, `davey` 0.2, `opus` (libopus, bundled build), `cpal`, `nnnoiseless`, `rtrb`, RustCrypto `aes-gcm` + `chacha20poly1305`, Slint 1.18.

**Spec:** `docs/superpowers/specs/2026-11-07-rxcord-voice-design.md`

## Global Constraints

- Never log tokens, voice tokens, secret keys or MLS secrets; `Debug` impls redact them (as `Action::Send` does in the main gateway).
- Never send or play unencrypted media when the session's `dave_protocol_version` > 0: DAVE failures drop frames, never fall back.
- Agents never contact Discord. Anything that needs a live server is a manual check in `docs/stage1-checks.md` (new "Voice" section). Values from the protocol that can't be confirmed offline carry `// VERIFY LIVE`.
- No audio device is opened and no voice thread runs unless connected to voice (idle CPU unchanged: `scripts/idle-check` < 0.5%).
- cpal callbacks: no locks, no allocation, no blocking — only `rtrb` push/pop.
- Audio format inside rxcord: 48 000 Hz, f32; capture mono, playback stereo; Opus frames 20 ms (960 samples/channel), send stereo-compatible mono Opus (`Channels::Mono` encoder is fine; Discord decodes).
- RNNoise frame: 480 samples at 48 kHz, samples scaled to i16 range (×32768) before `process_frame`, back after.
- Voice activity: send while RNNoise speech probability ≥ sensitivity (default 0.6, slider 0.1–0.95) plus 200 ms hangover; on stop send 5 Opus silence frames `[0xF8, 0xFF, 0xFE]`.
- Jitter buffer target 60 ms (3 frames), max 200 ms; Opus PLC (decode with `None`) for up to 2 missing frames, silence after.
- Per-user volume 0–200 % (`0.0..=2.0`), "mute for me"; persisted in `settings.json` keyed by user id string.
- Encryption mode preference: `aead_aes256_gcm_rtpsize`, else `aead_xchacha20_poly1305_rtpsize`; anything else → error, leave.
- Voice gateway URL: `wss://{endpoint}/?v=8`. Identify sends `max_dave_protocol_version: davey::DAVE_PROTOCOL_VERSION`.
- Existing UI conventions: tree rows, status bar, `bridge::with_ui`, right-click `ContextMenuArea` on top of content, settings via `settings.rs`.
- Every task ends with `cargo test --workspace` and `cargo clippy --workspace --all-targets` clean.

## Protocol reference (from Discord docs + DAVE whitepaper, 2026-11-07)

Voice opcodes (JSON unless noted): 0 Identify, 1 Select Protocol, 2 Ready, 3 Heartbeat `{t, seq_ack}`, 4 Session Description `{mode, secret_key[32], dave_protocol_version}`, 5 Speaking `{speaking, delay: 0, ssrc}` (server sends `{user_id, ssrc, speaking}`), 6 Heartbeat ACK, 7 Resume `{server_id, session_id, token, seq_ack}`, 8 Hello `{heartbeat_interval}`, 9 Resumed, 11 Clients Connect `{user_ids}` // VERIFY LIVE, 13 Client Disconnect `{user_id}` // VERIFY LIVE, 21 DAVE Prepare Transition `{transition_id, protocol_version}`, 22 Execute Transition `{transition_id}`, 23 Transition Ready `{transition_id}` (client), 24 Prepare Epoch `{epoch, protocol_version}`, 25 External Sender (binary), 26 Key Package (binary, client), 27 Proposals (binary: op byte + operation type u8 + data), 28 Commit Welcome (binary, client), 29 Announce Commit Transition (binary: transition_id u16 + commit), 30 Welcome (binary: transition_id u16 + welcome), 31 Invalid Commit Welcome `{transition_id}` (client, JSON).
Server JSON frames carry top-level `seq`; server binary frames start with `seq: u16 BE`, then `op: u8`. Client binary frames: `op: u8` + payload.
Close codes: 4006 session invalid (re-identify via main gateway), 4014 disconnected (kicked/moved/channel deleted: leave), 4015 server crashed (resume), 4016 unknown encryption mode, 4017 DAVE required, others: reconnect with backoff.
IP discovery: request `type=1 u16, len=70 u16, ssrc u32, 64 zero bytes, port u16` (74 bytes, BE); response type 2 with NUL-terminated address and port.
RTP: `0x80 0x78 seq:u16 ts:u32 ssrc:u32` (+ CSRCs, + extension if X bit). rtpsize AEAD: AAD = header incl. 4-byte extension preamble; ciphertext = extension body + payload; then 16-byte tag; then 4-byte BE nonce counter. Nonce = counter BE in the first 4 bytes, rest zero (12 bytes GCM, 24 XChaCha).
Main gateway op 4 Voice State Update: `{guild_id: id|null, channel_id: id|null, self_mute, self_deaf, self_video: false}`. Events: `VOICE_STATE_UPDATE`, `VOICE_SERVER_UPDATE {token, guild_id|null, channel_id?, endpoint|null}`, `CALL_CREATE {channel_id, message_id, ringing: [user ids], voice_states}`, `CALL_UPDATE`, `CALL_DELETE {channel_id}` // VERIFY LIVE (call fields). DM ring: `POST /channels/{id}/call/ring {recipients: null}`, stop: `POST /channels/{id}/call/stop-ringing {recipients: [me]}` // VERIFY LIVE. Voice Identify `server_id` = guild id, or the DM channel id for calls.

## Review Focus

- A voice packet from an SSRC whose user isn't known yet (Speaking not received): kept for ≤ 1 s, then dropped; never decrypted with the wrong user's key.
- Leaving/hanging up while the voice WS is mid-handshake or reconnecting: no zombie tasks, devices closed, op 4 `channel_id: null` sent once.
- Device unplugged mid-call (cpal stream error callback): stay connected, show "No microphone/speaker", reopen default device on the next attempt (every 5 s while in voice).
- DAVE transition races: media arriving for an epoch not executed yet is decrypted with `can_passthrough`/old key per davey; frames that fail to decrypt are dropped (counted, logged at debug), never played.
- Main gateway reconnect (READY replays voice states) while in voice: the voice connection survives; the tree re-renders from the new states without duplicates.

Each line has a pinned test in the task that owns it.

---

### Task 1: `crates/voice` skeleton, RTP and transport crypto

**Files:** Create `crates/voice/Cargo.toml`, `crates/voice/src/lib.rs`, `crates/voice/src/rtp.rs`. Modify root `Cargo.toml` (members, workspace deps `aes-gcm = "0.10"`, `chacha20poly1305 = "0.10"`).

**Interfaces — Produces:**
- `pub struct RtpHeader { pub seq: u16, pub timestamp: u32, pub ssrc: u32 }`
- `pub enum Mode { Aes256Gcm, XChaCha20 }` with `pub fn from_name(&str) -> Option<Mode>`, `pub fn name(self) -> &'static str`, `pub fn choose(offered: &[String]) -> Option<Mode>` (preference per Global Constraints).
- `pub struct Transport { mode: Mode, key: [u8; 32], counter: u32 }` (Debug redacts key): `new(mode, key, counter: u32)`, `encrypt(&mut self, h: &RtpHeader, payload: &[u8]) -> Vec<u8>` (full UDP packet), `decrypt(&self, packet: &[u8]) -> Option<(RtpHeader, Vec<u8>)>` (returns the payload with the decrypted extension body stripped).
- `pub fn discovery_request(ssrc: u32) -> [u8; 74]`, `pub fn discovery_response(buf: &[u8]) -> Option<(String, u16)>`.

- [ ] **Step 1: Failing tests** in `rtp.rs`: `aes_round_trip`, `xchacha_round_trip` (encrypt then decrypt returns header + payload; counter increments; last 4 bytes = counter BE); `decrypt_rejects_tamper` (flip one payload byte → None); `extension_body_is_encrypted` (hand-build a packet with X bit, CSRC count 0, preamble `0xBE 0xDE 0x00 0x01` + 4-byte body, encrypt with AAD = header+preamble: decrypt strips body, returns payload); `rejects_short_and_non_rtp` (len < 12+16+4, version ≠ 2 → None); `discovery_layout` (request bytes 0..2 = `00 01`, 2..4 = `00 46`, ssrc BE at 4..8, len 74); `discovery_parse` (response with "1.2.3.4\0" and port 50000 → ("1.2.3.4", 50000)); `mode_choice` (offered both → Aes256Gcm; only xchacha → XChaCha20; neither → None).
- [ ] **Step 2:** `cargo test -p rxcord-voice` → fails (no crate).
- [ ] **Step 3:** Implement. Header length = 12 + 4·CSRC count; if X bit, AAD includes the 4-byte preamble, body length = preamble length field × 4 is inside the ciphertext.
- [ ] **Step 4:** Tests pass; clippy clean.
- [ ] **Step 5:** Commit `voice: rtp packets, transport encryption, ip discovery`.

### Task 2: Voice gateway session (sans-IO)

**Files:** Create `crates/voice/src/session.rs`.

**Interfaces — Consumes:** `Mode::choose`, `discovery_request`. **Produces:**
- `pub struct Connect { pub server_id: u64, pub user_id: u64, pub session_id: String, pub token: String, pub endpoint: String }` (Debug redacts token).
- `pub enum Frame { Text(String), Binary(Vec<u8>) }`
- `pub enum Action { Send(Frame) /*Debug redacted*/, HeartbeatEvery(Duration), Discover { ip: String, port: u16, ssrc: u32 }, Ready { mode: Mode, key: [u8; 32], dave_version: u16, ssrc: u32 }, Speaking { user_id: u64, ssrc: u32 }, UserLeft(u64), UsersJoined(Vec<u64>), Dave(DaveIn), Resumed, Reconnect { resume: bool }, Leave(LeaveReason) }`
- `pub enum DaveIn { PrepareTransition { id: u16, version: u16 }, ExecuteTransition(u16), PrepareEpoch { epoch: u64, version: u16 }, ExternalSender(Vec<u8>), Proposals { op: u8, data: Vec<u8> }, AnnounceCommit { id: u16, commit: Vec<u8> }, Welcome { id: u16, welcome: Vec<u8> } }`
- `pub enum LeaveReason { Kicked, BadEncryption, Fatal(u16) }`
- `impl Session`: `new(c: Connect, max_dave: u16) -> Self`, `on_open(&mut self) -> Vec<Action>` (identify, or resume if a session ran before), `on_text(&mut self, &str) -> Vec<Action>`, `on_binary(&mut self, &[u8]) -> Vec<Action>`, `on_discovered(&mut self, ip: &str, port: u16) -> Vec<Action>` (select protocol), `heartbeat(&mut self, now_ms: u64) -> Action`, `speaking(&self, on: bool) -> Action`, `dave_send(&self, out: DaveOut) -> Action`, `on_close(&mut self, code: Option<u16>) -> Action`.
- `pub enum DaveOut { KeyPackage(Vec<u8>), CommitWelcome(Vec<u8>), TransitionReady(u16), InvalidCommitWelcome(u16) }`

- [ ] **Step 1: Failing tests:** `handshake_order` (open → Identify with `max_dave_protocol_version`; hello → HeartbeatEvery; ready → Discover with modes chosen; discovered → Select Protocol JSON with chosen mode name; session description → Ready with key and dave version); `heartbeat_carries_seq_ack` (after a server frame with `seq: 7`, heartbeat JSON has `seq_ack: 7`; before any, `-1`); `binary_dave_frames` (bytes `00 05 19 …` → seq 5 recorded, `DaveIn::ExternalSender`; `00 06 1b 00 …` → Proposals op 0; `00 07 1d 00 03 …` → AnnounceCommit id 3; `00 08 1e 00 04 …` → Welcome id 4); `json_dave_frames` (op 21/22/24 parsed); `client_binary_layout` (`dave_send(KeyPackage([1,2]))` → `Binary([26,1,2])`, CommitWelcome → op 28, TransitionReady → JSON op 23, Invalid → JSON op 31); `speaking_events` (server op 5 → Speaking, op 13 → UserLeft, op 11 → UsersJoined); `close_codes` (4014 → Leave(Kicked), 4016 → Leave(BadEncryption), 4015 → Reconnect{resume:true}, 4006 → Reconnect{resume:false}, 4017 → Leave(Fatal(4017)), None → Reconnect{resume:true}); `resume_after_reconnect` (second `on_open` after Ready sends op 7 with seq_ack); `debug_redacts` (`format!("{:?}", Action::Send(identify))` has no token).
- [ ] **Step 2–4:** red, implement, green.
- [ ] **Step 5:** Commit `voice: gateway session state machine`.

### Task 3: DAVE wrapper

**Files:** Create `crates/voice/src/dave.rs`. Add dep `davey = "0.2"`.

**Interfaces — Consumes:** `DaveIn`, `DaveOut`. **Produces:**
- `pub struct Dave { session: Option<davey::DaveSession>, user_id: u64, channel_id: u64, recognized: Vec<u64> }`
- `pub fn new(user_id: u64, channel_id: u64) -> Self`; `pub fn start(&mut self, version: u16) -> Vec<DaveOut>` (version 0: no session; else create session, emit KeyPackage); `pub fn handle(&mut self, ev: DaveIn) -> Vec<DaveOut>`; `pub fn set_members(&mut self, ids: Vec<u64>)`; `pub fn encrypt(&mut self, opus: &[u8]) -> Option<Vec<u8>>` (None = not ready → drop frame; passthrough copy when version 0); `pub fn decrypt(&mut self, user_id: u64, frame: &[u8]) -> Option<Vec<u8>>`; `pub fn privacy_code(&self) -> Option<String>`.
- Rules: ExternalSender → `set_external_sender`; Proposals → `process_proposals(op, data, Some(members))` → CommitWelcome if Some; AnnounceCommit → `process_commit`, ok → `prepare_transition(id, version)` + TransitionReady(id), err → InvalidCommitWelcome(id) + `start(version)` (re-key); Welcome → `process_welcome`, ok → prepare + TransitionReady, err → InvalidCommitWelcome + re-key; PrepareTransition{id: 0} → execute immediately; else store and TransitionReady(id); ExecuteTransition(id) → `execute_transition(id)`; PrepareEpoch{epoch: 1} → `reinit` + KeyPackage.

- [ ] **Step 1: Failing tests:** `version_zero_passthrough` (start(0) → no outs; encrypt returns input unchanged; decrypt returns input); `start_emits_key_package` (start(1) → one `KeyPackage` with non-empty bytes); `not_ready_drops` (start(1), encrypt → None — Review Focus: never unencrypted when DAVE on); `bad_commit_rekeys` (AnnounceCommit with garbage → `[InvalidCommitWelcome(id), KeyPackage(_)]`); `transition_zero_executes` (PrepareTransition{id:0} → no TransitionReady emitted); two-member round trip **if** davey 0.2 exposes a way to build an external sender in tests (check its own tests); otherwise record the ruling in the ledger and cover it in the manual checklist.
- [ ] **Step 2–4.** **Step 5:** Commit `voice: DAVE session wrapper`.

### Task 4: Voice driver (WS + UDP)

**Files:** Create `crates/voice/src/driver.rs`.

**Interfaces — Consumes:** Session, Dave, Transport, discovery fns. **Produces:**
- `pub enum VoiceEvent { Status(VoiceStatus), Speaking { user_id: u64, ssrc: u32, on: bool }, Media { user_id: u64, seq: u16, ts: u32, opus: Vec<u8> }, Left(LeaveReason), PrivacyCode(String) }`
- `pub enum VoiceStatus { Connecting, Connected, Reconnecting { attempt: u32 } }`
- `pub enum VoiceCmd { Opus(Vec<u8>) /*20 ms frame*/, Speaking(bool), Leave }`
- `pub async fn run_voice(c: Connect, user_id: u64, events: mpsc::Sender<VoiceEvent>, cmds: mpsc::Receiver<VoiceCmd>)`; internal `run_voice_at(..., url_override: Option<String>)` for tests.
- Behaviour: WS loop as main gateway driver (backoff from `rxcord_api::backoff`); UDP bind `0.0.0.0:0`, discovery with 2 s timeout ×3; RTP send: seq/ts (+960) per frame, Dave encrypt then Transport encrypt; receive: Transport decrypt → SSRC→user map (from Speaking) → Dave decrypt → `Media`. Unknown SSRC packets buffered ≤ 1 s (≤ 50 packets) then dropped. Gives up after 30 s of failed reconnects → `Left(Fatal(0))`.

- [ ] **Step 1: Failing tests** with a fake local server (tokio WS on 127.0.0.1 + UDP socket) speaking DAVE version 0: `connects_and_discovers` (driver sends Identify, gets Hello/Ready(port of fake UDP)/answers discovery, sends Select Protocol, gets Session Description → `Status(Connected)`); `sends_encrypted_frames` (VoiceCmd::Opus → fake UDP receives a packet the test decrypts with the key); `media_from_known_ssrc` (fake sends Speaking{user 9, ssrc 77} then an RTP packet → `Media{user_id:9}`); `unknown_ssrc_buffered_then_dropped` (packet before Speaking within 1 s → delivered after Speaking; after > 1 s → dropped; Review Focus); `leave_mid_handshake` (Leave before Ready → task ends, no panic; Review Focus).
- [ ] **Step 2–4.** **Step 5:** Commit `voice: websocket + udp driver`.

### Task 5: Main gateway commands and voice/call events (`api`)

**Files:** Modify `crates/api/src/gateway/driver.rs`, `gateway/session.rs`, `event.rs`, `model.rs`, `rest.rs`; tests in `crates/api/tests/parse.rs`, `rest.rs`.

**Interfaces — Produces:**
- `run_gateway(token, tx, cmds: mpsc::Receiver<GatewayCmd>)`; `pub enum GatewayCmd { VoiceState { guild_id: Option<Id>, channel_id: Option<Id>, self_mute: bool, self_deaf: bool } }` → op 4 JSON (sent only while connected; latest one re-sent after a re-identify if `channel_id` is Some).
- Model: `VoiceState { user_id: Id, channel_id: Option<Id>, guild_id: Option<Id>, session_id: String, self_mute: bool, self_deaf: bool, mute: bool, deaf: bool }` (lenient); `Guild` gains `voice_states: Vec<VoiceState>` (null_as_default; READY/GUILD_CREATE); `VoiceServer { token: String, guild_id: Option<Id>, channel_id: Option<Id>, endpoint: Option<String> }` (Debug redacts token); `Call { channel_id: Id, ringing: Vec<Id>, voice_states: Vec<VoiceState> }`.
- Events: `VoiceStateUpdate(VoiceState)`, `VoiceServerUpdate(VoiceServer)`, `CallCreate(Call)`, `CallUpdate(Call)`, `CallDelete { channel_id: Id }`.
- REST: `ring(channel) -> Result<(), ApiError>` (POST `/channels/{id}/call/ring` `{"recipients": null}`), `stop_ringing(channel, me) ` (POST `/channels/{id}/call/stop-ringing` `{"recipients": [me]}`), both `// VERIFY LIVE`.

- [ ] **Step 1: Failing tests:** parse each new event from JSON fixtures (incl. `endpoint: null`, missing `voice_states`); `voice_server_debug_redacts`; op 4 JSON shape (`self_video: false`, nulls); gateway session re-sends the last VoiceState after re-identify; `ring_bodies` (wiremock).
- [ ] **Step 2–4.** Update all `run_gateway` callers (runtime.rs, capture example). **Step 5:** Commit `api: voice state command, voice and call events, ring`.

### Task 6: Store — voice states and calls

**Files:** Modify `crates/store/src/state.rs`, `crates/store/tests/apply.rs`.

**Interfaces — Produces:** `Change::Voice(Id /*channel*/)`, `Change::Call(Id)`; `Store::voice_members(channel) -> Vec<&VoiceState>` (ordered by user id), `Store::my_voice(&self) -> Option<&VoiceState>`, `Store::call(channel) -> Option<&Call>`, `Store::ringing_me(&self) -> Vec<Id>` (DM channels whose call rings me).
- Rules: READY → voice states from each guild + calls none; GUILD_CREATE replaces that guild's states; VOICE_STATE_UPDATE moves a user (remove from old channel, add to new; `channel_id: null` removes) and emits `Voice` for both channels; GUILD_DELETE drops its states; CALL_* maintain calls; READY replaces all (no duplicates — Review Focus).

- [ ] **Step 1: Failing tests:** `voice_join_move_leave`, `ready_replaces_voice_states_without_duplicates`, `guild_delete_drops_voice`, `call_ringing_me`, `my_voice_tracks_self`.
- [ ] **Step 2–4.** **Step 5:** Commit `store: voice states and calls`.

### Task 7: Audio engine — devices, playback, mixer, jitter buffer

**Files:** Create `crates/voice/src/audio/{mod,devices,playback,jitter,mixer}.rs`. Deps: `cpal`, `opus` (bundled libopus; verify it builds on Linux and in the Windows CI job), `rtrb`.

**Interfaces — Produces:**
- `jitter::Jitter`: `push(seq: u16, opus: Vec<u8>)`, `pop(&mut self) -> Popped` where `enum Popped { Frame(Vec<u8>), Lost, Empty }`; target 3 frames, max 10 (older dropped), wraps u16 seq.
- `mixer::mix(out: &mut [f32], sources: &[(&[f32], f32 /*gain*/)])` stereo interleaved, clamps to [-1, 1].
- `playback::Playback`: `start(device: Option<String>) -> Result<Playback, AudioError>` (opens cpal output at 48 kHz stereo f32 if supported, else device default rate with linear resampling), `push(user_id, seq, opus)`, `set_gain(user_id, f32)`, `set_deafened(bool)`, `tick(&mut self)` (called every 10 ms from the voice thread: decode/PLC, mix, write to ring), drop = stream closed. Speaking level per user exposed for UI: `levels() -> Vec<(u64, bool)>`.
- `devices::{inputs, outputs}() -> Vec<String>`.

- [ ] **Step 1: Failing tests:** `jitter_reorders`, `jitter_reports_loss_once_then_plc_then_empty`, `jitter_drops_too_old`, `jitter_seq_wrap`; `mixer_gain_and_clamp`, `mixer_mute_for_me` (gain 0); `resample_48k_to_44k_length` (linear resampler output length ±1); `decode_round_trip` (opus encode a 440 Hz tone, decode, RMS within 20 %).
- [ ] **Step 2–4.** **Step 5:** Commit `voice: playback, jitter buffer, mixer`.

### Task 8: Join and listen in server voice channels (app)

**Files:** Modify `crates/app/src/{runtime,view,bridge,nav,main}.rs`, `crates/app/ui/{tree,app}.slint`; create `crates/app/src/voice.rs` (runtime-side voice controller).

**Interfaces — Produces:**
- `view::TreeRow` gains voice-member rows: `VoiceMember { user_id, name, speaking, muted, deafened }` listed under voice channels (depth+1), for every guild; voice channels become enabled (double-click joins).
- `Command::JoinVoice(Id)`, `Command::LeaveVoice`; `voice::Controller` holds the pending join (waits for both our VOICE_STATE_UPDATE session_id and VOICE_SERVER_UPDATE, 10 s timeout → error notice), starts `run_voice` + `Playback`, routes `VoiceEvent::Media` into playback, `Speaking` into tree highlights, leaves on `Left`/hang-up (op 4 null, drop driver and devices).
- Status bar voice mode: `🔊 Guild / Channel` + ✖ (mute/deafen come in Task 10), "Reconnecting voice…".

- [ ] **Step 1: Failing tests:** `tree_lists_voice_members` (view), `double_join_moves` (controller: join A then B → one op 4 per join, old driver stopped), `join_timeout` (no server update in 10 s → notice, state cleared), `hangup_mid_handshake_sends_null_once` (Review Focus).
- [ ] **Step 2–4.** Demo mode: a fake voice channel with two members (no audio). Screenshot check of the tree. **Step 5:** Commit `app: join voice channels and listen`.

### Task 9: Talk — capture, noise suppression, voice activity, send

**Files:** Create `crates/voice/src/audio/{capture,vad}.rs`. Dep: `nnnoiseless`.

**Interfaces — Produces:**
- `vad::Gate { threshold: f32, hangover_frames: u32 }`: `update(&mut self, speech_prob: f32) -> GateOut` where `enum GateOut { Send, Stop /*emit 5 silence frames*/, Idle }`.
- `capture::Capture`: `start(device, noise_suppression: bool) -> Result<Capture, AudioError>`, `tick(&mut self) -> Vec<CaptureOut>` (`Opus(Vec<u8>)`, `Speaking(bool)`, `Level(bool)`), `set_threshold(f32)`, `set_muted(bool)` (muted: no packets, Speaking(false)).
- Chunking: 480-sample RNNoise frames → 960-sample Opus frames (two RNNoise frames per Opus frame; use the max speech probability of the two).

- [ ] **Step 1: Failing tests:** `gate_hangover` (prob 0.9 ×1 then 0.0: Send for 10 more 20 ms frames (200 ms) then one Stop then Idle), `gate_threshold`, `chunker_480_to_960`, `muted_sends_nothing`, `silence_frames_on_stop` (Stop → exactly 5 `[0xF8,0xFF,0xFE]`), `noise_suppression_off_still_vad` (when off, VAD still uses RNNoise probability but outputs the raw samples).
- [ ] **Step 2–4.** Wire into the controller: Capture ticks on the voice thread, `VoiceCmd::Opus/Speaking`. **Step 5:** Commit `voice: capture, noise suppression, voice activity`.

### Task 10: Controls — mute, deafen, per-person volume, voice settings

**Files:** Modify `crates/app/src/{settings,voice,runtime,bridge,main}.rs`, `crates/app/ui/{app,tree}.slint`; create `crates/app/ui/voice_settings.slint`.

**Interfaces — Produces:**
- Settings: `input_device: Option<String>`, `output_device: Option<String>`, `vad_threshold: f32` (default 0.6), `noise_suppression: bool` (true), `user_volume: HashMap<String, f32>`, `user_muted: HashSet<String>` (all `#[serde(default)]`; old files still load).
- Voice bar 🎤 🎧 ✖ + level dot; mute/deafen send op 4 with flags and apply locally (deafen implies mute; undeafen restores the previous mute).
- Right-click a voice member → `Menu { Volume slider row via PopupWindow, "Mute for me" }`.
- ⚙ → "Voice settings…" popup: device ComboBoxes (`devices::inputs/outputs`), sensitivity Slider, noise suppression CheckBox; changes apply live (reopen device on change).

- [ ] **Step 1: Failing tests:** `settings_voice_defaults_and_old_file` (old settings.json without voice fields loads with defaults), `deafen_implies_mute_and_restores`, `volume_clamped_0_2`, `device_loss_retry` (controller: stream error → "No microphone" + retry every 5 s while in voice; Review Focus).
- [ ] **Step 2–4.** Screenshot check. **Step 5:** Commit `app: voice controls, per-person volume, voice settings`.

### Task 11: DM and group calls with ringing

**Files:** Modify `crates/app/src/{voice,runtime,bridge,notify,view}.rs`, `crates/app/ui/{tabs,app}.slint`; create `crates/voice/src/audio/tone.rs`.

**Interfaces — Produces:**
- `tone::Ring` — generated ring (two 440/480 Hz beeps 0.4 s on, 2 s period) written to the output device while ringing; stops on accept/decline/CALL_DELETE/timeout 45 s.
- 📞 button on a DM tab's bar → `Command::StartCall(channel)`: join voice in the DM channel (op 4 `guild_id: null`), then `rest.ring`.
- Incoming: `Store::ringing_me` non-empty → notification + ring + status bar "📞 Name is calling — Accept / Decline". Accept → (leave current voice) join; Decline → `stop_ringing`, stop tone.
- Call members listed under the DM in the tree (from `Call.voice_states`).

- [ ] **Step 1: Failing tests:** `incoming_call_prompt_once` (CALL_CREATE ringing me → one prompt; CALL_UPDATE without me → prompt cleared), `accept_while_in_voice_leaves_first`, `ring_tone_cadence` (tone sample count/period), `call_members_in_tree`.
- [ ] **Step 2–4.** **Step 5:** Commit `app: DM and group calls`.

### Task 12: Reconnects, polish, measurement, checklist

**Files:** Modify `crates/voice/src/driver.rs`, `crates/app/src/voice.rs`, `docs/stage1-checks.md`, `docs/benchmarks.md`.

- [ ] **Step 1: Failing tests:** `voice_server_change_reconnects` (new VOICE_SERVER_UPDATE while connected → driver restarted with new endpoint, playback kept), `main_gateway_ready_keeps_voice` (Review Focus: READY while in voice → no leave, tree without duplicates), `gives_up_after_30s`, `moved_by_moderator_follows` (our VOICE_STATE_UPDATE with another channel → controller follows).
- [ ] **Step 2–4.**
- [ ] **Step 5:** Measure: not in voice → `scripts/idle-check` < 0.5 %; in a call (demo loopback or live) → record CPU in `docs/benchmarks.md`.
- [ ] **Step 6:** Add the "Voice" manual checklist to `docs/stage1-checks.md` (every `// VERIFY LIVE` item, join/move/leave, DM call both ways, mute/deafen, volume, unplug, network loss, privacy code shown).
- [ ] **Step 7:** Commit `voice: reconnects, measurements, checklist`.
