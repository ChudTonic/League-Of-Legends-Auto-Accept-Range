//! Thin Windows helpers: admin check, League-game focus detection, relaunch-as-admin.

#[cfg(windows)]
pub fn is_admin() -> bool {
    use windows::Win32::UI::Shell::IsUserAnAdmin;
    unsafe { IsUserAnAdmin().as_bool() }
}

/// True when the focused window is the League *game* client (class
/// `RiotWindowClass`) — used to only hold the range key while in-game.
#[cfg(windows)]
pub fn lol_game_focused() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut buf);
        if len <= 0 {
            return false;
        }
        // Compare UTF-16 units directly — `hold_loop` calls this up to 50x/s
        // in-game, so avoid allocating a String per tick.
        buf[..len as usize].iter().copied().eq("RiotWindowClass".encode_utf16())
    }
}

/// Screen rect (physical px: left, top, right, bottom) of the League *client*
/// window — the CEF window champ select renders in, class `RCLIENT`. Used to
/// anchor the overlay to the client instead of the monitor (the client is
/// usually windowed, not fullscreen). `None` if it isn't open/visible.
#[cfg(windows)]
pub fn league_client_rect() -> Option<(i32, i32, i32, i32)> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowRect, IsWindowVisible};
    unsafe {
        let hwnd = FindWindowW(w!("RCLIENT"), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        // A minimized/zero client reports a degenerate rect — reject it.
        if rect.right - rect.left < 200 || rect.bottom - rect.top < 200 {
            return None;
        }
        Some((rect.left, rect.top, rect.right, rect.bottom))
    }
}

/// Relaunch the current executable elevated (UAC prompt). The caller should
/// exit afterwards so the elevated instance takes over.
#[cfg(windows)]
pub fn relaunch_as_admin() {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::Win32::UI::Shell::ShellExecuteW;

    if let Ok(exe) = std::env::current_exe() {
        let mut file: Vec<u16> = exe.as_os_str().encode_wide().collect();
        file.push(0);
        unsafe {
            ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(file.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }
}

/// Open a URL in the default browser via `ShellExecuteW` "open". Caller must
/// validate the URL first (see `open_external_url` in `lib.rs`).
#[cfg(windows)]
pub fn open_in_browser(url: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let mut file: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().collect();
    file.push(0);
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Open a folder in Explorer via the same `ShellExecuteW` "open" verb as
/// `open_in_browser` — it isn't just for URLs, a directory path works too.
/// Used by the tray's "Open Mods Folder" item.
#[cfg(windows)]
pub fn open_folder(path: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let mut file: Vec<u16> = std::ffi::OsStr::new(path).encode_wide().collect();
    file.push(0);
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(file.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Free bytes available on the volume containing `path`, or `None` if the
/// query fails. `path` must be an existing directory.
#[cfg(windows)]
pub fn free_disk_space_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut free_available = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free_available as *mut u64), None, None).ok()?;
    }
    Some(free_available)
}

// Non-Windows fallbacks so the crate still type-checks off-Windows.
#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}
#[cfg(not(windows))]
pub fn open_in_browser(_url: &str) {}
#[cfg(not(windows))]
pub fn open_folder(_path: &str) {}
#[cfg(not(windows))]
pub fn lol_game_focused() -> bool {
    false
}
#[cfg(not(windows))]
pub fn league_client_rect() -> Option<(i32, i32, i32, i32)> {
    None
}
#[cfg(not(windows))]
pub fn relaunch_as_admin() {}
#[cfg(not(windows))]
pub fn free_disk_space_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}
