//! Injection decision engine — ported from `threads/handlers/injection_trigger.py`
//! (`InjectionTrigger`), the densest module in the Python original (~1350
//! lines). Covers: mod-selection priority (custom skin mod > owned
//! skin/chroma LCU force > unowned-skin ZIP extraction > map/font/announcer/
//! other mods only), historic auto-selection of a previously-used custom mod
//! / category mod, the owned/base-skin LCU force-and-verify dance
//! (action-based PATCH falling back to `my-selection`), and base-skin
//! confirmation telemetry (`injection::base_skin_tracker`).
//!
//! `InjectionManager::inject_skin_immediately` ALWAYS resolves+extracts
//! `skin_name` as the primary mod via `SkinInjector::inject_skin`, which used
//! to `clean_mods_dir` UNCONDITIONALLY, wiping any `extra_mod_names` this
//! module had just pre-extracted. Fixed per `injector.rs::inject_skin`'s
//! CLEAN ORDERING CONTRACT: `inject_skin` now only cleans when it has no
//! extras to preserve, so `run_custom_mod_injection` cleans `mods_dir` itself
//! BEFORE extracting the custom mod / category mods — the union of primary +
//! extras now survives into the overlay.

#![allow(dead_code)] // consumed by ticker.rs; S9 troubleshooting UI

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

