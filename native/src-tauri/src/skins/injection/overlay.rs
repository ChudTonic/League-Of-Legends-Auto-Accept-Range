//! mod-tools `mkoverlay`/`runoverlay` wrapper — ported from
//! `injection\overlay\overlay_manager.py` (`OverlayManager.mk_run_overlay`).
//!
//! THE CLI CONTRACT is preserved character-for-character — these argv
//! strings are a wire contract with cslol's `mod-tools.exe`, not ours to reshape:
//!   `mkoverlay <mods_dir> <overlay_dir> --game:<gpath> --mods:<a>/<b> --noTFT --ignoreConflict`
//!   `runoverlay <overlay_dir> <overlay_dir>/cslol-config.json --game:<gpath> --opts:configless`
//!
//! Every argv above is built from trusted internal paths/config — no
//! user-controlled input ever reaches these commands directly.

#![allow(dead_code)] // consumed by S3+ (injector wiring)

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{OpenProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, PROCESS_SET_INFORMATION};

use crate::safety_manager::{InjectionDecision, InjectionDenial, InjectionOp, PolicyHook};
use crate::skins::injection::game_monitor::GameMonitor;
use crate::skins::injection::process::SharedOverlayProcess;
use crate::skins::injection::tools::{hide_directory_recursive, ToolPaths};
use crate::skins::slog::{log_error, log_info, log_warn};

/// `CREATE_NO_WINDOW` (0x08000000) — hides the console window mod-tools.exe
/// would otherwise flash open (ported from `subprocess.CREATE_NO_WINDOW`).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Boost the short-lived `mkoverlay` process's priority during injection
/// setup (ported from `config.ENABLE_MKOVERLAY_PRIORITY_BOOST`).
pub const ENABLE_MKOVERLAY_PRIORITY_BOOST: bool = true;
/// `runoverlay` runs for the entire game session; boosting its priority
/// would compete with the game for CPU and hurt performance (asymmetric
/// with the mkoverlay boost above).
pub const ENABLE_RUNOVERLAY_PRIORITY_BOOST: bool = false;

/// Default mkoverlay timeout — the ABSOLUTE ceiling for a build with no
/// suspended game; a build holding a suspended game hostage is bounded much
/// tighter by the auto-resume abort below.
const MKOVERLAY_TIMEOUT: Duration = Duration::from_secs(120);
/// `PROCESS_MONITOR_SLEEP_S` — poll interval while babysitting the running
/// `runoverlay` child.
const PROCESS_MONITOR_SLEEP: Duration = Duration::from_millis(500);
/// mkoverlay wait-loop poll interval; the overlay-size check below runs
/// every `SIZE_CHECK_EVERY` of these.
const MKOVERLAY_POLL: Duration = Duration::from_millis(50);
const SIZE_CHECK_EVERY: u32 = 40; // ~every 2s
/// A single-champion overlay is tens of MB; even a heavy multi-mod set stays
/// well under this. Crossing it means cslol's fuzzy WAD matching decided a
/// mod (typically a RAW/loose-file custom fantome with shared asset paths)
/// touches nearly every WAD and is rebuilding a full game copy (observed:
/// 17 GB / 156 WADs for a 4-mod set that legitimately touched 4).
const OVERLAY_SIZE_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Never inject the hook into a game loading UNSUSPENDED longer than this:
/// switching cslol's file-open redirects mid-load, after the game already
/// opened half its WADs, crashes it (observed: anticheat blocked the freeze,
/// hook landed at load+31s, game crashed). A fresh unsuspended game is still
/// safe — that's cslol's normal hook window.
const MAX_LATE_HOOK_AGE: Duration = Duration::from_secs(8);

/// How the mkoverlay wait loop ended.
enum BuildWait {
    Exited(std::process::ExitStatus),
    /// Hit `MKOVERLAY_TIMEOUT`.
    TimedOut,
    /// The game monitor's auto-resume safety fired: the game is running
    /// WITHOUT the overlay hooked, so finishing the build is pointless — it
    /// only grinds the disk against the loading game. Root cause of the
    /// "corrupted League" incident: 60s frozen -> forced resume -> another
    /// 60s of full-throttle WAD writes -> wedged Riot session.
    GameAutoResumed,
}

