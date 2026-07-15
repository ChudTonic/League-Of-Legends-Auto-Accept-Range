//! Central injection safety gate.
//!
//! Every skin-injection side effect — overlay build, game suspend, LCU patch,
//! and run-overlay — must pass [`evaluate_injection_policy`] immediately before
//! it executes. Evaluated from live backend state only; nothing here trusts the frontend.
//!
//! Fail-closed by design:
//!   * no monitor heartbeat / stale snapshot  -> `IntegrityFailed`
//!   * live game whose queue can't be classified -> `UnknownQueue`
//!   * no policy hook wired into a subsystem  -> that subsystem denies.
//!
//! [`spawn_safety_monitor`] runs from `setup()` for the whole app lifetime, so
//! ranked/queue blocking works even if Auto-Range has never been armed; it
//! maintains the `AppState::injection_blocked` atomic Auto-Range reads each tick.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::AppHandle;

use crate::{emit_state, lcu, safety, AppState, LockExt};

/// Version of the skin-injection risk disclosure the user must acknowledge.
/// Bump whenever the disclosure text changes materially to re-gate everyone
/// behind `ConsentMissing` until they re-accept.
pub const CURRENT_SKINS_ACK_VERSION: u32 = 1;

/// The monitor beats every `safety.check_interval` (>= 1s, default 2.5s). A
/// snapshot older than this means the monitor is dead/wedged, so injection
/// fails closed (`IntegrityFailed`).
pub const SNAPSHOT_STALE_AFTER: Duration = Duration::from_secs(15);

/// Gameflow phases during which injection operations are legitimate at all.
/// Everything else (Lobby, Matchmaking, EndOfGame, None, ...) is `WrongPhase`.
const INJECTION_PHASES: [&str; 4] = ["ChampSelect", "GameStart", "InProgress", "Reconnect"];

// ---------------------------------------------------------------------
// Policy vocabulary.
// ---------------------------------------------------------------------

/// The four injection side effects that must each be gated individually
/// (phase/queue state can change between them within one injection run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionOp {
    /// mkoverlay overlay build.
    Build,
    /// NtSuspendProcess on the launching game.
    Suspend,
    /// LCU PATCH forcing a skin selection.
    LcuPatch,
    /// Starting the runoverlay hook process.
    RunOverlay,
}

impl InjectionOp {
    pub fn as_str(&self) -> &'static str {
        match self {
            InjectionOp::Build => "build",
            InjectionOp::Suspend => "suspend",
            InjectionOp::LcuPatch => "lcu_patch",
            InjectionOp::RunOverlay => "run_overlay",
        }
    }
}

/// What the always-on monitor learned about the live queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueClass {
    /// Queue ranked-ness could not be determined (missing `isRanked` AND
    /// unknown queue id). Fails safe: blocks injection during a live game.
    Unknown,
    Unranked,
    Ranked,
}

/// One observation of the LCU gameflow session, published by the monitor.
#[derive(Debug, Clone)]
pub struct GameflowSnapshot {
    /// False when the client/LCU could not be reached at all.
    pub league_reachable: bool,
    /// Raw gameflow phase string (`ChampSelect`, `InProgress`, ...).
    pub phase: Option<String>,
    pub queue_id: Option<i64>,
    pub queue: QueueClass,
    /// When this snapshot was taken; `None` only before the monitor's first
    /// tick (treated as stale -> `IntegrityFailed`).
    pub updated: Option<Instant>,
}

impl Default for GameflowSnapshot {
    fn default() -> Self {
        Self { league_reachable: false, phase: None, queue_id: None, queue: QueueClass::Unknown, updated: None }
    }
}

/// Live safety state shared between the always-on monitor (writer) and every
/// policy evaluation (readers). Owned by `AppState`.
pub struct SafetyManager {
    snapshot: Mutex<GameflowSnapshot>,
}

impl SafetyManager {
    pub fn new() -> Self {
        Self { snapshot: Mutex::new(GameflowSnapshot::default()) }
    }

    pub fn publish(&self, snap: GameflowSnapshot) {
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = snap;
    }