use crate::lcu::{self, Auth};
use crate::safety_manager::{self, InjectionDecision, InjectionDenial, InjectionOp};
use crate::skins::features::historic::{self, HistoricEntry};
use crate::skins::features::special;
use crate::skins::broken_mods;
use crate::skins::injection::storage::{self, ModStorageService};
use crate::skins::injection::{base_skin_tracker, target_detect, zips, InjectionManager};
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
/// Clears `injection_inflight` on drop so every return path (including the
/// transient early-returns below) releases it and a later retry can proceed.
/// Holds an `Arc` clone (not a borrow) so it doesn't conflict with the function
/// moving `skins` into downstream calls.
struct InflightGuard(Arc<SkinsState>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.injection_inflight.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

pub async fn trigger_injection(app: AppHandle, skins: Arc<SkinsState>, ticker_id: u64, name: String, champion_name: String) {
    // Serialize the two fire paths (loadout ticker + GameStart fallback) so they
    // can't both build an overlay for the same game concurrently. Test-and-set;
    // the guard clears it on return, so a transient early-return still retries.
    if skins
        .injection_inflight
        .compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
        .is_err()
    {
        log_warn!("[INJECT] Injection already in progress — skipping duplicate trigger for {name}");
        return;
    }
    let _inflight = InflightGuard(Arc::clone(&skins));

    let app_state = app.state::<Arc<AppState>>().inner().clone();
    // P0-A safety gate at the pipeline entry: full policy check (master
    // switch, versioned consent, LCU reachability, phase, ranked/unknown
    // queue) from the always-on monitor.
    if policy_denied(&app, InjectionOp::Build).is_some() {
        log_warn!("[INJECT] Injection blocked by safety policy - skipping trigger for {name}");
        return;
    }

    let Some(injection) = app_state.skins_injection.lock_safe().clone() else {
        log_warn!("[INJECT] Injection manager not available yet - skipping trigger for {name}");
        return;
    };
    // Party mode: the connected peers' skins get staged into the overlay too,
    // which is what makes party members see each other's skins in-game. Held
    // here so each injection path can fold the peer mods in.
    let party_mgr = app_state.skins_party.lock_safe().clone();

    // Resolve the League "Game" directory and set it every trigger (cheap,
    // can change between launches) — without it `mkoverlay`'s `--game:<path>`
    // is unset, making injection a silent no-op.
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

    // No own skin selected. We still owe the overlay any connected party
    // peers' skins AND selected category mods — an empty name used to abort
    // the whole trigger, silently dropping teammates' skins (the ARAM
    // "she didn't pick, so she saw nobody's skin" bug). Route through
    // `run_custom_mod_injection` with `base_skin_name: None`: stages party +
    // category mods and injects mods-only, logging a clean skip if there's
    // nothing to inject.
    if name.is_empty() {
        let (selected_custom_mod, category_mods, champ_id) = {
            let shared = skins.shared.lock_safe();
            (shared.selected_custom_mod.clone(), shared.category_mods.clone(), shared.locked_champ_id.or(shared.hovered_champ_id))
        };
        let selected_custom_mod = drop_stale_custom_mod(&skins, selected_custom_mod, champ_id);
        let custom = selected_custom_mod.unwrap_or_else(|| CustomModSelection {
            skin_id: 0,
            champion_id: champ_id.unwrap_or(0),
            mod_name: String::new(),
            mod_path: String::new(),
            relative_path: String::new(),
        });
        // "Mod picked, no skin hovered" is the headline case for target
        // detection — nothing else tells the game which slot to load.
        let route = if custom.mod_path.is_empty() {
            CustomModRoute::default()
        } else {
            plan_custom_mod_route(&skins, &custom, champ_id).await
        };
        run_custom_mod_injection(
            &app,
            &skins,
            &injection,
            custom,
            &category_mods,
            route.base_skin_name,
            route.chroma_id,
            None,
            route.force_slot,
            champion_name.clone(),
            &party_mgr,
        )
        .await;
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

    // Chroma to inject on the UNOWNED path: `resolve_zip("skin_<base>", Some(chroma))`
    // resolves the chroma's own .fantome. Only when the picked chroma actually
    // belongs to the hovered base (the owned path forces its chroma via LCU instead).
    let inject_chroma_id = match (selected_chroma_id, ui_skin_id) {
        (Some(chroma), Some(base)) if special::is_chroma_of(chroma, base) => Some(chroma),
        _ => None,
    };

    auto_select_historic_custom_mod(&skins, champ_id, ui_skin_id);
    auto_select_historic_category_mods(&skins);

    let (selected_custom_mod, category_mods) = {
        let shared = skins.shared.lock_safe();
        (shared.selected_custom_mod.clone(), shared.category_mods.clone())
    };

    // A custom mod selected for a DIFFERENT champion is stale — the user
    // re-picked/swapped champions without reopening the Custom Mods UI.
    // Injecting it anyway forced the wrong champion's mod into the overlay
    // (observed: a 31s multi-champion build + a crash). Clear it and fall
    // through to the normal skin path.
    let selected_custom_mod = drop_stale_custom_mod(&skins, selected_custom_mod, champ_id);

    log_trigger_summary(ticker_id, &name, selected_custom_mod.as_ref(), &category_mods);

    let has_other_mods = category_mods.map.is_some()
        || category_mods.font.is_some()
        || category_mods.announcer.is_some()
        || !category_mods.others.is_empty();

    log_info!(
        "[INJECT-DECISION] ui_skin={ui_skin_id:?} chroma={selected_chroma_id:?} effective={effective_skin_id} inject_chroma={inject_chroma_id:?} owns_ui={} owns_effective={} custom_mod={} other_mods={has_other_mods}",
        ui_skin_id.is_some_and(|id| owned_skin_ids.contains(&id)),
        owned_skin_ids.contains(&effective_skin_id),
        selected_custom_mod.is_some(),
    );

    if let Some(custom_mod) = &selected_custom_mod {
        let route = plan_custom_mod_route(&skins, custom_mod, champ_id).await;
        run_custom_mod_injection(
            &app,
            &skins,
            &injection,
            custom_mod.clone(),
            &category_mods,
            route.base_skin_name,
            route.chroma_id,
            ui_skin_id,
            route.force_slot,
            champion_name.clone(),
            &party_mgr,
        )
        .await;
        return;
    }

    if has_other_mods {
        // Check the EFFECTIVE pick (the chroma when one is picked, else the base
        // skin). Owning the base skin does NOT grant its chromas, so a picked
        // chroma the user doesn't own must inject its own .fantome here — else
        // this path went mods-only and silently dropped the chroma.
        let effective_owned = effective_skin_id != 0 && owned_skin_ids.contains(&effective_skin_id);
        let base_skin_name = if !effective_owned && ui_skin_id != Some(0) { Some(name.clone()) } else { None };
        let chroma_for_inject = if effective_owned { None } else { inject_chroma_id };
        let dummy = CustomModSelection {
            skin_id: ui_skin_id.unwrap_or(0),
            champion_id: champ_id.unwrap_or(0),
            mod_name: name.to_uppercase(),
            mod_path: String::new(),
            relative_path: String::new(),
        };
        run_custom_mod_injection(&app, &skins, &injection, dummy, &category_mods, base_skin_name, chroma_for_inject, None, None, champion_name.clone(), &party_mgr).await;
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
    let client = lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S);

    // Stage party-member skins now (cleans the mods dir first so they survive)
    // and fold them into the overlay for both the owned and unowned paths.
    let mut party_folders = stage_party_mods(&party_mgr).await;
    // Fix the loading-screen name for UNOWNED skins (force-loaded as base SkinID 0);
    // owned skins run as their real SkinID so Riot's label already shows the name.
    // `stage_party_mods` only cleaned the mods dir when it had peers, so pass whether
    // that clean already happened.
    let unowned = effective_skin_id != 0 && !owned_skin_ids.contains(&effective_skin_id);
    if let Some(card) = stage_loadscreen_card(&app, &skins, ui_skin_id, champ_id, unowned, !party_folders.is_empty()).await {
        party_folders.push(card);
    }

    // Force owned skins/chromas via LCU (still runs injection afterward so the
    // overlay is built with any party/category mods). A chroma must be owned in
    // its OWN right — owning the base skin does not grant its chromas, so an
    // unowned chroma falls through to `inject_unowned_skin` (injects its .fantome).
    let base_owned = owned_skin_ids.contains(&effective_skin_id);
    if base_owned {
        // P0-A: re-check immediately before the LCU PATCH (phase/queue can
        // have changed since the entry gate). Denied -> abort the whole
        // trigger; the overlay build would be denied by its own gate anyway.
        if policy_denied(&app, InjectionOp::LcuPatch).is_some() {
            injection.resume_if_suspended();
            return;
        }
        force_owned_skin(&client, &auth, local_cell_id, effective_skin_id, champ_id, random_mode_active, &injection).await;
        spawn_owned_injection(app.clone(), skins.clone(), injection.clone(), name.clone(), champion_name.clone(), champ_id, party_folders);
        return;
    }

    // Inject if the user doesn't own the hovered skin.
    inject_unowned_skin(app, skins, client, auth, injection, name, inject_chroma_id, champion_name, champ_id, local_cell_id, random_mode_active, party_folders).await;
}

/// Clean the mods dir and (re)stage every connected party peer's skin into
/// it, returning their folder names. Staged AFTER the clean so they aren't
/// wiped; the owned/unowned paths pass the folders as `extra_mod_names` so
/// the injector skips its own clean. Returns empty (no clean) when party
/// mode is off or has no peers.
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

/// Bake `skin_id`'s name onto its loadscreen card and stage it as an overlay
/// mod, returning the mod folder name to fold into the overlay. Gated by
/// `skins.loadscreen_labels`; best-effort — any miss (feature off, no LCU data,
/// network down) returns `None` and the skin just injects unlabeled.
///
/// The card must land in an already-cleaned mods dir. `mods_dir_clean` tells the
/// helper the caller has NOT cleaned yet (no party staging happened), so it
/// cleans before writing — otherwise the primary-skin extract that follows would
/// see non-empty extras, skip its own clean, and leak stale mods.
/// Repoint the champion's name to the skin name in the localized string table so
/// the game's SkinID-0 loading-screen label reads the skin name. Only meaningful
/// for UNOWNED skins — those are force-loaded as base (SkinID 0); owned skins run
/// as their real SkinID and Riot's label already shows the skin name, so the
/// caller passes `unowned=false` and we skip. `mods_dir_clean` follows the same
/// clean-ordering contract as the party mods (clean before staging when nothing
/// else did).
async fn stage_loadscreen_card(
    app: &AppHandle,
    skins: &Arc<SkinsState>,
    ui_skin_id: Option<i64>,
    champ_id: Option<i64>,
    unowned: bool,
    mods_dir_clean: bool,
) -> Option<String> {
    let fresh_hovered = skins.shared.lock_safe().last_hovered_skin_id;
    log_info!(
        "[LOADSCREEN] stage: snapshot_skin={ui_skin_id:?} fresh_hovered={fresh_hovered:?} champ={champ_id:?} unowned={unowned} mods_dir_clean={mods_dir_clean}"
    );
    if !unowned {
        log_info!("[LOADSCREEN] skip: owned skin - Riot's label already shows the skin name");
        return None;
    }
    let Some(skin_id) = ui_skin_id.filter(|&id| id != 0) else {
        log_info!("[LOADSCREEN] skip: no non-zero skin id (snapshot_skin={ui_skin_id:?})");
        return None;
    };
    let Some(champ_id) = champ_id else {
        log_info!("[LOADSCREEN] skip: no champ id");
        return None;
    };
    let enabled = { app.state::<Arc<AppState>>().config.lock_safe().skins.loadscreen_labels };
    if !enabled {
        log_info!("[LOADSCREEN] skip: feature disabled");
        return None;
    }
    let Some(auth) = lcu::cached_auth() else {
        log_warn!("[LOADSCREEN] skip: no cached LCU auth");
        return None;
    };
    let lcu_client = lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S);
    let Some((champ_display, skin_name)) = lcu_ext::loadscreen_target(&lcu_client, &auth, champ_id, skin_id).await else {
        log_warn!("[LOADSCREEN] skip: LCU had no skin name for champ {champ_id} skin {skin_id}");
        return None;
    };
    log_info!("[LOADSCREEN] resolved skin {skin_id} -> '{skin_name}' (champ '{champ_display}')");

    // Committed to writing the mod — ensure the mods dir is clean first.
    if !mods_dir_clean {
        storage::clean_mods_dir(&paths::injection_mods_dir());
    }
    let r = crate::skins::features::loadscreen_label::build(&champ_display, &skin_name);
    log_info!("[LOADSCREEN] build result for '{skin_name}': {r:?}");
    r
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
    None
}

fn relative_path_of(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Auto-selects historic mods for map/font/announcer + the six "other"
/// category buckets (`features::historic::MOD_HISTORIC_CATEGORIES`).
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

/// How a selected custom mod should be injected, decided from which skin
/// slots its WAD chunks target and which of those the user owns.
#[derive(Debug, Default)]
struct CustomModRoute {
    /// ZIP token (`"skin_<id>"`) to inject as the primary when every slot the
    /// mod targets is unowned — the LCU refuses to select unowned skins, so
    /// the only way to show the right art is the repo's base-keyed port.
    base_skin_name: Option<String>,
    /// Chroma id for the ZIP resolve when the best unowned slot is a chroma.
    chroma_id: Option<i64>,
    /// Slot to force-select via LCU (owned, or base). None = target unknown.
    force_slot: Option<i64>,
}

async fn plan_custom_mod_route(skins: &Arc<SkinsState>, custom_mod: &CustomModSelection, champ_id: Option<i64>) -> CustomModRoute {
    let Some(cid) = champ_id else { return CustomModRoute::default() };
    let Some(auth) = lcu::cached_auth() else { return CustomModRoute::default() };
    let client = lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S);

    // A mod filed under a real skin folder declares its target; every Library
    // install is filed under base, so those get their true target detected
    // from the WAD chunk table (name-match fallback).
    let detection = if custom_mod.skin_id % 1000 != 0 {
        Some(target_detect::Detection { slots: vec![custom_mod.skin_id], via_name: false })
    } else {
        target_detect::detect_target_skin(Path::new(&custom_mod.mod_path), cid, &client, &auth).await
    };
    let Some(detection) = detection else { return CustomModRoute::default() };

    let (owned, picked_chroma) = {
        let shared = skins.shared.lock_safe();
        (shared.owned_skin_ids.clone(), shared.selected_chroma_id)
    };

    // Base slot covered -> forcing base is correct and always possible.
    if detection.slots.iter().any(|&s| s % 1000 == 0) {
        return CustomModRoute { force_slot: Some(cid * 1000), ..CustomModRoute::default() };
    }
    // Prefer the chroma picked in Chud when the mod covers it, then any owned
    // slot — selecting it makes the game load exactly the bins the mod overrides.
    let owned_slot = detection
        .slots
        .iter()
        .copied()
        .find(|&s| Some(s) == picked_chroma && owned.contains(&s))
        .or_else(|| detection.slots.iter().copied().find(|s| owned.contains(s)));
    if let Some(slot) = owned_slot {
        log_info!("[INJECT] Custom mod targets owned slot {slot}, forcing it");
        return CustomModRoute { force_slot: Some(slot), ..CustomModRoute::default() };
    }

    // Every targeted slot is unowned: no LCU force can make the game load
    // those bins (the client rejects unowned selections). Fall back to the
    // unowned pipeline for the closest official art — the repo ZIP is keyed
    // to base, so the game shows that skin/chroma while on the base slot.
    // A name-only match keeps the mod folder staged too in case its chunks
    // do apply.
    let best = picked_chroma.filter(|c| detection.slots.contains(c)).unwrap_or(detection.slots[0]);
    let (parent_skin, chroma) = classify_slot(best, lcu_ext::scrape_champion_skins(&client, &auth, cid).await.as_ref());
    log_warn!(
        "[INJECT] Custom mod targets only unowned slots {:?} (via_name={}) - injecting official ZIP for skin {parent_skin} chroma {chroma:?} + mod, forcing base",
        detection.slots,
        detection.via_name,
    );
    CustomModRoute {
        base_skin_name: Some(format!("skin_{parent_skin}")),
        chroma_id: chroma,
        force_slot: Some(cid * 1000),
    }
}

/// Resolve a detected slot to (parent skin, chroma) using the LCU catalog —
/// a chroma slot needs its parent's ZIP resolved with the chroma id.
fn classify_slot(slot: i64, cache: Option<&lcu_ext::ChampionSkinCache>) -> (i64, Option<i64>) {
    if let Some(cache) = cache {
        if cache.skin_id_map.contains_key(&slot) {
            return (slot, None);
        }
        if let Some(parent) = cache.skins.iter().find(|s| s.chroma_details.iter().any(|c| c.id == slot)) {
            return (parent.skin_id, Some(slot));
        }
    }
    // No catalog reachable: treat the slot as a skin and never invent a
    // chroma pairing.
    (slot, None)
}

/// Ported from `InjectionTrigger._inject_custom_mod` — see this module's
/// doc comment for the `clean_mods_dir`/`extra_mod_names` gap this inherits.
#[allow(clippy::too_many_arguments)]
async fn run_custom_mod_injection(
    app: &AppHandle,
    skins: &Arc<SkinsState>,
    injection: &Arc<InjectionManager>,
    custom_mod: CustomModSelection,
    category_mods: &CategoryModSelections,
    base_skin_name: Option<String>,
    chroma_id: Option<i64>,
    // The skin the user has selected in champ select — the fallback signal
    // for which slot to load when target detection came up empty.
    user_skin_id: Option<i64>,
    // Slot `plan_custom_mod_route` decided to force (owned target or base);
    // None on the category-dummy path and when the target is unknown.
    force_slot: Option<i64>,
    champion_name: String,
    party_mgr: &Option<Arc<crate::skins::party::manager::PartyManager>>,
) {
    let mods_dir = paths::injection_mods_dir();
    let cache_dir = paths::injection_extract_cache_dir();
    // Clean BEFORE any extraction so the mods dir ends up with the UNION of
    // everything this overlay needs — `SkinInjector::inject_skin` skips its
    // own clean once it sees `extra_names` is non-empty (CLEAN ORDERING
    // CONTRACT), so this is the one clean for this whole overlay.
    storage::clean_mods_dir(&mods_dir);
    let mut extra_names = Vec::new();
    let mut labels = Vec::new();
    let mut has_custom_skin_folder = !custom_mod.relative_path.is_empty() && !custom_mod.mod_path.is_empty();

    // SAFETY GUARD: refuse to inject a mod that overrides the champion's ROOT
    // character/ability record — it swaps the game's live ability data for the
    // mod's (usually stale) copy and breaks the champion in-game: missing/
    // unusable abilities, can't level, needs a full client repair. Skip staging
    // it, flag it BROKEN (persisted), and tell the user IN THE OVERLAY so a
    // blocked skin reads as "this skin is bad", not "Chud broke my game".
    if has_custom_skin_folder {
        if let Some(bad_path) =
            target_detect::overrides_ability_data(Path::new(&custom_mod.mod_path), custom_mod.champion_id)
        {
            log_warn!("[SAFETY] Blocked custom mod '{}' — overrides {bad_path} (champion ability data)", custom_mod.mod_name);
            broken_mods::flag(&custom_mod.relative_path, &custom_mod.mod_name, custom_mod.champion_id, &bad_path);
            notify_skin_blocked(app, &custom_mod.mod_name, &champion_name);
            has_custom_skin_folder = false;
        }
    }

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

    // Loadscreen name card for an OFFICIAL skin shown alongside category mods
    // (map/font/announcer). Skipped when a custom skin folder is present — that
    // art isn't an official skin, so no CommunityDragon loadscreen matches it.
    // The mods dir was already cleaned at the top of this function.
    if !has_custom_skin_folder {
        // `user_skin_id` is None on the category-mods path (the has_other_mods
        // dummy selection passes None), but the dummy still carries the real
        // picked skin in `custom_mod.skin_id` — prefer it. Only an UNOWNED skin
        // (base_skin_name present = base-forced) needs the name fix; the mods dir
        // was already cleaned at the top of this function.
        let card_skin_id = Some(custom_mod.skin_id).filter(|&id| id != 0).or(user_skin_id);
        if let Some(card) = stage_loadscreen_card(app, skins, card_skin_id, champion_id, base_skin_name.is_some(), true).await {
            extra_names.push(card);
            labels.push("Loadscreen name".to_string());
        }
    }

    // Force the champ-select selection onto the slot the overlay's WAD chunks
    // are keyed to. Runs for the unowned base-ZIP path (`base_skin_name`) AND
    // for every real custom SKIN mod — including owned targets, so a mod built
    // over an owned skin (e.g. a Soul Fighter Viego VFX edit) auto-selects that
    // skin instead of leaving the user to flip the carousel by hand. A
    // category-mods-only `dummy` selection (map/font/announcer) with no ZIP
    // never forces anything.
    if base_skin_name.is_some() || has_custom_skin_folder {
        if let (Some(cid), Some(auth)) = (champion_id, lcu::cached_auth()) {
            // P0-A: gate the LCU PATCH; denied -> abort this injection
            // entirely (never patch, never build).
            if policy_denied(app, InjectionOp::LcuPatch).is_some() {
                injection.resume_if_suspended();
                return;
            }
            let client = lcu::build_lcu_client(lcu_ext::LCU_API_TIMEOUT_S);
            let (local_cell, random_active) = {
                let shared = skins.shared.lock_safe();
                (shared.local_cell_id, shared.random_mode_active)
            };
            // Resolve the target slot. An unowned-skin ZIP is keyed to the
            // base slot, so that path always forces base. Otherwise use the
            // slot the route planner picked from the mod's detected targets,
            // falling back to the user's own pick.
            let force_skin_id = if base_skin_name.is_some() {
                Some(cid * 1000)
            } else {
                force_slot.or_else(|| user_skin_id.filter(|&id| id % 1000 != 0))
            };
            match force_skin_id {
                Some(id) => force_base_skin(&client, &auth, local_cell, id, random_active).await,
                None => {
                    // Unknown target (base-filed mod, nothing detected, no pick
                    // in Chud): a live non-base selection is the user telling us
                    // which skin the mod goes over — leave it alone instead of
                    // stomping it back to base.
                    match verify_skin_applied(&client, &auth, local_cell, cid * 1000).await {
                        Some(sel) if sel % 1000 != 0 => {
                            log_info!("[INJECT] Custom mod target unknown - keeping user's live selection (skinId={sel})");
                        }
                        _ => force_base_skin(&client, &auth, local_cell, cid * 1000, random_active).await,
                    }
                }
            }
        }
    }
    log_info!("[INJECT] Injecting mods: {}", labels.join(", "));

    spawn_game_end_watcher(skins.clone(), injection.clone());

    let injection = injection.clone();
    let app = app.clone();
    let ticker_champion_name = champion_name;
    tauri::async_runtime::spawn_blocking(move || {
        // With a base skin -> normal inject (resolves + extracts primary,
        // folds in `extra_names`). Without one -> mods-only overlay path:
        // routing pure extras through a `skin_0` placeholder would trip the
        // base-skin short-circuit and silently drop every extra mod.
        let ok = match &base_skin_name {
            Some(primary) => injection.inject_skin_immediately(primary, chroma_id, Some(&ticker_champion_name), champion_id, &extra_names),
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
            notify_injection_failed(&app, &injection, &ticker_champion_name);
        }
    });
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
    spawn_game_end_watcher(skins, injection.clone());
    tauri::async_runtime::spawn_blocking(move || {
        // Your own owned skin shows natively; `party_folders` are the peer
        // skins folded into the overlay so party members see each other's.
        let ok = injection.inject_skin_immediately(&name, None, Some(&champion_name), champion_id, &party_folders);
        if ok {
            log_info!("[INJECT] Owned-skin overlay build completed");
        } else {
            log_warn!("[INJECT] Owned-skin overlay build failed or was skipped");
            notify_injection_failed(&app, &injection, &champion_name);
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
    name: String,
    chroma_id: Option<i64>,
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
            // P0-A: gate the LCU PATCH; denied -> abort this injection
            // entirely (never patch, never build).
            if policy_denied(&app, InjectionOp::LcuPatch).is_some() {
                injection.resume_if_suspended();
                return;
            }
            force_base_skin(&client, &auth, local_cell_id, base_skin_id, random_mode_active).await;
        }
    }

    log_info!("[INJECT] Starting injection: {name}");
    spawn_game_end_watcher(skins.clone(), injection.clone());

    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // `name` is the user's unowned skin (primary); `party_folders` are the
        // connected peers' skins folded in so party members see each other's.
        let success = injection.inject_skin_immediately(&name, chroma_id, Some(&champion_name), champ_id, &party_folders);

        if random_mode_active {
            let mut shared = skins.shared.lock_safe();
            shared.random_skin_name = None;
            shared.random_skin_id = None;
            shared.random_mode_active = false;
            drop(shared);
            log_info!("[RANDOM] Random mode cleared after injection");
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
            notify_injection_failed(&app, &injection, &champion_name);
        }
    });
}

