//! Chud — native League of Legends toolkit (Tauri app).
//!
//! Chud wires the real Rust core behind the Hextech dashboard: config + stats +
//! LCU lockfile auth + an Auto-Accept loop, plus Auto-Range and the skins
//! subsystem. The app operates openly — no anti-cheat evasion.

mod auto_accept;
mod auto_range;
mod config;
mod input;
mod lcu;
mod lcu_ws;
mod net;
mod profile;
mod runes;
mod safety;
mod safety_manager;
mod skins;
mod stats;
mod telemetry;
mod winutil;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use config::Config;
use stats::Stats;

/// Shared application state. Short std-mutex locks only (never held across an
/// await in the async loop).
pub struct AppState {
    pub config: Mutex<Config>,
    pub stats: Mutex<Stats>,
    pub running: AtomicBool,        // Auto-Accept
    pub client_online: AtomicBool,
    pub phase: Mutex<String>,
    /// The current ready check was already accepted (shared dedup between the
    /// polling loop and the websocket task so one accept counts once).
    pub readycheck_handled: AtomicBool,
    /// Spawn slot for the LCU websocket task: the poller takes it via `swap`
    /// before spawning; the task clears it when it exits.
    pub ws_active: AtomicBool,
    pub auto_range_running: AtomicBool,
    /// Ranked kill-switch consumed by Auto-Range's hold loop. Maintained by the
    /// ALWAYS-RUNNING safety monitor, not by any individual tool. Skin injection
    /// does NOT read this — it goes through `safety_manager::evaluate_injection_policy`.
    pub injection_blocked: AtomicBool,
    /// Live gameflow/queue snapshot + policy state for the injection safety
    /// gates — see `safety_manager.rs`.
    pub safety: safety_manager::SafetyManager,
    pub chat_open: AtomicBool,         // in-game chat open -> release the key
    pub chat_listener_started: AtomicBool,
    pub game_focused: AtomicBool,      // published by tool loops; read by the chat hook (no Win32 in the hook)
    pub auto_range_gen: AtomicU64,     // bumped each arm so a stale duplicate loop exits
    pub auto_accept_gen: AtomicU64,
    pub config_gen: AtomicU64,         // bumped on save so running loops live-reload settings
    /// Skins subsystem shared state — see `docs/SKINS_PORT.md`.
    pub skins: Arc<skins::SkinsState>,
    /// The phase actor's handle, set once during `setup()`. `lcu_ws.rs` reads
    /// `input_tx` to fan events into it. `None` only before `setup()` spawns it.
    pub skins_phase: Mutex<Option<skins::phase::PhaseHandle>>,
    /// The skins bridge server's handle, set once during `setup()`. Holders
    /// clone it and call its `broadcast_*` methods to push state to the Pengu
    /// Loader plugins. `None` only before `setup()` spawns it.
    pub skins_bridge: Mutex<Option<skins::bridge::BridgeHandle>>,
    /// The injection manager, set once during `setup()`; pulled from here (via
    /// the app handle) to run an injection at the loadout deadline. `None` only
    /// before `setup()` builds it.
    pub skins_injection: Mutex<Option<Arc<skins::injection::InjectionManager>>>,
    /// The party mode manager, set once during `setup()` after the bridge,
    /// since it holds a `BridgeHandle` clone to push `party-state` updates
    /// proactively. `None` only before `setup()` builds it.
    pub skins_party: Mutex<Option<Arc<skins::party::manager::PartyManager>>>,
}

