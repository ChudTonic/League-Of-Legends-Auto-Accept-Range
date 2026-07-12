//! Pengu Loader lifecycle management — ported from
//! `utils\integration\pengu_loader.py`. Copies the bundled Pengu Loader
//! payload into `%LOCALAPPDATA%\Chud\Pengu Loader` (via `paths::pengu_loader_dir`),
//! preserving the user's `datastore` and per-plugin enable/disable choices
//! across every sync, then drives its CLI (`--set-league-path`,
//! `--force-activate`/`--force-deactivate`, `--restart-client`).
//!
//! The Python original's `_resolve_pengu_dir` branched on PyInstaller frozen/dev mode; Tauri
//! always runs from a real executable, so that branch collapses to the one
//! "copy bundled -> writable runtime dir" path every time (same
//! simplification `paths::assets_dir` already made).

#![allow(dead_code)] // consumed by S5+ (game-flow wiring calls into this)

use std::collections::HashSet;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use sysinfo::{ProcessesToUpdate, System};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{RegCloseKey, RegDeleteKeyW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ};

use crate::skins::injection::tools;
use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};

const PLUGIN_ENTRYPOINT: &str = "index.js";
const PLUGIN_ENTRYPOINT_DISABLED: &str = "index.js_";
const PLUGIN_ENTRYPOINT_BUNDLED_BACKUP: &str = "index.js.bundled";

/// Client-side process names Pengu Loader interacts with (ported from
/// `_LEAGUE_PROCESSES`).
const LEAGUE_PROCESSES: [&str; 4] =
    ["LeagueClient.exe", "LeagueClientUx.exe", "LeagueClientUxRender.exe", "League of Legends.exe"];
const PENGU_UI_PROCESS: &str = "Pengu Loader.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// IFEO registry key old Pengu Loader versions used to inject into League
/// (ported from `_IFEO_KEY`).
const IFEO_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\LeagueClientUx.exe";

/// Resolved once per process (ported from the Python module-level constant
/// `PENGU_DIR = _resolve_pengu_dir()`) — every call after the first reuses
/// the already-synced runtime directory instead of re-copying the whole
/// Pengu Loader payload on every activate/deactivate/set-path call.
static PENGU_DIR: OnceLock<PathBuf> = OnceLock::new();

fn pengu_dir() -> &'static Path {
    PENGU_DIR.get_or_init(resolve_pengu_dir)
}

/// Force the bundled→runtime Pengu sync on app startup, so newly-shipped
/// plugins (added in an update) always reach the runtime plugins folder even
/// when the user never re-activates Pengu. Idempotent (OnceLock) and preserves
/// datastore + plugin enable/disable state. Called from `skins::init`.
pub fn ensure_synced() {
    let _ = pengu_dir();
}

fn pengu_exe(dir: &Path) -> PathBuf {
    dir.join("Pengu Loader.exe")
}

fn is_available(dir: &Path) -> bool {
    pengu_exe(dir).exists()
}

fn active_flag_path() -> PathBuf {
    paths::state_dir().join("pengu_active.flag")
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Plugin enable/disable state preservation (ported verbatim from
// pengu_loader.py's `_sanitize_plugin_entrypoints` /
// `_snapshot_plugin_enable_state` / `_restore_plugin_enable_state`)
// ---------------------------------------------------------------------------

/// Ensure plugin enable/disable state survives the overlay sync.
///
/// Background: disabling a plugin renames `index.js` -> `index.js_`. The
/// sync below overlays the bundled Pengu Loader onto the runtime directory
/// without deleting extra files, so a disabled plugin can end up with BOTH
/// `index.js_` and a freshly-copied `index.js` — effectively re-enabling
/// (or duplicating) the plugin on next launch. Rule: if `index.js_` exists,
/// treat it as authoritative and park/remove any reintroduced `index.js`.
fn sanitize_plugin_entrypoints(pengu_dir: &Path) {
    let plugins_dir = pengu_dir.join("plugins");
    let Ok(entries) = std::fs::read_dir(&plugins_dir) else { return };

    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }

        let enabled = plugin_dir.join(PLUGIN_ENTRYPOINT);
        let disabled = plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED);
        if !disabled.exists() || !enabled.exists() {
            continue;
        }

        let backup = plugin_dir.join(PLUGIN_ENTRYPOINT_BUNDLED_BACKUP);
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        match std::fs::rename(&enabled, &backup) {
            Ok(()) => {
                log_info!("[PENGU] Preserved disabled plugin state by parking {} to {}", enabled.display(), backup.display())
            }
            Err(_) => {
                // Couldn't park it (locked/permission) — at least try to delete it.
                if std::fs::remove_file(&enabled).is_ok() {
                    log_info!("[PENGU] Removed reintroduced entrypoint for disabled plugin: {}", enabled.display());
                }
            }
        }
    }
}

