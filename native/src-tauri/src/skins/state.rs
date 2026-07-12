//! `SkinsShared`: the skins subsystem's per-session state, ported from
//! Python's `state\core\shared_state.py` `SharedState` dataclass.
//!
//! The Python original mutated this god object from 5+ OS threads under the
//! GIL, with documented races (see `docs/SKINS_PORT.md` "Threading model").
//! Chud puts the whole struct behind one `Mutex` (see `skins::SkinsState`),
//! so the per-field `threading.Lock`s Python needed (`timer_lock`,
//! `swiftplay_lock`) are gone — the outer coarse mutex already serializes
//! every mutation.
//!
//! Untyped back-references to other subsystem objects (`ui_skin_thread`,
//! `party_manager`, `swiftplay_handler`, `force_base_skins_callback`) are NOT
//! ported as fields; those become channels/handles wired in later
//! milestones. This struct is pure data.

#![allow(dead_code)] // consumed by S2+

use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Historic-mode's in-memory selection for the currently-tracked champion:
/// either a plain skin/chroma ID, or a custom mod's relative path (mirrors
/// Python's `historic_skin_id`, which could hold either an `int` or a
/// `"path:<rel>"` string — see `ui/handlers/historic_mode_handler.py`). The
/// on-disk union lives in `features::historic::HistoricEntry` (which keeps
/// the literal `"path:"` prefix for `historic.json` compatibility); this
/// type is the prefix-stripped runtime form other modules match on. See
/// `HistoricEntry::to_selection`/`From<&HistoricSelection> for HistoricEntry`
/// for the conversion between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoricSelection {
    SkinId(i64),
    /// Relative mod path (no `"path:"` prefix — that's disk-format bookkeeping).
    CustomMod(String),
}

/// Mirrors Python's `selected_custom_mod` dict shape
/// (`{skin_id, champion_id, mod_name, mod_path, relative_path}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomModSelection {
    pub skin_id: i64,
    pub champion_id: i64,
    pub mod_name: String,
    pub mod_path: String,
    pub relative_path: String,
}

/// One extracted-and-ready non-skin mod selection (map/font/announcer/other)
/// — mirrors Python's `selected_map_mod`/`selected_font_mod`/
/// `selected_announcer_mod`/`selected_other_mods` dict shapes.
///
/// MIGRATED (S5) from `bridge::ModSelection`/`ModSelections`: S4's own doc
/// comment flagged that those belonged here but `state.rs` wasn't in S4's
/// file scope. `state.rs` is S5's to edit, so the fields landed here, and a
/// reconciliation pass afterward made `bridge/handlers.rs`'s `select-map`/
/// `select-font`/`select-announcer`/`select-other` handlers write straight
/// into `SkinsShared::category_mods` (the bridge-local `ModSelection`/
/// `ModSelections` types were removed — nothing else read them), so
/// `trigger.rs`'s injection trigger now sees real bridge-driven selections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryModSelection {
    pub mod_name: String,
    pub mod_path: String,
    pub mod_folder_name: String,
    pub relative_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategoryModSelections {
    pub map: Option<CategoryModSelection>,
    pub font: Option<CategoryModSelection>,
    pub announcer: Option<CategoryModSelection>,
    pub others: Vec<CategoryModSelection>,
}

#[derive(Debug, Clone)]
pub struct SkinsShared {
    // ---- Phase / champ-select ----
    pub phase: Option<String>,
    pub hovered_champ_id: Option<i64>,
    pub locked_champ_id: Option<i64>,
    /// Wall-clock seconds (Python used `time.time()`, not monotonic) —
    /// 0.0 = unset.
    pub locked_champ_timestamp: f64,
    pub last_hovered_skin_key: Option<String>,
    pub last_hovered_skin_id: Option<i64>,
    pub last_hovered_skin_slug: Option<String>,
    /// Favorite skin per champion (`champ_id -> skin_id`), loaded from
    /// `favorites.json`. Lets you set a go-to skin per champ once and have it
    /// auto-applied every game — no in-client hovering.
    pub favorite_skins: std::collections::HashMap<i64, i64>,
    /// The favorite for the currently-locked champion (set on lock). Consumed
    /// by `resolve_injection_name` as a fallback BELOW a manual in-client pick.
    /// Kept separate from `last_hovered_skin_id` so the skin monitor's
    /// base-skin reports can't clobber it.
    pub active_favorite_skin_id: Option<i64>,
    /// Skin ID selected in the LCU (owned skin).
    pub selected_skin_id: Option<i64>,
    /// All owned skin IDs from the LCU inventory.
    pub owned_skin_ids: HashSet<i64>,
    pub processed_action_ids: HashSet<i64>,
    /// Legacy thread-stop flag from the Python original's OS-thread model; Chud cancels
    /// tokio tasks via generation counters/channels instead, but the field
    /// is kept for 1:1 parity until a later milestone proves it's unused.
    pub stop: bool,
    pub players_visible: i32,
    pub locks_by_cell: HashMap<i64, i64>,
    pub all_locked_announced: bool,
    pub local_cell_id: Option<i64>,

