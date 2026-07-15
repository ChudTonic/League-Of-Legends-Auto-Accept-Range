//! Outbound bridge state broadcasts (S4) — ported from `pengu\communication\
//! broadcaster.py`'s `Broadcaster`. Each method builds the JSON payload (via
//! the matching `protocol.rs` struct) and hands it to `BridgeHandle::send_raw`,
//! which fans it out to every connected client (see `bridge::mod`).
//!
//! S5/S6 seam: later milestones hold a `BridgeHandle` (from `AppState`) and
//! call these methods directly to push state updates.

#![allow(dead_code)]

use crate::skins::state::HistoricSelection;

use super::protocol::{
    ChampionLockedMsg, ChromaStateMsg, CustomModStateMsg, HistoricStateMsg, PartyStateMsg, PhaseChangeMsg,
    RandomModeStateMsg, SkinStateMsg, SkipBaseSkinMsg,
};
use super::BridgeHandle;

impl BridgeHandle {
    /// `Broadcaster.broadcast_skin_state`. `has_chromas` is the caller's job
    /// to compute — this function has no LCU/cache access of its own.
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

    /// `Broadcaster.broadcast_historic_state`. `resolved_skin_name` resolution
    /// for the `SkinId` case is the caller's job (see `handlers::historic`).
    /// A `CustomMod` selection has no ID to fit the field, so it overrides
    /// `resolved_skin_name` and reports `historicSkinId: null` — wire field
    /// names stay unchanged for the JS plugin contract either way.
    pub fn broadcast_historic_state(&self, active: bool, selection: Option<&HistoricSelection>, resolved_skin_name: Option<String>) {
        let (historic_skin_id, historic_skin_name) = match selection {
            Some(HistoricSelection::SkinId(id)) => (Some(*id), resolved_skin_name),
            Some(HistoricSelection::CustomMod(path)) => (None, Some(path.clone())),
            None => (None, None),
        };
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

    /// `Broadcaster.broadcast_party_state` — always the disabled/empty shape
    /// (see `handlers::party`'s stubs).
    pub fn broadcast_party_state_disabled(&self) {
        self.broadcast_json(serde_json::to_value(PartyStateMsg::disabled()).unwrap_or_default());
    }
}