/// Snapshot the user's enabled/disabled state for plugins before the
/// overlay sync. "enabled" = `index.js` exists and `index.js_` doesn't;
/// "disabled" = `index.js_` exists (ported from `_snapshot_plugin_enable_state`).
fn snapshot_plugin_enable_state(pengu_dir: &Path) -> (HashSet<String>, HashSet<String>) {
    let mut enabled = HashSet::new();
    let mut disabled = HashSet::new();

    let plugins_dir = pengu_dir.join("plugins");
    let Ok(entries) = std::fs::read_dir(&plugins_dir) else { return (enabled, disabled) };

    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let Some(name) = plugin_dir.file_name().and_then(|n| n.to_str()) else { continue };

        if plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED).exists() {
            disabled.insert(name.to_string());
        } else if plugin_dir.join(PLUGIN_ENTRYPOINT).exists() {
            enabled.insert(name.to_string());
        }
    }

    (enabled, disabled)
}

/// After the overlay sync, restore the user's prior plugin enable/disable
/// choices (ported from `_restore_plugin_enable_state`). Only `enabled` is
/// actually consulted here — same as the Python original, whose `disabled`
/// parameter goes unused too; disabled-state restoration is handled by
/// `sanitize_plugin_entrypoints` below, not by reading the snapshot.
fn restore_plugin_enable_state(pengu_dir: &Path, enabled: &HashSet<String>, _disabled: &HashSet<String>) {
    let plugins_dir = pengu_dir.join("plugins");

    // If the user had a plugin enabled, prefer enabled state: remove any
    // reintroduced `index.js_` from the bundle.
    for plugin_name in enabled {
        let plugin_dir = plugins_dir.join(plugin_name);
        if !plugin_dir.is_dir() {
            continue;
        }
        let enabled_entry = plugin_dir.join(PLUGIN_ENTRYPOINT);
        let disabled_entry = plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED);
        if enabled_entry.exists() && disabled_entry.exists() && std::fs::remove_file(&disabled_entry).is_ok() {
            log_info!(
                "[PENGU] Preserved enabled plugin state by removing bundled disabled entrypoint: {}",
                disabled_entry.display()
            );
        }
    }

    // If the user had a plugin disabled, keep disabled state authoritative
    // (also handles the "both files exist" case by parking `index.js`).
    sanitize_plugin_entrypoints(pengu_dir);
}

// ---------------------------------------------------------------------------
// Runtime directory resolution (ported from `_get_bundled_pengu_dir` /
// `_resolve_pengu_dir`)
// ---------------------------------------------------------------------------

fn copy_tree_preserving_datastore(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "datastore" {
            continue; // preserved separately below — never overwritten by the sync
        }
        let target = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree_preserving_datastore(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }

    // Seed the datastore once for a brand-new runtime directory. Pengu
    // Loader stores plugin/user settings there via `DataStore.*`;
    // overwriting it on every app update would wipe user preferences (ported
    // from the `bundled_datastore`/`runtime_datastore` seeding in
    // `_resolve_pengu_dir`).
    let bundled_datastore = src.join("datastore");
    let runtime_datastore = dst.join("datastore");
    if !runtime_datastore.exists() && bundled_datastore.exists() {
        let _ = std::fs::copy(&bundled_datastore, &runtime_datastore);
    }

    Ok(())
}

/// Copy the bundled Pengu Loader payload into the writable runtime
/// directory, preserving `datastore` and plugin enable/disable choices
/// across the sync (ported from `_resolve_pengu_dir`'s frozen-mode branch —
/// Chud has no unfrozen/dev-source-tree mode to fall back to).
fn resolve_pengu_dir() -> PathBuf {
    let bundled_dir = tools::pengu_loader_resource_dir();
    let runtime_dir = paths::pengu_loader_dir();

    if let Err(e) = std::fs::create_dir_all(&runtime_dir) {
        log_warn!("[PENGU] Failed to create Pengu Loader runtime directory: {e}");
        return bundled_dir;
    }

    if !bundled_dir.is_dir() {
        log_warn!("[PENGU] Bundled Pengu Loader directory not found: {}", bundled_dir.display());
        return runtime_dir;
    }

    // Snapshot plugin enabled/disabled state BEFORE overlaying bundled files.
    let (enabled_plugins, disabled_plugins) = snapshot_plugin_enable_state(&runtime_dir);

    if let Err(e) = copy_tree_preserving_datastore(&bundled_dir, &runtime_dir) {
        log_warn!("[PENGU] Failed to copy Pengu Loader to runtime directory: {e}");
        return bundled_dir;
    }
    log_info!("[PENGU] Synced Pengu Loader to runtime directory (preserving user files): {}", runtime_dir.display());

    // Restore plugin enable/disable state after the overlay sync.
    restore_plugin_enable_state(&runtime_dir, &enabled_plugins, &disabled_plugins);

    runtime_dir
}

