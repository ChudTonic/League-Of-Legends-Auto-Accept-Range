//! Mods category tree storage — ported from `injection\mods\storage.py`
//! (`ModStorageService`) and `mod_manager.py` (`ModManager`).
//!
//! CHANGE from Python: its `_ensure_mods_root_layout` `shutil.rmtree`'d any
//! unrecognized root-level folder — a data-loss trap for anything a category
//! rename (or a user's own experiment) left behind. This port only *logs*
//! unknown root folders; nothing under `mods/` is ever deleted for being unrecognized.

#![allow(dead_code)] // consumed by S3+ (injector/bridge wiring)

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use crate::skins::features::special;
use crate::skins::injection::tools::hide_directory_recursive;
use crate::skins::injection::zips::{safe_extractall, safe_remove_entry, ExtractError};
use crate::skins::slog::{log_info, log_warn};

pub const CATEGORY_SKINS: &str = "skins";
pub const CATEGORY_MAPS: &str = "maps";
pub const CATEGORY_FONTS: &str = "fonts";
pub const CATEGORY_ANNOUNCERS: &str = "announcers";
pub const CATEGORY_UI: &str = "ui";
pub const CATEGORY_VOICEOVER: &str = "voiceover";
pub const CATEGORY_LOADING_SCREEN: &str = "loading_screen";
pub const CATEGORY_VFX: &str = "vfx";
pub const CATEGORY_SFX: &str = "sfx";
pub const CATEGORY_OTHERS: &str = "others";

/// Ported verbatim from `ModStorageService.ROOT_CATEGORIES`.
pub const ROOT_CATEGORIES: &[&str] = &[
    CATEGORY_SKINS,
    CATEGORY_MAPS,
    CATEGORY_FONTS,
    CATEGORY_ANNOUNCERS,
    CATEGORY_UI,
    CATEGORY_VOICEOVER,
    CATEGORY_LOADING_SCREEN,
    CATEGORY_VFX,
    CATEGORY_SFX,
    CATEGORY_OTHERS,
];

/// Non-skin categories `list_mods_for_category` accepts (ported from the
/// membership check in `ModStorageService.list_mods_for_category`).
const LISTABLE_CATEGORIES: &[&str] = &[
    CATEGORY_MAPS,
    CATEGORY_FONTS,
    CATEGORY_ANNOUNCERS,
    CATEGORY_UI,
    CATEGORY_VOICEOVER,
    CATEGORY_LOADING_SCREEN,
    CATEGORY_VFX,
    CATEGORY_SFX,
    CATEGORY_OTHERS,
];

/// Metadata for a mod inside `mods/skins/{skin_id}` (ported from
/// `SkinModEntry`).
#[derive(Debug, Clone, PartialEq)]
pub struct SkinModEntry {
    pub champion_id: Option<i64>,
    pub skin_id: i64,
    pub mod_name: String,
    pub path: PathBuf,
    pub updated_at: f64,
    pub description: Option<String>,
}

/// A mod entry inside a non-skin category (ported from the dict shape
/// `ModStorageService.list_mods_for_category` returns to the bridge).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CategoryModEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: f64,
    pub description: Option<String>,
}

/// Service exposing the on-disk mods hierarchy (ported from
/// `ModStorageService`).
pub struct ModStorageService {
    mods_root: PathBuf,
}

impl ModStorageService {
    pub fn new(mods_root: PathBuf) -> Self {
        let svc = Self { mods_root };
        let _ = std::fs::create_dir_all(&svc.mods_root);
        svc.ensure_mods_root_layout();
        svc
    }

    pub fn mods_root(&self) -> &Path {
        &self.mods_root
    }

    /// Ensure `mods_root` contains all expected category folders.
    /// Unrecognized root-level folders are only logged, never deleted.
    fn ensure_mods_root_layout(&self) {
        for category in ROOT_CATEGORIES {
            let _ = std::fs::create_dir_all(self.mods_root.join(category));
        }

        let Ok(entries) = std::fs::read_dir(&self.mods_root) else {
            log_warn!("[ModStorage] Failed to scan mods root {}", self.mods_root.display());
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if ROOT_CATEGORIES.contains(&name) {
                continue;
            }
            log_info!("[ModStorage] Unknown mods category folder (left alone): {}", path.display());
        }
    }

