//! Injection decision engine — ported from `threads/handlers/injection_trigger.py`
//! (`InjectionTrigger`), the densest module in the Python original
//! (~1350 lines). Ported here: the mod-selection priority (custom skin mod >
//! owned skin/chroma LCU force > unowned-skin ZIP extraction > map/font/
//! announcer/other mods only), historic auto-selection of a previously-used
//! custom mod / category mod (so the user doesn't have to reopen the Custom
//! Mods UI every game), the owned/base-skin LCU force-and-verify dance
//! (action-based PATCH falling back to `my-selection`), and the base-skin
//! confirmation telemetry (`injection::base_skin_tracker`).
//!
//! RECONCILED (was "FLAGGED FOR THE LEAD"): Python's `_inject_custom_mod` ran
//! its own clean+extract+`OverlayManager.mk_run_overlay` sequence directly
//! against `SkinInjector.overlay_manager`, bypassing `SkinInjector.inject_skin`
//! entirely. S3's Rust port only exposes `InjectionManager::
//! inject_skin_immediately(skin_name, ..., extra_mod_names)`, which ALWAYS
//! resolves+extracts `skin_name` as the primary mod via `SkinInjector::
//! inject_skin` — and that function used to call `storage::clean_mods_dir`
//! UNCONDITIONALLY before extracting its primary skin, wiping any
//! `extra_mod_names` folders this module had just pre-extracted into
//! `mods_dir`. Fixed per `injector.rs::inject_skin`'s "CLEAN ORDERING
//! CONTRACT" doc comment: `inject_skin` now only cleans when it has no
//! extras to preserve, so `run_custom_mod_injection` below cleans `mods_dir`
//! itself BEFORE extracting the custom mod / category mods (see the
//! `storage::clean_mods_dir` call at its top) — the union of primary +
//! extras now survives into the overlay.

#![allow(dead_code)] // consumed by ticker.rs; S9 troubleshooting UI

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Manager};

use crate::lcu::{self, Auth};
use crate::skins::bridge::BridgeHandle;
use crate::skins::features::historic::{self, HistoricEntry};
use crate::skins::features::special;
use crate::skins::injection::storage::{self, ModStorageService};
use crate::skins::injection::{base_skin_tracker, zips, InjectionManager};
use crate::skins::lcu_ext;
use crate::skins::paths;
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::state::{CategoryModSelection, CategoryModSelections, CustomModSelection};
use crate::skins::SkinsState;
use crate::{AppState, LockExt};

/// `config.BASE_SKIN_VERIFICATION_WAIT_S` (0.15s).
const BASE_SKIN_VERIFICATION_WAIT: Duration = Duration::from_millis(150);
/// `config.LOG_SEPARATOR_WIDTH`.
const LOG_SEPARATOR_WIDTH: usize = 80;

