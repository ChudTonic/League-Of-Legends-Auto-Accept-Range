//! Chroma selection state machine — ported from `ui/chroma/selection_handler.py`
//! (`ChromaSelectionHandler.handle_selection`), with `ui/chroma/selector.py`'s
//! six near-identical per-champion form handlers (Elementalist Lux, Sahn Uzal
//! Mordekaiser, Spirit Blossom Morgana, Radiant Sett, KDA Seraphine, Viego,
//! Risen Legend HOL chromas) collapsed onto `features::special::FORMS`.
//!
//! `ui/chroma/panel.py`'s Qt widget bookkeeping is NOT ported (already
//! headless in Python; JS plugins own the button). These functions are pure
//! `&mut SkinsShared` mutations — the S4 bridge handler that calls them owns
//! broadcasting the resulting state (no callback param here; nothing consumes one yet).

#![allow(dead_code)]

use crate::skins::lcu_ext::ChampionSkinCache;
use crate::skins::state::SkinsShared;

use super::special;

fn resolve_skin_name(cache: Option<&ChampionSkinCache>, skin_id: i64) -> Option<String> {
    cache.and_then(|c| c.get_skin_by_id(skin_id)).map(|s| s.skin_name.clone())
}

/// Handle a chroma-wheel click: `chroma_id == 0` selects the base skin,
/// a `features::special` fake/real ID selects a form or HOL chroma, anything
/// else is a regular LCU chroma. `current_skin_id` is the skin the wheel was
/// shown for (the base skin the chromas belong to).
pub fn handle_selection(
    shared: &mut SkinsShared,
    cache: Option<&ChampionSkinCache>,
    current_skin_id: i64,
    chroma_id: i64,
    chroma_name: &str,
) {
    if let Some(form) = special::form_by_id(chroma_id) {
        handle_form_selection(shared, cache, form, chroma_id, chroma_name);
    } else if chroma_id == 0 {
        handle_base_skin_selection(shared, cache, current_skin_id);
    } else {
        handle_regular_chroma_selection(shared, cache, current_skin_id, chroma_id, chroma_name);
    }

    safety_check_historic_mode(shared);
    shared.pending_chroma_selection = false;
}

/// Consolidates the 7 near-identical Python `_handle_*_form_selection`
/// methods (one per champion) into a single body parameterized by `form`.
fn handle_form_selection(
    shared: &mut SkinsShared,
    cache: Option<&ChampionSkinCache>,
    form: &special::FormSkin,
    chroma_id: i64,
    chroma_name: &str,
) {
    if !form.zip_rel.is_empty() {
        shared.selected_form_path = Some(form.zip_rel.to_string());
    }
    shared.selected_chroma_id = Some(chroma_id);
    // Treat the fake/real form ID as the hovered skin so the injection
    // system sees it as unowned (Python: "Using fake/real ID for injection").
    shared.last_hovered_skin_id = Some(chroma_id);

    if shared.is_swiftplay_mode {
        shared.swiftplay_skin_tracking.insert(special::champion_of(form.base_id), chroma_id);
    }

    disable_historic_mode(shared);

    if let Some(base_name) = resolve_skin_name(cache, form.base_id) {
        shared.last_hovered_skin_key = Some(format!("{base_name} {chroma_name}"));
    }
}

/// `_handle_base_skin_selection`.
fn handle_base_skin_selection(shared: &mut SkinsShared, cache: Option<&ChampionSkinCache>, current_skin_id: i64) {
    shared.selected_chroma_id = None;

    if let Some(name) = resolve_skin_name(cache, current_skin_id) {
        shared.last_hovered_skin_key = Some(name);
    }

    if shared.is_swiftplay_mode {
        shared.swiftplay_skin_tracking.insert(special::champion_of(current_skin_id), current_skin_id);
    }
}

