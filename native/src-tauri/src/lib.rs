//! Chud — native League of Legends toolkit (Tauri app).
//!
//! Chud wires the real Rust core behind the Hextech dashboard: config + stats +
//! LCU lockfile auth + an Auto-Accept loop, plus Auto-Range and the skins
//! subsystem. The app operates openly — no anti-cheat evasion.

mod advisory;
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
    /// The injection manager, set once during `setup()`; pulled from here (via
    /// the app handle) to run an injection at the loadout deadline. `None` only
    /// before `setup()` builds it.
    pub skins_injection: Mutex<Option<Arc<skins::injection::InjectionManager>>>,
    /// The party mode manager, set once during `setup()`. `None` only before
    /// `setup()` builds it.
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
                // or the Skins page (`skins_save_settings`). A blind `*c = parsed`
                // would silently revert those. Preserve the dedicated-command
                // sections from the live config so a general save never clobbers them.
                parsed.library = c.library.clone();
                parsed.presence = c.presence.clone();
                parsed.skins = c.skins.clone();
                // Also owned by dedicated commands: the risk-ack (`set_injection_ack`,
                // also fired from the dashboard), the skins consent version, all of
                // party (`skins_party_*`), and the Auto-Accept arm toggle
                // (`toggle_tool`). A stale general-settings snapshot must not revert
                // these. block_in_ranked / auto_accept tunables stay owned by this page.
                parsed.safety.injection_ack = c.safety.injection_ack;
                parsed.safety.skins_ack_version = c.safety.skins_ack_version;
                parsed.party = c.party.clone();
                parsed.auto_accept.enabled = c.auto_accept.enabled;
                // Same clamp `Config::load` applies, so a bad interval can't be
                // persisted (a stale/oversized monitor interval fails injection
                // closed and would otherwise wedge the safety monitor).
                parsed.clamp_intervals();
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
    skins::injection::process::kill_all_modtools_processes_os();
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

/// The newest `chud_skins_*.log` in `skins::paths::logs_dir()`, by mtime —
/// `slog::init` rolls a fresh file per launch, so "newest" is "current run".
fn newest_log_file() -> Option<std::path::PathBuf> {
    let dir = skins::paths::logs_dir();
    std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("chud_skins_") && name.ends_with(".log")
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH))
        .map(|e| e.path())
}

