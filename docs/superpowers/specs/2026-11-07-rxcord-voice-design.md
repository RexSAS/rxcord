# rxcord — Stage 2 (Voice) Design

Date: 2026-11-07
Status: Sections approved in chat; awaiting written-spec review

## Goal

Talk with friends while gaming without FPS drops: server voice channels and DM/group calls, with
voice activity, mute/deafen, per-person volume and noise suppression, at ~1–3% of one CPU core while in
a call and zero cost when not in one. No GPU use.

## Decisions (from the user)

- Server voice channels **and** DM/group calls (with ringing).
- **Voice activity only** (no push-to-talk), with mute and deafen.
- Mixed headset/speaker use, but **no echo cancellation** (user's choice); noise suppression only.
- **Own voice stack** (not songbird): `davey` (DAVE E2EE), `opus`, `cpal`, `nnnoiseless`,
  `aes-gcm` / `chacha20poly1305`.

Out of scope: push-to-talk, echo cancellation, video/screen share (stage 3), soundboard, activities.

## What the user sees

- **Tree**: double-click a ♪ voice channel to join or move (single click does nothing, so no accidental
  joins). Everyone in a voice channel is listed under it (all servers, also when you're not in it), with
  a speaking highlight and 🔇 (muted) / 🎧 (deafened) marks. People in a DM call are listed under the
  DM the same way.
- **DM/group calls**: a 📞 button in a DM tab's bar starts a call (rings the others). Incoming call:
  desktop notification, a generated ring tone (no sound files) and the status bar becomes
  "📞 Bob is calling — Accept / Decline". Accepting while in another voice channel leaves it first.
- **Voice bar** (status bar while connected): `🔊 Server / Channel   🎤 🎧 ✖` — mute, deafen (also
  mutes), hang up — plus a level dot next to 🎤 that lights while the mic is sending.
- **Per-person**: right-click a person → volume 0–200% and "Mute for me"; saved per user id in
  `settings.json`.
- **Voice settings** (⚙ menu): input and output device (system default first), voice-activity
  sensitivity, noise suppression on/off (default on).

## Architecture

New crate `crates/voice`, same split as the main gateway: pure protocol logic with a thin I/O layer.

```
crates/voice/
├── session.rs   voice gateway state machine (pure, sans-IO): identify, hello/heartbeat, ready,
│                select protocol, session description, resume, speaking, client connect/disconnect,
│                DAVE opcodes routed to dave.rs, close codes → actions
├── dave.rs      DAVE via `davey`: MLS key packages, external sender, proposals, commit/welcome,
│                transitions; per-sender frame encrypt/decrypt
├── rtp.rs       RTP header build/parse, transport AEAD (aead_aes256_gcm_rtpsize preferred,
│                aead_xchacha20_poly1305_rtpsize fallback), IP discovery packet
├── driver.rs    tokio: voice WebSocket (v8, JSON + binary DAVE frames) + UDP socket; heartbeats,
│                reconnect/resume with backoff
└── audio/
    ├── capture.rs   mic → 48 kHz mono frames → nnnoiseless (noise suppression + speech probability)
    │                → voice activity (threshold + ~200 ms hangover) → Opus → packets
    ├── playback.rs  packets → per-SSRC jitter buffer (~60 ms) → Opus decode (PLC for short gaps)
    │                → per-person volume / "mute for me" → mix → speakers
    └── devices.rs   cpal device list and open/close
```

**Threads**: cpal's real-time callbacks only move samples through lock-free ring buffers; noise
suppression, Opus and mixing run on one dedicated voice thread; WebSocket/UDP run on the existing tokio
runtime. Audio devices are opened only while connected.

**Join flow**

```
double-click channel (or accept/start a call)
  → main gateway op 4 Voice State Update {guild_id|null, channel_id, self_mute, self_deaf}
  ← VOICE_STATE_UPDATE (our session_id) + VOICE_SERVER_UPDATE (endpoint, token)
  → voice WS identify {server_id, user_id, session_id, token, max_dave_protocol_version}
  ← hello (heartbeat interval), ready (ssrc, ip, port, modes)
  ↔ UDP IP discovery → select protocol {address, port, mode}
  ← session description (secret key, dave_protocol_version)
  ↔ DAVE: external sender, key package, proposals, commit/welcome, transition
  ⇄ UDP: 20 ms Opus frames, DAVE frame-encrypted, then transport-encrypted
leave: op 4 with channel_id null; streams and devices closed
```

**Changes in existing crates**

- `api`: op 4 voice state update; events VOICE_STATE_UPDATE, VOICE_SERVER_UPDATE, CALL_CREATE,
  CALL_UPDATE, CALL_DELETE (lenient parsing as everywhere); REST ring / stop-ringing for DM calls.
- `store`: voice states per channel (from READY guild voice states and VOICE_STATE_UPDATE), active
  calls; `Change::Voice(channel)`.
- `app`: tree rows for people in voice, double-click join, voice bar, call prompt + generated ring
  tone, right-click volume, voice settings, settings.json fields (devices, sensitivity, noise
  suppression, per-user volume/mute).

Exact opcode numbers, field names and the ring endpoints are taken from Discord's voice documentation
and a capture of the official client during implementation; values that can't be confirmed offline are
marked `// VERIFY LIVE` and listed in the manual checklist.

## Error handling

| Situation | Behaviour |
|---|---|
| Voice WS/UDP drops | Backoff reconnect + resume; voice bar "Reconnecting voice…"; give up after ~30 s and leave with a notice. |
| New voice server (VOICE_SERVER_UPDATE while connected) | Reconnect transparently, rejoin the DAVE group. |
| Kicked / channel deleted (close 4014) | Leave cleanly with a notice. Moved by a moderator: follow to the new channel. |
| DAVE failure (invalid commit/welcome) | Report to the voice server and re-key per protocol; never send or play unencrypted media. |
| Mic/speaker missing or unplugged | Stay connected; voice bar shows "No microphone" (muted) / "No speaker"; retry on device change. |
| Unknown SSRC or malformed packet | Drop silently. |
| Late/lost packets | Jitter buffer (~60 ms); Opus PLC for short gaps; silence for long ones. |
| Incoming call while in voice | Ring as usual; Accept leaves the current channel first. |
| Main gateway reconnects while in voice | Voice keeps running; voice states restored from the new READY. |

## Testing

- Unit (test-first): voice session handshake order, heartbeats, resume, every close code; RTP
  build/parse; transport AEAD round trips with fixed vectors; IP discovery parse; jitter buffer
  (reorder, loss, late); mixer with volume and "mute for me"; voice activity with hangover; store voice
  states → tree rows; call events → call prompt.
- DAVE: two local `davey` sessions exchange key packages/commit/welcome and encrypt/decrypt each
  other's frames, no network.
- Performance: idle CPU unchanged when not in a call (no devices open); in a call measured with
  `scripts/idle-check`, target < 3% of one core.
- Manual (spare account + a friend/second account): join/leave/move in a server, DM call both ways,
  mute/deafen, volume, unplug headset, network loss. Added to the stage checklist.

## Build order

1. Join and listen in server voice channels (store voice states, tree, voice WS, UDP, DAVE, playback).
2. Talk: mic, noise suppression, voice activity, Opus encode, send.
3. Controls: mute/deafen, per-person volume, voice settings.
4. DM/group calls with ringing.
5. Reconnects and polish.