    // ---- Loadout timer ----
    pub loadout_countdown_active: bool,
    /// Monotonic start instant (Python used `time.monotonic()`); `None` when
    /// the countdown isn't armed.
    pub loadout_t0: Option<Instant>,
    pub loadout_left0_ms: i64,
    pub last_remain_ms: i64,
    pub last_hover_written: bool,
    // `timer_lock` (threading.Lock) dropped — superseded by the outer
    // coarse Mutex<SkinsShared>.
    pub ticker_seq: u64,
    pub current_ticker: u64,

    // ---- Skin write config ----
    pub skin_write_ms: i64,
    /// Prevents UI-detection restart immediately after an injection.
    pub injection_completed: bool,
    pub inject_batch: Option<String>,

    // ---- Chroma selection ----
    pub selected_chroma_id: Option<i64>,
    /// Selected Form file path for Elementalist Lux (and the other
    /// forms/HOL special cases — see `features::special`).
    pub selected_form_path: Option<String>,
    pub pending_chroma_selection: bool,

    // ---- UI state management ----
    pub reset_skin_notification: bool,
    pub chroma_panel_open: bool,

    // Per-game ChampSelect reset coordination (shared by the HTTP poller and
    // the WebSocket phase handler so the reset runs exactly once per
    // ChampSelect even if one of the two phase sources misses the
    // transition — see `reset_for_champ_select`).
    pub champ_select_reset_done: bool,
    /// Signal to the champion-lock handler to forget its last lock.
    pub reset_last_locked: bool,

    // ---- Language detection ----
    pub current_language: Option<String>,

    // ---- Game mode detection ----
    pub current_game_mode: Option<String>,
    pub current_map_id: Option<i64>,
    pub current_queue_id: Option<i64>,
    /// Base skin name when the chroma panel was opened (avoids re-detecting
    /// the same skin).
    pub chroma_panel_skin_name: Option<String>,
    pub is_swiftplay_mode: bool,

    // ---- Swiftplay skin tracking ----
    /// champion_id -> last-detected skin_id.
    pub swiftplay_skin_tracking: HashMap<i64, i64>,
    /// Extracted mod folder names for Swiftplay injection.
    pub swiftplay_extracted_mods: Vec<String>,
    // `swiftplay_lock` (threading.Lock) dropped — superseded by the outer
    // coarse Mutex<SkinsShared>.

    // ---- UIA detection ----
    pub ui_last_text: Option<String>,
    pub ui_skin_id: Option<i64>,

    // ---- Random skin selection ----
    pub random_skin_name: Option<String>,
    pub random_skin_id: Option<i64>,
    pub random_mode_active: bool,

    // ---- Historic mode (remember last injected unowned skin per champion) ----
    pub historic_mode_active: bool,
    pub historic_selection: Option<HistoricSelection>,
    pub historic_first_detection_done: bool,

    // `ui_skin_thread` back-reference dropped — wired via channels in later
    // milestones.

    // ---- Champion exchange detection ----
    /// Hides the UI during a champion exchange.
    pub champion_exchange_triggered: bool,

    // ---- Own champion lock tracking ----
    pub own_champion_locked: bool,

    // ---- Custom mod selection ----
    pub selected_custom_mod: Option<CustomModSelection>,

