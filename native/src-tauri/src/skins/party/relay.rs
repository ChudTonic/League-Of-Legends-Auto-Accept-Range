//! Relay websocket client (S6) — ported from `party/network/ws_relay.py`
//! (`PartyRelay`). Connects to the already-deployed Cloudflare Worker relay
//! (`relay-worker/`, service name `chud-party-relay`) and speaks its PROTOCOL
//! V2 wire contract exactly — see that crate's `src/lib.rs` module doc, which
//! this client must match byte-for-byte:
//!   client -> server: `{"type":"join","name":str,"pubkey":hex}`
//!                     `{"type":"skin","skin":{...}|null}`
//!                     `{"type":"leave"}`
//!                     bare TEXT frame `"ping"` (keepalive @25s — NOT a
//!                     WebSocket control-frame ping; the worker string-matches
//!                     literal text and replies with literal text `"pong"`).
//!   server -> client: `{"type":"welcome","member_id":u64,"epoch":hex}` (sent
//!                     once per connect — a FRESH `member_id` every time; see
//!                     [`SessionCallback`])
//!                     `{"type":"members","epoch":hex,
//!                      "members":[{member_id,name,pubkey,skin?},...]}`
//!                     (full roster, sent on every join/skin/leave — no diffs)
//!
//! v2 (P0-F) carries NO summoner ids at all: the server assigns each socket a
//! random `member_id` clients cannot claim, and clients identify themselves
//! only by a display `name` + an ed25519 `pubkey`. Every selection is signed
//! (bound to the room's `epoch` + our `member_id`) by the party manager, not
//! this module — this client just relays bytes and hands the manager the
//! `welcome`/`members` payloads to act on.
//!
//! The relay itself has a real (Cloudflare-issued) TLS cert, unlike the LCU's
//! self-signed loopback cert `lcu_ws.rs` has to special-case — so this client
//! uses plain `tokio_tungstenite::connect_async` (default cert validation)
//! rather than a `danger_accept_invalid_certs` connector.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::skins::slog::{log_info, log_warn};

/// `ws_relay.py::PING_INTERVAL`.
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// Initial-connect timeout (`PartyRelay.connect`'s `timeout: float = 15.0` default).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Delay before retrying after an unexpected disconnect (not in the Python
/// original, which never reconnects — see this module's fix-list: Chud adds
/// auto-reconnect on top of the ported behavior).
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub const DEFAULT_RELAY_URL: &str = "wss://chud-party-relay.jivy26.workers.dev";

/// `compute_room_key` — `sha256(str(host_summoner_id).encode() + host_key).hexdigest()[:32]`,
/// byte-exact with `ws_relay.py::compute_room_key` so a host and any joiner
/// pasting their token independently derive the identical room key. As of
/// P0-F the first argument is an EPHEMERAL per-`enable()` id, not a real
/// summoner id — see `party::manager::enable_inner`'s doc comment; the hash
/// itself doesn't care what the number means.
pub fn compute_room_key(host_summoner_id: u64, host_key: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(host_summoner_id.to_string().as_bytes());
    hasher.update(host_key);
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..32].to_string()
}

/// Deterministic room key for a League lobby. Every member of a lobby sees the
/// same `partyId` (a GUID) from the LCU, so every Chud user in that lobby
/// derives the SAME room and auto-joins it — no token exchange needed. The
/// `"chud-lobby:"` prefix namespaces it so it can never collide with a
/// personal `compute_room_key` room.
pub fn compute_lobby_room_key(party_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"chud-lobby:");
    hasher.update(party_id.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..32].to_string()
}

/// One entry from a `{"type":"members",...}` broadcast. `member_id` is
/// server-assigned (never client-chosen); `name`/`pubkey` are empty for a
/// socket that connected but hasn't sent a valid `join` yet.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RelayMember {
    pub member_id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub pubkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<Value>,
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Outgoing commands queued from `PartyRelay`'s public methods to the
/// connection task — mirrors the Python original's ad hoc `_send_json` call
/// sites (`join`/`send_skin`/`disconnect`'s `{"type":"leave"}`).
enum OutCmd {
    Join { name: String, pubkey: String },
    Skin(Option<Value>),
    Leave,
}

/// Members-changed callback type — `set_on_members_changed`'s Rust
/// equivalent. `Fn`, not `FnMut`: the connection task calls it from a single
/// place but may call it many times over the relay's lifetime.
pub type MembersCallback = Arc<dyn Fn(Vec<RelayMember>) + Send + Sync>;

