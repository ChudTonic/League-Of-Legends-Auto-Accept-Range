//! App configuration. Mirrors the relevant slice of the Python `config.json`
//! schema. Loaded from the per-user config dir; missing fields fall back to
//! defaults via `#[serde(default)]`.

use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoAccept {
    /// Whether Auto-Accept arms on app launch. Persisted so the user's on/off
    /// choice survives a restart instead of silently re-arming every launch.
    pub enabled: bool,
    pub check_interval: f64,
    pub retry_delay: f64,
    pub max_retries: u32,
    pub max_backoff: f64,
}

impl Default for AutoAccept {
    fn default() -> Self {
        Self { enabled: true, check_interval: 1.0, retry_delay: 5.0, max_retries: 3, max_backoff: 30.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Lcu {
    pub request_timeout: f64,
    pub cmdline_timeout: f64,
}

impl Default for Lcu {
    fn default() -> Self {
        Self { request_timeout: 2.0, cmdline_timeout: 8.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Safety {
    pub block_in_ranked: bool,
    pub injection_ack: bool,
    pub check_interval: f64,
}

impl Default for Safety {
    fn default() -> Self {
        Self { block_in_ranked: true, injection_ack: false, check_interval: 2.5 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Autorange {
    pub range_hold_key: String,
    pub refresh_interval: f64,
    pub tick_sec: f64,
}

impl Default for Autorange {
    fn default() -> Self {
        Self { range_hold_key: "c".into(), refresh_interval: 7.5, tick_sec: 0.02 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Camera {
    pub camera_hold_key: String,
    pub recenter_mode: String, // "pulse" | "hold"
    pub recenter_hold_sec: f64,
    pub recenter_cooldown_sec: f64,
    pub lost_recenter_sec: f64,
    pub center_radius_px: i64,
    pub vision_interval: f64,
    pub tick_sec: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            camera_hold_key: "space".into(),
            recenter_mode: "pulse".into(),
            recenter_hold_sec: 0.24,
            recenter_cooldown_sec: 0.58,
            lost_recenter_sec: 0.5,
            center_radius_px: 260,
            vision_interval: 0.08,
            tick_sec: 0.02,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkinsCfg {
    /// Empty = autodetect from the running client / registry.
    pub league_path: String,
    pub injection_threshold_ms: u64,
    pub enabled: bool,
    pub auto_download_skins: bool,
    /// Empty = unset; `CHUD_RELAY_URL` env overrides at the use site (party
    /// mode is gated on this until the relay worker is deployed).
    pub party_relay_url: String,
    /// `GameMonitor`'s unconditional auto-resume safety timeout — never
    /// leave the game suspended longer than this even if `runoverlay` never
    /// starts. `GameMonitor::set_auto_resume_timeout` clamps 1..=180s.
    pub monitor_auto_resume_timeout_secs: f64,
}

impl Default for SkinsCfg {
    fn default() -> Self {
        Self {
            league_path: String::new(),
            injection_threshold_ms: 300,
            enabled: false,
            auto_download_skins: true,
            party_relay_url: String::new(),
            monitor_auto_resume_timeout_secs: 60.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Runes {
    /// Master switch for the rune/spell/build auto-importer.
    pub enabled: bool,
    /// Auto-import the moment you lock a champion in champ select.
    pub auto_import: bool,
    /// Chud "runes" Cloudflare Worker URL (the `/runes` endpoint that returns
    /// the normalized build). Empty = feature inert (no import attempted).
    pub endpoint: String,
    /// Preferred build source, passed through to the Worker ("winrate" |
    /// "popular"); the Worker decides how to honor it.
    pub sort: String,
}

impl Default for Runes {
    fn default() -> Self {
        Self { enabled: false, auto_import: true, endpoint: String::new(), sort: "winrate".into() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auto_accept: AutoAccept,
    pub autorange: Autorange,
    pub camera: Camera,
    pub lcu: Lcu,
    pub safety: Safety,
    pub skins: SkinsCfg,
    pub runes: Runes,
}

/// Per-user config file path: `%APPDATA%/LeagueOfLegendsTools/config.json`.
pub fn config_path() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("com", "LeagueOfAndi", "LeagueOfLegendsTools") {
        return dirs.config_dir().join("config.json");
    }
    PathBuf::from("config.json")
}

impl Config {
    /// Load from disk, falling back to defaults for a missing/invalid file.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("config: parse error ({e}); using defaults");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    /// Persist to disk (creates the config dir if needed).
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
    }
}