/// Mutex helper that ignores poisoning. A panic while holding a lock must not
/// cascade into every later `lock().unwrap()` panicking and freezing the app.
pub trait LockExt<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T>;
}
impl<T> LockExt<T> for Mutex<T> {
    fn lock_safe(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Admin status can't change during a process's lifetime (Windows elevation
/// needs a fresh process), so cache it — `snapshot` runs ~1/s while armed and
/// `winutil::is_admin` is a Win32 syscall.
fn admin_cached() -> bool {
    static ADMIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ADMIN.get_or_init(winutil::is_admin)
}

/// Build the full UI state payload. Same shape for `get_state` and the
/// `state-changed` event so the front-end handles both identically.
fn snapshot(state: &AppState) -> serde_json::Value {
    let running = state.running.load(Ordering::SeqCst);
    let online = state.client_online.load(Ordering::SeqCst);
    let (injection_ack, range_key) = {
        let c = state.config.lock_safe();
        (c.safety.injection_ack, c.autorange.range_hold_key.to_uppercase())
    };
    let admin = admin_cached();
    let range_running = state.auto_range_running.load(Ordering::SeqCst);
    let blocked = state.injection_blocked.load(Ordering::SeqCst);
    let phase = state.phase.lock_safe().clone();
    let stats = state.stats.lock_safe();

    let (status_text, status_tone) = if running && online {
        ("ARMED", "success")
    } else if running && !online {
        ("RECONNECTING", "warning")
    } else if online {
        ("READY", "ice")
    } else {
        ("CLIENT OFFLINE", "neutral")
    };

    let (range_status, range_tone) = if range_running && blocked {
        ("RANKED — BLOCKED", "danger")
    } else if range_running {
        ("ARMED", "success")
    } else {
        ("READY", "ice")
    };

    json!({
        "clientOnline": online,
        "adminReady": admin,
        "injectionAck": injection_ack,
        "injectionBlocked": blocked,
        "phase": phase,
        "activeToolCount": (running as i32) + (range_running as i32),
        "summary": {
            "sessionMatches": stats.session_matches_accepted.to_string(),
            "totalMatches": stats.total_matches_accepted.to_string(),
            "uptime": stats.uptime()
        },
        "tools": [
            {
                "id": "auto_accept", "title": "Auto-Accept", "safe": true, "requiresAdmin": false,
                "running": running, "statusText": status_text, "statusTone": status_tone,
                "subtitle": "Watches the Riot client and accepts the ready check for you.",
                "metricLabel": "Accepted · session", "metricValue": stats.session_matches_accepted.to_string(),
                "runtimeCopy": if running { "Watching the client, ready to snap up the next ready check." }
                               else { "Arm Auto-Accept when you're ready to queue." },
                "primaryActionText": if running { "Stop Tool" } else { "Arm Auto-Accept" }
            },
            {
                "id": "auto_range", "title": "Auto-Range", "safe": false, "requiresAdmin": true,
                "running": range_running, "statusText": range_status, "statusTone": range_tone,
                "subtitle": "Holds the show-range key while a live game is focused; auto-disabled in ranked.",
                "metricLabel": "Range key", "metricValue": range_key,
                "runtimeCopy": if blocked && range_running { "Ranked game detected — Auto-Range is disabled to protect your account." }
                               else if range_running { "Armed: holding range while the game window is focused." }
                               else { "Hold your attack-range indicator during live games." },
                "primaryActionText": if range_running { "Stop Tool" } else { "Launch Auto-Range" }
            }
        ]
    })
}

pub fn emit_state(app: &AppHandle, state: &AppState) {
    let _ = app.emit("state-changed", snapshot(state));
}

/// The Chud tray glyph (bundled at compile time), decoded to RGBA.
fn tray_icon() -> Option<tauri::image::Image<'static>> {
    let bytes = include_bytes!("../icons/tray.png");
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(tauri::image::Image::new_owned(img.into_raw(), w, h))
}

/// Proxy an LCU asset image (`/lol-game-data/assets/v1/<path>`) to the WebView,
/// so `http://lcu.localhost/<path>` `<img>` URLs work with auth + the LCU's
/// self-signed cert. Returns 404 when the client isn't reachable.
async fn fetch_lcu_asset(path: &str) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::Response;
    let blank = || Response::builder().status(404).body(Vec::new()).unwrap_or_else(|_| Response::new(Vec::new()));
    let Some(auth) = lcu::cached_auth() else { return blank() };
    let client = lcu::asset_client();
    // Accept two URL forms:
    //   * a full LCU asset path ("lol-game-data/...") — for item/spell icons whose
    //     iconPath lives outside the v1 tree (e.g. .../ASSETS/Items/Icons2D/...);
    //   * the v1 shortcut ("champion-icons/64.png") — prefixed automatically.
    let trimmed = path.trim_start_matches('/');
    let endpoint = if trimmed.starts_with("lol-game-data/") {
        format!("/{trimmed}")
    } else {
        format!("/lol-game-data/assets/v1/{trimmed}")
    };
    match lcu::get_bytes(client, &auth, &endpoint).await {
        Some((bytes, ct)) => Response::builder()
            .header("Content-Type", ct)
            .header("Access-Control-Allow-Origin", "*")
            .body(bytes)
            .unwrap_or_else(|_| Response::new(Vec::new())),
        None => {
            lcu::invalidate_auth();
            blank()
        }
    }
}

/// Arm Auto-Accept: bump the generation so any stale loop from a rapid
/// off→on toggle exits instead of running alongside the new one (same
/// duplicate-loop guard Auto-Range uses).
fn spawn_auto_accept(app: &AppHandle, state: Arc<AppState>) {
    let app = app.clone();
    let generation = state.auto_accept_gen.fetch_add(1, Ordering::SeqCst) + 1;
    tauri::async_runtime::spawn(async move { auto_accept::run(app, state, generation).await });
}

/// Arm/disarm Auto-Range, gated behind the ban-risk acknowledgment and admin.
/// Driven by the dashboard toggle only — once armed it is always-on in game
/// (no in-game hotkey; an accidental press could silently disarm it).
fn toggle_auto_range(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let acked = state.config.lock_safe().safety.injection_ack;
    if !acked || !winutil::is_admin() {
        emit_state(app, &state);
        return;
    }
    if state.auto_range_running.load(Ordering::SeqCst) {
        state.auto_range_running.store(false, Ordering::SeqCst);
    } else {
        let generation = state.auto_range_gen.fetch_add(1, Ordering::SeqCst) + 1;
        state.auto_range_running.store(true, Ordering::SeqCst);
        auto_range::start(app.clone(), state.inner().clone(), generation);
    }
    emit_state(app, &state);
}

#[tauri::command]
fn get_state(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    snapshot(&state)
}

#[tauri::command]
fn toggle_tool(id: String, app: AppHandle, state: tauri::State<Arc<AppState>>) {
    match id.as_str() {
        "auto_accept" => {
            let now_on = if state.running.load(Ordering::SeqCst) {
                state.running.store(false, Ordering::SeqCst); // loop exits on next check
                false
            } else {
                state.running.store(true, Ordering::SeqCst);
                spawn_auto_accept(&app, state.inner().clone());
                true
            };
            // Persist the choice so it survives an app restart — Auto-Accept no
            // longer silently re-arms itself on every launch.
            let mut cfg = state.config.lock_safe();
            cfg.auto_accept.enabled = now_on;
            let _ = cfg.save();
        }
        "auto_range" => {
            toggle_auto_range(&app);
            return;
        }
        _ => return,
    }
    emit_state(&app, &state);
}

#[tauri::command]
fn stop_all(app: AppHandle, state: tauri::State<Arc<AppState>>) {
    state.running.store(false, Ordering::SeqCst);
    // Stop the injection tools too (the UI's Stop All means ALL) and force
    // key-ups so nothing stays held.
    release_held_keys(&state);
    emit_state(&app, &state);
}

#[tauri::command]
fn set_injection_ack(accepted: bool, app: AppHandle, state: tauri::State<Arc<AppState>>) {
    {
        let mut cfg = state.config.lock_safe();
        cfg.safety.injection_ack = accepted;
        let _ = cfg.save();
    }
    emit_state(&app, &state);
}

/// Aggregate the player profile from the LCU. Returns `{clientOnline:false}`
/// when the client isn't reachable.
#[tauri::command]
async fn get_profile(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let timeout = state.config.lock_safe().lcu.request_timeout.max(4.0);
    let auth = match lcu::cached_auth() {
        Some(a) => a,
        None => return Ok(json!({ "clientOnline": false })),
    };
    let client = lcu::build_lcu_client(timeout);
    let result = profile::build_profile(&client, &auth).await;
    // Stale cached auth (client restarted) → drop it so the next call refinds.
    if result.get("clientOnline").and_then(|v| v.as_bool()) == Some(false) {
        lcu::invalidate_auth();
    }
    Ok(result)
}

#[tauri::command]
fn get_config(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    serde_json::to_value(&*state.config.lock_safe()).unwrap_or_else(|_| json!({}))
}

#[tauri::command]
fn save_config(cfg: serde_json::Value, app: AppHandle, state: tauri::State<Arc<AppState>>) {
    match serde_json::from_value::<Config>(cfg) {
        Ok(mut parsed) => {
            {
                let mut c = state.config.lock_safe();
                // The general Settings page sends its WHOLE `cfg` snapshot, but
                // that snapshot goes stale the moment the user flips a control
                // saved by a DEDICATED command — the Library beta toggle
                // (`set_library_enabled`), appear-offline (`set_appear_offline`),
                // client declutter (`skins_set_customization`), or the Skins
                // page (`skins_save_settings`). A blind `*c = parsed` would
                // silently revert those. Preserve the dedicated-command sections
                // from the live config so a general save never clobbers them.
                parsed.library = c.library.clone();
                parsed.client = c.client.clone();
                parsed.presence = c.presence.clone();
                parsed.skins = c.skins.clone();
                *c = parsed;
                let _ = c.save();
            }
            // Signal running tool loops to live-reload their parameters.
            state.config_gen.fetch_add(1, Ordering::SeqCst);
            emit_state(&app, &state);
        }
        Err(e) => eprintln!("save_config: rejected invalid config ({e})"),
    }
}

/// Open an external attribution link (e.g. "View on RuneForge") in the
/// user's default browser. Locked down: HTTPS only, and only the specific
/// hosts we surface links to — a webview-supplied URL can't turn this into a
/// general "launch anything" shell primitive.
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid url".to_string())?;
    if parsed.scheme() != "https" {
        return Err("only https links are allowed".to_string());
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let allowed = host == "runeforge.dev" || host.ends_with(".runeforge.dev");
    if !allowed {
        return Err(format!("host not allowed: {host}"));
    }
    winutil::open_in_browser(parsed.as_str());
    Ok(())
}

#[tauri::command]
fn request_admin(app: AppHandle) {
    // Relaunch elevated (UAC), then exit so the elevated instance takes over.
    if !winutil::is_admin() {
        winutil::relaunch_as_admin();
        app.exit(0);
    }
}

/// Disarm the injection tools and send unconditional key-ups for their
/// configured keys, so no key can be left stuck down when the process exits
/// mid-hold. Called on every exit path.
fn release_held_keys(state: &AppState) {
    state.auto_range_running.store(false, Ordering::SeqCst);
    let key = { state.config.lock_safe().autorange.range_hold_key.clone() };
    if let Some(mut injector) = input::Injector::new(&key) {
        injector.force_release();
    }
}

#[tauri::command]
fn exit_app(app: AppHandle, state: tauri::State<Arc<AppState>>) {
    release_held_keys(&state);
    app.exit(0);
}

/// Diagnostics snapshot for the Diagnostics page: app/build info, elevation,
/// live LCU/auth/phase state, tool states, resolved hotkeys, config summary,
/// and the on-disk config/data paths. Resolves LCU auth on demand (a quick
/// process scan), so it reflects the client's current reachability.
#[tauri::command]
fn get_diagnostics(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let cfg = state.config.lock_safe().clone();
    let phase = state.phase.lock_safe().clone();
    let auth = lcu::cached_auth();
    json!({
        "app": {
            "name": "Chud",
            "version": env!("CARGO_PKG_VERSION"),
            "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        },
        "system": {
            "admin": winutil::is_admin(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "lcu": {
            "clientOnline": state.client_online.load(Ordering::SeqCst),
            "authFound": auth.is_some(),
            "endpoint": auth.as_ref().map(|a| a.base_url.clone()).unwrap_or_default(),
            "phase": phase,
        },
        "tools": {
            "autoAccept": state.running.load(Ordering::SeqCst),
            "autoRange": state.auto_range_running.load(Ordering::SeqCst),
            "injectionBlocked": state.injection_blocked.load(Ordering::SeqCst),
            "injectionAck": cfg.safety.injection_ack,
            "skinsAckVersion": cfg.safety.skins_ack_version,
            "injectionPolicy": injection_policy_json(&state),
        },
        "hotkeys": { "autoRange": "none (always-on while armed)" },
        "config": {
            "rangeHoldKey": cfg.autorange.range_hold_key,
            "blockInRanked": cfg.safety.block_in_ranked,
            "checkInterval": cfg.auto_accept.check_interval,
        },
        "paths": {
            "config": config::config_path().to_string_lossy(),
            "data": stats::data_dir().to_string_lossy(),
        }
    })
}

// ============================================================
// Skins control panel — see `docs/SKINS_PORT.md` §5. These commands are thin
// wrappers over the skins subsystem; none re-derive logic that lives in
// `skins::*`. Party's `party-state` broadcast goes out over the in-client
// bridge WebSocket to the Pengu Loader plugins, NOT to this Tauri webview, so
// there is no push event here — the front-end polls `skins_party_get_state`.
// ============================================================

/// Disk/process checks shared by `skins_snapshot` and `skins_diagnostics` —
/// computed once per call so the two never disagree on the same request.
struct SkinsStatusChecks {
    pengu_active: bool,
    skins_downloaded: bool,
    hashes_ready: bool,
    tools_available: bool,
    dll_valid: bool,
    /// Why the DLL is invalid, for the setup gate: "" (ok) / "missing" /
    /// "mismatch" / "unreadable".
    dll_reason: &'static str,
}

fn skins_status_checks() -> SkinsStatusChecks {
    use skins::injection::tools::DllVerifyError;
    let tools_dir = skins::injection::tools::cslol_tools_dir();
    let dll = skins::injection::tools::verify_cslol_dll(&tools_dir);
    SkinsStatusChecks {
        pengu_active: skins::pengu::is_active(),
        skins_downloaded: skins::downloads::skins_present(&skins::paths::skins_dir()),
        hashes_ready: tools_dir.join("hashes.game.txt").exists(),
        tools_available: skins::injection::tools::check_tools_available(&tools_dir),
        dll_valid: dll.is_ok(),
        dll_reason: match dll {
            Ok(()) => "",
            Err(DllVerifyError::Missing) => "missing",
            Err(DllVerifyError::HashMismatch) => "mismatch",
            Err(DllVerifyError::Unreadable) => "unreadable",
        },
    }
}

fn skins_diagnostics_value(checks: &SkinsStatusChecks, bridge_port: Option<u16>) -> serde_json::Value {
    json!({
        "bridgePort": bridge_port,
        "penguActive": checks.pengu_active,
        "toolsAvailable": checks.tools_available,
        "dllValid": checks.dll_valid,
        "dllReason": checks.dll_reason,
        "cslolDir": skins::injection::tools::cslol_tools_dir().to_string_lossy(),
        "skinsDownloaded": checks.skins_downloaded,
        "hashesReady": checks.hashes_ready,
        "dataDir": skins::paths::data_root().to_string_lossy(),
    })
}

/// Open the cslol-tools folder (where the user drops `cslol-dll.dll`) in
/// Explorer — the setup gate's "Open folder" action.
#[tauri::command]
fn skins_open_cslol_dir() -> Result<(), String> {
    let dir = skins::injection::tools::cslol_tools_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    winutil::open_in_browser(&dir.to_string_lossy());
    Ok(())
}

/// Current injection-policy decision as UI JSON (`{allowed, code, message}`).
/// Overrides with `ACTIVE_JOB` when a job is in flight — the policy function
/// itself deliberately doesn't read the job flag (see its LOCKING NOTE); a
/// command thread holds no injection locks, so reading it here is safe.
fn injection_policy_json(state: &AppState) -> serde_json::Value {
    let busy = {
        let mgr = state.skins_injection.lock_safe().clone();
        mgr.is_some_and(|m| m.injection_in_progress())
    };
    let decision = safety_manager::evaluate_injection_policy(state, safety_manager::InjectionOp::Build);
    if busy {
        if let safety_manager::InjectionDecision::Allowed(_) = decision {
            let d = safety_manager::InjectionDenial::ActiveJob;
            return json!({ "allowed": false, "code": d.code(), "message": d.message(), "phase": null, "queueId": null });
        }
    }
    decision.to_json()
}

/// The Skins page's full state payload — same "one snapshot fn behind a thin
/// command" shape `snapshot()`/`get_state` already use for the dashboard.
fn skins_snapshot(state: &AppState) -> serde_json::Value {
    let cfg = state.config.lock_safe().skins.clone();
    let ack_version = state.config.lock_safe().safety.skins_ack_version;
    let party_cfg = state.config.lock_safe().party.clone();
    let bridge_port = state.skins_bridge.lock_safe().as_ref().map(|b| b.port());
    let checks = skins_status_checks();
    let party = match state.skins_party.lock_safe().as_ref() {
        Some(p) => p.get_state(),
        None => json!({
            "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [],
            "consent_ok": party_cfg.consent_version >= skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
            "consent_required_version": skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
            "auto_download_peer_announcers": party_cfg.auto_download_peer_announcers,
        }),
    };
    json!({
        "enabled": cfg.enabled,
        // Versioned backend consent (P0-A): the UI unlocks skins actions only
        // off ackOk, and the backend policy enforces it regardless.
        "ackOk": ack_version >= safety_manager::CURRENT_SKINS_ACK_VERSION,
        "ackVersion": ack_version,
        "ackRequiredVersion": safety_manager::CURRENT_SKINS_ACK_VERSION,
        "policy": injection_policy_json(state),
        "bridgePort": bridge_port,
        "penguActive": checks.pengu_active,
        "skinsDownloaded": checks.skins_downloaded,
        "hashesReady": checks.hashes_ready,
        "leaguePath": cfg.league_path,
        "injectionThresholdMs": cfg.injection_threshold_ms,
        "autoResumeSecs": cfg.monitor_auto_resume_timeout_secs,
        "autoDownload": cfg.auto_download_skins,
        "party": party,
        "diagnostics": skins_diagnostics_value(&checks, bridge_port),
    })
}

#[tauri::command]
fn skins_get_state(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    skins_snapshot(&state)
}

/// Persist a partial `SkinsCfg` update (only the keys present are applied —
/// snake_case, matching `config::SkinsCfg`'s field names). Live-applies the
/// auto-resume timeout to the running `InjectionManager`, since that field would
/// otherwise need an app restart to take effect.
#[tauri::command]
fn skins_save_settings(settings: serde_json::Value, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let auto_resume = {
        let mut cfg = state.config.lock_safe();
        if let Some(v) = settings.get("league_path").and_then(|v| v.as_str()) {
            cfg.skins.league_path = v.to_string();
        }
        if let Some(v) = settings.get("injection_threshold_ms").and_then(|v| v.as_u64()) {
            cfg.skins.injection_threshold_ms = v;
        }
        if let Some(v) = settings.get("monitor_auto_resume_timeout_secs").and_then(|v| v.as_f64()) {
            cfg.skins.monitor_auto_resume_timeout_secs = v;
        }
        if let Some(v) = settings.get("auto_download_skins").and_then(|v| v.as_bool()) {
            cfg.skins.auto_download_skins = v;
        }
        if let Some(v) = settings.get("party_relay_url").and_then(|v| v.as_str()) {
            cfg.skins.party_relay_url = v.to_string();
        }
        if let Some(v) = settings.get("enabled").and_then(|v| v.as_bool()) {
            cfg.skins.enabled = v;
        }
        let _ = cfg.save();
        cfg.skins.monitor_auto_resume_timeout_secs
    };
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    // Clone the Arc out and DROP the `skins_injection` guard first:
    // `set_auto_resume_timeout` takes the manager's inner+monitor locks, and
    // holding this guard across the call would invert lock order.
    let mgr = state.skins_injection.lock_safe().clone();
    if let Some(mgr) = mgr {
        mgr.set_auto_resume_timeout(auto_resume);
    }
    skins_snapshot(&state)
}

/// Persist the versioned skin-injection risk acknowledgement. `accepted: true`
/// stamps the current disclosure version; `false` revokes (back to 0). The
/// safety policy denies every entrypoint with `CONSENT_MISSING` below the
/// current version, so revocation takes effect immediately, mid-champ-select included.
#[tauri::command]
fn skins_set_ack(accepted: bool, app: AppHandle, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    {
        let mut cfg = state.config.lock_safe();
        cfg.safety.skins_ack_version = if accepted { safety_manager::CURRENT_SKINS_ACK_VERSION } else { 0 };
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    emit_state(&app, &state);
    skins_snapshot(&state)
}

/// Kick off the skin + hash download pipeline on the async runtime and
/// return immediately; progress/completion are reported via the
/// `skins-download-progress`/`skins-download-done` events (main.js's Skins
/// page renders its own progress bar off these — no Win32 dialog here, see
/// `docs/SKINS_PORT.md` §0).
#[tauri::command]
fn skins_download(force: bool, app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let skins_progress_app = app.clone();
        let mut skins_progress = move |done: u64, total: Option<u64>| {
            let _ = skins_progress_app.emit(
                "skins-download-progress",
                json!({ "phase": "skins", "done": done, "total": total }),
            );
        };
        let skins_result = skins::downloads::download_skins_on_startup(force, &mut skins_progress).await;
        if let Err(e) = &skins_result {
            eprintln!("skins_download: skin download failed: {e}");
        }

        let tools_dir = skins::injection::tools::cslol_tools_dir();
        let hashes_progress_app = app.clone();
        let mut hashes_progress = move |done: u64, total: Option<u64>| {
            let _ = hashes_progress_app.emit(
                "skins-download-progress",
                json!({ "phase": "hashes", "done": done, "total": total }),
            );
        };
        let hashes_result = skins::downloads::ensure_hashes(&tools_dir, &mut hashes_progress).await;
        if let Err(e) = &hashes_result {
            eprintln!("skins_download: hash download failed: {e}");
        }

        let ok = skins_result.is_ok() && hashes_result.is_ok();
        let error = skins_result
            .err()
            .map(|e| e.to_string())
            .or_else(|| hashes_result.err().map(|e| e.to_string()));
        let _ = app.emit("skins-download-done", json!({ "ok": ok, "error": error }));
    });
}

/// Activate the bundled Pengu Loader against the resolved League install path
/// (the configured `league_path`, falling back to LCU auto-detection via
/// `lcu_ext::resolve_game_dir`). Blocking (shells out to Pengu Loader's CLI) but
/// Tauri runs non-`async` commands on its own blocking thread pool, so this
/// never stalls the webview.
#[tauri::command]
fn skins_activate_pengu(state: tauri::State<Arc<AppState>>) -> Result<serde_json::Value, String> {
    // Pengu Loader modifies the League install, which Windows only permits with
    // admin rights — a non-elevated launch fails with "requires elevation" (os
    // error 740), which the UI used to surface as a misleading error. Gate here
    // with a clear, actionable message instead.
    if !winutil::is_admin() {
        return Err("Chud needs to run as administrator to set up skins. Pengu Loader modifies your League install, which Windows only allows with admin rights (skin injection needs admin too). Close Chud, right-click it, choose \"Run as administrator,\" then click Activate again.".to_string());
    }

    let configured = state.config.lock_safe().skins.league_path.clone();
    let league_path = if !configured.trim().is_empty() {
        Some(configured)
    } else {
        skins::lcu_ext::resolve_game_dir().map(|p| p.to_string_lossy().into_owned())
    };
    let res = skins::pengu::activate_on_start(league_path.as_deref());
    if res.activated {
        Ok(json!({
            "ok": true,
            "leaguePath": league_path,
            "restartNeeded": res.restart_needed,
            "restarted": res.restarted,
        }))
    } else {
        Err("Couldn't activate Pengu Loader. Make sure League of Legends is installed and, if needed, set its path in Settings, then try again.".to_string())
    }
}

/// Persist the Skins master enable/disable flag. Enforced: the safety policy
/// denies every injection entrypoint with `DISABLED` while this is off — the
/// phase actor/bridge/ticker still run and idle, but nothing they trigger can execute.
#[tauri::command]
fn skins_set_enabled(enabled: bool, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    {
        let mut cfg = state.config.lock_safe();
        cfg.skins.enabled = enabled;
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    skins_snapshot(&state)
}

/// In-client declutter/customization config (read by the CHUD-Declutter plugin
/// via the bridge). Returns the current toggles.
#[tauri::command]
fn skins_get_customization(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    serde_json::to_value(state.config.lock_safe().client.clone()).unwrap_or_else(|_| json!({}))
}

/// Persist the in-client declutter toggles. The CHUD-Declutter plugin polls the
/// bridge and re-applies within a few seconds; no client restart needed.
#[tauri::command]
fn skins_set_customization(customization: serde_json::Value, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    {
        let mut cfg = state.config.lock_safe();
        if let Ok(parsed) = serde_json::from_value::<config::ClientCustomization>(customization) {
            cfg.client = parsed;
        }
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    serde_json::to_value(state.config.lock_safe().client.clone()).unwrap_or_else(|_| json!({}))
}

/// The browsable skin catalog (every champ + its skins, flagged downloaded) for
/// the favorites picker. Takes `_state` only so it matches every other
/// registered command — a zero-argument sync command was silently dropped from
/// `generate_handler!`'s table, so `invoke("skins_catalog")` rejected as "not found".
#[tauri::command]
fn skins_catalog(_state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let champions = skins::favorites::catalog(None);
    skins::slog::log(skins::slog::Level::Info, &format!("[FAVORITES] catalog requested — {} champions", champions.len()));
    json!({ "champions": champions })
}

/// The current `champ_id -> favorite skin_id` map, as string-keyed JSON.
#[tauri::command]
fn skins_get_favorites(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let map = state.skins.shared.lock_safe().favorite_skins.clone();
    let obj: serde_json::Map<String, serde_json::Value> =
        map.iter().map(|(c, s)| (c.to_string(), json!(s))).collect();
    serde_json::Value::Object(obj)
}

/// Set (or clear, when `skin_id` is null) a champion's favorite skin. Persists
/// to disk and, if that champ is already locked this game, re-arms it live.
#[tauri::command]
fn skins_set_favorite(champ_id: i64, skin_id: Option<i64>, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let map = {
        let mut shared = state.skins.shared.lock_safe();
        match skin_id {
            Some(s) => { shared.favorite_skins.insert(champ_id, s); }
            None => { shared.favorite_skins.remove(&champ_id); }
        }
        // If we're mid-champ-select on this champ, re-arm the active favorite now.
        if shared.locked_champ_id == Some(champ_id) {
            shared.active_favorite_skin_id = shared.favorite_skins.get(&champ_id).copied();
        }
        shared.favorite_skins.clone()
    };
    skins::favorites::save(&map);
    let obj: serde_json::Map<String, serde_json::Value> =
        map.iter().map(|(c, s)| (c.to_string(), json!(s))).collect();
    serde_json::Value::Object(obj)
}

/// Fallback `party-state`-shaped JSON for the (brief) window before
/// `setup()` has built the `PartyManager` yet — reads consent straight from
/// config so the UI's gate still renders correctly even then.
fn skins_party_fallback_state(state: &AppState) -> serde_json::Value {
    let c = state.config.lock_safe();
    json!({
        "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [],
        "consent_ok": c.party.consent_version >= skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
        "consent_required_version": skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
        "auto_download_peer_announcers": c.party.auto_download_peer_announcers,
    })
}

/// Enable party mode. Persists `party.enabled=true` only when `enable()`
/// actually succeeds — consent is checked inside `enable()` itself, so a
/// not-yet-consented user's toggle never gets persisted as "on".
#[tauri::command]
async fn skins_party_enable(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else { return Err("Skins subsystem not ready yet".to_string()) };
    party.enable().await?;
    {
        let mut cfg = state.config.lock_safe();
        cfg.party.enabled = true;
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    Ok(party.get_state())
}

/// Disable party mode. Persists `party.enabled=false` first, then tears down
/// the live manager (so the flag is correct on disk even if disable() were
/// somehow interrupted).
#[tauri::command]
async fn skins_party_disable(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    {
        let mut cfg = state.config.lock_safe();
        cfg.party.enabled = false;
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else { return Ok(skins_party_fallback_state(&state)) };
    party.disable().await;
    Ok(party.get_state())
}

#[tauri::command]
async fn skins_party_add_peer(token: String, state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else { return Err("Skins subsystem not ready yet".to_string()) };
    party.add_peer(&token).await?;
    Ok(party.get_state())
}

/// Sync (not async): `PartyManager::get_state` doesn't await anything —
/// the front-end polls this while the Skins page is open (see this
/// section's doc comment on why there's no push event).
#[tauri::command]
fn skins_party_get_state(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    match state.skins_party.lock_safe().as_ref() {
        Some(party) => party.get_state(),
        None => skins_party_fallback_state(&state),
    }
}

/// Persist accepted/revoked party data-sharing consent (`docs/PRIVACY-PARTY.md`).
/// Revoking ALSO force-disables party mode and disconnects immediately — you
/// can't stay connected with consent pulled.
#[tauri::command]
async fn skins_party_set_consent(accepted: bool, state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    {
        let mut cfg = state.config.lock_safe();
        cfg.party.consent_version = if accepted { skins::party::manager::CURRENT_PARTY_CONSENT_VERSION } else { 0 };
        if !accepted {
            cfg.party.enabled = false;
        }
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);

    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else { return Ok(skins_party_fallback_state(&state)) };
    if !accepted {
        party.disable().await;
    }
    Ok(party.get_state())
}

/// Persist the peer-announcer auto-download opt-in — off by default; see
/// `PartyManager::maybe_download_peer_announcer`'s catalog-verification gate,
/// which this toggle sits in front of.
#[tauri::command]
fn skins_party_set_auto_announcers(enabled: bool, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    {
        let mut cfg = state.config.lock_safe();
        cfg.party.auto_download_peer_announcers = enabled;
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    match state.skins_party.lock_safe().as_ref() {
        Some(party) => party.get_state(),
        None => skins_party_fallback_state(&state),
    }
}

/// Background auto-import: while in champ select, when your picked champion
/// changes (and runes auto-import is on + configured), pull + apply the
/// current-patch best build for it — once per champion. Idles cheaply
/// otherwise. Self-contained: touches only the runes config + LCU, never the
/// auto-accept/skins subsystems.
/// P0-3: keep game hashes current. A Riot patch changes WAD layouts, and a
/// stale `hashes.game.txt` makes cslol's overlay build go pathological (the
/// 17GB / crash-repair-loop failure). `ensure_hashes` self-idles when nothing
/// changed (SHA check), so this is cheap on a no-patch launch and only pulls
/// the changed shards after a patch. Only REFRESHES an existing hash file —
/// the first-time 207MB download stays user-initiated (with its progress UI).
fn spawn_hash_autorefresh(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        if !state.config.lock_safe().skins.enabled {
            return;
        }
        let tools_dir = skins::injection::tools::cslol_tools_dir();
        if !tools_dir.join("hashes.game.txt").exists() {
            return; // no existing hashes to refresh
        }
        tokio::time::sleep(std::time::Duration::from_secs(20)).await; // let the app settle
        if !state.config.lock_safe().skins.enabled {
            return;
        }
        let mut noop = |_done: u64, _total: Option<u64>| {};
        match skins::downloads::ensure_hashes(&tools_dir, &mut noop).await {
            Ok(true) => {
                eprintln!("[hashes] refreshed after a game patch");
                let _ = app.emit(
                    "notification",
                    json!({
                        "title": "League updated",
                        "message": "Refreshed skin data for the new patch. Some custom skins may not work until their authors update them.",
                        "tone": "info"
                    }),
                );
            }
            Ok(false) => {}
            Err(e) => eprintln!("[hashes] auto-refresh failed (non-fatal): {e}"),
        }
    });
}

fn spawn_runes_auto_import(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        // LCU-only client. The Worker fetch below gets its OWN external client —
        // the LCU's `danger_accept_invalid_certs` must never be reused off loopback.
        let http = lcu::build_lcu_client(6.0);
        let mut last_champ: Option<i64> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let (enabled, auto, endpoint, sort, allowed) = {
                let c = state.config.lock_safe();
                (c.runes.enabled, c.runes.auto_import, c.runes.endpoint.clone(), c.runes.sort.clone(), net::allowed_origins(&c))
            };
            if !enabled || !auto || endpoint.trim().is_empty() {
                last_champ = None;
                continue;
            }
            let Some(auth) = lcu::cached_auth() else {
                last_champ = None;
                continue;
            };
            match runes::locked_champ_and_role(&http, &auth).await {
                Some((champ, role, mode)) if last_champ != Some(champ) => {
                    let ext_http = net::build_external_client(10.0, allowed.clone());
                    if let Some(build) = runes::fetch_build(&ext_http, &allowed, &endpoint, champ, &role, &mode, &sort).await {
                        let applied = runes::apply_build(&http, &auth, &build).await;
                        if applied.runes {
                            last_champ = Some(champ);
                            eprintln!("[runes] auto-imported build for champion {champ} ({role}/{mode})");
                        }
                    }
                }
                Some(_) => {} // same champion, already imported
                None => last_champ = None, // left champ select / no champion picked
            }
        }
    });
}

/// Keep the chat presence in sync with the "Appear Offline" toggle. The League
/// client resets `availability` back to `chat` on some gameflow events, so a
/// one-shot write isn't enough — this re-asserts `offline` while the toggle is
/// on, and restores `chat` once when it's turned off. Pure LCU write.
fn spawn_appear_offline(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let http = lcu::build_lcu_client(6.0);
        let mut was_enabled = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let enabled = { state.config.lock_safe().presence.appear_offline };
            let Some(auth) = lcu::cached_auth() else { continue }; // client down — nothing to assert
            if enabled {
                let current = lcu::get_json(&http, &auth, "/lol-chat/v1/me")
                    .await
                    .and_then(|v| v.get("availability").and_then(|a| a.as_str()).map(str::to_string));
                if current.as_deref() != Some("offline") {
                    let body = serde_json::json!({ "availability": "offline" });
                    let _ = lcu::request_json(&http, &auth, reqwest::Method::PUT, "/lol-chat/v1/me", Some(&body)).await;
                }
                was_enabled = true;
            } else if was_enabled {
                let body = serde_json::json!({ "availability": "chat" });
                let _ = lcu::request_json(&http, &auth, reqwest::Method::PUT, "/lol-chat/v1/me", Some(&body)).await;
                was_enabled = false;
            }
        }
    });
}