/// Entry point called by `ticker.rs` at the loadout deadline (ported from
/// `InjectionTrigger.trigger_injection`). `name` is the already-resolved
/// injection token (`"skin_1234"` / `"chroma_5678"`) from
/// `ticker::resolve_injection_name`.
pub async fn trigger_injection(app: AppHandle, skins: Arc<SkinsState>, ticker_id: u64, name: String, champion_name: String) {
    let app_state = app.state::<Arc<AppState>>().inner().clone();
    if app_state.injection_blocked.load(Ordering::SeqCst) {
        log_warn!("[INJECT] Injection blocked (ranked kill-switch) - skipping trigger for {name}");
        return;
    }

    let Some(injection) = app_state.skins_injection.lock_safe().clone() else {
        log_warn!("[INJECT] Injection manager not available yet - skipping trigger for {name}");
        return;
    };
    let bridge = app_state.skins_bridge.lock_safe().clone();
    // Party mode: the connected peers' skins get staged into the overlay too,
    // which is what makes party members see each other's skins in-game. Held
    // here so each injection path can fold the peer mods in.
    let party_mgr = app_state.skins_party.lock_safe().clone();

    // Resolve the League "Game" directory lazily and set it every trigger
    // (cheap, and the install dir can change between client launches) —
    // `mkoverlay`'s `--game:<path>` is unset without it, making injection a
    // silent no-op.
    let Some(game_dir) = lcu_ext::resolve_game_dir() else {
        log_warn!("[INJECT] Could not resolve League game directory (client not running?) - skipping trigger for {name}");
        return;
    };
    injection.set_game_dir(game_dir);

    // Mark that we've processed the last hovered skin (first effectful line
    // of Python's `trigger_injection`, past its `if not name: return` guard).
    {
        let mut shared = skins.shared.lock_safe();
        shared.last_hover_written = true;
    }

    // No own skin selected (this player kept their default / didn't pick one).
    // We still owe the overlay any connected party peers' skins AND any selected
    // category mods (map/font/announcer). An empty name used to abort the whole
    // trigger, silently dropping teammates' skins (the ARAM "she didn't pick, so
    // she saw nobody's skin" bug) and category mods. Route through
    // `run_custom_mod_injection` with `base_skin_name: None`: it stages party +
    // category mods and injects them mods-only (no primary skin), and logs a
    // clean skip if there's nothing to inject. The ranked kill-switch above
    // still applies.
    if name.is_empty() {
        let (selected_custom_mod, category_mods, champ_id) = {
            let shared = skins.shared.lock_safe();
            (shared.selected_custom_mod.clone(), shared.category_mods.clone(), shared.locked_champ_id.or(shared.hovered_champ_id))
        };
        let selected_custom_mod = drop_stale_custom_mod(&skins, bridge.as_ref(), selected_custom_mod, champ_id);
        let custom = selected_custom_mod.unwrap_or_else(|| CustomModSelection {
            skin_id: 0,
            champion_id: champ_id.unwrap_or(0),
            mod_name: String::new(),
            mod_path: String::new(),
            relative_path: String::new(),
        });
        run_custom_mod_injection(&app, &skins, &injection, bridge.as_ref(), custom, &category_mods, None, champion_name.clone(), &party_mgr).await;
        return;
    }

    let (ui_skin_id, selected_chroma_id, champ_id, owned_skin_ids, local_cell_id, random_mode_active) = {
        let shared = skins.shared.lock_safe();
        (
            shared.last_hovered_skin_id,
            shared.selected_chroma_id,
            shared.locked_champ_id.or(shared.hovered_champ_id),
            shared.owned_skin_ids.clone(),
            shared.local_cell_id,
            shared.random_mode_active,
        )
    };

    // Chroma override: use the selected chroma for owned-skin forcing if it
    // actually belongs to the hovered skin (chroma window base+1..=base+99).
    let effective_skin_id = match (selected_chroma_id, ui_skin_id) {
        (Some(chroma), Some(base)) if special::is_chroma_of(chroma, base) => chroma,
        _ => ui_skin_id.unwrap_or(0),
    };

    auto_select_historic_custom_mod(&skins, champ_id, ui_skin_id);
    auto_select_historic_category_mods(&skins);

    let (selected_custom_mod, category_mods) = {
        let shared = skins.shared.lock_safe();
        (shared.selected_custom_mod.clone(), shared.category_mods.clone())
    };

    // A custom mod selected for a DIFFERENT champion is stale — the user
    // picked it, then re-picked/swapped champions without reopening the
    // Custom Mods UI. Injecting it anyway forced the wrong champion's mod
    // into the overlay (observed: Selena S.T.U.N Ahri injected into an
    // Akshan game — a 31s multi-champion build + a crash). Clear it and
    // fall through to the normal skin path.
    let selected_custom_mod = drop_stale_custom_mod(&skins, bridge.as_ref(), selected_custom_mod, champ_id);

    log_trigger_summary(ticker_id, &name, selected_custom_mod.as_ref(), &category_mods);

    let has_other_mods = category_mods.map.is_some()
        || category_mods.font.is_some()
        || category_mods.announcer.is_some()
        || !category_mods.others.is_empty();

    if let Some(custom_mod) = &selected_custom_mod {
        let target_skin_id = custom_mod.skin_id;
        // A base skin (`skin_id % 1000 == 0`) is always "owned" — everyone has
        // it and its assets are already in the game, so there's NO downloadable
        // base-skin ZIP to fetch. Riot's inventory only lists purchased skins,
        // so without this a library custom mod (which targets the base slot
        // `champ*1000`) is treated as unowned and dies looking for a base-skin
        // ZIP. Treating it as owned routes it to the mods-only overlay (no ZIP);
        // `run_custom_mod_injection` still force-selects the base skin so the
        // game loads the assets the mod overrides.
        let target_is_base = target_skin_id % 1000 == 0;
        let is_owned = target_is_base || owned_skin_ids.contains(&target_skin_id);
        let base_skin_name = if is_owned { None } else { Some(name.clone()) };
        if is_owned {
            log_info!("[INJECT] Custom mod selected for owned/base skin {target_skin_id}, injecting custom mod only");
        } else {
            log_info!("[INJECT] Custom mod selected for unowned skin {target_skin_id}, injecting base skin ZIP + custom mod");
        }
        run_custom_mod_injection(&app, &skins, &injection, bridge.as_ref(), custom_mod.clone(), &category_mods, base_skin_name, champion_name.clone(), &party_mgr).await;
        return;
    }

    if has_other_mods {
        let is_owned = ui_skin_id.is_some_and(|id| owned_skin_ids.contains(&id));
        let base_skin_name = if !is_owned && ui_skin_id != Some(0) { Some(name.clone()) } else { None };
        let dummy = CustomModSelection {
            skin_id: ui_skin_id.unwrap_or(0),
            champion_id: champ_id.unwrap_or(0),
            mod_name: name.to_uppercase(),
            mod_path: String::new(),
            relative_path: String::new(),
        };
        run_custom_mod_injection(&app, &skins, &injection, bridge.as_ref(), dummy, &category_mods, base_skin_name, champion_name.clone(), &party_mgr).await;
        return;
    }

    // Skip injection for base skins (only reached once no mods are selected).
    if ui_skin_id == Some(0) {
        log_info!("[INJECT] skipping base skin injection (skinId=0) - no mods-only flow available");
        injection.resume_if_suspended();
        return;
    }

    let Some(auth) = lcu::cached_auth() else {
        log_warn!("[INJECT] LCU not available - skipping trigger for {name}");
        injection.resume_if_suspended();
        return;
    };
    let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);

    // Stage party-member skins now (cleans the mods dir first so they survive)
    // and fold them into the overlay for both the owned and unowned paths.
    let party_folders = stage_party_mods(&party_mgr).await;

    // Force owned skins/chromas via LCU (still runs injection afterward so
    // the overlay is built with any party/category mods).
    let base_owned = owned_skin_ids.contains(&effective_skin_id)
        || (ui_skin_id.is_some_and(|id| owned_skin_ids.contains(&id)) && Some(effective_skin_id) != ui_skin_id);
    if base_owned {
        force_owned_skin(&client, &auth, local_cell_id, effective_skin_id, champ_id, random_mode_active, &injection).await;
        spawn_owned_injection(app.clone(), skins.clone(), injection.clone(), name.clone(), champion_name.clone(), champ_id, party_folders);
        return;
    }

    // Inject if the user doesn't own the hovered skin.
    inject_unowned_skin(app, skins, client, auth, injection, bridge, name, champion_name, champ_id, local_cell_id, random_mode_active, party_folders).await;
}

