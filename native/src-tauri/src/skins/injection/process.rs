//! Overlay/mod-tools process lifecycle — ported from
//! `injection\overlay\process_manager.py` (`ProcessManager`). The Python
//! original duplicated the "terminate, wait briefly, force-kill" escalation
//! once for `kill_all_runoverlay_processes` and once for
//! `kill_all_modtools_processes`; both are collapsed here onto one
//! `kill_matching_modtools` helper (matching `docs/SKINS_PORT.md`'s
//! instruction to de-duplicate it).
//!
//! Windows note: there is no POSIX-style graceful `SIGTERM` here —
//! `sysinfo::Process::kill` (and `std::process::Child::kill`) both resolve
//! to `TerminateProcess` on this platform, same as psutil's `.terminate()`
//! and `.kill()` did in the Python original. The "escalation" is therefore
//! "ask, wait a beat, ask again" rather than a true signal upgrade.

#![allow(dead_code)] // consumed by S3+ (injector wiring)

use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::skins::slog::{log_info, log_warn};

/// Timeout for `stop_overlay_process`'s post-kill wait (ported from
/// `config.PROCESS_TERMINATE_TIMEOUT_S`).
pub const PROCESS_TERMINATE_TIMEOUT_S: f64 = 5.0;
/// Short post-terminate wait before force-killing during a process sweep
/// (ported from `config.PROCESS_TERMINATE_WAIT_S`).
pub const PROCESS_TERMINATE_WAIT_S: f64 = 0.3;
/// Wall-clock budget for a `kill_all_*` process-table scan (ported from
/// `config.PROCESS_ENUM_TIMEOUT_S`).
pub const PROCESS_ENUM_TIMEOUT_S: f64 = 2.0;

/// The currently-running `runoverlay` child, shared between `overlay.rs`
/// (which spawns it) and this module (which can be asked to stop it from
/// elsewhere — ChampSelect cleanup, app shutdown). Mirrors
/// `ProcessManager.current_overlay_process`.
pub type SharedOverlayProcess = Arc<Mutex<Option<Child>>>;

pub fn new_shared_overlay_process() -> SharedOverlayProcess {
    Arc::new(Mutex::new(None))
}

/// terminate -> wait -> kill escalation for a PID found via process
/// enumeration (ported from the duplicated terminate/wait/kill blocks in
/// `kill_all_runoverlay_processes`/`kill_all_modtools_processes`).
fn terminate_then_kill_pid(sys: &mut System, pid: Pid, wait_after_terminate: Duration) {
    let Some(proc) = sys.process(pid) else {
        return; // already gone
    };
    proc.kill();

    std::thread::sleep(wait_after_terminate);
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    if let Some(proc) = sys.process(pid) {
        // Still around after the wait — force-kill (same call as above on
        // Windows, but mirrors the original's terminate()-then-kill() shape).
        proc.kill();
    }
}

/// Stop the tracked overlay child process (ported from
/// `ProcessManager.stop_overlay_process`).
pub fn stop_overlay_process(shared: &SharedOverlayProcess) {
    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());

    let Some(child) = guard.as_mut() else {
        log_info!("[INJECT] No active overlay process to stop");
        return;
    };

    if matches!(child.try_wait(), Ok(Some(_))) {
        *guard = None;
        return;
    }

    log_info!("[INJECT] Stopping current overlay process");
    if let Err(e) = child.kill() {
        log_warn!("[INJECT] Failed to stop overlay process: {e}");
        return;
    }

    let deadline = Instant::now() + Duration::from_secs_f64(PROCESS_TERMINATE_TIMEOUT_S);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                log_info!("[INJECT] Overlay process stopped successfully");
                break;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => break,
        }
    }
    *guard = None;
}

/// Kill all `mod-tools.exe` processes whose command line contains
/// `"runoverlay"` (ChampSelect cleanup — ported from
/// `ProcessManager.kill_all_runoverlay_processes`), then also stop our own
/// tracked overlay child.
pub fn kill_all_runoverlay_processes(shared: &SharedOverlayProcess) {
    kill_matching_modtools(shared, true);
}

/// Kill every `mod-tools.exe` process regardless of command line
/// (application shutdown — ported from
/// `ProcessManager.kill_all_modtools_processes`), then also stop our own
/// tracked overlay child.
pub fn kill_all_modtools_processes(shared: &SharedOverlayProcess) {
    kill_matching_modtools(shared, false);
}

/// OS-level kill of leaked `runoverlay` `mod-tools.exe` processes WITHOUT
/// touching any `InjectionManager`/`SharedOverlayProcess` lock. This is the
/// deadlock-safe path: a skin injection holds the manager's `inner` mutex for
/// the WHOLE game (its overlay babysit loop blocks until `runoverlay` exits),
/// so if `runoverlay` never self-exits, the normal
/// `InjectionManager::kill_all_runoverlay_processes` — which locks `inner` to
/// reach the injector — can never run. Killing by OS enumeration here makes the
/// stuck babysit loop's `try_wait` observe the dead child, return, and release
/// `inner` + the `injection_in_progress` flag. Used by
/// `InjectionManager::reset_stuck_injection` on ChampSelect entry.
pub fn kill_runoverlay_processes_os() {
    kill_matching_modtools_os(true);
}

fn kill_matching_modtools(shared: &SharedOverlayProcess, runoverlay_only: bool) {
    kill_matching_modtools_os(runoverlay_only);
    // Also stop our tracked process if it exists.
    stop_overlay_process(shared);
}

fn kill_matching_modtools_os(runoverlay_only: bool) {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let start = Instant::now();
    let timeout = Duration::from_secs_f64(PROCESS_ENUM_TIMEOUT_S);
    let wait_after_terminate = Duration::from_secs_f64(PROCESS_TERMINATE_WAIT_S);
    let label = if runoverlay_only { "runoverlay" } else { "mod-tools.exe" };

    let mut targets: Vec<Pid> = Vec::new();
    for (pid, proc) in sys.processes() {
        if start.elapsed() > timeout {
            log_warn!("[INJECT] Process enumeration timeout after {PROCESS_ENUM_TIMEOUT_S}s - some processes may not be killed");
            break;
        }
        if !proc.name().to_string_lossy().eq_ignore_ascii_case("mod-tools.exe") {
            continue;
        }
        if runoverlay_only {
            let has_runoverlay = proc.cmd().iter().any(|a| a.to_string_lossy().contains("runoverlay"));
            if !has_runoverlay {
                continue;
            }
        }
        targets.push(*pid);
    }

    let killed_count = targets.len();
    for pid in targets {
        log_info!("[INJECT] Killing {label} process PID {}", pid.as_u32());
        terminate_then_kill_pid(&mut sys, pid, wait_after_terminate);
    }

    if killed_count > 0 {
        log_info!("[INJECT] Killed {killed_count} {label} process(es)");
    } else {
        log_info!("[INJECT] No {label} processes found to kill");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_overlay_process_on_empty_slot_is_a_noop() {
        let shared = new_shared_overlay_process();
        stop_overlay_process(&shared); // must not panic
        assert!(shared.lock().unwrap().is_none());
    }
}
