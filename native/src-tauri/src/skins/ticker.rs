//! Loadout deadline ticker + skin-name resolution — ported from
//! `threads/utilities/timer_manager.py` (`TimerManager`),
//! `threads/utilities/loadout_ticker.py` (`LoadoutTicker`), and
//! `threads/utilities/skin_name_resolver.py` (`SkinNameResolver`).
//!
//! Generation-counter model (see `docs/SKINS_PORT.md` "Threading model"):
//! Python's `LoadoutTicker` was a daemon `threading.Thread` that checked
//! `state.current_ticker == self.ticker_id` at its own loop head — a poke
//! flag written by `TimerManager.maybe_start_timer` under `state.timer_lock`,
//! and used `self.ticker.is_alive()` to decide whether a prior ticker was
//! still running before spawning a replacement. Chud instead bumps
//! `SkinsState.ticker_gen` (an `AtomicU64`) every time a new ticker is armed;
//! each ticker task captures its own generation at spawn and self-exits the
//! moment a newer one supersedes it. That makes the `is_alive()` liveness
//! check unnecessary — `maybe_start_timer` here always spawns unconditionally
//! once its own preconditions are met, and the generation check is what turns
//! a stale prior ticker into a no-op instead of a second writer.

#![allow(dead_code)] // consumed by phase.rs wiring; S9 troubleshooting UI

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tauri::AppHandle;
use tokio::time::Instant as TokioInstant;

use crate::lcu;
use crate::skins::features::special;
use crate::skins::lcu_ext::{self, ChampionSkinCache, DEFAULT_CACHE_TTL};
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::state::{HistoricSelection, SkinsShared};
use crate::skins::{trigger, SkinsState};
use crate::LockExt;

// ---------------------------------------------------------------------
// Magic values preserved verbatim (config.py).
// ---------------------------------------------------------------------

/// `config.TIMER_HZ_DEFAULT`.
pub const TIMER_HZ_DEFAULT: u32 = 250;
/// `config.TIMER_HZ_MIN`.
pub const TIMER_HZ_MIN: u32 = 10;
/// `config.TIMER_HZ_MAX`.
pub const TIMER_HZ_MAX: u32 = 2000;
/// `config.TIMER_POLL_PERIOD_S` (0.2s) — cadence for the ticker's periodic
/// LCU resync.
pub const TIMER_POLL_PERIOD: Duration = Duration::from_millis(200);
/// `config.FALLBACK_LOADOUT_MS_DEFAULT`.
pub const FALLBACK_LOADOUT_MS_DEFAULT: i64 = 0;
/// `config.SKIN_THRESHOLD_MS_DEFAULT` — the ticker's own fallback if
/// `SkinsShared.skin_write_ms` is unset/zero (Python: `getattr(state,
/// 'skin_write_ms', SKIN_THRESHOLD_MS_DEFAULT) or SKIN_THRESHOLD_MS_DEFAULT`).
pub const SKIN_THRESHOLD_MS_DEFAULT: i64 = 300;
/// `config.WS_PROBE_ITERATIONS` — probe attempts when FINALIZATION's timer
/// value isn't ready yet.
pub const WS_PROBE_ITERATIONS: u32 = 8;
/// `config.WS_PROBE_SLEEP_MS` (60ms; `WS_PROBE_ITERATIONS * WS_PROBE_SLEEP_MS`
/// ~= 480ms probe window).
pub const WS_PROBE_SLEEP: Duration = Duration::from_millis(60);

/// Namespacing wrapper matching the fix list's `TimerManager::maybe_start_timer`
/// call shape. Unlike Python's `TimerManager`, this holds no state of its
/// own: the `self.ticker`/`is_alive()` liveness bookkeeping it used is
/// superseded by `SkinsState.ticker_gen` (see module doc comment) — there is
/// nothing left to store between calls, so a unit struct is enough to keep
/// the familiar `TimerManager::maybe_start_timer(...)` call shape at the
/// `phase.rs` call site.
pub struct TimerManager;