    pub fn skins_dir(&self) -> PathBuf {
        self.mods_root.join(CATEGORY_SKINS)
    }

    pub fn get_skin_dir(&self, skin_id: i64) -> PathBuf {
        self.skins_dir().join(skin_id.to_string())
    }

    pub fn list_mods_for_skin(&self, skin_id: i64) -> Vec<SkinModEntry> {
        let skin_dir = self.get_skin_dir(skin_id);
        if !skin_dir.is_dir() {
            return Vec::new();
        }

        let champion_id = Some(special::champion_of(skin_id));
        let mut candidates: Vec<_> = std::fs::read_dir(&skin_dir).map(|e| e.flatten().collect()).unwrap_or_default();
        candidates.sort_by_key(|e: &std::fs::DirEntry| e.file_name().to_string_lossy().to_lowercase());

        let mut entries = Vec::new();
        for candidate in candidates {
            let path = candidate.path();
            let mod_name = if path.is_dir() {
                path.file_name().map(|n| n.to_string_lossy().into_owned())
            } else if is_skin_archive(&path) {
                path.file_stem().map(|n| n.to_string_lossy().into_owned())
            } else {
                None
            };
            let Some(mod_name) = mod_name else { continue };

            entries.push(SkinModEntry {
                champion_id,
                skin_id,
                mod_name,
                updated_at: mtime_secs(&path),
                description: read_mod_description(&path),
                path,
            });
        }
        entries
    }

    /// Every `SkinModEntry` whose champion matches `champion_id` (ported
    /// from `ModStorageService.list_mods_for_champion`).
    pub fn list_mods_for_champion(&self, champion_id: i64) -> Vec<SkinModEntry> {
        let skins_dir = self.skins_dir();
        if !skins_dir.is_dir() {
            return Vec::new();
        }

        let mut children: Vec<_> = std::fs::read_dir(&skins_dir).map(|e| e.flatten().collect()).unwrap_or_default();
        children.sort_by_key(|e: &std::fs::DirEntry| e.file_name().to_string_lossy().to_lowercase());

        let mut entries = Vec::new();
        for child in children {
            let path = child.path();
            if !path.is_dir() {
                continue;
            }
            let Some(skin_id) = path.file_name().and_then(|n| n.to_str()).and_then(|s| s.parse::<i64>().ok()) else {
                continue;
            };
            if special::champion_of(skin_id) != champion_id {
                continue;
            }
            entries.extend(self.list_mods_for_skin(skin_id));
        }
        entries
    }

    pub fn has_mods_for_skin(&self, skin_id: i64) -> bool {
        !self.list_mods_for_skin(skin_id).is_empty()
    }

    /// List mods in a non-skin category (ported from
    /// `ModStorageService.list_mods_for_category`).
    pub fn list_mods_for_category(&self, category: &str) -> Vec<CategoryModEntry> {
        if !LISTABLE_CATEGORIES.contains(&category) {
            return Vec::new();
        }

        let category_dir = self.mods_root.join(category);
        if !category_dir.is_dir() {
            return Vec::new();
        }

        let mut candidates: Vec<_> = std::fs::read_dir(&category_dir).map(|e| e.flatten().collect()).unwrap_or_default();
        candidates.sort_by_key(|e: &std::fs::DirEntry| e.file_name().to_string_lossy().to_lowercase());

        let mut entries = Vec::new();
        for candidate in candidates {
            let path = candidate.path();
            let mod_name = if path.is_dir() {
                path.file_name().map(|n| n.to_string_lossy().into_owned())
            } else if is_skin_archive(&path) {
                path.file_stem().map(|n| n.to_string_lossy().into_owned())
            } else {
                None
            };
            let Some(mod_name) = mod_name else { continue };

            let relative = path.strip_prefix(&self.mods_root).unwrap_or(&path).to_string_lossy().replace('\\', "/");

            entries.push(CategoryModEntry {
                id: relative.clone(),
                name: mod_name,
                path: relative,
                updated_at: mtime_secs(&path),
                description: read_mod_description(&path),
            });
        }
        entries
    }
}

