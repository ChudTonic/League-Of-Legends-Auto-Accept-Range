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
        Self {
            // Opt-in (off by default so we never silently overwrite someone's
            // rune page), but pre-pointed at Chud's runes Worker so turning the
            // toggle on is all it takes.
            enabled: false,
            auto_import: true,
            endpoint: "https://chud-runes.jivy26.workers.dev/runes".into(),
            sort: "winrate".into(),
        }
    }
}

/// In-client declutter/ad-remover toggles. Consumed by the `CHUD-Declutter`
/// Pengu plugin, which fetches this over the bridge (`/client-customization`)
/// and injects CSS to hide the matching League-client elements. Every option
/// defaults OFF (opt-in) so a fresh install never silently alters the client.
/// Selectors were captured from the live client DOM (see CHUD-Declutter CSS).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientCustomization {
    /// Master switch — when false the plugin removes all its injected CSS.
    pub enabled: bool,
    /// Hide the Store nav tab.
    pub hide_store: bool,
    /// Hide the Loot nav tab.
    pub hide_loot: bool,
    /// Hide the missions button / progression widget on the home screen.
    pub hide_missions: bool,
    /// Hide the battle-pass progression widget.
    pub hide_pass: bool,
    /// Hide promo deep-links and the Riot Discord banner (ads).
    pub hide_promos: bool,
    /// Hide the "buy RP / top up" nudge on the currency counter.
    pub hide_rp_topup: bool,
    /// Hide challenge/lobby banners.
    pub hide_challenges: bool,
    /// Hide the event countdown timer in the game-select bar.
    pub hide_event_timers: bool,
    /// Hide the animated video background on the play/home screen.
    pub hide_home_video: bool,
    /// Notification DND — hide attention-nag pips/badges (activity-center dot,
    /// call-to-action pips, Clash pip, loyalty/rewards badge, nav "new" badges).
    pub hide_notif_badges: bool,
}

impl Default for ClientCustomization {
    fn default() -> Self {
        Self {
            enabled: false,
            hide_store: false,
            hide_loot: false,
            hide_missions: false,
            hide_pass: false,
            hide_promos: false,
            hide_rp_topup: false,
            hide_challenges: false,
            hide_event_timers: false,
            hide_home_video: false,
            hide_notif_badges: false,
        }
    }
}

/// Chat presence override — "Appear Offline" and friends. When `appear_offline`
/// is on, Chud sets your League chat availability to `offline` and re-asserts it
/// (the client resets availability on some gameflow events), so you stay hidden
/// from your friends list while still playing. Off by default; toggling it off
/// restores you to online. Pure LCU write, no Vanguard surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Presence {
    pub appear_offline: bool,
}

impl Default for Presence {
    fn default() -> Self {
        Self { appear_offline: false }
    }
}

/// Skin Library (BETA) — the in-app browser for the upstream skin catalog,
/// served through Chud's `chud-skins` Cloudflare Worker. Hidden behind a beta
/// toggle (`enabled`, default off) until the feature is finished, so it only
/// shows for people who opt in from Settings.
/// A mod the user has installed via the Library (persisted so it survives
/// restarts, like favorites — kept in the config dir, not the install dir).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstalledMod {
    pub name: String,
    pub champ: String,
    pub version: String,
    pub size_mb: f64,
    /// Relative filename in the library mods dir.
    pub file: String,
}

impl Default for InstalledMod {
    fn default() -> Self {
        Self { name: String::new(), champ: String::new(), version: "1.0.0".into(), size_mb: 0.0, file: String::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub enabled: bool,
    pub endpoint: String,
    /// mod_id -> installed record.
    pub installed: std::collections::HashMap<String, InstalledMod>,
    /// favorited mod_ids.
    pub favs: Vec<String>,
    /// check for mod updates on launch.
    pub auto_update: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://chud-skins.jivy26.workers.dev".into(),
            installed: std::collections::HashMap::new(),
            favs: Vec::new(),
            auto_update: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub auto_accept: AutoAccept,
    pub autorange: Autorange,
    pub lcu: Lcu,
    pub safety: Safety,
    pub skins: SkinsCfg,
    pub runes: Runes,
    pub client: ClientCustomization,
    pub presence: Presence,
    pub library: Library,
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