    pub fn snapshot(&self) -> GameflowSnapshot {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Default for SafetyManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Context handed back on an allowed operation (what the decision was based
/// on, for logging/telemetry).
#[derive(Debug, Clone)]
pub struct InjectionContext {
    pub op: InjectionOp,
    pub phase: Option<String>,
    pub queue_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionDenial {
    /// Skins master switch is off.
    Disabled,
    /// The versioned risk acknowledgement is missing or outdated.
    ConsentMissing,
    /// The League client / LCU is unreachable.
    LeagueUnavailable,
    /// Live game whose queue can't be classified — fail safe.
    UnknownQueue,
    /// Ranked game detected.
    RankedQueue,
    /// The safety monitor itself is dead/stale or the policy hook is
    /// missing — the gates can't be trusted, so nothing runs.
    IntegrityFailed,
    /// cslol mod-tools helper is missing from disk.
    HelperUnavailable,
    /// Gameflow phase is not one where injection is legitimate.
    WrongPhase,
    /// Another injection job is already in flight.
    ActiveJob,
}

impl InjectionDenial {
    /// Stable machine-readable code — the UI shows this verbatim.
    pub fn code(&self) -> &'static str {
        match self {
            InjectionDenial::Disabled => "DISABLED",
            InjectionDenial::ConsentMissing => "CONSENT_MISSING",
            InjectionDenial::LeagueUnavailable => "LEAGUE_UNAVAILABLE",
            InjectionDenial::UnknownQueue => "UNKNOWN_QUEUE",
            InjectionDenial::RankedQueue => "RANKED_QUEUE",
            InjectionDenial::IntegrityFailed => "INTEGRITY_FAILED",
            InjectionDenial::HelperUnavailable => "HELPER_UNAVAILABLE",
            InjectionDenial::WrongPhase => "WRONG_PHASE",
            InjectionDenial::ActiveJob => "ACTIVE_JOB",
        }
    }

    /// Human explanation shown alongside the code.
    pub fn message(&self) -> &'static str {
        match self {
            InjectionDenial::Disabled => "Skins are turned off. Enable Skins to allow injection.",
            InjectionDenial::ConsentMissing => "The skin-injection risk acknowledgement has not been accepted (or was revoked / is outdated).",
            InjectionDenial::LeagueUnavailable => "The League client is not reachable, so the game state can't be verified.",
            InjectionDenial::UnknownQueue => "The current queue type could not be determined - blocking to be safe.",
            InjectionDenial::RankedQueue => "Ranked game detected - injection is disabled to protect your account.",
            InjectionDenial::IntegrityFailed => "The safety monitor is not running or its data is stale - injection fails closed.",
            InjectionDenial::HelperUnavailable => "The cslol mod-tools helper is missing - reinstall or re-download tools.",
            InjectionDenial::WrongPhase => "Not in a game phase where injection is allowed.",
            InjectionDenial::ActiveJob => "Another injection is already in progress.",
        }
    }
}

impl std::fmt::Display for InjectionDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

#[derive(Debug, Clone)]
pub enum InjectionDecision {
    Allowed(InjectionContext),
    Denied(InjectionDenial),
}

impl InjectionDecision {
    pub fn denial(&self) -> Option<InjectionDenial> {
        match self {
            InjectionDecision::Allowed(_) => None,
            InjectionDecision::Denied(d) => Some(*d),
        }
    }

    /// JSON shape the UI consumes (`{allowed, code, message, phase, queueId}`).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            InjectionDecision::Allowed(ctx) => json!({
                "allowed": true, "code": "ALLOWED", "message": "Injection permitted.",
                "phase": ctx.phase, "queueId": ctx.queue_id,
            }),
            InjectionDecision::Denied(d) => json!({
                "allowed": false, "code": d.code(), "message": d.message(),
                "phase": null, "queueId": null,
            }),
        }
    }
}

/// Injected into the injection subsystems (`InjectionManager`, `GameMonitor`,
/// `overlay::mk_run_overlay`) so they can consult the policy without holding
/// a reference to `AppState`. Built by [`make_policy_hook`].
pub type PolicyHook = Arc<dyn Fn(InjectionOp) -> InjectionDecision + Send + Sync>;

// ---------------------------------------------------------------------
// The decision core — pure over `PolicyInputs`, so every denial reason is
// unit-testable without an AppState/Tauri runtime.
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PolicyInputs {
    pub op: InjectionOp,
    pub skins_enabled: bool,
    /// Persisted `config.safety.skins_ack_version` (0 = never acknowledged).
    pub ack_version: u32,
    pub block_in_ranked: bool,
    pub helper_available: bool,
    pub snapshot: GameflowSnapshot,
    pub now: Instant,
    pub injection_in_progress: bool,
}

