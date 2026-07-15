//! Zip resolution + safe extraction + junction-or-extract cache — ported
//! from `injection\mods\zip_resolver.py` (`ZipResolver`), `safe_extract.py`,
//! and `junction.py`.
//!
//! `resolve_zip`'s special-case forms (Elementalist Lux, Sahn Uzal
//! Mordekaiser, Spirit Blossom Morgana, Radiant Sett, KDA Seraphine) were 5
//! near-duplicate private methods in Python, each hardcoding its own
//! fake-ID -> display-name -> file-stem table. Collapsed here onto one
//! generic dispatch through `features::special::FORMS` — Viego forms and
//! the Kai'Sa/Ahri HOL chromas already fell through to the generic scan in
//! Python too, preserved here via `FormSkin::zip_rel` being empty for those two.

#![allow(dead_code)] // consumed by S3+ (injector wiring)

use std::path::{Component, Path, PathBuf};

use crate::skins::features::special;
use crate::skins::slog::{log_error, log_info, log_warn};

/// Skin/chroma archive extensions cslol accepts (ported verbatim from
/// `zip_resolver.py::SKIN_EXTENSIONS`).
const SKIN_EXTENSIONS: [&str; 2] = [".zip", ".fantome"];

#[derive(Debug)]
pub enum ExtractError {
    Io(std::io::Error),
    Zip(zip::result::ZipError),
    /// A zip member's path would escape the destination directory (zip-slip).
    UnsafePath(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtractError::Io(e) => write!(f, "I/O error: {e}"),
            ExtractError::Zip(e) => write!(f, "zip error: {e}"),
            ExtractError::UnsafePath(name) => write!(f, "unsafe path in archive: {name}"),
        }
    }
}

impl std::error::Error for ExtractError {}

// ---------------------------------------------------------------------------
// Zip resolution (ported from ZipResolver.resolve_zip and its private
// per-champion helpers)
// ---------------------------------------------------------------------------

