# rxcord — Stage 1 (Core Text) Design

Date: 2026-11-07
Status: Draft, awaiting review

## Goal

rxcord is a native Discord client for Linux and Windows. Its defining goal is
**near-zero CPU/GPU cost while a game is running**, so talking to friends never
costs FPS. It is written in Rust with a Slint UI.

The full project is split into three stages, each with its own spec and plan:

1. **Core text** (this spec): login, guilds, channels, DMs, text chat, notifications.
2. **Voice**: join/talk, push-to-talk, noise suppression, per-user volume,
   DAVE end-to-end encryption (mandatory since March 2026; candidate crate: `davey`).
3. **Video and screen share**.

## Constraints and risks

- Third-party clients violate Discord's Terms of Service. Accounts can be banned.
  Mitigation: mimic the official desktop client's headers, identify payload and
  request patterns; never automate actions (no self-botting); respect rate limits.
- Discord's user API is undocumented and changes without notice. Parsing must be
  lenient and unknown data must never crash the app.
- Platforms: Linux and Windows. No macOS-specific work.

## Success criteria

- Idle (window open, no incoming events): average CPU < 0.5% over 60 s, no GPU
  redraws.
- All stage 1 features below work against a real account.
- A disconnect, rate limit, or unknown event never crashes the app or loses a
  typed message.

## Architecture

Cargo workspace:

```
rxcord/
├─ Cargo.toml          (workspace)
├─ crates/
│  ├─ api/      REST client + gateway websocket
│  ├─ store/    in-memory state built from gateway events
│  ├─ app/      Slint UI + glue (binary `rxcord`)
│  └─ voice/    reserved for stage 2 (not created in stage 1)
```

### `api`

- REST via `reqwest` (rustls), gateway via `tokio-tungstenite` with
  `zlib-stream` transport compression (`flate2`).
- Sends official-desktop-client-like headers and `X-Super-Properties`, and an
  identify payload with client capabilities matching the desktop client.
- Own serde models, not `twilight`: user-account `READY` and events differ from
  bot payloads. Every model uses `#[serde(default)]`/`Option` for non-essential
  fields; unknown event names deserialize to `Event::Unknown(name)`.
- Per-route rate limiter that honours `X-RateLimit-*` headers and global 429s.
- Exposes: `Gateway` (connect, resume, heartbeat, yields typed `Event`s) and
  `Rest` (typed calls for the actions in scope).

### `store`

- Pure synchronous Rust, no I/O, no async. Input: `Event`. Output: updated state
  plus a `Change` value describing what changed (e.g. `MessagesChanged(channel_id)`,
  `GuildListChanged`, `UnreadChanged(channel_id)`).
- Holds guilds, channels, DM channels, users, read states, and messages for
  opened channels only (cap ~200 per channel; oldest evicted).

### `app`

- Tokio runtime on a background thread owns `api` and `store`.
- `store` changes are forwarded to the UI thread with
  `slint::invoke_from_event_loop`; the UI only updates the Slint models
  affected by the `Change` (e.g. only the visible channel's message list).
- UI actions call `Rest` from the runtime; results come back as gateway events
  through the same path (single source of truth). Optimistic "pending" display
  is used only for sent messages.
- Slint renderer: default to the software renderer (keeps the GPU free for the
  game), configurable to the GPU renderer.
- Message list uses Slint `ListView` (virtualized).
- Images: fetched lazily when a row becomes visible, decoded at thumbnail size
  (`image` crate), held in a byte-capped LRU cache (default 64 MB).
- Notifications via `notify-rust` (Linux D-Bus, Windows toast).

### Data flow

```
Discord gateway ──► api (JSON → Event)
                     │ tokio mpsc
                     ▼
                   store (apply → Change)
                     │ slint::invoke_from_event_loop
                     ▼
                   Slint models (affected ones only)

UI action ──► api::Rest ──► Discord ──► gateway event ──► (same path)
```

## Authentication

1. **QR code login** via Discord's remote-auth gateway (scanned with the mobile app).
2. **Token paste** fallback.

Email/password login is out of scope (requires hCaptcha). The token is stored in
the OS credential store via the `keyring` crate (Secret Service on Linux,
Credential Manager on Windows); never written to a plain file or logged.

## Stage 1 feature scope

In scope:

- QR/token login, auto-reconnect with resume.
- Guild list; channel list (text channels; voice channels shown disabled);
  DMs and group DMs.
- Message history with scrollback; basic markdown (bold, italic, inline/block
  code, links, user/channel/role mentions).
- Send, reply, edit own, delete own messages.
- Image attachments as thumbnails; other attachments as a link.
- Unread markers and mention badges.
- Native notifications for mentions and DMs.

Out of scope (add only if missed): threads, forums, reactions, stickers, full
custom-emoji rendering, rich embeds beyond images, user profiles/popouts, guild
settings, typing indicators, presence/status.

## Error handling

| Situation | Behaviour |
|---|---|
| Gateway disconnect | Exponential backoff with jitter (1 s → 60 s cap). Resume if session valid, else re-identify. "Reconnecting…" banner. |
| Gateway close 4004 / REST 401 | Delete stored token, return to login screen. |
| REST 429 | Wait per headers (route or global), then retry. Never burst. |
| Unknown event / field | Log at debug level, ignore. |
| Malformed known event | Log at warn level, drop that event, keep connection. |
| Send fails | Message shown in failed state with a retry action; text is never dropped. |
| Image fetch fails | Show placeholder; no retry loop. |

## Testing

- `store`: unit tests replaying recorded, scrubbed gateway JSON fixtures
  (`crates/store/tests/fixtures/*.json`) and asserting resulting state and
  `Change` values. Primary test surface.
- `api`: deserialization tests against the same fixtures (including unknown
  events and missing optional fields); rate-limiter timing test with a mocked clock.
- UI: manual testing.
- Performance check: `scripts/idle-check` (Linux first) samples the running
  app's CPU for 60 s idle and fails if the average is ≥ 0.5%. Game-FPS
  comparison against the official client is a manual benchmark, recorded in
  `docs/benchmarks.md`.

## Open questions for later stages

- Does Songbird support DAVE, and can its bot-oriented driver be reused for a
  user client? Otherwise: own voice stack on `davey` + `opus` + `cpal`.
- Video/screen share capture and encode path on Linux (PipeWire) and Windows.