impl TimerManager {
    /// Start the loadout ticker if conditions are met — ONLY on FINALIZATION
    /// phase (ported from `TimerManager.maybe_start_timer`).
    ///
    /// `session` is the raw `/lol-champ-select/v1/session` JSON the caller
    /// (`phase.rs`, on observing FINALIZATION) already fetched — kept as
    /// `&Value` rather than a typed field on `lcu_ext::SessionData` because
    /// that struct doesn't model the session's `timer` sub-object, and
    /// `lcu_ext.rs` is out of this milestone's file scope to extend.
    pub async fn maybe_start_timer(app: AppHandle, skins: Arc<SkinsState>, session: &Value) {
        let mut phase_timer = timer_phase(session);
        let mut left_ms = timer_left_ms(session);

        if phase_timer != "FINALIZATION" {
            return;
        }

        {
            let mut shared = skins.shared.lock_safe();
            if shared.phase.as_deref() == Some("ChampSelect") {
                log_info!("[phase] Phase: FINALIZATION");
                shared.phase = Some("FINALIZATION".to_string());
            }
        }

        // If the timer value isn't ready yet, probe a few times
        // (WS_PROBE_ITERATIONS x WS_PROBE_SLEEP_MS ~= 480ms window).
        if left_ms <= 0 {
            if let Some(auth) = lcu::cached_auth() {
                let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);
                for _ in 0..WS_PROBE_ITERATIONS {
                    let Some(probe) = lcu_ext::shared_cache()
                        .get(&client, &auth, "/lol-champ-select/v1/session", DEFAULT_CACHE_TTL)
                        .await
                    else {
                        break;
                    };
                    phase_timer = timer_phase(&probe);
                    left_ms = timer_left_ms(&probe);
                    if phase_timer == "FINALIZATION" && left_ms > 0 {
                        break;
                    }
                    tokio::time::sleep(WS_PROBE_SLEEP).await;
                }
            }
        }

        if left_ms <= 0 {
            return;
        }

        let ticker_id = {
            let mut shared = skins.shared.lock_safe();
            if shared.loadout_countdown_active {
                return; // already running - mirrors Python's guard under `timer_lock`
            }
            shared.loadout_left0_ms = left_ms;
            shared.loadout_t0 = Some(std::time::Instant::now());
            shared.ticker_seq = shared.ticker_seq.wrapping_add(1);
            shared.current_ticker = shared.ticker_seq;
            shared.loadout_countdown_active = true;
            shared.current_ticker
        };

        log_info!(
            "[loadout] Ticker started #{ticker_id} (remaining {left_ms}ms / {:.3}s, hz={TIMER_HZ_DEFAULT}, phase=FINALIZATION)",
            left_ms as f64 / 1000.0
        );

        // Bump the generation so any (shouldn't-exist, but just in case)
        // prior ticker task exits on its next loop check instead of racing
        // this one.
        let generation = skins.ticker_gen.fetch_add(1, Ordering::SeqCst) + 1;
        tauri::async_runtime::spawn(async move {
            run_ticker(app, skins, ticker_id, generation).await;
        });
    }
}

fn timer_phase(session: &Value) -> String {
    session.get("timer").and_then(|t| t.get("phase")).and_then(Value::as_str).unwrap_or("").to_uppercase()
}

fn timer_left_ms(session: &Value) -> i64 {
    session.get("timer").and_then(|t| t.get("adjustedTimeLeftInPhase")).and_then(Value::as_i64).unwrap_or(0)
}

