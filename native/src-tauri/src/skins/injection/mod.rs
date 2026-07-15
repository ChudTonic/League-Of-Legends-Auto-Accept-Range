//! Injection subsystem (S3) — cslol mod-tools orchestration. `InjectionManager`
//! is ported from `injection\core\manager.py`, folding in
//! `threshold_manager.py`'s `ThresholdManager` as plain fields/methods rather
//! than a separate collaborator — there's no `shared_state` back-reference to
//! propagate threshold changes here (S4 owns broadcasting that), so the
//! extra class Python needed doesn't pull its weight in Rust.

#![allow(dead_code)] // consumed by S5+ (ticker/trigger wiring)

pub mod game_monitor;
pub mod injector;
pub mod overlay;
pub mod process;
pub mod storage;
pub mod tools;
pub mod zips;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::safety_manager::{InjectionDecision, InjectionDenial, InjectionOp, PolicyHook};
use crate::skins::injection::game_monitor::GameMonitor;
use crate::skins::injection::injector::SkinInjector;
use crate::skins::slog::{log_error, log_info, log_warn};

/// `config.INJECTION_LOCK_TIMEOUT_S` — timeout for acquiring the injection lock.
const INJECTION_LOCK_TIMEOUT: Duration = Duration::from_millis(2000);
/// Default injection threshold; config pushes a real value in via
/// `refresh_injection_threshold`.
const DEFAULT_INJECTION_THRESHOLD_S: f64 = 0.5;

struct Inner {
    injector: Option<SkinInjector>,
    game_monitor: GameMonitor,
    initialized: bool,
    last_skin_name: Option<String>,
    last_injection_time: Option<Instant>,
    injection_threshold: f64,
    current_champion: Option<String>,
}

/// Manages skin injection with automatic triggering (ported from
/// `InjectionManager`). All mutable state lives behind one `Mutex<Inner>` —
/// one coarse lock beats Python's per-object `threading.Lock`s, so its
/// separate `injection_lock`/`_cleanup_lock`/`_cleanup_in_progress` trio
/// collapses onto this one guard. `game_dir` is supplied by the caller via
/// `set_game_dir` rather than self-detected (`game_detector.py` not ported).
pub struct InjectionManager {
    tools_dir: PathBuf,
    mods_dir: PathBuf,
    zips_dir: PathBuf,
    overlay_dir: PathBuf,
    game_dir: Mutex<Option<PathBuf>>,
    /// Fast pre-check ahead of the real lock, checked before the timed acquire.
    injection_in_progress: AtomicBool,
    /// Safety policy hook (P0-A) — consulted immediately before every gated
    /// operation. `None` (never wired) FAILS CLOSED: all injection denied.
    policy: Mutex<Option<PolicyHook>>,
    inner: Mutex<Inner>,
}

impl InjectionManager {
    pub fn new(tools_dir: PathBuf, mods_dir: PathBuf, zips_dir: PathBuf, overlay_dir: PathBuf) -> Self {
        // Startup carcass sweep: a crash/kill mid-build can leave a
        // partially-written overlay (observed at 17 GB) and stale mod
        // junctions with no later injection to clean them. Both dirs are
        // rebuilt from scratch every injection, so clearing here is always
        // safe (a locked file from a still-running previous runoverlay just
        // fails the remove non-fatally).
        storage::clean_mods_dir(&mods_dir);
        storage::clean_overlay_dir(&overlay_dir);
        Self {
            tools_dir,
            mods_dir,
            zips_dir,
            overlay_dir,
            game_dir: Mutex::new(None),
            injection_in_progress: AtomicBool::new(false),
            policy: Mutex::new(None),
            inner: Mutex::new(Inner {
                injector: None,
                game_monitor: GameMonitor::new(),
                initialized: false,
                last_skin_name: None,
                last_injection_time: None,
                injection_threshold: DEFAULT_INJECTION_THRESHOLD_S,
                current_champion: None,
            }),
        }
    }

