//! Auto-Range: hold the "show range" key while a live League game is focused,
//! refreshing periodically so the indicator stays drawn. Two parts:
//!   * a dedicated thread that does the timing-sensitive hold/refresh, and
//!   * an async monitor that flips `injection_blocked` during a ranked game
//!     (the anti-cheat kill-switch).
//! The key is released whenever the game loses focus or a ranked game is
//! detected. Operates openly — no evasion.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::{emit_state, input::Injector, lcu, safety, winutil, AppState, LockExt};

pub fn start(app: AppHandle, state: Arc<AppState>, generation: u64) {
    // Start the global chat-key listener once (it can't be cleanly stopped).
    if !state.chat_listener_started.swap(true, Ordering::SeqCst) {
        start_chat_listener(state.clone());
    }
    {
        let state = state.clone();
        tauri::async_runtime::spawn(async move { ranked_monitor(state, generation).await });
    }
    std::thread::spawn(move || hold_loop(app, state, generation));
}

/// Track in-game chat open/close so we release the key while the user types
/// (otherwise the held/refreshed range key would spam characters into chat).
/// Enter toggles chat, Esc closes it — only counted while the game is focused.
///
/// CRITICAL: this runs inside `rdev`'s low-level Windows keyboard hook
/// (`WH_KEYBOARD_LL`), which is called synchronously on the OS input thread for
/// EVERY keystroke system-wide. It must return almost instantly and must NOT
/// make cross-process Win32 calls (e.g. `GetClassNameW` on the game window) —
/// doing so can stall all keyboard input for the whole machine if the game's
/// UI thread is busy (loading screen, alt-tab, Vanguard). So instead of probing
/// focus here, we read `state.game_focused`, which the tool loops publish every
/// tick. The hook therefore only touches lock-free atomics.
pub(crate) fn start_chat_listener(state: Arc<AppState>) {
    std::thread::spawn(move || {
        // Windows key auto-repeat delivers repeated KeyPress(Return) events to
        // the hook while Enter is held down, which would otherwise toggle
        // chat_open on every repeat instead of once per press. Track the
        // down/up edge so only the first press of a hold toggles.
        let enter_down = std::cell::Cell::new(false);
        let callback = move |event: rdev::Event| {
            // Handled before the active gate so a release while disarmed is
            // never missed (which would otherwise wedge the edge "down").
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

async fn ranked_monitor(state: Arc<AppState>, generation: u64) {
    let (block_enabled, timeout, interval) = {
        let c = state.config.lock_safe();
        (c.safety.block_in_ranked, c.lcu.request_timeout, c.safety.check_interval)
    };
    if !block_enabled {
        return;
    }
    let client = lcu::build_client(timeout);
    while state.auto_range_running.load(Ordering::SeqCst)
        && state.auto_range_gen.load(Ordering::SeqCst) == generation
    {
        // Reuse the shared cached auth (avoids a full process scan each tick).
        let block = match lcu::cached_auth() {
            Some(auth) => match lcu::gameflow_session(&client, &auth).await {
                Some(session) => safety::should_block(&session),
                None => {
                    lcu::invalidate_auth();
                    false // client unreachable -> not in a game -> don't block
                }
            },
            None => false,
        };
        state.injection_blocked.store(block, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs_f64(interval.max(1.0))).await;
    }
    // Auto-Range is the only injection tool now — always clear on exit.
    state.injection_blocked.store(false, Ordering::SeqCst);
}
