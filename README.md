# rxcord

A lightweight native Discord client for Linux and Windows (Rust + Slint), built to cost almost no CPU/GPU while you game.

> ⚠️ Third-party clients break Discord's Terms of Service; accounts can be limited or banned. Use at your own risk, ideally with a spare account first.

## Install

**Windows:** download and run [`rxcord-setup-windows-x86_64.exe`](https://github.com/RexSAS/rxcord/releases/latest/download/rxcord-setup-windows-x86_64.exe). It installs for your user only (no admin needed) and adds a Start menu entry, an optional desktop shortcut and an uninstaller.

**Linux (x86_64):**

```
curl -fsSL https://raw.githubusercontent.com/RexSAS/rxcord/master/installer/install.sh | sh
```

This puts `rxcord` in `~/.local/bin` and adds it to your app menu. Run it again to reinstall.

You can also download the bare binary for your platform from [Releases](https://github.com/RexSAS/rxcord/releases/latest).

**Updates:** when a new release is out, rxcord shows a small window offering to download it. It then shows a progress bar and an Install button that restarts into the new version. Set `RXCORD_NO_UPDATE=1` to turn this off.

## Build from source

```
cargo build --release -p rxcord
./target/release/rxcord
```

## Releasing

Bump `version` in `crates/app/Cargo.toml`, commit, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`). GitHub Actions builds both platforms and publishes the release.