    /// Set (or update) the detected League game directory. Must be called
    /// before injection can initialize.
    pub fn set_game_dir(&self, game_dir: PathBuf) {
        *self.game_dir.lock().unwrap_or_else(|e| e.into_inner()) = Some(game_dir);
    }

    /// Wire the safety policy hook (P0-A). Called once from `setup()`.
    /// Forwards a clone into the game monitor so the suspend path is gated
    /// too. Until this is called, every injection entrypoint fails closed.
    pub fn set_policy_hook(&self, hook: PolicyHook) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .game_monitor
            .set_policy_hook(hook.clone());
        *self.policy.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    fn policy_hook(&self) -> Option<PolicyHook> {
        self.policy.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Evaluate the safety policy for `op`. `None` hook = fail closed
    /// (`IntegrityFailed`): a build that can't consult the gates never runs.
    fn check_policy(&self, op: InjectionOp) -> Result<(), InjectionDenial> {
        match self.policy_hook() {
            Some(hook) => match hook(op) {
                InjectionDecision::Allowed(_) => Ok(()),
                InjectionDecision::Denied(d) => Err(d),
            },
            None => Err(InjectionDenial::IntegrityFailed),
        }
    }

    /// True while an injection job holds the pipeline (read by the UI's
    /// policy view; the internal fast pre-check uses the atomic directly).
    pub fn injection_in_progress(&self) -> bool {
        self.injection_in_progress.load(Ordering::SeqCst)
    }

    /// Initialize the injector lazily when first needed (ported from
    /// `InjectionManager._ensure_initialized`). Caller must already hold
    /// `self.inner`'s lock.
    fn ensure_initialized(&self, inner: &mut Inner) {
        if inner.initialized {
            return;
        }
        let Some(game_dir) = self.game_dir.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            log_error!("[INJECT] Cannot initialize injection system - League game directory not found");
            log_error!("[INJECT] Please ensure League Client is running or manually set the path in config.ini");
            return;
        };

        log_info!("[INJECT] Initializing injection system...");
        inner.injector = Some(SkinInjector::new(
            self.tools_dir.clone(),
            self.mods_dir.clone(),
            self.zips_dir.clone(),
            self.overlay_dir.clone(),
            game_dir,
            self.policy_hook(),
        ));
        inner.initialized = true;
        log_info!("[INJECT] Injection system initialized successfully");
    }

    /// Reload the injection threshold so config/tray changes apply
    /// immediately. Allows `0.0` as a deliberate "no cooldown" value but
    /// guards against negatives.
    pub fn refresh_injection_threshold(&self, new_threshold: f64) -> f64 {
        let clamped = new_threshold.max(0.0);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if (clamped - inner.injection_threshold).abs() >= 1e-6 {
            inner.injection_threshold = clamped;
            log_info!("[INJECT] Injection threshold reloaded: {clamped:.2}s");
        }
        inner.injection_threshold
    }