/// Clean the mods dir and (re)stage every connected party peer's skin into it,
/// returning their folder names. Party mods must be staged AFTER the mods-dir
/// clean or they'd be wiped, so the owned/unowned paths (which otherwise let
/// the injector do the clean) call this and pass the folders as
/// `extra_mod_names` — the injector then skips its own clean and keeps them.
/// Returns empty (and does NOT clean) when party mode is off or has no peers.
async fn stage_party_mods(party_mgr: &Option<Arc<crate::skins::party::manager::PartyManager>>) -> Vec<String> {
    let Some(pm) = party_mgr else { return Vec::new() };
    if pm.get_party_skins().await.is_empty() {
        return Vec::new();
    }
    storage::clean_mods_dir(&paths::injection_mods_dir());
    let staged = pm.prepare_party_mods().await;
    if !staged.is_empty() {
        log_info!("[INJECT] Staged {} party-member skin(s) for the overlay", staged.len());
    }
    staged
}

fn log_trigger_summary(ticker_id: u64, name: &str, custom_mod: Option<&CustomModSelection>, category_mods: &CategoryModSelections) {
    let mut labels = Vec::new();
    match custom_mod {
        Some(m) if !m.mod_name.is_empty() => labels.push(format!("{} (SKIN_{})", m.mod_name, m.skin_id)),
        _ => labels.push(name.to_uppercase()),
    }
    if let Some(m) = &category_mods.map {
        labels.push(format!("MAP: {}", m.mod_name));
    }
    if let Some(m) = &category_mods.font {
        labels.push(format!("FONT: {}", m.mod_name));
    }
    if let Some(m) = &category_mods.announcer {
        labels.push(format!("ANNOUNCER: {}", m.mod_name));
    }
    if !category_mods.others.is_empty() {
        let names: Vec<_> = category_mods.others.iter().map(|m| m.mod_name.clone()).collect();
        labels.push(format!("OTHER: {}", names.join(", ")));
    }
    log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
    log_info!("PREPARING INJECTION >>> {} <<<", labels.join(" + "));
    log_info!("   Loadout Timer: #{ticker_id}");
    log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
}

// ---------------------------------------------------------------------
// Historic auto-selection — ported from the `injection_trigger.py` block
// that auto-picks a previously-used custom mod / category mod so the user
// doesn't have to reopen the Custom Mods UI every game.
// ---------------------------------------------------------------------

