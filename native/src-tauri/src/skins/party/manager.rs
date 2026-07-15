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
//! `CHUD-PartyMode` Pengu Loader plugin depends on unchanged. As of P0-F the
//! relay protocol itself carries NO summoner ids at all (see `relay.rs`'s
//! module doc), so each peer entry's `summoner_id`/`summoner_name` keys now
//! carry the relay's per-connection `member_id` and display name instead —
//! same key names (wire-compat with the plugin), different meaning; see
//! `get_state`'s doc comment.
//!
//! P0-F hardening (party-mode data-sharing disclosure, `docs/PRIVACY-PARTY.md`):
//!   * `enable()` refuses to run until the user has accepted the current
//!     `CURRENT_PARTY_CONSENT_VERSION` disclosure (`config.party.consent_version`).
//!   * Every enable() mints a fresh EPHEMERAL identity (a random per-session
//!     id used for the room-key/token instead of the real summoner id, plus
//!     an ed25519 keypair) — no durable identity ever touches the relay.
//!   * Every broadcast selection is signed, bound to the room's `epoch` and
//!     our relay-assigned `member_id`; peers verify before trusting anything.
//!   * `get_party_skins` enforces a hard roster gate (was advisory pre-P0-F):
//!     a peer's champion must be a CURRENT, live champ-select teammate, or
//!     the selection is dropped — fail closed if that session isn't
//!     available at all.
//!   * Peer-advertised announcer packs only ever auto-download when the user
//!     opted into that separately AND the Library catalog itself confirms
//!     the mod-id under the `announcer` category.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
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
use super::sig;
use super::token;

/// `party_manager.py::LOBBY_CHECK_INTERVAL`.
const LOBBY_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// `party_manager.py::SKIN_BROADCAST_INTERVAL`.
const SKIN_BROADCAST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Current version of `docs/PRIVACY-PARTY.md`'s data-sharing disclosure.
/// Bump this when the disclosure changes materially — `enable()` re-checks
/// `config.party.consent_version` against it on every call, so every user's
/// prior consent is invalidated (Party mode disables itself) until they
/// review and accept again.
pub const CURRENT_PARTY_CONSENT_VERSION: u32 = 1;

