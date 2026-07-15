//! Thin Windows helpers: admin check, League-game focus detection, and
//! relaunch-as-administrator (for the injection tools).

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
        String::from_utf16_lossy(&buf[..len as usize]) == "RiotWindowClass"
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

/// Open a URL in the user's default browser via the shell (`ShellExecuteW`
/// "open"). Used for the "View on RuneForge" attribution link — the caller
/// must validate the URL first (see `open_external_url` in `lib.rs`).
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

// Non-Windows fallbacks so the crate still type-checks off-Windows.
#[cfg(not(windows))]
pub fn is_admin() -> bool {
    false
}
#[cfg(not(windows))]
pub fn open_in_browser(_url: &str) {}
#[cfg(not(windows))]
pub fn lol_game_focused() -> bool {
    false
}
#[cfg(not(windows))]
pub fn relaunch_as_admin() {}
