# rxcord stage 1 — checks and follow-ups for the user

## Manual checks (need a real Discord account)

- [x] Client identity now mimics the Discord web app in Firefox (captured 2026-11-07). Refresh CLIENT_BUILD_NUMBER and FIREFOX_VERSION in crates/api/src/client_props.rs from a browser capture every few weeks.
- [ ] Task 4: `RXCORD_TOKEN=... cargo run -p rxcord-api --example capture` → READY + other files in crates/api/tests/fixtures/captured; `grep -r '"content":"[^x]' crates/api/tests/fixtures/captured` prints nothing; `cargo test -p rxcord-api captured_fixtures_parse` passes on real data. Decide whether to commit captured fixtures (contain real snowflake IDs).
- [ ] Task 4: gateway stays connected >10 min (heartbeats work); kill network for 60 s → reconnects and resumes.
- [ ] Task 6: QR login end-to-end with the phone app (checked as part of Task 9 login).
- [ ] Task 9: `cargo run -p rxcord` → QR shows; scanning with the phone logs in (`RUST_LOG=rxcord=info` shows READY); restart → skips login; delete keyring entry ("rxcord"/"token") → login again; token-paste path works; revoke the token (log out elsewhere) → app returns to login.
- [ ] Task 10: guilds and DMs listed; opening a channel loads 50 messages; scrolling up loads more and stops at the start of history; a message from a second account appears live; unread dot clears after viewing; voice channels greyed out.
- [ ] Task 11: send, reply, edit, delete; unplug network → send → red row → reconnect → retry succeeds and the message appears once; Up arrow edits last own message; >2000 chars blocked; hover shows Reply/Edit/Delete (delete needs a second click), Enter sends, Shift+Enter newline, Esc cancels reply/edit; failed delete shows an inline row error.
- [ ] Task 12: a channel with images shows thumbnails; fast scrolling doesn't freeze; RSS stays below ~150 MB after scrolling through many images (`ps -o rss= -p $(pgrep -x rxcord)`); clicking a thumbnail opens the browser.
- [ ] Task 13: DM from a second account → OS notification (Linux, and Windows when available); mention in a guild → notification; focused on that channel → none; redelivery after reconnect → no duplicate notification.
- [ ] Task 14: logged in, idle on a quiet channel, `scripts/idle-check` → exit 0; also `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` shows no frames while idle; then the in-game FPS comparison vs the official client, recorded in docs/benchmarks.md.
- [x] BEFORE FIRST LIVE LOGIN (final review I5/I6): capture the official desktop client's identify payload and REST headers (devtools Network/WS tab) and match in client_props.rs the FULL `properties` field set (browser_user_agent, browser_version, os_version, native_build_number, client_event_source, …), the REST header set (X-Discord-Timezone, X-Debug-Options, Origin/Referer…), and the values. CAPABILITIES had two bits cleared on purpose (256, 16384) because rxcord doesn't implement token refresh / passive guild updates — re-enable only together with that support.
- [x] (2026-11-07, spare account: READY parses; DM names fixed) Run the `capture` example and `cargo test -p rxcord-api captured_fixtures_parse` on real data BEFORE daily use (confirms your READY parses).

- [ ] TeamSpeak layout (live account): tree expands/collapses, collapsed servers show the dot/badge; clicking a channel opens/activates a tab; 9th tab closes the least recently used; × and middle-click close; right-click a message → Reply/Edit/Open attachment/Delete…; links in messages open the browser; window size/position/divider restored after restart (position not on Wayland); delete the active channel (second account) → text moves to the next tab with a notice.

## Voice (stage 2, need a real account and a second one talking)

- [ ] Task 8: double-click a voice channel → you appear under it, the voice bar shows "🔊 Server / Channel", the second account's voice plays and its row turns green while it talks; double-click another voice channel → you move (one op 4); ✖ → you leave at once (the second account sees it); a moderator disconnecting you → "Disconnected from voice"; unplug the headset mid-call → "No speaker", plug back → audio resumes within ~5 s; kill the network for 5 s → "Reconnecting voice…" then back; restart the main gateway (network off ~60 s) while in voice → voice keeps playing, no duplicate rows (VERIFY LIVE: does a replayed op 4 bring a new VOICE_SERVER_UPDATE with the same endpoint?); idle with no voice → `scripts/idle-check` still < 0.5 %.

## Follow-ups (known gaps, in priority order)

1. **Text typed in a channel that gets deleted** — fixed for the active tab (moves to the next tab). Still lost: the saved draft of a deleted *background* tab, and unsent (pending/failed) messages of a deleted channel (rare). Fix: after `store.apply`, collect pending entries of channels that no longer exist plus the composer draft of a deleted selected channel; push them into the composer via one `orphan-text` callback with the note "Channel was deleted — unsent text moved here"; make `Drafts::switch` carry the composer text forward when the target is `None`.
2. **QR login socket failure (e.g. offline at first launch) shows an empty login screen** — set `login-error` in `Done::QrToken(Err)`.
3. **Channel permissions (VIEW_CHANNEL)** aren't computed: inaccessible channels are listed (they're dropped from the unread dot once they return 403). Needed before daily use in big servers — also reduces 403 requests the official client would never make.
4. **Notifications**: honour muted guilds/channels and `suppress_everyone` (READY `user_guild_settings`) and role mentions (`mention_roles` vs your member roles).
5. **Server-side logout** (`POST /auth/logout`) — "Log out" currently only deletes the token locally.
6. Smaller: banner wording after READY parse failure + reconnect; "Connecting with saved login…" also shown after a fresh login; deleted guild stays selected; `idle-check` exit code when the process dies mid-sample; `RXCORD_RENDERER=gpu` without GL doesn't fall back to software.
