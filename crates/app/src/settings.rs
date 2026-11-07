//! Window layout and voice settings remembered between runs in `rxcord/settings.json` under the config
//! dir. Never holds the token or message text.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// Audio device names (`devices::inputs/outputs`); None = system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Voice activity sensitivity, 0.1..=0.95.
    pub vad_threshold: f32,
    pub noise_suppression: bool,
    /// Per-person volume 0.0..=2.0 by user id (absent = 1.0).
    pub user_volume: HashMap<String, f32>,
    /// "Mute for me", by user id; the volume is kept for unmuting.
    pub user_muted: HashSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 360.0,
            height: 640.0,
            x: None,
            y: None,
            tree_height: 280.0,
            last_guild: None,
            input_device: None,
            output_device: None,
            vad_threshold: 0.6,
            noise_suppression: true,
            user_volume: HashMap::new(),
            user_muted: HashSet::new(),
        }
    }
}

/// 0.0..=2.0; junk is 1.0.
fn clamp_volume(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 2.0) } else { 1.0 }
}

impl Settings {
    pub fn volume(&self, user: &str) -> f32 {
        self.user_volume.get(user).copied().unwrap_or(1.0)
    }

    pub fn set_volume(&mut self, user: String, v: f32) {
        self.user_volume.insert(user, clamp_volume(v));
    }

    /// What playback applies: 0 when muted for me.
    pub fn gain(&self, user: &str) -> f32 {
        if self.user_muted.contains(user) { 0.0 } else { self.volume(user) }
    }

    pub fn set_threshold(&mut self, t: f32) {
        self.vad_threshold = if t.is_finite() { t.clamp(0.1, 0.95) } else { Settings::default().vad_threshold };
    }

    /// The voice fields of `from` (the window fields are main.rs's).
    pub fn take_voice(&mut self, from: &Settings) {
        self.input_device.clone_from(&from.input_device);
        self.output_device.clone_from(&from.output_device);
        self.vad_threshold = from.vad_threshold;
        self.noise_suppression = from.noise_suppression;
        self.user_volume.clone_from(&from.user_volume);
        self.user_muted.clone_from(&from.user_muted);
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

/// Load, change, save: the UI thread (window) and the runtime (voice) each write only their own fields.
pub fn update(f: impl FnOnce(&mut Settings)) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(p) = path() else { return };
    let mut s = load_from(&p);
    f(&mut s);
    if let Err(e) = save_to(&p, &s) {
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
    let t = s.vad_threshold;
    s.set_threshold(t);
    s.user_volume.values_mut().for_each(|v| *v = clamp_volume(*v));
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
        let s = Settings { width: 400.0, height: 700.0, x: Some(-20), y: Some(10), tree_height: 300.0, last_guild: Some(42), ..Default::default() };
        save_to(&p, &s).unwrap();
        assert_eq!(load_from(&p), s);
        std::fs::write(&p, "{not json").unwrap();
        assert_eq!(load_from(&p), Settings::default(), "corrupt file");
        std::fs::write(&p, r#"{"width": 0, "height": 1e9, "tree_height": -5, "last_guild": 7}"#).unwrap();
        let l = load_from(&p);
        assert_eq!((l.width, l.height, l.tree_height, l.last_guild), (360.0, 640.0, 280.0, Some(7)), "junk sizes reset");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_voice_defaults_and_old_file() {
        let dir = std::env::temp_dir().join(format!("rxcord-settings-voice-{}", std::process::id()));
        let p = dir.join("settings.json");
        std::fs::create_dir_all(&dir).unwrap();
        // a stage 1 file: no voice fields
        std::fs::write(&p, r#"{"width": 400, "height": 700, "x": null, "y": null, "tree_height": 300, "last_guild": 7}"#).unwrap();
        let l = load_from(&p);
        assert_eq!((l.width, l.last_guild), (400.0, Some(7)), "old fields kept");
        assert_eq!((l.input_device, l.output_device, l.vad_threshold, l.noise_suppression), (None, None, 0.6, true));
        assert!(l.user_volume.is_empty() && l.user_muted.is_empty());
        std::fs::write(&p, r#"{"vad_threshold": 7, "user_volume": {"5": 9.0, "6": -1, "7": 0.5}, "user_muted": ["5"]}"#).unwrap();
        let l = load_from(&p);
        assert_eq!(l.vad_threshold, 0.95, "junk threshold clamped");
        assert_eq!((l.volume("5"), l.volume("6"), l.volume("7"), l.volume("8")), (2.0, 0.0, 0.5, 1.0));
        assert_eq!((l.gain("5"), l.gain("7")), (0.0, 0.5), "muted for me: silent, volume kept");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn volume_clamped_0_2() {
        let mut s = Settings::default();
        for (v, want) in [(5.0, 2.0), (-1.0, 0.0), (1.5, 1.5), (f32::NAN, 1.0), (f32::INFINITY, 1.0)] {
            s.set_volume("1".into(), v);
            assert_eq!(s.volume("1"), want, "{v}");
        }
        s.set_threshold(0.0);
        assert_eq!(s.vad_threshold, 0.1);
    }

    #[test]
    fn take_voice_keeps_window_fields() {
        let mut win = Settings { width: 500.0, ..Default::default() };
        let mut voice = Settings { input_device: Some("mic".into()), noise_suppression: false, ..Default::default() };
        voice.user_muted.insert("3".into());
        win.take_voice(&voice);
        assert_eq!((win.width, win.input_device.as_deref(), win.noise_suppression), (500.0, Some("mic"), false));
        assert!(win.user_muted.contains("3"));
    }
}
