//! `%LOCALAPPDATA%\Chud` data-tree paths, plus elevation-aware desktop-user
//! resolution (ported from `utils\core\paths.py`).
//!
//! The elevation dance matters because Pengu Loader plugins run inside the
//! League client's CEF process — always unelevated, as the interactive
//! desktop user. If Chud itself runs elevated as a *different* account
//! ("Run as administrator"), naively reading `%LOCALAPPDATA%` would resolve
//! the admin account's directory and the two halves of the app would write
//! to different trees. Instead we find the desktop user via explorer.exe's
//! token and use that account's `AppData\Local`, matching what Pengu Loader sees.

#![allow(dead_code)] // consumed by S2+

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Cached desktop-user `AppData\Local` path (None = no mismatch detected, or
/// detection failed — caller falls back to this process's own env vars).
static DESKTOP_LOCALAPPDATA: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Root of Chud's writable data tree.
pub fn data_root() -> PathBuf {
    local_appdata().join("Chud")
}

pub fn skins_dir() -> PathBuf {
    data_root().join("skins")
}

pub fn state_dir() -> PathBuf {
    data_root().join("state")
}

pub fn injection_dir() -> PathBuf {
    data_root().join("injection")
}

pub fn injection_mods_dir() -> PathBuf {
    injection_dir().join("mods")
}

pub fn injection_overlay_dir() -> PathBuf {
    injection_dir().join("overlay")
}

pub fn injection_extract_cache_dir() -> PathBuf {
    injection_dir().join(".extract_cache")
}

pub fn mods_dir() -> PathBuf {
    data_root().join("mods")
}

pub fn resources_dir() -> PathBuf {
    data_root().join("resources")
}

pub fn logs_dir() -> PathBuf {
    data_root().join("logs")
}

pub fn pengu_loader_dir() -> PathBuf {
    data_root().join("Pengu Loader")
}

/// Create the full data-dir tree. Best-effort per directory: the first
/// failure (e.g. a locked-down profile) aborts and surfaces to the caller,
/// which logs it non-fatally (see `skins::init`).
pub fn ensure_tree() -> std::io::Result<()> {
    for dir in [
        skins_dir(),
        state_dir(),
        injection_mods_dir(),
        injection_overlay_dir(),
        injection_extract_cache_dir(),
        mods_dir(),
        resources_dir(),
        logs_dir(),
        pengu_loader_dir(),
    ] {
        std::fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Resolve `%LOCALAPPDATA%`, preferring the desktop user's profile over this
/// process's own environment when elevation caused a mismatch.
fn local_appdata() -> PathBuf {
    if let Some(desktop) = desktop_local_appdata() {
        return desktop;
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

#[cfg(windows)]
fn desktop_local_appdata() -> Option<PathBuf> {
    DESKTOP_LOCALAPPDATA.get_or_init(desktop_local_appdata_uncached).clone()
}

#[cfg(not(windows))]
fn desktop_local_appdata() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn desktop_local_appdata_uncached() -> Option<PathBuf> {
    let (desktop_user, profile_dir) = desktop_user_info()?;
    let current_user = std::env::var("USERNAME").unwrap_or_default();
    if desktop_user.eq_ignore_ascii_case(&current_user) {
        return None; // no mismatch — use this process's own env vars
    }
    let candidate = PathBuf::from(profile_dir).join("AppData").join("Local");
    candidate.is_dir().then_some(candidate)
}

/// Find the desktop user's username + profile directory via explorer.exe's
/// process token (always the interactive user, even when this process is
/// elevated as a different account). `None` on any failure — callers fall back.
#[cfg(windows)]
fn desktop_user_info() -> Option<(String, String)> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_INFORMATION};
    use windows::Win32::UI::Shell::GetUserProfileDirectoryW;
    use windows::core::{PCWSTR, PWSTR};

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    let pid = explorer_pid()?;

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_INFORMATION, false, pid).ok()?;
        let _process_guard = HandleGuard(process);

        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let _token_guard = HandleGuard(token);

        // First call sizes the buffer (expected to "fail" with the required length).
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), needed, &mut needed)
            .ok()?;
        let token_user = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let sid = token_user.User.Sid;

        let mut name = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain = [0u16; 256];
        let mut domain_len = domain.len() as u32;
        let mut sid_type = SID_NAME_USE(0);
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            PWSTR(name.as_mut_ptr()),
            &mut name_len,
            PWSTR(domain.as_mut_ptr()),
            &mut domain_len,
            &mut sid_type,
        )
        .ok()?;
        let username = String::from_utf16_lossy(&name[..name_len as usize]);

        let mut profile = [0u16; 260];
        let mut profile_len = profile.len() as u32;
        let profile_dir = if GetUserProfileDirectoryW(
            token,
            PWSTR(profile.as_mut_ptr()),
            &mut profile_len,
        )
        .is_ok()
        {
            let end = profile.iter().position(|&c| c == 0).unwrap_or(profile.len());
            String::from_utf16_lossy(&profile[..end])
        } else {
            format!("C:\\Users\\{username}")
        };

        Some((username, profile_dir))
    }
}

/// Locate explorer.exe's PID (always the interactive desktop user).
#[cfg(windows)]
fn explorer_pid() -> Option<u32> {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.processes()
        .values()
        .find(|p| p.name().to_string_lossy().eq_ignore_ascii_case("explorer.exe"))
        .map(|p| p.pid().as_u32())
}

/// Base assets directory: exe-relative `resources/assets`, with a dev-mode
/// fallback to the source tree so `cargo run` works without a bundled build.
fn assets_dir() -> PathBuf {
    let exe_candidate = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("resources").join("assets")));
    let dev_candidate =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join("assets");
    let cwd_candidate =
        std::env::current_dir().unwrap_or_default().join("resources").join("assets");

    [exe_candidate.clone(), Some(dev_candidate), Some(cwd_candidate)]
        .into_iter()
        .flatten()
        .find(|p| p.is_dir())
        .or(exe_candidate)
        .unwrap_or_else(|| PathBuf::from("resources/assets"))
}

/// Resolve an asset by relative name. Defense-in-depth against path
/// traversal: rejects absolute paths, drive letters, and `.`/`..`/empty
/// components lexically, then requires the resolved path to still live
/// under the assets dir even after symlink/junction resolution. Returns a
/// guaranteed-missing path for invalid input rather than `Option` (callers
/// already treat "doesn't exist" as not-found).
pub fn get_asset_path(name: &str) -> PathBuf {
    let assets = assets_dir();
    let invalid = assets.join("__invalid_asset_path__");

    let cleaned = name.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches('/');
    if cleaned.is_empty() || cleaned.contains(':') {
        return invalid;
    }

    let candidate = Path::new(cleaned);
    let has_bad_component = candidate.components().any(|c| {
        matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)
            || c.as_os_str().is_empty()
    });
    if candidate.is_absolute() || candidate.has_root() || has_bad_component {
        return invalid;
    }

    let asset_path = assets.join(candidate);
    match asset_path.canonicalize() {
        Ok(resolved) => {
            let assets_resolved = assets.canonicalize().unwrap_or_else(|_| assets.clone());
            if resolved.starts_with(&assets_resolved) {
                asset_path
            } else {
                invalid
            }
        }
        // Doesn't exist yet (e.g. not-yet-downloaded asset) — the lexical
        // component check above already ruled out traversal.
        Err(_) => asset_path,
    }
}