fn find_by_extensions(base: &Path, stem: &str) -> Option<PathBuf> {
    for ext in SKIN_EXTENSIONS {
        let candidate = base.join(format!("{stem}{ext}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Recursively search `base` for a file named exactly `filename` (ported
/// from `_rglob_by_extensions`'s `base.rglob(f"**/{pattern_stem}{ext}")`).
fn find_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().and_then(|n| n.to_str()) == Some(filename) {
            return Some(path);
        }
    }
    for subdir in subdirs {
        if let Some(found) = find_recursive(&subdir, filename) {
            return Some(found);
        }
    }
    None
}

fn rglob_by_extensions(base: &Path, pattern_stem: &str) -> Option<PathBuf> {
    for ext in SKIN_EXTENSIONS {
        if let Some(found) = find_recursive(base, &format!("{pattern_stem}{ext}")) {
            return Some(found);
        }
    }
    None
}

/// Resolve a skin/chroma ZIP by name, ID token, or literal path (ported from
/// `ZipResolver.resolve_zip`).
///
/// `champion_name` is accepted (matching the Python signature) but unused by
/// the resolution logic itself — Python's version never read it either.
pub fn resolve_zip(
    zips_dir: &Path,
    zip_arg: &str,
    chroma_id: Option<i64>,
    skin_name: Option<&str>,
    _champion_name: Option<&str>,
    champion_id: Option<i64>,
) -> Option<PathBuf> {
    log_info!("[INJECT] Resolving zip for: '{zip_arg}' (chroma_id: {chroma_id:?}, skin_name: {skin_name:?})");

    let cand = Path::new(zip_arg);
    if cand.exists() {
        return Some(cand.to_path_buf());
    }

    // Format: skin_{skin_id} - check if this is actually a chroma.
    if zip_arg.starts_with("skin_") {
        let Some(skin_id) = zip_arg.split('_').nth(1).and_then(|s| s.parse::<i64>().ok()) else {
            log_warn!("[INJECT] Malformed skin_ zip arg: {zip_arg}");
            return None;
        };
        let Some(champion_id) = champion_id else {
            log_warn!("[INJECT] No champion_id provided for skin ID: {skin_id}");
            return None;
        };

        // If chroma_id is provided, this is actually a chroma (Swiftplay case).
        if let Some(chroma_id) = chroma_id {
            return resolve_chroma_by_id(zips_dir, champion_id, chroma_id);
        }

        // Base skin - look for {champion_id}/{skin_id}/{skin_id}.zip/.fantome
        let skin_dir = zips_dir.join(champion_id.to_string()).join(skin_id.to_string());
        if let Some(found) = find_by_extensions(&skin_dir, &skin_id.to_string()) {
            log_info!("[INJECT] Found skin: {}", found.display());
            return Some(found);
        }

        // Not found as base skin - might be a chroma incorrectly labeled as skin_.
        log_info!("[INJECT] Base skin not found, checking if {skin_id} is a chroma...");
        return resolve_chroma_by_id(zips_dir, champion_id, skin_id);
    }

    // Format: chroma_{chroma_id} - this is a chroma.
    if zip_arg.starts_with("chroma_") {
        let Some(chroma_id) = zip_arg.split('_').nth(1).and_then(|s| s.parse::<i64>().ok()) else {
            log_warn!("[INJECT] Malformed chroma_ zip arg: {zip_arg}");
            return None;
        };
        let Some(champion_id) = champion_id else {
            log_warn!("[INJECT] No champion_id provided for chroma ID: {chroma_id}");
            return None;
        };
        return resolve_chroma_by_id(zips_dir, champion_id, chroma_id);
    }

    // For base skins (no chroma_id), we need skin_id — the UIA system
    // should have already resolved skin_name to skin_id upstream.
    if chroma_id.is_none() {
        if let Some(skin_name) = skin_name {
            if champion_id.is_none() {
                log_warn!("[INJECT] No champion_id provided for skin lookup: {skin_name}");
                return None;
            }
            log_warn!("[INJECT] No skin_id provided for skin '{skin_name}' - UIA should have resolved this");
            return None;
        }
    }

    if let Some(chroma_id) = chroma_id {
        // Forms/HOL-chroma special cases (features::special is the single
        // source of truth — see module doc comment above).
        if let Some(form) = special::form_by_id(chroma_id) {
            if !form.zip_rel.is_empty() {
                log_info!("[INJECT] Detected {} form fake ID: {chroma_id}", form.champion);
                let stem = Path::new(form.zip_rel).file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                log_info!("[INJECT] Looking for {} {} form", form.champion, form.display);
                if let Some(found) = rglob_by_extensions(zips_dir, stem) {
                    log_info!("[INJECT] Found {} {} form: {}", form.champion, form.display, found.display());
                    return Some(found);
                }
                log_warn!("[INJECT] {} {} form file not found", form.champion, form.display);
                return None;
            }
            // Empty zip_rel (Kai'Sa/Ahri HOL chromas): real skin IDs stored
            // like any other chroma — fall through to the generic scan below.
        }

        // Regular chromas: {champion_id}/{skin_id}/{chroma_id}/{chroma_id}.zip
        let Some(champion_id) = champion_id else {
            log_warn!("[INJECT] No champion_id provided for chroma lookup: {chroma_id}");
            return None;
        };
        return resolve_chroma_by_id(zips_dir, champion_id, chroma_id);
    }

    log_warn!("[INJECT] Base skin lookup by name not fully implemented for new structure: {zip_arg}");
    None
}

/// Resolve a chroma ZIP by champion ID + chroma ID, scanning every numeric
/// skin subdirectory under the champion's directory (ported from
/// `ZipResolver._resolve_chroma_by_id`).
fn resolve_chroma_by_id(zips_dir: &Path, champion_id: i64, chroma_id: i64) -> Option<PathBuf> {
    let champion_dir = zips_dir.join(champion_id.to_string());
    if !champion_dir.exists() {
        log_warn!("[INJECT] Champion directory not found: {}", champion_dir.display());
        return None;
    }

    let Ok(entries) = std::fs::read_dir(&champion_dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let skin_dir = entry.path();
        if !skin_dir.is_dir() {
            continue;
        }
        let Some(name) = skin_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.parse::<i64>().is_err() {
            continue; // not a skin ID directory
        }

        let chroma_dir = skin_dir.join(chroma_id.to_string());
        if chroma_dir.exists() {
            if let Some(found) = find_by_extensions(&chroma_dir, &chroma_id.to_string()) {
                log_info!(
                    "[INJECT] Found chroma: {}",
                    found.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                );
                return Some(found);
            }
        }
    }

    log_warn!("[INJECT] Chroma {chroma_id} not found in any skin directory for champion {champion_id}");
    None
}

// ---------------------------------------------------------------------------
// Safe extraction (ported from utils\core\safe_extract.py)
// ---------------------------------------------------------------------------

/// Reject a zip member path that would escape the destination directory,
/// returning its safe relative form. Component-aware (checks
/// `Component::ParentDir`/absolute/root, then `Path::starts_with` on the
/// joined result) rather than Python's string-prefix check, which a sibling
/// directory sharing `base` as a prefix (e.g. `mods-evil` vs `mods`) would incorrectly pass.
fn safe_member_relpath(name: &str) -> Option<PathBuf> {
    let cleaned = name.replace('\\', "/");
    let candidate = Path::new(&cleaned);

    let mut rel = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
}

/// Safely extract every entry of a ZIP file to `dest_dir`, validating each
/// member's path before extracting anything. Returns the number of files
/// extracted, or `Err(ExtractError::UnsafePath)` on the first zip-slip attempt found.
pub fn safe_extractall(zip_path: &Path, dest_dir: &Path) -> Result<usize, ExtractError> {
    std::fs::create_dir_all(dest_dir).map_err(ExtractError::Io)?;

    let file = std::fs::File::open(zip_path).map_err(ExtractError::Io)?;
    let mut archive = zip::ZipArchive::new(file).map_err(ExtractError::Zip)?;

    // First pass: validate every member stays inside dest_dir before
    // extracting anything.
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(ExtractError::Zip)?;
        let name = entry.name().to_string();
        let Some(rel) = safe_member_relpath(&name) else {
            log_error!("[SECURITY] Blocked unsafe path in archive: {name}");
            return Err(ExtractError::UnsafePath(name));
        };
        let target = dest_dir.join(&rel);
        if !target.starts_with(dest_dir) {
            log_error!("[SECURITY] Blocked unsafe path in archive: {name}");
            return Err(ExtractError::UnsafePath(name));
        }
    }

    let count = archive.len();
    archive.extract(dest_dir).map_err(ExtractError::Zip)?;
    log_info!("[EXTRACT] Safely extracted {count} files to {}", dest_dir.display());
    Ok(count)
}

// ---------------------------------------------------------------------------
// Junction-or-extract cache (ported from utils\core\junction.py)
// ---------------------------------------------------------------------------

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Create a Windows directory junction at `link` pointing to `source`,
/// falling back to a recursive copy if junction creation fails (ported from
/// `junction.py::create_junction`). Returns `true` on a real junction.
fn create_junction(source: &Path, link: &Path) -> bool {
    match junction::create(source, link) {
        Ok(()) => {
            log_info!("[JUNCTION] Created junction: {} -> {}", link.display(), source.display());
            true
        }
        Err(e) => {
            log_warn!("[JUNCTION] Junction create failed ({e}), falling back to copytree");
            match copy_dir_recursive(source, link) {
                Ok(()) => log_info!("[JUNCTION] Fallback copytree: {} -> {}", source.display(), link.display()),
                Err(copy_err) => log_error!("[JUNCTION] Fallback copytree also failed: {copy_err}"),
            }
            false
        }
    }
}

/// Remove `path` safely, distinguishing a junction (unlinked without
/// touching its target) from a real directory/file (ported from
/// `junction.py::safe_remove_entry`).
pub fn safe_remove_entry(path: &Path) {
    if junction::exists(path).unwrap_or(false) {
        match junction::delete(path) {
            Ok(()) => log_info!("[JUNCTION] Removed junction: {}", path.display()),
            Err(e) => log_warn!("[JUNCTION] Failed to remove junction {}: {e}", path.display()),
        }
        return;
    }

    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
        return;
    }

    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
}