    pub fn injection_threshold(&self) -> f64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).injection_threshold
    }

    /// Apply config `monitor_auto_resume_timeout_secs` to the owned
    /// `GameMonitor` (forwards to `set_auto_resume_timeout`'s 1..=180s clamp).
    pub fn set_auto_resume_timeout(&self, secs: f64) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).game_monitor.set_auto_resume_timeout(secs);
    }

    /// Track the currently-locked champion (ported from
    /// `InjectionManager.on_champion_locked`).
    pub fn on_champion_locked(&self, champion_name: &str) {
        if champion_name.is_empty() {
            log_info!("[INJECT] on_champion_locked called with empty champion name");
            return;
        }
        log_info!("[INJECT] on_champion_locked called for: {champion_name}");

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_initialized(&mut inner);
        if inner.current_champion.as_deref() != Some(champion_name) {
            inner.current_champion = Some(champion_name.to_string());
        }
    }

    pub fn current_champion(&self) -> Option<String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).current_champion.clone()
    }

    /// Immediately inject a specific skin, with optional chroma.
    ///
    /// `extra_mod_names` replaces Python's `extra_mods_callback`: party/category
    /// mods the caller has already extracted (see `SkinInjector::inject_skin`).
    pub fn inject_skin_immediately(
        &self,
        skin_name: &str,
        chroma_id: Option<i64>,
        champion_name: Option<&str>,
        champion_id: Option<i64>,
        extra_mod_names: &[String],
    ) -> bool {
        // Base-skin short-circuit (`skin_{id}` where `id == 0` or `champion_id
        // * 1000`): nothing to overlay on your own base skin. BUT a custom
        // mod or party mods in `extra_mod_names` must still inject OVER the
        // base skin — library mods live at `mods/skins/{champ*1000}`, so
        // skipping here silently no-op'd a custom "Separatist Jhin" on base Jhin.
        if extra_mod_names.is_empty() {
            if let Some(skin_id_str) = skin_name.strip_prefix("skin_") {
                if let Ok(skin_id) = skin_id_str.split('_').next().unwrap_or(skin_id_str).parse::<i64>() {
                    if skin_id == 0 {
                        log_info!("[INJECT] Base skin detected (skinId=0), no extra mods - injection skipped");
                        return false;
                    }
                    if let Some(champ) = champion_id {
                        if skin_id == champ * 1000 {
                            log_info!(
                                "[INJECT] Base skin detected (skinId={skin_id} for champion {champ}), no extra mods - injection skipped"
                            );
                            return false;
                        }
                    }
                }
            }
        }

        // Fast pre-check ahead of the real lock. This IS the `ActiveJob`
        // denial — the policy hook itself deliberately doesn't read this flag.
        if self.injection_in_progress.load(Ordering::SeqCst) {
            log_warn!("[SAFETY] Injection denied ({}) for {skin_name} - {}", InjectionDenial::ActiveJob.code(), InjectionDenial::ActiveJob.message());
            return false;
        }

        // P0-A safety gate: evaluated immediately before the build starts.
        if let Err(d) = self.check_policy(InjectionOp::Build) {
            log_warn!("[SAFETY] Injection denied ({}) for {skin_name} - {}", d.code(), d.message());
            return false;
        }

        // `std::sync::Mutex` has no timed acquire, so poll `try_lock` instead
        // of blocking indefinitely — a plain blocking `.lock()` would make a
        // concurrent caller wait out the ENTIRE injection (including the
        // runoverlay babysitting loop, which can span a whole game session).
        let deadline = Instant::now() + INJECTION_LOCK_TIMEOUT;
        let guard_opt = loop {
            match self.inner.try_lock() {
                Ok(guard) => break Some(guard),
                Err(TryLockError::Poisoned(p)) => break Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let Some(mut guard) = guard_opt else {
            log_warn!("[INJECT] Could not acquire injection lock - another injection in progress");
            return false;
        };

        self.injection_in_progress.store(true, Ordering::SeqCst);
        log_info!("[INJECT] Injection started - lock acquired for: {skin_name}");

        let success =
            self.do_inject_locked(&mut guard, skin_name, chroma_id, champion_name, champion_id, extra_mod_names);

        log_info!("[INJECT] Injection completed - lock released");
        // Stop monitor after injection completes (resumes the game if still
        // suspended). Happens while `guard` is still held, which is fine —
        // `GameMonitor::stop` is self-contained and doesn't reach back into `InjectionManager`.
        guard.game_monitor.stop();
        self.injection_in_progress.store(false, Ordering::SeqCst);

        success
    }

    /// Immediate mods-only injection: build the overlay from pre-staged mods
    /// with NO primary skin. Same lock/game-monitor discipline as
    /// `inject_skin_immediately`, minus the base-skin short-circuit. Used for
    /// a party peer who picked no skin but must still see teammates' party
    /// skins — previously this dropped ALL party skins for such a peer.
    pub fn inject_mods_only_immediately(&self, mod_names: &[String]) -> bool {
        if mod_names.is_empty() {
            return false;
        }

        if self.injection_in_progress.load(Ordering::SeqCst) {
            log_warn!("[SAFETY] Mods-only injection denied ({}) - {}", InjectionDenial::ActiveJob.code(), InjectionDenial::ActiveJob.message());
            return false;
        }

        // P0-A safety gate: evaluated immediately before the build starts.
        if let Err(d) = self.check_policy(InjectionOp::Build) {
            log_warn!("[SAFETY] Mods-only injection denied ({}) - {}", d.code(), d.message());
            return false;
        }

        let deadline = Instant::now() + INJECTION_LOCK_TIMEOUT;
        let guard_opt = loop {
            match self.inner.try_lock() {
                Ok(guard) => break Some(guard),
                Err(TryLockError::Poisoned(p)) => break Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let Some(mut guard) = guard_opt else {
            log_warn!("[INJECT] Could not acquire injection lock - another injection in progress");
            return false;
        };

        self.injection_in_progress.store(true, Ordering::SeqCst);
        log_info!("[INJECT] Mods-only injection started - lock acquired ({} mod(s))", mod_names.len());

        let success = self.do_inject_mods_only_locked(&mut guard, mod_names);

        log_info!("[INJECT] Mods-only injection completed - lock released");
        guard.game_monitor.stop();
        self.injection_in_progress.store(false, Ordering::SeqCst);

        success
    }

    /// Locked body of `inject_mods_only_immediately` (mirrors
    /// `do_inject_locked`'s init/threshold/monitor guards).
    fn do_inject_mods_only_locked(&self, inner: &mut Inner, mod_names: &[String]) -> bool {
        self.ensure_initialized(inner);
        if !inner.initialized || inner.injector.is_none() {
            log_error!("[INJECT] Cannot inject mods-only - League game directory not found");
            return false;
        }

        let now = Instant::now();
        if let Some(last) = inner.last_injection_time {
            let elapsed = now.duration_since(last).as_secs_f64();
            if elapsed < inner.injection_threshold {
                let remaining = inner.injection_threshold - elapsed;
                log_info!("[INJECT] Skipping mods-only injection (cooldown {remaining:.2}s remaining)");
                return false;
            }
        }

        if !inner.game_monitor.is_active() {
            log_info!("[INJECT] Starting game monitor for mods-only injection");
            inner.game_monitor.start();
        }

        let Some(injector) = inner.injector.as_ref() else { return false };
        let result = injector.inject_mods_only(&mut inner.game_monitor, mod_names);

        let success = matches!(result, Ok(true));
        if let Err(e) = &result {
            log_error!("[INJECT] inject_mods_only error: {e}");
        }
        if success {
            inner.last_skin_name = Some("<party-mods-only>".to_string());
            inner.last_injection_time = Some(now);
        }
        success
    }

    /// The locked body of `inject_skin_immediately`, factored out so the
    /// caller can hold `guard` across both this call and the following
    /// `stop()` without fighting the borrow checker over a closure capture.
    fn do_inject_locked(
        &self,
        inner: &mut Inner,
        skin_name: &str,
        chroma_id: Option<i64>,
        champion_name: Option<&str>,
        champion_id: Option<i64>,
        extra_mod_names: &[String],
    ) -> bool {
        self.ensure_initialized(inner);
        if !inner.initialized || inner.injector.is_none() {
            log_error!("[INJECT] Cannot inject - League game directory not found");
            log_error!("[INJECT] Please ensure League Client is running or manually set the path in config.ini");
            return false;
        }

        let now = Instant::now();
        if let Some(last) = inner.last_injection_time {
            let elapsed = now.duration_since(last).as_secs_f64();
            if elapsed < inner.injection_threshold {
                let remaining = inner.injection_threshold - elapsed;
                log_info!("[INJECT] Skipping immediate injection for '{skin_name}' (cooldown {remaining:.2}s remaining)");
                return false;
            }
        }

        // Start monitor now (only when injection actually happens).
        if !inner.game_monitor.is_active() {
            log_info!("[INJECT] Starting game monitor for injection");
            inner.game_monitor.start();
        }

        let Some(injector) = inner.injector.as_ref() else { return false };
        let result =
            injector.inject_skin(skin_name, &mut inner.game_monitor, chroma_id, champion_name, champion_id, extra_mod_names);

        let success = matches!(result, Ok(true));
        if let Err(e) = &result {
            log_error!("[INJECT] inject_skin error: {e}");
        }
        if success {
            inner.last_skin_name = Some(skin_name.to_string());
            inner.last_injection_time = Some(now);
        }
        success
    }

    /// Heal a stuck injection state at the start of a new champ select. Fixes
    /// "one leaked overlay blacks out skins for the rest of the session": an
    /// injection holds `self.inner` for the WHOLE game, and `runoverlay`
    /// sometimes never self-exits, so `inner` stays locked and
    /// `injection_in_progress` stays `true` forever (and the leaked
    /// `mod-tools.exe` locks the installer).
    ///
    /// Must NOT lock `self.inner` (that's exactly what's stuck) — instead
    /// kills leaked `runoverlay` processes via OS enumeration and clears
    /// `injection_in_progress` directly; killing the child releases `inner`
    /// on its own. Safe on ChampSelect entry: the previous game is definitely over.
    pub fn reset_stuck_injection(&self) {
        crate::skins::injection::process::kill_runoverlay_processes_os();
        if self.injection_in_progress.swap(false, Ordering::SeqCst) {
            log_warn!("[INJECT] Cleared a stuck injection lock from a previous game (leaked overlay)");
        }
    }

    /// Clean the injection system (ported from `InjectionManager.clean_system`).
    pub fn clean_system(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.injector.as_ref() {
            Some(injector) => injector.clean_system(),
            None => true, // nothing to clean if not initialized
        }
    }

    /// Resume the game if the monitor suspended it, then stop the monitor
    /// (ported from `InjectionManager.resume_if_suspended` — used when
    /// injection is skipped so the game is never left frozen).
    pub fn resume_if_suspended(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.game_monitor.resume_if_suspended();
    }

    /// Stop the current overlay process (ported from
    /// `InjectionManager.stop_overlay_process`).
    pub fn stop_overlay_process(&self) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(injector) = inner.injector.as_ref() {
            injector.stop_overlay_process();
        }
    }

    /// Kill all runoverlay processes — ChampSelect cleanup. Runs
    /// synchronously (bounded by `process::PROCESS_ENUM_TIMEOUT_S`); leaves
    /// backgrounding it to the caller (e.g. `spawn_blocking`) if profiling shows it's worth it.
    pub fn kill_all_runoverlay_processes(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.game_monitor.stop();
        if let Some(injector) = inner.injector.as_ref() {
            injector.kill_all_runoverlay_processes();
        }
    }

    /// Kill all mod-tools.exe processes — application shutdown (ported from
    /// `InjectionManager.kill_all_modtools_processes`).
    pub fn kill_all_modtools_processes(&self) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(injector) = inner.injector.as_ref() {
            injector.kill_all_modtools_processes();
        }
    }

    /// Get the last successfully injected skin (ported from
    /// `InjectionManager.last_injected_skin`).
    pub fn last_injected_skin(&self) -> Option<String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).last_skin_name.clone()
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).initialized
    }
}

