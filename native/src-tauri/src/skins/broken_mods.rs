//! Registry of custom mods flagged BROKEN — mods whose WAD overrides the
//! champion's root character/ability record (detected by `target_detect::
//! overrides_ability_data`). Injecting one replaces the game's live ability
//! data with the mod's bundled (usually stale) copy and breaks the champion
//! in-game — missing/unusable abilities, can't level, needs a client repair.
//!
//! Once a mod trips the inject-time safety guard we record it here so the app
//! can badge it in the Library, keep it out of favorites/random/party sync,
//! and surface WHY it was blocked. Keyed by the mod's `relative_path` within
//! the mods dir (stable across sessions).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::skins::paths;
use crate::skins::slog::log_info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenMod {
    pub name: String,
    pub champion_id: i64,
    /// The game path the mod overrides (e.g. the ability bin) — surfaced to the
    /// user so they know exactly why the skin was blocked.
    pub reason_path: String,
}

fn store_path() -> std::path::PathBuf {
    paths::state_dir().join("broken_mods.json")
}

static CACHE: OnceLock<Mutex<HashMap<String, BrokenMod>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, BrokenMod>> {
    CACHE.get_or_init(|| Mutex::new(load()))
}

fn load() -> HashMap<String, BrokenMod> {
    match std::fs::read_to_string(store_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

fn persist(map: &HashMap<String, BrokenMod>) {
    if let Ok(text) = serde_json::to_string_pretty(map) {
        let path = store_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, text);
    }
}

/// Record a mod as broken (idempotent). `rel_path` is the mod's stable relative
/// path within the mods dir; an empty path is ignored (nothing to key on).
pub fn flag(rel_path: &str, name: &str, champion_id: i64, reason_path: &str) {
    if rel_path.is_empty() {
        return;
    }
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    let is_new = map.get(rel_path).map(|b| b.reason_path != reason_path).unwrap_or(true);
    map.insert(
        rel_path.to_string(),
        BrokenMod { name: name.to_string(), champion_id, reason_path: reason_path.to_string() },
    );
    if is_new {
        log_info!("[SAFETY] Flagged broken mod '{name}' ({rel_path}) — overrides {reason_path}");
        persist(&map);
    }
}

pub fn is_broken(rel_path: &str) -> bool {
    !rel_path.is_empty() && cache().lock().unwrap_or_else(|e| e.into_inner()).contains_key(rel_path)
}

/// Full registry snapshot (rel_path -> details) for the Library UI.
pub fn list() -> HashMap<String, BrokenMod> {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Clear a mod's broken flag — e.g. the user replaced it with a fixed version.
pub fn unflag(rel_path: &str) {
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    if map.remove(rel_path).is_some() {
        persist(&map);
    }
}

/// Proactively scan every custom SKIN mod in the store and flag any that
/// override champion ability data — so a known-broken mod shows as BROKEN in the
/// Library BEFORE the user tries to play it, not only after a live inject block.
/// Cheap: reads each WAD's header + chunk table (a few KB), never its contents.
/// Keys by the same `relative_path` (relative to the mods dir, forward-slashed)
/// the inject-time guard uses, so a scan flag and a live flag are the same entry.
pub fn scan_and_flag() -> usize {
    let mods_root = paths::mods_dir();
    let skins_root = mods_root.join("skins");
    let Ok(slots) = std::fs::read_dir(&skins_root) else { return 0 };
    let mut flagged = 0usize;
    for slot in slots.flatten() {
        let slot_path = slot.path();
        if !slot_path.is_dir() {
            continue;
        }
        // Folder name is the base skin slot (championId * 1000).
        let Some(champion_id) = slot.file_name().to_str().and_then(|s| s.parse::<i64>().ok()).map(|s| s / 1000)
        else {
            continue;
        };
        let Ok(files) = std::fs::read_dir(&slot_path) else { continue };
        for f in files.flatten() {
            let path = f.path();
            let is_mod = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fantome" | "zip"))
                .unwrap_or(false);
            if !is_mod {
                continue;
            }
            if let Some(bad) =
                crate::skins::injection::target_detect::overrides_ability_data(&path, champion_id)
            {
                let rel = path.strip_prefix(&mods_root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if !is_broken(&rel) {
                    flagged += 1;
                }
                flag(&rel, &name, champion_id, &bad);
            }
        }
    }
    flagged
}
