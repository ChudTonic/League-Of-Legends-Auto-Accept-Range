//! Per-injection orchestration — ported from `injection\core\injector.py`
//! (`SkinInjector`). Collapses Python's PyInstaller `_MEIPASS`/one-dir/one-file
//! branching down to plain exe-relative resolution (`tools::resources_root`)
//! — Tauri always runs from a real executable, no frozen/dev split to detect.

#![allow(dead_code)] // consumed by S3+ (mod.rs / S5 trigger wiring)

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::safety_manager::PolicyHook;
use crate::skins::injection::game_monitor::GameMonitor;
use crate::skins::injection::process::{self, SharedOverlayProcess};
use crate::skins::injection::{overlay, storage, tools, zips};
use crate::skins::slog::{log_error, log_info, log_warn};

/// CSLOL-based skin injector (ported from `SkinInjector`).
pub struct SkinInjector {
    pub tools_dir: PathBuf,
    pub mods_dir: PathBuf,
    pub zips_dir: PathBuf,
    pub overlay_dir: PathBuf,
    pub game_dir: PathBuf,
    process: SharedOverlayProcess,
    /// Safety policy hook (P0-A), forwarded into `overlay::mk_run_overlay`
    /// so the build and run-overlay steps re-check the gates at execution
    /// time (phase/queue can change between the entry check and here).
    policy: Option<PolicyHook>,
}

impl SkinInjector {
    pub fn new(
        tools_dir: PathBuf,
        mods_dir: PathBuf,
        zips_dir: PathBuf,
        overlay_dir: PathBuf,
        game_dir: PathBuf,
        policy: Option<PolicyHook>,
    ) -> Self {
        let _ = std::fs::create_dir_all(&mods_dir);
        let _ = std::fs::create_dir_all(&zips_dir);

        // Check for CSLOL tools up front — logged only, doesn't block construction.
        tools::check_tools_available(&tools_dir);

        Self { tools_dir, mods_dir, zips_dir, overlay_dir, game_dir, process: process::new_shared_overlay_process(), policy }
    }

    /// Inject a single skin, with optional chroma and extra (party/category) mods.
    ///
    /// `extra_mod_names` replaces Python's `extra_mods_callback`: the caller
    /// (trigger/party hook) must already have staged those mod folders into
    /// `mods_dir` and pass the resulting folder names directly.
    ///
    /// CLEAN ORDERING CONTRACT: `clean_mods_dir` only runs here when
    /// `extra_mod_names` is empty — when extras are passed they're ALREADY
    /// staged in `mods_dir` for this overlay, so cleaning here would wipe
    /// them. In that case cleaning is the CALLER's job: clean first, stage
    /// extras, then call this (`trigger.rs`/`swiftplay.rs` both do this), so
    /// one overlay can carry a primary skin plus every pre-staged extra.
    pub fn inject_skin(
        &self,
        skin_name: &str,
        game_monitor: &mut GameMonitor,
        chroma_id: Option<i64>,
        champion_name: Option<&str>,
        champion_id: Option<i64>,
        extra_mod_names: &[String],
    ) -> Result<bool, String> {
        let injection_start = Instant::now();

        // Strip a trailing numeric skin ID for chroma path construction.
        let base_skin_name = strip_trailing_skin_id(skin_name);

        let Some(zp) =
            zips::resolve_zip(&self.zips_dir, skin_name, chroma_id, Some(&base_skin_name), champion_name, champion_id)
        else {
            log_error!("[INJECT] Skin '{skin_name}' not found in {}", self.zips_dir.display());
            log_available_skins(&self.zips_dir);
            return Ok(false);
        };

        log_info!("[INJECT] Using skin file: {}", zp.display());

        let clean_start = Instant::now();
        if should_clean_mods_dir(extra_mod_names) {
            storage::clean_mods_dir(&self.mods_dir);
        } else {
            log_info!(
                "[INJECT] Skipping mods-dir clean - {} extra mod(s) already staged by caller",
                extra_mod_names.len()
            );
        }
        storage::clean_overlay_dir(&self.overlay_dir);
        log_info!("[INJECT] Directory cleanup took {:.2}s", clean_start.elapsed().as_secs_f64());

        let extract_start = Instant::now();
        let mod_folder = storage::extract_zip_to_mod(&self.mods_dir, &zp).map_err(|e| e.to_string())?;
        log_info!("[INJECT] ZIP extraction took {:.2}s", extract_start.elapsed().as_secs_f64());

        let mut mod_names =
            vec![mod_folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()];
        if !extra_mod_names.is_empty() {
            log_info!(
                "[INJECT] Including {} party/extra mod(s): {}",
                extra_mod_names.len(),
                extra_mod_names.join(", ")
            );
            mod_names.extend(extra_mod_names.iter().cloned());
        }

        let tools = tools::detect_tools(&self.tools_dir);
        let result = overlay::mk_run_overlay(
            &tools,
            &self.mods_dir,
            &self.overlay_dir,
            &self.game_dir,
            &mod_names,
            &self.process,
            game_monitor,
            self.policy.as_ref(),
        )
        .map_err(|e| e.to_string())?;

        let total_duration = injection_start.elapsed().as_secs_f64();
        if result == 0 {
            log_info!("[INJECT] Completed in {total_duration:.2}s");
            Ok(true)
        } else {
            log_warn!("[INJECT] Failed - timeout or error after {total_duration:.2}s");
            Ok(false)
        }
    }