/// mtime-cached extraction of `zip_path` into `cache_dir`, keyed by
/// `folder_name`. Cache is invalidated when the source file's mtime changes
/// (e.g. the user replaces the archive with an updated version).
fn get_or_extract_to_cache(zip_path: &Path, folder_name: &str, cache_dir: &Path) -> Result<PathBuf, ExtractError> {
    std::fs::create_dir_all(cache_dir).map_err(ExtractError::Io)?;
    let cached = cache_dir.join(folder_name);
    let stamp = cache_dir.join(format!("{folder_name}.mtime"));

    let source_mtime = std::fs::metadata(zip_path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64().to_string())
        .unwrap_or_default();

    let mut needs_extract = true;
    if cached.is_dir() && stamp.exists() {
        if let Ok(stored) = std::fs::read_to_string(&stamp) {
            if stored.trim() == source_mtime {
                needs_extract = false;
                log_info!("[JUNCTION] Cache hit for {folder_name}");
            }
        }
    }

    if needs_extract {
        if cached.exists() || junction::exists(&cached).unwrap_or(false) {
            safe_remove_entry(&cached);
        }
        std::fs::create_dir_all(&cached).map_err(ExtractError::Io)?;

        log_info!(
            "[JUNCTION] Extracting {} to cache: {}",
            zip_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            cached.display()
        );
        safe_extractall(zip_path, &cached)?;

        let _ = std::fs::write(&stamp, &source_mtime);
    }

    Ok(cached)
}

