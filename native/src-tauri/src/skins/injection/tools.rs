//! cslol tool presence + DMCA dll-hash gate — ported from
//! `injection\tools\tools_manager.py` (`ToolsManager`) and the `_check_dll_hash`
//! allowlist in `main\__init__.py`. `cslol-diag.exe` is dropped (dead per
//! `docs/SKINS_PORT.md` scope decisions — "cslol-diag.exe wiring") — it is
//! not in the required-tools list or `ToolPaths`.

#![allow(dead_code)] // consumed by S3+ (injector/overlay wiring)

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::skins::slog::{log_error, log_warn};

/// SHA-256 allowlist for the DMCA-restricted `cslol-dll.dll` (ported
/// verbatim from `main\__init__.py::_VALID_DLL_HASHES`). Chud never ships
/// this file — the user supplies their own signed copy; we only ever verify
/// it matches a known-good build.
const VALID_DLL_HASHES: &[&str] =
    &["4a009619c6dea691780b2f20cf17e08de478a78b3f11cd72759dd71c00ad1c90"];

/// Required tool filenames (ported from `ToolsManager.check_tools_available`,
/// minus `cslol-diag.exe` — dropped, dead).
const REQUIRED_TOOLS: &[&str] = &["mod-tools.exe", "cslol-dll.dll", "wad-extract.exe", "wad-make.exe"];

/// Resolved paths to the bundled cslol tools (ported from
/// `ToolsManager.detect_tools`'s return dict, minus the dropped `"diag"` entry).
#[derive(Debug, Clone)]
pub struct ToolPaths {
    pub modtools: PathBuf,
    pub wad_extract: PathBuf,
    pub wad_make: PathBuf,
    pub cslol_dll: PathBuf,
}

/// Why `verify_cslol_dll` refused the DLL — mirrors the two failure paths in
/// `main\__init__.py::_check_dll_present` (missing vs. hash-mismatch), minus
/// the Windows TaskDialog/MessageBox UI those functions also drove (a
/// user-facing prompt for this is S9's concern, not the injection subsystem's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DllVerifyError {
    /// `cslol-dll.dll` isn't present in the tools directory at all.
    Missing,
    /// The file exists but its SHA-256 isn't in `VALID_DLL_HASHES`.
    HashMismatch,
    /// The file exists but couldn't be read/hashed (locked, permissions...).
    Unreadable,
}

/// Exe-relative bundled-resources root, with a dev-mode fallback to the
/// source tree (same pattern as `paths::assets_dir` — `cargo run` works
/// without a bundled build). `paths.rs` only knows the *writable*
/// `%LOCALAPPDATA%\Chud` tree, not the read-only bundled-resources tree, so
/// this helper lives here and is reused by `pengu.rs` for the bundled Pengu
/// Loader payload.
pub fn resources_root() -> PathBuf {
    let exe_candidate = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("resources")));
    let dev_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");

    [exe_candidate.clone(), Some(dev_candidate)]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .or(exe_candidate)
        .unwrap_or_else(|| PathBuf::from("resources"))
}

/// RUNTIME cslol-tools directory — `%LOCALAPPDATA%\Chud\cslol-tools`
/// (user-data), NOT the install folder. We run `mod-tools.exe` from here so an
/// installer update never has to overwrite a locked, in-use tool — running it
/// straight out of the install dir was the recurring "Error opening file for
/// writing: mod-tools.exe" cause, because the running overlay held that exact
/// file open while the updater tried to replace it. It's also precisely where
/// the user-supplied `cslol-dll.dll` already lives, so mod-tools finds the DLL
/// right beside itself.
pub fn cslol_tools_dir() -> PathBuf {
    crate::skins::paths::data_root().join("cslol-tools")
}

/// The read-only copy shipped inside the installer (`(exe)/resources/cslol-tools/`)
/// — the SOURCE that `ensure_cslol_tools` seeds the runtime dir from on first
/// launch. Never executed directly, so the updater can always overwrite it.
pub fn bundled_cslol_tools_dir() -> PathBuf {
    resources_root().join("cslol-tools")
}