    /// Build the overlay from ALREADY-STAGED mods only, with no primary
    /// skin. Used when this player picked no skin of their own but still
    /// owes the overlay their party teammates' skins. The caller must have
    /// staged `mod_names` into `mods_dir` already; this never
    /// resolves/extracts a primary skin and never cleans the mods dir.
    pub fn inject_mods_only(&self, game_monitor: &mut GameMonitor, mod_names: &[String]) -> Result<bool, String> {
        if mod_names.is_empty() {
            return Ok(false);
        }
        let injection_start = Instant::now();
        storage::clean_overlay_dir(&self.overlay_dir);
        log_info!("[INJECT] Mods-only overlay from {} staged mod(s): {}", mod_names.len(), mod_names.join(", "));

        let tools = tools::detect_tools(&self.tools_dir);
        let result = overlay::mk_run_overlay(
            &tools,
            &self.mods_dir,
            &self.overlay_dir,
            &self.game_dir,
            mod_names,
            &self.process,
            game_monitor,
            self.policy.as_ref(),
        )
        .map_err(|e| e.to_string())?;

        let total_duration = injection_start.elapsed().as_secs_f64();
        if result == 0 {
            log_info!("[INJECT] Mods-only completed in {total_duration:.2}s");
            Ok(true)
        } else {
            log_warn!("[INJECT] Mods-only failed - timeout or error after {total_duration:.2}s");
            Ok(false)
        }
    }

    /// Clean the injection system (ported from `SkinInjector.clean_system`).
    pub fn clean_system(&self) -> bool {
        if self.mods_dir.exists() {
            match std::fs::read_dir(&self.mods_dir) {
                Ok(entries) => {
                    // Remove entries individually so junctions are unlinked safely.
                    for entry in entries.flatten() {
                        zips::safe_remove_entry(&entry.path());
                    }
                }
                Err(e) => {
                    log_error!("[INJECT] Failed to clean system: {e}");
                    return false;
                }
            }
            if let Err(e) = std::fs::remove_dir_all(&self.mods_dir) {
                log_warn!("[INJECT] remove_dir_all(mods_dir) non-fatal error: {e}");
            }
        }
        if self.overlay_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&self.overlay_dir) {
                log_warn!("[INJECT] remove_dir_all(overlay_dir) non-fatal error: {e}");
            }
        }
        log_info!("[INJECT] System cleaned successfully");
        true
    }

    /// Stop the current overlay process (ported from
    /// `SkinInjector.stop_overlay_process`).
    pub fn stop_overlay_process(&self) {
        process::stop_overlay_process(&self.process);
    }

    /// Kill all runoverlay processes — ChampSelect cleanup (ported from
    /// `SkinInjector.kill_all_runoverlay_processes`).
    pub fn kill_all_runoverlay_processes(&self) {
        process::kill_all_runoverlay_processes(&self.process);
    }

    /// Kill all mod-tools.exe processes — application shutdown (ported from
    /// `SkinInjector.kill_all_modtools_processes`).
    pub fn kill_all_modtools_processes(&self) {
        process::kill_all_modtools_processes(&self.process);
    }
}

/// Whether `inject_skin` should wipe `mods_dir` before extracting the
/// primary skin — see `inject_skin`'s "CLEAN ORDERING CONTRACT" doc comment.
/// Only cleans when there are no caller-staged extras to preserve.
fn should_clean_mods_dir(extra_mod_names: &[String]) -> bool {
    extra_mod_names.is_empty()
}

fn strip_trailing_skin_id(skin_name: &str) -> String {
    let parts: Vec<&str> = skin_name.split_whitespace().collect();
    if let Some(last) = parts.last() {
        if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
            return parts[..parts.len() - 1].join(" ");
        }
    }
    skin_name.to_string()
}

/// Diagnostic dump of the first 10 available skin archives, ported from
/// `inject_skin`'s "Skin not found" logging branch.
fn log_available_skins(zips_dir: &Path) {
    let mut avail = Vec::new();
    collect_archives(zips_dir, &mut avail);
    if avail.is_empty() {
        return;
    }
    log_info!("[INJECT] Available skins (first 10):");
    for a in avail.iter().take(10) {
        log_info!("  - {}", a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    }
}

fn collect_archives(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_archives(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
            Some("zip") | Some("fantome")
        ) {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_trailing_skin_id_removes_only_trailing_numeric_token() {
        assert_eq!(strip_trailing_skin_id("Elementalist Lux 99007"), "Elementalist Lux");
        assert_eq!(strip_trailing_skin_id("Elementalist Lux"), "Elementalist Lux");
        assert_eq!(strip_trailing_skin_id("K/DA 100"), "K/DA");
    }

    #[test]
    fn should_clean_mods_dir_only_when_no_extras_to_preserve() {
        assert!(should_clean_mods_dir(&[]));
        assert!(!should_clean_mods_dir(&["CHUD-Map".to_string()]));
    }

    /// A pre-staged extra mod folder must survive the conditional clean when
    /// extras are present, but still get wiped in the plain single-skin case
    /// so stale mods don't leak into a fresh overlay.
    #[test]
    fn clean_ordering_preserves_staged_extras_but_still_wipes_stale_mods_alone() {
        let root = std::env::temp_dir().join("chud_injector_test_clean_ordering");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("CHUD-ExtraMod")).unwrap();

        if should_clean_mods_dir(&["CHUD-ExtraMod".to_string()]) {
            storage::clean_mods_dir(&root);
        }
        assert!(root.join("CHUD-ExtraMod").is_dir(), "staged extra must survive when extras are present");

        if should_clean_mods_dir(&[]) {
            storage::clean_mods_dir(&root);
        }
        assert!(!root.join("CHUD-ExtraMod").is_dir(), "stale mod must be wiped when there are no extras to preserve");

        let _ = std::fs::remove_dir_all(&root);
    }
}