fn is_skin_archive(path: &Path) -> bool {
    path.is_file()
        && matches!(path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(), Some("zip") | Some("fantome"))
}

fn mtime_secs(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs_f64())
        .unwrap_or(0.0)
}

/// Read `description.txt` inside a mod directory, or `{name}.txt` beside a
/// single-file mod (ported from `ModStorageService._read_mod_description`).
fn read_mod_description(candidate: &Path) -> Option<String> {
    let description_file =
        if candidate.is_dir() { candidate.join("description.txt") } else { candidate.with_extension("txt") };
    if !description_file.exists() {
        return None;
    }
    match std::fs::read_to_string(&description_file) {
        Ok(text) => Some(text.trim().to_string()),
        Err(e) => {
            log_info!("[ModStorage] Unable to read descriptor {}: {e}", description_file.display());
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Extraction / cleanup (ported from injection\mods\mod_manager.py::ModManager)
// ---------------------------------------------------------------------------

/// Clean the mods directory, unlinking junctions safely rather than
/// following them into the (possibly huge, cached) target.
pub fn clean_mods_dir(mods_dir: &Path) {
    if !mods_dir.is_dir() {
        let _ = std::fs::create_dir_all(mods_dir);
        return;
    }
    let Ok(entries) = std::fs::read_dir(mods_dir) else { return };
    for entry in entries.flatten() {
        safe_remove_entry(&entry.path());
    }
}

/// Clean the overlay directory to prevent file-lock issues on the next
/// injection (ported from `ModManager.clean_overlay_dir`).
pub fn clean_overlay_dir(overlay_dir: &Path) {
    if overlay_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(overlay_dir) {
            log_warn!("[INJECT] Failed to clean overlay directory: {e}");
        } else {
            log_info!("[INJECT] Cleaned overlay directory");
        }
    }
    let _ = std::fs::create_dir_all(overlay_dir);
}

/// Extract a ZIP-compatible skin archive into `mods_dir`. The target folder
/// name is the zip's file stem, matching the `--mods:<name>` argument
/// `overlay::mk_run_overlay` builds from it.
pub fn extract_zip_to_mod(mods_dir: &Path, zip_path: &Path) -> Result<PathBuf, ExtractError> {
    let stem = zip_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mod".to_string());
    let target = mods_dir.join(&stem);

    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
    std::fs::create_dir_all(&target).map_err(ExtractError::Io)?;

    // Security: use safe extraction to prevent path traversal attacks.
    safe_extractall(zip_path, &target)?;

    // Hide extracted files so they can't be easily browsed.
    hide_directory_recursive(&target);

    let file_type = match zip_path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("zip") => "ZIP",
        Some("fantome") => ".fantome",
        _ => "archive",
    };
    log_info!(
        "[INJECT] Extracted {file_type}: {}",
        zip_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    );

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_layout_creates_categories_and_leaves_unknown_dirs() {
        let root = std::env::temp_dir().join("chud_storage_test_layout");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("some_users_experiment")).unwrap();

        let svc = ModStorageService::new(root.clone());

        for category in ROOT_CATEGORIES {
            assert!(root.join(category).is_dir(), "missing category {category}");
        }
        // The unknown folder must survive — no destructive rmtree.
        assert!(root.join("some_users_experiment").is_dir());

        drop(svc);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn has_mods_for_skin_reflects_extracted_folders() {
        let root = std::env::temp_dir().join("chud_storage_test_skins");
        let _ = std::fs::remove_dir_all(&root);
        let svc = ModStorageService::new(root.clone());

        assert!(!svc.has_mods_for_skin(99007));
        std::fs::create_dir_all(svc.get_skin_dir(99007).join("My Cool Mod")).unwrap();
        assert!(svc.has_mods_for_skin(99007));

        let entries = svc.list_mods_for_champion(99);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].champion_id, Some(99));

        let _ = std::fs::remove_dir_all(&root);
    }
}