/// Base-skin confirmation timing tracker. Tracks elapsed time between
/// forcing a base skin (LCU PATCH) and the WebSocket confirmation it
/// applied, persisting up to `MAX_SAMPLES` samples to
/// `base_skin_samples.json` so the troubleshooting UI can recommend a
/// threshold from real historical data instead of guessing.
pub mod base_skin_tracker {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Serialize};

    use crate::skins::paths;
    use crate::skins::slog::{log_info, log_warn};

    const MAX_SAMPLES: usize = 50;
    const MAX_CONFIRMATION_S: f64 = 10.0;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Sample {
        pub elapsed_ms: i64,
        pub confirmed: bool,
        pub ts: i64,
    }

    #[derive(Debug, Clone, Serialize)]
    pub struct Stats {
        pub total_samples: usize,
        pub confirmed_count: usize,
        pub timeout_count: usize,
        pub avg_ms: Option<f64>,
        pub p90_ms: Option<i64>,
        pub max_ms: Option<i64>,
        pub recommended_threshold_ms: Option<i64>,
    }

    struct Pending {
        skin_id: i64,
        start: Instant,
    }

    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

    fn data_path() -> PathBuf {
        paths::data_root().join("base_skin_samples.json")
    }

    fn load_samples() -> Vec<Sample> {
        let Ok(text) = std::fs::read_to_string(data_path()) else { return Vec::new() };
        let Ok(mut samples) = serde_json::from_str::<Vec<Sample>>(&text) else { return Vec::new() };
        let len = samples.len();
        if len > MAX_SAMPLES {
            samples.drain(0..len - MAX_SAMPLES);
        }
        samples
    }

    fn save_samples(mut samples: Vec<Sample>) {
        let len = samples.len();
        if len > MAX_SAMPLES {
            samples.drain(0..len - MAX_SAMPLES);
        }
        let path = data_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string(&samples) {
            let _ = std::fs::write(path, text);
        }
    }

    fn unix_now() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
    }

    /// Call after PATCHing the base skin (ported from `start_tracking`).
    pub fn start_tracking(target_skin_id: i64) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending { skin_id: target_skin_id, start: Instant::now() });
        log_info!("[TRACKER] Tracking base skin confirmation for skinId={target_skin_id}");
    }

    /// Call from the WebSocket session handler when `selectedSkinId` updates
    /// (ported from `on_skin_confirmed`). Returns the elapsed seconds if
    /// this was the pending confirmation.
    pub fn on_skin_confirmed(skin_id: i64) -> Option<f64> {
        let target = {
            let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(p) if p.skin_id == skin_id => guard.take(),
                _ => None,
            }
        }?;

        let elapsed_s = target.start.elapsed().as_secs_f64();
        // Discard impossibly late confirmations — likely stale tracking
        // from a previous champ select.
        if elapsed_s > MAX_CONFIRMATION_S {
            log_warn!(
                "[TRACKER] Discarding stale confirmation (skinId={}) after {elapsed_s:.1}s (>{MAX_CONFIRMATION_S}s)",
                target.skin_id
            );
            return None;
        }

        log_info!("[TRACKER] Base skin confirmed (skinId={}) in {elapsed_s:.3}s", target.skin_id);

        let mut samples = load_samples();
        samples.push(Sample { elapsed_ms: (elapsed_s * 1000.0).round() as i64, confirmed: true, ts: unix_now() });
        save_samples(samples);
        Some(elapsed_s)
    }

    /// Call when leaving ChampSelect with a pending confirmation (ported
    /// from `on_champ_select_exit`). Records a timeout sample.
    pub fn on_champ_select_exit() -> Option<f64> {
        let target = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        let elapsed_s = target.start.elapsed().as_secs_f64();
        log_warn!("[TRACKER] Base skin confirmation TIMED OUT (skinId={}) after {elapsed_s:.3}s", target.skin_id);

        let mut samples = load_samples();
        samples.push(Sample { elapsed_ms: (elapsed_s * 1000.0).round() as i64, confirmed: false, ts: unix_now() });
        save_samples(samples);
        Some(elapsed_s)
    }

    /// Compute recommendation statistics from historical samples (ported
    /// from `get_stats`).
    pub fn get_stats() -> Stats {
        let samples = load_samples();
        let confirmed: Vec<&Sample> = samples.iter().filter(|s| s.confirmed).collect();
        let timeout_count = samples.len() - confirmed.len();

        if confirmed.is_empty() {
            return Stats {
                total_samples: samples.len(),
                confirmed_count: 0,
                timeout_count,
                avg_ms: None,
                p90_ms: None,
                max_ms: None,
                recommended_threshold_ms: None,
            };
        }

        let mut times: Vec<i64> = confirmed.iter().map(|s| s.elapsed_ms).collect();
        times.sort_unstable();
        let avg = times.iter().sum::<i64>() as f64 / times.len() as f64;
        let p90_idx = ((times.len() as f64 * 0.9) as usize).saturating_sub(1).min(times.len() - 1);
        let p90 = times[p90_idx];
        let max_ms = *times.last().unwrap();
        // Recommended = p90 + 30% buffer, floored at 300ms (slider min), capped at 2000ms.
        let recommended = (p90 as f64 * 1.3).clamp(300.0, 2000.0) as i64;

        Stats {
            total_samples: samples.len(),
            confirmed_count: confirmed.len(),
            timeout_count,
            avg_ms: Some(avg.round()),
            p90_ms: Some(p90),
            max_ms: Some(max_ms),
            recommended_threshold_ms: Some(recommended),
        }
    }

    /// Clear all saved samples (ported from `clear_samples`).
    pub fn clear_samples() {
        let _ = std::fs::write(data_path(), "[]");
        log_info!("[TRACKER] Samples cleared");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_injection_threshold_clamps_negatives_and_allows_zero() {
        let mgr = InjectionManager::new(PathBuf::new(), PathBuf::new(), PathBuf::new(), PathBuf::new());
        assert_eq!(mgr.refresh_injection_threshold(-5.0), 0.0);
        assert_eq!(mgr.refresh_injection_threshold(1.25), 1.25);
    }

    #[test]
    fn inject_skin_immediately_skips_base_skin_without_touching_the_lock() {
        let mgr = InjectionManager::new(PathBuf::new(), PathBuf::new(), PathBuf::new(), PathBuf::new());
        // skinId == 0 short-circuits.
        assert!(!mgr.inject_skin_immediately("skin_0", None, None, None, &[]));
        // skinId == champion_id*1000 (base skin) short-circuits.
        assert!(!mgr.inject_skin_immediately("skin_99000", None, None, Some(99), &[]));
        assert!(!mgr.is_initialized());
    }

    #[test]
    fn clean_system_is_a_noop_true_when_never_initialized() {
        let mgr = InjectionManager::new(PathBuf::new(), PathBuf::new(), PathBuf::new(), PathBuf::new());
        assert!(mgr.clean_system());
    }

    #[test]
    fn injection_without_policy_hook_fails_closed() {
        // P0-A: an InjectionManager whose policy hook was never wired must
        // deny every entrypoint (IntegrityFailed), not run ungated.
        let mgr = InjectionManager::new(PathBuf::new(), PathBuf::new(), PathBuf::new(), PathBuf::new());
        assert!(!mgr.inject_skin_immediately("skin_103001", None, None, Some(103), &[]));
        assert!(!mgr.inject_mods_only_immediately(&["m".to_string()]));
        assert!(!mgr.is_initialized(), "denied before any initialization");
    }

    #[test]
    fn policy_hook_denial_blocks_and_allowance_reaches_the_pipeline() {
        use crate::safety_manager::{InjectionContext, InjectionDecision, InjectionDenial, PolicyHook};
        use std::sync::Arc;

        let mgr = InjectionManager::new(PathBuf::new(), PathBuf::new(), PathBuf::new(), PathBuf::new());

        let denies: PolicyHook = Arc::new(|_| InjectionDecision::Denied(InjectionDenial::Disabled));
        mgr.set_policy_hook(denies);
        assert!(!mgr.inject_mods_only_immediately(&["m".to_string()]));
        assert!(!mgr.is_initialized(), "denied at the gate, never initialized");

        let allows: PolicyHook = Arc::new(|op| {
            InjectionDecision::Allowed(InjectionContext { op, phase: Some("ChampSelect".into()), queue_id: Some(430) })
        });
        mgr.set_policy_hook(allows);
        // Past the gate now; fails later on the missing game dir (no League
        // in the test environment) — which proves the denial above came from
        // the policy, not from initialization.
        assert!(!mgr.inject_mods_only_immediately(&["m".to_string()]));
    }
}
