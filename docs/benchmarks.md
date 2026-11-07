# Benchmarks

| Game | Resolution | FPS official client (in call) | FPS rxcord (in call) | CPU idle | RSS |
|------|------------|-------------------------------|----------------------|----------|-----|
| TODO (user) | TODO | TODO | TODO | TODO (logged-in session) | TODO |

The in-game FPS rows need a real account and a game; fill them in by hand.

## Results (2026-11-07, offline, NOT a real session)

Software renderer, 60 s sample, `scripts/idle-check`, no network (`bwrap --unshare-net`).

| Scenario | CPU idle | RSS |
|----------|----------|-----|
| Release build, login screen (QR fetch fails) | 0.02% | 45 MB |
| Debug build, `RXCORD_DEMO=1` main screen | 0.02% | 82 MB |

Stage 2 (voice code in, not in voice; 2026-11-07, offline, NOT a real session):

| Scenario | CPU idle | RSS |
|----------|----------|-----|
| Release build, stored token, gateway retrying offline (`bwrap --unshare-net`) | 0.07% | 46 MB |
| Debug build, `RXCORD_DEMO=1` main screen (demo exists only in debug builds) | 0.02% | 84 MB |

No voice thread and no audio device while not in voice (9 threads, none `rxcord-voice-*`).
In-call CPU: not measurable offline (needs a live call) — manual item in docs/stage1-checks.md.

| In a call (manual) | CPU | RSS |
|--------------------|-----|-----|
| TODO (user): second account talking, 60 s `scripts/idle-check` | TODO | TODO |

`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` on the release login screen: 0 fps for the
whole minute (one initial frame, then nothing).

## How to measure

```sh
cargo build --release -p rxcord
./target/release/rxcord &                  # log in, leave a quiet channel open
scripts/idle-check                         # 60 s, exit 1 if CPU >= 0.5% of one core
scripts/idle-check --secs 10 <pid>         # shorter / explicit pid
ps -o rss= -p $(pgrep -x rxcord)           # RSS in KiB
SLINT_DEBUG_PERFORMANCE=refresh_lazy,console ./target/release/rxcord   # frame log
RXCORD_RENDERER=gpu ./target/release/rxcord                            # femtovg instead of software
```
