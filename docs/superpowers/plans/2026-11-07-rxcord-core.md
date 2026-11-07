# rxcord Stage 1 (Core Text) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A native Rust + Slint Discord client for Linux and Windows with login, guilds, channels, DMs, text chat and notifications, idling at < 0.5% CPU.

**Architecture:** Cargo workspace with three crates: `rxcord-api` (sans-IO protocol logic plus thin tokio IO drivers for REST, gateway and QR login), `rxcord-store` (pure synchronous state: `apply(Event) -> Vec<Change>`), and `rxcord` (Slint app; tokio runtime on a background thread owns api + store and pushes `Send` view data to the UI thread via `slint::invoke_from_event_loop`).

**Tech Stack:** Rust (edition 2024, stable), tokio, tokio-tungstenite, reqwest (rustls), serde/serde_json, flate2, rsa + sha2 + base64, Slint 1.x, keyring, notify-rust, image, qrcode, lru, tracing.

**Spec:** `docs/superpowers/specs/2026-11-07-rxcord-core-design.md`

## Global Constraints

- Platforms: Linux and Windows. TLS via rustls only (no OpenSSL).
- REST base: `https://discord.com/api/v9`. Gateway: `wss://gateway.discord.gg/?encoding=json&v=9&compress=zlib-stream`. Remote auth: `wss://remote-auth-gateway.discord.gg/?v=2`.
- Client identity (headers, `X-Super-Properties`, identify `properties`, `capabilities`) lives in ONE file: `crates/api/src/client_props.rs`. Nothing else hardcodes it.
- The token is never logged, never written to a plain file. Any struct holding it implements `Debug` with the token redacted. Storage: OS keyring only (service `"rxcord"`, user `"token"`).
- `rxcord-store` has no I/O and no async.
- Parsing is lenient: unknown events → `Event::Unknown`, unknown fields ignored, non-essential fields `Option`/`#[serde(default)]`.
- Reconnect backoff: 1 s doubling to a 60 s cap, with jitter.
- Message cap: 200 per opened channel (see Task 7 for exact eviction rule).
- Image cache: 64 MB byte cap.
- Idle target: average CPU < 0.5% over 60 s, no redraws while idle.
- Default Slint renderer: software; GPU renderer opt-in via setting / `SLINT_BACKEND`.
- Out of scope (do NOT build): threads, forums, reactions, stickers, custom emoji beyond image, rich embeds, profiles, guild settings, typing indicators, presence, voice.

## Review Focus

