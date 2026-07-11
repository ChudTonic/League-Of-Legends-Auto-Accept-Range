//! Phase engine actor: the single writer for the skins subsystem's phase
//! state (S2). Ported from `threads/core/phase_thread.py` +
//! `threads/websocket/websocket_event_handler.py` + `threads/handlers/
//! phase_handler.py` + `champion_lock_handler.py` + `game_mode_detector.py` +
//! `lcu_monitor_thread.py`.
//!
//! The Python original had THREE threads racing to write `state.phase`
//! (`PhaseThread`'s HTTP poll, `WSEventThread`'s websocket handler, and
//! `LCUMonitorThread`'s reconnect bookkeeping) with documented races (see
//! `docs/SKINS_PORT.md` "Threading model" and `champ_select_reset.py`'s
//! docstring on the "works once, then stops" bug the dual writers caused).
//! Chud collapses all three into one tokio task that owns `SkinsShared.phase`
//! exclusively: the LCU websocket fan-out (`lcu_ws.rs`) and a slow poll
//! fallback both feed observations into this actor's `mpsc` channel instead
//! of writing state themselves, so there is exactly one phase-change
//! decision point.
//!
//! `PhaseThread`'s null-phase-streak debounce and `LCUMonitorThread`'s
//! LCU-disconnect debounce were two different constants (both `3`) guarding
//! two conceptually different things in Python, but they fired off the same
//! underlying signal (the LCU stopped answering). Here they're unified into
//! one `null_streak` counter: preserves both debounce semantics (3 polls)
//! while actually being the single-writer fix the poll/WS race needed.

#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;
use tokio::sync::{broadcast, mpsc};

use crate::lcu;
use crate::skins::lcu_ext::{self, ChampionSkinCache, SessionData};
use crate::skins::slog::{log_info, log_warn};
use crate::skins::state::SkinsShared;
use crate::skins::SkinsState;
use crate::LockExt;

/// Consecutive null-phase (or LCU-unreachable) polls before treating it as a
/// real disconnect — `PhaseThread._null_phase_streak` /
/// `LCUMonitorThread.LCU_DISCONNECT_DEBOUNCE_POLLS`, both `3` in Python.
const DISCONNECT_DEBOUNCE_POLLS: u32 = 3;
/// Poll-fallback cadence — the WS fan-out covers the fast path; this only
/// fills gaps (missed WS events, cold start before the WS connects).
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Gameflow phase, parsed from the LCU's raw string. LCU quirk ported from
/// `phase_thread.py`: the endpoint sometimes returns the literal string
/// `"None"` (as opposed to no body at all) to mean "no active phase" — both
/// normalize to `Phase::None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    ChampSelect,
    Matchmaking,
    ReadyCheck,
    GameStart,
    InProgress,
    EndOfGame,
    Lobby,
    None,
    Other(String),
}

impl Phase {
    pub fn parse(raw: Option<&str>) -> Phase {
        match raw {
            None | Some("None") => Phase::None,
            Some("ChampSelect") => Phase::ChampSelect,
            Some("Matchmaking") => Phase::Matchmaking,
            Some("ReadyCheck") => Phase::ReadyCheck,
            Some("GameStart") => Phase::GameStart,
            Some("InProgress") => Phase::InProgress,
            Some("EndOfGame") => Phase::EndOfGame,
            Some("Lobby") => Phase::Lobby,
            Some(other) => Phase::Other(other.to_string()),
        }
    }

    /// The raw LCU string for this phase (what `SkinsShared::phase` stores),
    /// or `None` for `Phase::None`.
    pub fn as_raw(&self) -> Option<&str> {
        match self {
            Phase::None => None,
            Phase::ChampSelect => Some("ChampSelect"),
            Phase::Matchmaking => Some("Matchmaking"),
            Phase::ReadyCheck => Some("ReadyCheck"),
            Phase::GameStart => Some("GameStart"),
            Phase::InProgress => Some("InProgress"),
            Phase::EndOfGame => Some("EndOfGame"),
            Phase::Lobby => Some("Lobby"),
            Phase::Other(s) => Some(s.as_str()),
        }
    }
}

