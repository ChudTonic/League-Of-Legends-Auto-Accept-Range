//! Injection subsystem (S3) — cslol mod-tools orchestration. `InjectionManager`
//! is ported from `injection\core\manager.py` (`InjectionManager`), folding in
//! `injection\config\threshold_manager.py` (`ThresholdManager`) as a couple of
//! plain fields/methods rather than a separate collaborator object — there's
//! no `shared_state` back-reference to propagate threshold changes into here
//! (S4's `state.rs`/bridge own broadcasting that), so the extra class Python
//! needed to hold one float and a change-detector doesn't pull its weight in
//! Rust.

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

use crate::skins::injection::game_monitor::GameMonitor;
use crate::skins::injection::injector::SkinInjector;
use crate::skins::slog::{log_error, log_info, log_warn};

/// `config.INJECTION_LOCK_TIMEOUT_S` — timeout for acquiring the injection lock.
const INJECTION_LOCK_TIMEOUT: Duration = Duration::from_millis(2000);
/// `get_config_float("General", "injection_threshold", 0.5)`'s default —
/// S4's config surface is expected to push a real value in via
/// `refresh_injection_threshold` once it exists.
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
/// matching `docs/SKINS_PORT.md`'s "Threading model" (one coarse lock beats
/// Python's per-object `threading.Lock`s) — so Python's separate
/// `injection_lock`/`_cleanup_lock`/`_cleanup_in_progress` trio collapses
/// onto the one guard here. `game_dir` is supplied by the caller (S4/S5's
/// config or LCU-path wiring) via `set_game_dir` rather than detected here —
/// `injection/game/game_detector.py` isn't in this milestone's scope.
pub struct InjectionManager {
    tools_dir: PathBuf,
    mods_dir: PathBuf,
    zips_dir: PathBuf,
    overlay_dir: PathBuf,
    game_dir: Mutex<Option<PathBuf>>,
    /// Fast pre-check ahead of the real lock (ported from Python's
    /// `self._injection_in_progress` boolean, checked before
    /// `injection_lock.acquire(timeout=...)`).
    injection_in_progress: AtomicBool,
    inner: Mutex<Inner>,
}

impl InjectionManager {
    pub fn new(tools_dir: PathBuf, mods_dir: PathBuf, zips_dir: PathBuf, overlay_dir: PathBuf) -> Self {
        Self {
            tools_dir,
            mods_dir,
            zips_dir,
            overlay_dir,
            game_dir: Mutex::new(None),
            injection_in_progress: AtomicBool::new(false),
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
    /// before injection can initialize — mirrors `SkinInjector.__init__`'s
    /// `game_dir` parameter, just supplied externally instead of
    /// self-detected.
    pub fn set_game_dir(&self, game_dir: PathBuf) {
        *self.game_dir.lock().unwrap_or_else(|e| e.into_inner()) = Some(game_dir);
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
        ));
        inner.initialized = true;
        log_info!("[INJECT] Injection system initialized successfully");
    }

    /// Reload the injection threshold so config/tray changes apply
    /// immediately (ported from `ThresholdManager.refresh` /
    /// `InjectionManager.refresh_injection_threshold`). Allows `0.0` as a
    /// deliberate "no cooldown" value but guards against negatives.
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
    /// `GameMonitor` (ported call site: `lib.rs`'s `setup()`, right after
    /// construction). Forwards to `GameMonitor::set_auto_resume_timeout`,
    /// which keeps the 1..=180s clamp.
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