pub fn decide(i: &PolicyInputs) -> InjectionDecision {
    if !i.skins_enabled {
        return InjectionDecision::Denied(InjectionDenial::Disabled);
    }
    if i.ack_version < CURRENT_SKINS_ACK_VERSION {
        return InjectionDecision::Denied(InjectionDenial::ConsentMissing);
    }
    if !i.helper_available {
        return InjectionDecision::Denied(InjectionDenial::HelperUnavailable);
    }
    // Gate integrity: the monitor must be alive and recent, else fail closed.
    let fresh = i
        .snapshot
        .updated
        .is_some_and(|t| i.now.saturating_duration_since(t) <= SNAPSHOT_STALE_AFTER);
    if !fresh {
        return InjectionDecision::Denied(InjectionDenial::IntegrityFailed);
    }
    if !i.snapshot.league_reachable {
        return InjectionDecision::Denied(InjectionDenial::LeagueUnavailable);
    }
    let phase_ok = i
        .snapshot
        .phase
        .as_deref()
        .is_some_and(|p| INJECTION_PHASES.contains(&p));
    if !phase_ok {
        return InjectionDecision::Denied(InjectionDenial::WrongPhase);
    }
    if i.block_in_ranked {
        match i.snapshot.queue {
            QueueClass::Ranked => return InjectionDecision::Denied(InjectionDenial::RankedQueue),
            QueueClass::Unknown => return InjectionDecision::Denied(InjectionDenial::UnknownQueue),
            QueueClass::Unranked => {}
        }
    }
    // Only the entrypoint op contends for the job slot; inner ops run INSIDE
    // the active job and must not deny themselves.
    if i.op == InjectionOp::Build && i.injection_in_progress {
        return InjectionDecision::Denied(InjectionDenial::ActiveJob);
    }
    InjectionDecision::Allowed(InjectionContext {
        op: i.op,
        phase: i.snapshot.phase.clone(),
        queue_id: i.snapshot.queue_id,
    })
}

// ---------------------------------------------------------------------
// AppState-backed evaluation + hook.
// ---------------------------------------------------------------------

/// Evaluate the policy for one operation from live backend state. Called
/// immediately before every build, suspend, LCU patch, and run-overlay.
///
/// LOCKING NOTE: only takes `state.config`/`state.safety.snapshot` (short leaf
/// locks) — deliberately does NOT touch `state.skins_injection`, since this runs
/// inside the injection pipeline while `InjectionManager::inner` is held, and
/// locking `skins_injection` from there would deadlock. `ActiveJob` for the
/// in-flight case is enforced by `InjectionManager` itself instead.
pub fn evaluate_injection_policy(state: &AppState, op: InjectionOp) -> InjectionDecision {
    let (skins_enabled, ack_version, block_in_ranked) = {
        let c = state.config.lock_safe();
        (c.skins.enabled, c.safety.skins_ack_version, c.safety.block_in_ranked)
    };
    let helper_available =
        crate::skins::injection::tools::tools_present(&crate::skins::injection::tools::cslol_tools_dir());
    decide(&PolicyInputs {
        op,
        skins_enabled,
        ack_version,
        block_in_ranked,
        helper_available,
        snapshot: state.safety.snapshot(),
        now: Instant::now(),
        injection_in_progress: false, // see LOCKING NOTE
    })
}

/// Build the hook the injection subsystems call. Captures `Arc<AppState>`
/// (app-lifetime singleton; the resulting Arc cycle is intentional).
pub fn make_policy_hook(state: Arc<AppState>) -> PolicyHook {
    Arc::new(move |op| evaluate_injection_policy(&state, op))
}

// ---------------------------------------------------------------------
// Always-on gameflow monitor.
// ---------------------------------------------------------------------