/// The ticker task itself (ported from `LoadoutTicker.run`).
async fn run_ticker(app: AppHandle, skins: Arc<SkinsState>, ticker_id: u64, generation: u64) {
    // Exit immediately if superseded before we even started (mirrors
    // `if state.current_ticker != self.ticker_id: return` at the top of
    // Python's `run`).
    if skins.ticker_gen.load(Ordering::SeqCst) != generation {
        return;
    }

    let hz = TIMER_HZ_DEFAULT.clamp(TIMER_HZ_MIN, TIMER_HZ_MAX);
    let tick_period = Duration::from_secs_f64(1.0 / hz as f64);

    let (left0_ms, t0) = {
        let shared = skins.shared.lock_safe();
        (shared.loadout_left0_ms, shared.loadout_t0)
    };
    let Some(t0) = t0 else { return };
    // Monotonic deadline (`tokio::time::Instant`, per the fix list).
    let mut deadline = TokioInstant::from_std(t0) + Duration::from_millis(left0_ms.max(0) as u64);

    let mut prev_remain_ms: i64 = 1_000_000_000;
    let mut last_poll: Option<TokioInstant> = None;
    let mut last_bucket: Option<i64> = None;

    let client = lcu::build_client(lcu_ext::LCU_API_TIMEOUT_S);

    loop {
        if skins.ticker_gen.load(Ordering::SeqCst) != generation {
            break; // superseded by a newer ticker - stale-invalidation
        }

        let (active, current, phase) = {
            let shared = skins.shared.lock_safe();
            (shared.loadout_countdown_active, shared.current_ticker, shared.phase.clone())
        };
        if !active || current != ticker_id || !matches!(phase.as_deref(), Some("ChampSelect") | Some("FINALIZATION")) {
            break;
        }

        let now = TokioInstant::now();
        let needs_resync = last_poll.map_or(true, |lp| now.duration_since(lp) >= TIMER_POLL_PERIOD);
        if needs_resync {
            last_poll = Some(now);
            if let Some(auth) = lcu::cached_auth() {
                if let Some(session) =
                    lcu_ext::shared_cache().get(&client, &auth, "/lol-champ-select/v1/session", DEFAULT_CACHE_TTL).await
                {
                    let phase_timer = timer_phase(&session);
                    let left_ms = timer_left_ms(&session);

                    if phase_timer == "FINALIZATION" {
                        let mut shared = skins.shared.lock_safe();
                        if shared.phase.as_deref() != Some("FINALIZATION") {
                            log_info!("[loadout] Phase transition detected: {:?} -> FINALIZATION", shared.phase);
                            shared.phase = Some("FINALIZATION".to_string());
                        }
                    }

                    if phase_timer == "FINALIZATION" && left_ms > 0 {
                        let candidate = TokioInstant::now() + Duration::from_millis(left_ms as u64);
                        if candidate < deadline {
                            deadline = candidate;
                        }
                    }
                }
            }
        }

        let now = TokioInstant::now();
        let mut remain_ms = if deadline > now { deadline.duration_since(now).as_millis() as i64 } else { 0 };
        // Anti-jitter clamp: never let the countdown appear to go backwards.
        if remain_ms > prev_remain_ms {
            remain_ms = prev_remain_ms;
        }
        prev_remain_ms = remain_ms;

        {
            let mut shared = skins.shared.lock_safe();
            shared.last_remain_ms = remain_ms;
        }

        let bucket = remain_ms / 1000;
        if Some(bucket) != last_bucket {
            last_bucket = Some(bucket);
            log_info!("[loadout #{ticker_id}] T-{bucket}s");
            // NOTE(seam): Python notified `injection_manager.on_loadout_countdown(seconds)`
            // here for tray/UI feedback. `InjectionManager` (S3, out of this
            // milestone's file scope) has no equivalent hook — logging only.
        }

        let (thresh, already_written) = {
            let shared = skins.shared.lock_safe();
            let thresh = if shared.skin_write_ms != 0 { shared.skin_write_ms } else { SKIN_THRESHOLD_MS_DEFAULT };
            (thresh, shared.last_hover_written)
        };

        if remain_ms <= thresh && !already_written {
            fire_injection(&app, &skins, &client, ticker_id).await;
        }

        if remain_ms <= 0 {
            break;
        }
        tokio::time::sleep(tick_period).await;
    }

    // End of ticker: only release if we're still the current ticker (mirrors
    // the matching guard at Python's thread exit).
    let mut shared = skins.shared.lock_safe();
    if shared.current_ticker == ticker_id {
        shared.loadout_countdown_active = false;
    }
}