/// Observations fed into the phase actor from both the LCU websocket
/// fan-out (`lcu_ws.rs`) and this module's own poll fallback.
#[derive(Debug, Clone)]
pub enum PhaseInput {
    /// Raw gameflow-phase string (`None` = the WS/poll source saw no phase).
    Phase(Option<String>),
    HoveredChampion(Option<i64>),
    Session(SessionData),
}

/// Broadcast to later milestones (bridge S4, ticker S5) subscribing via
/// `PhaseHandle::subscribe`.
#[derive(Debug, Clone)]
pub enum PhaseEvent {
    ChampSelectEntered,
    ChampionLocked { champion_id: i64 },
    PhaseChanged { phase: Option<String>, game_mode: Option<String>, map_id: Option<i64>, queue_id: Option<i64> },
    /// The ticker-start gate itself is S5's job; this just marks the signal.
    Finalization,
    LcuDisconnected,
    LcuReconnected,
}

/// Handle returned by `spawn`: `input_tx` feeds observations in (from
/// `lcu_ws.rs`'s fan-out and this module's own poll fallback caller sites),
/// `events` is subscribed to by later milestones.
pub struct PhaseHandle {
    pub input_tx: mpsc::Sender<PhaseInput>,
    pub events: broadcast::Sender<PhaseEvent>,
}

impl PhaseHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<PhaseEvent> {
        self.events.subscribe()
    }
}

/// Spawn the phase actor. Bumps `skins.phase_gen` so a previously spawned
/// actor (if any) exits on its next loop check instead of racing this one —
/// same duplicate-loop guard `lib.rs`'s tool loops already use.
pub fn spawn(app: AppHandle, skins: Arc<SkinsState>) -> PhaseHandle {
    let generation = skins.phase_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let (input_tx, input_rx) = mpsc::channel(128);
    let (events, _) = broadcast::channel(32);
    let events_for_task = events.clone();
    tauri::async_runtime::spawn(async move {
        run(app, skins, input_rx, events_for_task, generation).await;
    });
    PhaseHandle { input_tx, events }
}

fn now_unix_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

async fn run(
    app: AppHandle,
    skins: Arc<SkinsState>,
    mut input_rx: mpsc::Receiver<PhaseInput>,
    events: broadcast::Sender<PhaseEvent>,
    generation: u64,
) {
    let mut last_phase: Option<String> = None;
    let mut null_streak: u32 = 0;
    let mut disconnected = false;
    let mut last_locked_champion_id: Option<i64> = None;
    let mut scraper_cache = ChampionSkinCache::default();
    let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);

    let mut poll_timer = tokio::time::interval(POLL_INTERVAL);
    poll_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll_timer.tick().await; // first tick fires immediately; consume it so the loop starts on the WS/mpsc path

    loop {
        if skins.phase_gen.load(Ordering::SeqCst) != generation {
            break; // superseded by a newer phase actor
        }

        tokio::select! {
            maybe_input = input_rx.recv() => {
                match maybe_input {
                    Some(input) => {
                        handle_input(
                            &app, &skins, &client, &events, input,
                            &mut last_phase, &mut null_streak, &mut disconnected,
                            &mut last_locked_champion_id, &mut scraper_cache,
                        ).await;
                    }
                    None => break, // sender dropped -> app shutting down
                }
            }
            _ = poll_timer.tick() => {
                let raw = poll_phase(&client).await;
                handle_input(
                    &app, &skins, &client, &events, PhaseInput::Phase(raw),
                    &mut last_phase, &mut null_streak, &mut disconnected,
                    &mut last_locked_champion_id, &mut scraper_cache,
                ).await;
            }
        }
    }
}