/// Ported from `injection_trigger.py`'s historic-custom-mod auto-select
/// block: if no custom mod is already selected, look up this champion's
/// last-used custom mod path from `historic.json` and, if it still exists in
/// mod storage AND matches the skin currently being injected, select it.
fn auto_select_historic_custom_mod(skins: &Arc<SkinsState>, champ_id: Option<i64>, ui_skin_id: Option<i64>) {
    if skins.shared.lock_safe().selected_custom_mod.is_some() {
        return;
    }
    let Some(champ_id) = champ_id else { return };
    let Some(entry) = historic::get_historic_skin_for_champion(champ_id) else { return };
    let Some(relative_path) = entry.custom_mod_path() else { return };

    let normalized = relative_path.replace('\\', "/");
    let mut parts = normalized.splitn(2, '/');
    let (Some("skins"), Some(rest)) = (parts.next(), parts.next()) else {
        log_warn!("[HISTORIC] Invalid saved custom mod path format: {relative_path}");
        return;
    };
    let Some(historic_skin_id) = rest.split('/').next().and_then(|s| s.parse::<i64>().ok()) else { return };
    if let Some(ui_id) = ui_skin_id {
        if historic_skin_id != ui_id {
            return; // stored mod is for a different skin than what's being injected
        }
    }

    let mod_storage = ModStorageService::new(paths::mods_dir());
    let entries = mod_storage.list_mods_for_skin(historic_skin_id);
    let Some(found) = entries.iter().find(|e| relative_path_of(&e.path, mod_storage.mods_root()) == normalized) else {
        log_warn!("[HISTORIC] Saved custom mod not found in storage: {relative_path}");
        return;
    };

    let selection = CustomModSelection {
        skin_id: historic_skin_id,
        champion_id: special::champion_of(historic_skin_id),
        mod_name: found.mod_name.clone(),
        mod_path: found.path.to_string_lossy().into_owned(),
        relative_path: normalized,
    };
    log_info!("[HISTORIC] Auto-selected saved custom mod: {} (skin {historic_skin_id})", selection.mod_name);
    skins.shared.lock_safe().selected_custom_mod = Some(selection);
}

/// Clear (and un-broadcast) a selected custom mod whose target champion
/// doesn't match the champion actually locked/hovered for THIS game.
/// Returns the selection only while it's still valid.
fn drop_stale_custom_mod(
    skins: &Arc<SkinsState>,
    bridge: Option<&BridgeHandle>,
    selection: Option<CustomModSelection>,
    champ_id: Option<i64>,
) -> Option<CustomModSelection> {
    let m = selection?;
    let stale = m.champion_id != 0 && champ_id.is_some_and(|c| c != m.champion_id);
    if !stale {
        return Some(m);
    }
    log_warn!(
        "[INJECT] Clearing stale custom mod '{}' - it targets champion {} but champion {:?} is locked",
        m.mod_name,
        m.champion_id,
        champ_id
    );
    skins.shared.lock_safe().selected_custom_mod = None;
    if let Some(b) = bridge {
        b.broadcast_custom_mod_state(false, None, None);
    }
    None
}

fn relative_path_of(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Ported from `injection_trigger.py`'s `auto_select_historic_mod` closure,
/// called for map/font/announcer + the six "other" category buckets
/// (`features::historic::MOD_HISTORIC_CATEGORIES`, minus map/font/announcer
/// which have their own single-slot fields on `ModHistoric`).
fn auto_select_historic_category_mods(skins: &Arc<SkinsState>) {
    let mod_storage = ModStorageService::new(paths::mods_dir());
    let historic_mods = historic::load_mod_historic();

    let (has_map, has_font, has_announcer, has_others) = {
        let shared = skins.shared.lock_safe();
        (
            shared.category_mods.map.is_some(),
            shared.category_mods.font.is_some(),
            shared.category_mods.announcer.is_some(),
            !shared.category_mods.others.is_empty(),
        )
    };

    if !has_map {
        if let Some(path) = &historic_mods.map {
            if let Some(sel) = auto_select_category(&mod_storage, storage::CATEGORY_MAPS, path) {
                log_info!("[HISTORIC] Auto-selected historic map mod: {}", sel.mod_name);
                skins.shared.lock_safe().category_mods.map = Some(sel);
            }
        }
    }
    if !has_font {
        if let Some(path) = &historic_mods.font {
            if let Some(sel) = auto_select_category(&mod_storage, storage::CATEGORY_FONTS, path) {
                log_info!("[HISTORIC] Auto-selected historic font mod: {}", sel.mod_name);
                skins.shared.lock_safe().category_mods.font = Some(sel);
            }
        }
    }
    if !has_announcer {
        if let Some(path) = &historic_mods.announcer {
            if let Some(sel) = auto_select_category(&mod_storage, storage::CATEGORY_ANNOUNCERS, path) {
                log_info!("[HISTORIC] Auto-selected historic announcer mod: {}", sel.mod_name);
                skins.shared.lock_safe().category_mods.announcer = Some(sel);
            }
        }
    }
    if !has_others {
        let mut valid = Vec::new();
        for (category, paths_list) in [
            (storage::CATEGORY_UI, &historic_mods.ui),
            (storage::CATEGORY_VOICEOVER, &historic_mods.voiceover),
            (storage::CATEGORY_LOADING_SCREEN, &historic_mods.loading_screen),
            (storage::CATEGORY_VFX, &historic_mods.vfx),
            (storage::CATEGORY_SFX, &historic_mods.sfx),
            (storage::CATEGORY_OTHERS, &historic_mods.others),
        ] {
            for path in paths_list {
                if let Some(sel) = auto_select_category(&mod_storage, category, path) {
                    log_info!("[HISTORIC] Auto-selected historic other mod: {}", sel.mod_name);
                    valid.push(sel);
                }
            }
        }
        if !valid.is_empty() {
            skins.shared.lock_safe().category_mods.others = valid;
        }
    }
}

fn auto_select_category(mod_storage: &ModStorageService, category: &str, historic_path: &str) -> Option<CategoryModSelection> {
    let entry = mod_storage.list_mods_for_category(category).into_iter().find(|e| e.id == historic_path)?;
    let source = mod_storage.mods_root().join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if !source.exists() {
        log_info!("[HISTORIC] Historic {category} mod file not found (mod may have been deleted), ignoring: {}", source.display());
        return None;
    }
    Some(CategoryModSelection {
        mod_name: entry.name,
        mod_path: source.to_string_lossy().into_owned(),
        mod_folder_name: mod_folder_name_for(&source),
        relative_path: entry.id,
    })
}

fn mod_folder_name_for(path: &Path) -> String {
    if path.is_dir() {
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "mod".to_string())
    } else {
        path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "mod".to_string())
    }
}

