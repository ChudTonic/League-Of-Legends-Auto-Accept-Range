//! Swiftplay/Brawl pipeline — ported from `threads/handlers/swiftplay_handler.py`
//! (`SwiftplayHandler`) and the Swiftplay-specific branches of
//! `threads/handlers/lobby_processor.py` (`LobbyProcessor`).
//!
//! Queue/mode magic values preserved verbatim: queue 480 + `{SWIFTPLAY, BRAWL}`
//! (`lcu_ext::SWIFTPLAY_QUEUE_ID`/`SWIFTPLAY_MODES`, S2). The core idea: since
//! Swiftplay locks the player's champion in the LOBBY (before ChampSelect even
//! starts) and queues near-instantly, injection can't wait for the normal
//! loadout-ticker deadline — skins are extracted during Matchmaking so the
//! overlay build at GameStart is (close to) instant.
//!
//! TOCTOU FIX (per the fix list): Python's `run_swiftplay_overlay` guarded
//! reentrancy with a plain `threading.Lock` while OTHER call sites
//! (`phase_handler.py`'s ChampSelect-Swiftplay branch) separately checked
//! `self._overlay_lock.locked()` to decide whether to skip — a classic
//! check-then-act race (the lock could be acquired between the check and the
//! decision). `run_overlay_if_ready` below uses `tokio::sync::Mutex::try_lock`
//! as the ONE atomic gate; nothing else peeks at its state to make a decision.

#![allow(dead_code)] // consumed by phase.rs wiring

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Manager};
use tokio::sync::Mutex as TokioMutex;

use crate::lcu;
use crate::skins::features::special;
use crate::skins::injection::{storage, zips};
use crate::skins::lcu_ext;
use crate::skins::paths;
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::SkinsState;
use crate::{AppState, LockExt};

/// Per-process Swiftplay bookkeeping that used to live as instance fields on
/// Python's `SwiftplayHandler` (which was itself a long-lived singleton
/// object, so a process-wide static here is the direct equivalent — see
/// `lcu_ext::shared_cache`'s doc comment for the same pattern).
struct SwiftplayRuntime {
    injection_triggered: bool,
    overlay_done: bool,
    /// (champion_id -> skin_id) snapshot at the last successful extraction —
    /// used only for diagnostics/re-arm bookkeeping here (party-mode restore
    /// semantics from `trigger_swiftplay_injection` are S6 territory).
    last_injected_tracking: HashMap<i64, i64>,
    user_changed_since_inject: HashSet<i64>,
}

impl Default for SwiftplayRuntime {
    fn default() -> Self {
        Self { injection_triggered: false, overlay_done: false, last_injected_tracking: HashMap::new(), user_changed_since_inject: HashSet::new() }
    }
}

static RUNTIME: OnceLock<Mutex<SwiftplayRuntime>> = OnceLock::new();
fn runtime() -> &'static Mutex<SwiftplayRuntime> {
    RUNTIME.get_or_init(|| Mutex::new(SwiftplayRuntime::default()))
}

/// Dedicated async lock guarding `run_overlay_if_ready` reentrancy — see this
/// module's doc comment for why it replaces the Python original's
/// check-then-act `.locked()` race.
static OVERLAY_LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
fn overlay_lock() -> &'static TokioMutex<()> {
    OVERLAY_LOCK.get_or_init(|| TokioMutex::new(()))
}

fn now_unix_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Mark a champion as explicitly browsed by the user since the last
/// extraction (ported from `SwiftplayHandler.mark_champion_changed`) — S6/S9
/// wire this from the bridge's skin-hover handler; exposed here as the seam.
pub fn mark_champion_changed(champion_id: i64) {
    runtime().lock_safe().user_changed_since_inject.insert(champion_id);
}

// ---------------------------------------------------------------------
// Lobby detection — ported from `LobbyProcessor.monitor_lobby_state` +
// `SwiftplayHandler.handle_swiftplay_lobby`/`_process_swiftplay_champion_selection`.
// ---------------------------------------------------------------------

