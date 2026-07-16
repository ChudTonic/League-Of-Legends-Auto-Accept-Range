//! Anonymous usage heartbeat — ships DARK (off unless `config.telemetry.enabled`).
//!
//! Sends ONLY a per-UTC-day rotating random id + a coarse `major.minor` version,
//! every 60s, fire-and-forget. No accounts, no summoner/LCU/game data, and the
//! server never stores IPs. The id regenerates every UTC day, so nothing links
//! one day to the next — the on-disk file only ever holds today's throwaway id.
//! Server + full privacy model: `telemetry-worker/SPEC.md` (deploy-side, gitignored).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::skins::paths;
use crate::{AppState, LockExt};

const ENDPOINT: &str = "https://chud-telemetry.jivy26.workers.dev/beat";
const HOST: &str = "chud-telemetry.jivy26.workers.dev";
const INTERVAL: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Default)]
struct DailyId {
    day: u64,
    id: String,
}

fn id_path() -> std::path::PathBuf {
    paths::state_dir().join("telemetry_id.json")
}

/// UTC-day bucket (days since epoch) — changes exactly at UTC midnight, no date
/// library needed.
fn utc_day() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 86_400).unwrap_or(0)
}

fn random_hex16() -> String {
    let mut buf = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Load or rotate the per-day random id. Regenerates when the UTC day changes;
/// the file only ever holds today's id (no cross-day link, no persistent secret).
fn daily_id() -> String {
    let today = utc_day();
    if let Ok(txt) = std::fs::read_to_string(id_path()) {
        if let Ok(d) = serde_json::from_str::<DailyId>(&txt) {
            if d.day == today && d.id.len() == 32 {
                return d.id;
            }
        }
    }
    let id = random_hex16();
    let _ = std::fs::create_dir_all(paths::state_dir());
    let _ = std::fs::write(id_path(), serde_json::to_string(&DailyId { day: today, id: id.clone() }).unwrap_or_default());
    id
}

/// Coarse `major.minor` — a version reveals nothing identifying in aggregate.
fn short_version() -> String {
    let v = env!("CARGO_PKG_VERSION");
    let mut it = v.split('.');
    match (it.next(), it.next()) {
        (Some(a), Some(b)) => format!("{a}.{b}"),
        _ => v.to_string(),
    }
}

/// Background loop: beat every 60s while enabled. Fire-and-forget — a failed or
/// slow request never affects the app. Re-reads the flag each tick so it can be
/// toggled without a restart. Ships dark: does nothing until `telemetry.enabled`.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let allowed: HashSet<String> = [HOST.to_string()].into_iter().collect();
        let client = crate::net::build_external_client(10.0, allowed);
        loop {
            let enabled = {
                let state = app.state::<Arc<AppState>>();
                let cfg = state.config.lock_safe();
                cfg.telemetry.enabled
            };
            if enabled {
                let body = serde_json::json!({ "id": daily_id(), "v": short_version() });
                let _ = client.post(ENDPOINT).json(&body).timeout(Duration::from_secs(8)).send().await;
            }
            tokio::time::sleep(INTERVAL).await;
        }
    });
}