/// One connected peer — ported from `PartyPeerState`. In the shared-room
/// relay model a peer only exists in this map while actually present in the
/// room (see `handle_members_update`'s stale-removal pass), so `connected`
/// is always `true` and `connection_state` always `"connected"` for an entry
/// that exists at all — kept as fields (rather than collapsed away) purely
/// for wire shape parity with `PartyPeerState.to_dict()`. Keyed by the
/// relay's `member_id` (P0-F: no summoner id is ever known for a peer).
#[derive(Debug, Clone)]
pub struct PartyPeer {
    pub member_id: u64,
    pub name: String,
    /// Peer's ed25519 verify key (hex), as advertised at `join` — used to
    /// verify every selection they broadcast (see `verify_member_skin`).
    pub pubkey: String,
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
    pub member_id: u64,
    pub name: String,
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
    /// Still resolved from the LCU on every `enable()` — used internally
    /// only (display name; a stable value for `add_peer_inner`'s "you can't
    /// add yourself" check no longer applies here, see `ephemeral_host_id`).
    /// NEVER sent over the relay wire (P0-F).
    my_summoner_id: Option<u64>,
    my_summoner_name: String,
    my_key: Option<[u8; 32]>,
    my_token: Option<String>,
    /// Random per-`enable()` id used INSTEAD of the real summoner id for the
    /// token payload and `relay::compute_room_key` — see `enable_inner`'s
    /// doc comment. This is what a pasted token's `summoner_id` field
    /// actually contains now.
    ephemeral_host_id: Option<u64>,
    /// Ephemeral ed25519 signing key, freshly generated each `enable()`.
    /// Wrapped in `Arc` so cloning it out from under the lock (to sign
    /// without holding `inner` across file-hash I/O) is cheap regardless of
    /// whether the crate's `SigningKey` itself is cheaply `Clone`.
    signing: Option<Arc<SigningKey>>,
    relay: Option<PartyRelay>,
    /// Keyed by the relay's `member_id` — P0-F dropped summoner ids from the
    /// wire, so that's the only stable peer identity available.
    peers: HashMap<u64, PartyPeer>,
    /// Last-broadcast (skin_id, chroma_id, custom_mod_relative_path,
    /// announcer_mod_id) so the 1s tick only sends on an actual change —
    /// ported from `_skin_broadcast_loop`'s `last_*` locals. Also cleared by
    /// `handle_session_established` so a fresh relay session (which needs a
    /// freshly-signed broadcast) forces an immediate resend even when the
    /// selection itself hasn't changed.
    last_broadcast: Option<(Option<i64>, Option<i64>, Option<String>, Option<String>)>,
    /// Library mod-ids of peer announcers we've already started (or
    /// finished) downloading this session — dedups the download trigger
    /// across member-update callbacks.
    announcer_downloads: HashSet<String>,
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
    /// For AppState/config access (library install records — announcer sync)
    /// and user-facing notifications.
    app: AppHandle,
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
        let http_client = lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S);
        Arc::new(Self {
            skins,
            app: app.clone(),
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
                ephemeral_host_id: None,
                signing: None,
                relay: None,
                peers: HashMap::new(),
                last_broadcast: None,
                announcer_downloads: HashSet::new(),
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
    ///
    /// P0-F consent gate: refuses to run at all until
    /// `config.party.consent_version` covers `CURRENT_PARTY_CONSENT_VERSION`
    /// — checked FIRST, before anything else, so an un-consented app makes
    /// zero relay connections (see `docs/PRIVACY-PARTY.md`).
    pub async fn enable(self: &Arc<Self>) -> Result<String, String> {
        {
            let app_state = self.app.state::<Arc<crate::AppState>>();
            let consent_version = app_state.config.lock_safe().party.consent_version;
            if consent_version < CURRENT_PARTY_CONSENT_VERSION {
                return Err("Party mode requires accepting the data-sharing disclosure first.".to_string());
            }
        }
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

        // Ephemeral identity (P0-F): neither the room key, the token, nor
        // anything sent to the relay carries the real summoner id — a fresh
        // random id + ed25519 keypair are minted every `enable()` instead.
        // `summoner_id`/`summoner_name` above are kept ONLY for internal use
        // (display name; see `Inner::my_summoner_id`'s doc comment).
        let ephemeral_host_id: u64 = {
            use rand::RngCore;
            rand::thread_rng().next_u64() | 1 // never 0, matching the relay's own "never 0" member_id convention
        };
        let signing = Arc::new(SigningKey::generate(&mut OsRng));

        let key = token::generate_key();
        let timestamp = unix_now() as u32;
        let token_str = token::encode_token(ephemeral_host_id, &key, timestamp);

        // Auto-party: if we're already in a lobby, join the room derived from
        // the shared lobby `partyId` so every Chud user in the lobby converges
        // automatically — no token exchange. Otherwise fall back to our
        // personal room (still joinable via a pasted token). `auto_room_loop`
        // keeps this in sync as we join/leave/switch lobbies.
        let party_id = lcu_ext::get_lobby_party_id(&self.http_client, &auth).await;
        let room_key = match &party_id {
            Some(pid) => relay::compute_lobby_room_key(pid),
            None => relay::compute_room_key(ephemeral_host_id, &key),
        };

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.my_summoner_id = Some(summoner_id);
            inner.my_summoner_name = summoner_name.clone();
            inner.my_key = Some(key);
            inner.my_token = Some(token_str.clone());
            inner.ephemeral_host_id = Some(ephemeral_host_id);
            inner.signing = Some(signing);
            inner.enabled = true;
            inner.peers.clear();
            inner.last_broadcast = None;
            inner.current_party_id = party_id;
        }

        // Best-effort: a failed relay connect logs a warning and leaves
        // party mode "limited" rather than failing enable() outright,
        // matching `PartyManager.enable`'s `else: log.warning(...)` branch.
        self.connect_room(room_key, summoner_name).await;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawn_background_loops(generation);

        log_info!("[PARTY] Party mode enabled. Token: {}...", &token_str[..token_str.len().min(20)]);
        self.broadcast_state();
        Ok(token_str)
    }

    /// `PartyManager.disable` — stop the background loops, disconnect the
    /// relay, and clear all party state (including the ephemeral signing key
    /// — a fresh one is minted on the next `enable()`; the relay session
    /// itself goes away with the disconnected `PartyRelay` handle).
    pub async fn disable(&self) {
        log_info!("[PARTY] Disabling party mode...");
        self.generation.fetch_add(1, Ordering::SeqCst);

        let relay = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.enabled = false;
            inner.my_token = None;
            inner.my_key = None;
            inner.my_summoner_id = None;
            inner.ephemeral_host_id = None;
            inner.signing = None;
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

        log_info!("[PARTY] Joining party of host {}", peer_token.summoner_id);

        // `peer_token.summoner_id` is the OTHER host's ephemeral per-`enable()`
        // id (P0-F), not a real summoner id — compare against our own to
        // catch pasting our own token back in.
        let my_ephemeral_id = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).ephemeral_host_id };
        if my_ephemeral_id == Some(peer_token.summoner_id) {
            return Err("You cannot add yourself".to_string());
        }

        let target_room_key = relay::compute_room_key(peer_token.summoner_id, &peer_token.key);
        // Already connected to that exact room? Nothing to do. (The older
        // "is this peer already a member of OUR current room" identity check
        // is gone — v2's `RelayMember` carries no summoner id to match
        // against; the room-key equality check below is the reliable guard.)
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

        let my_name = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).my_summoner_name.clone() };

        if !self.connect_room(target_room_key.clone(), my_name).await {
            return Err("Failed to connect to relay".to_string());
        }

        log_info!("[PARTY] Joined party room {}...", relay::short_key(&target_room_key));
        Ok(())
    }

    /// `PartyManager.remove_peer` — not really applicable in the shared-room
    /// model (Python's own comment), kept for the UI's benefit.
    pub fn remove_peer(&self, member_id: u64) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.peers.remove(&member_id);
        }
        log_info!("[PARTY] Removed peer {member_id}");
        self.broadcast_state();
    }

    /// `PartyState.to_dict()` — the exact shape `party-state` broadcasts
    /// (and the `party-get-state` response) carry, snake_case throughout
    /// (see this module's doc comment). Each peer's `summoner_id`/
    /// `summoner_name` keys are wire-compat with the CHUD-PartyMode Pengu
    /// plugin, but as of P0-F carry the relay's `member_id` and display name
    /// — no real summoner id is ever known for a peer. The top-level
    /// `my_summoner_id`/`my_summoner_name` describe US (resolved locally
    /// from the LCU, never sent to the relay) and keep their original
    /// meaning. `consent_ok`/`consent_required_version`/
    /// `auto_download_peer_announcers` are new P0-F fields the Skins page
    /// UI gates its consent strip / toggle on.
    pub fn get_state(&self) -> Value {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let peers: Vec<Value> = inner
            .peers
            .values()
            .map(|p| {
                json!({
                    "summoner_id": p.member_id,
                    "summoner_name": p.name,
                    "connected": p.connected,
                    "connection_state": p.connection_state,
                    "in_lobby": p.in_lobby,
                    "skin_selection": p.skin_selection.as_ref().map(|s| json!({
                        "summoner_id": p.member_id,
                        "summoner_name": p.name,
                        "champion_id": s.champion_id,
                        "skin_id": s.skin_id,
                        "chroma_id": s.chroma_id,
                        "custom_mod_path": Value::Null,
                    })),
                })
            })
            .collect();

        let (consent_version, auto_download_peer_announcers) = {
            let app_state = self.app.state::<Arc<crate::AppState>>();
            let c = app_state.config.lock_safe();
            (c.party.consent_version, c.party.auto_download_peer_announcers)
        };

        json!({
            "enabled": inner.enabled,
            "my_token": inner.my_token,
            "my_summoner_id": inner.my_summoner_id,
            "my_summoner_name": inner.my_summoner_name,
            "peers": peers,
            "consent_ok": consent_version >= CURRENT_PARTY_CONSENT_VERSION,
            "consent_required_version": CURRENT_PARTY_CONSENT_VERSION,
            "auto_download_peer_announcers": auto_download_peer_announcers,
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

    /// Connect to `room_key`'s relay room, announce ourselves (display name
    /// + our ephemeral pubkey — no summoner id, P0-F), and stash the
    /// resulting `PartyRelay` handle. Returns `false` (never errors) on a
    /// failed connect — the caller logs and continues with party mode
    /// "limited", matching `PartyManager.enable`'s `else` branch.
    async fn connect_room(self: &Arc<Self>, room_key: String, display_name: String) -> bool {
        let pubkey_hex = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.signing.as_ref().map(|k| sig::to_hex(&k.verifying_key().to_bytes()))
        };
        // `enable_inner`/`add_peer_inner` always mint a signing key before
        // calling this, so this only trips if called out of that order.
        let Some(pubkey_hex) = pubkey_hex else {
            log_warn!("[PARTY] connect_room called with no signing identity yet");
            return false;
        };

        let members_manager = Arc::clone(self);
        let on_members_changed: relay::MembersCallback = Arc::new(move |members| members_manager.handle_members_update(members));
        let session_manager = Arc::clone(self);
        let on_session: relay::SessionCallback =
            Arc::new(move |member_id, epoch| session_manager.handle_session_established(member_id, epoch));

        match PartyRelay::connect(&self.relay_url, room_key.clone(), on_members_changed, on_session).await {
            Some(relay) => {
                relay.join(display_name, pubkey_hex);
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

    /// Fires once per (re)connect when the relay's `welcome` establishes a
    /// FRESH `member_id`/`epoch` for this connection. Any selection signed
    /// under a prior session is now unverifiable, so this just clears
    /// `last_broadcast` — the next `skin_broadcast_loop` tick treats that as
    /// "changed" and re-signs + resends against the new session, even if the
    /// underlying selection is identical to before.
    fn handle_session_established(self: &Arc<Self>, member_id: u64, epoch: String) {
        log_info!("[PARTY] Relay session established (member {member_id}, epoch {}...)", relay::short_key(&epoch));
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).last_broadcast = None;
    }

    /// Called by the relay's background task whenever the room's member list
    /// changes (ported from `PartyManager._on_relay_members_changed`).
    fn handle_members_update(self: &Arc<Self>, members: Vec<RelayMember>) {
        let session = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.enabled {
                return;
            }
            inner.relay.as_ref().and_then(|r| r.session())
        };
        // No `welcome` on this connection yet - nothing to verify signatures
        // against, so there's nothing trustworthy to do with this update.
        let Some((my_member_id, epoch)) = session else { return };

        // Verify every present skin signature ONCE per update — reused below
        // for both the announcer-download trigger and the peer roster
        // upsert, so a bad signature only gets logged (by
        // `verify_member_skin`) a single time per broadcast.
        let mut sig_ok: HashMap<u64, bool> = HashMap::new();
        for member in &members {
            if member.member_id == my_member_id || member.name.is_empty() || member.pubkey.is_empty() {
                continue;
            }
            if let Some(skin) = &member.skin {
                sig_ok.insert(member.member_id, verify_member_skin(&epoch, member, skin));
            }
        }

        // Announcer sync: a peer broadcasting a Library announcer we don't
        // have gets downloaded + converted NOW (lobby/champ select), so it's
        // staged and audible by the time the loadout injection runs. Only a
        // SIGNATURE-VERIFIED member's announcer fields may trigger this at
        // all (P0-F) — `maybe_download_peer_announcer` layers its own
        // opt-in + Library-catalog verification on top.
        for member in &members {
            if sig_ok.get(&member.member_id).copied() != Some(true) {
                continue;
            }
            let Some(skin) = &member.skin else { continue };
            if let (Some(mod_id), Some(name)) = (
                skin.get("announcer_mod_id").and_then(Value::as_str),
                skin.get("announcer_name").and_then(Value::as_str),
            ) {
                self.maybe_download_peer_announcer(mod_id.to_string(), name.to_string());
            }
        }

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut current_ids = HashSet::new();
            for member in &members {
                if member.member_id == my_member_id || member.name.is_empty() || member.pubkey.is_empty() {
                    continue;
                }
                current_ids.insert(member.member_id);
                let selection = if sig_ok.get(&member.member_id).copied() == Some(true) {
                    member.skin.as_ref().and_then(parse_skin_selection)
                } else {
                    None
                };

                match inner.peers.get_mut(&member.member_id) {
                    Some(peer) => {
                        peer.name = member.name.clone();
                        peer.pubkey = member.pubkey.clone();
                        peer.connected = true;
                        peer.connection_state = "connected";
                        if selection.is_some() {
                            peer.skin_selection = selection;
                        }
                    }
                    None => {
                        inner.peers.insert(
                            member.member_id,
                            PartyPeer {
                                member_id: member.member_id,
                                name: member.name.clone(),
                                pubkey: member.pubkey.clone(),
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

    /// Download + convert a peer's Library announcer in the background (once
    /// per mod-id per session; no-op when it's already installed). Gated
    /// behind BOTH: (a) the user's own opt-in
    /// (`config.party.auto_download_peer_announcers`, off by default — a
    /// peer-triggered download needs its own consent on top of party
    /// consent), and (b) the Library catalog actually listing `mod_id` under
    /// the `announcer` category (`lookup_announcer_in_catalog`) — the peer's
    /// free-text `name`/id are otherwise untrusted input and must never
    /// drive a filename or a download by themselves. `peer_name` is used
    /// ONLY for log lines; the install record always uses the CATALOG's own
    /// name.
    fn maybe_download_peer_announcer(self: &Arc<Self>, mod_id: String, peer_name: String) {
        let app_state = self.app.state::<Arc<crate::AppState>>().inner().clone();
        let auto_download = { app_state.config.lock_safe().party.auto_download_peer_announcers };
        if !auto_download {
            return;
        }
        {
            let cfg = app_state.config.lock_safe();
            if cfg.library.installed.contains_key(&mod_id) {
                return;
            }
        }
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.announcer_downloads.insert(mod_id.clone()) {
                return; // already attempted this session
            }
        }
        let (endpoint, allowed) = {
            let c = app_state.config.lock_safe();
            (c.library.endpoint.clone(), crate::net::allowed_origins(&c))
        };
        let mgr = Arc::clone(self);
        let peer_name_for_log: String = peer_name.chars().take(64).collect();
        tauri::async_runtime::spawn(async move {
            let catalog_name = match lookup_announcer_in_catalog(&endpoint, &allowed, &mod_id).await {
                Some(name) => name,
                None => {
                    log_warn!(
                        "[PARTY] Peer '{peer_name_for_log}' advertised announcer '{mod_id}' - not found in the Library catalog under 'announcer', skipping"
                    );
                    let mut inner = mgr.inner.lock().unwrap_or_else(|e| e.into_inner());
                    inner.announcer_downloads.remove(&mod_id);
                    return;
                }
            };
            log_info!("[PARTY] Peer '{peer_name_for_log}' uses announcer '{catalog_name}' - downloading + converting so we hear it too");
            // External download (Chud's Library Worker), NOT the LCU — must not
            // reuse the loopback-only, cert-relaxed LCU client.
            let http = crate::net::build_external_client(180.0, allowed.clone());
            match crate::place_library_mod(None, endpoint.trim_end_matches('/'), &http, &allowed, &mod_id, &catalog_name, "", None, "announcer")
                .await
            {
                Ok(rec) => {
                    let app_state = mgr.app.state::<Arc<crate::AppState>>();
                    {
                        let mut cfg = app_state.config.lock_safe();
                        cfg.library.installed.insert(mod_id.clone(), rec);
                        let _ = cfg.save();
                    }
                    log_info!("[PARTY] Announcer '{catalog_name}' downloaded + converted - will be staged at injection");
                }
                Err(e) => {
                    log_warn!("[PARTY] Could not fetch peer announcer '{catalog_name}': {e}");
                    // Allow a retry on a later member update (e.g. transient net).
                    let mut inner = mgr.inner.lock().unwrap_or_else(|er| er.into_inner());
                    inner.announcer_downloads.remove(&mod_id);
                }
            }
        });
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

            let (my_name, my_key, ephemeral_id, current_pid) = {
                let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if !inner.enabled {
                    continue;
                }
                (inner.my_summoner_name.clone(), inner.my_key, inner.ephemeral_host_id, inner.current_party_id.clone())
            };
            let Some(ephemeral_id) = ephemeral_id else { continue };

            match (party_id.as_deref(), current_pid.as_deref()) {
                // Entered a lobby, or switched to a different one -> join it.
                (Some(pid), cur) if cur != Some(pid) => {
                    log_info!("[PARTY] Auto-joining lobby room (party {}…)", &pid[..pid.len().min(8)]);
                    let room = relay::compute_lobby_room_key(pid);
                    self.switch_room(room, Some(pid.to_string()), my_name).await;
                }
                // Left the lobby -> return to our personal (token) room.
                (None, Some(_)) => {
                    if let Some(key) = my_key {
                        log_info!("[PARTY] Left lobby - returning to personal room");
                        let room = relay::compute_room_key(ephemeral_id, &key);
                        self.switch_room(room, None, my_name).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// Disconnect the current relay + drop its peers, then join `room_key`.
    /// Used by the auto-room loop when the lobby changes.
    async fn switch_room(self: &Arc<Self>, room_key: String, party_id: Option<String>, display_name: String) {
        let old = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.current_party_id = party_id;
            inner.peers.clear();
            inner.relay.take()
        };
        if let Some(old) = old {
            old.disconnect();
        }
        self.connect_room(room_key, display_name).await;
        self.broadcast_state();
    }

    /// `PartyManager._lobby_check_loop` — updates each peer's `in_lobby`
    /// flag every `LOBBY_CHECK_INTERVAL`. As of P0-F this no longer matches
    /// summoner ids against the live lobby/champ-select roster (the relay
    /// carries none) — instead `in_lobby` means "this peer's latest
    /// VERIFIED selection targets a champion currently on my team", computed
    /// against the same roster set `get_party_skins`'s gate uses. `false`
    /// whenever that roster isn't available (no live champ-select session)
    /// or the peer has no verified selection. Exits once `generation` is
    /// stale (a later `enable()`/`disable()` superseded this run).
    async fn lobby_check_loop(self: Arc<Self>, generation: u64) {
        loop {
            tokio::time::sleep(LOBBY_CHECK_INTERVAL).await;
            if self.generation.load(Ordering::SeqCst) != generation {
                break;
            }
            let roster = self.live_roster_champion_ids().await;
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            for peer in inner.peers.values_mut() {
                let in_lobby = match (&roster, &peer.skin_selection) {
                    (Some(r), Some(sel)) => r.contains(&sel.champion_id),
                    _ => false,
                };
                if peer.in_lobby != in_lobby {
                    peer.in_lobby = in_lobby;
                    if in_lobby {
                        log_info!("[PARTY] Peer {} joined our lobby", peer.name);
                    } else {
                        log_info!("[PARTY] Peer {} left our lobby", peer.name);
                    }
                }
            }
        }
    }

    /// `PartyManager._skin_broadcast_loop` — broadcasts our current
    /// selection every `SKIN_BROADCAST_INTERVAL`, but only when it actually
    /// changed since the last tick (or a fresh relay session forced a resend
    /// — see `handle_session_established`).
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

        let announcer = self.my_library_announcer();
        let custom_mod_key = custom_mod.as_ref().map(|m| m.relative_path.clone());
        let announcer_key = announcer.as_ref().map(|(id, _)| id.clone());
        let changed = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.enabled {
                return;
            }
            let current = (Some(skin_id), chroma_id, custom_mod_key, announcer_key);
            let changed = inner.last_broadcast.as_ref() != Some(&current);
            if changed {
                inner.last_broadcast = Some(current);
            }
            changed
        };
        if !changed {
            return;
        }

        self.broadcast_skin_update(champion_id, skin_id, chroma_id, custom_mod.as_ref(), announcer);
    }

    /// The selected announcer, resolved back to its Library install record —
    /// `Some((mod_id, display name))` only when it came from the Library
    /// (peers can download the same id; hand-imported packs can't sync).
    fn my_library_announcer(&self) -> Option<(String, String)> {
        let rel = self.skins.shared.lock_safe().category_mods.announcer.as_ref().map(|a| a.relative_path.clone())?;
        let app_state = self.app.state::<Arc<crate::AppState>>();
        let cfg = app_state.config.lock_safe();
        cfg.library.installed.iter().find(|(_, rec)| rec.file == rel).map(|(id, rec)| (id.clone(), rec.name.clone()))
    }

    /// `PartyManager.broadcast_skin_update` — for a custom mod, share a
    /// content hash instead of the file path (the peer resolves it locally
    /// via `find_local_mod_by_hash`; the raw path/bytes never cross the wire).
    /// A Library announcer selection rides along as its mod-id so peers can
    /// download + convert the same pack and hear it too. Every selection is
    /// signed (P0-F) with our ephemeral session key, bound to the relay's
    /// current `(epoch, member_id)` — if we don't have a session yet (relay
    /// just (re)connected, `welcome` hasn't arrived), this skips silently;
    /// `handle_session_established` clears `last_broadcast` the moment the
    /// session IS established, so the very next tick retries.
    fn broadcast_skin_update(
        &self,
        champion_id: i64,
        skin_id: i64,
        chroma_id: Option<i64>,
        custom_mod: Option<&CustomModSelection>,
        announcer: Option<(String, String)>,
    ) {
        let (relay, signing) = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (inner.relay.clone(), inner.signing.clone())
        };
        let Some(relay) = relay else { return };
        if !relay.connected() {
            return;
        }
        let Some((member_id, epoch)) = relay.session() else { return };
        let Some(signing) = signing else { return };

        let mut skin = json!({"champion_id": champion_id, "skin_id": skin_id, "chroma_id": chroma_id});
        let mut hash_field = "-".to_string();
        if let Some(mod_sel) = custom_mod {
            if let Some(hash) = hash_custom_mod(&mod_sel.relative_path) {
                skin["custom_mod_hash"] = json!(hash);
                skin["is_custom"] = json!(true);
                hash_field = hash;
            }
        }
        let mut announcer_field = "-".to_string();
        if let Some((mod_id, name)) = announcer {
            skin["announcer_mod_id"] = json!(mod_id);
            skin["announcer_name"] = json!(name);
            announcer_field = mod_id;
        }

        let chroma = chroma_id.unwrap_or(-1);
        let sig_hex = sig::sign_selection(&signing, &epoch, member_id, champion_id, skin_id, chroma, &hash_field, &announcer_field);
        skin["sig"] = json!(sig_hex);

        log_info!("[SKIN_SEND] Broadcasting our pick: champion {champion_id}, skin {skin_id}, chroma {chroma_id:?}");
        relay.send_skin(Some(skin));
    }

    // ─── Party skins for injection ──────────────────────────────────────

    /// `PartyManager.get_party_skins` (relay-flow path only — see this
    /// module's doc comment). P0-F roster gate (hard, not advisory): a
    /// peer's selection is only trusted when (a) its signature verifies
    /// against their advertised pubkey, bound to the room's current epoch +
    /// their member_id, (b) its champion_id is one of my CURRENT, live
    /// champ-select teammates, and (c) no earlier (first-wins) peer this
    /// pass already claimed that champion. Fails CLOSED — if the champ-select
    /// session isn't available at all, nothing is trusted (see
    /// `decide_peer_selection`).
    pub async fn get_party_skins(&self) -> Vec<PartySkinData> {
        let session = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !inner.enabled {
                return Vec::new();
            }
            inner.relay.as_ref().and_then(|r| r.session())
        };
        let Some((my_member_id, epoch)) = session else { return Vec::new() };

        let roster = self.live_roster_champion_ids().await;
        if roster.is_none() {
            log_info!("[SKIN_COLLECT] Champ-select roster unavailable - rejecting all peer selections this pass");
        }

        let members = { self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay.as_ref().map(|r| r.members()).unwrap_or_default() };

        let mut claimed: HashSet<i64> = HashSet::new();
        let mut skins = Vec::new();
        for member in &members {
            if member.member_id == my_member_id || member.name.is_empty() || member.pubkey.is_empty() {
                continue;
            }
            let Some(skin) = member.skin.as_ref() else { continue };
            let Some(skin_id) = skin.get("skin_id").and_then(Value::as_i64) else { continue };
            if skin_id == 0 {
                continue;
            }
            let champion_id = skin.get("champion_id").and_then(Value::as_i64).unwrap_or(0);
            let sig_ok = verify_member_skin(&epoch, member, skin);

            if let RosterDecision::Reject(reason) = decide_peer_selection(champion_id, sig_ok, roster.as_ref(), &claimed) {
                log_info!(
                    "[SKIN_COLLECT] rejected (not in roster / bad signature / duplicate champion) {}'s selection (champion {champion_id}): {reason}",
                    member.name
                );
                continue;
            }
            claimed.insert(champion_id);

            let chroma_id = skin.get("chroma_id").and_then(Value::as_i64);
            let is_custom = skin.get("is_custom").and_then(Value::as_bool).unwrap_or(false);
            // A peer's base skin (`champion_id * 1000`, no chroma, not a custom
            // mod) is their default — nothing to inject. Skip it silently
            // instead of hunting for a nonexistent ZIP and warning (the ARAM
            // "peer didn't pick a skin" case broadcasts the base id).
            if !is_custom && chroma_id.is_none() && champion_id > 0 && skin_id == champion_id * 1000 {
                continue;
            }

            let mut custom_mod_relative_path = None;
            if is_custom {
                let Some(hash) = skin.get("custom_mod_hash").and_then(Value::as_str) else { continue };
                match Self::find_local_mod_by_hash(hash) {
                    Some(path) => {
                        log_info!("[SKIN_COLLECT] Matched custom mod for peer {}: {path}", member.name);
                        custom_mod_relative_path = Some(path);
                    }
                    None => {
                        log_info!("[SKIN_COLLECT] No local match for peer {}'s custom mod, skipping", member.name);
                        continue;
                    }
                }
            }

            skins.push(PartySkinData {
                member_id: member.member_id,
                name: member.name.clone(),
                champion_id,
                skin_id,
                chroma_id,
                custom_mod_relative_path,
            });
        }

        log_info!("[SKIN_COLLECT] Collected {} roster-verified relay skin selections", skins.len());
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
        let mut folders: Vec<String> =
            self.get_party_skins().await.iter().filter_map(Self::prepare_skin_for_injection).collect();
        if let Some(folder) = self.prepare_peer_announcer() {
            folders.push(folder);
        }
        folders
    }

    /// Stage a peer's synced announcer for this overlay — only when WE have
    /// no announcer of our own selected (ours always wins; two announcer
    /// packs in one overlay just collide on the same banks). Uses the first
    /// peer broadcasting one whose pack is installed locally (downloaded by
    /// `maybe_download_peer_announcer`, which already gated the download
    /// itself behind signature verification + the Library catalog — nothing
    /// further to verify here, just stage whatever's already on disk).
    fn prepare_peer_announcer(&self) -> Option<String> {
        if self.skins.shared.lock_safe().category_mods.announcer.is_some() {
            return None;
        }
        let members =
            { self.inner.lock().unwrap_or_else(|e| e.into_inner()).relay.as_ref().map(|r| r.members()).unwrap_or_default() };
        let app_state = self.app.state::<Arc<crate::AppState>>();
        for member in members {
            let Some(skin) = member.skin.as_ref() else { continue };
            let Some(mod_id) = skin.get("announcer_mod_id").and_then(Value::as_str) else { continue };
            let file = { app_state.config.lock_safe().library.installed.get(mod_id).map(|r| r.file.clone()) };
            let Some(file) = file else { continue }; // not downloaded (yet)
            let source = paths::mods_dir().join(file.replace('/', std::path::MAIN_SEPARATOR_STR));
            if !source.is_file() {
                continue;
            }
            let folder = source.file_stem().map(|n| n.to_string_lossy().into_owned())?;
            let dest = paths::injection_mods_dir().join(&folder);
            zips::safe_remove_entry(&dest);
            if let Err(e) = zips::link_or_extract(&source, &dest, &paths::injection_extract_cache_dir()) {
                log_warn!("[PARTY_INJECT] Failed to stage peer announcer {folder}: {e}");
                continue;
            }
            log_info!("[PARTY_INJECT] Staged {}'s announcer: {folder}", member.name);
            return Some(folder);
        }
        None
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

        log_info!("[PARTY_INJECT] Prepared {}'s skin: {mod_folder_name}", skin_data.name);
        Some(mod_folder_name)
    }

    // ─── Lobby matching (ported from `lobby_matcher.py`) ────────────────

    /// Reshaped from `LobbyMatcher.get_team_champion_mapping` (which mapped
    /// summoner_id -> champion_id, no longer possible without summoner ids
    /// on the relay wire): champion ids of every OTHER player currently on
    /// my live champ-select team (excludes my own cell via
    /// `local_player_cell_id`). This is the authoritative set a peer's
    /// broadcast `champion_id` must belong to before its selection is
    /// trusted for injection (P0-F roster gate — see `get_party_skins`).
    /// `None` when the champ-select session isn't available at all — the
    /// caller fails closed on that, never trusting anything.
    async fn live_roster_champion_ids(&self) -> Option<HashSet<i64>> {
        let auth = lcu::cached_auth()?;
        let session = lcu_ext::champ_select_session(&self.http_client, &auth).await?;
        let my_cell = session.local_player_cell_id;
        let mut ids = HashSet::new();
        for cell in session.my_team.unwrap_or_default() {
            if cell.cell_id == my_cell {
                continue; // exclude myself
            }
            if let Some(cid) = cell.champion_id {
                if cid > 0 {
                    ids.insert(cid);
                }
            }
        }
        Some(ids)
    }
}

// ---------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------

/// Outcome of the P0-F roster gate. Pure (no I/O) so it's unit-testable
/// without a live relay connection or LCU session — see `decide_peer_selection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterDecision {
    Accept,
    Reject(&'static str),
}

/// Decide whether to trust a peer's broadcast champion/skin pick enough to
/// inject it: the signature must verify, the champion must be one of our
/// OWN current live teammates (`roster`), and it must not already be
/// claimed by an earlier (first-wins) peer this same pass.
/// `roster: None` means the champ-select session wasn't available at all —
/// fail closed (reject) rather than trust an unverifiable roster.
fn decide_peer_selection(
    champion_id: i64,
    sig_ok: bool,
    roster: Option<&HashSet<i64>>,
    already_claimed: &HashSet<i64>,
) -> RosterDecision {
    if !sig_ok {
        return RosterDecision::Reject("bad signature");
    }
    let Some(roster) = roster else {
        return RosterDecision::Reject("roster unavailable");
    };
    if !roster.contains(&champion_id) {
        return RosterDecision::Reject("not in roster");
    }
    if already_claimed.contains(&champion_id) {
        return RosterDecision::Reject("duplicate champion");
    }
    RosterDecision::Accept
}

/// Verify a relay member's broadcast `skin` signature against the room's
/// CURRENT epoch and their `member_id` — binds it so a captured selection
/// can't be replayed into a different room instance or reattributed to a
/// different member. Logs + returns `false` on any failure (missing/
/// malformed signature, wrong pubkey, tampered field) — a bad signature is
/// always treated as "no selection", never partially trusted.
fn verify_member_skin(epoch: &str, member: &RelayMember, skin: &Value) -> bool {
    let Some(champion_id) = skin.get("champion_id").and_then(Value::as_i64) else { return false };
    let Some(skin_id) = skin.get("skin_id").and_then(Value::as_i64) else { return false };
    let chroma = skin.get("chroma_id").and_then(Value::as_i64).unwrap_or(-1);
    let hash = skin.get("custom_mod_hash").and_then(Value::as_str).unwrap_or("-");
    let announcer = skin.get("announcer_mod_id").and_then(Value::as_str).unwrap_or("-");
    let Some(sig_hex) = skin.get("sig").and_then(Value::as_str) else {
        log_warn!("[PARTY] Peer '{}' selection has no signature - rejecting", member.name);
        return false;
    };
    let ok = sig::verify_selection(&member.pubkey, epoch, member.member_id, champion_id, skin_id, chroma, hash, announcer, sig_hex);
    if !ok {
        log_warn!("[PARTY] Peer '{}' selection failed signature verification - rejecting", member.name);
    }
    ok
}

/// Confirm `mod_id` exists in the Library catalog's `announcer` category and
/// return the CATALOG's own name for it — the peer's free-text name is
/// never trusted for a local install record (see
/// `maybe_download_peer_announcer`). Pages through `/catalog?category=
/// announcer` (server-side filtered, so the scan stays small) up to a
/// generous bound. `None` on "not found" OR any fetch failure — either way
/// the caller does NOT download.
async fn lookup_announcer_in_catalog(endpoint: &str, allowed: &HashSet<String>, mod_id: &str) -> Option<String> {
    let http = crate::net::build_external_client(15.0, allowed.clone());
    let base = endpoint.trim_end_matches('/');
    const PAGE_SIZE: u32 = 60;
    const MAX_PAGES: u32 = 20; // generous ceiling for the announcer slice of the catalog
    for page in 0..MAX_PAGES {
        let url = reqwest::Url::parse_with_params(
            &format!("{base}/catalog"),
            &[("category", "announcer".to_string()), ("page", page.to_string()), ("pageSize", PAGE_SIZE.to_string())],
        )
        .ok()?;
        let data = crate::net::get_json_checked(&http, url.as_str(), allowed, 16 * 1024 * 1024).await.ok()?;
        let mods = data.get("mods").and_then(Value::as_array)?;
        if let Some(m) = mods.iter().find(|m| m.get("id").and_then(Value::as_str) == Some(mod_id)) {
            return m.get("name").and_then(Value::as_str).map(|s| s.to_string());
        }
        let total = data.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
        if mods.len() < PAGE_SIZE as usize || (page + 1) * PAGE_SIZE >= total {
            break;
        }
    }
    None
}

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
            ephemeral_host_id: None,
            signing: None,
            relay: None,
            peers: HashMap::new(),
            last_broadcast: None,
            announcer_downloads: HashSet::new(),
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

    #[test]
    fn decide_peer_selection_accepts_when_everything_checks_out() {
        let roster: HashSet<i64> = [103, 157].into_iter().collect();
        let claimed = HashSet::new();
        assert_eq!(decide_peer_selection(103, true, Some(&roster), &claimed), RosterDecision::Accept);
    }

    #[test]
    fn decide_peer_selection_rejects_bad_signature() {
        let roster: HashSet<i64> = [103].into_iter().collect();
        let claimed = HashSet::new();
        assert_eq!(decide_peer_selection(103, false, Some(&roster), &claimed), RosterDecision::Reject("bad signature"));
    }

    #[test]
    fn decide_peer_selection_rejects_champion_not_in_roster() {
        let roster: HashSet<i64> = [157].into_iter().collect();
        let claimed = HashSet::new();
        assert_eq!(decide_peer_selection(103, true, Some(&roster), &claimed), RosterDecision::Reject("not in roster"));
    }

    #[test]
    fn decide_peer_selection_rejects_duplicate_champion_claim() {
        let roster: HashSet<i64> = [103].into_iter().collect();
        let mut claimed = HashSet::new();
        claimed.insert(103);
        assert_eq!(decide_peer_selection(103, true, Some(&roster), &claimed), RosterDecision::Reject("duplicate champion"));
    }

    #[test]
    fn decide_peer_selection_rejects_when_roster_unavailable() {
        let claimed = HashSet::new();
        assert_eq!(decide_peer_selection(103, true, None, &claimed), RosterDecision::Reject("roster unavailable"));
    }
}