/// Place mod content at `dest` using the fastest available method (ported
/// from `junction.py::link_or_extract`):
/// - directory source: junction `dest` -> `source` (zero-copy, falls back
///   to a recursive copy);
/// - zip/fantome source: extract once into `cache_dir`, then junction
///   `dest` -> the cached directory (subsequent calls are instant);
/// - any other file: plain copy into `dest`.
pub fn link_or_extract(source: &Path, dest: &Path, cache_dir: &Path) -> Result<(), ExtractError> {
    if source.is_dir() {
        create_junction(source, dest);
        return Ok(());
    }

    if source.is_file() {
        let ext = source.extension().and_then(|e| e.to_str()).map(str::to_lowercase);
        if matches!(ext.as_deref(), Some("zip") | Some("fantome")) {
            let folder_name = source.file_stem().and_then(|s| s.to_str()).unwrap_or("mod").to_string();
            let cached = get_or_extract_to_cache(source, &folder_name, cache_dir)?;
            create_junction(&cached, dest);
        } else {
            std::fs::create_dir_all(dest).map_err(ExtractError::Io)?;
            let target = dest.join(source.file_name().unwrap_or_default());
            std::fs::copy(source, &target).map_err(ExtractError::Io)?;
            log_info!(
                "[JUNCTION] Copied file: {} -> {}",
                source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                dest.display()
            );
        }
        return Ok(());
    }

    log_warn!("[JUNCTION] Source does not exist: {}", source.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_member_relpath_rejects_traversal_and_absolute() {
        assert!(safe_member_relpath("../../etc/passwd").is_none());
        assert!(safe_member_relpath("C:\\Windows\\System32\\evil.dll").is_none());
        assert!(safe_member_relpath("/etc/passwd").is_none());
        assert_eq!(safe_member_relpath("Fizz/Fizz.wad.client").unwrap(), PathBuf::from("Fizz/Fizz.wad.client"));
    }

    #[test]
    fn resolve_zip_returns_literal_existing_path() {
        let dir = std::env::temp_dir().join("chud_zips_test_literal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("MySkin.zip");
        std::fs::write(&file, b"fake").unwrap();

        let found = resolve_zip(&dir, file.to_str().unwrap(), None, None, None, None);
        assert_eq!(found, Some(file));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_zip_finds_form_via_special_table() {
        let dir = std::env::temp_dir().join("chud_zips_test_forms");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Lux/Forms")).unwrap();
        let file = dir.join("Lux/Forms/Lux Elementalist Air.zip");
        std::fs::write(&file, b"fake").unwrap();

        // fake_id 99991 -> "Lux/Forms/Lux Elementalist Air.zip" per features::special::FORMS.
        // zip_arg must NOT start with "skin_"/"chroma_" — those prefixes route
        // straight to the generic directory scan, bypassing special-forms dispatch.
        let found = resolve_zip(&dir, "Lux", Some(99991), Some("Lux"), None, Some(99));
        assert_eq!(found, Some(file));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