/// Spawn the always-running ranked/queue monitor. Runs for the whole app
/// lifetime regardless of Auto-Range/Skins state; publishes a
/// `GameflowSnapshot` every tick and maintains the `injection_blocked`
/// atomic Auto-Range's hold loop reads.
pub fn spawn_safety_monitor(app: AppHandle, state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        let mut client: Option<(u64, reqwest::Client)> = None;
        loop {
            let (timeout, interval, block_in_ranked, cfg_gen) = {
                let c = state.config.lock_safe();
                (
                    c.lcu.request_timeout,
                    c.safety.check_interval,
                    c.safety.block_in_ranked,
                    state.config_gen.load(Ordering::SeqCst),
                )
            };
            // Rebuild the client only when config changed (timeout may differ).
            let http = match &client {
                Some((gen, c)) if *gen == cfg_gen => c.clone(),
                _ => {
                    let c = lcu::build_lcu_client(timeout);
                    client = Some((cfg_gen, c.clone()));
                    c
                }
            };

            let snap = match lcu::cached_auth() {
                Some(auth) => match lcu::gameflow_session(&http, &auth).await {
                    Some(session) => {
                        let phase = session
                            .get("phase")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let queue_id = session
                            .get("gameData")
                            .and_then(|g| g.get("queue"))
                            .and_then(|q| q.get("id"))
                            .and_then(|v| v.as_i64());
                        let queue = match safety::queue_is_ranked(&session) {
                            Some(true) => QueueClass::Ranked,
                            Some(false) => QueueClass::Unranked,
                            None => QueueClass::Unknown,
                        };
                        GameflowSnapshot { league_reachable: true, phase, queue_id, queue, updated: Some(Instant::now()) }
                    }
                    None => {
                        lcu::invalidate_auth();
                        GameflowSnapshot { league_reachable: false, updated: Some(Instant::now()), ..Default::default() }
                    }
                },
                None => GameflowSnapshot { league_reachable: false, updated: Some(Instant::now()), ..Default::default() },
            };

            // Ranked kill-switch for Auto-Range: block only during a live game
            // whose queue is ranked/unknown. Unreachable client never blocks.
            let live = snap
                .phase
                .as_deref()
                .is_some_and(|p| INJECTION_PHASES.contains(&p));
            let block = block_in_ranked
                && snap.league_reachable
                && live
                && matches!(snap.queue, QueueClass::Ranked | QueueClass::Unknown);

            state.safety.publish(snap);
            if state.injection_blocked.swap(block, Ordering::SeqCst) != block {
                emit_state(&app, &state);
            }

            tokio::time::sleep(Duration::from_secs_f64(interval.max(1.0))).await;
        }
    });
}