/// Session-established callback: fired once per (re)connect, right after the
/// server's `welcome` (a FRESH `member_id` + the room's `epoch`). The party
/// manager uses this to (re)sign and rebroadcast its current selection bound
/// to the new identity — a selection signed under a PRIOR member_id/epoch is
/// unverifiable once the socket reconnects, so it is never replayed (see
/// `RelayShared::last_join`'s doc comment).
pub type SessionCallback = Arc<dyn Fn(u64, String) + Send + Sync>;

struct RelayShared {
    connected: AtomicBool,
    /// Cleared by `disconnect()` so the reconnect loop stops trying after an
    /// intentional leave (as opposed to a transient drop, which retries).
    should_run: AtomicBool,
    members: StdMutex<Vec<RelayMember>>,
    /// Last `join` we sent (name, pubkey), so a RECONNECTED socket can
    /// re-announce itself — without this a client that drops + reconnects
    /// becomes a nameless ghost in the room until the user manually
    /// re-enables party mode. The last SKIN is deliberately NOT kept here
    /// (v2): its signature is bound to the OLD member_id + epoch and would
    /// fail verification under the fresh session a reconnect gets, so
    /// `on_session` triggers the manager to re-sign and rebroadcast instead.
    last_join: StdMutex<Option<(String, String)>>,
    /// This connection's server-assigned identity, `(member_id, epoch)`, set
    /// from the `welcome` message and kept fresh (epoch only) by every
    /// `members` broadcast. `None` until the first `welcome` arrives.
    my_session: StdMutex<Option<(u64, String)>>,
}

/// WebSocket connection to a shared party room (ported from `PartyRelay`).
/// A cheap, cloneable handle: the actual socket lives on a spawned tokio
/// task; this struct is just a command sender + a shared members snapshot.
#[derive(Clone)]
pub struct PartyRelay {
    room_key: String,
    shared: Arc<RelayShared>,
    cmd_tx: mpsc::UnboundedSender<OutCmd>,
}