// ---------------------------------------------------------------------------
// Process helpers (ported from `_is_league_running` / `_terminate_pengu_ui` /
// `_run_cli`)
// ---------------------------------------------------------------------------

fn is_league_running() -> bool {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.processes().values().any(|p| {
        let name = p.name().to_string_lossy();
        LEAGUE_PROCESSES.iter().any(|league_name| name.eq_ignore_ascii_case(league_name))
    })
}

fn terminate_pengu_ui() {
    match Command::new("taskkill").args(["/IM", PENGU_UI_PROCESS, "/F"]).creation_flags(CREATE_NO_WINDOW).output() {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            if !matches!(code, 0 | 128 | 255) {
                log_info!("[PENGU] taskkill for Pengu UI returned {code}");
            }
        }
        Err(e) => log_info!("[PENGU] Failed to terminate Pengu Loader UI process: {e}"),
    }
}

/// Execute the Pengu Loader CLI with the given arguments (ported from
/// `_run_cli`). Returns `true` when the command exits with one of `ok_codes`.
fn run_cli(dir: &Path, args: &[&str], ok_codes: &[i32]) -> bool {
    if !is_available(dir) {
        log_info!("[PENGU] Pengu Loader executable not found; skipping command {args:?}");
        return false;
    }

    let exe = pengu_exe(dir);
    match Command::new(&exe).args(args).current_dir(dir).creation_flags(CREATE_NO_WINDOW).output() {
        Ok(out) => {
            let code = out.status.code().unwrap_or(-1);
            if !ok_codes.contains(&code) {
                log_warn!(
                    "[PENGU] Pengu Loader CLI command {} exited with code {code} (expected {ok_codes:?})",
                    args.join(" ")
                );
                return false;
            }
            true
        }
        Err(e) => {
            log_warn!("[PENGU] Failed to launch Pengu Loader CLI {}: {e}", exe.display());
            false
        }
    }
}

fn write_active_flag() {
    let path = active_flag_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, "active").is_ok() {
        log_info!("[PENGU] Pengu active flag written: {}", path.display());
    }
}

fn clear_active_flag() {
    let _ = std::fs::remove_file(active_flag_path());
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Clean up the old Pengu Loader IFEO (Image File Execution Options)
/// registry entry — older Pengu Loader versions used IFEO to inject into
/// League, which can crash newer client versions (ported from
/// `cleanup_old_pengu_ifeo`; equivalent to Pengu's own
/// `irm https://pengu.lol/clean | iex` one-liner). Returns `true` if an
/// entry was found and deleted.
pub fn cleanup_old_pengu_ifeo() -> bool {
    let subkey = to_wide(IFEO_KEY);
    unsafe {
        let mut existing = HKEY::default();
        let open_status = RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(subkey.as_ptr()), 0, KEY_READ, &mut existing);
        if open_status != ERROR_SUCCESS {
            return false; // key doesn't exist (or inaccessible) — nothing to clean up
        }
        let _ = RegCloseKey(existing);

        let delete_status = RegDeleteKeyW(HKEY_LOCAL_MACHINE, PCWSTR(subkey.as_ptr()));
        if delete_status == ERROR_SUCCESS {
            log_info!("[PENGU] Cleaned up old Pengu Loader IFEO registry entry");
            true
        } else {
            log_info!("[PENGU] No permission to clean up IFEO registry entry (requires admin)");
            false
        }
    }
}

/// Set the League path in Pengu Loader configuration (ported from
/// `set_league_path`).
pub fn set_league_path(league_path: &str) -> bool {
    let dir = pengu_dir();
    if !is_available(dir) {
        log_info!("[PENGU] Pengu Loader not available; skipping set-league-path.");
        return false;
    }
    let trimmed = league_path.trim();
    if trimmed.is_empty() {
        log_warn!("[PENGU] Empty league path provided; skipping set-league-path.");
        return false;
    }

    log_info!("[PENGU] Setting League path in Pengu Loader: {trimmed}");
    run_cli(dir, &["--set-league-path", trimmed, "--silent"], &[0])
}

/// Outcome of `activate_on_start`, so the caller can tell the user whether the
/// League client actually rebooted (Pengu's `d3d9.dll` hook only loads on a
/// client restart; that restart hits the LCU and intermittently fails when the
/// client is unreachable — a login screen, mid-transition, or stale lockfile).
pub struct ActivateResult {
    /// Pengu's `--force-activate` succeeded (the hook is in place on disk).
    pub activated: bool,
    /// League was running, so a client restart was needed to load the hook.
    pub restart_needed: bool,
    /// The client restart actually went through. When `restart_needed` is true
    /// but this is false, the user must restart League manually to load Pengu.
    pub restarted: bool,
}