/// Create the overlay (`mkoverlay`) and run it (`runoverlay`), resuming the
/// suspended game exactly when `runoverlay` starts.
///
/// Returns `Ok(0)` on success, or `Ok(<code>)` with a sentinel return code:
/// `127` missing tool, `124` mkoverlay timeout, `125` build aborted because
/// the game auto-resumed without it, `126` hook refused (game loaded too
/// long unsuspended), `123` safety policy denial (P0-A), `1` general error,
/// or the child's own exit code. `Err` is reserved for setup failures with
/// no sentinel code (e.g. failing to create the overlay directory).
#[allow(clippy::too_many_arguments)]
pub fn mk_run_overlay(
    tools: &ToolPaths,
    mods_dir: &Path,
    overlay_dir: &Path,
    game_dir: &Path,
    mod_names: &[String],
    process: &SharedOverlayProcess,
    game_monitor: &mut GameMonitor,
    policy: Option<&PolicyHook>,
) -> Result<i32, String> {
    if !tools.modtools.exists() {
        log_error!("[INJECTOR] Missing mod-tools.exe in {}", tools.modtools.display());
        return Ok(127);
    }

    // P0-A safety gate, re-checked before the build (state may have changed
    // since the entry check). No child spawned yet, so just resume any
    // suspended game and bail. `None` hook fails closed.
    if let Some(denial) = policy_denial(policy, InjectionOp::Build) {
        log_error!("[SAFETY] mkoverlay blocked ({}) - {}", denial.code(), denial.message());
        game_monitor.resume_if_suspended();
        return Ok(123);
    }

    std::fs::create_dir_all(overlay_dir)
        .map_err(|e| format!("failed to create overlay directory {}: {e}", overlay_dir.display()))?;

    let names_str = mod_names.join("/");
    let gpath = game_dir.display().to_string();

    log_info!(
        "[INJECT] Creating overlay: {} mkoverlay {} {} --game:{gpath} --mods:{names_str} --noTFT --ignoreConflict",
        tools.modtools.display(),
        mods_dir.display(),
        overlay_dir.display()
    );

    let mkoverlay_start = Instant::now();
    let mut child = match Command::new(&tools.modtools)
        .arg("mkoverlay")
        .arg(mods_dir)
        .arg(overlay_dir)
        .arg(format!("--game:{gpath}"))
        .arg(format!("--mods:{names_str}"))
        .arg("--noTFT")
        .arg("--ignoreConflict")
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_error!("[INJECT] mkoverlay error: {e} - monitor will auto-resume if needed");
            return Ok(1);
        }
    };

    if ENABLE_MKOVERLAY_PRIORITY_BOOST {
        boost_priority(child.id());
    }

    // Drain stdout+stderr on separate threads — reading only one pipe risks
    // filling the other's buffer and deadlocking the child.
    let stdout_pipe = child.stdout.take().expect("mkoverlay spawned with piped stdout");
    let stderr_pipe = child.stderr.take().expect("mkoverlay spawned with piped stderr");
    let stdout_thread = std::thread::spawn(move || drain_pipe(stdout_pipe));
    let stderr_thread = std::thread::spawn(move || drain_pipe(stderr_pipe));

    let deadline = mkoverlay_start + MKOVERLAY_TIMEOUT;
    let mut polls = 0u32;
    let mut size_warned = false;
    let wait_result = loop {
        match child.try_wait() {
            Ok(Some(s)) => break BuildWait::Exited(s),
            Ok(None) => {
                if game_monitor.auto_resume_fired() {
                    break BuildWait::GameAutoResumed;
                }
                if Instant::now() >= deadline {
                    break BuildWait::TimedOut;
                }
                polls += 1;
                if !size_warned && polls % SIZE_CHECK_EVERY == 0 && dir_size(overlay_dir) > OVERLAY_SIZE_WARN_BYTES {
                    size_warned = true;
                    log_warn!(
                        "[INJECT] Overlay build exceeded {} GiB and is still growing - a mod in this set ({}) is forcing a near-full-game WAD rebuild (usually a RAW/loose-file custom mod with shared asset paths). Repackage it as a proper WAD mod.",
                        OVERLAY_SIZE_WARN_BYTES / (1024 * 1024 * 1024),
                        mod_names.join(", ")
                    );
                }
                std::thread::sleep(MKOVERLAY_POLL);
            }
            Err(e) => {
                log_error!("[INJECT] mkoverlay error: {e} - monitor will auto-resume if needed");
                let _ = child.kill();
                let _ = child.wait();
                cleanup_failed_build(mods_dir, overlay_dir);
                return Ok(1);
            }
        }
    };

    let status = match wait_result {
        BuildWait::Exited(s) => s,
        BuildWait::GameAutoResumed => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_thread.join();
            let _ = stderr_thread.join();
            log_error!(
                "[INJECT] Game auto-resumed while the overlay was still building ({:.1}s in) - aborting mkoverlay; skins are skipped this game. The build was too slow to hook before the safety limit (mods: {}).",
                mkoverlay_start.elapsed().as_secs_f64(),
                mod_names.join(", ")
            );
            cleanup_failed_build(mods_dir, overlay_dir);
            return Ok(125);
        }
        BuildWait::TimedOut => {
            let _ = child.kill();
            let _ = child.wait();
            let mut all_lines = stdout_thread.join().unwrap_or_default();
            all_lines.extend(stderr_thread.join().unwrap_or_default());
            if all_lines.is_empty() {
                log_warn!("[INJECT] mkoverlay timeout - no output captured");
            } else {
                let tail_start = all_lines.len().saturating_sub(10);
                log_warn!("[INJECT] mkoverlay timeout - last output: {}", all_lines[tail_start..].join("; "));
            }
            cleanup_failed_build(mods_dir, overlay_dir);
            return Ok(124);
        }
    };
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();

    let mkoverlay_duration = mkoverlay_start.elapsed();

    if !status.success() {
        let code = status.code().unwrap_or(1);
        log_error!("[INJECT] mkoverlay failed with return code: {code}");
        cleanup_failed_build(mods_dir, overlay_dir);
        return Ok(code);
    }

    log_info!("[INJECT] mkoverlay completed in {:.2}s", mkoverlay_duration.as_secs_f64());

    // Build finished, but if the safety net released the game meanwhile
    // (race: completes a beat after auto-resume fired), the game is already
    // loading vanilla assets — treat it like the abort case.
    if game_monitor.auto_resume_fired() {
        log_error!("[INJECT] Game auto-resumed before runoverlay could start - skipping overlay this game");
        cleanup_failed_build(mods_dir, overlay_dir);
        return Ok(125);
    }
    // Same idea when the freeze never happened (anticheat refused the
    // suspend) — late injection into a half-loaded game crashes it.
    if let Some(age) = game_monitor.unsuspended_game_age() {
        if age > MAX_LATE_HOOK_AGE {
            log_error!(
                "[INJECT] Game has been loading UNSUSPENDED for {:.0}s (freeze was blocked) - hooking now risks crashing it; skipping overlay this game",
                age.as_secs_f64()
            );
            cleanup_failed_build(mods_dir, overlay_dir);
            return Ok(126);
        }
    }

    // Wipe extracted skin files now that mkoverlay is done with them, and
    // hide the overlay files so they can't be easily browsed.
    wipe_dir_contents(mods_dir);
    hide_directory_recursive(overlay_dir);

    log_info!("[INJECT] mkoverlay done - starting runoverlay");

    // P0-A safety gate, re-checked before the hook process starts — the
    // build takes seconds, plenty of time for a queue/phase/consent change.
    if let Some(denial) = policy_denial(policy, InjectionOp::RunOverlay) {
        log_error!("[SAFETY] runoverlay blocked ({}) - {}", denial.code(), denial.message());
        cleanup_failed_build(mods_dir, overlay_dir);
        game_monitor.resume_if_suspended();
        return Ok(123);
    }

    // ---- runoverlay ----
    let cfg = overlay_dir.join("cslol-config.json");
    log_info!("[INJECT] Running overlay");

    let mut run_child = match Command::new(&tools.modtools)
        .arg("runoverlay")
        .arg(overlay_dir)
        .arg(&cfg)
        .arg(format!("--game:{gpath}"))
        .arg("--opts:configless")
        .creation_flags(CREATE_NO_WINDOW)
        // Capture output on drain threads — cslol reports hook attempts/failures
        // here, and discarding them left us blind to "runoverlay never attached".
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_error!("[INJECT] runoverlay error: {e}");
            return Ok(1);
        }
    };
    if let Some(pipe) = run_child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    log_info!("[RUNOVERLAY] {line}");
                }
            }
        });
    }
    if let Some(pipe) = run_child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    log_warn!("[RUNOVERLAY] {line}");
                }
            }
        });
    }
    let run_child = run_child;

    if ENABLE_RUNOVERLAY_PRIORITY_BOOST {
        boost_priority(run_child.id());
    }

    {
        let mut guard = process.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(run_child);
    }

    // Resume game NOW - runoverlay started, game can load while runoverlay hooks in.
    log_info!("[INJECT] runoverlay started - resuming game");
    game_monitor.resume();

    // Return as soon as the overlay is live — DO NOT block until the game ends.
    // The caller holds the injection mutex across this whole function; the
    // old code looped waiting for `runoverlay` to exit, holding the mutex
    // for the ENTIRE match. When that wait hung (`try_wait` never returned
    // even after self-exit), the mutex leaked and every later injection that
    // session silently failed ("skins stopped loading after one game").
    // There's no legitimate concurrent injection DURING a game, so holding
    // the mutex bought nothing — the overlay persists on its own via the
    // running `runoverlay` process; `reset_stuck_injection` reaps it at the
    // next champ-select, and `runoverlay` also self-exits when the game closes.
    log_info!("[INJECT] Overlay is live - injection complete");
    Ok(0)
}