    // ---- Category mod selections (map/font/announcer/other) ----
    /// See `CategoryModSelections`'s doc comment for the S5 migration note.
    /// Persists across games like Python's originals (`selected_map_mod`
    /// etc. are NOT cleared by `reset_for_champ_select` — see its doc
    /// comment / injection_trigger.py's "Keep mod selections in state so
    /// they persist across games" comment) — deliberately not touched by
    /// either reset function below.
    pub category_mods: CategoryModSelections,

    // ---- Party mode (P2P skin sharing with friends) ----
    pub party_mode_enabled: bool,
    pub party_token: Option<String>,
    // `party_manager` back-reference dropped — wired via channels in later
    // milestones.
}

impl Default for SkinsShared {
    fn default() -> Self {
        Self {
            phase: None,
            hovered_champ_id: None,
            locked_champ_id: None,
            locked_champ_timestamp: 0.0,
            last_hovered_skin_key: None,
            last_hovered_skin_id: None,
            last_hovered_skin_slug: None,
            selected_skin_id: None,
            owned_skin_ids: HashSet::new(),
            processed_action_ids: HashSet::new(),
            stop: false,
            players_visible: 0,
            locks_by_cell: HashMap::new(),
            all_locked_announced: false,
            local_cell_id: None,

            loadout_countdown_active: false,
            loadout_t0: None,
            loadout_left0_ms: 0,
            last_remain_ms: 0,
            last_hover_written: false,
            ticker_seq: 0,
            current_ticker: 0,

            skin_write_ms: 2000,
            injection_completed: false,
            inject_batch: None,

            selected_chroma_id: None,
            selected_form_path: None,
            pending_chroma_selection: false,

            reset_skin_notification: false,
            chroma_panel_open: false,

            champ_select_reset_done: false,
            reset_last_locked: false,

            current_language: None,

            current_game_mode: None,
            current_map_id: None,
            current_queue_id: None,
            chroma_panel_skin_name: None,
            is_swiftplay_mode: false,

            swiftplay_skin_tracking: HashMap::new(),
            swiftplay_extracted_mods: Vec::new(),

            ui_last_text: None,
            ui_skin_id: None,

            random_skin_name: None,
            random_skin_id: None,
            random_mode_active: false,

            historic_mode_active: false,
            historic_selection: None,
            historic_first_detection_done: false,

            champion_exchange_triggered: false,

            own_champion_locked: false,

            selected_custom_mod: None,

            category_mods: CategoryModSelections::default(),

            party_mode_enabled: false,
            party_token: None,

            favorite_skins: HashMap::new(),
            active_favorite_skin_id: None,
        }
    }
}

impl SkinsShared {
    /// Per-game re-arm reset for a fresh ChampSelect entry (ported from
    /// `threads\handlers\champ_select_reset.py::perform_champ_select_reset`).
    /// Idempotent via `champ_select_reset_done`: returns `false` (no-op) if
    /// the reset already ran for the current ChampSelect. Callers must call
    /// `note_phase_for_champ_select_guard` on every observed phase so the
    /// guard re-arms once ChampSelect/FINALIZATION is left.
    ///
    /// Side effects the Python version performed inline here — reloading
    /// `owned_skin_ids` from the LCU inventory, requesting UI
    /// reinitialization, and broadcasting the champion-unlock state to the
    /// bridge — are NOT part of this pure state mutation; the S2+ caller
    /// (phase actor / lcu_ext) performs them after this returns `true`.
    pub fn reset_for_champ_select(&mut self) -> bool {
        if self.champ_select_reset_done {
            return false;
        }
        self.champ_select_reset_done = true;

        // Reset skin detection state.
        self.last_hovered_skin_key = None;
        self.last_hovered_skin_id = None;
        self.last_hovered_skin_slug = None;
        self.ui_last_text = None;
        self.ui_skin_id = None;

        // Reset LCU skin selection.
        self.selected_skin_id = None;
        self.owned_skin_ids.clear();

        // The two flags that gate injection for the next game.
        self.last_hover_written = false;
        self.injection_completed = false;
        self.loadout_countdown_active = false;

        // Reset champion lock state for the new game.
        self.locked_champ_id = None;
        self.locked_champ_timestamp = 0.0;
        self.own_champion_locked = false;
        self.reset_last_locked = true;

        // Reset random skin state.
        self.random_skin_name = None;
        self.random_skin_id = None;
        self.random_mode_active = false;

        // Reset historic mode state.
        self.historic_mode_active = false;
        self.historic_selection = None;
        self.historic_first_detection_done = false;

        // Clear the custom mod selection from the previous game so the
        // mod-name popup doesn't re-appear until re-picked.
        self.selected_custom_mod = None;

        // Reset exchange tracking.
        self.champion_exchange_triggered = false;

        // Signal the caller to reset skin notification debouncing.
        self.reset_skin_notification = true;
        self.processed_action_ids.clear();

        true
    }