/// Called on a `Lobby` phase observation (ported from the lobby-mode-detection
/// half of `LobbyProcessor.monitor_lobby_state`). Detects Swiftplay/Brawl,
/// records the champion selection already made in the lobby (Swiftplay locks
/// before ChampSelect even starts), and runs exit cleanup on a swiftplay ->
/// non-swiftplay transition.
pub async fn on_lobby_entered(app: AppHandle, skins: Arc<SkinsState>) {
    let _ = &app;
    let Some(auth) = lcu::cached_auth() else { return };
    let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);

    let was_swiftplay = skins.shared.lock_safe().is_swiftplay_mode;
    let mode = lcu_ext::detect_game_mode(&client, &auth).await;

    if mode.is_swiftplay {
        {
            let mut shared = skins.shared.lock_safe();
            shared.current_game_mode = mode.game_mode.clone();
            shared.current_map_id = mode.map_id;
            shared.current_queue_id = mode.queue_id;
            shared.is_swiftplay_mode = true;
        }
        log_info!("[swiftplay] Lobby - Game mode: {:?}, Map ID: {:?}", mode.game_mode, mode.map_id);

        if let Some(sel) = lcu_ext::get_swiftplay_champion_selection(&client, &auth).await {
            apply_champion_selection(&skins, sel);
        }
    } else if was_swiftplay {
        cleanup_swiftplay_exit(&skins);
    }
}

fn apply_champion_selection(skins: &Arc<SkinsState>, sel: lcu_ext::ChampionSelection) {
    if sel.champion_id <= 0 {
        return;
    }
    log_info!("[swiftplay] Champion selected in lobby: {}", sel.champion_id);
    let mut shared = skins.shared.lock_safe();
    shared.locked_champ_id = Some(sel.champion_id);
    shared.locked_champ_timestamp = now_unix_secs();
    shared.own_champion_locked = true;
    if sel.skin_id > 0 {
        shared.selected_skin_id = Some(sel.skin_id);
    }
}

// ---------------------------------------------------------------------
// ChampSelect-in-Swiftplay catch-up — ported from `phase_handler.py`'s
// `phase == "ChampSelect" and self.state.is_swiftplay_mode` branch. Called
// from `phase.rs::champ_select_entry` when it detects Swiftplay BEFORE
// running the normal per-game reset (Swiftplay already locked its champion
// in the lobby; the normal reset would wipe that lock).
// ---------------------------------------------------------------------

pub fn on_champ_select_in_swiftplay(app: AppHandle, skins: Arc<SkinsState>) {
    tauri::async_runtime::spawn(async move {
        let (has_extracted, has_tracking) = {
            let shared = skins.shared.lock_safe();
            (!shared.swiftplay_extracted_mods.is_empty(), !shared.swiftplay_skin_tracking.is_empty())
        };

        if has_extracted {
            log_info!("[swiftplay] ChampSelect in Swiftplay mode - running overlay injection");
            run_overlay_if_ready(app, skins).await;
            return;
        }

        // Best-effort duplicate check only (informational) — the real
        // synchronization boundary is `run_overlay_if_ready`'s `try_lock`.
        let already_handled = runtime().lock_safe().injection_triggered || overlay_lock().try_lock().is_err();
        if already_handled {
            log_info!("[swiftplay] ChampSelect in Swiftplay mode - extraction/overlay already handled, skipping");
        } else if has_tracking {
            log_info!("[swiftplay] ChampSelect in Swiftplay mode - Matchmaking phase missed, triggering late injection");
            extract_tracked_skins(&skins).await;
        } else {
            log_warn!("[swiftplay] ChampSelect in Swiftplay mode - no tracked skins available for injection");
        }
    });
}

// ---------------------------------------------------------------------
// Matchmaking-state early extraction — ported from
// `SwiftplayHandler.monitor_swiftplay_matchmaking` + `trigger_swiftplay_injection`.
// ---------------------------------------------------------------------

/// Called on a `Matchmaking` phase observation. SIMPLIFICATION (flagged for
/// the lead): Python gated extraction on the lobby's matchmaking
/// `searchState` transitioning to `"Searching"` (polled continuously,
/// independent of the gameflow phase); `phase.rs` only observes discrete
/// gameflow-phase transitions, so this fires once per `Matchmaking` phase
/// entry instead. In practice the phase flips to `Matchmaking` at essentially
/// the same moment `searchState` becomes `"Searching"`, so this should be
/// behaviorally equivalent for the common case.
pub async fn on_matchmaking_started(app: AppHandle, skins: Arc<SkinsState>) {
    let _ = &app;
    if !skins.shared.lock_safe().is_swiftplay_mode {
        return;
    }

    {
        let mut rt = runtime().lock_safe();
        if rt.injection_triggered {
            return;
        }
        rt.injection_triggered = true;
    }

    log_info!("[swiftplay] Matchmaking started - extracting tracked skins for instant GameStart injection");
    extract_tracked_skins(&skins).await;
}