/// Last `max_bytes` of a log file (whole file if smaller) — enough recent
/// context for a bug report without shipping a multi-MB log.
fn tail_log(path: &std::path::Path, max_bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max_bytes {
        let _ = f.seek(SeekFrom::Start(len - max_bytes));
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Send a user bug report (free-text description + the current run's log
/// tail) to the relay. No auth beyond the host allowlist — same relay the
/// Skins party feature already talks to.
#[tauri::command]
async fn submit_bug_report(description: String) -> Result<(), String> {
    if description.trim().is_empty() {
        return Err("Please describe the issue.".to_string());
    }
    let log = newest_log_file().map(|p| tail_log(&p, 200 * 1024)).unwrap_or_default();
    let payload = json!({
        "id": telemetry::daily_id(),
        "version": env!("CARGO_PKG_VERSION"),
        "description": description,
        "log": log,
    });
    let allowed = net::built_in_allowed_origins();
    let client = net::build_external_client(20.0, allowed.clone());
    let resp = client
        .post("https://chud-party-relay.jivy26.workers.dev/bug-report")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("server returned {}", resp.status()))
    }
}

// ============================================================
// Skins control panel — see `docs/SKINS_PORT.md` §5. These commands are thin
// wrappers over the skins subsystem; none re-derive logic that lives in
// `skins::*`. Party state has no push event to the Tauri webview — the
// front-end polls `skins_party_get_state`.
// ============================================================

/// Disk/process checks shared by `skins_snapshot` and `skins_diagnostics` —
/// computed once per call so the two never disagree on the same request.
struct SkinsStatusChecks {
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

fn skins_diagnostics_value(checks: &SkinsStatusChecks) -> serde_json::Value {
    json!({
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
    let (cfg, ack_version, party_cfg) = {
        let c = state.config.lock_safe();
        (c.skins.clone(), c.safety.skins_ack_version, c.party.clone())
    };
    // Current champ-select champion (locked wins over hover) + the active
    // per-game pick — so the app/overlay picker can show "pick a skin for THIS
    // game" without any client injection.
    let (current_champ, current_pick, current_chroma, current_random, current_custom, historic_on, cat_map, cat_font, cat_announcer, cat_others, historic_restored) = {
        let s = state.skins.shared.lock_safe();
        (
            s.locked_champ_id.or(s.hovered_champ_id),
            s.last_hovered_skin_id,
            s.selected_chroma_id,
            if s.random_mode_active { s.random_skin_id } else { None },
            s.selected_custom_mod.as_ref().map(|m| m.mod_name.clone()),
            s.historic_enabled,
            // Only echo a slot if its mod file still exists on disk — a mod
            // deleted out from under the app must not leave a ghost selection
            // the UI can't clear (read-only check: nothing is mutated/saved here).
            s.category_mods.map.as_ref().filter(|m| std::path::Path::new(&m.mod_path).exists()).map(|m| m.mod_name.clone()),
            s.category_mods.font.as_ref().filter(|m| std::path::Path::new(&m.mod_path).exists()).map(|m| m.mod_name.clone()),
            s.category_mods.announcer.as_ref().filter(|m| std::path::Path::new(&m.mod_path).exists()).map(|m| m.mod_name.clone()),
            s.category_mods.others.iter().filter(|m| std::path::Path::new(&m.mod_path).exists()).map(|m| m.relative_path.clone()).collect::<Vec<_>>(),
            // Skin id historic actually restored this game (for the overlay to
            // highlight + show its "restored your last pick" banner).
            if s.historic_mode_active {
                match &s.historic_selection {
                    Some(skins::state::HistoricSelection::SkinId(id)) => Some(*id),
                    _ => None,
                }
            } else {
                None
            },
        )
    };
    let checks = skins_status_checks();
    let party = match state.skins_party.lock_safe().as_ref() {
        Some(p) => p.get_state(),
        None => json!({
            "enabled": false, "my_token": null, "my_summoner_id": null, "my_summoner_name": "Unknown", "peers": [],
            "consent_ok": party_cfg.consent_version >= skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
            "consent_required_version": skins::party::manager::CURRENT_PARTY_CONSENT_VERSION,
            "auto_download_peer_announcers": party_cfg.auto_download_peer_announcers,
            "auto_download_peer_custom_mods": party_cfg.auto_download_peer_custom_mods,
        }),
    };
    json!({
        "enabled": cfg.enabled,
        "overlayCardCols": cfg.overlay_card_cols,
        // Versioned backend consent (P0-A): the UI unlocks skins actions only
        // off ackOk, and the backend policy enforces it regardless.
        "ackOk": ack_version >= safety_manager::CURRENT_SKINS_ACK_VERSION,
        "ackVersion": ack_version,
        "ackRequiredVersion": safety_manager::CURRENT_SKINS_ACK_VERSION,
        "policy": injection_policy_json(state),
        "skinsDownloaded": checks.skins_downloaded,
        "hashesReady": checks.hashes_ready,
        "leaguePath": cfg.league_path,
        "injectionThresholdMs": cfg.injection_threshold_ms,
        "autoResumeSecs": cfg.monitor_auto_resume_timeout_secs,
        "autoDownload": cfg.auto_download_skins,
        "loadscreenLabels": cfg.loadscreen_labels,
        "party": party,
        "currentChampId": current_champ,
        "currentPickSkinId": current_pick,
        "currentChromaId": current_chroma,
        "currentRandomSkinId": current_random,
        "currentCustomMod": current_custom,
        "historicEnabled": historic_on,
        "historicRestoredSkinId": historic_restored,
        "categoryMods": { "map": cat_map, "font": cat_font, "announcer": cat_announcer, "others": cat_others },
        "diagnostics": skins_diagnostics_value(&checks),
    })
}

#[tauri::command]
fn skins_get_state(state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    skins_snapshot(&state)
}

/// Persist the overlay skin-grid column count (1 = large cards … 3 = small).
/// Clamped to 1..=3 so a bad value can't break the grid layout.
#[tauri::command]
fn skins_set_overlay_card_cols(cols: u8, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    let cols = cols.clamp(1, 3);
    {
        let mut cfg = state.config.lock_safe();
        cfg.skins.overlay_card_cols = cols;
        let _ = cfg.save();
    }
    state.config_gen.fetch_add(1, Ordering::SeqCst);
    json!({ "cols": cols })
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
        if let Some(v) = settings.get("loadscreen_labels").and_then(|v| v.as_bool()) {
            cfg.skins.loadscreen_labels = v;
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

/// Persist the Skins master enable/disable flag. Enforced: the safety policy
/// denies every injection entrypoint with `DISABLED` while this is off — the
/// phase actor/ticker still run and idle, but nothing they trigger can execute.
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

/// Is a live LCU champ-select PATCH (force skin/chroma selection) permitted right
/// now? The overlay pick/preview commands write to the League client, so they must
/// clear the same P0-A safety gate every injection side effect does — otherwise a
/// disabled master switch, missing consent, or the ranked kill-switch could still
/// be bypassed by a swatch click or chroma hover.
fn lcu_patch_allowed(state: &AppState) -> bool {
    matches!(
        safety_manager::evaluate_injection_policy(state, safety_manager::InjectionOp::LcuPatch),
        safety_manager::InjectionDecision::Allowed(_)
    )
}

/// Manual per-game skin pick from the Chud app/overlay — the injection-free
/// replacement for the old in-client wheel's hover message. Sets the
/// `last_hovered_*` fields by skin ID directly (no DOM scrape / fuzzy name
/// match). `chroma_id` optionally sets the chroma; `skin_name` is display-only.
/// The loadout ticker reads these at the injection deadline exactly as before.
#[tauri::command]
async fn skins_pick_skin(
    skin_id: i64,
    chroma_id: Option<i64>,
    skin_name: Option<String>,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    use skins::slog::log_info;
    log_info!("[PICK] commit skin_id={skin_id} chroma_id={chroma_id:?}");
    let local_cell_id = {
        let mut shared = state.skins.shared.lock_safe();
        shared.last_hovered_skin_id = Some(skin_id);
        shared.last_hovered_skin_slug = Some(format!("skin_{skin_id}"));
        shared.last_hovered_skin_key = skin_name.or_else(|| Some(format!("skin_{skin_id}")));
        shared.selected_chroma_id = chroma_id;
        // A manual pick overrides historic mode this game — historic has top
        // priority in `resolve_injection_name`, so without this the manual choice
        // would be silently ignored while the toggle stays on for next game.
        shared.historic_mode_active = false;
        shared.historic_selection = None;
        // Picking a normal skin/chroma supersedes an active custom mod — without
        // this, a custom mod picked earlier stays selected and the injector
        // forces this skin while overlaying the (different) custom mod, so the
        // mod silently no-ops. Last pick wins.
        shared.selected_custom_mod = None;
        shared.manual_pick_this_session = true;
        shared.local_cell_id
    };
    // Live-preview the pick in the League champ-select 3D model so the user sees
    // the exact skin/chroma colour before locking. Owned skins/chromas only — the
    // client can't preview unowned content, so the PATCH just no-ops there while
    // in-game injection still applies. Gated by the safety policy: no live client
    // write when skins are disabled / consent missing / ranked kill-switch active.
    if lcu_patch_allowed(state.inner()) {
        if let Some(auth) = lcu::cached_auth() {
            let client = lcu::build_lcu_client(4.0);
            let target = chroma_id.unwrap_or(skin_id);
            skins::trigger::force_skin_via_lcu(&client, &auth, local_cell_id, target).await;
        }
    } else {
        log_info!("[SAFETY] pick LCU preview skipped — injection policy denied");
    }
    Ok(())
}

/// Preview-only: force the champ-select 3D model to a skin/chroma WITHOUT
/// committing the per-game pick, so the overlay can live-preview on hover as the
/// user sweeps across chroma swatches. Owned content only (client no-ops unowned).
#[tauri::command]
async fn skins_preview_skin(
    skin_id: i64,
    chroma_id: Option<i64>,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    use skins::slog::log_info;
    log_info!("[PREVIEW] hover skin_id={skin_id} chroma_id={chroma_id:?}");
    if !lcu_patch_allowed(state.inner()) {
        return Ok(()); // safety gate: no live client write when injection is disallowed
    }
    let local_cell_id = { state.skins.shared.lock_safe().local_cell_id };
    if let Some(auth) = lcu::cached_auth() {
        let client = lcu::build_lcu_client(4.0);
        let target = chroma_id.unwrap_or(skin_id);
        skins::trigger::force_skin_via_lcu(&client, &auth, local_cell_id, target).await;
    }
    Ok(())
}

/// Clear a manual per-game pick, falling back to the champion's favorite (or
/// base). Mirrors the wheel's dismiss path.
#[tauri::command]
fn skins_clear_pick(state: tauri::State<Arc<AppState>>) {
    let mut shared = state.skins.shared.lock_safe();
    shared.last_hovered_skin_id = None;
    shared.last_hovered_skin_slug = None;
    shared.last_hovered_skin_key = None;
    shared.selected_chroma_id = None;
    shared.selected_form_path = None;
    // Also cancel a historic restore for this game (the overlay's "Undo" on the
    // historic banner routes here); the `historic_enabled` toggle stays on so
    // the next champ still restores.
    shared.historic_mode_active = false;
    shared.historic_selection = None;
}

/// List a champion's full skin + chroma catalog (owned AND unowned) from the
/// LCU, each tagged `owned`. This is the data the Chud-app/overlay skin picker
/// renders — the injection-free replacement for the wheel scraping the client's
/// champ-select carousel.
#[tauri::command]
async fn skins_list_champion_skins(
    champion_id: i64,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    let Some(auth) = lcu::cached_auth() else {
        return Err("League client not connected".into());
    };
    let client = lcu::build_lcu_client(6.0);
    let cache = match skins::lcu_ext::scrape_champion_skins(&client, &auth, champion_id).await {
        Some(c) => c,
        None => {
            lcu::invalidate_auth();
            return Err("Could not load skins for this champion".into());
        }
    };
    let owned = state.skins.shared.lock_safe().owned_skin_ids.clone();
    let skins_dir = skins::paths::skins_dir();
    let skins: Vec<serde_json::Value> = cache
        .skins
        .iter()
        .map(|s| {
            // `downloaded` mirrors `favorites::catalog` — an unowned skin needs
            // its ZIP on disk to inject; owned skins are forced natively via LCU.
            let downloaded = skins_dir.join(champion_id.to_string()).join(s.skin_id.to_string()).exists();
            json!({
                "skinId": s.skin_id,
                "skinName": s.skin_name,
                "owned": owned.contains(&s.skin_id),
                "downloaded": downloaded,
                "chromas": s.chroma_details.iter().map(|c| json!({"id": c.id, "name": c.name, "colors": c.colors})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "championId": cache.champion_id,
        "championName": cache.champion_name,
        "skins": skins,
    }))
}

/// Roll a random skin for the champion — restricted to skins the user can
/// actually inject (owned → native LCU, or downloaded → cslol), and each skin's
/// chroma pool trimmed the same way, so the dice never lands on something with
/// no .fantome. Sets random mode; the engine injects the roll at game start.
#[tauri::command]
async fn skins_roll_random(
    champion_id: i64,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    use skins::slog::log_info;
    let Some(auth) = lcu::cached_auth() else {
        return Err("League client not connected".into());
    };
    let client = lcu::build_lcu_client(6.0);
    let mut cache = match skins::lcu_ext::scrape_champion_skins(&client, &auth, champion_id).await {
        Some(c) => c,
        None => {
            lcu::invalidate_auth();
            return Err("Could not load skins for this champion".into());
        }
    };
    let owned = state.skins.shared.lock_safe().owned_skin_ids.clone();
    let skins_dir = skins::paths::skins_dir();
    let champ_root = skins_dir.join(champion_id.to_string());
    let injectable = |skin_id: i64, chroma: Option<i64>| -> bool {
        owned.contains(&skin_id)
            || match chroma {
                Some(c) => champ_root.join(skin_id.to_string()).join(c.to_string()).exists(),
                None => champ_root.join(skin_id.to_string()).exists(),
            }
    };
    cache.skins.retain(|s| injectable(s.skin_id, None));
    for s in &mut cache.skins {
        let sid = s.skin_id;
        s.chroma_details.retain(|c| owned.contains(&c.id) || champ_root.join(sid.to_string()).join(c.id.to_string()).exists());
    }
    let (name, id) = {
        let mut shared = state.skins.shared.lock_safe();
        if skins::features::random::start_randomization(&mut shared, &cache) {
            shared.manual_pick_this_session = true; // a roll is a manual choice too
            (shared.random_skin_name.clone(), shared.random_skin_id)
        } else {
            (None, None)
        }
    };
    log_info!("[RANDOM] roll champ={champion_id} pool={} -> id={id:?} name={name:?}", cache.skins.len());
    match id {
        Some(i) => Ok(json!({ "skinId": i, "skinName": name })),
        None => Err("No injectable skins to roll — download or own some for this champion first".into()),
    }
}

/// Turn random mode off (also clears any rolled skin).
#[tauri::command]
fn skins_cancel_random(state: tauri::State<Arc<AppState>>) {
    use skins::slog::log_info;
    log_info!("[RANDOM] cancel");
    skins::features::random::cancel_randomization(&mut state.skins.shared.lock_safe());
}

/// List the user's own skin mods (`.fantome`/`.zip`/folder) for a champion,
/// from `%LOCALAPPDATA%\Chud\mods\skins\<skinId>\`.
#[tauri::command]
fn skins_list_custom_mods(champion_id: i64) -> Result<serde_json::Value, String> {
    use skins::slog::log_info;
    let storage = skins::injection::storage::ModStorageService::new(skins::paths::mods_dir());
    let root = storage.mods_root().to_path_buf();
    let entries = storage.list_mods_for_champion(champion_id);
    log_info!("[CUSTOM] list champ={champion_id} -> {} mod(s)", entries.len());
    let mods: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            let rel = e.path.strip_prefix(&root).unwrap_or(&e.path).to_string_lossy().replace('\\', "/");
            json!({
                "skinId": e.skin_id,
                "modName": e.mod_name,
                "relativePath": rel,
                "description": e.description,
                "hasPreview": custom_mod_preview_path(&e.path).is_some(),
            })
        })
        .collect();
    Ok(json!({ "championId": champion_id, "mods": mods }))
}

/// Find a preview image for a custom mod: a `.png/.jpg/.jpeg/.webp` sitting next
/// to a single-file mod (same stem) or inside a mod folder (`preview.*` or any
/// root-level image). `.fantome` files carry no image, so this is the only way
/// to show a custom skin's real look — the user drops the image beside the mod.
fn custom_mod_preview_path(mod_path: &std::path::Path) -> Option<std::path::PathBuf> {
    const EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
    let is_img = |p: &std::path::Path| {
        p.extension()
            .and_then(|x| x.to_str())
            .map(|x| EXTS.contains(&x.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
    };
    if mod_path.is_dir() {
        for e in EXTS {
            let p = mod_path.join(format!("preview.{e}"));
            if p.exists() {
                return Some(p);
            }
        }
        std::fs::read_dir(mod_path).ok()?.flatten().map(|d| d.path()).find(|p| is_img(p))
    } else {
        EXTS.iter().map(|e| mod_path.with_extension(e)).find(|p| p.exists())
    }
}

/// Return a custom mod's sidecar preview image as a `data:` URL (base64), or
/// `null` if it has none / is too large. Called on hover, cached client-side.
#[tauri::command]
fn skins_custom_mod_preview(champion_id: i64, mod_id: String) -> Result<Option<String>, String> {
    use base64::Engine as _;
    let storage = skins::injection::storage::ModStorageService::new(skins::paths::mods_dir());
    let root = storage.mods_root().to_path_buf();
    let rel_of = |p: &std::path::Path| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
    let Some(entry) = storage
        .list_mods_for_champion(champion_id)
        .into_iter()
        .find(|e| e.mod_name == mod_id || rel_of(&e.path) == mod_id)
    else {
        return Ok(None);
    };
    let Some(img) = custom_mod_preview_path(&entry.path) else { return Ok(None) };
    let meta = std::fs::metadata(&img).map_err(|e| e.to_string())?;
    if meta.len() > 6_000_000 {
        return Ok(None); // too big to inline as a data URL
    }
    let bytes = std::fs::read(&img).map_err(|e| e.to_string())?;
    let mime = match img.extension().and_then(|x| x.to_str()).map(|x| x.to_ascii_lowercase()).as_deref() {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(Some(format!("data:{mime};base64,{b64}")))
}

/// Select one of the user's custom skin mods for this game — extracts it into
/// the injection staging dir (same as the old bridge path) and records the pick.
#[tauri::command]
async fn skins_pick_custom_mod(
    champion_id: i64,
    mod_id: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    use skins::slog::log_info;
    let storage = skins::injection::storage::ModStorageService::new(skins::paths::mods_dir());
    let root = storage.mods_root().to_path_buf();
    let rel_of = |p: &std::path::Path| p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/");
    let Some(entry) = storage
        .list_mods_for_champion(champion_id)
        .into_iter()
        .find(|e| e.mod_name == mod_id || rel_of(&e.path) == mod_id)
    else {
        return Err("Custom mod not found".into());
    };

    let source = entry.path.clone();
    let mod_folder_name = if source.is_dir() { source.file_name() } else { source.file_stem() }
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mod".to_string());
    let dest = skins::paths::injection_mods_dir().join(&mod_folder_name);
    if dest.exists() {
        skins::injection::zips::safe_remove_entry(&dest);
    }
    let cache_dir = skins::paths::injection_extract_cache_dir();
    if let Err(e) = skins::injection::zips::link_or_extract(&source, &dest, &cache_dir) {
        return Err(format!("Failed to extract custom mod: {e}"));
    }
    let relative_path = rel_of(&source);
    {
        let mut shared = state.skins.shared.lock_safe();
        if shared.historic_mode_active {
            shared.historic_mode_active = false;
            shared.historic_selection = None;
        }
        shared.manual_pick_this_session = true;
        shared.selected_custom_mod = Some(skins::state::CustomModSelection {
            skin_id: entry.skin_id,
            champion_id,
            mod_name: mod_folder_name.clone(),
            mod_path: source.to_string_lossy().into_owned(),
            relative_path,
        });
    }
    log_info!("[CUSTOM] pick champ={champion_id} skin={} mod={mod_folder_name}", entry.skin_id);

    // Surface the chroma slots the mod's WAD chunks target so the overlay can
    // offer a swatch row (e.g. a chroma-VFX pack covering six chroma bins) —
    // the injector loads whichever one the user picks via
    // `skins_set_custom_mod_chroma`.
    let mut chroma_slots: Vec<serde_json::Value> = Vec::new();
    if let Some(auth) = lcu::cached_auth() {
        let client = lcu::build_lcu_client(4.0);
        let detection = skins::injection::target_detect::detect_target_skin(&source, champion_id, &client, &auth).await;
        if let Some(det) = detection.filter(|d| !d.via_name) {
            if let Some(cache) = skins::lcu_ext::scrape_champion_skins(&client, &auth, champion_id).await {
                chroma_slots = det
                    .slots
                    .iter()
                    .filter_map(|s| cache.chroma_id_map.get(s))
                    .map(|c| json!({ "id": c.id, "name": c.name, "colors": c.colors }))
                    .collect();
            }
        }
    }
    Ok(json!({ "ok": true, "modName": mod_folder_name, "skinId": entry.skin_id, "chromaSlots": chroma_slots }))
}

/// Pick which of a custom mod's target chromas to load, WITHOUT dropping the
/// mod selection (the normal `skins_pick_skin` supersedes the mod — last pick
/// wins — so the mod's own swatch row needs this dedicated path).
#[tauri::command]
fn skins_set_custom_mod_chroma(chroma_id: Option<i64>, state: tauri::State<Arc<AppState>>) {
    use skins::slog::log_info;
    log_info!("[CUSTOM] chroma pick {chroma_id:?}");
    state.skins.shared.lock_safe().selected_chroma_id = chroma_id;
}

/// Clear a selected custom mod (falls back to the normal skin/favorite path).
#[tauri::command]
fn skins_clear_custom_mod(state: tauri::State<Arc<AppState>>) {
    use skins::slog::log_info;
    log_info!("[CUSTOM] clear");
    state.skins.shared.lock_safe().selected_custom_mod = None;
}

/// Resolve a local custom mod's name to its Library (R2) preview image, cached
/// to disk so repeat hovers never hit the network. A `.fantome` carries no image,
/// but mods from the Library have a preview on R2 — matched via the `chud-skins`
/// catalog. Returns a `data:` URL (from `%LOCALAPPDATA%\Chud\thumbs\` on a hit,
/// or freshly downloaded + cached on a miss); `null` if no catalog match.
#[tauri::command]
async fn skins_custom_mod_thumb(
    champion_id: i64,
    mod_name: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<Option<String>, String> {
    use base64::Engine as _;
    use skins::slog::log_info;

    let cache_dir = skins::paths::skins_dir().parent().map(|p| p.join("thumbs")).ok_or("no cache dir")?;
    let safe: String = mod_name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    let to_data_url = |bytes: &[u8], ext: &str| {
        let mime = match ext { "jpg" | "jpeg" => "image/jpeg", "webp" => "image/webp", _ => "image/png" };
        format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
    };

    // Disk-cache hit — no network at all.
    for ext in ["png", "jpg", "jpeg", "webp"] {
        if let Ok(bytes) = std::fs::read(cache_dir.join(format!("{safe}.{ext}"))) {
            return Ok(Some(to_data_url(&bytes, ext)));
        }
    }

    // Miss: resolve name -> R2 thumb URL via the catalog, then download + cache.
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let http = net::build_external_client(15.0, allowed.clone());
    let mut u = reqwest::Url::parse(&format!("{}/catalog", endpoint.trim_end_matches('/'))).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("champion", &champion_id.to_string())
        .append_pair("search", &mod_name)
        .append_pair("pageSize", "60");
    let val = net::get_json_checked(&http, u.as_str(), &allowed, 4 * 1024 * 1024).await?;
    let want = mod_name.trim().to_lowercase();
    let name_of = |m: &serde_json::Value| m.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().to_lowercase();
    let mods = val.get("mods").and_then(|m| m.as_array()).cloned().unwrap_or_default();
    let thumb_url = mods
        .iter()
        .find(|m| name_of(m) == want)
        .or_else(|| mods.iter().find(|m| { let n = name_of(m); !n.is_empty() && (n.contains(&want) || want.contains(&n)) }))
        .and_then(|m| m.get("thumb").and_then(|t| t.as_str()).map(String::from));
    let Some(thumb_url) = thumb_url else {
        log_info!("[CUSTOM] thumb champ={champion_id} name='{mod_name}' -> no catalog match");
        return Ok(None);
    };
    let ext = thumb_url.rsplit('.').next().map(|e| e.to_ascii_lowercase()).filter(|e| ["png", "jpg", "jpeg", "webp"].contains(&e.as_str())).unwrap_or_else(|| "png".into());
    let bytes = net::get_bytes_checked(&http, &thumb_url, &allowed, 8 * 1024 * 1024).await?;
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(cache_dir.join(format!("{safe}.{ext}")), &bytes);
    log_info!("[CUSTOM] thumb champ={champion_id} name='{mod_name}' -> cached {} bytes", bytes.len());
    Ok(Some(to_data_url(&bytes, &ext)))
}

// ── Category mods (maps / fonts / announcers / ui / vfx / sfx / voiceover /
// loading_screen / others) — champion-independent global mods, picked from the
// overlay. map/font/announcer are single-select slots; everything else stacks
// in the `others` bucket. The engine already injects `category_mods`. ──────────

/// List the user's installed mods for a category (from `%LOCALAPPDATA%\Chud\mods\<category>\`).
#[tauri::command]
fn skins_list_category_mods(category: String) -> Result<serde_json::Value, String> {
    use skins::slog::log_info;
    let storage = skins::injection::storage::ModStorageService::new(skins::paths::mods_dir());
    let entries = storage.list_mods_for_category(&category);
    log_info!("[CATEGORY] list {category} -> {} mod(s)", entries.len());
    let mods: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| json!({ "id": e.id, "name": e.name, "relativePath": e.path, "description": e.description }))
        .collect();
    Ok(json!({ "category": category, "mods": mods }))
}

/// Select a category mod for this game. Single-select for map/font/announcer;
/// all other categories stack in `others` (multi-select).
#[tauri::command]
fn skins_pick_category_mod(
    category: String,
    mod_id: String,
    state: tauri::State<Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    use skins::slog::log_info;
    let storage = skins::injection::storage::ModStorageService::new(skins::paths::mods_dir());
    let root = storage.mods_root().to_path_buf();
    let Some(entry) = storage
        .list_mods_for_category(&category)
        .into_iter()
        .find(|e| e.id == mod_id || e.name == mod_id)
    else {
        return Err("Category mod not found".into());
    };
    let selection = skins::state::CategoryModSelection {
        mod_name: entry.name.clone(),
        mod_path: root.join(entry.path.replace('/', "\\")).to_string_lossy().into_owned(),
        mod_folder_name: entry.name.clone(),
        relative_path: entry.path.clone(),
    };
    let persisted = {
        let mut shared = state.skins.shared.lock_safe();
        match category.as_str() {
            "maps" => shared.category_mods.map = Some(selection),
            "fonts" => shared.category_mods.font = Some(selection),
            "announcers" => shared.category_mods.announcer = Some(selection),
            _ => {
                shared.category_mods.others.retain(|o| o.relative_path != selection.relative_path);
                shared.category_mods.others.push(selection);
            }
        }
        shared.category_mods.clone()
    };
    // Set-and-forget: persist so it re-applies every game until changed.
    skins::favorites::save_category_mods(&persisted);
    log_info!("[CATEGORY] pick {category} mod={}", entry.name);
    Ok(json!({ "ok": true, "category": category, "modName": entry.name }))
}

/// Clear a category selection. For map/font/announcer clears the slot; for the
/// stacking categories, `mod_id` removes one (or omit it to clear all `others`).
#[tauri::command]
fn skins_clear_category_mod(category: String, mod_id: Option<String>, state: tauri::State<Arc<AppState>>) {
    use skins::slog::log_info;
    let persisted = {
        let mut shared = state.skins.shared.lock_safe();
        match category.as_str() {
            "maps" => shared.category_mods.map = None,
            "fonts" => shared.category_mods.font = None,
            "announcers" => shared.category_mods.announcer = None,
            _ => match mod_id {
                Some(id) => shared.category_mods.others.retain(|o| o.relative_path != id && o.mod_name != id),
                None => shared.category_mods.others.clear(),
            },
        }
        shared.category_mods.clone()
    };
    skins::favorites::save_category_mods(&persisted);
    log_info!("[CATEGORY] clear {category}");
}

// ── Forms (Elementalist Lux, Sahn Uzal, Risen Legend HOL chromas — the
// `features::special::FORMS` table). A skin's forms are surfaced like chromas;
// picking one routes through the same `chroma::handle_selection` dispatcher. ──

/// List the alternate forms for a skin (empty for skins without any).
#[tauri::command]
fn skins_list_forms(skin_id: i64) -> serde_json::Value {
    let forms: Vec<serde_json::Value> = skins::features::special::FORMS
        .iter()
        .filter(|f| f.base_id == skin_id)
        .map(|f| json!({ "fakeId": f.fake_id, "display": f.display }))
        .collect();
    json!({ "skinId": skin_id, "forms": forms })
}

/// Pick a special form (routes through `chroma::handle_selection`, which detects
/// the form by its fake id and sets `selected_form_path` + the hovered id).
#[tauri::command]
fn skins_pick_form(skin_id: i64, fake_id: i64, display: String, state: tauri::State<Arc<AppState>>) {
    use skins::slog::log_info;
    log_info!("[FORM] pick skin={skin_id} form={fake_id} ({display})");
    let mut shared = state.skins.shared.lock_safe();
    skins::features::chroma::handle_selection(&mut shared, None, skin_id, fake_id, &display);
    shared.manual_pick_this_session = true;
}

/// Toggle "historic mode" — remember + auto-apply the last pick per champion.
/// Session-scoped; when enabled mid-champ-select it applies the current champ's
/// remembered pick immediately, and every future lock restores that champ's.
#[tauri::command]
fn skins_set_historic_mode(enabled: bool, state: tauri::State<Arc<AppState>>) -> bool {
    use skins::slog::log_info;
    let champ = {
        let mut shared = state.skins.shared.lock_safe();
        shared.historic_enabled = enabled;
        if enabled {
            shared.locked_champ_id.or(shared.hovered_champ_id)
        } else {
            shared.historic_mode_active = false;
            shared.historic_selection = None;
            None
        }
    };
    if let Some(cid) = champ {
        if let Some(entry) = skins::features::historic::get_historic_skin_for_champion(cid) {
            let mut shared = state.skins.shared.lock_safe();
            shared.historic_selection = Some(entry.to_selection());
            shared.historic_mode_active = true;
        }
    }
    log_info!("[HISTORIC] mode {}", if enabled { "enabled" } else { "disabled" });
    enabled
}

/// Screen rect of the League client window (physical px) so the overlay can
/// anchor itself to the client — which is usually windowed, not fullscreen —
/// instead of the monitor corner. `null` when the client isn't open/visible.
#[tauri::command]
fn league_client_rect() -> Option<serde_json::Value> {
    winutil::league_client_rect().map(|(l, t, r, b)| json!({ "left": l, "top": t, "right": r, "bottom": b }))
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
        "auto_download_peer_custom_mods": c.party.auto_download_peer_custom_mods,
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

#[tauri::command]
fn skins_party_set_auto_custom_mods(enabled: bool, state: tauri::State<Arc<AppState>>) -> serde_json::Value {
    {
        let mut cfg = state.config.lock_safe();
        cfg.party.auto_download_peer_custom_mods = enabled;
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
    // Truncate on a CHAR boundary, not a byte offset — `&cleaned[..80]` panics
    // the moment 80 bytes lands mid-codepoint (e.g. 80+ CJK chars, 3 bytes each).
    let cleaned: String = cleaned.chars().take(80).collect();
    if cleaned.is_empty() { fallback.to_string() } else { cleaned }
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
/// Categories that are never tied to one champion (game-wide) — these stay in
/// their flat category folder even when a champion happens to be attached.
/// Everything else (vfx/sfx/voiceover/loading_screen/ui/other), when it carries
/// a champion, is filed as a per-champion custom mod instead.
const CATEGORY_ALWAYS_GLOBAL: [&str; 3] = ["map_skin", "announcer", "font"];

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

/// One-time (idempotent) migration: custom cosmetics that were installed under a
/// flat category folder (vfx/sfx/…) but belong to a champion get moved into that
/// champ's `skins/{champ*1000}` folder, so they become selectable per-champion
/// custom mods (they show in the overlay's chroma bar instead of being stranded
/// in the Other tab). Returns true if any record changed (caller saves config).
fn migrate_champion_category_mods(cfg: &mut config::Config) -> bool {
    use skins::slog::{log_info, log_warn};
    const MOVABLE: [&str; 6] = ["vfx", "sfx", "voiceover", "loading_screen", "ui", "others"];
    let mods_root = skins::paths::mods_dir();
    let mut changed = false;
    for rec in cfg.library.installed.values_mut() {
        if rec.champ.trim().is_empty() {
            continue;
        }
        let Some((folder, filename)) = rec.file.split_once('/') else { continue };
        if !MOVABLE.contains(&folder) {
            continue;
        }
        let Some(cid) = resolve_champ_id_by_name(&rec.champ) else { continue };
        let new_rel = format!("skins/{}/{}", cid * 1000, filename);
        let src = mods_root.join(&rec.file);
        let dst = mods_root.join(&new_rel);
        if !src.exists() {
            // File already gone/moved — just correct the record so it lists.
            rec.file = new_rel;
            changed = true;
            continue;
        }
        // Never clobber: `fs::rename` overwrites an existing destination on
        // Windows, so a name collision (two mods → same champ + filename) would
        // silently destroy the first. Leave this one in place instead.
        if dst.exists() {
            log_warn!("[MIGRATE] not moving {} — mods/{new_rel} already exists (name collision)", rec.file);
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                log_info!("[MIGRATE] {} -> mods/{new_rel}", rec.file);
                rec.file = new_rel;
                changed = true;
            }
            Err(e) => log_warn!("[MIGRATE] could not move {}: {e}", rec.file),
        }
    }
    changed
}

/// One-time (idempotent) migration for mods installed before 3.0.9's
/// download-time target detection: they're still stuck on the base placeholder
/// (`skins/{champ*1000}`) with `target_skin_id: None`, so injection still forces
/// base instead of the real skin. Re-runs the same offline chunk scan
/// `place_library_mod` does at download time and, on a hit, re-files the mod via
/// `library_set_target_skin` — the exact move + record-update path a manual
/// "Pick skin" takes. Only ever touches `target_skin_id: None` mods sitting on a
/// base placeholder, so re-running is harmless. Needs the LCU up to resolve the
/// champion alias; the caller retries until it is (`spawn_library_target_migration`).
async fn migrate_library_targets(app: &AppHandle) {
    use skins::slog::log_info;
    let mods_root = skins::paths::mods_dir();

    // Snapshot candidates without holding the config lock across the async
    // detection below: `(mod_id, champ_id, absolute .fantome path)` for every
    // mod still on a base placeholder (`skins/{champ*1000}/…`) with no resolved
    // target. `champ_id` comes straight out of the placeholder folder number —
    // no name resolution needed, unlike `place_library_mod`'s first-install path.
    let candidates: Vec<(String, i64, std::path::PathBuf)> = {
        let state = app.state::<Arc<AppState>>();
        let cfg = state.config.lock_safe();
        cfg.library
            .installed
            .iter()
            .filter_map(|(mod_id, rec)| {
                if rec.target_skin_id.is_some() {
                    return None;
                }
                // Gate to champion_skin — running skin-detection on a champ-tied
                // vfx/sfx/ui/voiceover/loading_screen mod would misfile it as a
                // skin swap. Records saved before `category` existed are "" —
                // best-effort: still migrate those (they're the pre-3.0.9 stuck
                // skins this exists for) at the residual risk of a legacy
                // champ-tied vfx/sfx mod (also "") getting misclassified.
                if rec.category != "champion_skin" && !rec.category.is_empty() {
                    return None;
                }
                let mut comps = std::path::Path::new(&rec.file).components();
                if comps.next()?.as_os_str().to_str()? != "skins" {
                    return None;
                }
                let num: i64 = comps.next()?.as_os_str().to_str()?.parse().ok()?;
                if num % 1000 != 0 {
                    return None;
                }
                Some((mod_id.clone(), num / 1000, mods_root.join(&rec.file)))
            })
            .collect()
    };
    if candidates.is_empty() {
        return;
    }

    let mut moved = 0u32;
    for (mod_id, champ_id, abs_path) in candidates {
        if !abs_path.exists() {
            continue;
        }
        let Some(skin_id) = skins::injection::target_detect::detect_target_skin_offline(&abs_path, champ_id).await else {
            continue;
        };
        let state = app.state::<Arc<AppState>>();
        if library_set_target_skin(app.clone(), mod_id, skin_id, state).await.is_ok() {
            moved += 1;
        }
    }
    if moved > 0 {
        log_info!("[MIGRATE] moved {moved} mod(s) to detected skins");
    }
}

/// Retries `migrate_library_targets` roughly every 20s, up to 6 tries (~2min),
/// so the common "launch Chud, then open League" flow still gets migrated —
/// detection needs the LCU up to resolve the champion alias. Gives up quietly
/// after that; those mods just keep their "Pick skin" fallback until next launch.
fn spawn_library_target_migration(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        for _ in 0..6 {
            if lcu::cached_auth().is_some() {
                migrate_library_targets(&app).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        }
    });
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
#[allow(clippy::too_many_arguments)]
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
    // Resolved champion id, when this mod lands under a per-champ skins folder
    // (either a champion_skin or a champ-carrying vfx/sfx/etc.) — captured here
    // so the champion-skin download-time target detection below doesn't have to
    // re-run champ resolution.
    let mut resolved_champ_id: Option<i64> = None;
    // Resolve the destination folder (relative to mods/) by category.
    let rel_dir: std::path::PathBuf = match library_category_dir(category) {
        // A champion-specific cosmetic (a chroma, or a skin-variant VFX/SFX/etc.)
        // carries a champion — file it under that champ's skins folder so it's a
        // selectable per-champion custom mod (shows in the chroma bar), not a
        // stranded global category mod. Global categories (maps/fonts/announcers)
        // never carry a champion, so they stay in their flat folder.
        Some(cat_dir) => match champ_id.filter(|&id| id > 0).or_else(|| resolve_champ_id_by_name(champ)) {
            Some(cid) if !CATEGORY_ALWAYS_GLOBAL.contains(&category) => {
                resolved_champ_id = Some(cid);
                std::path::PathBuf::from("skins").join((cid * 1000).to_string())
            }
            _ => std::path::PathBuf::from(cat_dir),
        },
        None => {
            let cid = champ_id.filter(|&id| id > 0).or_else(|| resolve_champ_id_by_name(champ)).ok_or_else(|| {
                log_warn!("[LIBRARY] no champion resolved for skin mod '{name}' (champ='{champ}')");
                format!("Couldn't match \"{champ}\" to a champion.")
            })?;
            resolved_champ_id = Some(cid);
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

    // A corrupt/non-zip body (currently only caught by the >1KB size floor
    // above) must never get written to disk and badged "installed" — verify
    // it's a readable archive before anything below touches the filesystem.
    if zip::ZipArchive::new(std::io::Cursor::new(bytes)).is_err() {
        log_warn!("[LIBRARY] '{name}' isn't a readable archive - refusing to install");
        return Err(format!("'{name}' didn't download as a valid mod archive — try again."));
    }

    let dir = skins::paths::mods_dir().join(&rel_dir);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let stem = sanitize_mod_filename(name, mod_id);
    let file_name = format!("{stem}.fantome");
    let file_path = dir.join(&file_name);
    tokio::fs::write(&file_path, bytes).await.map_err(|e| e.to_string())?;
    let mut rel_file = rel_dir.join(&file_name).to_string_lossy().replace('\\', "/");
    log_info!("[LIBRARY] installed '{name}' ({size_mb:.1} MB) -> mods/{rel_file}");

    // Champion skins are filed under the base placeholder (skins/{champ*1000})
    // because the library can't know which real skin slot a custom mod's WAD
    // chunks target (see target_detect.rs). Try to resolve it right now with an
    // offline chunk-hash scan and re-file under the real skin id immediately —
    // that's strictly better than leaving it for injection time to guess (and
    // possibly get wrong). Best-effort: `resolved_champ_id` and the LCU-backed
    // scan can both come back empty, in which case the mod stays on the
    // placeholder with `target_skin_id: None` and the UI offers a manual pick.
    let mut target_skin_id: Option<i64> = None;
    if category == "champion_skin" {
        if let Some(cid) = resolved_champ_id {
            if let Some(id) = skins::injection::target_detect::detect_target_skin_offline(&file_path, cid).await {
                let real_dir = skins::paths::mods_dir().join("skins").join(id.to_string());
                if tokio::fs::create_dir_all(&real_dir).await.is_ok() {
                    let real_path = real_dir.join(&file_name);
                    if tokio::fs::rename(&file_path, &real_path).await.is_ok() {
                        rel_file = std::path::PathBuf::from("skins").join(id.to_string()).join(&file_name).to_string_lossy().replace('\\', "/");
                        target_skin_id = Some(id);
                        log_info!("[LIBRARY] '{name}' auto-detected -> skin {id}, refiled to mods/{rel_file}");
                    }
                }
            }
        }
    }

    let record = config::InstalledMod {
        name: name.to_string(),
        champ: champ.to_string(),
        version: "1.0.0".into(),
        size_mb,
        file: rel_file,
        scan_verdict: summary.verdict.clone(),
        scan_sha: summary.sha256.clone(),
        target_skin_id,
        category: category.to_string(),
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
#[allow(clippy::too_many_arguments)]
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

/// Manually resolve a champion-skin mod's target slot when download-time
/// auto-detection (`place_library_mod`) couldn't confidently pick one — moves
/// the `.fantome` into `mods/skins/{skin_id}/`, which is what makes the fast
/// path in `plan_custom_mod_route` (trigger.rs) force the real skin slot
/// instead of guessing at game time. `_app` isn't used yet (no live client
/// write needed here) but kept in the signature for parity with the rest of
/// the Library command surface, which all take it.
#[tauri::command]
async fn library_set_target_skin(
    _app: AppHandle,
    mod_id: String,
    skin_id: i64,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    use skins::slog::log_info;
    let (old_rel, name) = {
        let c = state.config.lock_safe();
        let rec = c.library.installed.get(&mod_id).ok_or("mod not installed")?;
        (rec.file.clone(), rec.name.clone())
    };
    let old_path = skins::paths::mods_dir().join(&old_rel);
    let file_name = old_path.file_name().ok_or("mod file has no name")?.to_owned();
    let new_dir = skins::paths::mods_dir().join("skins").join(skin_id.to_string());
    tokio::fs::create_dir_all(&new_dir).await.map_err(|e| e.to_string())?;
    let mut new_path = new_dir.join(&file_name);
    if old_path != new_path {
        // `tokio::fs::rename` overwrites an existing destination on Windows —
        // the exact landmine `migrate_champion_category_mods` documents. Suffix
        // the destination filename until free instead of clobbering an
        // unrelated mod that happens to already occupy this name in the
        // target skin's slot.
        if new_path.exists() {
            let stem = new_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let ext = new_path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            let mut n = 2;
            loop {
                let candidate = new_dir.join(format!("{stem}-{n}.{ext}"));
                if !candidate.exists() {
                    new_path = candidate;
                    break;
                }
                n += 1;
            }
        }
        tokio::fs::rename(&old_path, &new_path).await.map_err(|e| format!("couldn't move '{name}': {e}"))?;
    }
    let file_name = new_path.file_name().ok_or("mod file has no name")?.to_owned();
    let new_rel = std::path::PathBuf::from("skins").join(skin_id.to_string()).join(&file_name).to_string_lossy().replace('\\', "/");
    {
        let mut c = state.config.lock_safe();
        if let Some(rec) = c.library.installed.get_mut(&mod_id) {
            rec.file = new_rel.clone();
            rec.target_skin_id = Some(skin_id);
        }
        let _ = c.save();
    }
    log_info!("[LIBRARY] '{name}' target skin set to {skin_id} - moved to mods/{new_rel}");
    Ok(())
}

// ── Import Mod: guided local install of a .fantome/.zip the user already has ──
// (replaces the old "drop it in mods\skins\{champId*1000}\ yourself" workflow —
// the picker + modal (native/ui/src/library.js) resolve champ + target skin,
// then this files it exactly like a Library download and registers the same
// `InstalledMod` record so it shows on the in-client Custom Mods button.

/// Native open-file dialog for "Import Mod", filtered to mod archives. `rfd`'s
/// dialog is a blocking OS call — `spawn_blocking` keeps it off the async
/// runtime's worker threads while it's open. `None` on cancel or dialog error;
/// the caller (the UI) just no-ops on that, same as an undetected target.
#[tauri::command]
async fn pick_mod_file() -> Option<String> {
    tauri::async_runtime::spawn_blocking(|| rfd::FileDialog::new().add_filter("Mod", &["fantome", "zip"]).pick_file())
        .await
        .ok()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Best-effort skin prefill for the Import Mod modal — the same offline
/// chunk-hash scan `place_library_mod` runs at Library download time, applied
/// to a file straight from the picker (not yet copied anywhere). `None` (no
/// League client running, or no single confident chunk match) just leaves the
/// modal's skin dropdown on "Auto".
#[tauri::command]
async fn detect_mod_target(file_path: String, champion_id: i64) -> Option<i64> {
    skins::injection::target_detect::detect_target_skin_offline(std::path::Path::new(&file_path), champion_id).await
}

/// Core of `import_mod`, factored out so it's unit-testable without AppState or
/// the real user-data dir: validate the archive, resolve the target folder,
/// write the file under `mods_root`, and build the `InstalledMod` record.
/// Returns (record, sanitized stem, relative file path).
///
/// Folder resolution mirrors `place_library_mod`'s category -> folder rules
/// exactly (`library_category_dir` + `CATEGORY_ALWAYS_GLOBAL`): a champion-
/// carrying category (`champion_skin`, or a champ-tagged vfx/sfx/etc that
/// isn't in `CATEGORY_ALWAYS_GLOBAL`) with a resolved `champion_id` files under
/// that champ's `skins/{champ*1000}` (or the explicit `skin_id` slot); a global
/// category (maps/fonts/announcers, or no champion) files under its flat
/// `mods/{category_dir}` folder instead. `skin_id` None or a base id
/// (`% 1000 == 0`) files under the champion's base placeholder ("Auto"), and
/// only `champion_skin` ever carries an explicit `target_skin_id` — matching
/// `place_library_mod`, which only runs target detection for that category.
fn place_imported_mod(
    bytes: &[u8],
    category: &str,
    champion_id: Option<i64>,
    skin_id: Option<i64>,
    name: &str,
    champ: &str,
    fallback_stem: &str,
    mods_root: &std::path::Path,
) -> Result<(config::InstalledMod, String, String), String> {
    if zip::ZipArchive::new(std::io::Cursor::new(bytes)).is_err() {
        return Err("That file isn't a valid mod archive.".to_string());
    }
    let cid = champion_id.filter(|&id| id > 0);
    // The champion's skins/{champ*1000} folder, or the explicit skin slot when
    // the caller picked a real (non-base) skin.
    let champ_dir = |id: i64| match skin_id.filter(|s| s % 1000 != 0) {
        Some(sid) => std::path::PathBuf::from("skins").join(sid.to_string()),
        None => std::path::PathBuf::from("skins").join((id * 1000).to_string()),
    };
    let mut target_skin_id: Option<i64> = None;
    let rel_dir = match library_category_dir(category) {
        Some(cat_dir) => match cid.filter(|_| !CATEGORY_ALWAYS_GLOBAL.contains(&category)) {
            Some(id) => {
                if category == "champion_skin" {
                    target_skin_id = skin_id;
                }
                champ_dir(id)
            }
            None => std::path::PathBuf::from(cat_dir),
        },
        // champion_skin (or an unknown category) -> always champion-carrying.
        None => {
            let id = cid.ok_or_else(|| "Pick a champion for this mod.".to_string())?;
            target_skin_id = skin_id;
            champ_dir(id)
        }
    };
    let dir = mods_root.join(&rel_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = sanitize_mod_filename(name, fallback_stem);
    // Two imports with the same (or both blank -> fallback) display name would
    // otherwise silently overwrite each other via `fs::write` — suffix the
    // stem until the path is free, same dedup pattern as the mod_id loop in
    // `import_mod` below.
    let mut file_name = format!("{stem}.fantome");
    let mut n = 2;
    while dir.join(&file_name).exists() {
        file_name = format!("{stem}-{n}.fantome");
        n += 1;
    }
    std::fs::write(dir.join(&file_name), bytes).map_err(|e| e.to_string())?;
    let rel_file = rel_dir.join(&file_name).to_string_lossy().replace('\\', "/");
    let record = config::InstalledMod {
        name: name.to_string(),
        champ: champ.to_string(),
        version: "1.0.0".into(),
        size_mb: (bytes.len() as f64) / 1_048_576.0,
        file: rel_file.clone(),
        scan_verdict: String::new(),
        scan_sha: String::new(),
        target_skin_id,
        category: category.to_string(),
    };
    Ok((record, stem, rel_file))
}

/// Guided "Import Mod": file a user-supplied `.fantome`/`.zip` into the right
/// `mods/` slot for its category and register it exactly like a Library
/// install (`place_library_mod`) does, so it shows on the in-client Custom Mods
/// button and gets picked up by injection like any other installed mod.
/// `champion_id`/`skin_id` only matter for `champion_skin` (and other champ-
/// carrying categories) — the UI omits them for global categories (maps,
/// fonts, announcers, …). `skin_id` `None` (or a base id, `% 1000 == 0`) is
/// "Auto / let Chud decide" — the mod lands on the champion's base
/// placeholder and injection's game-time detection applies it, same as an
/// unresolved Library download.
#[tauri::command]
async fn import_mod(
    file_path: String,
    category: String,
    champion_id: Option<i64>,
    skin_id: Option<i64>,
    name: String,
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<(), String> {
    use skins::slog::log_info;
    let src = std::path::PathBuf::from(&file_path);
    // Sanity ceiling before reading a local file of arbitrary size into memory —
    // same 512MB ceiling `place_library_mod` applies to a network download.
    let meta = tokio::fs::metadata(&src).await.map_err(|_| "That file isn't a valid mod archive.".to_string())?;
    if meta.len() > 512 * 1024 * 1024 {
        return Err("That file is too large to be a mod.".to_string());
    }
    let bytes = tokio::fs::read(&src).await.map_err(|_| "That file isn't a valid mod archive.".to_string())?;

    // Import used to bypass ModScan entirely — run the same in-memory scan the
    // Library install path runs, before anything below touches disk.
    let (endpoint, allowed) = {
        let c = state.config.lock_safe();
        (c.library.endpoint.clone(), net::allowed_origins(&c))
    };
    let http = net::build_external_client(20.0, allowed.clone());
    let bytes = Arc::new(bytes);
    let summary = scan_downloaded_mod(endpoint.trim_end_matches('/'), &allowed, &http, bytes.clone(), &name).await;
    if summary.blocking {
        return Err(format!("'{name}' was flagged by ModScan ({}) — not imported.", summary.verdict));
    }

    // Cosmetic only (Installed-list display) — best-effort from the local
    // catalog; empty for global categories that don't carry a champion.
    let champ = champion_id
        .and_then(|cid| skins::favorites::catalog(None).into_iter().find(|c| c.champ_id == cid))
        .map(|c| c.champ_name)
        .unwrap_or_default();
    // Fall back to the source file's own stem so a blank Name still files sanely.
    let fallback = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "imported-mod".to_string());
    let (mut record, stem, rel_file) =
        place_imported_mod(bytes.as_slice(), &category, champion_id, skin_id, &name, &champ, &fallback, &skins::paths::mods_dir())?;
    record.scan_verdict = summary.verdict.clone();
    record.scan_sha = summary.sha256.clone();

    let mut c = state.config.lock_safe();
    let mut mod_id = format!("local-{}", stem.to_lowercase().replace(' ', "-"));
    let mut n = 2;
    while c.library.installed.contains_key(&mod_id) {
        mod_id = format!("local-{}-{n}", stem.to_lowercase().replace(' ', "-"));
        n += 1;
    }
    c.library.installed.insert(mod_id.clone(), record);
    // The record was inserted before this fallible save — on failure it must
    // not stay as a ghost entry (surviving in memory until some later save
    // persists it despite the caller seeing this Err).
    if let Err(e) = c.save() {
        c.library.installed.remove(&mod_id);
        return Err(e.to_string());
    }
    log_info!("[LIBRARY] imported '{name}' -> mods/{rel_file}");
    Ok(())
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

/// Last Riot-break advisory state — boot belt-and-suspenders for the same
/// listener race as `updater_check` below. `None` until the first poll lands.
#[tauri::command]
fn advisory_status() -> Option<serde_json::Value> {
    advisory::last_payload()
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
            advisory_status,
            get_state,
            toggle_tool,
            stop_all,
            set_injection_ack,
            request_admin,
            open_external_url,
            exit_app,
            get_diagnostics,
            submit_bug_report,
            get_config,
            save_config,
            get_profile,
            skins_get_state,
            skins_set_overlay_card_cols,
            skins_save_settings,
            skins_set_ack,
            skins_open_cslol_dir,
            skins_download,
            skins_set_enabled,
            skins_catalog,
            skins_get_favorites,
            skins_set_favorite,
            skins_pick_skin,
            skins_preview_skin,
            skins_clear_pick,
            skins_list_champion_skins,
            skins_roll_random,
            skins_cancel_random,
            skins_list_custom_mods,
            skins_pick_custom_mod,
            skins_set_custom_mod_chroma,
            skins_clear_custom_mod,
            skins_custom_mod_preview,
            skins_custom_mod_thumb,
            skins_list_category_mods,
            skins_pick_category_mod,
            skins_clear_category_mod,
            skins_list_forms,
            skins_pick_form,
            skins_set_historic_mode,
            league_client_rect,
            skins_party_enable,
            skins_party_disable,
            skins_party_add_peer,
            skins_party_get_state,
            skins_party_set_consent,
            skins_party_set_auto_announcers,
            skins_party_set_auto_custom_mods,
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
            library_set_target_skin,
            pick_mod_file,
            detect_mod_target,
            import_mod,
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

            // Move champion-tagged cosmetics (chromas/VFX) that were installed
            // into flat category folders into the champ's skins folder, so they
            // surface as selectable custom mods in the chroma bar.
            {
                let mut cfg = st.config.lock_safe();
                if migrate_champion_category_mods(&mut cfg) {
                    let _ = cfg.save();
                }
            }

            // Retroactively resolve pre-3.0.9 champion-skin mods still stuck on
            // the base placeholder (needs the LCU up, hence the retry loop).
            spawn_library_target_migration(handle.clone());

            // Rune/build auto-import watcher (inert until enabled + a Worker
            // endpoint is configured).
            spawn_runes_auto_import(st.clone());
            spawn_appear_offline(st.clone());
            spawn_hash_autorefresh(handle.clone(), st.clone());
            // Anonymous usage heartbeat — no-ops until `telemetry.enabled` (dark).
            telemetry::spawn(handle.clone());

            // Skin-picker overlay visibility: float it over the client during
            // champ select (shown once per entry, dismissable), hide it any
            // other time — so single-monitor users pick on top of the client
            // instead of alt-tabbing to the app. Polls the live LCU phase
            // directly (not the ~2.5s-lagged safety snapshot) so it hides
            // promptly when the game starts and never covers gameplay.
            let overlay_handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                let client = lcu::build_lcu_client(3.0);
                let mut was_champ_select = false;
                let mut shown_this_cs = false;
                let mut vis_emitted = false;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                    let in_cs = match lcu::cached_auth() {
                        Some(auth) => lcu::get_phase(&client, &auth).await.as_deref() == Some("ChampSelect"),
                        None => false,
                    };
                    let enabled = {
                        let st = overlay_handle.state::<Arc<AppState>>();
                        let e = st.config.lock_safe().skins.enabled;
                        e
                    };
                    if in_cs && !was_champ_select {
                        shown_this_cs = false; // fresh champ select — allow one auto-show
                    }
                    was_champ_select = in_cs;
                    let want_visible = in_cs && enabled;
                    if let Some(w) = overlay_handle.get_webview_window("overlay") {
                        // Show the launcher the moment champ select starts (not only
                        // after a champ is hovered/locked) — the global mods
                        // (maps/announcer/fonts/other) are champion-independent, so
                        // users get the whole champ-select window to set them up.
                        if in_cs && enabled && !shown_this_cs {
                            let _ = w.show(); // no set_focus — never steal focus from the client
                            shown_this_cs = true;
                        } else if !in_cs {
                            let _ = w.hide();
                        }
                        // Tell the overlay JS to pause/resume its own poll loop —
                        // it can't see window visibility from inside the webview.
                        if want_visible != vis_emitted {
                            vis_emitted = want_visible;
                            let _ = w.emit("overlay-visibility", want_visible);
                        }
                    }
                }
            });

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

            // Riot-break advisory poll: launch + every 10 min while open, so a
            // break that starts overnight surfaces without a restart. Runs in
            // dev builds too (unlike the updater) — it's how the popup is tested.
            {
                let advisory_handle = handle.clone();
                tauri::async_runtime::spawn(async move { advisory::run(advisory_handle).await });
            }

            // Skins phase engine: always spawned — it just idles (poll fallback
            // finds no LCU auth, WS fan-out has nothing to send) when there's no
            // client to watch. Cheaper than gating on a settings flag.
            let phase_handle = skins::phase::spawn(handle.clone(), st.skins.clone());

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
            // Stash the injection manager so the ticker/trigger can pull it from
            // the app handle at the loadout deadline.
            *st.skins_injection.lock_safe() = Some(injection_manager);

            // Party mode manager.
            let party_manager = skins::party::manager::PartyManager::new(&handle, st.skins.clone());
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
            let openmods = MenuItem::with_id(app, "openmods", "Open Mods Folder", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &openmods, &quit])?;
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
                    "openmods" => {
                        let dir = skins::paths::mods_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        winutil::open_folder(&dir.to_string_lossy());
                    }
                    "quit" => {
                        release_held_keys(&app.state::<Arc<AppState>>());
                        skins::injection::process::kill_all_modtools_processes_os();
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
        .build(tauri::generate_context!())
        .expect("error while building Chud")
        .run(|_app, event| {
            // Last-resort backstop: reap mod-tools.exe children on ANY exit path
            // (tray Exit, exit_app, OS signal, future paths). Lock-free.
            if let tauri::RunEvent::Exit = event {
                skins::injection::process::kill_all_modtools_processes_os();
            }
        });
}

#[cfg(test)]
mod import_mod_tests {
    use super::*;

    /// A minimal but VALID zip — what `place_imported_mod`'s archive check accepts.
    fn tiny_zip() -> Vec<u8> {
        use std::io::Write;
        let mut cur = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut cur);
            zw.start_file("info.json", zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(b"{\"Name\":\"test\"}").unwrap();
            zw.finish().unwrap();
        }
        cur.into_inner()
    }

    #[test]
    fn imports_under_target_skin_and_builds_record() {
        let root = std::env::temp_dir().join("chud_import_test_skin");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        // Aphelios champ 523, explicit non-base target skin 523001.
        let (rec, stem, rel) =
            place_imported_mod(&bytes, "champion_skin", Some(523), Some(523001), "Ryley Aphelios", "Aphelios", "fallback", &root).unwrap();
        assert_eq!(rel, format!("skins/523001/{stem}.fantome"));
        assert!(
            root.join("skins").join("523001").join(format!("{stem}.fantome")).exists(),
            "file must land under the target skin folder"
        );
        assert_eq!(rec.target_skin_id, Some(523001));
        assert_eq!(rec.file, rel);
        assert_eq!(rec.name, "Ryley Aphelios");
        assert_eq!(rec.champ, "Aphelios");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_files_under_base_placeholder() {
        let root = std::env::temp_dir().join("chud_import_test_base");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        // "Auto" (skin_id None) -> base placeholder champ*1000 = 523000.
        let (rec, stem, rel) =
            place_imported_mod(&bytes, "champion_skin", Some(523), None, "Some Mod", "Aphelios", "fallback", &root).unwrap();
        assert_eq!(rel, format!("skins/523000/{stem}.fantome"));
        assert!(root.join("skins").join("523000").join(format!("{stem}.fantome")).exists());
        assert_eq!(rec.target_skin_id, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn base_skin_pick_files_under_placeholder_but_keeps_explicit_target() {
        let root = std::env::temp_dir().join("chud_import_test_basepick");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        // Explicit "Base skin" = Some(523000): %1000==0 -> base placeholder folder,
        // but target_skin_id stays the explicit value (two independent rules).
        let (rec, _stem, rel) =
            place_imported_mod(&bytes, "champion_skin", Some(523), Some(523000), "Base Mod", "Aphelios", "fb", &root).unwrap();
        assert!(rel.starts_with("skins/523000/"));
        assert_eq!(rec.target_skin_id, Some(523000));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_non_zip() {
        let root = std::env::temp_dir().join("chud_import_test_bad");
        let r = place_imported_mod(b"definitely not a zip file", "champion_skin", Some(523), None, "x", "Aphelios", "fb", &root);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("valid mod archive"));
        assert!(!root.exists(), "a rejected archive must not create any folder/file");
    }

    #[test]
    fn non_skin_category_files_under_flat_category_folder() {
        let root = std::env::temp_dir().join("chud_import_test_font");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        // "font" is a global category (CATEGORY_ALWAYS_GLOBAL) -> flat
        // mods/fonts/ folder, no champion, no target skin, no skins/ folder.
        let (rec, stem, rel) =
            place_imported_mod(&bytes, "font", None, None, "Cool Font", "", "fallback", &root).unwrap();
        assert_eq!(rel, format!("fonts/{stem}.fantome"));
        assert!(root.join("fonts").join(format!("{stem}.fantome")).exists());
        assert!(!root.join("skins").exists(), "a global category must never create a skins/ folder");
        assert_eq!(rec.target_skin_id, None);
        assert_eq!(rec.champ, "");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn colliding_names_get_a_distinct_suffixed_file_not_a_clobber() {
        let root = std::env::temp_dir().join("chud_import_test_collision");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        let (rec1, stem1, rel1) =
            place_imported_mod(&bytes, "champion_skin", Some(523), None, "Same Name", "Aphelios", "fallback", &root).unwrap();
        let (rec2, stem2, rel2) =
            place_imported_mod(&bytes, "champion_skin", Some(523), None, "Same Name", "Aphelios", "fallback", &root).unwrap();
        assert_ne!(rel1, rel2, "second import of the same name must not overwrite the first");
        // `stem` (the sanitized display name) is the same both times — the
        // dedup suffix is applied only to the on-disk file name, in `rel`.
        assert_eq!(stem1, stem2);
        assert_eq!(rel2, format!("skins/523000/{stem2}-2.fantome"));
        assert!(root.join(&rec1.file).exists(), "first file must still exist");
        assert!(root.join(&rec2.file).exists(), "second (suffixed) file must exist");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_ascii_name_does_not_panic_on_truncation() {
        let root = std::env::temp_dir().join("chud_import_test_nonascii");
        let _ = std::fs::remove_dir_all(&root);
        let bytes = tiny_zip();
        // 80 CJK chars = 240 bytes — byte-slicing `[..80]` would panic mid-codepoint.
        let long_name = "한".repeat(80);
        let (rec, _stem, rel) =
            place_imported_mod(&bytes, "champion_skin", Some(523), None, &long_name, "Aphelios", "fallback", &root).unwrap();
        assert!(root.join(&rel).exists());
        assert_eq!(rec.name, long_name);
        let _ = std::fs::remove_dir_all(&root);
    }
}
