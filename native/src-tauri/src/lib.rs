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
    });

    tauri::Builder::default()
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
            get_profile
        ])
        .setup(|app| {
            // Auto-start Auto-Accept (matches the Python app's auto_start_main).
            let handle = app.handle().clone();
            let st = app.state::<Arc<AppState>>().inner().clone();
            st.running.store(true, Ordering::SeqCst);
            spawn_auto_accept(&handle, st.clone());

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
            let bridge_handle =
                skins::bridge::spawn(handle.clone(), st.skins.clone(), injection_manager, &phase_handle);
            *st.skins_bridge.lock_safe() = Some(bridge_handle);

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