/// Extract (or junction) `source` into `mods_dir`, returning its resulting
/// folder name (ported from the repeated re-extract-after-clean pattern in
/// `injection_trigger.py`'s `_inject_custom_mod`).
fn extract_mod(source: &str, mods_dir: &Path, cache_dir: &Path) -> Option<String> {
    let source_path = Path::new(source);
    if !source_path.exists() {
        log_info!("[INJECT] Mod source not found (may have been deleted), ignoring: {}", source_path.display());
        return None;
    }
    let folder_name = mod_folder_name_for(source_path);
    let dest = mods_dir.join(&folder_name);
    zips::safe_remove_entry(&dest);
    if let Err(e) = zips::link_or_extract(source_path, &dest, cache_dir) {
        log_warn!("[INJECT] Failed to extract mod {}: {e}", source_path.display());
        return None;
    }
    dest.exists().then_some(folder_name)
}

// ---------------------------------------------------------------------
// Custom-mod / category-mods injection path.
// ---------------------------------------------------------------------

/// Ported from `InjectionTrigger._inject_custom_mod` — see this module's
/// doc comment for the `clean_mods_dir`/`extra_mod_names` gap this inherits.
#[allow(clippy::too_many_arguments)]
async fn run_custom_mod_injection(
    app: &AppHandle,
    skins: &Arc<SkinsState>,
    injection: &Arc<InjectionManager>,
    bridge: Option<&BridgeHandle>,
    custom_mod: CustomModSelection,
    category_mods: &CategoryModSelections,
    base_skin_name: Option<String>,
    champion_name: String,
    party_mgr: &Option<Arc<crate::skins::party::manager::PartyManager>>,
) {
    let mods_dir = paths::injection_mods_dir();
    let cache_dir = paths::injection_extract_cache_dir();
    // Clean BEFORE any extraction (custom mod, category mods, or the primary
    // `inject_skin_immediately` resolves below) so the mods dir ends up with
    // the UNION of everything this overlay needs — `SkinInjector::
    // inject_skin` itself skips its own clean once it sees `extra_names` is
    // non-empty (see its "CLEAN ORDERING CONTRACT" doc comment), so this is
    // the one clean that runs for this whole overlay.
    storage::clean_mods_dir(&mods_dir);
    let mut extra_names = Vec::new();
    let mut labels = Vec::new();
    let has_custom_skin_folder = !custom_mod.relative_path.is_empty() && !custom_mod.mod_path.is_empty();

    if let Some(base_name) = &base_skin_name {
        log_info!("[INJECT] Extracting base skin ZIP: {base_name}");
        // The base skin becomes `inject_skin_immediately`'s primary
        // `skin_name` below (it does its own resolve+extract) — nothing to
        // pre-extract here for it.
        labels.push(format!("Base Skin ({base_name})"));
    }

    if has_custom_skin_folder {
        if let Some(folder) = extract_mod(&custom_mod.mod_path, &mods_dir, &cache_dir) {
            log_info!("[INJECT] Custom skin mod ready: {folder}");
            extra_names.push(folder);
            labels.push(if custom_mod.mod_name.is_empty() { "Custom Mod".to_string() } else { custom_mod.mod_name.clone() });
        }
    }

    for (label_prefix, sel) in [("Map", &category_mods.map), ("Font", &category_mods.font), ("Announcer", &category_mods.announcer)] {
        if let Some(m) = sel {
            if let Some(folder) = extract_mod(&m.mod_path, &mods_dir, &cache_dir) {
                log_info!("[INJECT] Including {} mod: {}", label_prefix.to_lowercase(), m.mod_name);
                extra_names.push(folder);
                labels.push(format!("{label_prefix} ({})", m.mod_name));
            }
        }
    }
    for m in &category_mods.others {
        if let Some(folder) = extract_mod(&m.mod_path, &mods_dir, &cache_dir) {
            log_info!("[INJECT] Including other mod: {}", m.mod_name);
            extra_names.push(folder);
            labels.push(format!("Other ({})", m.mod_name));
        }
    }
    // Fold in party-member skins. The mods dir was already cleaned at the top
    // of this function, so `prepare_party_mods` stages into it without wiping
    // what we just extracted.
    if let Some(pm) = party_mgr {
        for folder in pm.prepare_party_mods().await {
            log_info!("[INJECT] Including party-member skin: {folder}");
            extra_names.push(folder);
            labels.push("Party skin".to_string());
        }
    }

    if base_skin_name.is_none() && extra_names.is_empty() {
        log_warn!("[INJECT] No mods available to inject (skin, map, font, announcer, or other)");
        injection.resume_if_suspended();
        return;
    }

    let champion_id = if custom_mod.champion_id != 0 { Some(custom_mod.champion_id) } else { None };

    // Force the champion's base skin via the LCU when the overlay's assets are
    // keyed to it: the unowned path (base ZIP + mod), OR a real custom skin mod
    // that targets the base skin itself (the user may have a different skin
    // selected in champ select, so we must move them to base for the mod to
    // show). Scoped to `has_custom_skin_folder` so the category-mods-only
    // `dummy` selection (whose skin_id can be a base value) doesn't force a base
    // skin when the user only added a map/font/announcer.
    let target_is_base_custom_skin = has_custom_skin_folder && custom_mod.skin_id % 1000 == 0;
    if base_skin_name.is_some() || target_is_base_custom_skin {
        if let (Some(cid), Some(auth)) = (champion_id, lcu::cached_auth()) {
            let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);
            let (local_cell, random_active) = {
                let shared = skins.shared.lock_safe();
                (shared.local_cell_id, shared.random_mode_active)
            };
            force_base_skin(&client, &auth, local_cell, cid * 1000, random_active, bridge).await;
        }
    }
    if let Some(b) = bridge {
        if has_custom_skin_folder {
            b.broadcast_custom_mod_state(true, Some(custom_mod.mod_name.clone()).filter(|s| !s.is_empty()), Some(custom_mod.skin_id));
        }
    }

    log_info!("[INJECT] Injecting mods: {}", labels.join(", "));

    spawn_game_end_watcher(skins.clone(), injection.clone());

    let injection = injection.clone();
    let ticker_champion_name = champion_name;
    tauri::async_runtime::spawn_blocking(move || {
        // With a base skin -> normal inject (it resolves + extracts the primary
        // and folds in `extra_names`). WITHOUT one (party and/or category mods
        // only) -> the mods-only overlay path: routing pure extras through
        // `inject_skin_immediately` with a `skin_0` placeholder would trip its
        // base-skin short-circuit and silently drop every extra mod.
        let ok = match &base_skin_name {
            Some(primary) => injection.inject_skin_immediately(primary, None, Some(&ticker_champion_name), champion_id, &extra_names),
            None => injection.inject_mods_only_immediately(&extra_names),
        };
        if ok {
            log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
            log_info!("CUSTOM MOD INJECTION COMPLETED");
            log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
            if let Some(cid) = champion_id {
                if has_custom_skin_folder {
                    historic::write_historic_entry(cid, HistoricEntry::Path(format!("path:{}", custom_mod.relative_path)));
                }
            }
        } else {
            log_error!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
            log_error!("CUSTOM MOD INJECTION FAILED");
            log_error!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
        }
    });
    // `app` is reserved for a later milestone (S9 event emission on
    // injection completion); not read yet.
    let _ = app;
}