/// Resolve the injection name + label and hand off to `trigger::trigger_injection`
/// once (ported from the `remain_ms <= thresh` body of `LoadoutTicker.run`).
async fn fire_injection(app: &AppHandle, skins: &Arc<SkinsState>, client: &reqwest::Client, ticker_id: u64) {
    let champ_id = {
        let shared = skins.shared.lock_safe();
        shared.locked_champ_id.or(shared.hovered_champ_id)
    };

    let cache = match (champ_id, lcu::cached_auth()) {
        (Some(cid), Some(auth)) => lcu_ext::scrape_champion_skins(client, &auth, cid).await,
        _ => None,
    };

    let (name, champion_name, label) = {
        let shared = skins.shared.lock_safe();
        let label = build_skin_label(&shared, cache.as_ref());
        let name = resolve_injection_name(&shared, cache.as_ref());
        let cname = cache.as_ref().and_then(|c| c.champion_name.clone()).unwrap_or_default();
        (name, cname, label)
    };

    log_info!("[INJECT] Skin label: {label:?}");
    log_info!("[INJECT] Final name variable: {name:?}");

    // Always hand off to `trigger_injection`, even with no own skin resolved:
    // an empty name now routes to a party-only injection (peer skins alone) so
    // keeping your default skin no longer drops every teammate's skin. When
    // there's also no party skin, `trigger_injection` logs the skip itself.
    trigger::trigger_injection(app.clone(), skins.clone(), ticker_id, name.unwrap_or_default(), champion_name).await;
}

/// Universal game-launch injection entry for modes whose champ-select has no
/// FINALIZATION loadout countdown (Practice Tool, and as a safety net for any
/// mode), so the loadout ticker never armed. Called on the GameStart phase and
/// — last resort — on InProgress. Guarded by `last_hover_written` so a game
/// whose ticker already fired never injects twice. The injection manager arms
/// the game monitor itself, which suspends League as it launches so the overlay
/// builds before champion assets load.
pub(crate) async fn inject_for_game(app: &AppHandle, skins: &Arc<SkinsState>, client: &reqwest::Client) {
    let (already, has_champ) = {
        let s = skins.shared.lock_safe();
        (s.last_hover_written, s.locked_champ_id.or(s.hovered_champ_id).is_some())
    };
    if already {
        return; // the loadout ticker (or an earlier phase) already triggered this game
    }
    if !has_champ {
        return; // nothing locked/hovered to inject for
    }
    let id = skins.ticker_gen.fetch_add(1, Ordering::SeqCst) + 1;
    log_info!("[INJECT] Game-launch injection trigger #{id} (no loadout countdown)");
    fire_injection(app, skins, client, id).await;
}

// ---------------------------------------------------------------------
// Skin name resolution — ported from `SkinNameResolver`.
// ---------------------------------------------------------------------

/// `utils.core.utilities.is_chroma_id`: hardcoded special (forms/HOL) IDs
/// always count as a chroma, falling back to real `chroma_id_map` membership
/// otherwise.
fn is_chroma_id(skin_id: i64, cache: Option<&ChampionSkinCache>) -> bool {
    special::is_special_id(skin_id) || cache.is_some_and(|c| c.is_chroma(skin_id))
}