    /// Re-arm the ChampSelect reset guard once we leave ChampSelect/FINALIZATION
    /// (ported from `champ_select_reset.py::note_phase_for_reset`). Call on
    /// every observed phase transition.
    pub fn note_phase_for_champ_select_guard(&mut self, phase: Option<&str>) {
        let in_champ_select = matches!(phase, Some("ChampSelect") | Some("FINALIZATION"));
        if !in_champ_select {
            self.champ_select_reset_done = false;
        }
    }

    /// Full reset on LCU disconnect (ported from
    /// `main\core\lcu_handler.py::create_lcu_disconnection_handler`). A
    /// superset of `reset_for_champ_select` — also clears phase/game-mode/
    /// swiftplay state that survives across a single ChampSelect but not a
    /// full client disconnect.
    ///
    /// UI-thread cache reset, LCU WS disconnect, and tray-icon refresh are
    /// side effects on Python's `ui_skin_thread`/`app_status` back-references
    /// (not state fields); wired via channels in later milestones.
    pub fn reset_on_lcu_disconnect(&mut self) {
        self.phase = None;
        self.hovered_champ_id = None;
        self.locked_champ_id = None;
        self.locked_champ_timestamp = 0.0;
        self.own_champion_locked = false;
        self.players_visible = 0;
        self.all_locked_announced = false;
        self.loadout_countdown_active = false;
        self.loadout_t0 = None;
        self.loadout_left0_ms = 0;
        self.last_remain_ms = 0;
        self.last_hover_written = false;
        self.selected_skin_id = None;
        self.selected_chroma_id = None;
        self.selected_form_path = None;
        self.pending_chroma_selection = false;
        self.chroma_panel_open = false;
        self.reset_skin_notification = true;
        self.current_game_mode = None;
        self.current_map_id = None;
        self.current_queue_id = None;
        self.chroma_panel_skin_name = None;
        self.is_swiftplay_mode = false;
        self.random_mode_active = false;
        self.random_skin_name = None;
        self.random_skin_id = None;
        self.historic_mode_active = false;
        self.historic_selection = None;
        self.historic_first_detection_done = false;
        self.ui_skin_id = None;
        self.ui_last_text = None;
        self.last_hovered_skin_key = None;
        self.last_hovered_skin_id = None;
        self.last_hovered_skin_slug = None;
        self.champion_exchange_triggered = false;
        self.injection_completed = false;

        self.locks_by_cell.clear();
        self.processed_action_ids.clear();
        self.owned_skin_ids.clear();
        self.swiftplay_skin_tracking.clear();
        self.swiftplay_extracted_mods.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_dataclass_defaults() {
        let s = SkinsShared::default();
        assert_eq!(s.skin_write_ms, 2000);
        assert!(s.owned_skin_ids.is_empty());
        assert!(!s.champ_select_reset_done);
    }

    #[test]
    fn champ_select_reset_is_idempotent_until_rearmed() {
        let mut s = SkinsShared::default();
        s.selected_skin_id = Some(12345);
        assert!(s.reset_for_champ_select());
        assert_eq!(s.selected_skin_id, None);

        // Second call before leaving ChampSelect is a no-op.
        s.selected_skin_id = Some(999);
        assert!(!s.reset_for_champ_select());
        assert_eq!(s.selected_skin_id, Some(999));

        // Leaving ChampSelect re-arms the guard.
        s.note_phase_for_champ_select_guard(Some("InProgress"));
        assert!(s.reset_for_champ_select());
        assert_eq!(s.selected_skin_id, None);
    }
}