fn parse_injected_id(name: &str) -> Option<i64> {
    name.split_once('_').and_then(|(_, id)| id.parse::<i64>().ok())
}

/// P0-A: evaluate the safety policy for `op`. On denial: log it, push the
/// typed code to the UI (`injection-denied` event — the Skins page shows the
/// backend reason verbatim), and return it so the caller aborts.
/// Surface a REAL injection failure to the user (P0-1). Reads the reason the
/// injector recorded; `None` = success or a benign skip, so nothing is shown.
/// A custom mod was blocked by the ability-data safety guard. Tell the user in
/// BOTH places: the main-window toast (if they have it open) and — the one that
/// matters mid-game — a transient banner in the in-game overlay, so a skin that
/// silently doesn't apply is understood as a bad skin, not a Chud bug (heads off
/// support tickets). `champion` gives the message a concrete subject.
fn notify_skin_blocked(app: &AppHandle, mod_name: &str, champion: &str) {
    let name = if mod_name.trim().is_empty() { "That skin" } else { mod_name };
    let champ = if champion.trim().is_empty() { "the champion" } else { champion };
    let message = format!("{name} modifies {champ}'s abilities and was skipped to protect your game.");
    let _ = app.emit(
        "notification",
        serde_json::json!({ "title": "Broken skin blocked", "message": message.clone(), "tone": "warning" }),
    );
    // Overlay-only payload — overlay.js shows it as a ~7s banner in champ select.
    let _ = app.emit(
        "skin-blocked",
        serde_json::json!({ "mod": name, "champion": champ, "message": message }),
    );
}