/// Evaluate the safety policy for `op`; `Some(denial)` blocks the caller.
/// A missing hook fails closed (`IntegrityFailed`) — by the time execution
/// reaches this module the hook must have been wired in `setup()`.
fn policy_denial(policy: Option<&PolicyHook>, op: InjectionOp) -> Option<InjectionDenial> {
    match policy {
        Some(hook) => match hook(op) {
            InjectionDecision::Allowed(_) => None,
            InjectionDecision::Denied(d) => Some(d),
        },
        None => Some(InjectionDenial::IntegrityFailed),
    }
}

fn drain_pipe(pipe: impl Read) -> Vec<String> {
    BufReader::new(pipe).lines().map_while(Result::ok).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// Delete extracted skin files immediately after mkoverlay consumes them.
/// Junction-safe: the custom mod path stages entries as junctions into the
/// extract cache/user's mod library, which must be unlinked, never recursed into.
fn wipe_dir_contents(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        crate::skins::injection::zips::safe_remove_entry(&entry.path());
    }
    log_info!("[INJECT] Wiped mods directory after mkoverlay");
}

/// Failure-path cleanup: unlink staged mods and delete the (possibly
/// multi-GB, partially-written) overlay so an aborted build never leaves a
/// carcass on disk (observed: a killed 122s build left 17 GB behind).
fn cleanup_failed_build(mods_dir: &Path, overlay_dir: &Path) {
    wipe_dir_contents(mods_dir);
    let _ = std::fs::remove_dir_all(overlay_dir);
    let _ = std::fs::create_dir_all(overlay_dir);
    log_info!("[INJECT] Cleaned mods + overlay directories after failed build");
}

/// Recursive directory size; cheap for the overlay's ~hundreds of entries.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Delete overlay WAD files after runoverlay finishes, recreating the empty
/// directory (ported from `OverlayManager._wipe_overlay_dir`).
fn wipe_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::create_dir_all(dir);
    log_info!("[INJECT] Wiped overlay directory after game ended");
}

/// Boost `pid`'s priority class to `HIGH_PRIORITY_CLASS`. Best-effort —
/// swallows failure and just logs.
fn boost_priority(pid: u32) {
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) else {
            log_warn!("[INJECT] Could not open process {pid} to boost priority");
            return;
        };
        if SetPriorityClass(handle, HIGH_PRIORITY_CLASS).is_err() {
            log_warn!("[INJECT] Could not boost process priority (PID={pid})");
        } else {
            log_info!("[INJECT] Boosted process priority (PID={pid})");
        }
        let _ = CloseHandle(handle);
    }
}