/// Toggle "Appear Offline". Persists the choice and applies it to the live
/// client immediately (best-effort); `spawn_appear_offline` keeps re-asserting.
#[tauri::command]
async fn set_appear_offline(enabled: bool, state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    {
        let mut c = state.config.lock_safe();
        c.presence.appear_offline = enabled;
        let _ = c.save();
    }
    if let Some(auth) = lcu::cached_auth() {
        let http = lcu::build_lcu_client(6.0);
        let body = serde_json::json!({ "availability": if enabled { "offline" } else { "chat" } });
        let _ = lcu::request_json(&http, &auth, reqwest::Method::PUT, "/lol-chat/v1/me", Some(&body)).await;
    }
    Ok(enabled)
}

/// Read the current "Appear Offline" state (for UI hydration).
#[tauri::command]
fn get_appear_offline(state: tauri::State<Arc<AppState>>) -> bool {
    state.config.lock_safe().presence.appear_offline
}

/// Skin Library (BETA) gate state — `{enabled, endpoint}`.
#[tauri::command]
fn library_get(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let c = state.config.lock_safe();
    json!({ "enabled": c.library.enabled, "endpoint": c.library.endpoint })
}

/// Flip the Skin Library beta toggle (hides/shows the Library page).
#[tauri::command]
fn set_library_enabled(enabled: bool, state: tauri::State<Arc<AppState>>) -> bool {
    let mut c = state.config.lock_safe();
    c.library.enabled = enabled;
    let _ = c.save();
    enabled
}

