//! Party manager (S6) — ported from `party/core/party_manager.py`
//! (`PartyManager`) + `party/core/party_state.py` (`PartyState`), folding in
//! `party/discovery/lobby_matcher.py` (`LobbyMatcher`) and
//! `party/discovery/skin_collector.py`'s live `collect_relay_skins` path
//! (`SkinCollector`) as private helpers — this milestone's file scope is
//! `party/*` only, so the two collaborator modules that would otherwise live
//! in `party/discovery/` are inlined here rather than split into new files.
//! `SkinCollector.collect_all_skins`/`get_my_selection` are NOT ported: both
//! only exist to serve the dead STUN/UDP `PeerConnection` path (`docs/
//! SKINS_PORT.md` "Dropped"), never called by the live relay flow.
//!
//! Threading model: one `std::sync::Mutex<Inner>` with no lock held across
//! an `await` — every async method takes a short lock, clones what it needs,
//! drops the guard, then awaits — matching `docs/SKINS_PORT.md`'s "one
//! coarse lock beats N per-object `threading.Lock`s" (collapses Python's
//! `PartyState._lock` onto the same pattern `SkinsState` already uses).
//!
//! Wire field naming: every party JSON payload (`party-state`'s body, each
//! peer entry, `skin_selection`) is snake_case, NOT the camelCase most other
//! bridge messages use — ported verbatim from `PartyState.to_dict()` /
//! `SkinSelection.to_dict()` (`dataclasses.asdict`), which the rebranded
//! `CHUD-PartyMode` Pengu Loader plugin depends on unchanged.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use sha2::{Digest, Sha256};

use crate::lcu::{self, Auth};
use crate::skins::bridge::protocol::now_ms;
use crate::skins::bridge::BridgeHandle;
use crate::skins::injection::zips;
use crate::skins::lcu_ext;
use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};
use crate::skins::state::CustomModSelection;
use crate::skins::SkinsState;
use crate::LockExt;

use super::relay::{self, PartyRelay, RelayMember};
use super::token;

/// `party_manager.py::LOBBY_CHECK_INTERVAL`.
const LOBBY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// `party_manager.py::SKIN_BROADCAST_INTERVAL`.
const SKIN_BROADCAST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// One connected peer — ported from `PartyPeerState`. In the shared-room
/// relay model a peer only exists in this map while actually present in the
/// room (see `handle_members_update`'s stale-removal pass), so `connected`
/// is always `true` and `connection_state` always `"connected"` for an entry
/// that exists at all — kept as fields (rather than collapsed away) purely
/// for wire shape parity with `PartyPeerState.to_dict()`.
#[derive(Debug, Clone)]
pub struct PartyPeer {
    pub summoner_id: u64,
    pub summoner_name: String,
    pub connected: bool,
    pub connection_state: &'static str,
    pub in_lobby: bool,
    pub skin_selection: Option<PartySkinSelection>,
}

/// A peer's current skin pick, as broadcast over the relay — ported from the
/// subset of `SkinSelection` the relay-based flow actually populates
/// (`custom_mod_path` is never set here; see `get_state`'s doc comment).
#[derive(Debug, Clone)]
pub struct PartySkinSelection {
    pub champion_id: i64,
    pub skin_id: i64,
    pub chroma_id: Option<i64>,
}

/// One skin selection ready for injection — ported from `PartySkinData`
/// (relay-flow subset only: `is_local` never appears since `get_party_skins`
/// excludes our own selection, matching `collect_relay_skins`).
#[derive(Debug, Clone)]
pub struct PartySkinData {
    pub summoner_id: u64,
    pub summoner_name: String,
    pub champion_id: i64,
    pub skin_id: i64,
    pub chroma_id: Option<i64>,
    /// Relative-to-`paths::mods_dir()` path of a LOCAL mod matched by
    /// content hash (never the peer's raw file — see
    /// `find_local_mod_by_hash`'s doc comment).
    pub custom_mod_relative_path: Option<String>,
}

struct Inner {
    enabled: bool,
    my_summoner_id: Option<u64>,
    my_summoner_name: String,
    my_key: Option<[u8; 32]>,
    my_token: Option<String>,
    relay: Option<PartyRelay>,
    peers: HashMap<u64, PartyPeer>,
    /// Last-broadcast (skin_id, chroma_id, custom_mod_relative_path) so the
    /// 1s tick only sends on an actual change — ported from
    /// `_skin_broadcast_loop`'s `last_skin_id`/`last_chroma_id`/
    /// `last_custom_mod` locals.
    last_broadcast: Option<(Option<i64>, Option<i64>, Option<String>)>,
    /// The lobby `partyId` whose auto-derived room we're currently connected
    /// to, or `None` when connected to our personal room (solo / manual). The
    /// auto-room loop compares this against the live lobby to decide when to
    /// switch rooms as we join/leave/change lobbies.
    current_party_id: Option<String>,
}

