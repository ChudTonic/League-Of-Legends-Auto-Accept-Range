//! Local bridge server for the in-client Pengu Loader plugins (S4) — an axum
//! server on `127.0.0.1`, port picked from the first free slot in
//! `50000..=50010` (ported from `utils\core\utilities.py::find_free_port`,
//! narrowed from Python's 100-port scan since one local server never needs
//! that many fallback attempts), written to `state_dir/bridge_port.txt` and
//! served over all three plugin discovery paths (`GET /bridge-port`,
//! `GET /port`, and the port file itself).
//!
//! `BridgeHandle` is the seam later milestones hang off of: S5 (game-flow
//! ticker/trigger) and S6 (party) hold a clone and call its
//! `broadcast_*` methods (see `broadcast.rs`) to push state to the plugins
//! without reaching into `ws.rs` themselves. It is deliberately thin —
//! `Clone + Send + Sync`, cheap to pass around — backed by a
//! `tokio::sync::broadcast::Sender<String>` fanout channel: every connected
//! plugin's WebSocket task subscribes its own `Receiver`, so "broadcast to
//! all clients" falls out of the channel's own semantics instead of manually
//! iterating a `Vec` of per-client senders. This still satisfies the
//! broadcast-only contract (`docs/SKINS_PORT.md` §3) — there is no
//! per-client targeted send anywhere in this module.

#![allow(dead_code)]

pub mod broadcast;
pub mod handlers;
pub mod http;
pub mod protocol;
pub mod ws;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tauri::AppHandle;
use tokio::net::TcpListener;
use tokio::sync::broadcast::{Receiver, Sender};

use crate::lcu;
use crate::skins::injection::storage::ModStorageService;
use crate::skins::injection::InjectionManager;
use crate::skins::lcu_ext;
use crate::skins::phase::{PhaseEvent, PhaseHandle};
use crate::skins::paths;
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::SkinsState;

const PORT_RANGE_START: u16 = 50000;
const PORT_RANGE_END: u16 = 50010;
/// Outbound fanout channel capacity — generous enough that a burst of state
/// broadcasts (e.g. several `select-*` mods in a row) never lags a
/// momentarily-slow client; a lagged client just misses the oldest entries
/// (`tokio::sync::broadcast`'s documented behavior), it never blocks a sender.
const BROADCAST_CHANNEL_CAPACITY: usize = 256;

/// One extracted-and-ready non-skin mod selection (map/font/announcer/other)
/// — ported from the ad hoc dict shape Python stuffed onto `shared_state`
/// (`selected_map_mod`/`selected_font_mod`/`selected_announcer_mod`/
/// `selected_other_mods`).
///
/// DEVIATION (flagged for the lead): these belong conceptually next to
/// `SkinsShared::selected_custom_mod` in `state.rs`, but `state.rs` isn't in
/// this milestone's file scope, so they live here instead. S5's injection
/// trigger will need read access to them (as `extra_mod_names` for
/// `InjectionManager::inject_skin_immediately`) — recommend migrating this
/// into `SkinsShared` in a follow-up so trigger.rs doesn't have to reach into
/// `bridge::BridgeContext`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModSelection {
    pub mod_name: String,
    pub mod_path: String,
    pub mod_folder_name: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModSelections {
    pub map: Option<ModSelection>,
    pub font: Option<ModSelection>,
    pub announcer: Option<ModSelection>,
    pub others: Vec<ModSelection>,
}

struct BridgeInner {
    tx: Sender<String>,
    port: u16,
}

/// Cheap, cloneable handle to the running bridge. Store this in `AppState`;
/// any `#[tauri::command]` or spawned task can call its `broadcast_*`
/// methods (see `broadcast.rs`) to push a state update to every connected
/// plugin — this is the S5/S6 integration seam.
#[derive(Clone)]
pub struct BridgeHandle(Arc<BridgeInner>);

impl BridgeHandle {
    pub fn port(&self) -> u16 {
        self.0.port
    }

    /// Subscribe to the outbound fanout — one call per connected WebSocket
    /// (see `ws::handle_socket`).
    pub fn subscribe(&self) -> Receiver<String> {
        self.0.tx.subscribe()
    }

    /// Broadcast a pre-serialized JSON string to every connected client.
    /// Errors (no subscribers currently connected) are expected and silently
    /// ignored — mirrors Python's `if not connections: return` early-out in
    /// every `Broadcaster.broadcast_*` method.
    pub fn send_raw(&self, message: String) {
        let _ = self.0.tx.send(message);
    }

    /// Convenience wrapper for the ad hoc request/response payloads
    /// (`handlers.rs`'s settings-data, skin-mods-response, etc.) that don't
    /// have a dedicated `protocol.rs` struct.
    pub fn broadcast_json(&self, value: serde_json::Value) {
        self.send_raw(value.to_string());
    }
}

/// Everything the bridge's WebSocket/HTTP handlers need, bundled so it can be
/// cloned into each connection task. All fields are `Arc`/cheap-`Clone`.
#[derive(Clone)]
pub struct BridgeContext {
    /// Kept for on-demand access to `AppState` (skins config, injection ack,
    /// admin checks) without widening `bridge::spawn`'s signature — see
    /// `handlers::settings` for the call sites.
    pub app: AppHandle,
    pub skins: Arc<SkinsState>,
    pub injection: Arc<InjectionManager>,
    pub mod_storage: Arc<ModStorageService>,
    pub handle: BridgeHandle,
    pub http_client: reqwest::Client,
    /// See `ModSelections`'s doc comment for why this lives here instead of
    /// `SkinsShared`.
    pub mod_selections: Arc<Mutex<ModSelections>>,
}