/// Legacy Library download dir (pre-2.0 installs landed here). Kept only so
/// `library_remove` can still clean up files a prior build wrote here; new
/// installs go into the custom-mod store (`mods/skins/{skin_id}`) so they
/// surface on the in-client Custom Mods button in champ select.
fn library_mods_dir() -> std::path::PathBuf {
    skins::paths::data_root().join("library")
}

/// Resolve a champion display name to its numeric id via the offline skin
/// catalog (base-skin name == champion name). Case-insensitive; a small set of
/// known aliases covers names that differ between mod sources and Riot data.
fn resolve_champ_id_by_name(name: &str) -> Option<i64> {
    let want = name.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    let alias = match want.as_str() {
        "wukong" => "monkeyking",
        "nunu" | "nunu & willump" | "nunu and willump" => "nunu & willump",
        "renata glasc" => "renata",
        _ => want.as_str(),
    };
    skins::favorites::catalog(None)
        .into_iter()
        .find(|c| {
            let cn = c.champ_name.to_lowercase();
            cn == want || cn == alias || cn.replace(['&', ' ', '.', '\''], "") == want.replace(['&', ' ', '.', '\''], "")
        })
        .map(|c| c.champ_id)
}

/// Turn a mod display name into a safe `.fantome` file stem (no path/reserved
/// chars, bounded length). Falls back to `fallback` when nothing usable remains.
fn sanitize_mod_filename(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    let cleaned = if cleaned.len() > 80 { &cleaned[..80] } else { cleaned };
    if cleaned.is_empty() { fallback.to_string() } else { cleaned.to_string() }
}

