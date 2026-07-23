//! Presence nudge (S6 follow-up): while the user is in a lobby/queue with
//! party consent accepted but party mode OFF, hold a lightweight
//! identity-free relay connection to the lobby room and, if another member
//! shows up (another Chud user with party mode on OR off), nudge them once
//! per lobby to turn party mode on. This is a SEPARATE `PartyRelay` from
//! `PartyManager`'s — that one is `None` while party mode is off, and this
//! connection must never send `join`/`skin`, so it stays identity-less even
//! when a peer's client is fully joined.
//!
//! Detects the both-off case symmetrically: every presence-only socket sends
//! `{"type":"presence"}`, which the relay just re-broadcasts the roster on
//! (`relay-worker/src/lib.rs`'s `"presence"` arm) — so two off-mode users in
//! the same lobby each see the other's socket in the member count without
//! either ever identifying themselves.

#![allow(dead_code)]

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::json;
use tauri::{AppHandle, Emitter, Manager};

use crate::lcu;
use crate::skins::lcu_ext;
use crate::skins::slog::{log_info, log_warn};
use crate::skins::SkinsState;
use crate::{AppState, LockExt};

use super::manager::{PartyManager, CURRENT_PARTY_CONSENT_VERSION};
use super::relay::{self, PartyRelay};

const POLL_INTERVAL: Duration = Duration::from_secs(4);

/// Only these two phases count as "in a party, pre/during-queue" for the
/// nudge — ReadyCheck/ChampSelect/everything after is out of scope (by then
/// party mode's own skin-sync path is what matters, not this heads-up).
fn is_lobby_phase(phase: Option<&str>) -> bool {
    matches!(phase, Some("Lobby") | Some("Matchmaking"))
}

/// Pure debounce: nudge once per distinct `party_id`. Re-arms only when the
/// caller clears `last_nudged_party_id` (party_id changed or the lobby was left).
fn should_nudge(party_id: &str, last_nudged_party_id: Option<&str>) -> bool {
    last_nudged_party_id != Some(party_id)
}

/// Owns the presence-only relay connection and its poll loop. Spawned once at
/// startup (`lib.rs::setup`); self-gates every tick on consent/phase/party-mode
/// so it never contacts the relay without consent and never runs alongside a
/// real party connection.
pub struct PresenceDetector {
    app: AppHandle,
    party_manager: Arc<PartyManager>,
    skins: Arc<SkinsState>,
    http_client: reqwest::Client,
    relay_url: String,
}