/// Seed the user-data runtime cslol-tools dir from the bundled installer copy.
/// The tools are byte-identical across Chud versions, so only a MISSING file is
/// copied — never overwrite (an existing `mod-tools.exe` may be held open by a
/// live runoverlay, and the user-supplied `cslol-dll.dll` must never be
/// clobbered). Because the app runs everything from user-data afterward, the
/// installer's copy is never executed, never locked, and an update that
/// overwrites it can't fail. Call once at startup; failures are silent (the
/// injection hash gate reports a genuinely missing/invalid tool). Supersedes
/// the old dll-only shuffle — the DLL's persistent location was already this
/// same user-data dir, so nothing moves for existing installs.
pub fn ensure_cslol_tools() {
    let bundled = bundled_cslol_tools_dir();
    let runtime = cslol_tools_dir();
    let _ = std::fs::create_dir_all(&runtime);
    // Small, byte-identical tools + the user-supplied DLL: straight copy if
    // missing. Never overwrite (a present file may be a locked runoverlay's
    // mod-tools.exe, or the user's own dll).
    for tool in ["mod-tools.exe", "wad-extract.exe", "wad-make.exe", "cslol-dll.dll"] {
        let dst = runtime.join(tool);
        if !dst.exists() {
            let src = bundled.join(tool);
            if src.exists() {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }
    // The ~207MB game-hash file mod-tools reads from its own dir is downloaded
    // at runtime (never in the installer). Migrate an existing copy from the
    // old install-folder location so relocating the runtime dir doesn't force a
    // fresh 207MB re-download. Atomic (temp + rename) so an interrupted copy
    // never leaves a corrupt hashes file that would break mkoverlay; if it's
    // absent (fresh install), the downloader fetches it into the runtime dir.
    let hashes_dst = runtime.join("hashes.game.txt");
    let hashes_src = bundled.join("hashes.game.txt");
    if !hashes_dst.exists() && hashes_src.exists() {
        let tmp = runtime.join("hashes.game.txt.migrating");
        let _ = std::fs::remove_file(&tmp);
        if std::fs::copy(&hashes_src, &tmp).is_ok() {
            let _ = std::fs::rename(&tmp, &hashes_dst);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Bundled Pengu Loader payload directory: `(exe)/resources/pengu-loader/`.
pub fn pengu_loader_resource_dir() -> PathBuf {
    resources_root().join("pengu-loader")
}

/// Check if all required CSLOL tools are present (ported from
/// `ToolsManager.check_tools_available`).
pub fn check_tools_available(tools_dir: &Path) -> bool {
    let missing: Vec<&str> =
        REQUIRED_TOOLS.iter().copied().filter(|tool| !tools_dir.join(tool).exists()).collect();

    if !missing.is_empty() {
        log_warn!("[INJECT] Missing CSLOL tools: {:?}", missing);
        log_warn!("[INJECT] Please place the bundled cslol tools in {}", tools_dir.display());
        return false;
    }

    true
}

/// Detect CSLOL tool paths, logging an error for anything missing (ported
/// from `ToolsManager.detect_tools`).
pub fn detect_tools(tools_dir: &Path) -> ToolPaths {
    let paths = ToolPaths {
        modtools: tools_dir.join("mod-tools.exe"),
        wad_extract: tools_dir.join("wad-extract.exe"),
        wad_make: tools_dir.join("wad-make.exe"),
        cslol_dll: tools_dir.join("cslol-dll.dll"),
    };

    for (name, exe) in
        [("mod-tools.exe", &paths.modtools), ("wad-extract.exe", &paths.wad_extract), ("wad-make.exe", &paths.wad_make)]
    {
        if !exe.exists() {
            log_error!("[INJECTOR] Missing tool: {name} ({})", exe.display());
        }
    }

    paths
}

/// Verify `cslol-dll.dll` against the pinned SHA-256 allowlist (ported from
/// `main\__init__.py::_check_dll_hash`/`_check_dll_present`'s hash-gate logic).
/// Hard-fails (returns `Err`) if missing or mismatched — exactly like the Python original;
/// callers must treat either as "injection unavailable", not a soft warning.
pub fn verify_cslol_dll(tools_dir: &Path) -> Result<(), DllVerifyError> {
    let dll_path = tools_dir.join("cslol-dll.dll");
    if !dll_path.exists() {
        log_error!(
            "[INJECT] cslol-dll.dll missing from {} (DMCA-restricted — you must supply your own signed copy)",
            tools_dir.display()
        );
        return Err(DllVerifyError::Missing);
    }

    let bytes = std::fs::read(&dll_path).map_err(|e| {
        log_error!("[INJECT] Failed to read cslol-dll.dll for hash verification: {e}");
        DllVerifyError::Unreadable
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex = format!("{:x}", hasher.finalize());

    if VALID_DLL_HASHES.contains(&hex.as_str()) {
        Ok(())
    } else {
        log_error!("[INJECT] cslol-dll.dll hash not recognized ({hex}) — refusing to use it");
        Err(DllVerifyError::HashMismatch)
    }
}

/// Set Hidden+System attributes on `path` and everything under it
/// (Windows only). Ported from the identical `_hide_directory` helper that
/// was duplicated in both `overlay_manager.py` (hides the overlay dir after
/// mkoverlay) and `mod_manager.py` (hides an extracted mod dir) — one copy
/// here, shared by `overlay.rs` and `storage.rs`.
#[cfg(windows)]
pub fn hide_directory_recursive(path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};

    fn hide_one(p: &Path) {
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
        }
    }

    fn collect_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                collect_recursive(&entry_path, out);
            }
            out.push(entry_path);
        }
    }

    hide_one(path);
    let mut children = Vec::new();
    collect_recursive(path, &mut children);
    for child in &children {
        hide_one(child);
    }
}

#[cfg(not(windows))]
pub fn hide_directory_recursive(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dll_is_reported_as_missing_not_mismatch() {
        let dir = std::env::temp_dir().join("chud_tools_test_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(verify_cslol_dll(&dir), Err(DllVerifyError::Missing));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_hash_is_reported_as_mismatch() {
        let dir = std::env::temp_dir().join("chud_tools_test_mismatch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cslol-dll.dll"), b"not the real dll").unwrap();
        assert_eq!(verify_cslol_dll(&dir), Err(DllVerifyError::HashMismatch));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