/// The full catalog in one shot (the Library page filters it client-side).
#[tauri::command]
async fn library_catalog_all(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let http = net::build_external_client(20.0, allowed.clone());
    let url = format!("{}/all", endpoint.trim_end_matches('/'));
    net::get_json_checked(&http, &url, &allowed, 16 * 1024 * 1024).await
}

/// Installed mods + favorites + auto-update flag (for UI hydration).
#[tauri::command]
fn library_state(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let c = state.config.lock_safe();
    json!({ "installed": c.library.installed, "favs": c.library.favs, "autoUpdate": c.library.auto_update })
}

/// Toggle a favorite mod; returns the updated fav list.
#[tauri::command]
fn library_set_favorite(mod_id: String, on: bool, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let mut c = state.config.lock_safe();
    c.library.favs.retain(|f| f != &mod_id);
    if on { c.library.favs.push(mod_id); }
    let _ = c.save();
    json!(c.library.favs)
}

#[tauri::command]
fn library_set_auto_update(on: bool, state: tauri::State<Arc<AppState>>) -> bool {
    let mut c = state.config.lock_safe();
    c.library.auto_update = on;
    let _ = c.save();
    on
}

/// Map a catalog category to its custom-mod store subdirectory (relative to
/// `mods/`). Champion skins nest under `skins/{champ*1000}`; every other
/// category is a flat root folder the in-client wheel reads via
/// `list_mods_for_category`. Returns `None` for champion skins (handled
/// separately since they need the resolved champion id).
fn library_category_dir(category: &str) -> Option<&'static str> {
    match category {
        "map_skin" => Some("maps"),
        "announcer" => Some("announcers"),
        "font" => Some("fonts"),
        "ui" => Some("ui"),
        "vfx" => Some("vfx"),
        "sfx" => Some("sfx"),
        "voiceover" => Some("voiceover"),
        "loading_screen" => Some("loading_screen"),
        "miscellaneous" | "other" | "others" => Some("others"),
        _ => None, // champion_skin (and unknown) -> skins/{champ*1000}
    }
}

/// ModScan result surfaced to the UI before a downloaded mod is written to disk
/// (see `scan_downloaded_mod`). `vt` is the Worker's reputation lookup verbatim
/// when it succeeded; a timeout/miss/offline leaves it `None`. VT only escalates
/// the structural scan verdict, never gates on its own.
#[derive(serde::Serialize, Clone)]
pub(crate) struct ScanSummary {
    pub(crate) verdict: String,
    pub(crate) sha256: String,
    pub(crate) blocking: bool,
    pub(crate) findings: Vec<serde_json::Value>,
    pub(crate) vt: Option<serde_json::Value>,
}

/// The worse of two verdicts (Malicious > Suspicious > Clean) — used so a
/// VirusTotal hit can only ESCALATE the structural scanner's call, never
/// downgrade it.
fn worse_verdict(a: modscan_core::Verdict, b: modscan_core::Verdict) -> modscan_core::Verdict {
    use modscan_core::Verdict::*;
    match (a, b) {
        (Malicious, _) | (_, Malicious) => Malicious,
        (Suspicious, _) | (_, Suspicious) => Suspicious,
        _ => Clean,
    }
}

fn verdict_str(v: modscan_core::Verdict) -> &'static str {
    match v {
        modscan_core::Verdict::Clean => "clean",
        modscan_core::Verdict::Suspicious => "suspicious",
        modscan_core::Verdict::Malicious => "malicious",
    }
}

/// Scan a just-downloaded mod's bytes IN MEMORY before anything is written to
/// disk — a malicious archive never touches the filesystem unless the caller
/// explicitly overrides (`force`). Structural scan runs on a blocking thread;
/// the reputation lookup on top can only worsen the verdict, never error out.
async fn scan_downloaded_mod(
    endpoint: &str,
    allowed: &std::collections::HashSet<String>,
    http: &reqwest::Client,
    bytes: Arc<Vec<u8>>,
    label: &str,
) -> ScanSummary {
    use skins::slog::{log_info, log_warn};

    let report = match tokio::task::spawn_blocking(move || modscan_core::scan_bytes(&bytes)).await {
        Ok(report) => report,
        Err(e) => {
            // modscan-core's own contract is "never panics" — this should be
            // unreachable, but fail CLOSED (blocking) rather than silently
            // treat an internal failure as a clean mod.
            log_warn!("[MODSCAN] {label}: scan task failed unexpectedly: {e}");
            return ScanSummary {
                verdict: "malicious".to_string(),
                sha256: String::new(),
                blocking: true,
                findings: vec![json!({ "severity": "malicious", "code": "scan-task-failed", "entry": null, "detail": e.to_string() })],
                vt: None,
            };
        }
    };

    // Best-effort VirusTotal reputation via the chud-skins Worker. Any failure
    // just yields `None`; must never turn a Clean/Suspicious verdict into an error.
    let vt_json = net::get_json_checked(
        http,
        &format!("{}/reputation/{}", endpoint.trim_end_matches('/'), report.sha256),
        allowed,
        64 * 1024,
    )
    .await
    .ok();

    let vt_escalation = vt_json.as_ref().and_then(|v| {
        if v.get("known").and_then(|k| k.as_bool()) != Some(true) {
            return None;
        }
        match v.get("verdict").and_then(|s| s.as_str()) {
            Some("malicious") => Some(modscan_core::Verdict::Malicious),
            Some("suspicious") => Some(modscan_core::Verdict::Suspicious),
            _ => None,
        }
    });
    let effective = vt_escalation.map(|vt| worse_verdict(report.verdict, vt)).unwrap_or(report.verdict);
    let verdict = verdict_str(effective);
    let n = report.findings.len();
    // Always log the outcome, not just the scary cases — a silent clean scan was
    // indistinguishable from "scanner never ran".
    let short_sha: String = report.sha256.chars().take(12).collect();
    let vt_note = vt_json
        .as_ref()
        .filter(|v| v.get("known").and_then(|k| k.as_bool()) == Some(true))
        .and_then(|v| v.get("vt"))
        .map(|vt| format!(" [VT {}/{} flagged]", vt.get("malicious").and_then(|m| m.as_i64()).unwrap_or(0), vt.get("total").and_then(|t| t.as_i64()).unwrap_or(0)))
        .unwrap_or_default();
    if effective != modscan_core::Verdict::Clean {
        log_warn!("[MODSCAN] {label}: {verdict} — {n} finding(s){vt_note}; {}", report.human_summary());
    } else {
        log_info!("[MODSCAN] {label}: clean ({} entries, sha {short_sha}){vt_note}", report.entry_count);
    }

    let findings: Vec<serde_json::Value> =
        report.findings.iter().map(|f| serde_json::to_value(f).unwrap_or_else(|_| json!({}))).collect();

    ScanSummary {
        verdict: verdict.to_string(),
        sha256: report.sha256,
        blocking: effective != modscan_core::Verdict::Clean,
        findings,
        vt: vt_json,
    }
}