/// `SkinNameResolver.resolve_injection_name` — historic > random > hovered
/// priority, `"skin_{id}"`/`"chroma_{id}"` token format.
///
/// RECONCILED (was "DEVIATION flagged for the lead"): `SkinsShared` now has
/// `historic_selection: Option<HistoricSelection>` (was `historic_skin_id:
/// Option<i64>`), so the `"path:..."` custom-mod case has a home. Ported from
/// `skin_name_resolver.py`'s custom-mod branch: extract the base skin ID from
/// the mod's `"skins/{skin_id}/..."` relative path (falling back to the
/// locked/hovered champion's base skin on a malformed path) and return
/// `"skin_{id}"` — same as Python, this only resolves a NAME for the base
/// skin ZIP; the actual custom-mod file is picked up independently by
/// `trigger::auto_select_historic_custom_mod`, which re-reads `historic.json`
/// itself and populates `selected_custom_mod` before injection runs, so both
/// paths end up flowing through the one custom-mod injection code path in
/// `trigger.rs` (no parallel copy here).
pub fn resolve_injection_name(shared: &SkinsShared, cache: Option<&ChampionSkinCache>) -> Option<String> {
    // Historic mode override.
    if shared.historic_mode_active {
        match &shared.historic_selection {
            Some(HistoricSelection::SkinId(hist_id)) => {
                let hist_id = *hist_id;
                // Python's historic/random branches check plain `chroma_id_map`
                // membership (no special-forms fallback) — NOT `is_base_skin`'s
                // richer `is_chroma_id`. Preserved verbatim: only the hovered-skin
                // branch below uses `is_chroma_id`.
                let is_chroma = cache.is_some_and(|c| c.is_chroma(hist_id));
                let name = if is_chroma { format!("chroma_{hist_id}") } else { format!("skin_{hist_id}") };
                log_info!("[HISTORIC] Using historic {} ID for injection: {hist_id}", if is_chroma { "chroma" } else { "skin" });
                return Some(name);
            }
            Some(HistoricSelection::CustomMod(path)) => {
                log_info!("[HISTORIC] Using historic custom mod path for injection: {path}");
                let normalized = path.replace('\\', "/");
                let mut parts = normalized.splitn(3, '/');
                if let (Some("skins"), Some(id_str)) = (parts.next(), parts.next()) {
                    if let Ok(base_skin_id) = id_str.parse::<i64>() {
                        log_info!("[HISTORIC] Extracted base skin ID {base_skin_id} from mod path, returning: skin_{base_skin_id}");
                        return Some(format!("skin_{base_skin_id}"));
                    }
                }
                log_warn!("[HISTORIC] Invalid mod path format, expected 'skins/{{skin_id}}/...': {path}");
                // Fallback: the locked/hovered champion's base skin.
                if let Some(champ_id) = shared.locked_champ_id.or(shared.hovered_champ_id) {
                    let base_skin_id = champ_id * 1000;
                    log_info!("[HISTORIC] Fallback: Returning default skin for custom mod injection: skin_{base_skin_id}");
                    return Some(format!("skin_{base_skin_id}"));
                }
                log_warn!("[HISTORIC] No champion ID available for custom mod path");
                return None;
            }
            None => {}
        }
    }

    // Random mode.
    if shared.random_mode_active {
        if let Some(random_id) = shared.random_skin_id {
            let is_chroma = cache.is_some_and(|c| c.is_chroma(random_id));
            let name = if is_chroma { format!("chroma_{random_id}") } else { format!("skin_{random_id}") };
            log_info!("[RANDOM] Injecting random {}: (ID: {random_id})", if is_chroma { "chroma" } else { "skin" });
            return Some(name);
        }
        log_error!("[RANDOM] No random skin ID available for injection");
        return None;
    }

    // Favorite-skin fallback: apply the champ's saved favorite whenever the
    // player hasn't manually picked a non-base skin this game (see favorites.rs).
    let favorite = shared.active_favorite_skin_id;

    // Normal hovered skin.
    if let Some(skin_id) = shared.last_hovered_skin_id {
        // "On base skin" = the client reports the champion's base (num 0) skin,
        // i.e. no manual pick — that's when the favorite takes over.
        let champ_base = shared.locked_champ_id.or(shared.hovered_champ_id).map(|c| c * 1000);
        if Some(skin_id) == champ_base {
            if let Some(fav) = favorite.filter(|f| *f != skin_id) {
                let is_base = !is_chroma_id(fav, cache);
                log_info!("[FAVORITES] On base skin — applying favorite skin {fav} instead");
                return Some(if is_base { format!("skin_{fav}") } else { format!("chroma_{fav}") });
            }
        }
        let is_base = !is_chroma_id(skin_id, cache);
        return Some(if is_base { format!("skin_{skin_id}") } else { format!("chroma_{skin_id}") });
    }

    // Nothing hovered yet — still honor the favorite if one is armed.
    if let Some(fav) = favorite {
        let is_base = !is_chroma_id(fav, cache);
        log_info!("[FAVORITES] No manual pick — applying favorite skin {fav}");
        return Some(if is_base { format!("skin_{fav}") } else { format!("chroma_{fav}") });
    }

    None
}

