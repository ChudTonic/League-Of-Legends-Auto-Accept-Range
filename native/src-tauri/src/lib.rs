//! Chud — native League of Legends toolkit (Tauri app).
//!
//! M1 wires the real Rust core behind the Hextech dashboard: config + stats +
//! LCU lockfile auth + an Auto-Accept loop. Auto-Accept is the safe LCU-API
//! core and is the only functional tool this milestone; Auto-Range (M2) and
//! Camera Assist (M3) appear as planned/not-yet-ported. The app operates
//! openly — no anti-cheat evasion.

mod auto_accept;
mod auto_range;
mod camera_assist;
mod config;
mod input;
mod lcu;
mod lcu_ws;
mod profile;
mod safety;
mod skins;
mod stats;
mod vision;
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
    pub camera_running: AtomicBool,
    pub injection_blocked: AtomicBool, // ranked kill-switch (shared by injection tools)
    pub chat_open: AtomicBool,         // in-game chat open -> release the key
    pub chat_listener_started: AtomicBool,
    pub game_focused: AtomicBool,      // published by tool loops; read by the chat hook (no Win32 in the hook)
    pub auto_range_gen: AtomicU64,     // bumped each arm so a stale duplicate loop exits
    pub camera_gen: AtomicU64,
    pub auto_accept_gen: AtomicU64,
    pub config_gen: AtomicU64,         // bumped on save so running loops live-reload settings
    /// Skins subsystem shared state (S2+) — see `docs/SKINS_PORT.md`.
    pub skins: Arc<skins::SkinsState>,
    /// The phase actor's handle, set once during `setup()`. `lcu_ws.rs`
    /// reads `input_tx` to fan events into it; later milestones (bridge S4,
    /// ticker S5) will read `events` to subscribe. `None` only in the brief
    /// window before `setup()` spawns it.
    pub skins_phase: Mutex<Option<skins::phase::PhaseHandle>>,
    /// The skins bridge server's handle (S4), set once during `setup()`.
    /// Later milestones (S5 ticker/trigger, S6 party) hold a clone and call
    /// its `broadcast_*` methods to push state to the Pengu Loader plugins —
    /// see `skins::bridge::broadcast`. `None` only in the brief window
    /// before `setup()` spawns it.
    pub skins_bridge: Mutex<Option<skins::bridge::BridgeHandle>>,
    /// The injection manager (S3), set once during `setup()`. S5's ticker /
    /// trigger pull it from here (via the app handle) to run an injection at
    /// the loadout deadline. `None` only before `setup()` builds it.
    pub skins_injection: Mutex<Option<Arc<skins::injection::InjectionManager>>>,
    /// The party mode manager (S6), set once during `setup()` — after the
    /// bridge, since it holds a `BridgeHandle` clone to push `party-state`
    /// updates proactively (see `skins::party::manager::PartyManager`).
    /// `bridge::handlers`'s party-* handlers pull it from here. `None` only
    /// in the brief window before `setup()` builds it.
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

