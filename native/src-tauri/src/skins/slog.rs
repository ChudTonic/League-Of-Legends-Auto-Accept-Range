//! Skins subsystem file logger. Never-blocks-the-caller design, ported from
//! `utils\core\logging.py`'s queue handler (minus its three-tier
//! customer/verbose/debug complexity — no config surface for log modes
//! yet). One bounded channel + one writer thread; `try_send` means a caller
//! never stalls even if the writer is behind — overflow is silently dropped.

#![allow(dead_code)] // consumed by S2+
#![allow(unused_macros)] // info!/warn!/error! land their first call sites in S2+

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_ROTATIONS: u32 = 3;
const CHANNEL_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

static SENDER: OnceLock<SyncSender<String>> = OnceLock::new();

/// Start the writer thread against `logs_dir/chud_skins_{timestamp}.log`.
/// Safe to call more than once (later calls no-op) — matches `skins::init`
/// being callable defensively.
pub fn init(logs_dir: &Path) {
    if SENDER.get().is_some() {
        return;
    }
    let _ = std::fs::create_dir_all(logs_dir);
    let path = log_file_path(logs_dir);
    let (tx, rx) = sync_channel::<String>(CHANNEL_CAPACITY);
    if SENDER.set(tx).is_err() {
        return; // lost an init race — the other caller's writer thread owns it
    }
    let _ = std::thread::Builder::new().name("chud-skins-log".into()).spawn(move || writer_loop(path, rx));
}

/// Queue a log line. Drops silently (never blocks) if the writer is behind
/// or logging hasn't been initialized yet.
pub fn log(level: Level, msg: &str) {
    let Some(tx) = SENDER.get() else { return };
    let (h, mi, s, ms) = local_time_hms_ms();
    let line = format!("{h:02}:{mi:02}:{s:02}.{ms:03} | {:<5} | {msg}", level.tag());
    let _ = tx.try_send(line);
}

// Named `log_info!`/`log_warn!`/`log_error!` rather than bare `info!`/
// `warn!`/`error!` — the latter collides with the built-in `warn` attribute
// macro (E0659 ambiguous name) when re-exported via `pub(crate) use`.
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::skins::slog::log($crate::skins::slog::Level::Info, &format!($($arg)*)) };
}
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::skins::slog::log($crate::skins::slog::Level::Warn, &format!($($arg)*)) };
}
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::skins::slog::log($crate::skins::slog::Level::Error, &format!($($arg)*)) };
}
#[allow(unused_imports)] // first call sites land in S2+
pub(crate) use log_error;
#[allow(unused_imports)]
pub(crate) use log_info;
#[allow(unused_imports)]
pub(crate) use log_warn;

fn writer_loop(path: PathBuf, rx: Receiver<String>) {
    let mut file = open_append(&path);
    while let Ok(line) = rx.recv() {
        if let Some(f) = file.as_mut() {
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            if size >= MAX_BYTES {
                drop(file.take());
                rotate(&path);
                file = open_append(&path);
            }
        }
        if let Some(f) = file.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Rotate `base` -> `base.1`, shifting `.1` -> `.2`, `.2` -> `.3` first, and
/// dropping anything beyond `.3` (max 3 kept; no delete-on-rotation retention
/// policy — that's `cleanup_old_logs`'s job).
fn rotate(base: &Path) {
    for i in (1..MAX_ROTATIONS).rev() {
        let from = rotated_path(base, i);
        let to = rotated_path(base, i + 1);
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    let _ = std::fs::rename(base, rotated_path(base, 1));
}

fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut name = base.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    base.with_file_name(name)
}

fn log_file_path(logs_dir: &Path) -> PathBuf {
    logs_dir.join(format!("chud_skins_{}.log", timestamp_compact()))
}

/// Delete `chud_skins_*.log*` files older than 24h (ported from
/// `logging.py::cleanup_logs`'s age-only retention policy).
pub fn cleanup_old_logs(logs_dir: &Path) {
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(24 * 60 * 60)) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(logs_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !(name.starts_with("chud_skins_") && name.contains(".log")) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

fn timestamp_compact() -> String {
    let (y, mo, d, h, mi, s, _ms) = local_now();
    format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")
}

fn local_time_hms_ms() -> (u16, u16, u16, u16) {
    let (_, _, _, h, mi, s, ms) = local_now();
    (h, mi, s, ms)
}

/// (year, month, day, hour, minute, second, millisecond) in local time.
#[cfg(windows)]
fn local_now() -> (u16, u16, u16, u16, u16, u16, u16) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    (st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds)
}

#[cfg(not(windows))]
fn local_now() -> (u16, u16, u16, u16, u16, u16, u16) {
    (1970, 1, 1, 0, 0, 0, 0)
}
