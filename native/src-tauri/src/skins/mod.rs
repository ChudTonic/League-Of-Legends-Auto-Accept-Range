//! Skins subsystem: LCU skin features, injection pipeline, Pengu Loader
//! bridge, and party mode — see `docs/SKINS_PORT.md` for architecture/provenance.
//! S1 ships only the foundation (paths/logging/config/state); later milestones build on it.

#![allow(dead_code)] // consumed by S2+

pub mod announcer_fix;
pub mod announcer_studio;
pub mod bridge;
pub mod downloads;
pub mod favorites;
pub mod features;
pub mod injection;
pub mod lcu_ext;
pub mod mod_scope;
pub mod party;
pub mod paths;
pub mod pengu;
pub mod phase;
pub mod skin_db;
pub mod slog;
pub mod state;
pub mod swiftplay;
pub mod ticker;
pub mod trigger;

use std::sync::atomic::AtomicU64;
use std::sync::Mutex;

/// All mutable skins state, one coarse Mutex (see `docs/SKINS_PORT.md`
/// "Threading model") — simpler than the Python original's 5-thread GIL races. Split later if contention shows.
pub struct SkinsState {
    pub shared: Mutex<state::SkinsShared>,
    /// Bumped by the phase actor (S2) so stale in-flight loops exit instead of
    /// racing the current one — same pattern as `lib.rs`'s `AppState`.
    pub phase_gen: AtomicU64,
    /// Bumped on each loadout-ticker (re)arm (S5) for the same reason.
    pub ticker_gen: AtomicU64,
}

impl SkinsState {
    pub fn new() -> Self {
        let mut shared = state::SkinsShared::default();
        shared.favorite_skins = favorites::load();
        Self { shared: Mutex::new(shared), phase_gen: AtomicU64::new(0), ticker_gen: AtomicU64::new(0) }
    }
}

impl Default for SkinsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Bring up the skins data-dir tree and file logger. Call once at app start;
/// non-fatal on failure — caller `eprintln`'s and continues, subsystem just stays unavailable.
pub fn init() -> std::io::Result<()> {
    paths::ensure_tree()?;
    slog::init(&paths::logs_dir());
    slog::cleanup_old_logs(&paths::logs_dir());
    // Seed cslol tools into user-data so an installer update never overwrites a locked in-use mod-tools.exe.
    injection::tools::ensure_cslol_tools();
    // Sync bundled Pengu plugins into the runtime folder every launch, so a
    // plugin added in an update reaches the client without re-activating Pengu.
    pengu::ensure_synced();
    Ok(())
}
