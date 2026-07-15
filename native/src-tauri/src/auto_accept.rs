//! Auto-Accept: poll the LCU gameflow phase and accept ready checks. Runs as a
//! tokio task controlled by `AppState::running`; pushes UI updates via events.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;

use crate::skins::slog::{log_info, log_warn};
use crate::{emit_state, lcu, lcu_ws, AppState, LockExt};

/// Exponential backoff: base * 2^min(errors,6), capped.
fn backoff(base: f64, errors: u32, cap: f64) -> f64 {
    let factor = 2f64.powi(errors.min(6) as i32);
    (base * factor).min(cap)
}

/// Consecutive transient failures to ride out at fast poll cadence before backing
/// off. A ready-check window is only ~10-13s, so backing off on the first error
/// risks sleeping through a queue pop — only escalate once failures persist.
const FAST_RETRY_GRACE: u32 = 4;

/// Sleep duration after a failed poll: fast (`check_interval`) for the first
/// `FAST_RETRY_GRACE` consecutive failures, then exponential backoff.
fn retry_sleep_secs(check_interval: f64, base: f64, errors: u32, cap: f64) -> f64 {
    if errors <= FAST_RETRY_GRACE {
        check_interval.max(0.2)
    } else {
        backoff(base, errors - FAST_RETRY_GRACE, cap)
    }
}

/// True while this loop instance is the current one: still toggled on AND not
/// superseded by a newer arm (generation bump).
fn current(state: &AppState, generation: u64) -> bool {
    state.running.load(Ordering::SeqCst)
        && state.auto_accept_gen.load(Ordering::SeqCst) == generation
}

/// Sleep in small chunks so a stop toggle (or a superseding re-arm) takes
/// effect within ~200ms even during a long backoff.
async fn sleep_interruptible(state: &AppState, generation: u64, secs: f64) {
    let mut remaining = secs;
    while remaining > 0.0 && current(state, generation) {
        let chunk = remaining.min(0.2);
        tokio::time::sleep(Duration::from_secs_f64(chunk)).await;
        remaining -= chunk;
    }
}

pub async fn run(app: AppHandle, state: Arc<AppState>, generation: u64) {
    let (check_interval, retry_delay, max_backoff, timeout) = {
        let c = state.config.lock_safe();
        (
            c.auto_accept.check_interval,
            c.auto_accept.retry_delay,
            c.auto_accept.max_backoff,
            c.lcu.request_timeout,
        )
    };
    let client = lcu::build_lcu_client(timeout);
    let mut auth: Option<lcu::Auth> = None;
    let mut errors: u32 = 0;
    state.readycheck_handled.store(false, Ordering::SeqCst);

    while current(&state, generation) {
        if auth.is_none() {
            // Shared cache with other subsystems, avoids each loop doing its own
            // process-table scan on LCU startup/reconnect.
            auth = lcu::cached_auth();
        }
        match auth.clone() {
            None => {
                state.client_online.store(false, Ordering::SeqCst);
                emit_state(&app, &state);
                errors = errors.saturating_add(1);
                sleep_interruptible(&state, generation, retry_sleep_secs(check_interval, retry_delay, errors, max_backoff)).await;
            }
            Some(a) => match lcu::get_phase(&client, &a).await {
                Some(phase) => {
                    errors = 0;
                    state.client_online.store(true, Ordering::SeqCst);
                    *state.phase.lock_safe() = phase.clone();

                    // WS task accepts the instant a ready check fires instead of at
                    // poll cadence; spawn slot prevents duplicates, cleared on drop.
                    if !state.ws_active.swap(true, Ordering::SeqCst) {
                        let ws_app = app.clone();
                        let ws_state = state.clone();
                        let ws_auth = a.clone();
                        tauri::async_runtime::spawn(async move {
                            lcu_ws::run(ws_app, ws_state.clone(), ws_auth, generation).await;
                            ws_state.ws_active.store(false, Ordering::SeqCst);
                        });
                    }

                    if phase == "ReadyCheck" {
                        if !state.readycheck_handled.load(Ordering::SeqCst) {
                            log_info!("[AUTO-ACCEPT] Ready check detected (poll) - accepting");
                            let accepted = lcu::accept_match(&client, &a).await;
                            if accepted && !state.readycheck_handled.swap(true, Ordering::SeqCst) {
                                log_info!("[AUTO-ACCEPT] Accepted ready check");
                                state.stats.lock_safe().record_accept();
                            } else if !accepted {
                                // Leave `readycheck_handled` false so the next poll retries.
                                log_warn!("[AUTO-ACCEPT] Accept request failed - retrying next poll");
                            }
                        }
                    } else {
                        state.readycheck_handled.store(false, Ordering::SeqCst);
                    }
                    emit_state(&app, &state);
                    sleep_interruptible(&state, generation, check_interval.max(0.2)).await;
                }
                None => {
                    // Client closed or restarted on a new lockfile port. Invalidate the
                    // SHARED cache (not just local `auth`) so the next poll re-reads the
                    // lockfile instead of re-fetching the same stale cached auth.
                    lcu::invalidate_auth();
                    auth = None;
                    state.client_online.store(false, Ordering::SeqCst);
                    emit_state(&app, &state);
                    errors = errors.saturating_add(1);
                    if errors == FAST_RETRY_GRACE + 1 {
                        log_warn!("[AUTO-ACCEPT] LCU unresponsive for {FAST_RETRY_GRACE} polls - backing off (client likely closed)");
                    }
                    sleep_interruptible(&state, generation, retry_sleep_secs(check_interval, retry_delay, errors, max_backoff)).await;
                }
            },
        }
    }

    // Stopped by the user — but only publish "offline" if we are still the
    // current loop; a superseded loop must not clobber its replacement's state.
    if state.auto_accept_gen.load(Ordering::SeqCst) == generation {
        state.client_online.store(false, Ordering::SeqCst);
        emit_state(&app, &state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_transient_failure_retries_fast_not_through_the_ready_check_window() {
        // Guards against backing off 10s on the first failure, which could sleep
        // through a ~10-13s ready check window.
        let (check, base, cap) = (1.0, 5.0, 30.0);
        for errors in 1..=FAST_RETRY_GRACE {
            assert_eq!(retry_sleep_secs(check, base, errors, cap), check, "failure #{errors} must retry fast");
        }
        // Past the grace window, backoff ramps from the base, not a huge first jump.
        assert_eq!(retry_sleep_secs(check, base, FAST_RETRY_GRACE + 1, cap), backoff(base, 1, cap));
        assert!(retry_sleep_secs(check, base, FAST_RETRY_GRACE + 3, cap) <= cap);
    }

    #[test]
    fn fast_retry_respects_the_200ms_floor() {
        assert_eq!(retry_sleep_secs(0.05, 5.0, 1, 30.0), 0.2);
    }
}
