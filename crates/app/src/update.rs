//! Self-update from GitHub releases: each platform checks for its own release file.
use crate::{AppWindow, bridge};
use rxcord_api::update::{self, Release};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

/// The release file built for this platform (see .github/workflows/release.yml).
pub const ASSET: &str = if cfg!(windows) { "rxcord-windows-x86_64.exe" } else { "rxcord-linux-x86_64" };
const MAX_SIZE: usize = 200 << 20;
const FIRST_CHECK: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(6 * 3600);

pub fn current() -> (u64, u64, u64) {
    update::parse_version(env!("CARGO_PKG_VERSION")).expect("crate version is x.y.z")
}

/// Off for debug builds (running from source) and with RXCORD_NO_UPDATE set.
pub fn enabled() -> bool {
    !cfg!(debug_assertions) && std::env::var_os("RXCORD_NO_UPDATE").is_none()
}

/// The update card's buttons.
#[derive(Debug)]
pub enum Step {
    Download,
    Install,
}

/// Checks now and then; on a newer release the card offers it, then downloads and installs it as the
/// user clicks through (`steps`).
pub async fn run(ui: slint::Weak<AppWindow>, mut steps: mpsc::UnboundedReceiver<Step>) {
    let mut found: Option<Release> = None;
    // the verified new binary, waiting for Install
    let mut ready: Option<Vec<u8>> = None;
    let mut next = tokio::time::Instant::now() + FIRST_CHECK;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {
                next += EVERY;
                match update::latest(ASSET).await {
                    // the same release again: leave the card as it is
                    Ok(Some(r)) if r.version > current() && found.as_ref().is_none_or(|f| f.tag != r.tag) => {
                        bridge::update_state(&ui, "available", &r, 0.0);
                        ready = None;
                        found = Some(r);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::info!("update check failed: {e}"),
                }
            }
            step = steps.recv() => {
                let Some(step) = step else { return };
                let Some(r) = &found else { continue };
                match step {
                    Step::Download => {
                        bridge::update_state(&ui, "downloading", r, -1.0);
                        match fetch(&ui, r).await {
                            Ok(bytes) => {
                                ready = Some(bytes);
                                bridge::update_state(&ui, "ready", r, 1.0);
                            }
                            Err(e) => fail(&ui, r, e),
                        }
                    }
                    Step::Install => {
                        let Some(bytes) = ready.take() else { continue };
                        let installed = match std::env::current_exe() {
                            Ok(exe) => tokio::task::spawn_blocking(move || replace_exe(&exe, &bytes)).await.unwrap_or_else(|e| Err(e.to_string())),
                            Err(e) => Err(e.to_string()),
                        };
                        match installed {
                            Ok(()) => bridge::with_ui(&ui, |ui| ui.invoke_update_restart()),
                            Err(e) => fail(&ui, r, e),
                        }
                    }
                }
            }
        }
    }
}

fn fail(ui: &slint::Weak<AppWindow>, r: &Release, e: String) {
    tracing::warn!("update failed: {e}");
    bridge::update_state(ui, "failed", r, 0.0);
}

/// Downloads the release file with its checksum, moving the card's progress bar.
async fn fetch(ui: &slint::Weak<AppWindow>, r: &Release) -> Result<Vec<u8>, String> {
    let sum_url = r.sha256_url.as_deref().ok_or("the release has no checksum file")?;
    let sum = update::download(sum_url, 1024).await.map_err(|e| e.to_string())?;
    let mut shown = -1;
    let bytes = update::download_with(&r.asset_url, MAX_SIZE, |done, total| {
        let Some(total) = total.filter(|t| *t > 0) else { return };
        let pct = (done * 100 / total) as i32;
        if pct != shown {
            // one UI update per percent, not per chunk
            shown = pct;
            bridge::update_state(ui, "downloading", r, pct as f32 / 100.0);
        }
    })
    .await
    .map_err(|e| e.to_string())?;
    verify(&bytes, &String::from_utf8_lossy(&sum))?;
    Ok(bytes)
}

/// `sum_file`: "<hex>  <name>" as written by sha256sum.
fn verify(bytes: &[u8], sum_file: &str) -> Result<(), String> {
    let want = sum_file.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let got: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if want.len() == 64 && want == got { Ok(()) } else { Err("download doesn't match its checksum".into()) }
}

fn old_path(exe: &Path) -> PathBuf {
    exe.with_extension("old")
}

/// Writes the new binary next to the running one, then swaps it in. Windows can't overwrite a running
/// exe but can rename it, so it's moved aside first (deleted on the next start).
fn replace_exe(exe: &Path, bytes: &[u8]) -> Result<(), String> {
    let new = exe.with_extension("new");
    let err = |what: &str, e: std::io::Error| format!("{what} {}: {e}", new.parent().map(|d| d.display().to_string()).unwrap_or_default());
    std::fs::write(&new, bytes).map_err(|e| err("can't write to", e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| err("can't make executable in", e))?;
    }
    if cfg!(windows) {
        let _ = std::fs::remove_file(old_path(exe));
        std::fs::rename(exe, old_path(exe)).map_err(|e| err("can't move the old version in", e))?;
    }
    std::fs::rename(&new, exe).map_err(|e| err("can't install into", e))
}

/// Deletes the previous version left by a Windows update.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

/// Starts the updated binary; the caller then quits.
pub fn restart() {
    if let Ok(exe) = std::env::current_exe()
        && let Err(e) = std::process::Command::new(exe).spawn()
    {
        tracing::warn!("restart failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum() {
        let sum = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  rxcord-linux-x86_64\n";
        assert!(verify(b"hello", sum).is_ok());
        assert!(verify(b"hellO", sum).is_err());
        assert!(verify(b"hello", "").is_err());
    }

    #[test]
    fn swaps_binary() {
        let dir = std::env::temp_dir().join(format!("rxcord-update-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "rxcord.exe" } else { "rxcord" });
        std::fs::write(&exe, b"old").unwrap();
        replace_exe(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_extension("new").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn version_parses() {
        assert!(current() >= (0, 1, 0));
    }
}