/// Download one Library mod from the Worker (our R2), scan it IN MEMORY
/// (ModScan), and place it in the custom-mod store: champion skins under
/// `mods/skins/{champ*1000}`, everything else under `mods/{category}`. Shared
/// by single and bundle install — returns the record without touching config
/// so the caller can batch a save. When the scan blocks (`Suspicious`/
/// `Malicious`) and `force` is false, nothing is written; the returned record
/// is `None`, which the caller must treat as "not installed."
pub(crate) async fn place_library_mod(
    app: Option<&AppHandle>,
    base: &str,
    http: &reqwest::Client,
    allowed: &std::collections::HashSet<String>,
    mod_id: &str,
    name: &str,
    champ: &str,
    champ_id: Option<i64>,
    category: &str,
    force: bool,
) -> Result<(Option<config::InstalledMod>, ScanSummary), String> {
    use skins::slog::{log_info, log_warn};
    // Resolve the destination folder (relative to mods/) by category.
    let rel_dir: std::path::PathBuf = match library_category_dir(category) {
        Some(cat_dir) => std::path::PathBuf::from(cat_dir),
        None => {
            let cid = champ_id.filter(|&id| id > 0).or_else(|| resolve_champ_id_by_name(champ)).ok_or_else(|| {
                log_warn!("[LIBRARY] no champion resolved for skin mod '{name}' (champ='{champ}')");
                format!("Couldn't match \"{champ}\" to a champion.")
            })?;
            std::path::PathBuf::from("skins").join((cid * 1000).to_string())
        }
    };

    // Resolve URL: the Worker's small JSON response (capped generously).
    let dl = net::get_json_checked(http, &format!("{base}/download/{mod_id}"), allowed, 16 * 1024 * 1024)
        .await
        .map_err(|e| { log_warn!("[LIBRARY] download-resolve failed for {mod_id}: {e}"); e })?;
    let url = dl.get("url").and_then(|v| v.as_str()).ok_or("could not resolve download")?.to_string();
    // Binary .fantome/.zip download — 512MB is a sanity ceiling, not a target.
    let raw_bytes = net::get_bytes_checked(http, &url, allowed, 512 * 1024 * 1024).await?;
    // A tiny body is the Worker's 404 (not mirrored/resolvable yet) — treat as a
    // real failure so a bundle reports it, not a 9-byte .fantome written to disk.
    if raw_bytes.len() < 1024 {
        return Err(format!("'{name}' isn't available yet (still mirroring) — try again shortly."));
    }

    // Refcount the downloaded bytes: the scan and the announcer-convert both
    // need a copy, and these can be 512MB — an Arc clone is a pointer bump,
    // not a data copy, so peak memory stays ~1x instead of 2-3x.
    let raw_bytes = Arc::new(raw_bytes);

    // Scan IN MEMORY before anything below touches disk; a blocking verdict
    // without `force` stops here.
    let summary = scan_downloaded_mod(base, allowed, http, raw_bytes.clone(), name).await;
    if summary.blocking && !force {
        return Ok((None, summary));
    }

    // Announcer packs: retarget the global announcer banks so the pack works on
    // SR, ARAM, and Nexus Blitz — done once at download time, never mid-champ-select.
    let converted: Option<Vec<u8>> = if category == "announcer" {
        if let Some(app) = app {
            let _ = app.emit("library-install-phase", json!({ "modId": mod_id, "phase": "converting" }));
        }
        let original = raw_bytes.clone();
        let converted = tokio::task::spawn_blocking(move || skins::announcer_fix::retarget_announcer_pack(&original))
            .await
            .map_err(|e| e.to_string())?;
        if converted.is_none() {
            log_warn!("[LIBRARY] announcer pack '{name}' has no global announcer banks - installed as-is");
        }
        converted
    } else {
        None
    };
    // Write the retargeted pack if there is one, else the original bytes
    // (borrowed from the Arc — no copy).
    let bytes: &[u8] = match &converted {
        Some(v) => v.as_slice(),
        None => raw_bytes.as_slice(),
    };
    let size_mb = (bytes.len() as f64) / 1_048_576.0;

    let dir = skins::paths::mods_dir().join(&rel_dir);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let stem = sanitize_mod_filename(name, mod_id);
    let file_name = format!("{stem}.fantome");
    tokio::fs::write(dir.join(&file_name), bytes).await.map_err(|e| e.to_string())?;
    let rel_file = rel_dir.join(&file_name).to_string_lossy().replace('\\', "/");
    log_info!("[LIBRARY] installed '{name}' ({size_mb:.1} MB) -> mods/{rel_file}");

    let record = config::InstalledMod {
        name: name.to_string(),
        champ: champ.to_string(),
        version: "1.0.0".into(),
        size_mb,
        file: rel_file,
        scan_verdict: summary.verdict.clone(),
        scan_sha: summary.sha256.clone(),
    };
    Ok((Some(record), summary))
}

// ── ModScan panel: on-demand scanning of installed mods + the mods folder ──

/// Structural-only ScanSummary (no VirusTotal lookup) — used by the folder
/// sweep, which would otherwise fire one reputation request per file.
fn structural_summary(report: modscan_core::ScanReport) -> ScanSummary {
    let blocking = report.verdict != modscan_core::Verdict::Clean;
    let findings = report.findings.iter().map(|f| serde_json::to_value(f).unwrap_or_else(|_| json!({}))).collect();
    ScanSummary { verdict: verdict_str(report.verdict).to_string(), sha256: report.sha256, blocking, findings, vt: None }
}

/// Every `.fantome`/`.zip` under `dir`, recursively.
fn collect_mod_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("fantome") || e.eq_ignore_ascii_case("zip")) {
                out.push(p);
            }
        }
    }
    out
}

/// Re-scan one installed Library mod on disk (structural + VirusTotal) and
/// persist the fresh verdict onto its install record.
#[tauri::command]
async fn modscan_rescan(mod_id: String, state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let (rel_file, name, endpoint, allowed) = {
        let c = state.config.lock_safe();
        let rec = c.library.installed.get(&mod_id).ok_or("mod not installed")?;
        (rec.file.clone(), rec.name.clone(), c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let path = skins::paths::mods_dir().join(&rel_file);
    let bytes = tokio::fs::read(&path).await.map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let http = net::build_external_client(20.0, allowed.clone());
    let summary = scan_downloaded_mod(endpoint.trim_end_matches('/'), &allowed, &http, Arc::new(bytes), &name).await;
    {
        let mut c = state.config.lock_safe();
        if let Some(rec) = c.library.installed.get_mut(&mod_id) {
            rec.scan_verdict = summary.verdict.clone();
            rec.scan_sha = summary.sha256.clone();
        }
        let _ = c.save();
    }
    serde_json::to_value(&summary).map_err(|e| e.to_string())
}

/// Manual scan of an arbitrary file (drag/drop or file picker) — structural +
/// VirusTotal.
#[tauri::command]
async fn modscan_scan_path(path: String, state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let p = std::path::PathBuf::from(&path);
    if !p.is_file() {
        return Err("not a file".to_string());
    }
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let bytes = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
    let http = net::build_external_client(20.0, allowed.clone());
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let summary = scan_downloaded_mod(endpoint.trim_end_matches('/'), &allowed, &http, Arc::new(bytes), &name).await;
    serde_json::to_value(json!({ "file": path, "name": name, "scan": summary })).map_err(|e| e.to_string())
}

/// Sweep every mod archive under the mods folder (structural only, so a big
/// folder stays responsive). Returns a per-file verdict list.
#[tauri::command]
async fn modscan_scan_folder() -> Result<serde_json::Value, String> {
    let mods_dir = skins::paths::mods_dir();
    let files = tokio::task::spawn_blocking(move || collect_mod_files(&mods_dir)).await.map_err(|e| e.to_string())?;
    let mut results = Vec::with_capacity(files.len());
    let (mut clean, mut flagged) = (0u32, 0u32);
    for path in files {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let file_str = path.to_string_lossy().into_owned();
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let report = tokio::task::spawn_blocking(move || modscan_core::scan_bytes(&bytes)).await.map_err(|e| e.to_string())?;
                let summary = structural_summary(report);
                if summary.verdict == "clean" { clean += 1; } else { flagged += 1; }
                results.push(json!({ "file": file_str, "name": name, "verdict": summary.verdict, "findings": summary.findings, "sha256": summary.sha256 }));
            }
            Err(e) => {
                flagged += 1;
                results.push(json!({ "file": file_str, "name": name, "verdict": "error", "error": e.to_string() }));
            }
        }
    }
    Ok(json!({ "total": results.len(), "clean": clean, "flagged": flagged, "results": results }))
}