/// Ported from `trigger_swiftplay_injection`: extract every tracked
/// champion's skin ZIP into the injection mods directory now, so the overlay
/// build at GameStart only has to run `mkoverlay`/`runoverlay` (no ZIP
/// resolution) — the whole point of doing this during Matchmaking instead of
/// waiting for GameStart.
async fn extract_tracked_skins(skins: &Arc<SkinsState>) {
    let tracking = {
        let shared = skins.shared.lock_safe();
        shared.swiftplay_skin_tracking.clone()
    };
    if tracking.is_empty() {
        log_warn!("[swiftplay] No tracked skins - cannot trigger injection");
        return;
    }

    // Prune champions no longer present in the lobby's player slots (a
    // champion swap after the initial tracking snapshot) — ported from
    // `_get_active_lobby_champion_ids` + the pruning step in
    // `trigger_swiftplay_injection`.
    let active_ids = match lcu::cached_auth() {
        Some(auth) => {
            let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);
            lcu_ext::get_swiftplay_dual_champion_selection(&client, &auth).await.map(|dual| {
                dual.champions.iter().map(|c| c.champion_id).filter(|&id| id > 0).collect::<HashSet<i64>>()
            })
        }
        None => None,
    };

    let filtered: HashMap<i64, i64> = match &active_ids {
        Some(ids) if !ids.is_empty() => {
            let stale: Vec<i64> = tracking.keys().copied().filter(|cid| !ids.contains(cid)).collect();
            if !stale.is_empty() {
                let mut shared = skins.shared.lock_safe();
                for cid in &stale {
                    shared.swiftplay_skin_tracking.remove(cid);
                }
                log_info!("[swiftplay] Pruned {} stale champion(s) from tracking", stale.len());
            }
            tracking.into_iter().filter(|(cid, _)| ids.contains(cid)).collect()
        }
        _ => tracking,
    };

    if filtered.is_empty() {
        log_warn!("[swiftplay] No tracked skins for active champions - cannot trigger injection");
        return;
    }

    let mods_dir = paths::injection_mods_dir();
    storage::clean_mods_dir(&mods_dir);
    storage::clean_overlay_dir(&paths::injection_overlay_dir());

    let mut extracted = Vec::new();
    for (champion_id, skin_id) in &filtered {
        let is_base = special::is_base(*skin_id);
        let (injection_name, chroma_id) = if is_base { (format!("skin_{skin_id}"), None) } else { (format!("chroma_{skin_id}"), Some(*skin_id)) };

        let Some(zip_path) = zips::resolve_zip(&paths::skins_dir(), &injection_name, chroma_id, Some(&injection_name), None, Some(*champion_id)) else {
            log_warn!("[swiftplay] Skin ZIP not found: {injection_name}");
            continue;
        };
        match storage::extract_zip_to_mod(&mods_dir, &zip_path) {
            Ok(folder) => {
                log_info!("[swiftplay] Extracted {injection_name} to mods directory");
                extracted.push(folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            }
            Err(e) => log_warn!("[swiftplay] Error extracting skin {skin_id}: {e}"),
        }
    }

    if extracted.is_empty() {
        log_warn!("[swiftplay] No mods extracted - cannot inject");
        return;
    }

    {
        let mut shared = skins.shared.lock_safe();
        shared.swiftplay_extracted_mods = extracted.clone();
    }
    {
        let mut rt = runtime().lock_safe();
        rt.last_injected_tracking = filtered;
        rt.user_changed_since_inject.clear();
    }
    log_info!("[swiftplay] Extracted {} skin(s) - will inject on GameStart", extracted.len());
}

// ---------------------------------------------------------------------
// Overlay build on GameStart — ported from `SwiftplayHandler.run_swiftplay_overlay`.
// ---------------------------------------------------------------------

pub async fn on_game_start(app: AppHandle, skins: Arc<SkinsState>) {
    if !skins.shared.lock_safe().is_swiftplay_mode {
        return;
    }
    run_overlay_if_ready(app, skins).await;
}

