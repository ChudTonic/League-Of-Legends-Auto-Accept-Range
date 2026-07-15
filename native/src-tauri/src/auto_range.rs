//! Auto-Range: hold the "show range" key while a live League game is focused,
//! refreshing periodically so the indicator stays drawn. A dedicated thread does
//! the timing-sensitive hold/refresh; the ranked kill-switch it obeys
//! (`AppState::injection_blocked`) is maintained by the ALWAYS-RUNNING safety
//! monitor (`safety_manager::spawn_safety_monitor`), not by this module — ranked
//! detection must not depend on Auto-Range being armed. Key releases on focus
//! loss or ranked detection. Operates openly — no evasion.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::{emit_state, input::Injector, winutil, AppState, LockExt};

pub fn start(app: AppHandle, state: Arc<AppState>, generation: u64) {
    // Start the global chat-key listener once (it can't be cleanly stopped).
    if !state.chat_listener_started.swap(true, Ordering::SeqCst) {
        start_chat_listener(state.clone());
    }
    std::thread::spawn(move || hold_loop(app, state, generation));
}

/// Track in-game chat open/close so the range key releases while the user types.
/// Enter toggles chat, Esc closes it — only while the game is focused.
///
/// SAFETY: runs inside `rdev`'s low-level Windows keyboard hook (`WH_KEYBOARD_LL`),
/// called synchronously on the OS input thread for every keystroke system-wide.
/// Must return instantly and must NOT make cross-process Win32 calls (e.g.
/// `GetClassNameW`) — that can stall system-wide input if the game's UI thread is
/// busy. So it reads `state.game_focused` (published by the tool loops) instead of
/// probing focus itself — lock-free atomics only.
pub(crate) fn start_chat_listener(state: Arc<AppState>) {
    std::thread::spawn(move || {
        // Track the down/up edge so key-repeat doesn't re-toggle chat_open on
        // every repeated KeyPress(Return) while Enter is held.
        let enter_down = std::cell::Cell::new(false);
        let callback = move |event: rdev::Event| {
            // Checked before the active gate so a release while disarmed isn't
            // missed, which would wedge the edge "down".
            if let rdev::EventType::KeyRelease(rdev::Key::Return) = event.event_type {
                enter_down.set(false);
            }
            // Only react while an injection tool that cares about chat is armed.
            if !state.auto_range_running.load(Ordering::SeqCst) {
                return;
            }
            match event.event_type {
                rdev::EventType::KeyPress(rdev::Key::Return) => {
                    if !enter_down.get() {
                        // Atomic read only — no Win32 in the hook (see doc above).
                        if state.game_focused.load(Ordering::SeqCst) {
                            let now = state.chat_open.load(Ordering::SeqCst);
                            state.chat_open.store(!now, Ordering::SeqCst);
                        }
                    }
                    enter_down.set(true);
                }
                rdev::EventType::KeyPress(rdev::Key::Escape) => {
                    state.chat_open.store(false, Ordering::SeqCst);
                }
                _ => {}
            }
        };
        let _ = rdev::listen(callback);
    });
}

/// Read the live Auto-Range params from config. Re-read when `config_gen`
/// changes so Settings edits apply without re-arming.
fn read_params(state: &AppState) -> (String, f64, f64) {
    let c = state.config.lock_safe();
    (c.autorange.range_hold_key.clone(), c.autorange.refresh_interval, c.autorange.tick_sec)
}

fn hold_loop(app: AppHandle, state: Arc<AppState>, generation: u64) {
    let (mut key_name, mut refresh_every, mut tick) = read_params(&state);
    let mut cfg_seen = state.config_gen.load(Ordering::SeqCst);
    let mut injector = match Injector::new(&key_name) {
        Some(i) => i,
        None => {
            state.auto_range_running.store(false, Ordering::SeqCst);
            emit_state(&app, &state);
            return;
        }
    };
    let mut last_refresh = Instant::now();

    // Exit when disarmed OR superseded by a newer arm (generation bump), so a
    // stale duplicate loop can never fight the current one over the key.
    while state.auto_range_running.load(Ordering::SeqCst)
        && state.auto_range_gen.load(Ordering::SeqCst) == generation
    {
        // Live-reload config if it changed. Rebuild the injector only if the
        // key actually changed (releasing the old key first).
        let cfg_now = state.config_gen.load(Ordering::SeqCst);
        if cfg_now != cfg_seen {
            cfg_seen = cfg_now;
            let (k, r, t) = read_params(&state);
            refresh_every = r;
            tick = t;
            if k != key_name {
                injector.release();
                if let Some(i) = Injector::new(&k) {
                    injector = i;
                    key_name = k;
                    last_refresh = Instant::now() - Duration::from_secs_f64(refresh_every);
                }
            }
        }

        let focused = winutil::lol_game_focused();
        // Publish focus for the chat hook (which must not call Win32 itself).
        state.game_focused.store(focused, Ordering::SeqCst);
        if !focused {
            state.chat_open.store(false, Ordering::SeqCst); // reset on focus loss
        }
        let need_hold = focused
            && !state.injection_blocked.load(Ordering::SeqCst)
            && !state.chat_open.load(Ordering::SeqCst);
        if need_hold {
            injector.press();
            if last_refresh.elapsed().as_secs_f64() >= refresh_every {
                injector.refresh();
                last_refresh = Instant::now();
            }
        } else {
            injector.release();
        }

        // Fast tick only while in-game; idle back-off otherwise to save CPU.
        let sleep = if focused { tick.max(0.01) } else { 0.25 };
        std::thread::sleep(Duration::from_secs_f64(sleep));
    }
    // injector's Drop releases the key.
    state.game_focused.store(false, Ordering::SeqCst);
    emit_state(&app, &state);
}
