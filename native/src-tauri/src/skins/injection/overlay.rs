//! mod-tools `mkoverlay`/`runoverlay` wrapper — ported from
//! `injection\overlay\overlay_manager.py` (`OverlayManager.mk_run_overlay`).
//!
//! THE CLI CONTRACT (`docs/SKINS_PORT.md` §2/§3) is preserved
//! character-for-character — these argv strings are a wire contract with
//! cslol's `mod-tools.exe`, not our code to reshape:
//!   `mkoverlay <mods_dir> <overlay_dir> --game:<gpath> --mods:<a>/<b> --noTFT --ignoreConflict`
//!   `runoverlay <overlay_dir> <overlay_dir>/cslol-config.json --game:<gpath> --opts:configless`
//!
//! Security note carried over from the Python original: every argv above is
//! built from trusted internal paths/config — no user-controlled input ever
//! reaches these commands directly.

#![allow(dead_code)] // consumed by S3+ (injector wiring)

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{OpenProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, PROCESS_SET_INFORMATION};

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
/// would compete with the game for CPU and hurt performance (ported
/// verbatim from `config.ENABLE_RUNOVERLAY_PRIORITY_BOOST`, which the
/// Python original ships `false` — asymmetric with the mkoverlay boost above).
pub const ENABLE_RUNOVERLAY_PRIORITY_BOOST: bool = false;

/// `subprocess.Popen(..., timeout=120)`'s default mkoverlay timeout (ported
/// verbatim from `OverlayManager.mk_run_overlay`'s `timeout: int = 120`).
/// This is the ABSOLUTE ceiling for a build with no suspended game; a build
/// holding a suspended game hostage is bounded much tighter by the
/// auto-resume abort below.
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
/// touches nearly every WAD in the game and is rebuilding a full game copy
/// (observed 2026-07-12: 17 GB / 156 WADs for a 4-mod set that legitimately
/// touched 4 WADs). Warn loudly so the offending mod set is identifiable.
const OVERLAY_SIZE_WARN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Never inject the hook into a game that has been loading UNSUSPENDED
/// longer than this: cslol's dll redirects file opens, and switching
/// redirects mid-load on a game that already opened half its WADs crashes
/// it (observed 2026-07-12: anticheat blocked the freeze, mkoverlay ran
/// 31s, hook landed at load+31s, game crashed). A fresh unsuspended game
/// (a few seconds old) is still safe — that's cslol's normal hook window.
const MAX_LATE_HOOK_AGE: Duration = Duration::from_secs(8);

/// How the mkoverlay wait loop ended.
enum BuildWait {
    Exited(std::process::ExitStatus),
    /// Hit `MKOVERLAY_TIMEOUT`.
    TimedOut,
    /// The game monitor's auto-resume safety fired: the game is now running
    /// WITHOUT the overlay hooked, so finishing the build is pointless — it
    /// would only keep grinding the disk (at boosted priority) against the
    /// loading game. This was the "corrupted League" incident chain: 60s
    /// frozen game -> forced resume -> another 60s of full-throttle WAD
    /// writes during load -> wedged Riot session needing reboot + repair.
    GameAutoResumed,
}