// ---------------------------------------------------------------------
// Owned-skin / unowned-skin injection paths.
// ---------------------------------------------------------------------

fn spawn_owned_injection(
    app: AppHandle,
    skins: Arc<SkinsState>,
    injection: Arc<InjectionManager>,
    name: String,
    champion_name: String,
    champion_id: Option<i64>,
    party_folders: Vec<String>,
) {
    // `app` is reserved for a later milestone (S9 event emission); not read yet.
    let _ = &app;
    spawn_game_end_watcher(skins, injection.clone());
    tauri::async_runtime::spawn_blocking(move || {
        // Your own owned skin shows natively; `party_folders` are the peer
        // skins folded into the overlay so party members see each other's.
        let ok = injection.inject_skin_immediately(&name, None, Some(&champion_name), champion_id, &party_folders);
        if ok {
            log_info!("[INJECT] Owned-skin overlay build completed");
        } else {
            log_warn!("[INJECT] Owned-skin overlay build failed or was skipped");
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn inject_unowned_skin(
    app: AppHandle,
    skins: Arc<SkinsState>,
    client: reqwest::Client,
    auth: Auth,
    injection: Arc<InjectionManager>,
    bridge: Option<BridgeHandle>,
    name: String,
    champion_name: String,
    champ_id: Option<i64>,
    local_cell_id: Option<i64>,
    random_mode_active: bool,
    party_folders: Vec<String>,
) {
    if let Some(cid) = champ_id {
        let base_skin_id = cid * 1000;
        let actual = verify_skin_applied(&client, &auth, local_cell_id, base_skin_id).await;
        if actual != Some(base_skin_id) {
            force_base_skin(&client, &auth, local_cell_id, base_skin_id, random_mode_active, bridge.as_ref()).await;
        }
    }

    log_info!("[INJECT] Starting injection: {name}");
    spawn_game_end_watcher(skins.clone(), injection.clone());

    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // `name` is the user's unowned skin (primary); `party_folders` are the
        // connected peers' skins folded in so party members see each other's.
        let success = injection.inject_skin_immediately(&name, None, Some(&champion_name), champ_id, &party_folders);

        if random_mode_active {
            let mut shared = skins.shared.lock_safe();
            shared.random_skin_name = None;
            shared.random_skin_id = None;
            shared.random_mode_active = false;
            drop(shared);
            log_info!("[RANDOM] Random mode cleared after injection");
            if let Some(b) = &bridge {
                b.broadcast_random_mode_state(false, None);
            }
        }

        if success {
            if let (Some(cid), Some(injected_id)) = (champ_id, parse_injected_id(&name)) {
                historic::write_historic_entry(cid, HistoricEntry::Skin(injected_id));
                log_info!("[HISTORIC] Stored last injected ID {injected_id} for champion {cid}");
            }
            log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
            log_info!("INJECTION COMPLETED >>> {} <<<", name.to_uppercase());
            log_info!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
        } else {
            log_error!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
            log_error!("INJECTION FAILED >>> {} <<<", name.to_uppercase());
            log_error!("{}", "=".repeat(LOG_SEPARATOR_WIDTH));
        }
        // `app` is reserved for a later milestone (S9 event emission); not read yet.
        let _ = &app;
    });
}

fn parse_injected_id(name: &str) -> Option<i64> {
    name.split_once('_').and_then(|(_, id)| id.parse::<i64>().ok())
}

// ---------------------------------------------------------------------
// LCU force/verify helpers — ported from `_force_owned_skin`/`_force_base_skin`.
// ---------------------------------------------------------------------

async fn find_pick_action(client: &reqwest::Client, auth: &Auth, my_cell: i64) -> Option<(i64, bool)> {
    let session = lcu_ext::champ_select_session(client, auth).await?;
    for round in session.actions.iter().flatten() {
        for action in round {
            if action.actor_cell_id == Some(my_cell) && action.kind.as_deref() == Some("pick") {
                return Some((action.id.unwrap_or(0), action.completed.unwrap_or(false)));
            }
        }
    }
    None
}

/// Action-based PATCH falling back to `my-selection` (ported from the
/// repeated try-action-then-my-selection pattern in both
/// `_force_owned_skin`/`_force_base_skin`).
async fn force_skin_via_lcu(client: &reqwest::Client, auth: &Auth, my_cell: Option<i64>, target_skin_id: i64) -> bool {
    if let Some(my_cell) = my_cell {
        if let Some((action_id, completed)) = find_pick_action(client, auth, my_cell).await {
            if !completed && action_id != 0 && lcu_ext::set_selected_skin(client, auth, action_id, target_skin_id).await {
                return true;
            }
        }
    }
    lcu_ext::set_my_selection_skin(client, auth, target_skin_id).await
}

async fn verify_skin_applied(client: &reqwest::Client, auth: &Auth, my_cell: Option<i64>, target_skin_id: i64) -> Option<i64> {
    let _ = target_skin_id;
    let session = lcu_ext::champ_select_session(client, auth).await?;
    let my_cell = my_cell?;
    session.my_team.iter().flatten().find(|p| p.cell_id == Some(my_cell)).and_then(|p| p.selected_skin_id)
}

/// `InjectionTrigger._force_owned_skin`.
async fn force_owned_skin(
    client: &reqwest::Client,
    auth: &Auth,
    local_cell_id: Option<i64>,
    skin_id: i64,
    champ_id: Option<i64>,
    random_mode_active: bool,
    injection: &Arc<InjectionManager>,
) {
    if champ_id.is_none() {
        return;
    }
    log_info!("[INJECT] User owns this skin/chroma (skinId={skin_id}), forcing selection via LCU");

    if force_skin_via_lcu(client, auth, local_cell_id, skin_id).await {
        log_info!("[INJECT] Owned skin/chroma forced");
        if !random_mode_active {
            tokio::time::sleep(BASE_SKIN_VERIFICATION_WAIT).await;
            match verify_skin_applied(client, auth, local_cell_id, skin_id).await {
                Some(actual) if actual == skin_id => log_info!("[INJECT] Owned skin/chroma verified: {actual}"),
                Some(actual) => log_warn!("[INJECT] Verification failed: {actual} != {skin_id}"),
                None => {}
            }
        } else {
            log_info!("[INJECT] Skipping verification wait in random mode");
        }
    } else {
        log_warn!("[INJECT] Failed to force owned skin/chroma");
    }

    injection.resume_if_suspended();
}

/// `InjectionTrigger._force_base_skin` (minus the Qt-era UI-hide calls — S9/JS
/// territory now; `broadcast_skip_base_skin` replaces
/// `state.ui_skin_thread._broadcast_skip_base_skin()`).
async fn force_base_skin(
    client: &reqwest::Client,
    auth: &Auth,
    local_cell_id: Option<i64>,
    base_skin_id: i64,
    random_mode_active: bool,
    bridge: Option<&BridgeHandle>,
) {
    log_info!("[INJECT] Forcing base skin (skinId={base_skin_id})");
    if let Some(b) = bridge {
        b.broadcast_skip_base_skin();
    }

    let t_force0 = std::time::Instant::now();
    let forced = force_skin_via_lcu(client, auth, local_cell_id, base_skin_id).await;

    if !forced {
        log_warn!("[INJECT] Failed to force base skin - injection may fail");
        return;
    }

    log_info!("[INJECT] Base skin forced");
    let dt_force_s = t_force0.elapsed().as_secs_f64();
    log_info!("[INJECT] Base skin force time: {dt_force_s:.3}s");
    // Start tracking for WebSocket confirmation (base_skin_tracker persists
    // p90 timing samples the S9 troubleshooting UI recommends a threshold from).
    base_skin_tracker::start_tracking(base_skin_id);

    if random_mode_active {
        log_info!("[INJECT] Skipping base skin verification wait in random mode");
        return;
    }
    tokio::time::sleep(BASE_SKIN_VERIFICATION_WAIT).await;
    match verify_skin_applied(client, auth, local_cell_id, base_skin_id).await {
        Some(actual) if actual == base_skin_id => log_info!("[INJECT] Base skin verified: {actual}"),
        Some(actual) => log_warn!("[INJECT] Base skin verification failed: {actual} != {base_skin_id}"),
        None => {}
    }
}

/// Ported from `InjectionTrigger`'s local `game_ended_callback` closure —
/// Python threaded it into `inject_skin_immediately(stop_callback=...)` so
/// the overlay babysit loop could bail once the game had been InProgress and
/// then ended. `InjectionManager::inject_skin_immediately` (S3, out of this
/// milestone's file scope) has no such callback parameter, so this achieves
/// a similar effect from OUTSIDE via the existing public
/// `kill_all_runoverlay_processes` sweep instead of a callback threaded
/// through the blocking call.
fn spawn_game_end_watcher(skins: Arc<SkinsState>, injection: Arc<InjectionManager>) {
    tauri::async_runtime::spawn(async move {
        let mut has_been_in_progress = false;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let phase = skins.shared.lock_safe().phase.clone();
            match phase.as_deref() {
                Some("InProgress") => has_been_in_progress = true,
                Some("Reconnect") | Some("GameStart") => {}
                _ if has_been_in_progress => {
                    // OS-level reset, NOT `kill_all_runoverlay_processes` — the
                    // latter locks the injection mutex that this game's babysit
                    // loop still holds, so it would self-deadlock exactly when a
                    // runoverlay failed to self-exit (the case we're cleaning
                    // up). `reset_stuck_injection` kills by OS enumeration with
                    // no lock, letting the babysit loop's `try_wait` return.
                    injection.reset_stuck_injection();
                    break;
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_injected_id_extracts_trailing_number() {
        assert_eq!(parse_injected_id("skin_84002"), Some(84002));
        assert_eq!(parse_injected_id("chroma_103001"), Some(103001));
        assert_eq!(parse_injected_id("malformed"), None);
    }

    #[test]
    fn mod_folder_name_for_uses_stem_for_archives_and_name_for_dirs() {
        assert_eq!(mod_folder_name_for(Path::new("C:/mods/skins/84002/cool-mod_1.0.fantome")), "cool-mod_1.0");
        assert_eq!(mod_folder_name_for(Path::new("C:/mods/skins/84002/cool-mod.zip")), "cool-mod");
    }

    #[test]
    fn log_trigger_summary_falls_back_to_name_without_custom_mod() {
        // Smoke test only (this function is log-output-only) - verifying it
        // doesn't panic on the "no mods selected" shape.
        log_trigger_summary(1, "skin_84002", None, &CategoryModSelections::default());
    }
}
