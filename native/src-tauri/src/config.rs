//! App configuration. Mirrors the relevant slice of the Python `config.json`
//! schema. Loaded from the per-user config dir; missing fields fall back to
//! defaults via `#[serde(default)]`.

use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoAccept {
    /// Whether Auto-Accept arms on launch; persisted so it doesn't silently
    /// re-arm regardless of the user's last choice.
    pub enabled: bool,
    pub check_interval: f64,
    pub retry_delay: f64,
    pub max_backoff: f64,
}

impl Default for AutoAccept {
    fn default() -> Self {
        Self { enabled: true, check_interval: 1.0, retry_delay: 5.0, max_backoff: 30.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Lcu {
    pub request_timeout: f64,
}

impl Default for Lcu {
    fn default() -> Self {
        Self { request_timeout: 2.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Safety {
    pub block_in_ranked: bool,
    /// Dashboard (Auto-Range / input-injection) ban-risk acknowledgement.
    pub injection_ack: bool,
    /// Versioned skin-injection risk acknowledgement. `0` = never acknowledged;
    /// injection allowed only while >= `safety_manager::CURRENT_SKINS_ACK_VERSION`,
    /// so bumping that constant re-gates everyone. Backend-persisted (the old
    /// frontend-only localStorage ack never actually gated anything).
    pub skins_ack_version: u32,
    pub check_interval: f64,
}

impl Default for Safety {
    fn default() -> Self {
        Self { block_in_ranked: true, injection_ack: false, skins_ack_version: 0, check_interval: 2.5 }
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
    /// `GameMonitor`'s unconditional auto-resume safety timeout — never leave
    /// the game suspended longer than this even if `runoverlay` never starts
    /// (clamped 1..=180s). Default 25s: 60s missed the Vanguard startup
    /// handshake and wedged the session until reboot; 25s still covers the
    /// slowest legitimate overlay builds.
    pub monitor_auto_resume_timeout_secs: f64,
    /// Overlay skin-grid column count (1 = large cards … 3 = small). Persisted
    /// so the user's chosen card size sticks across games and restarts.
    pub overlay_card_cols: u8,
    /// Bake the skin's name onto its loading-screen card (the game shows none).
    /// Best-effort cosmetic overlay folded into the injection; a failure never
    /// blocks the skin itself.
    pub loadscreen_labels: bool,
}

impl Default for SkinsCfg {
    fn default() -> Self {
        Self {
            league_path: String::new(),
            injection_threshold_ms: 300,
            enabled: false,
            auto_download_skins: true,
            party_relay_url: String::new(),
            monitor_auto_resume_timeout_secs: 25.0,
            overlay_card_cols: 2,
            loadscreen_labels: true,
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
    /// Chud "runes" Worker URL (`/runes` endpoint returning the normalized
    /// build). Empty = feature inert.
    pub endpoint: String,
    /// Preferred build source, passed through to the Worker ("winrate" |
    /// "popular"); the Worker decides how to honor it.
    pub sort: String,
}

impl Default for Runes {
    fn default() -> Self {
        Self {
            // Opt-in (never silently overwrite a rune page), but pre-pointed at
            // Chud's runes Worker so turning the toggle on is all it takes.
            enabled: false,
            auto_import: true,
            endpoint: "https://chud-runes.jivy26.workers.dev/runes".into(),
            sort: "winrate".into(),
        }
    }
}

/// Chat presence override. When `appear_offline` is on, sets League chat
/// availability to `offline` and re-asserts it (the client resets availability on
/// some gameflow events). Off by default. Pure LCU write, no Vanguard surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Presence {
    pub appear_offline: bool,
}

/// Skin Library (BETA): in-app browser for the upstream skin catalog, served
/// through `chud-skins`. Hidden behind a beta toggle until finished.
/// A mod installed via the Library, persisted in the config dir (not the install
/// dir) so it survives restarts, like favorites.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstalledMod {
    pub name: String,
    pub champ: String,
    pub version: String,
    pub size_mb: f64,
    /// Relative filename in the library mods dir.
    pub file: String,
    /// ModScan verdict recorded at install/rescan time ("clean"/"suspicious"/
    /// "malicious", or "" for pre-ModScan installs never rescanned).
    #[serde(default)]
    pub scan_verdict: String,
    /// SHA-256 of the scanned file (for the ModScan status view).
    #[serde(default)]
    pub scan_sha: String,
    /// Real skin id this mod's assets target, once known. `None` means a
    /// champion-skin mod filed under the base placeholder (`skins/{champ*1000}`)
    /// whose target couldn't be auto-detected at download time — the UI shows a
    /// "Pick skin" control until the user (or a later rescan) resolves it.
    #[serde(default)]
    pub target_skin_id: Option<i64>,
    /// Download/import category ("champion_skin", "vfx", "font", …). Empty for
    /// records saved before this field existed (serde default) — treated as
    /// "unknown" by anything that gates on it.
    #[serde(default)]
    pub category: String,
}

impl Default for InstalledMod {
    fn default() -> Self {
        Self { name: String::new(), champ: String::new(), version: "1.0.0".into(), size_mb: 0.0, file: String::new(), scan_verdict: String::new(), scan_sha: String::new(), target_skin_id: None, category: String::new() }
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
            // Stable enough to ship on by default; the Settings toggle can still turn it off.
            enabled: true,
            endpoint: "https://chud-skins.jivy26.workers.dev".into(),
            installed: std::collections::HashMap::new(),
            favs: Vec::new(),
            auto_update: true,
        }
    }
}

/// Extra hosts allowed for outbound external (non-LCU) requests, on top of
/// `net::allowed_origins`'s built-ins and the hosts implied by the configured
/// endpoints. Empty by default — for an operator pointing an endpoint somewhere
/// `net` can't infer (e.g. a self-hosted mirror on a different domain).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    pub extra_allowed_origins: Vec<String>,
}

/// Party mode. OFF by default; `PartyManager::enable` refuses to run until the
/// versioned data-sharing consent is accepted — no relay connection before opt-in.
/// Transmission details are in `docs/PRIVACY-PARTY.md`; bumping the consent
/// version in `party::manager` re-gates everyone when the disclosure changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Party {
    /// User's persisted on/off choice. Consent is checked independently —
    /// `enabled=true` with stale/no consent still refuses to connect.
    pub enabled: bool,
    /// Version of the party data-sharing disclosure the user accepted
    /// (0 = never / revoked).
    pub consent_version: u32,
    /// Auto-download announcer packs peers advertise (verified against the
    /// Library catalog first). Off by default — needs its own opt-in.
    pub auto_download_peer_announcers: bool,
    /// Auto-download the custom `.fantome` a peer picks — content-addressed via
    /// the skins worker, then ModScan-gated before it's ever trusted. ON by
    /// default so custom skins sync out of the box; the scan is the guard, and
    /// a flagged file is never installed.
    pub auto_download_peer_custom_mods: bool,
}

impl Default for Party {
    fn default() -> Self {
        Self {
            enabled: false,
            consent_version: 0,
            auto_download_peer_announcers: false,
            auto_download_peer_custom_mods: true,
        }
    }
}

/// Anonymous usage telemetry. ON by default. Sends only a per-UTC-day rotating
/// random id + coarse version (no accounts, no IPs stored server-side, no game
/// data). See `telemetry-worker/SPEC.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Telemetry {
    pub enabled: bool,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self { enabled: true }
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
    pub presence: Presence,
    pub library: Library,
    pub network: Network,
    pub party: Party,
    pub telemetry: Telemetry,
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
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("config: parse error ({e}); using defaults");
                Config::default()
            }),
            Err(_) => Config::default(),
        };
        cfg.clamp_intervals();
        cfg
    }

    /// Clamp intervals that gate safety-critical timing so a stale/hand-edited
    /// config value can't self-lock the app: the safety monitor fails injection
    /// closed on any snapshot older than 15s, so an oversized `safety.check_interval`
    /// would wedge it shut; `auto_accept.check_interval` is bounded on the low end too.
    pub(crate) fn clamp_intervals(&mut self) {
        self.safety.check_interval = self.safety.check_interval.clamp(1.0, 10.0);
        self.auto_accept.check_interval = self.auto_accept.check_interval.clamp(0.2, 10.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Party mode must ship fully opted-out: no auto-connect, no accepted
    /// disclosure, no peer-triggered downloads.
    #[test]
    fn party_defaults_are_off() {
        let p = Party::default();
        assert!(!p.enabled);
        assert_eq!(p.consent_version, 0);
        assert!(!p.auto_download_peer_announcers);
    }
}