/// Announcer Studio: return the assignable slot list (key/category/label/
/// milestone) so the UI can render the builder grid.
#[tauri::command]
fn announcer_studio_slots() -> serde_json::Value {
    serde_json::to_value(skins::announcer_studio::SLOTS).unwrap_or_else(|_| json!([]))
}

/// Announcer Studio: build + install a custom announcer pack from the UI's
/// per-slot PCM audio. Returns `{ok, file, slots_filled, milestones_skipped, error}`.
#[tauri::command]
async fn announcer_studio_build(
    name: String,
    slots: Vec<skins::announcer_studio::SlotAudio>,
    include_milestones: Option<bool>,
) -> Result<serde_json::Value, String> {
    let result = tauri::async_runtime::spawn_blocking(move || {
        skins::announcer_studio::build_pack(&name, &slots, include_milestones.unwrap_or(false))
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(result).unwrap_or_else(|_| json!({"ok": false})))
}

/// Install a single Library mod, gated behind ModScan (see `place_library_mod`).
/// Returns `{status: "installed"|"blocked", scan, record}` — `force` lets the
/// user override a blocked verdict after seeing the warning; defaults `false`
/// so a bare call never silently installs something flagged.
#[tauri::command]
async fn library_install(
    app: AppHandle,
    mod_id: String,
    name: String,
    champ: String,
    champ_id: Option<i64>,
    category: Option<String>,
    force: Option<bool>,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let http = net::build_external_client(180.0, allowed.clone());
    let (rec, summary) = place_library_mod(
        Some(&app),
        endpoint.trim_end_matches('/'),
        &http,
        &allowed,
        &mod_id,
        &name,
        &champ,
        champ_id,
        &category.unwrap_or_default(),
        force.unwrap_or(false),
    )
    .await?;
    let record_json = match &rec {
        Some(r) => {
            let mut c = state.config.lock_safe();
            c.library.installed.insert(mod_id, r.clone());
            let _ = c.save();
            serde_json::to_value(r).unwrap_or_else(|_| json!({}))
        }
        None => serde_json::Value::Null,
    };
    Ok(json!({
        "status": if rec.is_some() { "installed" } else { "blocked" },
        "scan": serde_json::to_value(&summary).unwrap_or_else(|_| json!({})),
        "record": record_json,
    }))
}

/// The curated champion bundles (from the Worker), enriched with per-skin
/// details + `ready` (mirrored) flags for the Library/Dashboard UI.
#[tauri::command]
async fn library_bundles(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let http = net::build_external_client(20.0, allowed.clone());
    let url = format!("{}/bundles", endpoint.trim_end_matches('/'));
    net::get_json_checked(&http, &url, &allowed, 16 * 1024 * 1024).await
}

/// Install a whole champion bundle in one shot: every skin lands in that
/// champ's custom-mod slot, so all of them show on the in-client Custom Mods
/// button in champ select. `skins` is `[{id, name}, ...]`. Skips (and reports)
/// any skin that isn't mirrored/resolvable yet rather than failing the pack.
#[tauri::command]
async fn library_install_bundle(
    champ: String,
    champ_id: Option<i64>,
    skins: Vec<serde_json::Value>,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    use skins::slog::{log_info, log_warn};
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let base = endpoint.trim_end_matches('/').to_string();
    let http = net::build_external_client(180.0, allowed.clone());
    log_info!("[LIBRARY] installing bundle '{champ}' ({} skins)", skins.len());

    // A blocked skin is reported, not force-installed — no bundle-wide override;
    // the user installs a blocked one individually if they accept the risk.
    let mut recs: Vec<(String, config::InstalledMod)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut blocked: Vec<serde_json::Value> = Vec::new();
    for s in &skins {
        let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let nm = s.get("name").and_then(|v| v.as_str()).unwrap_or("Skin").to_string();
        if id.is_empty() {
            continue;
        }
        match place_library_mod(None, &base, &http, &allowed, &id, &nm, &champ, champ_id, "champion_skin", false).await {
            Ok((Some(rec), _summary)) => recs.push((id, rec)),
            Ok((None, summary)) => {
                log_warn!("[LIBRARY] bundle '{champ}' skin '{nm}' blocked by ModScan: {}", summary.verdict);
                blocked.push(json!({ "id": id, "name": nm, "scan": summary }));
            }
            Err(e) => { log_warn!("[LIBRARY] bundle '{champ}' skin '{nm}' skipped: {e}"); failed.push(nm); }
        }
    }
    {
        let mut c = state.config.lock_safe();
        for (id, rec) in &recs {
            c.library.installed.insert(id.clone(), rec.clone());
        }
        let _ = c.save();
    }
    log_info!("[LIBRARY] bundle '{champ}': {} installed, {} skipped, {} blocked", recs.len(), failed.len(), blocked.len());
    Ok(json!({ "champ": champ, "installed": recs.len(), "failed": failed, "blocked": blocked, "installedRecords": c_installed_ids(&recs) }))
}

/// Small helper: the mod ids just installed by a bundle (for the UI to mark).
fn c_installed_ids(recs: &[(String, config::InstalledMod)]) -> Vec<String> {
    recs.iter().map(|(id, _)| id.clone()).collect()
}

/// Remove an installed mod (delete the file + forget the record).
#[tauri::command]
fn library_remove(mod_id: String, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    use skins::slog::log_info;
    let mut c = state.config.lock_safe();
    if let Some(rec) = c.library.installed.remove(&mod_id) {
        // `rec.file` is relative to `mods/`; older builds stored it relative to
        // `mods/skins` or the legacy `library/` dir — try all three.
        let _ = std::fs::remove_file(skins::paths::mods_dir().join(&rec.file));
        let _ = std::fs::remove_file(skins::paths::mods_dir().join("skins").join(&rec.file));
        let _ = std::fs::remove_file(library_mods_dir().join(&rec.file));
        log_info!("[LIBRARY] removed '{}' (was mods/{})", rec.name, rec.file);
    }
    let _ = c.save();
    json!({ "installed": c.library.installed })
}

/// Import the current-patch best runes + summoner spells + item build for your
/// locked champion into the live client, via the runes Worker + the local LCU.
/// Manual trigger for an "Import build" button (`spawn_runes_auto_import` calls
/// the same `runes::import_now` on champ-lock). No Riot Web API key involved.
#[tauri::command]
async fn runes_import_now(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let (enabled, endpoint, sort, allowed) = {
        let c = state.config.lock_safe();
        (c.runes.enabled, c.runes.endpoint.clone(), c.runes.sort.clone(), net::allowed_origins(&c))
    };
    if !enabled {
        return Err("Rune import is turned off — enable it in Settings first.".to_string());
    }
    if endpoint.trim().is_empty() {
        return Err("Rune import isn't set up yet (no build server configured).".to_string());
    }
    let Some(auth) = lcu::cached_auth() else {
        return Err("League client isn't running.".to_string());
    };
    let http = lcu::build_lcu_client(6.0);
    let ext_http = net::build_external_client(6.0, allowed.clone());
    let applied = runes::import_now(&http, &auth, &ext_http, &allowed, &endpoint, &sort).await;
    if !applied.runes && !applied.spells && !applied.items {
        return Err("Couldn't import — be in champ select with a champion picked, and make sure a build exists for it.".to_string());
    }
    Ok(json!({ "runes": applied.runes, "spells": applied.spells, "items": applied.items }))
}

/// Update metadata surfaced to the UI so it can show a themed "update
/// available" pill instead of a forced silent restart. See `updater_install`.
#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    notes: String,
}

/// On startup, check GitHub Releases for a signed newer version and, if one
/// exists, emit `update-available` to the UI. Deliberately does NOT auto-install
/// — the user clicks the in-app pill on their own schedule, so relaunching
/// mid-game never forces downtime. Best-effort: any failure just logs.
async fn run_startup_update_check(app: AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else {
        eprintln!("[update] updater unavailable");
        return;
    };
    match updater.check().await {
        Ok(Some(update)) => {
            eprintln!("[update] newer version {} available", update.version);
            let _ = app.emit(
                "update-available",
                json!({ "version": update.version, "notes": update.body.clone().unwrap_or_default() }),
            );
        }
        Ok(None) => eprintln!("[update] already up to date"),
        Err(e) => eprintln!("[update] check failed: {e}"),
    }
}

/// UI-driven update check (Settings "check for updates" + a boot belt-and-
/// suspenders call in case `update-available` fired before the webview
/// attached its listener). Returns `None` when already up to date.
#[tauri::command]
async fn updater_check(app: AppHandle) -> Option<UpdateInfo> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().ok()?;
    match updater.check().await {
        Ok(Some(update)) => {
            Some(UpdateInfo { version: update.version.clone(), notes: update.body.clone().unwrap_or_default() })
        }
        _ => None,
    }
}

