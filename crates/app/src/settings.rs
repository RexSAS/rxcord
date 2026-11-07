//! Window layout remembered between runs in `rxcord/settings.json` under the config dir.
//! Never holds the token or message text.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub width: f32,
    pub height: f32,
    /// Window position; None on Wayland (apps can't place windows) or before the first save.
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Height of the tree above the divider, logical px.
    pub tree_height: f32,
    /// Guild expanded on start.
    pub last_guild: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { width: 360.0, height: 640.0, x: None, y: None, tree_height: 280.0, last_guild: None }
    }
}

/// `$XDG_CONFIG_HOME` or `~/.config` on Linux, `%APPDATA%` on Windows.
fn path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))
    };
    Some(base?.join("rxcord").join("settings.json"))
}

/// Defaults when the file is missing or unreadable.
pub fn load() -> Settings {
    path().map(|p| load_from(&p)).unwrap_or_default()
}

pub fn save(s: &Settings) {
    if let Some(p) = path()
        && let Err(e) = save_to(&p, s)
    {
        tracing::warn!("saving settings failed: {e}");
    }
}

fn load_from(p: &Path) -> Settings {
    let Ok(raw) = std::fs::read_to_string(p) else { return Settings::default() };
    let mut s: Settings = serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!("ignoring unreadable settings: {e}");
        Settings::default()
    });
    // Junk sizes would give an unusable window.
    let d = Settings::default();
    if !(200.0..=10000.0).contains(&s.width) || !(200.0..=10000.0).contains(&s.height) {
        (s.width, s.height) = (d.width, d.height);
    }
    if !(60.0..=10000.0).contains(&s.tree_height) {
        s.tree_height = d.tree_height;
    }
    s
}

/// Write-then-rename so a crash never leaves a half-written file.
fn save_to(p: &Path, s: &Settings) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_bad_files() {
        let dir = std::env::temp_dir().join(format!("rxcord-settings-test-{}", std::process::id()));
        let p = dir.join("rxcord").join("settings.json");
        assert_eq!(load_from(&p), Settings::default(), "missing file");
        let s = Settings { width: 400.0, height: 700.0, x: Some(-20), y: Some(10), tree_height: 300.0, last_guild: Some(42) };
        save_to(&p, &s).unwrap();
        assert_eq!(load_from(&p), s);
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_from(&p), Settings::default(), "corrupt file");
        std::fs::write(&p, r#"{"width": 0, "height": 1e9, "tree_height": -5, "last_guild": 7}"#).unwrap();
        let l = load_from(&p);
        assert_eq!((l.width, l.height, l.tree_height, l.last_guild), (360.0, 640.0, 280.0, Some(7)), "junk sizes reset");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
