# rxcord

A lightweight native Discord client for Linux and Windows (Rust + Slint), built to cost almost no CPU/GPU while you game.

> ⚠️ Third-party clients break Discord's Terms of Service; accounts can be limited or banned. Use at your own risk, ideally with a spare account first.

## Install

Download the file for your platform from [Releases](https://github.com/RexSAS/rxcord/releases/latest) and run it. rxcord checks for new releases and offers a one-click update in its status bar (set `RXCORD_NO_UPDATE=1` to turn that off).

## Build from source

```
cargo build --release -p rxcord
./target/release/rxcord
```

## Releasing

Bump `version` in `crates/app/Cargo.toml`, commit, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`). GitHub Actions builds both platforms and publishes the release.