async fn poll_phase(client: &reqwest::Client) -> Option<String> {
    let auth = lcu::cached_auth()?;
    lcu::get_phase(client, &auth).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_input(
    app: &AppHandle,
    skins: &Arc<SkinsState>,
    client: &reqwest::Client,
    events: &broadcast::Sender<PhaseEvent>,
    input: PhaseInput,
    last_phase: &mut Option<String>,
    null_streak: &mut u32,
    disconnected: &mut bool,
    last_locked_champion_id: &mut Option<i64>,
    scraper_cache: &mut ChampionSkinCache,
) {
    match input {
        PhaseInput::Phase(raw) => {
            process_phase_observation(app, skins, client, events, raw, last_phase, null_streak, disconnected).await;
        }
        PhaseInput::HoveredChampion(cid) => {
            let mut shared = skins.shared.lock_safe();
            if cid.is_some() && cid != shared.hovered_champ_id {
                shared.hovered_champ_id = cid;
            }
        }
        PhaseInput::Session(session) => {
            process_session(skins, client, events, session, last_locked_champion_id, scraper_cache).await;
        }
    }
}

/// De-duplicates same-phase observations from both the WS fan-out and the
/// poll fallback (`ph != last_phase`) and drives the disconnect debounce.
#[allow(clippy::too_many_arguments)]
async fn process_phase_observation(
    app: &AppHandle,
    skins: &Arc<SkinsState>,
    client: &reqwest::Client,
    events: &broadcast::Sender<PhaseEvent>,
    raw: Option<String>,
    last_phase: &mut Option<String>,
    null_streak: &mut u32,
    disconnected: &mut bool,
) {
    let phase = Phase::parse(raw.as_deref());

    if phase == Phase::None {
        *null_streak += 1;
        if *null_streak == DISCONNECT_DEBOUNCE_POLLS && last_phase.is_some() {
            log_warn!("[phase] LCU unreachable for {DISCONNECT_DEBOUNCE_POLLS} polls - resetting skins state");
            {
                let mut shared = skins.shared.lock_safe();
                shared.reset_on_lcu_disconnect();
            }
            *last_phase = None;
            *disconnected = true;
            let _ = events.send(PhaseEvent::LcuDisconnected);
        } else if *null_streak >= DISCONNECT_DEBOUNCE_POLLS {
            // Already reset above on the first crossing; nothing left to clear
            // for swiftplay-mode state beyond that reset (owned by swiftplay.rs
            // in S5 — the fuller `cleanup_swiftplay_exit` orchestration is
            // deferred there, this actor only owns `SkinsShared` fields).
            let mut shared = skins.shared.lock_safe();
            if !shared.is_swiftplay_mode && !shared.swiftplay_extracted_mods.is_empty() {
                shared.swiftplay_extracted_mods.clear();
            }
        }
        return;
    }

    *null_streak = 0;
    if *disconnected {
        *disconnected = false;
        let _ = events.send(PhaseEvent::LcuReconnected);
    }

    let raw_phase = phase.as_raw().map(str::to_string);
    if raw_phase == *last_phase {
        return; // same phase already processed - de-duped
    }

    {
        let mut shared = skins.shared.lock_safe();
        shared.note_phase_for_champ_select_guard(raw_phase.as_deref());
        shared.phase = raw_phase.clone();
    }

    match &phase {
        Phase::ChampSelect => champ_select_entry(app, skins, client, events).await,
        Phase::Other(s) if s == "FINALIZATION" => {
            log_info!("[phase] Entering FINALIZATION");
            let _ = events.send(PhaseEvent::Finalization);

            // S5: start the loadout ticker. `ticker::TimerManager::maybe_start_timer`
            // needs the raw champ-select session JSON (its `timer` sub-object
            // isn't modeled on `lcu_ext::SessionData` — out of this
            // milestone's file scope to add), so it's fetched here rather
            // than inside `ticker.rs`.
            if let Some(auth) = lcu::cached_auth() {
                if let Some(session) =
                    lcu_ext::shared_cache().get(client, &auth, "/lol-champ-select/v1/session", lcu_ext::DEFAULT_CACHE_TTL).await
                {
                    crate::skins::ticker::TimerManager::maybe_start_timer(app.clone(), Arc::clone(skins), &session).await;
                }
            }
        }
        Phase::InProgress => {
            log_info!("[phase] InProgress");
            // Last resort: if both the loadout ticker (FINALIZATION) and
            // GameStart were missed (a mode that jumps straight here), still
            // attempt injection. The `last_hover_written` guard inside makes
            // this a no-op when we already fired for this game.
            let is_swiftplay = { skins.shared.lock_safe().is_swiftplay_mode };
            if !is_swiftplay {
                crate::skins::ticker::inject_for_game(app, skins, client).await;
            }
        }
        Phase::Matchmaking => {
            phase_exit_reset(skins);
            crate::skins::swiftplay::on_matchmaking_started(app.clone(), Arc::clone(skins)).await;
        }
        Phase::GameStart => {
            let is_swiftplay = { skins.shared.lock_safe().is_swiftplay_mode };
            if is_swiftplay {
                phase_exit_reset(skins);
                crate::skins::swiftplay::on_game_start(app.clone(), Arc::clone(skins)).await;
            } else {
                // Normal / Practice Tool / any non-swiftplay mode: the loadout
                // ticker only arms on the champ-select FINALIZATION timer, which
                // Practice Tool (and some other modes) never reach. Inject as the
                // game launches — the game monitor suspends League on spawn so
                // the overlay builds before champion assets load. Inject BEFORE
                // the exit reset so the locked selection is still readable.
                crate::skins::ticker::inject_for_game(app, skins, client).await;
                phase_exit_reset(skins);
            }
        }
        Phase::Lobby => {
            phase_exit_reset(skins);
            crate::skins::swiftplay::on_lobby_entered(app.clone(), Arc::clone(skins)).await;
        }
        _ => phase_exit_reset(skins),
    }

    let (game_mode, map_id, queue_id) = {
        let shared = skins.shared.lock_safe();
        (shared.current_game_mode.clone(), shared.current_map_id, shared.current_queue_id)
    };
    let _ = events.send(PhaseEvent::PhaseChanged { phase: raw_phase.clone(), game_mode, map_id, queue_id });
    *last_phase = raw_phase;
}

/// Non-ChampSelect/FINALIZATION/InProgress phase exit — ported from
/// `websocket_event_handler.py::_handle_phase_exit` /
/// `phase_handler.py::_reset_state`.
fn phase_exit_reset(skins: &Arc<SkinsState>) {
    let mut shared = skins.shared.lock_safe();
    shared.hovered_champ_id = None;
    shared.players_visible = 0;
    shared.locks_by_cell.clear();
    shared.all_locked_announced = false;
    shared.loadout_countdown_active = false;
}

/// ChampSelect entry: the idempotent per-game reset (guarded by
/// `champ_select_reset_done`), game-mode detection, and the owned-skins
/// reload — consolidates `champ_select_reset.py::perform_champ_select_reset`
/// + `game_mode_detector.py::detect_game_mode`, called from exactly one
/// place regardless of which source (WS or poll) observed the transition.
///
/// S5 fix (`docs/SKINS_PORT.md`'s open reconciliation item "Swiftplay skips
/// champ-select reset not honored"): Swiftplay locks the player's champion in
/// the LOBBY, before ChampSelect even starts. Running the normal per-game
/// reset here (which clears `locked_champ_id`/`own_champion_locked`/etc.)
/// would wipe that lock every time. Ported from
/// `websocket_event_handler.py`'s `ph == "ChampSelect"` branch, which
/// detects game mode FIRST specifically so it can decide whether to run the
/// normal reset or the Swiftplay branch instead.
async fn champ_select_entry(app: &AppHandle, skins: &Arc<SkinsState>, client: &reqwest::Client, events: &broadcast::Sender<PhaseEvent>) {
    let mode = match lcu::cached_auth() {
        Some(auth) => Some(lcu_ext::detect_game_mode(client, &auth).await),
        None => None,
    };
    let is_swiftplay = mode.as_ref().is_some_and(|m| m.is_swiftplay);

    if is_swiftplay {
        {
            let mut shared = skins.shared.lock_safe();
            if let Some(m) = &mode {
                shared.current_game_mode = m.game_mode.clone();
                shared.current_map_id = m.map_id;
                shared.current_queue_id = m.queue_id;
            }
            shared.is_swiftplay_mode = true;
        }
        log_info!("[phase] ChampSelect in Swiftplay mode - skipping normal per-game reset");
        crate::skins::swiftplay::on_champ_select_in_swiftplay(app.clone(), Arc::clone(skins));
        let _ = events.send(PhaseEvent::ChampSelectEntered);
        return;
    }

    let did_reset = {
        let mut shared = skins.shared.lock_safe();
        shared.reset_for_champ_select()
    };
    if !did_reset {
        return;
    }

    log_info!("[phase] Entering ChampSelect - resetting state for new game");

    if let Some(auth) = lcu::cached_auth() {
        if let Some(ids) = lcu_ext::owned_skin_ids(client, &auth).await {
            let mut shared = skins.shared.lock_safe();
            log_info!("[phase] Loaded {} owned skins from inventory", ids.len());
            shared.owned_skin_ids = ids;
        } else {
            log_warn!("[phase] Failed to load owned skins from LCU inventory");
        }
    }
    if let Some(m) = mode {
        let mut shared = skins.shared.lock_safe();
        shared.current_game_mode = m.game_mode;
        shared.current_map_id = m.map_id;
        shared.current_queue_id = m.queue_id;
        shared.is_swiftplay_mode = m.is_swiftplay;
    }

    let _ = events.send(PhaseEvent::ChampSelectEntered);
}

/// Champion lock/exchange detection + the consolidated "on champion locked"
/// pipeline trigger, ported from `champion_lock_handler.py::
/// handle_session_locks` + `on_own_champion_locked` + `handle_champion_exchange`
/// (the Python original had these near-duplicated across the WS handler and the late-lock
/// bootstrap in `lcu_monitor_thread.py`; this is the one place it happens).
async fn process_session(
    skins: &Arc<SkinsState>,
    client: &reqwest::Client,
    events: &broadcast::Sender<PhaseEvent>,
    session: SessionData,
    last_locked_champion_id: &mut Option<i64>,
    scraper_cache: &mut ChampionSkinCache,
) {
    let new_locks = lcu_ext::compute_locked(&session);
    let mut lock_outcome: Option<i64> = None;

    {
        let mut shared = skins.shared.lock_safe();

        if shared.reset_last_locked {
            *last_locked_champion_id = None;
            shared.reset_last_locked = false;
        }

        if let Some(cell_id) = session.local_player_cell_id {
            shared.local_cell_id = Some(cell_id);
        }

        // Track the selected skin ID from myTeam (skin-confirm callback into
        // the base-skin tracker is trigger.rs/S5 territory - deferred).
        if let Some(my_cell) = shared.local_cell_id {
            if let Some(player) =
                session.my_team.iter().flatten().find(|p| p.cell_id == Some(my_cell))
            {
                if let Some(selected) = player.selected_skin_id {
                    shared.selected_skin_id = Some(selected);
                }
            }
        }

        // Visible players (distinct cellIds), falling back to action actors.
        let mut seen: HashSet<i64> = HashSet::new();
        for side in [session.my_team.as_deref(), session.their_team.as_deref()].into_iter().flatten() {
            for p in side {
                if let Some(cid) = p.cell_id {
                    seen.insert(cid);
                }
            }
        }
        if seen.is_empty() {
            for round in session.actions.iter().flatten() {
                for action in round {
                    if let Some(cid) = action.actor_cell_id {
                        seen.insert(cid);
                    }
                }
            }
        }
        if !seen.is_empty() {
            shared.players_visible = seen.len() as i32;
        }

        if let Some(my_cell) = shared.local_cell_id {
            if let Some(&new_champ) = new_locks.get(&my_cell) {
                let is_exchange = last_locked_champion_id.is_some()
                    && *last_locked_champion_id != Some(new_champ)
                    && shared.locked_champ_id.is_some()
                    && shared.locked_champ_id != Some(new_champ);

                if is_exchange {
                    log_info!("[phase] Champion exchange detected: {:?} -> {new_champ}", shared.locked_champ_id);
                    apply_champion_exchange(&mut shared, new_champ);
                    lock_outcome = Some(new_champ);
                } else {
                    let old_champ = shared.locked_champ_id;
                    shared.locked_champ_id = Some(new_champ);
                    shared.locked_champ_timestamp = now_unix_secs();
                    if old_champ.is_some() && old_champ != Some(new_champ) {
                        shared.selected_chroma_id = None;
                    }
                    if apply_own_champion_locked(&mut shared, old_champ, new_champ) {
                        lock_outcome = Some(new_champ);
                    }
                }
                *last_locked_champion_id = Some(new_champ);
            }
        }

        shared.locks_by_cell = new_locks;
        let total = shared.players_visible;
        let locked_count = shared.locks_by_cell.len() as i32;
        if total > 0 && locked_count >= total && !shared.all_locked_announced {
            shared.all_locked_announced = true;
        }
        if locked_count < total {
            shared.all_locked_announced = false;
        }
    }

    if let Some(champion_id) = lock_outcome {
        log_info!("[phase] Own champion locked: {champion_id}");
        if let Some(auth) = lcu::cached_auth() {
            if !scraper_cache.is_loaded_for_champion(champion_id) {
                if let Some(fresh) = lcu_ext::scrape_champion_skins(client, &auth, champion_id).await {
                    *scraper_cache = fresh;
                } else {
                    log_warn!("[phase] Failed to scrape skins for champion {champion_id}");
                }
            }
        }
        let _ = events.send(PhaseEvent::ChampionLocked { champion_id });
    }
}

/// `handle_champion_exchange`: mid-select champion swap resets skin/chroma/
/// injection/historic state and re-arms the lock as the new champion.
fn apply_champion_exchange(shared: &mut SkinsShared, new_champion_id: i64) {
    shared.last_hovered_skin_key = None;
    shared.last_hovered_skin_id = None;
    shared.last_hovered_skin_slug = None;
    shared.selected_chroma_id = None;
    shared.injection_completed = false;
    shared.last_hover_written = false;
    shared.locked_champ_id = Some(new_champion_id);
    shared.locked_champ_timestamp = now_unix_secs();
    shared.own_champion_locked = true;
    shared.historic_mode_active = false;
    shared.historic_skin_id = None;
    shared.historic_first_detection_done = false;
    shared.champion_exchange_triggered = true;
}

/// `on_own_champion_locked`: returns whether the detection/UI pipeline
/// should trigger (first lock this ChampSelect, or a genuine champion
/// change) as opposed to a redundant re-lock of the same champion.
fn apply_own_champion_locked(shared: &mut SkinsShared, old_champion_id: Option<i64>, champion_id: i64) -> bool {
    let should_trigger =
        !shared.own_champion_locked || (old_champion_id.is_some() && old_champion_id != Some(champion_id));
    shared.own_champion_locked = true;
    if should_trigger {
        shared.historic_mode_active = false;
        shared.historic_skin_id = None;
        shared.historic_first_detection_done = false;
    }
    should_trigger
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_parse_normalizes_none_variants() {
        assert_eq!(Phase::parse(None), Phase::None);
        assert_eq!(Phase::parse(Some("None")), Phase::None);
        assert_eq!(Phase::parse(Some("ChampSelect")), Phase::ChampSelect);
        assert_eq!(Phase::parse(Some("FINALIZATION")), Phase::Other("FINALIZATION".to_string()));
    }

    #[test]
    fn phase_as_raw_round_trips() {
        for raw in ["ChampSelect", "Matchmaking", "ReadyCheck", "GameStart", "InProgress", "EndOfGame", "Lobby"] {
            assert_eq!(Phase::parse(Some(raw)).as_raw(), Some(raw));
        }
        assert_eq!(Phase::None.as_raw(), None);
    }

    #[test]
    fn own_champion_locked_triggers_on_first_lock_and_champion_change() {
        let mut shared = SkinsShared::default();
        assert!(apply_own_champion_locked(&mut shared, None, 103));
        assert!(shared.own_champion_locked);

        // Re-lock of the same champion should not re-trigger.
        assert!(!apply_own_champion_locked(&mut shared, Some(103), 103));

        // A genuine champion change re-triggers.
        assert!(apply_own_champion_locked(&mut shared, Some(103), 238));
    }
}
