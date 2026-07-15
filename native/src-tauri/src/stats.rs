//! Match/session statistics. Persisted to the per-user data dir. Single process,
//! so no cross-process file lock is needed.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub total_matches_accepted: u64,
    /// Reset to 0 each launch (see `start_session`).
    pub session_matches_accepted: u64,
    /// Unix seconds when the current session started (0 = not started).
    pub session_start: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self { total_matches_accepted: 0, session_matches_accepted: 0, session_start: 0 }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Per-user data directory (also used for debug captures).
pub fn data_dir() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("com", "LeagueOfAndi", "LeagueOfLegendsTools") {
        return dirs.data_dir().to_path_buf();
    }
    PathBuf::from(".")
}

fn stats_path() -> PathBuf {
    data_dir().join("statistics.json")
}

impl Stats {
    pub fn load() -> Self {
        match std::fs::read_to_string(stats_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Stats::default(),
        }
    }

    pub fn save(&self) {
        let path = stats_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Reset session counters at launch (keeps the lifetime total).
    pub fn start_session(&mut self) {
        self.session_matches_accepted = 0;
        self.session_start = now_secs();
        self.save();
    }

    pub fn record_accept(&mut self) {
        self.total_matches_accepted += 1;
        self.session_matches_accepted += 1;
        self.save();
    }

    /// Human-readable uptime, e.g. "2h 14m 06s".
    pub fn uptime(&self) -> String {
        if self.session_start == 0 {
            return "0s".into();
        }
        let secs = now_secs().saturating_sub(self.session_start);
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        if h > 0 {
            format!("{h}h {m:02}m {s:02}s")
        } else if m > 0 {
            format!("{m}m {s:02}s")
        } else {
            format!("{s}s")
        }
    }
}