fn notify_injection_failed(app: &AppHandle, injection: &InjectionManager, label: &str) {
    if let Some(reason) = injection.take_injection_error() {
        let label = if label.trim().is_empty() { "Your skin" } else { label };
        let _ = app.emit(
            "notification",
            serde_json::json!({ "title": "Skin didn't apply", "message": format!("{label} — {reason}."), "tone": "danger" }),
        );
    }
}

fn policy_denied(app: &AppHandle, op: InjectionOp) -> Option<InjectionDenial> {
    let app_state = app.state::<Arc<AppState>>();
    match safety_manager::evaluate_injection_policy(&app_state, op) {
        InjectionDecision::Allowed(_) => None,
        InjectionDecision::Denied(d) => {
            log_warn!("[SAFETY] {} denied ({}) - {}", op.as_str(), d.code(), d.message());
            let _ = app.emit(
                "injection-denied",
                serde_json::json!({ "op": op.as_str(), "code": d.code(), "message": d.message() }),
            );
            Some(d)
        }
    }
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
pub(crate) async fn force_skin_via_lcu(client: &reqwest::Client, auth: &Auth, my_cell: Option<i64>, target_skin_id: i64) -> bool {
    log_info!("[LCU-FORCE] force_skin_via_lcu target={target_skin_id} cell={my_cell:?}");
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

/// `InjectionTrigger._force_base_skin` (minus the Qt-era UI-hide calls;
/// `broadcast_skip_base_skin` replaces those now).
async fn force_base_skin(
    client: &reqwest::Client,
    auth: &Auth,
    local_cell_id: Option<i64>,
    base_skin_id: i64,
    random_mode_active: bool,
) {
    log_info!("[INJECT] Forcing skin selection (skinId={base_skin_id})");

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

/// Python threaded a `game_ended_callback` into `inject_skin_immediately`
/// so the overlay babysit loop could bail once the game had been InProgress
/// and then ended. `InjectionManager` has no such callback parameter, so
/// this achieves the same effect from OUTSIDE via the public
/// `kill_all_runoverlay_processes` sweep.
fn spawn_game_end_watcher(skins: Arc<SkinsState>, injection: Arc<InjectionManager>) {
    tauri::async_runtime::spawn(async move {
        let mut has_been_in_progress = false;
        let mut has_seen_game = false;
        let mut game_first_seen: Option<std::time::Instant> = None;
        let mut ticks: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            ticks += 1;

            let phase = skins.shared.lock_safe().phase.clone();
            if matches!(phase.as_deref(), Some("InProgress")) {
                has_been_in_progress = true;
            }
            let phase_ended = has_been_in_progress
                && !matches!(phase.as_deref(), Some("InProgress") | Some("Reconnect") | Some("GameStart"));

            // Second, LCU-independent signal: the phase freezes at InProgress if
            // the client closes mid-game, so watch the game process too — this
            // is what stops runoverlay leaking for hours.
            let game_running = injection.game_process_running();
            if game_running {
                has_seen_game = true;
                game_first_seen.get_or_insert_with(std::time::Instant::now);
            }
            let game_exited = has_seen_game && !game_running;

            if phase_ended || game_exited {
                // How long the injected game lived — feeds the fleet break
                // detector (a game that dies seconds after the overlay hooked is
                // the Vanguard-break crash signature; a full-length one is proof
                // injection works). Only report once we actually watched a game
                // run and exit, so a dodge/never-launched path stays silent.
                if game_exited {
                    if let Some(started) = game_first_seen {
                        if let Some(outcome) = crate::advisory::classify_game_duration(started.elapsed().as_secs()) {
                            crate::advisory::report_outcome(outcome);
                        }
                    }
                }
                // OS-enumeration kill, no lock; kill_all_runoverlay_processes
                // would deadlock on the mutex this game's babysit loop holds.
                injection.reset_stuck_injection();
                break;
            }

            // Dodge backstop: neither signal fires if no game ever launched.
            if ticks >= 3600 {
                // Dodge/never-launched backstop: still reap any spawned overlay so
                // a stuck runoverlay doesn't outlive the watcher.
                injection.reset_stuck_injection();
                break;
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