// ---------------------------------------------------------------------
// Tests — every denial reason + concurrent phase changes.
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_snapshot(phase: &str, queue: QueueClass) -> GameflowSnapshot {
        GameflowSnapshot {
            league_reachable: true,
            phase: Some(phase.to_string()),
            queue_id: Some(430),
            queue,
            updated: Some(Instant::now()),
        }
    }

    fn allowed_inputs() -> PolicyInputs {
        PolicyInputs {
            op: InjectionOp::Build,
            skins_enabled: true,
            ack_version: CURRENT_SKINS_ACK_VERSION,
            block_in_ranked: true,
            helper_available: true,
            snapshot: fresh_snapshot("ChampSelect", QueueClass::Unranked),
            now: Instant::now(),
            injection_in_progress: false,
        }
    }

    fn denial_of(i: &PolicyInputs) -> Option<InjectionDenial> {
        decide(i).denial()
    }

    #[test]
    fn happy_path_is_allowed() {
        assert!(denial_of(&allowed_inputs()).is_none());
    }

    #[test]
    fn disabled_master_switch_denies_every_op() {
        for op in [InjectionOp::Build, InjectionOp::Suspend, InjectionOp::LcuPatch, InjectionOp::RunOverlay] {
            let mut i = allowed_inputs();
            i.op = op;
            i.skins_enabled = false;
            assert_eq!(denial_of(&i), Some(InjectionDenial::Disabled), "op {:?}", op);
        }
    }

    #[test]
    fn missing_or_revoked_or_outdated_consent_denies_every_op() {
        for op in [InjectionOp::Build, InjectionOp::Suspend, InjectionOp::LcuPatch, InjectionOp::RunOverlay] {
            for ack in [0, CURRENT_SKINS_ACK_VERSION - 1] {
                let mut i = allowed_inputs();
                i.op = op;
                i.ack_version = ack;
                assert_eq!(denial_of(&i), Some(InjectionDenial::ConsentMissing), "op {:?} ack {ack}", op);
            }
        }
    }

    #[test]
    fn helper_missing_denies() {
        let mut i = allowed_inputs();
        i.helper_available = false;
        assert_eq!(denial_of(&i), Some(InjectionDenial::HelperUnavailable));
    }

    #[test]
    fn no_snapshot_ever_published_fails_closed_as_integrity() {
        // Regression guard: an absent snapshot must DENY, never allow.
        let mut i = allowed_inputs();
        i.snapshot.updated = None;
        assert_eq!(denial_of(&i), Some(InjectionDenial::IntegrityFailed));
    }

    #[test]
    fn stale_snapshot_fails_closed_as_integrity() {
        let mut i = allowed_inputs();
        i.now = Instant::now() + SNAPSHOT_STALE_AFTER + Duration::from_secs(1);
        assert_eq!(denial_of(&i), Some(InjectionDenial::IntegrityFailed));
    }

    #[test]
    fn league_unreachable_denies() {
        let mut i = allowed_inputs();
        i.snapshot.league_reachable = false;
        assert_eq!(denial_of(&i), Some(InjectionDenial::LeagueUnavailable));
    }

    #[test]
    fn wrong_phase_denies() {
        for phase in ["Lobby", "Matchmaking", "EndOfGame", "None"] {
            let mut i = allowed_inputs();
            i.snapshot.phase = Some(phase.to_string());
            assert_eq!(denial_of(&i), Some(InjectionDenial::WrongPhase), "phase {phase}");
        }
        let mut i = allowed_inputs();
        i.snapshot.phase = None;
        assert_eq!(denial_of(&i), Some(InjectionDenial::WrongPhase));
    }

    #[test]
    fn unknown_queue_blocks_rather_than_allows() {
        let mut i = allowed_inputs();
        i.snapshot.queue = QueueClass::Unknown;
        assert_eq!(denial_of(&i), Some(InjectionDenial::UnknownQueue));
    }

    #[test]
    fn ranked_queue_denies_in_every_live_phase() {
        for phase in ["ChampSelect", "GameStart", "InProgress", "Reconnect"] {
            let mut i = allowed_inputs();
            i.snapshot = fresh_snapshot(phase, QueueClass::Ranked);
            assert_eq!(denial_of(&i), Some(InjectionDenial::RankedQueue), "phase {phase}");
        }
    }

    #[test]
    fn block_in_ranked_off_skips_queue_denials() {
        let mut i = allowed_inputs();
        i.block_in_ranked = false;
        i.snapshot.queue = QueueClass::Ranked;
        assert!(denial_of(&i).is_none());
        i.snapshot.queue = QueueClass::Unknown;
        assert!(denial_of(&i).is_none());
    }

    #[test]
    fn active_job_denies_build_but_not_inner_ops() {
        let mut i = allowed_inputs();
        i.injection_in_progress = true;
        assert_eq!(denial_of(&i), Some(InjectionDenial::ActiveJob));
        for op in [InjectionOp::Suspend, InjectionOp::LcuPatch, InjectionOp::RunOverlay] {
            i.op = op;
            assert!(denial_of(&i).is_none(), "inner op {:?} must not self-deny", op);
        }
    }

    #[test]
    fn denial_precedence_disabled_beats_consent_beats_queue() {
        let mut i = allowed_inputs();
        i.skins_enabled = false;
        i.ack_version = 0;
        i.snapshot.queue = QueueClass::Ranked;
        assert_eq!(denial_of(&i), Some(InjectionDenial::Disabled));
        i.skins_enabled = true;
        assert_eq!(denial_of(&i), Some(InjectionDenial::ConsentMissing));
        i.ack_version = CURRENT_SKINS_ACK_VERSION;
        assert_eq!(denial_of(&i), Some(InjectionDenial::RankedQueue));
    }

    #[test]
    fn every_denial_has_a_stable_code_and_message() {
        let all = [
            InjectionDenial::Disabled,
            InjectionDenial::ConsentMissing,
            InjectionDenial::LeagueUnavailable,
            InjectionDenial::UnknownQueue,
            InjectionDenial::RankedQueue,
            InjectionDenial::IntegrityFailed,
            InjectionDenial::HelperUnavailable,
            InjectionDenial::WrongPhase,
            InjectionDenial::ActiveJob,
        ];
        let mut codes: Vec<&str> = all.iter().map(|d| d.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len(), "codes must be unique");
        for d in all {
            assert!(!d.message().is_empty());
        }
    }

    /// One thread flips the published snapshot between allowed/ranked while
    /// readers evaluate continuously — every decision must be exactly one of
    /// the two legal outcomes, never a torn/other state.
    #[test]
    fn concurrent_phase_changes_never_produce_torn_decisions() {
        let mgr = Arc::new(SafetyManager::new());
        mgr.publish(fresh_snapshot("ChampSelect", QueueClass::Unranked));

        let writer_mgr = Arc::clone(&mgr);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let mut ranked = false;
            while !writer_stop.load(Ordering::SeqCst) {
                ranked = !ranked;
                let queue = if ranked { QueueClass::Ranked } else { QueueClass::Unranked };
                writer_mgr.publish(fresh_snapshot("ChampSelect", queue));
            }
        });

        let mut saw_allowed = false;
        let mut saw_ranked = false;
        for _ in 0..5000 {
            let mut i = allowed_inputs();
            i.snapshot = mgr.snapshot();
            match decide(&i) {
                InjectionDecision::Allowed(_) => saw_allowed = true,
                InjectionDecision::Denied(InjectionDenial::RankedQueue) => saw_ranked = true,
                other => panic!("unexpected decision under concurrent flips: {:?}", other.denial()),
            }
        }
        stop.store(true, Ordering::SeqCst);
        writer.join().unwrap();
        assert!(saw_allowed && saw_ranked, "test should observe both states (allowed={saw_allowed}, ranked={saw_ranked})");
    }
}