impl PartyRelay {
    /// `PartyRelay.connect` — attempts the initial connection synchronously
    /// (bounded by `CONNECT_TIMEOUT`, matching the Python default) and, on
    /// success, spawns the background task that owns the socket for the rest
    /// of this relay's life (keepalive + receive + auto-reconnect). Returns
    /// `None` on initial-connect failure, exactly like Python's `connect()`
    /// returning `False` — the caller logs "party mode limited" and moves on
    /// without starting any loops (mirrors `PartyManager.enable`).
    pub async fn connect(
        relay_url: &str,
        room_key: String,
        on_members_changed: MembersCallback,
        on_session: SessionCallback,
    ) -> Option<Self> {
        let url = format!("{relay_url}/room?key={room_key}");
        log_info!("[RELAY] Connecting to room {}...", short_key(&room_key));

        let first_ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect_async(&url)).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(e)) => {
                log_warn!("[RELAY] Connection failed: {e}");
                return None;
            }
            Err(_) => {
                log_warn!("[RELAY] Connection timed out after {CONNECT_TIMEOUT:?}");
                return None;
            }
        };
        log_info!("[RELAY] Connected");

        let shared = Arc::new(RelayShared {
            connected: AtomicBool::new(true),
            should_run: AtomicBool::new(true),
            members: StdMutex::new(Vec::new()),
            last_join: StdMutex::new(None),
            my_session: StdMutex::new(None),
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let relay_url = relay_url.to_string();
        let task_room_key = room_key.clone();
        let task_shared = shared.clone();
        tauri::async_runtime::spawn(async move {
            run_connection(first_ws, relay_url, task_room_key, task_shared, cmd_rx, on_members_changed, on_session).await;
        });

        Some(Self { room_key, shared, cmd_tx })
    }

    pub fn room_key(&self) -> &str {
        &self.room_key
    }

    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
    }

    pub fn members(&self) -> Vec<RelayMember> {
        self.shared.members.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// This connection's server-assigned `(member_id, epoch)`, once the
    /// `welcome` has arrived. `None` briefly right after connect (before the
    /// `welcome` lands) or between connections during a reconnect.
    pub fn session(&self) -> Option<(u64, String)> {
        self.shared.my_session.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `PartyRelay.join` — announce ourselves to the room: a display name and
    /// our ephemeral ed25519 pubkey (hex). No summoner id — v2 has none on
    /// the wire at all (see this module's doc comment).
    pub fn join(&self, name: String, pubkey: String) {
        let _ = self.cmd_tx.send(OutCmd::Join { name, pubkey });
    }

    /// `PartyRelay.send_skin` — broadcast our current selection (`None` clears it).
    pub fn send_skin(&self, skin: Option<Value>) {
        let _ = self.cmd_tx.send(OutCmd::Skin(skin));
    }

    /// `PartyRelay.disconnect` — leave the room and stop the connection task
    /// for good (no reconnect attempt follows an intentional leave).
    pub fn disconnect(&self) {
        self.shared.should_run.store(false, Ordering::SeqCst);
        let _ = self.cmd_tx.send(OutCmd::Leave);
    }
}

/// Truncate a room key to its first 8 hex chars for log lines (never log the
/// full key — it IS the room secret). Shared with `manager.rs`'s connect/
/// reconnect logging rather than duplicated.
pub(crate) fn short_key(room_key: &str) -> &str {
    &room_key[..room_key.len().min(8)]
}

/// The connection task body: owns the socket, forwards `OutCmd`s to it,
/// handles the ping/pong keepalive and the `welcome`/`members` messages, and
/// reconnects on an unexpected drop (see this module's doc comment on why
/// that's an addition over the Python original, which has no reconnect).
async fn run_connection(
    first_ws: WsStream,
    relay_url: String,
    room_key: String,
    shared: Arc<RelayShared>,
    mut cmd_rx: mpsc::UnboundedReceiver<OutCmd>,
    on_members_changed: MembersCallback,
    on_session: SessionCallback,
) {
    let mut pending_ws = Some(first_ws);

    loop {
        let ws = match pending_ws.take() {
            Some(ws) => ws,
            None => {
                let url = format!("{relay_url}/room?key={room_key}");
                match connect_async(&url).await {
                    Ok((ws, _)) => {
                        shared.connected.store(true, Ordering::SeqCst);
                        log_info!("[RELAY] Reconnected to room {}", short_key(&room_key));
                        ws
                    }
                    Err(e) => {
                        log_warn!("[RELAY] Reconnect failed: {e}");
                        tokio::time::sleep(RECONNECT_DELAY).await;
                        if !shared.should_run.load(Ordering::SeqCst) {
                            break;
                        }
                        continue;
                    }
                }
            }
        };

        let left_intentionally = run_one_connection(ws, &shared, &mut cmd_rx, &on_members_changed, &on_session).await;
        shared.connected.store(false, Ordering::SeqCst);
        // A reconnect gets a brand-new `member_id`/`epoch` from the server's
        // next `welcome` — drop the stale one now so nothing signs against
        // it in the gap before that arrives.
        *shared.my_session.lock().unwrap_or_else(|e| e.into_inner()) = None;

        if left_intentionally || !shared.should_run.load(Ordering::SeqCst) {
            break;
        }
        log_info!("[RELAY] Connection lost, reconnecting in {RECONNECT_DELAY:?}...");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }

    *shared.members.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    log_info!("[RELAY] Disconnected");
}

/// Drive one live connection until it closes (by us or by the peer).
/// Returns `true` if the close was an intentional `leave` (or the owning
/// `PartyRelay` was dropped) — in that case the caller must NOT reconnect.
async fn run_one_connection(
    ws: WsStream,
    shared: &Arc<RelayShared>,
    cmd_rx: &mut mpsc::UnboundedReceiver<OutCmd>,
    on_members_changed: &MembersCallback,
    on_session: &SessionCallback,
) -> bool {
    let (mut write, mut read) = ws.split();
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    ping_timer.tick().await; // first tick fires immediately — consume it so the real cadence is 25s.

    // Re-announce ourselves on (re)connect. On the FIRST connection this is
    // None (the manager sends `join` right after connecting), so it's a
    // no-op; on a RECONNECT it carries the last join, without which the
    // fresh socket would be a nameless ghost the server never re-registers.
    // The last SKIN is intentionally NOT replayed here — see
    // `RelayShared::last_join`'s doc comment on why (its signature would no
    // longer verify under this connection's fresh member_id/epoch).
    {
        let join = shared.last_join.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((name, pubkey)) = join {
            let payload = json!({"type":"join","name":name,"pubkey":pubkey});
            if write.send(Message::Text(payload.to_string())).await.is_err() {
                return false;
            }
        }
    }

    loop {
        tokio::select! {
            _ = ping_timer.tick() => {
                if write.send(Message::Text("ping".to_string())).await.is_err() {
                    return false;
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(OutCmd::Join { name, pubkey }) => {
                        // Remember for reconnect replay before sending.
                        *shared.last_join.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some((name.clone(), pubkey.clone()));
                        let payload = json!({"type":"join","name":name,"pubkey":pubkey});
                        if write.send(Message::Text(payload.to_string())).await.is_err() {
                            return false;
                        }
                    }
                    Some(OutCmd::Skin(skin)) => {
                        let payload = json!({"type":"skin","skin":skin});
                        if write.send(Message::Text(payload.to_string())).await.is_err() {
                            return false;
                        }
                    }
                    Some(OutCmd::Leave) => {
                        // Intentional leave — forget our identity so a later
                        // re-enable doesn't replay a stale join.
                        *shared.last_join.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        let _ = write.send(Message::Text(json!({"type":"leave"}).to_string())).await;
                        let _ = write.close().await;
                        return true;
                    }
                    None => {
                        // The owning `PartyRelay` was dropped — leave cleanly, same as an
                        // explicit `disconnect()`.
                        let _ = write.send(Message::Text(json!({"type":"leave"}).to_string())).await;
                        let _ = write.close().await;
                        shared.should_run.store(false, Ordering::SeqCst);
                        return true;
                    }
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        // Keepalive reply is a bare TEXT frame, never JSON — string-match
                        // it first so `serde_json::from_str` below never sees it.
                        if text == "pong" {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                        match value.get("type").and_then(Value::as_str) {
                            Some("welcome") => {
                                // Fresh identity for THIS connection — never
                                // client-chosen. Store it and let the manager
                                // (re)sign + rebroadcast against it.
                                let member_id = value.get("member_id").and_then(Value::as_u64);
                                let epoch = value.get("epoch").and_then(Value::as_str).map(str::to_string);
                                if let (Some(member_id), Some(epoch)) = (member_id, epoch) {
                                    *shared.my_session.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some((member_id, epoch.clone()));
                                    on_session(member_id, epoch);
                                }
                            }
                            Some("members") => {
                                // Keep the stored epoch fresh — the server
                                // re-sends it on every broadcast, not just welcome.
                                if let Some(epoch) = value.get("epoch").and_then(Value::as_str) {
                                    let mut session = shared.my_session.lock().unwrap_or_else(|e| e.into_inner());
                                    if let Some((_, e)) = session.as_mut() {
                                        *e = epoch.to_string();
                                    }
                                }
                                let members: Vec<RelayMember> = value
                                    .get("members")
                                    .and_then(|m| serde_json::from_value(m.clone()).ok())
                                    .unwrap_or_default();
                                *shared.members.lock().unwrap_or_else(|e| e.into_inner()) = members.clone();
                                log_info!("[RELAY] Members updated: {} in room", members.len());
                                on_members_changed(members);
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-vector cross-check: this room key was computed independently by
    /// Python (`hashlib.sha256(str(123456789).encode() + bytes(range(32))).hexdigest()[:32]`)
    /// — proves this port derives the identical room key a Python-side host
    /// or joiner would compute from the same token.
    #[test]
    fn compute_room_key_matches_known_python_vector() {
        let key: [u8; 32] = (0u8..32).collect::<Vec<u8>>().try_into().unwrap();
        let room_key = compute_room_key(123456789, &key);
        assert_eq!(room_key, "52305518fe56eff47dbe97f1bd4435ae");
        assert_eq!(room_key.len(), 32);
    }

    #[test]
    fn compute_room_key_is_deterministic_and_key_sensitive() {
        let key_a = [1u8; 32];
        let key_b = [2u8; 32];
        assert_eq!(compute_room_key(42, &key_a), compute_room_key(42, &key_a));
        assert_ne!(compute_room_key(42, &key_a), compute_room_key(42, &key_b));
    }

    #[test]
    fn short_key_truncates_to_8_without_panicking_on_short_input() {
        assert_eq!(short_key("abcdefghij"), "abcdefgh");
        assert_eq!(short_key("abc"), "abc");
    }
}