1. **Duplicate MESSAGE_CREATE after a resume** (same message id delivered twice) → message appears once. Test: Task 7.
2. **Event for a channel/guild the store doesn't know** (e.g. thread, newly visible channel) → ignored, no panic. Test: Task 7.
3. **Malformed markdown** (unterminated ```` ``` ````, lone `**`, 4000 chars no spaces) → renders as plain text, parse is linear time. Test: Task 8.
4. **Token revoked mid-session** (REST 401 while gateway still up) → app returns to login and deletes stored token. Test: Task 5 (`ApiError::Unauthorized`), Task 9 wiring.
5. **Silent network death / laptop sleep** (no heartbeat ACK) → zombie detected, reconnect with resume. Test: Task 3.

---

### Task 1: Workspace, models and event parsing

**Files:**
- Create: `Cargo.toml` (workspace, members `crates/*`), `.gitignore` (`target/`)
- Create: `crates/api/Cargo.toml`, `crates/api/src/lib.rs`, `crates/api/src/id.rs`, `crates/api/src/model.rs`, `crates/api/src/event.rs`
- Test: `crates/api/tests/parse.rs`, fixtures in `crates/api/tests/fixtures/`

**Interfaces:**
- Produces:
  - `pub struct Id(pub u64)` — `Copy, Eq, Ord, Hash`; serde from/to JSON string; `Display`.
  - `User { id: Id, username: String, global_name: Option<String>, avatar: Option<String> }` + `fn display_name(&self) -> &str` (global_name, else username). Missing `username` defaults to `"Unknown user"`.
  - `Channel { id, kind: u8 /* "type" */, guild_id: Option<Id>, name: Option<String>, position: i32, parent_id: Option<Id>, recipient_ids: Vec<Id>, last_message_id: Option<Id> }`
  - `Guild { id, name: String, icon: Option<String>, channels: Vec<Channel> }` — deserializes both legacy shape (`name` top-level) and capabilities shape (`properties.name`, `properties.icon`).
  - `Attachment { id, filename, url, proxy_url, width: Option<u32>, height: Option<u32>, content_type: Option<String>, size: u64 }`
  - `Message { id, channel_id: Id, author: User, content: String, timestamp: String, edited_timestamp: Option<String>, attachments: Vec<Attachment>, mentions: Vec<User>, mention_everyone: bool, message_reference: Option<MessageRef { message_id: Option<Id> }>, referenced_message: Option<Box<Message>>, nonce: Option<String> }` — `nonce` accepts JSON string or integer.
  - `MessageUpdate { id, channel_id, content: Option<String>, edited_timestamp: Option<String>, attachments: Option<Vec<Attachment>> }`
  - `ReadStateEntry { id: Id, last_message_id: Option<Id>, mention_count: u32 }`
  - `Ready { user: User, guilds: Vec<Guild>, private_channels: Vec<Channel>, read_state: Vec<ReadStateEntry>, users: Vec<User>, session_id: String, resume_gateway_url: String }` — `read_state` accepts both a bare array and `{ "entries": [...] }`.
  - `enum Event { Ready(Box<Ready>), Resumed, GuildCreate(Guild), GuildDelete { id: Id }, ChannelCreate(Channel), ChannelUpdate(Channel), ChannelDelete(Channel), MessageCreate(Box<Message>), MessageUpdate(MessageUpdate), MessageDelete { id: Id, channel_id: Id }, MessageAck { channel_id: Id, message_id: Id }, Unknown(String) }`
  - `pub fn parse_dispatch(name: &str, data: &serde_json::value::RawValue) -> Result<Event, serde_json::Error>`

- [ ] **Step 1: Create the workspace** with `cargo new --lib crates/api --name rxcord-api`, add deps via `cargo add` (serde+derive, serde_json+raw_value, thiserror, tracing). Write fixtures by hand: `ready_legacy.json`, `ready_capabilities.json` (guild with `properties`, read_state as `{entries}`), `message_create.json`, `message_create_minimal_author.json` (author has only `id`), `message_create_int_nonce.json`, `unknown_event.json`.

- [ ] **Step 2: Write failing tests** in `crates/api/tests/parse.rs`:
  - `ready_legacy_parses`: 1 guild named as in fixture, its channels parsed, read_state len matches.
  - `ready_capabilities_parses`: guild `name` comes from `properties.name`; read_state from `entries`.
  - `message_minimal_author`: `author.display_name() == "Unknown user"`.
  - `message_int_nonce`: `nonce == Some("1234".into())`.
  - `unknown_event_is_unknown`: `parse_dispatch("SOME_NEW_THING", ..)` → `Ok(Event::Unknown("SOME_NEW_THING"))`.
  - `malformed_known_event_errs`: `parse_dispatch("MESSAGE_CREATE", r#"{"id":5}"#)` → `Err`.

- [ ] **Step 3: Run** `cargo test -p rxcord-api` — expect compile failure / FAIL.
- [ ] **Step 4: Implement** `id.rs`, `model.rs`, `event.rs`. Guild dual shape: deserialize into a private helper struct with optional `name`/`icon`/`properties`, then `impl From`/`try_from`.
- [ ] **Step 5: Run** `cargo test -p rxcord-api` — PASS.
- [ ] **Step 6: Commit** `feat(api): models and dispatch parsing`

---

### Task 2: zlib-stream inflater

**Files:**
- Create: `crates/api/src/inflate.rs`
- Test: in-module `#[cfg(test)]`

**Interfaces:**
- Produces: `pub struct Inflater` with `fn new() -> Self` and `fn push(&mut self, chunk: &[u8]) -> Option<Result<String, InflateError>>` — buffers until the chunk ends with `00 00 FF FF`, then inflates with a persistent `flate2::Decompress` (shared zlib context across messages) and returns the UTF-8 text.

- [ ] **Step 1: Write failing tests** `two_messages_shared_context` (compress two JSON strings with one `flate2::Compress` using `FlushCompress::Sync`, feed each → both decode correctly) and `split_frame` (one message split into 2 chunks → first push `None`, second `Some(Ok(..))`).
- [ ] **Step 2: Run** `cargo test -p rxcord-api inflate` — FAIL.
- [ ] **Step 3: Implement** `Inflater` (add `flate2` dep).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(api): zlib-stream inflater`

---

### Task 3: Gateway session state machine (sans-IO)

**Files:**
- Create: `crates/api/src/client_props.rs`, `crates/api/src/gateway/mod.rs`, `crates/api/src/gateway/session.rs`, `crates/api/src/backoff.rs`
- Test: in-module tests

**Interfaces:**
- Consumes: `parse_dispatch`, `Event` (Task 1).
- Produces:
  - `client_props.rs`: `pub const CAPABILITIES: u64`, `pub const CLIENT_BUILD_NUMBER: u64`, `pub const CLIENT_VERSION: &str`, `pub fn user_agent() -> String`, `pub fn super_properties_json() -> serde_json::Value` (keys: `os`, `browser: "Discord Client"`, `release_channel: "stable"`, `client_version`, `os_version`, `os_arch`, `system_locale`, `client_build_number`), `pub fn super_properties_header() -> String` (base64 of the JSON). OS fields chosen by `cfg!(windows)`. Leave a `// UPDATE FROM OFFICIAL CLIENT` comment on the version constants; initial values: user fills from the current official desktop client (see Review note at the end).
  - `pub struct Session` with `fn new(token: String) -> Self`, `fn on_text(&mut self, text: &str) -> Vec<Action>`, `fn on_heartbeat_due(&mut self) -> Vec<Action>`, `fn on_close(&mut self, code: Option<u16>) -> Action`, `fn resume_url(&self) -> Option<&str>`, `fn heartbeat_interval(&self) -> Option<Duration>`.
  - `pub enum Action { Send(String), Dispatch(Event), Reconnect { resume: bool }, AuthFailed }`
  - `pub fn backoff(attempt: u32, jitter: f64) -> Duration` — `min(2^attempt, 60)` seconds × `(0.5 + 0.5 * jitter)`, `jitter ∈ [0,1)`.

Protocol rules the tests pin (opcodes: 0 dispatch, 1 heartbeat request, 2 identify, 6 resume, 7 reconnect, 9 invalid session, 10 hello, 11 ack):
- Hello (op 10): store interval; send Resume `{token, session_id, seq}` if session_id and seq known, else Identify `{token, capabilities, properties, presence:{status:"unknown",since:0,activities:[],afk:false}, compress:false, client_state:{guild_versions:{}}}`.
- Op 0: update `seq` from `s`; READY stores `session_id`, `resume_gateway_url`; emit `Dispatch(event)`. Malformed → `tracing::warn!`, emit nothing.
- `on_heartbeat_due`: if the previous heartbeat is un-ACKed → `Reconnect{resume:true}`; else `Send({"op":1,"d":seq})` and mark awaiting ACK. Op 11 clears awaiting. Op 1 → send heartbeat immediately.
- Op 7 → `Reconnect{resume:true}`. Op 9 with `d:false` → clear session, `Reconnect{resume:false}`; `d:true` → `Reconnect{resume:true}`.
- Close 4004 → `AuthFailed`. Close 4007/4009 → clear session, `Reconnect{resume:false}`. Other/None → `Reconnect{resume:true}`.

- [ ] **Step 1: Write failing tests:** `hello_without_session_identifies` (op 2, contains capabilities), `hello_with_session_resumes` (after READY + seq 5 → op 6 with seq 5), `zombie_reconnects` (heartbeat due twice with no ACK → `Reconnect{resume:true}`), `ack_clears_zombie`, `invalid_session_false_clears`, `close_4004_auth_failed`, `backoff_caps_at_60` (`backoff(10, 0.999)` ≤ 60 s and ≥ 59 s), `backoff_first_is_about_1s` (`backoff(0,0.0) == 500ms`), `debug_redacts_token` (`format!("{:?}", session)` doesn't contain the token).
- [ ] **Step 2: Run** `cargo test -p rxcord-api gateway backoff` — FAIL.
- [ ] **Step 3: Implement** `client_props.rs`, `session.rs`, `backoff.rs`.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(api): gateway session state machine and backoff`

---

### Task 4: Gateway IO driver + fixture capture tool

**Files:**
- Create: `crates/api/src/gateway/driver.rs`, `crates/api/src/scrub.rs`, `crates/api/examples/capture.rs`

**Interfaces:**
- Consumes: `Session`, `Action`, `Inflater`, `backoff`, `client_props`.
- Produces:
  - `pub enum GatewayEvent { Event(Event), Status(ConnStatus) }`, `pub enum ConnStatus { Connecting, Connected, Reconnecting { attempt: u32 }, AuthFailed }`
  - `pub async fn run_gateway(token: String, tx: tokio::sync::mpsc::Sender<GatewayEvent>)` — loops forever until `AuthFailed` or `tx` closed. Connects (to `resume_url` when resuming) with `User-Agent` from `client_props`; first heartbeat after `interval * rand[0,1)`, then every `interval`; feeds binary frames through `Inflater`; executes `Action`s; sleeps `backoff(attempt, rand)` between attempts; resets `attempt` to 0 after a dispatch is received.
  - `pub fn scrub(v: &mut serde_json::Value)` — replaces string values of keys `token`, `email`, `phone`, `content`, `username`, `global_name`, `name`, `topic`, `avatar`, `icon`, `banner`, `bio`, `url`, `proxy_url`, `filename` with `"x"`, recursively.

- [ ] **Step 1: Write failing test** `scrub_redacts_nested` (nested object/array with `content`, `email`, `name` → all `"x"`; `id` untouched).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** `scrub`. **Step 4: Run** — PASS.
- [ ] **Step 5: Implement** `driver.rs` (deps: tokio full, tokio-tungstenite with `rustls-tls-webpki-roots`, futures-util, rand).
- [ ] **Step 6: Implement** `examples/capture.rs`: reads `RXCORD_TOKEN` env var, runs the gateway for `RXCORD_CAPTURE_SECS` (default 60) and writes each raw dispatch, passed through `scrub`, to `crates/api/tests/fixtures/captured/<seq>_<EVENT>.json`. It must hook in before typed parsing, so add `pub fn run_gateway_raw(token, tx: Sender<(String, Box<RawValue>)>)` sharing the loop with `run_gateway` (one generic inner loop, two thin wrappers).
- [ ] **Step 7: Manual verify** (needs a real token): `RXCORD_TOKEN=... cargo run -p rxcord-api --example capture`. Expect a READY file plus others, and `grep -r "\"content\":\"[^x]" crates/api/tests/fixtures/captured` should print nothing. Then add a test `captured_fixtures_parse`: every captured file passes through `parse_dispatch` without `Err` (skip the test when the dir is empty). Before committing captured fixtures, the user decides whether to commit them (they contain real snowflake IDs).
- [ ] **Step 8: Commit** `feat(api): gateway driver and fixture capture`

---

### Task 5: REST client and rate limiter

**Files:**
- Create: `crates/api/src/rest.rs`, `crates/api/src/ratelimit.rs`, `crates/api/src/nonce.rs`
- Test: in-module tests; `crates/api/tests/rest.rs` using `wiremock`

**Interfaces:**
- Produces:
  - `pub enum ApiError { Unauthorized, Http { status: u16, body: String }, Network(reqwest::Error), Decode(serde_json::Error) }` (thiserror).
  - `pub struct Rest` — `fn new(token: String) -> Self`, `fn with_base(token, base: String) -> Self` (tests). Methods:
    - `async fn messages(&self, channel: Id, before: Option<Id>, limit: u8) -> Result<Vec<Message>, ApiError>` → `GET /channels/{c}/messages?limit=&before=`
    - `async fn send_message(&self, channel: Id, content: &str, nonce: &str, reply_to: Option<Id>) -> Result<Message, ApiError>` → `POST /channels/{c}/messages` `{content, nonce, message_reference?: {message_id}}`
    - `async fn edit_message(&self, channel: Id, message: Id, content: &str) -> Result<Message, ApiError>` → `PATCH`
    - `async fn delete_message(&self, channel: Id, message: Id) -> Result<(), ApiError>` → `DELETE`
    - `async fn ack(&self, channel: Id, message: Id) -> Result<(), ApiError>` → `POST /channels/{c}/messages/{m}/ack` `{"token":null}`
    - `async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, ApiError>` (CDN; no Authorization header)
  - Every authed request sends `Authorization: <token>`, `User-Agent`, `X-Super-Properties`, `X-Discord-Locale`. 401 → `ApiError::Unauthorized`.
  - `pub struct RateLimiter` — `async fn acquire(&self, route: &str)`, `fn update(&self, route: &str, remaining: Option<u32>, reset_after: Option<f64>)`, `fn on_429(&self, route: &str, retry_after: f64, global: bool)`. Route key = `"{METHOD} /channels/{channel_id}/messages"` (message id dropped). `acquire` waits while route is exhausted or global is locked. 429 → `on_429` then retry the same request, max 3 times.
  - `pub fn make_nonce(unix_ms: u64) -> String` → `((unix_ms - 1420070400000) << 22).to_string()`.

- [ ] **Step 1: Write failing tests:**
  - `ratelimit_waits_until_reset` (`#[tokio::test(start_paused = true)]`: `update(route, Some(0), Some(2.0))`; `acquire` completes only after 2 s of virtual time).
  - `global_429_blocks_all_routes`.
  - `nonce_is_snowflake_shaped` (`make_nonce(1420070400000 + 1) == (1u64<<22).to_string()`).
  - wiremock: `send_message_body` (asserts JSON body including `message_reference`), `unauthorized_maps` (401 → `Unauthorized`), `retries_after_429` (first 429 `{"retry_after":0.01,"global":false}`, then 200 → `Ok`).
- [ ] **Step 2: Run** `cargo test -p rxcord-api rest ratelimit nonce` — FAIL.
- [ ] **Step 3: Implement** (deps: reqwest with `json`, `rustls-tls`, `default-features = false`; dev-dep wiremock).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(api): REST client with rate limiting`

---

### Task 6: QR code login (remote auth)

**Files:**
- Create: `crates/api/src/remote_auth/mod.rs`, `crates/api/src/remote_auth/protocol.rs`, `crates/api/src/remote_auth/driver.rs`

**Interfaces:**
- Produces:
  - `pub struct RaProtocol` — `fn new() -> Self` (generates RSA-2048 key), `fn on_text(&mut self, text: &str) -> Result<Vec<RaAction>, RaError>`, `fn decrypt_token(&self, encrypted_b64: &str) -> Result<String, RaError>`.
  - `pub enum RaAction { Send(String), ShowQr(String), Scanned { username: String }, Ticket(String), Cancelled, StartHeartbeat(Duration) }`
  - `pub enum QrStatus { Qr(String), Scanned(String), Expired }`
  - `pub async fn qr_login(status: mpsc::Sender<QrStatus>) -> Result<String, ApiError>` → returns the token.

Protocol (JSON `op` strings): `hello{heartbeat_interval,timeout_ms}` → send `init{encoded_public_key: b64(SPKI DER)}` + start heartbeat (`{"op":"heartbeat"}`); `nonce_proof{encrypted_nonce}` → RSA-OAEP-SHA256 decrypt, send `nonce_proof{proof: base64url_nopad(sha256(nonce))}`; `pending_remote_init{fingerprint}` → `ShowQr("https://discord.com/ra/{fingerprint}")`; `pending_ticket{encrypted_user_payload}` → decrypt → `"id:discrim:avatar:username"` → `Scanned{username}`; `pending_login{ticket}` → `Ticket(ticket)`; `cancel` → `Cancelled`. Driver: on `Ticket`, `POST /users/@me/remote-auth/login {ticket}` → `{encrypted_token}` → `decrypt_token`. On timeout/close → `QrStatus::Expired` and return `Err`.

- [ ] **Step 1: Write failing tests** (in `protocol.rs`): `hello_sends_init_with_pubkey` (decode b64 → parse as SPKI ok), `nonce_proof_roundtrip` (encrypt `b"abc"` with the protocol's public key → feed `nonce_proof` → sent proof == base64url_nopad(sha256("abc"))), `fingerprint_to_qr_url`, `ticket_payload_username` (encrypted `"1:0:x:alice"` → `Scanned{username:"alice"}`), `unknown_op_ignored` (→ `Ok(vec![])`).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** (deps: rsa with `sha2`, sha2, base64). **Step 4: Run** — PASS.
- [ ] **Step 5: Implement** `driver.rs`. Manual check happens in Task 9.
- [ ] **Step 6: Commit** `feat(api): QR remote-auth login`

---

### Task 7: Store

**Files:**
- Create: `crates/store/Cargo.toml`, `crates/store/src/lib.rs`, `crates/store/src/state.rs`
- Test: `crates/store/tests/apply.rs` (uses the `crates/api/tests/fixtures/*.json` files via relative path)

**Interfaces:**
- Consumes: all `rxcord_api` models and `Event`.
- Produces:
  - `pub enum Change { Me, GuildList, Channels { guild: Option<Id> }, DmList, Messages(Id), Unread(Id) }` (`Clone, PartialEq, Debug`)
  - `pub struct Pending { pub nonce: String, pub channel: Id, pub content: String, pub reply_to: Option<Id>, pub failed: bool }`
  - `pub struct Store` with:
    - `fn apply(&mut self, ev: &Event) -> Vec<Change>`
    - `fn open_channel(&mut self, id: Id) -> bool` (true = history must be fetched: channel had no stored messages), `fn close_channel(&mut self, id: Id)` (trims to the 200 newest)
    - `fn prepend_history(&mut self, channel: Id, msgs: Vec<Message>) -> Vec<Change>` (input newest-first from API; dedupe by id)
    - `fn add_pending(&mut self, p: Pending) -> Change`, `fn fail_pending(&mut self, nonce: &str) -> Change`, `fn take_pending(&mut self, nonce: &str) -> Option<Pending>`
    - accessors: `me()`, `guilds() -> &[Guild]` (READY order), `channels_of(guild: Id) -> Vec<&Channel>` (sorted by `position`, then id), `dms() -> Vec<&Channel>` (sorted by `last_message_id` desc), `channel(id)`, `user(id)`, `messages(channel) -> &VecDeque<Message>` (oldest → newest), `pending(channel) -> Vec<&Pending>`, `is_unread(channel) -> bool` (`last_message_id > read.last_read`), `mention_count(channel) -> u32`
  - `pub const MESSAGE_CAP: usize = 200;`

Rules:
- READY replaces all state. GuildCreate/Delete and Channel* update maps → `GuildList` / `Channels{..}` / `DmList`.
- MessageCreate: always update `channel.last_message_id`; if channel unknown → `vec![]`. If channel opened: push unless the id already exists (dedupe), evict oldest beyond `MESSAGE_CAP`, remove pending with matching nonce → `Messages(c)`. If not authored by me: increment `mention_count` when it mentions me (`mentions` contains me, `mention_everyone`, or it's a DM) → `Unread(c)`. If authored by me → mark read.
- MessageUpdate merges present fields; MessageDelete removes; both → `Messages(c)` only if present.
- MessageAck sets `last_read`, zeroes mentions → `Unread(c)`.

- [ ] **Step 1: Write failing tests:** `ready_populates` (guild/channel/dm counts from `ready_legacy.json`), `duplicate_create_once`, `unknown_channel_ignored` (no panic, `vec![]`), `cap_evicts_oldest` (201 creates → len 200, first id gone), `pending_cleared_by_nonce`, `dm_counts_as_mention`, `own_message_marks_read`, `ack_clears_unread`, `history_prepend_dedupes`, `close_trims_to_cap`.
- [ ] **Step 2: Run** `cargo test -p rxcord-store` — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(store): event-driven state`

---

### Task 8: Markdown parser

**Files:**
- Create: `crates/app/src/markdown.rs` (app crate created here with `cargo new crates/app --name rxcord`; `main.rs` stays a stub until Task 9)

**Interfaces:**
- Produces:
  - `pub enum Block { Para(Vec<Span>), Code { lang: Option<String>, text: String } }`
  - `pub struct Span { pub text: String, pub bold: bool, pub italic: bool, pub strike: bool, pub code: bool, pub link: Option<String>, pub mention: Option<Mention> }`
  - `pub enum Mention { User(Id), Channel(Id), Role(Id), Everyone }`
  - `pub fn parse(src: &str) -> Vec<Block>`

Supports: ```` ```lang\n...``` ```` fences, `` `code` ``, `**bold**`, `*italic*`/`_italic_`, `~~strike~~`, bare `http(s)://` links, `<@id>`, `<@!id>`, `<#id>`, `<@&id>`, `@everyone`/`@here`. An unmatched delimiter is plain text. Single left-to-right pass, no backtracking beyond the current line.

- [ ] **Step 1: Write failing tests:** `plain`, `bold_italic_nested` (`***x***` → bold+italic), `inline_code_no_format_inside` (`` `**a**` `` → code span text `**a**`), `fence_with_lang`, `unterminated_fence_is_text`, `lone_stars_are_text`, `mentions_parse`, `link_parse`, `long_no_space_linear` (200_000 `*` chars parses in < 50 ms in debug build).
- [ ] **Step 2: Run** `cargo test -p rxcord markdown` — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(app): discord markdown parser`

---

### Task 9: App shell: runtime thread, bridge, login

**Files:**
- Create: `crates/app/build.rs`, `crates/app/ui/app.slint`, `crates/app/ui/login.slint`, `crates/app/src/main.rs`, `crates/app/src/runtime.rs`, `crates/app/src/token.rs`, `crates/app/src/view.rs`

**Interfaces:**
- Consumes: `run_gateway`, `Rest`, `qr_login`, `Store`, `Change`.
- Produces:
  - `pub enum Command { LoginToken(String), StartQr, Logout, OpenChannel(Id), LoadOlder(Id), Send { channel: Id, content: String, reply_to: Option<Id> }, Retry(String /*nonce*/), Edit { channel: Id, message: Id, content: String }, Delete { channel: Id, message: Id }, ThumbVisible(String /*url*/) }`
  - `pub fn spawn_runtime(ui: slint::Weak<AppWindow>) -> tokio::sync::mpsc::UnboundedSender<Command>` (starts a `std::thread` running a multi-thread tokio runtime with 2 worker threads).
  - `token.rs`: `load() -> Option<String>`, `save(&str)`, `delete()` (keyring features: `sync-secret-service`, `crypto-rust`, `windows-native`).
  - `view.rs`: plain `Send` view structs + pure builders, unit tested: `GuildRow { id, name, unread: bool, mentions: u32 }`, `ChannelRow { id, name, kind, unread, mentions, enabled: bool /* false for voice */ }`, `MessageRow { id: Option<Id>, nonce: Option<String>, author: String, time: String /* local HH:MM */, blocks: Vec<Block>, thumbs: Vec<String>, files: Vec<(String,String)>, reply_preview: Option<String>, edited: bool, mine: bool, pending: bool, failed: bool }`; `fn guild_rows(&Store)`, `fn channel_rows(&Store, guild: Id)`, `fn dm_rows(&Store)`, `fn message_rows(&Store, channel: Id) -> Vec<MessageRow>` (stored messages then pending ones).
  - Slint screens: `login` (QR image + status text + token field + button), `main` (empty frame for Task 10), global `reconnecting` banner bound to `ConnStatus`.

Flow: startup → `token::load()` → if some, start gateway; else show login. `AuthFailed` or any `ApiError::Unauthorized` → `token::delete()`, stop gateway, show login. QR: render the URL with `qrcode` into a `SharedPixelBuffer` on the runtime, build `slint::Image` on the UI thread. Logging via `tracing-subscriber`, `RUST_LOG` env, default `warn`.

- [ ] **Step 1: Write failing tests** in `view.rs`: `message_rows_include_pending_last`, `voice_channel_disabled` (kind 2 → `enabled == false`), `time_is_local_hhmm`.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement** `view.rs`. **Step 4: Run** — PASS.
- [ ] **Step 5: Implement** the shell (`build.rs` with `slint_build::compile("ui/app.slint")`, runtime, token, login UI).
- [ ] **Step 6: Manual verify:** `cargo run -p rxcord` → QR shows; scanning with the phone logs in (READY received, `RUST_LOG=rxcord=info` shows it); restart → skips login; delete keyring entry → login again. Token paste path also works.
- [ ] **Step 7: Commit** `feat(app): runtime, bridge and login`

---

### Task 10: Guilds, channels, DMs, message view, scrollback, unread

**Files:**
- Create: `crates/app/ui/sidebar.slint`, `crates/app/ui/messages.slint`, `crates/app/ui/message_row.slint`
- Modify: `crates/app/ui/app.slint`, `crates/app/src/runtime.rs`

**Interfaces:**
- Consumes: `view.rs` builders, `Command`, `Change`.
- Produces: Slint callbacks `select-guild(string)`, `select-channel(string)`, `load-older()`, `thumb-visible(string)`; UI models `guilds`, `channels`, `messages`.

Rules: `Change::Messages(c)` refreshes the `messages` model only when `c` is the selected channel. `GuildList`/`Channels`/`DmList`/`Unread` refresh only the affected sidebar model. Opening a channel: `store.open_channel` → if true, `Rest::messages(c, None, 50)`; scroll near top → `LoadOlder` → `messages(c, oldest_id, 50)`. When the newest message is visible and the channel is unread, send `ack` (at most once per channel per 3 s). Switching channel calls `close_channel` on the previous one. Markdown blocks render as: `Para` → Slint `StyledText` if the pinned Slint version has it, otherwise one `Text` per paragraph with formatting stripped and code spans shown as-is (decide in Step 1, note the choice in the commit message); `Code` → monospace `Text` in a framed `Rectangle`. Message list = `ListView` (virtualized).

- [ ] **Step 1: Check** whether the Slint version in `Cargo.lock` supports styled/rich text in `Text`; record the decision.
- [ ] **Step 2: Implement** sidebar, message list, row, wiring.
- [ ] **Step 3: Manual verify:** guilds and DMs listed; opening a channel loads 50 messages; scrolling up loads more; a new message from another account appears live; unread dot clears after viewing; voice channels greyed out.
- [ ] **Step 4: Commit** `feat(app): navigation and message view`

---

### Task 11: Composer: send, reply, edit, delete, retry

**Files:**
- Create: `crates/app/ui/composer.slint`
- Modify: `crates/app/ui/message_row.slint`, `crates/app/src/runtime.rs`

**Interfaces:**
- Consumes: `Rest::{send_message, edit_message, delete_message}`, `Store::{add_pending, fail_pending}`, `make_nonce`.
- Produces: callbacks `send(string)`, `start-reply(string)`, `start-edit(string)`, `delete(string)`, `retry(string)`.

Rules: Send → `make_nonce(now)` → `add_pending` (shows instantly, greyed) → `send_message`; on `Err` → `fail_pending` (red + retry button). Content is kept until the gateway echo removes the pending entry. Retry reuses the same nonce. Edit/Delete only for `mine`. Enter sends, Shift+Enter adds a newline, Up arrow on an empty composer edits your last message, Esc cancels reply/edit. Empty/whitespace-only input is not sent. Content > 2000 chars: send blocked with a counter shown.

- [ ] **Step 1: Write failing test** in `runtime.rs` (extract pure fn): `fn validate_outgoing(s: &str) -> Result<&str, OutgoingError>` → `Empty` for whitespace only, `TooLong(n)` for > 2000 chars (count `char`s), `Ok(trimmed_end)` otherwise.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Implement** composer UI + wiring.
- [ ] **Step 6: Manual verify:** send, reply, edit, delete; unplug network → send → red → reconnect → retry succeeds, message appears once.
- [ ] **Step 7: Commit** `feat(app): composer`

---

### Task 12: Image thumbnails and cache

**Files:**
- Create: `crates/app/src/images.rs`
- Modify: `crates/app/src/runtime.rs`, `crates/app/ui/message_row.slint`

**Interfaces:**
- Produces:
  - `pub struct ByteLru` — `fn new(cap_bytes: usize)`, `fn get(&mut self, key: &str) -> Option<SharedPixelBuffer<Rgba8Pixel>>`, `fn insert(&mut self, key: String, buf: SharedPixelBuffer<Rgba8Pixel>)` (cost = `w*h*4`; evicts LRU until under cap; an item larger than cap is not stored). Built on `lru::LruCache::unbounded()` + a byte counter.
  - `pub const IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;`
  - `pub fn thumb_url(proxy_url: &str, w: Option<u32>, h: Option<u32>, max: u32) -> String` — appends `width`/`height` query params scaled to fit `max` (400), keeping aspect ratio, using `?` or `&` correctly.

Rules: a row's `init` fires `thumb-visible(url)` → runtime: cache hit → push image; miss → `get_bytes(thumb_url)` → decode (`image`, features png/jpeg/webp/gif first frame) → insert → push to UI if still relevant. Max 4 concurrent fetches (`tokio::sync::Semaphore`). Failure → placeholder, no retry. Only `content_type` starting with `image/` gets thumbnails; other attachments render as filename links that open in the browser via the `opener` crate.

- [ ] **Step 1: Write failing tests:** `evicts_lru_over_cap`, `oversize_not_stored`, `get_refreshes_recency`, `thumb_url_scales` (`(800,400,max 400)` → `width=400&height=200`), `thumb_url_existing_query` (uses `&`).
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Wire** into runtime and row. **Manual verify:** a channel with images shows thumbnails; scrolling quickly doesn't freeze; RSS stays below ~150 MB after scrolling through many images (`ps -o rss`).
- [ ] **Step 6: Commit** `feat(app): image thumbnails with byte-capped cache`

---

### Task 13: Notifications

**Files:**
- Create: `crates/app/src/notify.rs`
- Modify: `crates/app/src/runtime.rs`

**Interfaces:**
- Produces: `pub fn should_notify(msg: &Message, me: Id, is_dm: bool, selected: Option<Id>, window_focused: bool) -> bool` — true when author ≠ me AND (is_dm OR mentions me OR mention_everyone) AND NOT (window_focused AND selected == msg.channel_id). `pub fn show(title: &str, body: &str)` via `notify-rust` (body = first 200 chars, markdown not rendered).

Window focus: track via Slint `Window` winit accessor (`has_focus`) or a focus property updated from the UI; store as `AtomicBool` readable by the runtime.

- [ ] **Step 1: Write failing tests:** `own_message_never`, `dm_notifies`, `mention_notifies`, `focused_selected_suppressed`, `focused_other_channel_notifies`.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Manual verify** on Linux (and Windows when available): DM from another account → OS notification.
- [ ] **Step 6: Commit** `feat(app): native notifications`

---

### Task 14: Renderer selection, idle check, benchmark doc

**Files:**
- Create: `scripts/idle-check`, `docs/benchmarks.md`
- Modify: `crates/app/src/main.rs`, `crates/app/Cargo.toml`

**Interfaces:**
- Produces: renderer chosen at startup: `RXCORD_RENDERER=gpu` → Slint's default GPU renderer, otherwise `slint::BackendSelector::new().renderer_name("software".into()).select()`. `SLINT_BACKEND` set by the user still wins (don't override it).

`scripts/idle-check` (bash, Linux): finds the `rxcord` PID (`pgrep -x rxcord`), samples `utime+stime` from `/proc/<pid>/stat` at t=0 and t=60, computes CPU% of one core using `getconf CLK_TCK`, prints it, exits 1 if ≥ 0.5.

- [ ] **Step 1: Implement** renderer selection; Slint features `backend-winit`, `renderer-software`, `renderer-femtovg`.
- [ ] **Step 2: Implement** `scripts/idle-check`; `chmod +x`.
- [ ] **Step 3: Verify:** build with `cargo build --release -p rxcord`, log in, leave the window idle on a quiet channel, run `scripts/idle-check` → exit 0. Also run with `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` and confirm no frames are reported while idle. If it fails, find what's redrawing/polling (timers, animations, heartbeat-triggered model resets) and fix it before closing the task.
- [ ] **Step 4: Write** `docs/benchmarks.md` with a table template (game, resolution, FPS official client in call, FPS rxcord, CPU idle, RSS) and the first idle-CPU and RSS result.
- [ ] **Step 5: Commit** `feat(app): software renderer default and idle check`

---

## Notes for the executor

- `client_props.rs` version constants (build number, client version, user agent) must match a current official desktop client; ask the user to provide them before Task 9's manual login test. Wrong or very stale values increase ban risk.
- Discord's undocumented API can differ from these notes (e.g. remote-auth message names). If real traffic disagrees with this plan, follow real traffic, fix the fixture/test, and note it in the commit.
- Everything requiring a real account (Tasks 4, 6, 9–14 manual checks) needs the user; stop and ask rather than skipping.