impl PresenceDetector {
    pub fn spawn(app: AppHandle, party_manager: Arc<PartyManager>, skins: Arc<SkinsState>) {
        let relay_url = resolve_relay_url(&app);
        let detector = Self {
            app,
            party_manager,
            skins,
            http_client: lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S),
            relay_url,
        };
        tauri::async_runtime::spawn(async move { detector.run().await });
    }

    async fn run(self) {
        let mut relay: Option<PartyRelay> = None;
        let mut current_party_id: Option<String> = None;
        let mut last_nudged_party_id: Option<String> = None;
        let mut last_status: Option<String> = None;
        let mut poll_timer = tokio::time::interval(POLL_INTERVAL);

        loop {
            poll_timer.tick().await;

            let consent_version = {
                let app_state = self.app.state::<Arc<AppState>>();
                let v = app_state.config.lock_safe().party.consent_version;
                v
            };
            if consent_version < CURRENT_PARTY_CONSENT_VERSION {
                // Never contact the relay without consent.
                Self::drop_connection(&mut relay);
                log_presence(&mut last_status, format!("idle - party consent not accepted (v{consent_version} < v{CURRENT_PARTY_CONSENT_VERSION})"));
                continue;
            }

            // Party mode owns the room the moment it's enabled — drop our
            // socket so there's never two connections for the same user
            // (also covers the user flipping party mode ON mid-poll).
            if self.party_manager.enabled() {
                Self::drop_connection(&mut relay);
                log_presence(&mut last_status, "idle - party mode is on".to_string());
                continue;
            }

            let phase = { self.skins.shared.lock_safe().phase.clone() };
            if !is_lobby_phase(phase.as_deref()) {
                Self::drop_connection(&mut relay);
                current_party_id = None;
                last_nudged_party_id = None;
                log_presence(&mut last_status, format!("idle - phase {:?} is not lobby/queue", phase.as_deref()));
                continue;
            }

            let Some(auth) = lcu::cached_auth() else {
                log_presence(&mut last_status, "waiting - LCU auth not ready".to_string());
                continue;
            };
            let Some(party_id) = lcu_ext::get_lobby_party_id(&self.http_client, &auth).await else {
                log_presence(&mut last_status, "waiting - no lobby partyId (not in a party lobby)".to_string());
                continue;
            };

            if current_party_id.as_deref() != Some(party_id.as_str()) {
                current_party_id = Some(party_id.clone()); // new lobby - re-arm the nudge
                last_nudged_party_id = None;
            }

            let room_key = relay::compute_lobby_room_key(&party_id);
            let needs_connect = relay.as_ref().map(|r| r.room_key() != room_key).unwrap_or(true);
            if needs_connect {
                Self::drop_connection(&mut relay);
                relay = self.connect_presence(room_key).await;
            }

            let Some(active) = &relay else {
                log_presence(&mut last_status, "connect failed - retrying next poll".to_string());
                continue;
            };
            let count = active.member_count();
            log_presence(&mut last_status, format!("in lobby room (party {}), {count} member(s)", &party_id[..party_id.len().min(8)]));
            if count > 1 && should_nudge(&party_id, last_nudged_party_id.as_deref()) {
                log_info!("[PARTY] Presence: {count} in room -> nudging (chudders detected)");
                let _ = self.app.emit(
                    "notification",
                    json!({
                        "title": "Chudders in your party",
                        "message": "Someone here uses Chud. Turn on party mode to see each other's skins.",
                        "tone": "info",
                    }),
                );
                last_nudged_party_id = Some(party_id);
            }
        }
    }

    fn drop_connection(relay: &mut Option<PartyRelay>) {
        if let Some(r) = relay.take() {
            r.disconnect();
        }
    }

    /// Connect a fresh identity-less relay session and send the initial
    /// `presence` ping. `on_session` re-sends `presence` on every subsequent
    /// (re)connect too — a socket that drops and auto-reconnects (network
    /// blip) is invisible in the room until it messages again, same as any
    /// other relay client.
    async fn connect_presence(&self, room_key: String) -> Option<PartyRelay> {
        let slot: Arc<StdMutex<Option<PartyRelay>>> = Arc::new(StdMutex::new(None));
        let slot_for_session = Arc::clone(&slot);
        let on_members_changed: relay::MembersCallback = Arc::new(|_members| {}); // member_count() is polled directly
        let on_session: relay::SessionCallback = Arc::new(move |_member_id, _epoch| {
            if let Some(r) = slot_for_session.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                r.send_presence();
            }
        });

        match PartyRelay::connect(&self.relay_url, room_key, on_members_changed, on_session).await {
            Some(new_relay) => {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_relay.clone());
                new_relay.send_presence();
                Some(new_relay)
            }
            None => {
                log_warn!("[PARTY] Presence relay connection failed - will retry next poll");
                None
            }
        }
    }
}

/// Log a presence-loop status line only when it CHANGES — the loop polls every
/// few seconds, so logging unconditionally would flood the log during a lobby.
/// This gives a diagnosable trail (which gate the loop stops at, the member
/// count once connected) without spam.
fn log_presence(last: &mut Option<String>, status: String) {
    if last.as_deref() != Some(status.as_str()) {
        log_info!("[PARTY] Presence: {status}");
        *last = Some(status);
    }
}

/// Same resolution order as `party::manager`'s private `resolve_relay_url`
/// (env override, then configured, then default) — kept as its own copy
/// rather than widening that fn's visibility, since this change's scope is
/// `party::relay`/`party::presence` only.
fn resolve_relay_url(app: &AppHandle) -> String {
    if let Ok(url) = std::env::var("CHUD_RELAY_URL") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    let configured = {
        let app_state = app.state::<Arc<AppState>>();
        let url = app_state.config.lock_safe().skins.party_relay_url.clone();
        url
    };
    if !configured.trim().is_empty() {
        return configured;
    }
    relay::DEFAULT_RELAY_URL.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_lobby_phase_only_true_for_lobby_and_matchmaking() {
        assert!(is_lobby_phase(Some("Lobby")));
        assert!(is_lobby_phase(Some("Matchmaking")));
        assert!(!is_lobby_phase(Some("ReadyCheck")));
        assert!(!is_lobby_phase(Some("ChampSelect")));
        assert!(!is_lobby_phase(None));
    }

    #[test]
    fn should_nudge_once_per_distinct_party_id_and_rearms_on_change_or_clear() {
        assert!(should_nudge("party-a", None));
        assert!(!should_nudge("party-a", Some("party-a")));
        assert!(should_nudge("party-b", Some("party-a")));
        // Clearing the marker (left the lobby, or a fresh lobby entry) re-arms
        // even the SAME party id.
        assert!(should_nudge("party-a", None));
    }
}
