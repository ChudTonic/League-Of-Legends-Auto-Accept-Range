//! Outbound bridge state broadcasts (S4) — ported from `pengu\communication\
//! broadcaster.py`'s `Broadcaster`. Each method here builds the JSON payload
//! (via the matching `protocol.rs` struct) and hands it to
//! `BridgeHandle::send_raw`, which fans it out to every connected client —
//! see `bridge::mod`'s doc comment for why that's a plain broadcast-channel
//! send rather than iterating a client list.
//!
//! THIS IS THE S5/S6 SEAM: a `#[tauri::command]` or a spawned task in a later
//! milestone holds a `BridgeHandle` (from `AppState`) and calls these methods
//! directly, e.g. `app_state.skins_bridge.lock_safe().as_ref().unwrap()
//! .broadcast_custom_mod_state(true, Some("My Mod".into()), Some(103000))`.

#![allow(dead_code)]

use super::protocol::{
    ChampionLockedMsg, ChromaStateMsg, CustomModStateMsg, HistoricStateMsg, PartyStateMsg, PhaseChangeMsg,
    RandomModeStateMsg, SkinStateMsg, SkipBaseSkinMsg,
};
use super::BridgeHandle;

impl BridgeHandle {
    /// `Broadcaster.broadcast_skin_state`. `has_chromas` is the caller's job
    /// to compute (from a `lcu_ext::ChampionSkinCache`, the forms table, or
    /// the special-chroma ID lists) — this function has no LCU/cache access
    /// of its own, matching `BridgeHandle`'s deliberately thin surface.
    pub fn broadcast_skin_state(&self, skin_name: String, skin_id: Option<i64>, has_chromas: bool) {
        let msg = SkinStateMsg::new(skin_name, skin_id, has_chromas);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_chroma_state`. Always the "no ChromaPanelManager"
    /// fallback shape — see `ChromaStateMsg::new`'s doc comment.
    pub fn broadcast_chroma_state(&self, selected_chroma_id: Option<i64>) {
        let msg = ChromaStateMsg::new(selected_chroma_id);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_historic_state`. `historic_skin_name`
    /// resolution (custom-mod-path exclusion, chroma-vs-skin lookup) is the
    /// caller's job — see `handlers::historic` for that logic.
    pub fn broadcast_historic_state(&self, active: bool, historic_skin_id: Option<i64>, historic_skin_name: Option<String>) {
        let msg = HistoricStateMsg::new(active, historic_skin_id, historic_skin_name);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_custom_mod_state`.
    pub fn broadcast_custom_mod_state(&self, active: bool, mod_name: Option<String>, skin_id: Option<i64>) {
        let msg = CustomModStateMsg::new(active, mod_name, skin_id);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_phase_change`.
    pub fn broadcast_phase_change(&self, phase: Option<String>, game_mode: Option<String>, map_id: Option<i64>, queue_id: Option<i64>) {
        let msg = PhaseChangeMsg::new(phase, game_mode, map_id, queue_id);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_champion_locked`.
    pub fn broadcast_champion_locked(&self, locked: bool) {
        let msg = ChampionLockedMsg::new(locked);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_random_mode_state`.
    pub fn broadcast_random_mode_state(&self, active: bool, random_skin_id: Option<i64>) {
        let msg = RandomModeStateMsg::new(active, random_skin_id);
        self.broadcast_json(serde_json::to_value(&msg).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_skip_base_skin` — no timestamp, no other
    /// fields (see `SkipBaseSkinMsg`'s doc comment).
    pub fn broadcast_skip_base_skin(&self) {
        self.broadcast_json(serde_json::to_value(SkipBaseSkinMsg::new()).unwrap_or_default());
    }

    /// `Broadcaster.broadcast_party_state`. Party mode is S6 — always the
    /// disabled/empty shape until that milestone lands (see
    /// `handlers::party`'s stubs, which are the seam S6 replaces).
    pub fn broadcast_party_state_disabled(&self) {
        self.broadcast_json(serde_json::to_value(PartyStateMsg::disabled()).unwrap_or_default());
    }
}