/// Create the overlay (`mkoverlay`) and run it (`runoverlay`), resuming the
/// suspended game exactly when `runoverlay` starts (ported from
/// `OverlayManager.mk_run_overlay`).
///
/// Returns `Ok(0)` on success, or `Ok(<code>)` mirroring one of Python's
/// sentinel return codes (`127` missing tool, `124` mkoverlay timeout, `125`
/// build aborted because the game auto-resumed without it, `126` hook
/// refused because the game loaded too long unsuspended, `1`
/// general error, or the child's own nonzero exit code) — wrapped in
/// `Result` per the S3 interface contract, but every failure path Python
/// handled without raising stays an `Ok` here too; `Err` is reserved for
/// setup failures Python didn't model as a return code (e.g. failing to
/// create the overlay directory).
pub fn mk_run_overlay(
    tools: &ToolPaths,
    mods_dir: &Path,
    overlay_dir: &Path,
    game_dir: &Path,
    mod_names: &[String],
    process: &SharedOverlayProcess,
    game_monitor: &mut GameMonitor,
) -> Result<i32, String> {
    if !tools.modtools.exists() {
        log_error!("[INJECTOR] Missing mod-tools.exe in {}", tools.modtools.display());
        return Ok(127);
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

    // Drain stdout+stderr on separate threads — CSLOL's logi() may write to
    // stdout, and reading only one pipe risks filling the other's buffer and
    // deadlocking the child (ported from overlay_manager.py's read_output
    // threads).
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

    // Build finished, but if the safety net released the game in the meantime
    // (race: build completes a beat after the auto-resume fired), the game is
    // already loading vanilla assets — hooking now yields partial/no skins.
    // Treat it like the abort case instead of pretending the injection worked.
    if game_monitor.auto_resume_fired() {
        log_error!("[INJECT] Game auto-resumed before runoverlay could start - skipping overlay this game");
        cleanup_failed_build(mods_dir, overlay_dir);
        return Ok(125);
    }
    // Same idea when the freeze never happened at all (anticheat refused the
    // suspend): a game that has been loading normally for a while must not
    // be hooked now — late injection into a half-loaded game crashes it.
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

    // ---- runoverlay ----
    let cfg = overlay_dir.join("cslol-config.json");
    log_info!("[INJECT] Running overlay");

    let run_child = match Command::new(&tools.modtools)
        .arg("runoverlay")
        .arg(overlay_dir)
        .arg(&cfg)
        .arg(format!("--game:{gpath}"))
        .arg("--opts:configless")
        .creation_flags(CREATE_NO_WINDOW)
        // Don't capture stdout/stderr — DEVNULL avoids the same pipe-buffer
        // deadlock risk as mkoverlay, but runoverlay's session is too
        // long-lived to babysit with drain threads.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log_error!("[INJECT] runoverlay error: {e}");
            return Ok(1);
        }
    };

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
    // The caller (`InjectionManager::do_inject_locked`) holds the injection
    // mutex across this whole function. The old code then sat in a loop waiting
    // for `runoverlay` to exit, which meant the mutex was held for the ENTIRE
    // match — and if that wait ever hung (observed in the wild: the loop's
    // `try_wait` never returned even after `runoverlay` had self-exited), the
    // mutex was leaked and EVERY later injection in the session timed out
    // acquiring it and silently failed ("skins stopped loading after one game").
    // There is no legitimate concurrent injection DURING a game, so holding the
    // mutex here bought nothing. The overlay persists on its own via the running
    // `runoverlay` process; the next champ-select's `reset_stuck_injection`
    // sweep reaps it, and the next injection overwrites the tracked child +
    // cleans the overlay dir before rebuilding. `runoverlay` also self-exits
    // when the game closes.
    log_info!("[INJECT] Overlay is live - injection complete");
    Ok(0)
}

fn drain_pipe(pipe: impl Read) -> Vec<String> {
    BufReader::new(pipe).lines().map_while(Result::ok).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// Delete extracted skin files immediately after mkoverlay consumes them
/// (ported from `OverlayManager._wipe_mods_dir`). Junction-safe: the custom
/// mod path stages entries as junctions into the extract cache / the user's
/// mod library, which must be unlinked, never recursed into.
fn wipe_dir_contents(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        crate::skins::injection::zips::safe_remove_entry(&entry.path());
    }
    log_info!("[INJECT] Wiped mods directory after mkoverlay");
}

/// Failure-path cleanup: unlink staged mods and delete the (possibly
/// multi-GB, partially-written) overlay so an aborted build never leaves a
/// carcass on disk (observed 2026-07-12: a killed 122s build left 17 GB in
/// the overlay dir until the next injection happened to clean it).
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

/// Boost `pid`'s priority class to `HIGH_PRIORITY_CLASS` (ported from
/// `psutil.Process.nice(psutil.HIGH_PRIORITY_CLASS)`). Best-effort — mirrors
/// Python swallowing the exception and just logging.
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