/// `SkinNameResolver.build_skin_label` — clean skin label for logging.
///
/// The Python original's `base.replace(" ", " ").replace("'", "'")` calls
/// were investigated byte-for-byte (flagged for the lead as a "suspicious
/// no-op" to check): both quoted operands of BOTH `.replace()` calls are the
/// exact same plain ASCII character (0x20 space / 0x27 straight apostrophe)
/// on each side — this is not a hidden U+00A0 non-breaking-space or curly-quote
/// (U+2019) normalization defeated by a copy-paste bug, it is a byte-identical
/// no-op with no hidden intent recoverable from the source. Omitted here
/// rather than transcribed as literal dead code.
pub fn build_skin_label(shared: &SkinsShared, cache: Option<&ChampionSkinCache>) -> Option<String> {
    let raw = shared
        .last_hovered_skin_key
        .clone()
        .or_else(|| shared.last_hovered_skin_slug.clone())
        .or_else(|| shared.last_hovered_skin_id.map(|id| id.to_string()))?;

    let champ_id = shared.locked_champ_id.or(shared.hovered_champ_id);
    let champion_loaded = champ_id.is_some_and(|cid| cache.is_some_and(|c| c.is_loaded_for_champion(cid)));
    let champion_name = if champion_loaded { cache.and_then(|c| c.champion_name.clone()).unwrap_or_default() } else { String::new() };

    let mut base = if champion_loaded {
        shared
            .last_hovered_skin_id
            .and_then(|id| cache.and_then(|c| c.get_skin_by_id(id)))
            .map(|s| s.skin_name.trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    if base.is_empty() {
        base = raw.trim().to_string();
    }

    // Remove a champion-name prefix/suffix if present (case-insensitive;
    // League champion names are ASCII so byte-length matches after lowercasing).
    if !champion_name.is_empty() {
        let champ_lower = champion_name.to_lowercase();
        let base_lower = base.to_lowercase();
        if base_lower.starts_with(&format!("{champ_lower} ")) {
            base = base[champion_name.len() + 1..].trim_start().to_string();
        } else if base_lower.ends_with(&format!(" {champ_lower}")) {
            let cut = base.len().saturating_sub(champion_name.len() + 1);
            base = base[..cut].trim_end().to_string();
        }
    }

    // Don't re-append the champion name if it's already a whole word in the label.
    let already_included =
        !champion_name.is_empty() && base.to_lowercase().split_whitespace().any(|w| w == champion_name.to_lowercase());
    let label = if already_included || champion_name.is_empty() {
        base.trim().to_string()
    } else {
        format!("{base} {champion_name}").trim().to_string()
    };
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skins::lcu_ext::{ChromaInfo, SkinInfo};

    fn cache_with_chroma(champion_id: i64, base_skin_id: i64, chroma_id: i64) -> ChampionSkinCache {
        let mut cache = ChampionSkinCache { champion_id: Some(champion_id), champion_name: Some("Ahri".into()), ..Default::default() };
        let base = SkinInfo { skin_id: base_skin_id, champion_id, skin_name: "Base".to_string(), ..Default::default() };
        cache.skin_id_map.insert(base_skin_id, base.clone());
        cache.skins.push(base);
        cache.chroma_id_map.insert(chroma_id, ChromaInfo { id: chroma_id, skin_id: base_skin_id, ..Default::default() });
        cache
    }

    #[test]
    fn resolve_injection_name_priority_historic_over_random_over_hovered() {
        let cache = cache_with_chroma(103, 103000, 103001);

        let mut shared = SkinsShared::default();
        shared.last_hovered_skin_id = Some(103000);
        assert_eq!(resolve_injection_name(&shared, Some(&cache)), Some("skin_103000".to_string()));

        shared.random_mode_active = true;
        shared.random_skin_id = Some(103001);
        assert_eq!(resolve_injection_name(&shared, Some(&cache)), Some("chroma_103001".to_string()));

        shared.historic_mode_active = true;
        shared.historic_selection = Some(HistoricSelection::SkinId(103000));
        assert_eq!(resolve_injection_name(&shared, Some(&cache)), Some("skin_103000".to_string()));
    }

    #[test]
    fn resolve_injection_name_historic_custom_mod_extracts_base_skin_id_from_path() {
        let mut shared = SkinsShared::default();
        shared.historic_mode_active = true;
        shared.historic_selection = Some(HistoricSelection::CustomMod("skins/234000/old-aatrox-viego_1.2.0.fantome".to_string()));
        assert_eq!(resolve_injection_name(&shared, None), Some("skin_234000".to_string()));
    }

    #[test]
    fn resolve_injection_name_historic_custom_mod_falls_back_to_champion_base_skin_on_malformed_path() {
        let mut shared = SkinsShared::default();
        shared.historic_mode_active = true;
        shared.locked_champ_id = Some(103);
        shared.historic_selection = Some(HistoricSelection::CustomMod("not-a-skins-path.fantome".to_string()));
        assert_eq!(resolve_injection_name(&shared, None), Some("skin_103000".to_string()));
    }

    #[test]
    fn resolve_injection_name_historic_custom_mod_returns_none_without_champion_on_malformed_path() {
        let mut shared = SkinsShared::default();
        shared.historic_mode_active = true;
        shared.historic_selection = Some(HistoricSelection::CustomMod("not-a-skins-path.fantome".to_string()));
        assert_eq!(resolve_injection_name(&shared, None), None);
    }

    #[test]
    fn resolve_injection_name_hovered_uses_special_forms_table_for_chroma_detection() {
        // 99991 (Elementalist Lux Air fake ID) has no cache entry at all, but
        // is a hardcoded special ID -> must resolve as a chroma, unlike the
        // historic/random branches which only check `chroma_id_map` membership.
        let mut shared = SkinsShared::default();
        shared.last_hovered_skin_id = Some(99991);
        assert_eq!(resolve_injection_name(&shared, None), Some("chroma_99991".to_string()));
    }

    #[test]
    fn resolve_injection_name_random_mode_without_id_logs_and_returns_none() {
        let mut shared = SkinsShared::default();
        shared.random_mode_active = true;
        shared.random_skin_id = None;
        assert_eq!(resolve_injection_name(&shared, None), None);
    }

    #[test]
    fn resolve_injection_name_returns_none_when_nothing_hovered() {
        let shared = SkinsShared::default();
        assert_eq!(resolve_injection_name(&shared, None), None);
    }

    #[test]
    fn build_skin_label_strips_champion_prefix_and_avoids_double_append() {
        let cache = {
            let mut c = ChampionSkinCache { champion_id: Some(103), champion_name: Some("Ahri".into()), ..Default::default() };
            let skin = SkinInfo { skin_id: 103000, champion_id: 103, skin_name: "Ahri Base".to_string(), ..Default::default() };
            c.skin_id_map.insert(103000, skin.clone());
            c.skins.push(skin);
            c
        };
        let mut shared = SkinsShared::default();
        shared.locked_champ_id = Some(103);
        shared.last_hovered_skin_id = Some(103000);
        shared.last_hovered_skin_key = Some("Ahri Base".to_string());
        assert_eq!(build_skin_label(&shared, Some(&cache)), Some("Base Ahri".to_string()));
    }

    #[test]
    fn build_skin_label_none_when_nothing_hovered() {
        let shared = SkinsShared::default();
        assert_eq!(build_skin_label(&shared, None), None);
    }
}