/// `_handle_regular_chroma_selection`. Python stripped a stale chroma-ID
/// suffix off a cached Qt panel display string before re-appending; we look
/// the base name up fresh each time, so there's nothing to strip.
fn handle_regular_chroma_selection(
    shared: &mut SkinsShared,
    cache: Option<&ChampionSkinCache>,
    current_skin_id: i64,
    chroma_id: i64,
    chroma_name: &str,
) {
    shared.selected_chroma_id = Some(chroma_id);
    shared.last_hovered_skin_id = Some(chroma_id);

    if shared.is_swiftplay_mode {
        shared.swiftplay_skin_tracking.insert(special::champion_of(chroma_id), chroma_id);
    }

    disable_historic_mode(shared);
    let _ = chroma_name; // kept for signature parity / future broadcast payload use (S4)

    if let Some(base_name) = resolve_skin_name(cache, current_skin_id) {
        shared.last_hovered_skin_key = Some(format!("{base_name} {chroma_id}"));
    }
}

/// `_disable_historic_mode` (minus the JS broadcast — S4 seam, see module doc).
fn disable_historic_mode(shared: &mut SkinsShared) {
    if shared.historic_mode_active {
        shared.historic_mode_active = false;
        shared.historic_selection = None;
    }
}

/// `_safety_check_historic_mode`: any non-base selection kills historic mode.
fn safety_check_historic_mode(shared: &mut SkinsShared) {
    let (Some(locked), Some(selected)) = (shared.locked_champ_id, shared.last_hovered_skin_id) else { return };
    let base_skin_id = locked * 1000;
    if shared.historic_mode_active && selected != base_skin_id {
        shared.historic_mode_active = false;
        shared.historic_selection = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_with_skin(champion_id: i64, skin_id: i64, name: &str) -> ChampionSkinCache {
        let mut cache = ChampionSkinCache { champion_id: Some(champion_id), ..Default::default() };
        let skin = crate::skins::lcu_ext::SkinInfo {
            skin_id,
            champion_id,
            skin_name: name.to_string(),
            ..Default::default()
        };
        cache.skin_id_map.insert(skin_id, skin.clone());
        cache.skins.push(skin);
        cache
    }

    #[test]
    fn base_skin_selection_clears_chroma_and_sets_key() {
        let mut shared = SkinsShared::default();
        let cache = cache_with_skin(99, 99000, "Lux");
        handle_selection(&mut shared, Some(&cache), 99000, 0, "");
        assert_eq!(shared.selected_chroma_id, None);
        assert_eq!(shared.last_hovered_skin_key, Some("Lux".to_string()));
    }

    #[test]
    fn form_selection_uses_special_table_and_sets_form_path() {
        let mut shared = SkinsShared::default();
        let cache = cache_with_skin(99, 99007, "Elementalist Lux");
        handle_selection(&mut shared, Some(&cache), 99007, 99999, "Fire");
        assert_eq!(shared.selected_chroma_id, Some(99999));
        assert_eq!(shared.last_hovered_skin_id, Some(99999));
        assert_eq!(shared.selected_form_path, Some("Lux/Forms/Elementalist Lux Fire.zip".to_string()));
        assert_eq!(shared.last_hovered_skin_key, Some("Elementalist Lux Fire".to_string()));
    }

    #[test]
    fn regular_chroma_selection_sets_ids_and_disables_historic() {
        let mut shared = SkinsShared::default();
        shared.historic_mode_active = true;
        let cache = cache_with_skin(103, 103000, "Ahri");
        handle_selection(&mut shared, Some(&cache), 103000, 103001, "Foxfire");
        assert_eq!(shared.selected_chroma_id, Some(103001));
        assert!(!shared.historic_mode_active);
        assert_eq!(shared.last_hovered_skin_key, Some("Ahri 103001".to_string()));
    }

    #[test]
    fn safety_check_disables_historic_on_non_base_selection() {
        let mut shared = SkinsShared::default();
        shared.historic_mode_active = true;
        shared.locked_champ_id = Some(103);
        shared.last_hovered_skin_id = Some(103001); // not the base skin (103000)
        safety_check_historic_mode(&mut shared);
        assert!(!shared.historic_mode_active);
    }
}