    /// Immediately inject a specific skin, with optional chroma (ported from
    /// `InjectionManager.inject_skin_immediately`).
    ///
    /// `extra_mod_names` is `injector::SkinInjector::inject_skin`'s
    /// replacement for Python's `extra_mods_callback` (party/category mods
    /// the caller has already extracted) — see that function's doc comment.
    pub fn inject_skin_immediately(
        &self,
        skin_name: &str,
        chroma_id: Option<i64>,
        champion_name: Option<&str>,
        champion_id: Option<i64>,
        extra_mod_names: &[String],
    ) -> bool {
        // Base-skin short-circuit (ported verbatim: `skin_{id}` where
        // `id == 0` or `id == champion_id * 1000` skips injection outright).
        if let Some(skin_id_str) = skin_name.strip_prefix("skin_") {
            if let Ok(skin_id) = skin_id_str.split('_').next().unwrap_or(skin_id_str).parse::<i64>() {
                if skin_id == 0 {
                    log_info!("[INJECT] Base skin detected (skinId=0) - injection skipped");
                    return false;
                }
                if let Some(champ) = champion_id {
                    if skin_id == champ * 1000 {
                        log_info!(
                            "[INJECT] Base skin detected (skinId={skin_id} for champion {champ}) - injection skipped"
                        );
                        return false;
                    }
                }
            }
        }

        // Fast pre-check ahead of the real lock (ported from Python's
        // `self._injection_in_progress` boolean check).
        if self.injection_in_progress.load(Ordering::SeqCst) {
            log_warn!("[INJECT] Injection already in progress - skipping request for: {skin_name}");
            return false;
        }

        // Emulate `injection_lock.acquire(timeout=INJECTION_LOCK_TIMEOUT_S)`
        // — `std::sync::Mutex` has no timed acquire, so poll `try_lock`
        // instead of blocking indefinitely (a plain blocking `.lock()` here
        // would make a concurrent caller wait out the ENTIRE injection,
        // including the runoverlay babysitting loop that can span a whole
        // game session — Python's timeout-then-bail behavior is the point).
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
        // Stop monitor after injection completes (resumes the game if it's
        // still suspended). The Python original did this after releasing `injection_lock`;
        // here it happens while `guard` is still held, which is fine —
        // `GameMonitor::stop` is self-contained and doesn't reach back into
        // `InjectionManager`.
        guard.game_monitor.stop();
        self.injection_in_progress.store(false, Ordering::SeqCst);

        success
    }

    /// Immediate mods-only injection: build the overlay from pre-staged mods
    /// with NO primary skin. Same lock / game-monitor discipline as
    /// `inject_skin_immediately`, minus the base-skin short-circuit (there is
    /// no primary skin to classify). Used for a party peer who selected no
    /// skin of their own but must still inject teammates' party skins — the
    /// case that previously dropped ALL party skins (a peer sees nobody's
    /// skin unless they themselves picked one).
    pub fn inject_mods_only_immediately(&self, mod_names: &[String]) -> bool {
        if mod_names.is_empty() {
            return false;
        }

        if self.injection_in_progress.load(Ordering::SeqCst) {
            log_warn!("[INJECT] Injection already in progress - skipping mods-only request");
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
    /// `do_inject_locked`'s init/threshold/monitor guards, then calls the
    /// injector's mods-only overlay builder).
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
    /// caller can hold `guard` across both this call and the `stop()` that
    /// follows it without fighting the borrow checker over a captured
    /// `&mut` in a closure.
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

    /// Heal a stuck injection state at the start of a new champ select. This is
    /// the fix for the "one leaked overlay blacks out skins for the rest of the
    /// session" bug: an injection holds `self.inner` for the WHOLE game (its
    /// babysit loop blocks until `runoverlay` exits), and `runoverlay`
    /// sometimes never self-exits — so `inner` stays locked and
    /// `injection_in_progress` stays `true`, and every later pick is rejected by
    /// the fast pre-check with "Injection already in progress". It ALSO leaks
    /// the `mod-tools.exe` process (which then locks the installer).
    ///
    /// Crucially this must NOT lock `self.inner` (that's exactly what's stuck),
    /// so it kills leaked `runoverlay` processes via OS enumeration and clears
    /// the `injection_in_progress` flag directly. Killing the child makes any
    /// stuck babysit loop's `try_wait` return, so it releases `inner` on its own
    /// well before the next loadout injection fires. Safe to call on ChampSelect
    /// entry: the previous game is definitively over, so no legitimate injection
    /// is in flight.
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

    /// Kill all runoverlay processes — ChampSelect cleanup (ported from
    /// `InjectionManager.kill_all_runoverlay_processes`). The Python original ran this on a
    /// background thread guarded by `_cleanup_in_progress`/`_cleanup_lock`
    /// so ChampSelect phase transitions never blocked on it; this port runs
    /// synchronously (the sweep itself is bounded by
    /// `process::PROCESS_ENUM_TIMEOUT_S`) and leaves backgrounding it to the
    /// S5 caller (e.g. `tokio::task::spawn_blocking`) if profiling shows
    /// it's worth it.
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

/// Base-skin confirmation timing tracker — ported from
/// `injection\config\base_skin_tracker.py`. Tracks the elapsed time between
/// forcing a base skin (LCU PATCH) and receiving the WebSocket confirmation
/// that it applied, persisting up to `MAX_SAMPLES` samples to
/// `%LOCALAPPDATA%\Chud\base_skin_samples.json` so the troubleshooting UI can
/// recommend a threshold from real historical data instead of guessing.
/// `time.perf_counter()` (monotonic elapsed) maps to `std::time::Instant`;
/// the on-disk `ts` field stays Unix-epoch seconds like Python's `time.time()`.
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
}