/// Build the full UI state payload. Same shape for `get_state` and the
/// `state-changed` event so the front-end handles both identically.
fn snapshot(state: &AppState) -> serde_json::Value {
    let running = state.running.load(Ordering::SeqCst);
    let online = state.client_online.load(Ordering::SeqCst);
    let (injection_ack, range_key, recenter_mode) = {
        let c = state.config.lock_safe();
        (c.safety.injection_ack, c.autorange.range_hold_key.to_uppercase(), c.camera.recenter_mode.clone())
    };
    let admin = winutil::is_admin();
    let range_running = state.auto_range_running.load(Ordering::SeqCst);
    let camera_running = state.camera_running.load(Ordering::SeqCst);
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
    let (cam_status, cam_tone) = if camera_running && blocked {
        ("RANKED — BLOCKED", "danger")
    } else if camera_running {
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
        "activeToolCount": (running as i32) + (range_running as i32) + (camera_running as i32),
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
            },
            {
                "id": "camera_assist", "title": "Camera Assist", "safe": false, "requiresAdmin": true,
                "running": camera_running, "statusText": cam_status, "statusTone": cam_tone,
                "subtitle": "Recenters the camera on your champion when you drift; auto-disabled in ranked.",
                "metricLabel": "Recenter", "metricValue": recenter_mode,
                "runtimeCopy": if blocked && camera_running { "Ranked game detected — Camera Assist is disabled to protect your account." }
                               else if camera_running { "Armed. Note: champion detection is a first pass pending live validation." }
                               else { "Auto-recenter the camera while playing unlocked." },
                "primaryActionText": if camera_running { "Stop Tool" } else { "Launch Camera Assist" }
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

/// Arm/disarm Camera Assist, gated like Auto-Range.
fn toggle_camera(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let acked = state.config.lock_safe().safety.injection_ack;
    if !acked || !winutil::is_admin() {
        emit_state(app, &state);
        return;
    }
    if state.camera_running.load(Ordering::SeqCst) {
        state.camera_running.store(false, Ordering::SeqCst);
    } else {
        let generation = state.camera_gen.fetch_add(1, Ordering::SeqCst) + 1;
        state.camera_running.store(true, Ordering::SeqCst);
        camera_assist::start(app.clone(), state.inner().clone(), generation);
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
            if state.running.load(Ordering::SeqCst) {
                state.running.store(false, Ordering::SeqCst); // loop exits on next check
            } else {
                state.running.store(true, Ordering::SeqCst);
                spawn_auto_accept(&app, state.inner().clone());
            }
        }
        "auto_range" => {
            toggle_auto_range(&app);
            return;
        }
        "camera_assist" => {
            toggle_camera(&app);
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
    let client = lcu::build_client(timeout);
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
        Ok(parsed) => {
            {
                let mut c = state.config.lock_safe();
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
    state.camera_running.store(false, Ordering::SeqCst);
    let keys = {
        let c = state.config.lock_safe();
        [c.autorange.range_hold_key.clone(), c.camera.camera_hold_key.clone()]
    };
    for key in keys {
        if let Some(mut injector) = input::Injector::new(&key) {
            injector.force_release();
        }
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
            "cameraAssist": state.camera_running.load(Ordering::SeqCst),
            "injectionBlocked": state.injection_blocked.load(Ordering::SeqCst),
            "injectionAck": cfg.safety.injection_ack,
        },
        "hotkeys": { "autoRange": "none (always-on while armed)", "cameraAssist": "none (always-on while armed)" },
        "config": {
            "rangeHoldKey": cfg.autorange.range_hold_key,
            "cameraHoldKey": cfg.camera.camera_hold_key,
            "recenterMode": cfg.camera.recenter_mode,
            "blockInRanked": cfg.safety.block_in_ranked,
            "checkInterval": cfg.auto_accept.check_interval,
        },
        "paths": {
            "config": config::config_path().to_string_lossy(),
            "data": stats::data_dir().to_string_lossy(),
        }
    })
}

/// Validation aid (M3): capture the screen, run player detection, save the
/// frame + detected candidates to the data dir so detection accuracy can be
/// checked against a live game. Call from the console while in a game:
///   window.__TAURI__.core.invoke('capture_debug_frame').then(console.log)
#[tauri::command]
fn capture_debug_frame() -> serde_json::Value {
    let Some(frame) = vision::capture_primary() else {
        return json!({ "ok": false, "error": "capture failed" });
    };
    let (w, h) = (frame.width(), frame.height());
    let candidates = vision::detect_player_candidates(&frame);
    let dir = stats::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let png = dir.join("camera_debug.png");
    let _ = frame.save(&png);
    let cands: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            json!({
                "box": [c.bar_box.0, c.bar_box.1, c.bar_box.2, c.bar_box.3],
                "anchor": [c.player_anchor.0, c.player_anchor.1],
                "width": c.width, "height": c.height,
                "confidence": c.confidence, "manaBonus": c.mana_bonus
            })
        })
        .collect();
    let _ = std::fs::write(
        dir.join("camera_debug.json"),
        serde_json::to_string_pretty(&cands).unwrap_or_default(),
    );
    json!({
        "ok": true,
        "path": png.to_string_lossy(),
        "frame": [w, h],
        "candidateCount": cands.len(),
        "candidates": cands
    })
}

// ============================================================
// Skins control panel (S9) — see `docs/SKINS_PORT.md` §5. These commands are
// thin wrappers over the already-implemented skins subsystem (S1-S8); none
// of them re-derive logic that already lives in `skins::*`. Party's
// `party-state` broadcast (`PartyManager::broadcast_state`) goes out over
// the in-client bridge WebSocket to the Pengu Loader plugins, NOT to this
// Tauri webview, so there is no push event for party changes here — the
// front-end polls `skins_party_get_state` while the Skins page is open.
// ============================================================

/// Disk/process checks shared by `skins_snapshot` and `skins_diagnostics` —
/// computed once per call so the two never disagree on the same request.
struct SkinsStatusChecks {
    pengu_active: bool,
    skins_downloaded: bool,
    hashes_ready: bool,
    tools_available: bool,
    dll_valid: bool,
}

fn skins_status_checks() -> SkinsStatusChecks {
    let tools_dir = skins::injection::tools::cslol_tools_dir();
    SkinsStatusChecks {
        pengu_active: skins::pengu::is_active(),
        skins_downloaded: skins::downloads::skins_present(&skins::paths::skins_dir()),
        hashes_ready: tools_dir.join("hashes.game.txt").exists(),
        tools_available: skins::injection::tools::check_tools_available(&tools_dir),
        dll_valid: skins::injection::tools::verify_cslol_dll(&tools_dir).is_ok(),
    }
}

fn skins_diagnostics_value(checks: &SkinsStatusChecks, bridge_port: Option<u16>) -> serde_json::Value {
    json!({
        "bridgePort": bridge_port,
        "penguActive": checks.pengu_active,
        "toolsAvailable": checks.tools_available,
        "dllValid": checks.dll_valid,
        "skinsDownloaded": checks.skins_downloaded,
        "hashesReady": checks.hashes_ready,
        "dataDir": skins::paths::data_root().to_string_lossy(),
    })
}

/// The Skins page's full state payload — same "one snapshot fn behind a thin
/// command" shape `snapshot()`/`get_state` already use for the dashboard.
fn skins_snapshot(state: &AppState) -> serde_json::Value {
    let cfg = state.config.lock_safe().skins.clone();
    let bridge_port = state.skins_bridge.lock_safe().as_ref().map(|b| b.port());
    let checks = skins_status_checks();
    let party = match state.skins_party.lock_safe().as_ref() {
        Some(p) => p.get_state(),
        None => json!({ "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [] }),
    };
    json!({
        "enabled": cfg.enabled,
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
/// snake_case, matching `config::SkinsCfg`'s own field names, same contract
/// `save_config` uses for the main config). Live-applies the auto-resume
/// timeout to the running `InjectionManager` (`docs/SKINS_PORT.md`'s
/// reconciliation note) since that field would otherwise need an app
/// restart to take effect.
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
    if let Some(mgr) = state.skins_injection.lock_safe().as_ref() {
        mgr.set_auto_resume_timeout(auto_resume);
    }
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

/// Activate the bundled Pengu Loader against the resolved League install
/// path (the configured `league_path`, falling back to LCU auto-detection
/// via `lcu_ext::resolve_game_dir` — the same "Game" folder `mkoverlay`'s
/// `--game:` flag targets). Blocking (shells out to Pengu Loader's CLI) but
/// Tauri runs non-`async` commands on its own blocking thread pool, so this
/// never stalls the webview.
#[tauri::command]
fn skins_activate_pengu(state: tauri::State<Arc<AppState>>) -> Result<serde_json::Value, String> {
    let configured = state.config.lock_safe().skins.league_path.clone();
    let league_path = if !configured.trim().is_empty() {
        Some(configured)
    } else {
        skins::lcu_ext::resolve_game_dir().map(|p| p.to_string_lossy().into_owned())
    };
    if skins::pengu::activate_on_start(league_path.as_deref()) {
        Ok(json!({ "ok": true, "leaguePath": league_path }))
    } else {
        Err("Failed to activate Pengu Loader — check that it's bundled and the League path is correct.".to_string())
    }
}

/// Persist the Skins master enable/disable flag. Advisory only this
/// milestone: it is not yet wired to gate the phase actor/bridge/ticker
/// themselves (those already run unconditionally per `docs/SKINS_PORT.md`'s
/// "always spawned, just idles" note) — deeper subsystem gating is future
/// work; this just persists the flag and reflects it back in state.
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

#[tauri::command]
fn skins_diagnostics(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let bridge_port = state.skins_bridge.lock_safe().as_ref().map(|b| b.port());
    let checks = skins_status_checks();
    skins_diagnostics_value(&checks, bridge_port)
}

#[tauri::command]
async fn skins_party_enable(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else { return Err("Skins subsystem not ready yet".to_string()) };
    party.enable().await?;
    Ok(party.get_state())
}

#[tauri::command]
async fn skins_party_disable(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let party = { state.skins_party.lock_safe().clone() };
    let Some(party) = party else {
        return Ok(json!({ "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [] }));
    };
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
        None => json!({ "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [] }),
    }
}

/// Update metadata surfaced to the UI so it can show a themed "update
/// available" pill instead of a forced silent restart. See `updater_install`.
#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    notes: String,
}

/// On startup, check GitHub Releases for a signed newer version and, if one
/// exists, emit `update-available` to the UI. We deliberately do NOT auto-
/// install on launch anymore — the user clicks the in-app pill to update on
/// their own schedule, so relaunching mid-game never forces downtime.
/// Best-effort: any failure just logs and the current version keeps running.
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
/// First kills any lingering `mod-tools.exe`/runoverlay processes: they hold
/// `cslol-tools\mod-tools.exe` open, and the NSIS installer (silent or manual)
/// fails with "Error opening file for writing" if it can't overwrite them —
/// the exact error a stale overlay from an earlier game causes. Emits
/// `update-progress` ({downloaded,total}) so the UI can render a themed bar.
#[tauri::command]
async fn updater_install(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;

    use crate::skins::slog::{log_info, log_warn};

    log_info!("[update] user requested install - preparing");
    // Belt-and-suspenders: kill lingering mod-tools/runoverlay. (As of the
    // cslol-tools relocation these run from user-data, not the install folder,
    // so the installer no longer needs them unlocked — but clearing them is
    // still cheap insurance.)
    let injection = app.state::<Arc<AppState>>().skins_injection.lock_safe().clone();
    if let Some(inj) = injection {
        inj.kill_all_modtools_processes();
    }

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
        camera_running: AtomicBool::new(false),
        injection_blocked: AtomicBool::new(false),
        chat_open: AtomicBool::new(false),
        chat_listener_started: AtomicBool::new(false),
        game_focused: AtomicBool::new(false),
        auto_range_gen: AtomicU64::new(0),
        camera_gen: AtomicU64::new(0),
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
            exit_app,
            capture_debug_frame,
            get_diagnostics,
            get_config,
            save_config,
            get_profile,
            skins_get_state,
            skins_save_settings,
            skins_download,
            skins_activate_pengu,
            skins_set_enabled,
            skins_diagnostics,
            skins_party_enable,
            skins_party_disable,
            skins_party_add_peer,
            skins_party_get_state,
            updater_check,
            updater_install
        ])
        .setup(|app| {
            // Auto-start Auto-Accept (matches the Python app's auto_start_main).
            let handle = app.handle().clone();
            let st = app.state::<Arc<AppState>>().inner().clone();
            st.running.store(true, Ordering::SeqCst);
            spawn_auto_accept(&handle, st.clone());

            // Auto-update: on startup, silently check GitHub Releases for a
            // signed newer version, install it, and relaunch. This is what lets
            // users (e.g. a friend/family member) stop swapping the exe by hand.
            // Best-effort: any failure just logs and the app runs the current
            // version. `cfg!(debug_assertions)` skips it in dev builds.
            if !cfg!(debug_assertions) {
                let update_handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    // Check once shortly after launch, then every 30 min while the
                    // app stays open — so a release published mid-session surfaces
                    // the "update available" pill without needing a restart.
                    run_startup_update_check(update_handle.clone()).await;
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
                        run_startup_update_check(update_handle.clone()).await;
                    }
                });
            }

            // Skins phase engine (S2): always spawned — it just idles (poll
            // fallback finds no LCU auth, WS fan-out has nothing to send)
            // when the skins subsystem has no client to watch. Cheaper than
            // gating on a not-yet-existent settings flag and respawning later.
            let phase_handle = skins::phase::spawn(handle.clone(), st.skins.clone());

            // Skins bridge server (S4): the local axum server the in-client
            // Pengu Loader plugins connect to. `InjectionManager` is
            // constructed here (nothing else in the app owns one yet) with
            // the standard bundled-tools/injection-tree paths; `set_game_dir`
            // is left unset this milestone (S5's game-flow wiring resolves
            // the League install directory and calls it). `bridge::spawn`
            // only needs to `subscribe()` the phase actor's events (a
            // `&self` method), so it borrows `phase_handle` rather than
            // consuming it — `PhaseHandle` isn't `Clone`, and `skins_phase`
            // below still needs to own it for `lcu_ws.rs`'s fan-out.
            let injection_manager = std::sync::Arc::new(skins::injection::InjectionManager::new(
                skins::injection::tools::cslol_tools_dir(),
                skins::paths::injection_mods_dir(),
                skins::paths::skins_dir(),
                skins::paths::injection_overlay_dir(),
            ));
            // Apply the configured auto-resume safety timeout (defaults to
            // `GameMonitor`'s own 60s default; clamped 1..=180s either way).
            injection_manager
                .set_auto_resume_timeout(st.config.lock_safe().skins.monitor_auto_resume_timeout_secs);
            let bridge_handle = skins::bridge::spawn(
                handle.clone(),
                st.skins.clone(),
                injection_manager.clone(),
                &phase_handle,
            );
            *st.skins_bridge.lock_safe() = Some(bridge_handle.clone());
            // Stash the injection manager so S5's ticker/trigger can pull it
            // from the app handle at the loadout deadline.
            *st.skins_injection.lock_safe() = Some(injection_manager);

            // Party mode manager (S6): built after the bridge so it can hold
            // a `BridgeHandle` clone to push `party-state` updates the
            // moment the relay's member list changes, not just on request
            // (see `PartyManager::handle_members_update`).
            let party_manager = skins::party::manager::PartyManager::new(&handle, st.skins.clone(), bridge_handle);
            *st.skins_party.lock_safe() = Some(party_manager.clone());

            // Seamless party: auto-enable at startup so there's no button to
            // press. `enable()` retries until the LCU is reachable, then the
            // auto-room loop joins the shared lobby room whenever you're in a
            // lobby — party members converge with zero tokens/clicks. Idles
            // harmlessly (personal room, no peers) when solo.
            tauri::async_runtime::spawn(async move {
                let _ = party_manager.enable().await;
            });

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
