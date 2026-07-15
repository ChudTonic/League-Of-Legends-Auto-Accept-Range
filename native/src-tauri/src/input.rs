//! Keyboard injection for Auto-Range / Camera Assist via Win32 `SendInput`, sending
//! both scancode and virtual-key forms of every transition: DirectX titles read
//! scancodes, other paths read VKs, so sending both guarantees the game sees the
//! key. Operates openly — no hooking or evasion.

#[cfg(windows)]
mod imp {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    };

    /// Named keys the config accepts beyond single characters.
    fn named_vk(name: &str) -> Option<u16> {
        Some(match name {
            "space" => 0x20,
            "enter" | "return" => 0x0D,
            "tab" => 0x09,
            "esc" | "escape" => 0x1B,
            _ => return None,
        })
    }

    /// Resolve a config key name to (virtual key, scancode). `None` when the name
    /// is neither a known named key nor a resolvable character — callers treat
    /// that as a disarm (see `Injector::new`), never a silent default remap.
    pub fn resolve(name: &str) -> Option<(u16, u16)> {
        let normalized = name.trim().to_lowercase();
        let vk = named_vk(&normalized).or_else(|| {
            let ch = normalized.chars().next().filter(|_| normalized.chars().count() == 1)?;
            let scan_result = unsafe { VkKeyScanW(ch as u16) };
            (scan_result != -1).then_some((scan_result & 0xFF) as u16)
        })?;
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
        Some((vk, scan))
    }

    fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// Send both forms of one transition: scancode event + virtual-key event.
    pub fn transition(vk: u16, scan: u16, up: bool) {
        let up_flag = if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
        let inputs = [
            key_input(0, scan, KEYEVENTF_SCANCODE | up_flag),
            key_input(vk, 0, up_flag),
        ];
        unsafe {
            SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn resolve(_name: &str) -> Option<(u16, u16)> {
        Some((0, 0))
    }
    pub fn transition(_vk: u16, _scan: u16, _up: bool) {}
}

pub struct Injector {
    vk: u16,
    scan: u16,
    holding: bool,
}

impl Injector {
    pub fn new(key_name: &str) -> Option<Self> {
        imp::resolve(key_name).map(|(vk, scan)| Self { vk, scan, holding: false })
    }

    pub fn press(&mut self) {
        if !self.holding {
            imp::transition(self.vk, self.scan, false);
            self.holding = true;
        }
    }

    pub fn release(&mut self) {
        if self.holding {
            imp::transition(self.vk, self.scan, true);
            self.holding = false;
        }
    }

    /// Unconditional key-up regardless of tracked hold state, used on app exit so
    /// a key can't stay stuck if the process ends before a tool loop releases it.
    pub fn force_release(&mut self) {
        imp::transition(self.vk, self.scan, true);
        self.holding = false;
    }

    /// Brief up→down edge so League redraws the range indicator.
    pub fn refresh(&mut self) {
        if self.holding {
            imp::transition(self.vk, self.scan, true);
            std::thread::sleep(std::time::Duration::from_millis(30));
            imp::transition(self.vk, self.scan, false);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release(); // never leave the key stuck down
    }
}