/// See this module's doc comment for the TOCTOU fix this implements.
///
/// DEVIATION (still true, but the data-loss part is fixed — see
/// `injector.rs::inject_skin`'s "CLEAN ORDERING CONTRACT"): `InjectionManager`
/// has no low-level "run an overlay for an arbitrary pre-extracted mod-folder
/// list, no forced primary skin" entry point, so one tracked (champion, skin)
/// pair is re-resolved as `inject_skin_immediately`'s primary `skin_name`
/// (redundant work, but correct) and every other already-extracted folder
/// rides along as `extra_mod_names`. `extract_tracked_skins` already cleans
/// `mods_dir` before staging any of them, and `inject_skin` no longer
/// re-cleans once it sees `extra_mod_names` is non-empty, so a multi-champion
/// Swiftplay lobby's other extracted skins now survive into this overlay.
async fn run_overlay_if_ready(app: AppHandle, skins: Arc<SkinsState>) {
    let Ok(_guard) = overlay_lock().try_lock() else {
        log_info!("[swiftplay] Overlay already running - skipping duplicate call");
        return;
    };
    if runtime().lock_safe().overlay_done {
        log_info!("[swiftplay] Overlay already completed - skipping duplicate call");
        return;
    }

    let extracted_folders = {
        let mut shared = skins.shared.lock_safe();
        if shared.swiftplay_extracted_mods.is_empty() {
            log_info!("[swiftplay] No extracted mods available for overlay injection");
            return;
        }
        std::mem::take(&mut shared.swiftplay_extracted_mods)
    };
    let tracking = runtime().lock_safe().last_injected_tracking.clone();

    let app_state = app.state::<Arc<AppState>>().inner().clone();
    let Some(injection) = app_state.skins_injection.lock_safe().clone() else {
        log_error!("[swiftplay] Injection manager not available");
        return;
    };

    // Resolve the League "Game" directory lazily and set it every overlay
    // build (cheap, and the install dir can change between client launches)
    // — `mkoverlay`'s `--game:<path>` is unset without it, making injection a
    // silent no-op.
    let Some(game_dir) = lcu_ext::resolve_game_dir() else {
        log_warn!("[swiftplay] Could not resolve League game directory (client not running?) - skipping overlay injection");
        return;
    };
    injection.set_game_dir(game_dir);

    log_info!("[swiftplay] Running overlay injection for {} mod(s): {}", extracted_folders.len(), extracted_folders.join(", "));

    let Some((&champion_id, &skin_id)) = tracking.iter().next() else {
        log_warn!("[swiftplay] No tracking data for extracted mods - cannot pick a primary skin");
        return;
    };
    let primary = if special::is_base(skin_id) { format!("skin_{skin_id}") } else { format!("chroma_{skin_id}") };
    let skin_id_str = skin_id.to_string();
    let extras: Vec<String> = extracted_folders.iter().filter(|f| f.as_str() != skin_id_str).cloned().collect();

    let ok = tauri::async_runtime::spawn_blocking(move || injection.inject_skin_immediately(&primary, None, None, Some(champion_id), &extras))
        .await
        .unwrap_or(false);

    if ok {
        log_info!("[swiftplay] Successfully injected {} skin(s) for Swiftplay", extracted_folders.len());
        runtime().lock_safe().overlay_done = true;
    } else {
        log_warn!("[swiftplay] Injection completed with non-zero exit code");
    }
}

// ---------------------------------------------------------------------
// Exit cleanup — ported from `SwiftplayHandler.cleanup_swiftplay_exit`.
// ---------------------------------------------------------------------

/// Clear Swiftplay-specific state when leaving the lobby/mode. NOTE (per the
/// fix list's "preserving the still-in-same-Swiftplay-queue case"): this must
/// only be called on an actual swiftplay -> non-swiftplay transition (see
/// `on_lobby_entered`'s `was_swiftplay && !mode.is_swiftplay` guard) — never
/// on every poll, or a transient lobby-data hiccup would wipe tracking mid-queue.
pub fn cleanup_swiftplay_exit(skins: &Arc<SkinsState>) {
    log_info!("[swiftplay] Clearing Swiftplay skin tracking - leaving Swiftplay mode");
    {
        let mut shared = skins.shared.lock_safe();
        shared.swiftplay_skin_tracking.clear();
        shared.swiftplay_extracted_mods.clear();
        shared.ui_skin_id = None;
        shared.ui_last_text = None;
        shared.last_hovered_skin_id = None;
        shared.last_hovered_skin_key = None;
        shared.own_champion_locked = false;
        shared.locked_champ_id = None;
        shared.locked_champ_timestamp = 0.0;
        shared.is_swiftplay_mode = false;
        shared.current_queue_id = None;
    }
    *runtime().lock_safe() = SwiftplayRuntime::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_lock_try_lock_is_exclusive() {
        // Reset any state a prior test in this process may have left.
        let _first = overlay_lock().try_lock().expect("first lock succeeds");
        assert!(overlay_lock().try_lock().is_err(), "a second concurrent try_lock must fail while the first guard is held");
    }

    #[test]
    fn runtime_default_starts_clean() {
        let rt = SwiftplayRuntime::default();
        assert!(!rt.injection_triggered);
        assert!(!rt.overlay_done);
        assert!(rt.last_injected_tracking.is_empty());
    }
}