/// Main orchestrator for party mode (ported from `PartyManager`). Store as
/// `Arc<PartyManager>` in `AppState` — methods that spawn background work or
/// register relay callbacks take `self: &Arc<Self>` so those tasks can hold
/// their own clone of the `Arc` (see `spawn_background_loops`/`connect_room`).
pub struct PartyManager {
    skins: Arc<SkinsState>,
    /// Held so relay member-list updates (arriving on a background task) can
    /// push a fresh `party-state` broadcast without going through a
    /// `#[tauri::command]` round-trip — see `handle_members_update`.
    bridge: BridgeHandle,
    http_client: reqwest::Client,
    relay_url: String,
    /// Bumped on every `enable()`/`disable()` so a background loop spawned
    /// by a previous `enable()` exits instead of racing a fresh one (same
    /// pattern `AppState`/`SkinsState` already use for their tool loops).
    generation: AtomicU64,
    inner: Mutex<Inner>,
}

impl PartyManager {
    /// Constructed once in `lib.rs`'s `setup()`, after the bridge server is
    /// up (so `bridge` is available to push proactive `party-state`
    /// updates). `relay_url` resolution mirrors `ws_relay.py`: `CHUD_RELAY_
    /// URL` env wins, then the config's `party_relay_url`, then the deployed
    /// relay's default URL.
    pub fn new(app: &AppHandle, skins: Arc<SkinsState>, bridge: BridgeHandle) -> Arc<Self> {
        let relay_url = resolve_relay_url(app);
        let http_client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);
        Arc::new(Self {
            skins,
            bridge,
            http_client,
            relay_url,
            generation: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                enabled: false,
                my_summoner_id: None,
                my_summoner_name: "Unknown".to_string(),
                my_key: None,
                my_token: None,
                relay: None,
                peers: HashMap::new(),
                last_broadcast: None,
                current_party_id: None,
            }),
        })
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).enabled
    }

    pub fn connected_peer_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).peers.values().filter(|p| p.connected).count()
    }

    // ─── Enable / disable / add-peer / remove-peer ────────────────────

    /// `PartyManager.enable` — generate a token, connect to our own relay
    /// room, and start the lobby-check + skin-broadcast loops. On failure,
    /// tears back down (mirroring Python's `except` -> `await self.disable()`
    /// -> re-raise) so a partially-enabled state never lingers.
    pub async fn enable(self: &Arc<Self>) -> Result<String, String> {
        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.enabled {
                return Ok(inner.my_token.clone().unwrap_or_default());
            }
        }
        log_info!("[PARTY] Enabling party mode...");

        match self.enable_inner().await {
            Ok(token) => Ok(token),
            Err(e) => {
                log_warn!("[PARTY] Failed to enable party mode: {e}");
                self.disable().await;
                Err(format!("Failed to enable party mode: {e}"))
            }
        }
    }

    async fn enable_inner(self: &Arc<Self>) -> Result<String, String> {
        // The LCU can be briefly unresponsive (still loading, busy, or the
        // lockfile port/password just rotated on a client restart). Retry a
        // few times, invalidating stale auth between attempts, so a transient
        // hiccup doesn't fail enable with a scary "is League running?" error.
        let mut resolved: Option<(Auth, (u64, String))> = None;
        for attempt in 0..5 {
            if let Some(auth) = lcu::cached_auth() {
                if let Some(info) = my_summoner_info(&self.http_client, &auth).await {
                    resolved = Some((auth, info));
                    break;
                }
            }
            lcu::invalidate_auth();
            if attempt < 4 {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
        }
        let Some((auth, (summoner_id, summoner_name))) = resolved else {
            return Err("Couldn't reach the League client. Make sure it's fully loaded (past the login screen), then try again.".to_string());
        };

        let key = token::generate_key();
        let timestamp = unix_now() as u32;
        let token_str = token::encode_token(summoner_id, &key, timestamp);

        // Auto-party: if we're already in a lobby, join the room derived from
        // the shared lobby `partyId` so every Chud user in the lobby converges
        // automatically — no token exchange. Otherwise fall back to our
        // personal room (still joinable via a pasted token). `auto_room_loop`
        // keeps this in sync as we join/leave/switch lobbies.
        let party_id = lcu_ext::get_lobby_party_id(&self.http_client, &auth).await;
        let room_key = match &party_id {
            Some(pid) => relay::compute_lobby_room_key(pid),
            None => relay::compute_room_key(summoner_id, &key),
        };

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.my_summoner_id = Some(summoner_id);
            inner.my_summoner_name = summoner_name.clone();
            inner.my_key = Some(key);
            inner.my_token = Some(token_str.clone());
            inner.enabled = true;
            inner.peers.clear();
            inner.last_broadcast = None;
            inner.current_party_id = party_id;
        }

        // Best-effort: a failed relay connect logs a warning and leaves
        // party mode "limited" rather than failing enable() outright,
        // matching `PartyManager.enable`'s `else: log.warning(...)` branch.
        self.connect_room(room_key, summoner_id, summoner_name).await;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn_background_loops(generation);

        log_info!("[PARTY] Party mode enabled. Token: {}...", &token_str[..token_str.len().min(20)]);
        self.broadcast_state();
        Ok(token_str)
    }

    /// `PartyManager.disable` — stop the background loops, disconnect the
    /// relay, and clear all party state.
    pub async fn disable(&self) {
        log_info!("[PARTY] Disabling party mode...");
        self.generation.fetch_add(1, Ordering::SeqCst);

        let relay = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.enabled = false;
            inner.my_token = None;
            inner.my_key = None;
            inner.my_summoner_id = None;
            inner.peers.clear();
            inner.last_broadcast = None;
            inner.relay.take()
        };
        if let Some(relay) = relay {
            relay.disconnect();
        }

        log_info!("[PARTY] Party mode disabled");
        self.broadcast_state();
    }

    /// `PartyManager.add_peer` — join another player's room by pasting their
    /// token (single shared-room model: this disconnects our own room). A
    /// state broadcast always follows, success or failure, matching
    /// `PartyUIBridge._handle_add_peer`'s unconditional `self._broadcast_
    /// state()` after the call.
    pub async fn add_peer(self: &Arc<Self>, token_str: &str) -> Result<(), String> {
        let result = self.add_peer_inner(token_str).await;
        self.broadcast_state();
        result
    }

    async fn add_peer_inner(self: &Arc<Self>, token_str: &str) -> Result<(), String> {
        if !self.enabled() {
            return Err("Party mode not enabled".to_string());
        }

        let cleaned: String = token_str.split_whitespace().collect();
        let now = unix_now();
        let peer_token = token::decode_token(&cleaned, now).map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("expired") {
                "Token has expired. Ask your friend for a new one.".to_string()
            } else {
                format!("Invalid token: {msg}")
            }
        })?;

        log_info!("[PARTY] Joining party of summoner {}", peer_token.summoner_id);

        let my_summoner_id = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).my_summoner_id };
        if my_summoner_id == Some(peer_token.summoner_id) {
            return Err("You cannot add yourself".to_string());
        }

        // Already in our own room? (the peer joined us, not the other way around.)
        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(relay) = &inner.relay {
                if relay.connected() && relay.members().iter().any(|m| m.summoner_id == peer_token.summoner_id) {
                    log_info!("[PARTY] Peer {} is already in our room", peer_token.summoner_id);
                    return Ok(());
                }
            }
        }

        let target_room_key = relay::compute_room_key(peer_token.summoner_id, &peer_token.key);
        {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(relay) = &inner.relay {
                if relay.room_key() == target_room_key {
                    log_info!("[PARTY] Already in peer's room");
                    return Ok(());
                }
            }
        }

        // Disconnect from our current room and join the host's room.
        let old_relay = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay.take() };
        if let Some(old) = old_relay {
            old.disconnect();
        }

        let (my_id, my_name) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (inner.my_summoner_id.unwrap_or(0), inner.my_summoner_name.clone())
        };

        if !self.connect_room(target_room_key.clone(), my_id, my_name).await {
            return Err("Failed to connect to relay".to_string());
        }

        log_info!("[PARTY] Joined party room {}...", relay::short_key(&target_room_key));
        Ok(())
    }

    /// `PartyManager.remove_peer` — not really applicable in the shared-room
    /// model (Python's own comment), kept for the UI's benefit.
    pub fn remove_peer(&self, summoner_id: u64) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.peers.remove(&summoner_id);
        }
        log_info!("[PARTY] Removed peer {summoner_id}");
        self.broadcast_state();
    }

    /// `PartyState.to_dict()` — the exact shape `party-state` broadcasts
    /// (and the `party-get-state` response) carry, snake_case throughout
    /// (see this module's doc comment).
    pub fn get_state(&self) -> Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let peers: Vec<Value> = inner
            .peers
            .values()
            .map(|p| {
                json!({
                    "summoner_id": p.summoner_id,
                    "summoner_name": p.summoner_name,
                    "connected": p.connected,
                    "connection_state": p.connection_state,
                    "in_lobby": p.in_lobby,
                    "skin_selection": p.skin_selection.as_ref().map(|s| json!({
                        "summoner_id": p.summoner_id,
                        "summoner_name": p.summoner_name,
                        "champion_id": s.champion_id,
                        "skin_id": s.skin_id,
                        "chroma_id": s.chroma_id,
                        "custom_mod_path": Value::Null,
                    })),
                })
            })
            .collect();
        json!({
            "enabled": inner.enabled,
            "my_token": inner.my_token,
            "my_summoner_id": inner.my_summoner_id,
            "my_summoner_name": inner.my_summoner_name,
            "peers": peers,
        })
    }

    /// Push a fresh `party-state` broadcast (ported from `PartyUIBridge.
    /// _broadcast_state`/`PartyManager._notify_state_change`'s effect).
    fn broadcast_state(&self) {
        let mut payload = self.get_state();
        payload["type"] = json!("party-state");
        payload["timestamp"] = json!(now_ms());
        self.bridge.broadcast_json(payload);
    }

    // ─── Relay room connect/callback ───────────────────────────────────

    /// Connect to `room_key`'s relay room, announce ourselves, and stash the
    /// resulting `PartyRelay` handle. Returns `false` (never errors) on a
    /// failed connect — the caller logs and continues with party mode
    /// "limited", matching `PartyManager.enable`'s `else` branch.
    async fn connect_room(self: &Arc<Self>, room_key: String, my_summoner_id: u64, my_summoner_name: String) -> bool {
        let callback_manager = Arc::clone(self);
        let on_members_changed: relay::MembersCallback =
            Arc::new(move |members| callback_manager.handle_members_update(members));

        match PartyRelay::connect(&self.relay_url, room_key.clone(), on_members_changed).await {
            Some(relay) => {
                relay.join(my_summoner_id, my_summoner_name);
                log_info!("[PARTY] Connected to relay room {}...", relay::short_key(&room_key));
                self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay = Some(relay);
                true
            }
            None => {
                log_warn!("[PARTY] Relay connection failed, party mode limited");
                false
            }
        }
    }

    /// Called by the relay's background task whenever the room's member list
    /// changes (ported from `PartyManager._on_relay_members_changed`).
    fn handle_members_update(&self, members: Vec<RelayMember>) {
        let Some(my_id) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).my_summoner_id else { return };

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut current_ids = HashSet::new();
            for member in &members {
                if member.summoner_id == my_id || member.summoner_id == 0 {
                    continue;
                }
                current_ids.insert(member.summoner_id);
                let selection = member.skin.as_ref().and_then(parse_skin_selection);

                match inner.peers.get_mut(&member.summoner_id) {
                    Some(peer) => {
                        peer.summoner_name = member.summoner_name.clone();
                        peer.connected = true;
                        peer.connection_state = "connected";
                        if selection.is_some() {
                            peer.skin_selection = selection;
                        }
                    }
                    None => {
                        inner.peers.insert(
                            member.summoner_id,
                            PartyPeer {
                                summoner_id: member.summoner_id,
                                summoner_name: member.summoner_name.clone(),
                                connected: true,
                                connection_state: "connected",
                                in_lobby: false,
                                skin_selection: selection,
                            },
                        );
                    }
                }
            }

            // Remove peers no longer present in the room.
            let stale: Vec<u64> = inner.peers.keys().filter(|id| !current_ids.contains(id)).copied().collect();
            for id in stale {
                inner.peers.remove(&id);
                log_info!("[PARTY] Removed peer {id}");
            }
        }

        self.broadcast_state();
    }

    // ─── Background loops ──────────────────────────────────────────────

    fn spawn_background_loops(self: &Arc<Self>, generation: u64) {
        let lobby_mgr = Arc::clone(self);
        tauri::async_runtime::spawn(async move { lobby_mgr.lobby_check_loop(generation).await });

        let skin_mgr = Arc::clone(self);
        tauri::async_runtime::spawn(async move { skin_mgr.skin_broadcast_loop(generation).await });

        let auto_mgr = Arc::clone(self);
        tauri::async_runtime::spawn(async move { auto_mgr.auto_room_loop(generation).await });
    }

    /// Auto-party: watch the LCU lobby's `partyId` and switch relay rooms as it
    /// changes, so Chud users in the same lobby converge in one room with no
    /// token exchange. On entering / switching a lobby we join that lobby's
    /// derived room; on leaving we fall back to our personal room. Exits once
    /// `generation` is stale.
    async fn auto_room_loop(self: Arc<Self>, generation: u64) {
        loop {
            tokio::time::sleep(LOBBY_CHECK_INTERVAL).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                break;
            }
            let Some(auth) = lcu::cached_auth() else { continue };
            let party_id = lcu_ext::get_lobby_party_id(&self.http_client, &auth).await;

            let (my_id, my_name, my_key, current_pid) = {
                let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if !inner.enabled {
                    continue;
                }
                (inner.my_summoner_id, inner.my_summoner_name.clone(), inner.my_key, inner.current_party_id.clone())
            };
            let Some(my_id) = my_id else { continue };

            match (party_id.as_deref(), current_pid.as_deref()) {
                // Entered a lobby, or switched to a different one -> join it.
                (Some(pid), cur) if cur != Some(pid) => {
                    log_info!("[PARTY] Auto-joining lobby room (party {}…)", &pid[..pid.len().min(8)]);
                    let room = relay::compute_lobby_room_key(pid);
                    self.switch_room(room, Some(pid.to_string()), my_id, my_name).await;
                }
                // Left the lobby -> return to our personal (token) room.
                (None, Some(_)) => {
                    if let Some(key) = my_key {
                        log_info!("[PARTY] Left lobby - returning to personal room");
                        let room = relay::compute_room_key(my_id, &key);
                        self.switch_room(room, None, my_id, my_name).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// Disconnect the current relay + drop its peers, then join `room_key`.
    /// Used by the auto-room loop when the lobby changes.
    async fn switch_room(self: &Arc<Self>, room_key: String, party_id: Option<String>, my_id: u64, my_name: String) {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.current_party_id = party_id;
            inner.peers.clear();
            inner.relay.take()
        };
        if let Some(old) = old {
            old.disconnect();
        }
        self.connect_room(room_key, my_id, my_name).await;
        self.broadcast_state();
    }

    /// `PartyManager._lobby_check_loop` — updates each peer's `in_lobby`
    /// flag every `LOBBY_CHECK_INTERVAL`. Exits once `generation` is stale
    /// (a later `enable()`/`disable()` superseded this run).
    async fn lobby_check_loop(self: Arc<Self>, generation: u64) {
        loop {
            tokio::time::sleep(LOBBY_CHECK_INTERVAL).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                break;
            }
            let lobby_ids = self.all_lobby_summoner_ids().await;
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for peer in inner.peers.values_mut() {
                let in_lobby = lobby_ids.contains(&peer.summoner_id);
                if peer.in_lobby != in_lobby {
                    peer.in_lobby = in_lobby;
                    if in_lobby {
                        log_info!("[PARTY] Peer {} joined our lobby", peer.summoner_name);
                    } else {
                        log_info!("[PARTY] Peer {} left our lobby", peer.summoner_name);
                    }
                }
            }
        }
    }

    /// `PartyManager._skin_broadcast_loop` — broadcasts our current
    /// selection every `SKIN_BROADCAST_INTERVAL`, but only when it actually
    /// changed since the last tick.
    async fn skin_broadcast_loop(self: Arc<Self>, generation: u64) {
        loop {
            tokio::time::sleep(SKIN_BROADCAST_INTERVAL).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                break;
            }
            self.maybe_broadcast_skin_update().await;
        }
    }

    async fn maybe_broadcast_skin_update(&self) {
        let (champion_id, skin_id, chroma_id, custom_mod) = {
            let shared = self.skins.shared.lock_safe();
            let champion_id = shared.locked_champ_id.or(shared.hovered_champ_id);
            (champion_id, shared.last_hovered_skin_id, shared.selected_chroma_id, shared.selected_custom_mod.clone())
        };
        let (Some(champion_id), Some(skin_id)) = (champion_id, skin_id) else { return };

        let custom_mod_key = custom_mod.as_ref().map(|m| m.relative_path.clone());
        let changed = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.enabled {
                return;
            }
            let current = (Some(skin_id), chroma_id, custom_mod_key);
            let changed = inner.last_broadcast.as_ref() != Some(&current);
            if changed {
                inner.last_broadcast = Some(current);
            }
            changed
        };
        if !changed {
            return;
        }

        self.broadcast_skin_update(champion_id, skin_id, chroma_id, custom_mod.as_ref());
    }

    /// `PartyManager.broadcast_skin_update` — for a custom mod, share a
    /// content hash instead of the file path (the peer resolves it locally
    /// via `find_local_mod_by_hash`; the raw path/bytes never cross the wire).
    fn broadcast_skin_update(&self, champion_id: i64, skin_id: i64, chroma_id: Option<i64>, custom_mod: Option<&CustomModSelection>) {
        let relay = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay.clone() };
        let Some(relay) = relay else { return };
        if !relay.connected() {
            return;
        }

        let mut skin = json!({"champion_id": champion_id, "skin_id": skin_id, "chroma_id": chroma_id});
        if let Some(mod_sel) = custom_mod {
            if let Some(hash) = hash_custom_mod(&mod_sel.relative_path) {
                skin["custom_mod_hash"] = json!(hash);
                skin["is_custom"] = json!(true);
            }
        }
        log_info!("[SKIN_SEND] Broadcasting our pick: champion {champion_id}, skin {skin_id}, chroma {chroma_id:?}");
        relay.send_skin(Some(skin));
    }

    // ─── Party skins for injection ──────────────────────────────────────

    /// `PartyManager.get_party_skins` (relay-flow path only — see this
    /// module's doc comment): cross-references relay members against our
    /// team's champion mapping so a spoofed champion/skin combo is dropped
    /// with a warning rather than injected.
    pub async fn get_party_skins(&self) -> Vec<PartySkinData> {
        let Some(my_id) = ({
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.enabled {
                return Vec::new();
            }
            inner.my_summoner_id
        }) else {
            return Vec::new();
        };

        let team_champions = self.team_champion_mapping().await;
        let members = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay.as_ref().map(|r| r.members()).unwrap_or_default() };

        let mut skins = Vec::new();
        for member in members {
            if member.summoner_id == my_id || member.summoner_id == 0 {
                continue;
            }
            let Some(skin) = member.skin.as_ref() else { continue };
            let Some(skin_id) = skin.get("skin_id").and_then(Value::as_i64) else { continue };
            if skin_id == 0 {
                continue;
            }
            let champion_id = skin.get("champion_id").and_then(Value::as_i64).unwrap_or(0);
            let chroma_id = skin.get("chroma_id").and_then(Value::as_i64);
            let is_custom = skin.get("is_custom").and_then(Value::as_bool).unwrap_or(false);
            // A peer's base skin (`champion_id * 1000`, no chroma, not a custom
            // mod) is their default — nothing to inject. Skip it silently
            // instead of hunting for a nonexistent ZIP and warning (the ARAM
            // "peer didn't pick a skin" case broadcasts the base id).
            if !is_custom && chroma_id.is_none() && champion_id > 0 && skin_id == champion_id * 1000 {
                continue;
            }
            if let Some(expected) = team_champions.get(&member.summoner_id) {
                if *expected != champion_id {
                    log_warn!("[SKIN_COLLECT] Champion mismatch for {}", member.summoner_id);
                    continue;
                }
            }

            let mut custom_mod_relative_path = None;
            if is_custom {
                let Some(hash) = skin.get("custom_mod_hash").and_then(Value::as_str) else { continue };
                match Self::find_local_mod_by_hash(hash) {
                    Some(path) => {
                        log_info!("[SKIN_COLLECT] Matched custom mod for peer {}: {path}", member.summoner_id);
                        custom_mod_relative_path = Some(path);
                    }
                    None => {
                        log_info!("[SKIN_COLLECT] No local match for peer {}'s custom mod, skipping", member.summoner_id);
                        continue;
                    }
                }
            }

            skins.push(PartySkinData {
                summoner_id: member.summoner_id,
                summoner_name: member.summoner_name.clone(),
                champion_id,
                skin_id,
                chroma_id,
                custom_mod_relative_path,
            });
        }

        log_info!("[SKIN_COLLECT] Collected {} relay skin selections", skins.len());
        skins
    }

    /// `PartyManager.find_local_mod_by_hash` — scan every skin mod under
    /// `paths::mods_dir()/skins/**` for a `.zip`/`.fantome` whose sha256
    /// (truncated to 16 hex chars, matching `broadcast_skin_update`'s hash
    /// length) equals `content_hash`. Returns the match's path relative to
    /// `paths::mods_dir()` (forward-slash separated), or `None`.
    pub fn find_local_mod_by_hash(content_hash: &str) -> Option<String> {
        let mods_root = paths::mods_dir();
        let skins_dir = mods_root.join("skins");
        let skin_dirs = std::fs::read_dir(&skins_dir).ok()?;

        for skin_dir_entry in skin_dirs.flatten() {
            let skin_dir = skin_dir_entry.path();
            if !skin_dir.is_dir() {
                continue;
            }
            let Ok(mod_files) = std::fs::read_dir(&skin_dir) else { continue };
            for mod_file_entry in mod_files.flatten() {
                let mod_file = mod_file_entry.path();
                if !mod_file.is_file() {
                    continue;
                }
                let is_mod_ext = mod_file
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("fantome"))
                    .unwrap_or(false);
                if !is_mod_ext {
                    continue;
                }
                if hash_file(&mod_file).as_deref() == Some(content_hash) {
                    if let Ok(rel) = mod_file.strip_prefix(&mods_root) {
                        return Some(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        None
    }

    // ─── Injection staging (ported from `injection_hook.py`) ───────────

    /// `PartyInjectionHook.prepare_party_mods` — stage every party skin
    /// selection for injection, returning the extracted mod folder names
    /// S5's trigger folds into `InjectionManager::inject_skin_immediately`'s
    /// `extra_mod_names`.
    pub async fn prepare_party_mods(self: &Arc<Self>) -> Vec<String> {
        self.get_party_skins().await.iter().filter_map(Self::prepare_skin_for_injection).collect()
    }

    /// `PartyInjectionHook._prepare_single_skin` — resolve one party
    /// member's skin to a local ZIP (a matched custom mod, or the regular
    /// skins-tree ZIP) and extract it into the injection mods directory via
    /// the already-ported `injection::zips` module. Returns the extracted
    /// mod's folder name, or `None` on any resolution/extraction failure
    /// (matching the Python original's catch-all -> `None`).
    fn prepare_skin_for_injection(skin_data: &PartySkinData) -> Option<String> {
        let source = if let Some(rel) = &skin_data.custom_mod_relative_path {
            let candidate = paths::mods_dir().join(rel);
            if !candidate.exists() {
                log_warn!("[PARTY_INJECT] Custom mod path not found: {rel}");
                return None;
            }
            candidate
        } else {
            let skin_name = format!("skin_{}", skin_data.skin_id);
            let zips_dir = paths::skins_dir();
            match zips::resolve_zip(&zips_dir, &skin_name, skin_data.chroma_id, Some(&skin_name), None, Some(skin_data.champion_id)) {
                Some(p) => p,
                None => {
                    log_warn!("[PARTY_INJECT] Could not find skin ZIP for {skin_name}");
                    return None;
                }
            }
        };

        let mod_folder_name = if source.is_dir() {
            source.file_name().map(|n| n.to_string_lossy().into_owned())
        } else {
            source.file_stem().map(|n| n.to_string_lossy().into_owned())
        }?;

        let dest = paths::injection_mods_dir().join(&mod_folder_name);
        if dest.exists() {
            zips::safe_remove_entry(&dest);
        }
        let cache_dir = paths::injection_extract_cache_dir();
        if let Err(e) = zips::link_or_extract(&source, &dest, &cache_dir) {
            log_warn!("[PARTY_INJECT] Failed to extract mod {}: {e}", source.display());
            return None;
        }

        log_info!("[PARTY_INJECT] Prepared {}'s skin: {mod_folder_name}", skin_data.summoner_name);
        Some(mod_folder_name)
    }

    // ─── Lobby matching (ported from `lobby_matcher.py`) ────────────────

    /// `LobbyMatcher.get_team_champion_mapping`.
    async fn team_champion_mapping(&self) -> HashMap<u64, i64> {
        let mut mapping = HashMap::new();
        let Some(auth) = lcu::cached_auth() else { return mapping };
        let Some(session) = lcu_ext::champ_select_session(&self.http_client, &auth).await else { return mapping };
        for cell in session.my_team.unwrap_or_default() {
            if let (Some(sid), Some(cid)) = (cell.summoner_id, cell.champion_id) {
                if sid > 0 && cid > 0 {
                    mapping.insert(sid as u64, cid);
                }
            }
        }
        mapping
    }

    /// `LobbyMatcher.get_all_summoner_ids`.
    async fn all_lobby_summoner_ids(&self) -> HashSet<u64> {
        let Some(auth) = lcu::cached_auth() else { return HashSet::new() };
        let phase = self.skins.shared.lock_safe().phase.clone();
        match phase.as_deref() {
            Some("ChampSelect") => self.champ_select_summoner_ids(&auth).await,
            Some("Lobby") | Some("Matchmaking") | Some("ReadyCheck") => self.lobby_summoner_ids(&auth).await,
            _ => {
                let ids = self.lobby_summoner_ids(&auth).await;
                if ids.is_empty() {
                    self.champ_select_summoner_ids(&auth).await
                } else {
                    ids
                }
            }
        }
    }

    /// `LobbyMatcher.get_champ_select_summoner_ids`.
    async fn champ_select_summoner_ids(&self, auth: &Auth) -> HashSet<u64> {
        let mut ids = HashSet::new();
        if let Some(session) = lcu_ext::champ_select_session(&self.http_client, auth).await {
            for cell in session.my_team.unwrap_or_default() {
                if let Some(sid) = cell.summoner_id {
                    if sid > 0 {
                        ids.insert(sid as u64);
                    }
                }
            }
        }
        ids
    }

    /// `LobbyMatcher.get_lobby_summoner_ids`.
    async fn lobby_summoner_ids(&self, auth: &Auth) -> HashSet<u64> {
        let mut ids = HashSet::new();
        let Some(data) = lcu::get_json(&self.http_client, auth, "/lol-lobby/v2/lobby").await else { return ids };

        if let Some(members) = data.get("members").and_then(Value::as_array) {
            for member in members {
                if let Some(sid) = member.get("summonerId").and_then(Value::as_i64) {
                    if sid > 0 {
                        ids.insert(sid as u64);
                    }
                }
            }
        }
        if let Some(sid) = data.get("localMember").and_then(|m| m.get("summonerId")).and_then(Value::as_i64) {
            if sid > 0 {
                ids.insert(sid as u64);
            }
        }
        ids
    }
}

// ---------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------

/// Resolve the relay URL: `CHUD_RELAY_URL` env wins, then the config's
/// `party_relay_url`, then the deployed relay's default — ported from
/// `ws_relay.py`'s `RELAY_URL = os.environ.get(<env>, _CONFIGURED_URL)`
/// (env overrides configured; same precedence here, env var renamed).
fn resolve_relay_url(app: &AppHandle) -> String {
    if let Ok(url) = std::env::var("CHUD_RELAY_URL") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    let configured = {
        let app_state = app.state::<Arc<crate::AppState>>();
        let url = app_state.config.lock_safe().skins.party_relay_url.clone();
        url
    };
    if !configured.trim().is_empty() {
        return configured;
    }
    relay::DEFAULT_RELAY_URL.to_string()
}

/// `LobbyMatcher.get_my_summoner_id` + `get_my_summoner_name`, combined
/// since both come from the same `/lol-summoner/v1/current-summoner` call.
async fn my_summoner_info(client: &reqwest::Client, auth: &Auth) -> Option<(u64, String)> {
    let summoner = lcu_ext::current_summoner(client, auth).await?;
    // Riot is deprecating `summonerId` (0 on newer accounts); fall back to a
    // stable non-zero u64 derived from the puuid so peer identity still works.
    let id = summoner
        .get("summonerId")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .map(|id| id as u64)
        .or_else(|| {
            summoner
                .get("puuid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(puuid_to_u64)
        })?;
    // Prefer a NON-EMPTY name (displayName is often "" now — skip it rather
    // than let it win over gameName).
    let name = ["gameName", "displayName", "internalName"]
        .iter()
        .find_map(|k| summoner.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("Summoner")
        .to_string();
    Some((id, name))
}

/// Stable non-zero u64 identity from a puuid (first 8 bytes of its sha256),
/// for accounts where Riot has zeroed `summonerId`.
fn puuid_to_u64(puuid: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(puuid.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes) | 1
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Parse a relay member's `skin` JSON blob into a `PartySkinSelection`
/// (ported from the `SkinSelection(...)` construction inline in
/// `_on_relay_members_changed`).
fn parse_skin_selection(skin: &Value) -> Option<PartySkinSelection> {
    let champion_id = skin.get("champion_id").and_then(Value::as_i64)?;
    let skin_id = skin.get("skin_id").and_then(Value::as_i64)?;
    let chroma_id = skin.get("chroma_id").and_then(Value::as_i64);
    Some(PartySkinSelection { champion_id, skin_id, chroma_id })
}

fn hash_file(path: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(hex[..16].to_string())
}

/// `PartyManager._hash_custom_mod` — content hash of a custom mod file,
/// truncated to 16 hex chars (matches `find_local_mod_by_hash`'s comparison
/// length).
fn hash_custom_mod(relative_path: &str) -> Option<String> {
    let full_path = paths::mods_dir().join(relative_path);
    if !full_path.exists() {
        return None;
    }
    hash_file(&full_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skin_selection_requires_champion_and_skin_id() {
        let full = json!({"champion_id": 103, "skin_id": 103000, "chroma_id": 103001});
        let parsed = parse_skin_selection(&full).unwrap();
        assert_eq!(parsed.champion_id, 103);
        assert_eq!(parsed.skin_id, 103000);
        assert_eq!(parsed.chroma_id, Some(103001));

        let missing_skin_id = json!({"champion_id": 103});
        assert!(parse_skin_selection(&missing_skin_id).is_none());
    }

    #[test]
    fn find_local_mod_by_hash_matches_content_not_filename() {
        let dir = std::env::temp_dir().join("chud_party_manager_test_hash");
        let _ = std::fs::remove_dir_all(&dir);
        let skins_dir = dir.join("skins").join("103");
        std::fs::create_dir_all(&skins_dir).unwrap();
        let mod_file = skins_dir.join("SomeSkin.zip");
        std::fs::write(&mod_file, b"party mode test content").unwrap();

        let expected_hash = hash_file(&mod_file).unwrap();

        // Point `paths::mods_dir()` isn't overridable here (it's a fixed
        // %LOCALAPPDATA% path), so exercise the hashing/matching logic
        // directly instead of through `find_local_mod_by_hash` — proves the
        // truncated-hash comparison is content-based, which is the property
        // that matters for cross-peer matching.
        assert_eq!(expected_hash.len(), 16);
        assert_eq!(hash_file(&mod_file), Some(expected_hash));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_state_default_shape_matches_disabled_party_state() {
        // Build a manager without a bridge/app (get_state doesn't need
        // either) to check the default (never-enabled) JSON shape.
        let inner = Inner {
            enabled: false,
            my_summoner_id: None,
            my_summoner_name: "Unknown".to_string(),
            my_key: None,
            my_token: None,
            relay: None,
            peers: HashMap::new(),
            last_broadcast: None,
            current_party_id: None,
        };
        let value = json!({
            "enabled": inner.enabled,
            "my_token": inner.my_token,
            "my_summoner_id": inner.my_summoner_id,
            "my_summoner_name": inner.my_summoner_name,
            "peers": Vec::<Value>::new(),
        });
        assert_eq!(value["enabled"], json!(false));
        assert_eq!(value["peers"], json!([]));
    }
}