/// Find the first free port in `50000..=50010` (ported from
/// `find_free_port`, range narrowed per this module's doc comment).
fn find_free_port() -> Option<u16> {
    for port in PORT_RANGE_START..=PORT_RANGE_END {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

fn bridge_port_file() -> PathBuf {
    paths::state_dir().join("bridge_port.txt")
}

/// Ported from `utilities.py::write_bridge_port` — best-effort, logged not
/// propagated (a failed write just means plugins fall back to the
/// `/bridge-port`/`/port` HTTP discovery paths instead of the file).
fn write_bridge_port(port: u16) {
    let path = bridge_port_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, port.to_string()) {
        Ok(()) => log_info!("[bridge] Wrote bridge port {port} to {}", path.display()),
        Err(e) => log_warn!("[bridge] Failed to write bridge port file: {e}"),
    }
}

/// Read the current port back from disk (used by `http::route`'s
/// `/bridge-port` handler so a stale in-memory port never disagrees with
/// what's actually on disk — mirrors the Python handler's own file-read).
pub(crate) fn read_bridge_port_file() -> Option<u16> {
    std::fs::read_to_string(bridge_port_file()).ok()?.trim().parse().ok()
}

/// Loopback-origin check (ported from `utils\core\security.py::
/// is_loopback_origin`) — reject browser requests whose `Origin` header
/// isn't hosted on this machine. NOTE: this is not real authentication — a
/// non-browser client (the actual Pengu Loader plugin's CEF fetch context)
/// sends no `Origin` header at all and bypasses this check entirely, exactly
/// like the Python original. It only stops a malicious webpage the user
/// visits in a normal browser from reaching this loopback server.
pub(crate) fn is_loopback_origin(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else { return false };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(after_bracket) = host_port.strip_prefix('[') {
        after_bracket.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    matches!(host.to_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1")
}

/// Spawn the bridge: binds the axum server, writes the port file, and wires
/// the phase-event subscription that rebroadcasts phase-change/champion-lock
/// to the plugins. Returns immediately with a `BridgeHandle`; the server
/// itself runs on a spawned tokio task for the lifetime of the app.
///
/// `phase` is borrowed rather than consumed: `PhaseHandle` isn't `Clone`,
/// and `lib.rs`'s `setup()` still needs to store the original in
/// `AppState::skins_phase` for `lcu_ws.rs`'s fan-out — `PhaseHandle::subscribe`
/// only needs `&self`, so a reference is all this function requires.
pub fn spawn(app: AppHandle, skins: Arc<SkinsState>, injection: Arc<InjectionManager>, phase: &PhaseHandle) -> BridgeHandle {
    let port = find_free_port().unwrap_or_else(|| {
        log_error!("[bridge] No free port found in {PORT_RANGE_START}..={PORT_RANGE_END}; falling back to {PORT_RANGE_START}");
        PORT_RANGE_START
    });
    write_bridge_port(port);

    let (tx, _rx) = tokio::sync::broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
    let handle = BridgeHandle(Arc::new(BridgeInner { tx, port }));

    let mod_storage = Arc::new(ModStorageService::new(paths::mods_dir()));
    let http_client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);

    let ctx = BridgeContext {
        app,
        skins,
        injection,
        mod_storage,
        handle: handle.clone(),
        http_client,
        mod_selections: Arc::new(Mutex::new(ModSelections::default())),
    };

    spawn_phase_rebroadcast(ctx.clone(), phase);
    spawn_server(ctx, port);

    handle
}

/// Subscribe to `PhaseHandle`'s broadcast and rebroadcast the two events the
/// bridge needs to forward to plugins (`docs/SKINS_PORT.md`: "Subscribe to
/// rebroadcast phase-change/champion-locked to plugins"). Other `PhaseEvent`
/// variants (`ChampSelectEntered`, `Finalization`, LCU connect/disconnect)
/// have no plugin-facing message in the Python protocol and are intentionally
/// left unhandled here.
fn spawn_phase_rebroadcast(ctx: BridgeContext, phase: &PhaseHandle) {
    let mut events = phase.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match events.recv().await {
                Ok(PhaseEvent::PhaseChanged { phase, game_mode, map_id, queue_id }) => {
                    ctx.handle.broadcast_phase_change(phase, game_mode, map_id, queue_id);
                }
                Ok(PhaseEvent::ChampionLocked { .. }) => {
                    ctx.handle.broadcast_champion_locked(true);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_server(ctx: BridgeContext, port: u16) {
    let router = ws::router(ctx);
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                log_error!("[bridge] Failed to bind 127.0.0.1:{port}: {e}");
                return;
            }
        };
        log_info!("[bridge] Listening on http://127.0.0.1:{port} (HTTP + WebSocket)");
        if let Err(e) = axum::serve(listener, router).await {
            log_error!("[bridge] Server stopped unexpectedly: {e}");
        }
    });
}