/// Download + install the pending update with progress events, then relaunch.
/// Emits `update-progress` (`{downloaded,total}`) so the UI can render a themed bar.
#[tauri::command]
async fn updater_install(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    use crate::skins::slog::{log_info, log_warn};

    log_info!("[update] user requested install - preparing");
    // Intentionally NOT killing mod-tools: cslol-tools now run from user-data (not
    // the install folder), so the installer never touches them, and
    // `kill_all_modtools_processes` would block on the injection mutex a live
    // in-game overlay holds for the whole match, hanging the update.

    let updater = app.updater().map_err(|e| {
        log_warn!("[update] updater unavailable: {e}");
        e.to_string()
    })?;
    let update = updater
        .check()
        .await
        .map_err(|e| {
            log_warn!("[update] check failed: {e}");
            e.to_string()
        })?
        .ok_or_else(|| {
            log_warn!("[update] install requested but no update available");
            "no update available".to_string()
        })?;
    log_info!("[update] installing {} silently", update.version);

    let downloaded = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let app_progress = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                let d = downloaded.fetch_add(chunk as u64, Ordering::SeqCst) + chunk as u64;
                let _ = app_progress
                    .emit("update-progress", json!({ "downloaded": d, "total": total.unwrap_or(0) }));
            },
            || {},
        )
        .await
        .map_err(|e| {
            log_warn!("[update] download/install failed: {e}");
            e.to_string()
        })?;

    log_info!("[update] installed - relaunching");
    app.restart();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Skins subsystem foundation (data-dir tree + file logger). Non-fatal:
    // the rest of the app must come up even if this fails (e.g. a locked-down
    // profile) — the skins tools just stay unavailable this session.
    if let Err(e) = skins::init() {
        eprintln!("skins::init failed (continuing without skins subsystem): {e}");
    }

    let config = Config::load();
    let mut stats = Stats::load();
    stats.start_session();

    let state = Arc::new(AppState {
        config: Mutex::new(config),
        stats: Mutex::new(stats),
        running: AtomicBool::new(false),
        client_online: AtomicBool::new(false),
        phase: Mutex::new(String::new()),
        readycheck_handled: AtomicBool::new(false),
        ws_active: AtomicBool::new(false),
        auto_range_running: AtomicBool::new(false),
        injection_blocked: AtomicBool::new(false),
        safety: safety_manager::SafetyManager::new(),
        chat_open: AtomicBool::new(false),
        chat_listener_started: AtomicBool::new(false),
        game_focused: AtomicBool::new(false),
        auto_range_gen: AtomicU64::new(0),
        auto_accept_gen: AtomicU64::new(0),
        config_gen: AtomicU64::new(0),
        skins: Arc::new(skins::SkinsState::new()),
        skins_phase: Mutex::new(None),
        skins_bridge: Mutex::new(None),
        skins_injection: Mutex::new(None),
        skins_party: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Second launch -> focus the existing window.
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .register_asynchronous_uri_scheme_protocol("lcu", |_app, request, responder| {
            let path = request.uri().path().trim_start_matches('/').to_string();
            tauri::async_runtime::spawn(async move {
                responder.respond(fetch_lcu_asset(&path).await);
            });
        })
        .manage(state)
        .on_window_event(|window, event| {
            // Close -> minimize to tray instead of exiting (exit via tray/Exit).
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            toggle_tool,
            stop_all,
            set_injection_ack,
            request_admin,
            open_external_url,
            exit_app,
            get_diagnostics,
            get_config,
            save_config,
            get_profile,
            skins_get_state,
            skins_save_settings,
            skins_set_ack,
            skins_open_cslol_dir,
            skins_download,
            skins_activate_pengu,
            skins_set_enabled,
            skins_get_customization,
            skins_set_customization,
            skins_catalog,
            skins_get_favorites,
            skins_set_favorite,
            skins_party_enable,
            skins_party_disable,
            skins_party_add_peer,
            skins_party_get_state,
            skins_party_set_consent,
            skins_party_set_auto_announcers,
            runes_import_now,
            set_appear_offline,
            get_appear_offline,
            library_get,
            set_library_enabled,
            library_catalog_all,
            library_state,
            library_set_favorite,
            library_set_auto_update,
            library_install,
            library_remove,
            library_bundles,
            library_install_bundle,
            modscan_rescan,
            modscan_scan_path,
            modscan_scan_folder,
            announcer_studio_slots,
            announcer_studio_build,
            updater_check,
            updater_install
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let st = app.state::<Arc<AppState>>().inner().clone();
            // Only arm Auto-Accept on launch if the user left it enabled — its
            // on/off state is persisted, so a disable survives a restart.
            if st.config.lock_safe().auto_accept.enabled {
                st.running.store(true, Ordering::SeqCst);
                spawn_auto_accept(&handle, st.clone());
            }

            // Rune/build auto-import watcher (inert until enabled + a Worker
            // endpoint is configured).
            spawn_runes_auto_import(st.clone());
            spawn_appear_offline(st.clone());
            spawn_hash_autorefresh(handle.clone(), st.clone());
            // Anonymous usage heartbeat — no-ops until `telemetry.enabled` (dark).
            telemetry::spawn(handle.clone());

            // Auto-update: silently check GitHub Releases for a signed newer
            // version and surface the pill. Best-effort: any failure just logs
            // and the app runs the current version. Skipped in dev builds.
            if !cfg!(debug_assertions) {
                let update_handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    // Check shortly after launch, then every 10 min while the app
                    // stays open, so a release published mid-session surfaces the
                    // pill without a restart. Cheap: a tiny GitHub CDN fetch.
                    run_startup_update_check(update_handle.clone()).await;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(10 * 60)).await;
                        run_startup_update_check(update_handle.clone()).await;
                    }
                });
            }

            // Skins phase engine: always spawned — it just idles (poll fallback
            // finds no LCU auth, WS fan-out has nothing to send) when there's no
            // client to watch. Cheaper than gating on a settings flag.
            let phase_handle = skins::phase::spawn(handle.clone(), st.skins.clone());

            // Skins bridge server: the local axum server the in-client Pengu
            // Loader plugins connect to. `bridge::spawn` only needs to
            // `subscribe()` the phase actor's events, so it borrows
            // `phase_handle` rather than consuming it — `PhaseHandle` isn't
            // `Clone`, and `skins_phase` below still needs to own it for
            // `lcu_ws.rs`'s fan-out.
            let injection_manager = std::sync::Arc::new(skins::injection::InjectionManager::new(
                skins::injection::tools::cslol_tools_dir(),
                skins::paths::injection_mods_dir(),
                skins::paths::skins_dir(),
                skins::paths::injection_overlay_dir(),
            ));
            // Wire the safety policy hook into the injection pipeline (manager
            // entry, game-suspend watcher, mkoverlay/runoverlay) and start the
            // ALWAYS-RUNNING ranked/queue monitor. Until the hook is set the
            // pipeline fails closed; the monitor keeps the policy's gameflow
            // snapshot fresh (a stale snapshot also denies).
            injection_manager.set_policy_hook(safety_manager::make_policy_hook(st.clone()));
            safety_manager::spawn_safety_monitor(handle.clone(), st.clone());
            // Apply the configured auto-resume safety timeout. Bound to a local
            // FIRST: holding the config guard across this call would invert the
            // config->inner->monitor lock order the policy hook establishes.
            let auto_resume_secs = st.config.lock_safe().skins.monitor_auto_resume_timeout_secs;
            injection_manager.set_auto_resume_timeout(auto_resume_secs);
            // Startup sweep: auto-fix custom mods imported while Chud was closed
            // (scope champion skins, retarget announcer packs). ChampSelect entry
            // re-sweeps for files dropped while running.
            {
                let sweep_app = handle.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    skins::mod_scope::sweep_imported_mods(Some(&sweep_app));
                });
            }
            let bridge_handle = skins::bridge::spawn(
                handle.clone(),
                st.skins.clone(),
                injection_manager.clone(),
                &phase_handle,
            );
            *st.skins_bridge.lock_safe() = Some(bridge_handle.clone());
            // Stash the injection manager so the ticker/trigger can pull it from
            // the app handle at the loadout deadline.
            *st.skins_injection.lock_safe() = Some(injection_manager);

            // Party mode manager: built after the bridge so it can hold a
            // `BridgeHandle` clone to push `party-state` updates the moment the
            // relay's member list changes, not just on request.
            let party_manager = skins::party::manager::PartyManager::new(&handle, st.skins.clone(), bridge_handle);
            *st.skins_party.lock_safe() = Some(party_manager.clone());

            // NO auto-connect: zero relay connections until the user has both
            // turned Party on AND accepted the current disclosure version (see
            // docs/PRIVACY-PARTY.md). `enable()` re-checks consent itself — this
            // is just the startup-resume path for a user who already did.
            let (party_enabled, party_consent_version) = {
                let c = st.config.lock_safe();
                (c.party.enabled, c.party.consent_version)
            };
            if party_enabled && party_consent_version >= skins::party::manager::CURRENT_PARTY_CONSENT_VERSION {
                tauri::async_runtime::spawn(async move {
                    let _ = party_manager.enable().await;
                });
            }

            *st.skins_phase.lock_safe() = Some(phase_handle);

            // No in-game hotkeys by design: the tools are armed/disarmed from
            // the dashboard only and stay always-on while armed, so an
            // accidental keypress mid-game can never silently disarm them.

            // System tray with show/exit + left-click to restore.
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
            let show = MenuItem::with_id(app, "show", "Show Dashboard", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::new()
                .tooltip("Chud")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => {
                        release_held_keys(&app.state::<Arc<AppState>>());
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                });
            // Prefer the dedicated tray glyph; fall back to the app icon.
            if let Some(icon) = tray_icon().or_else(|| app.default_window_icon().cloned()) {
                tray = tray.icon(icon);
            }
            tray.build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Chud");
}