/// Force-activate Pengu Loader (ported from `activate_on_start`). Places the
/// hook, then restarts the League client so it loads with it — reporting
/// whether that restart succeeded so the UI can fall back to "restart manually".
pub fn activate_on_start(league_path: Option<&str>) -> ActivateResult {
    let dir = pengu_dir();
    if !is_available(dir) {
        log_info!("[PENGU] Pengu Loader not available; skipping activation.");
        return ActivateResult { activated: false, restart_needed: false, restarted: false };
    }

    if let Some(path) = league_path {
        if !set_league_path(path) {
            log_warn!("[PENGU] Failed to set league path in Pengu Loader, continuing with activation anyway.");
        }
    }

    terminate_pengu_ui();
    let restart_needed = is_league_running();

    log_info!("[PENGU] Activating Pengu Loader (restart League client: {restart_needed}).");

    // Write flag *before* activation so it persists even if the process is
    // killed mid-activation or the CLI returns an unexpected exit code.
    write_active_flag();

    let activated = run_cli(dir, &["--force-activate", "--silent"], &[0]);
    let mut restarted = false;
    if activated && restart_needed {
        restarted = run_cli(dir, &["--restart-client", "--silent"], &[0]);
        if !restarted {
            log_warn!(
                "[PENGU] Client restart failed (LCU unreachable) — Pengu is activated but the user must restart League manually to load it."
            );
        }
    }
    ActivateResult { activated, restart_needed, restarted }
}

/// Force-deactivate Pengu Loader when Chud shuts down (ported from
/// `deactivate_on_exit`).
pub fn deactivate_on_exit() -> bool {
    let dir = pengu_dir();
    if !is_available(dir) {
        return false;
    }

    let restart_needed = is_league_running();
    log_info!("[PENGU] Deactivating Pengu Loader (restart League client: {restart_needed}).");

    let deactivated = run_cli(dir, &["--force-deactivate", "--silent"], &[0]);
    if deactivated {
        clear_active_flag();
        if restart_needed {
            run_cli(dir, &["--restart-client", "--silent"], &[0]);
        }
    }
    deactivated
}

/// Check for a leftover active flag from a previous unclean shutdown and
/// deactivate Pengu Loader if found (ported from `cleanup_if_dirty`).
pub fn cleanup_if_dirty() -> bool {
    if !active_flag_path().exists() {
        return false;
    }

    log_info!("[PENGU] Detected leftover Pengu active flag — cleaning up from previous session.");
    let deactivated = deactivate_on_exit();
    // Flag is already cleared inside deactivate_on_exit() on success; clear
    // explicitly too in case deactivation itself failed, so we don't retry
    // forever on every launch.
    clear_active_flag();
    deactivated
}

/// Whether Pengu Loader is currently marked active — i.e. the flag file
/// `activate_on_start` writes and `deactivate_on_exit` clears still exists.
/// Used by the Skins control panel (S9) to show real activation status
/// without shelling out to the CLI.
pub fn is_active() -> bool {
    active_flag_path().exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_plugin_entrypoints_parks_reintroduced_index_js() {
        let root = std::env::temp_dir().join("chud_pengu_test_sanitize");
        let _ = std::fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugins").join("CHUD-Test");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join(PLUGIN_ENTRYPOINT), "enabled").unwrap();
        std::fs::write(plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED), "disabled").unwrap();

        sanitize_plugin_entrypoints(&root);

        assert!(!plugin_dir.join(PLUGIN_ENTRYPOINT).exists());
        assert!(plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED).exists());
        assert!(plugin_dir.join(PLUGIN_ENTRYPOINT_BUNDLED_BACKUP).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_then_restore_keeps_user_enabled_plugin_enabled() {
        let root = std::env::temp_dir().join("chud_pengu_test_restore");
        let _ = std::fs::remove_dir_all(&root);
        let plugin_dir = root.join("plugins").join("CHUD-Test");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        // User has it enabled locally.
        std::fs::write(plugin_dir.join(PLUGIN_ENTRYPOINT), "enabled").unwrap();

        let (enabled, disabled) = snapshot_plugin_enable_state(&root);
        assert!(enabled.contains("CHUD-Test"));

        // Simulate the bundle re-introducing a disabled marker during sync.
        std::fs::write(plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED), "disabled").unwrap();

        restore_plugin_enable_state(&root, &enabled, &disabled);

        assert!(plugin_dir.join(PLUGIN_ENTRYPOINT).exists());
        assert!(!plugin_dir.join(PLUGIN_ENTRYPOINT_DISABLED).exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
