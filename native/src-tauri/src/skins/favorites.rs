//! "Favorite skin per champion" — pick a go-to skin for each champ once, and
//! Chud auto-applies it every game (no in-client hovering). The map
//! (`champ_id -> skin_id`) persists and loads into `SkinsShared::favorite_skins`;
//! the champ-lock handler (phase.rs) copies the locked champ's favorite into
//! `active_favorite_skin_id`, and `ticker::resolve_injection_name` applies it
//! as a fallback below any manual in-client pick.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::skins::paths;
use crate::skins::slog::log_info;

/// Favorites live next to `config.json` — a stable per-user dir that survives
/// updates. They used to live in `data_root()`, which is also the NSIS
/// install dir and gets wiped on every update, silently losing favorites.
fn favorites_path() -> std::path::PathBuf {
    crate::config::config_path()
        .parent()
        .map(|d| d.join("favorites.json"))
        .unwrap_or_else(|| paths::data_root().join("favorites.json"))
}

/// Old location (install dir) — read as a fallback so a user who saved
/// favorites before this fix keeps them.
fn legacy_favorites_path() -> std::path::PathBuf {
    paths::data_root().join("favorites.json")
}

/// Load the `champ_id -> skin_id` favorites map (empty if absent/invalid).
pub fn load() -> HashMap<i64, i64> {
    let text = std::fs::read_to_string(favorites_path()).or_else(|_| std::fs::read_to_string(legacy_favorites_path()));
    let Ok(text) = text else { return HashMap::new() };
    let Ok(obj) = serde_json::from_str::<HashMap<String, i64>>(&text) else { return HashMap::new() };
    obj.into_iter().filter_map(|(k, v)| k.parse::<i64>().ok().map(|c| (c, v))).collect()
}

/// Persist the favorites map.
pub fn save(map: &HashMap<i64, i64>) {
    let obj: HashMap<String, i64> = map.iter().map(|(c, s)| (c.to_string(), *s)).collect();
    if let Ok(text) = serde_json::to_string_pretty(&obj) {
        if let Some(parent) = favorites_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(favorites_path(), text);
        log_info!("[FAVORITES] Saved {} favorite skin(s)", map.len());
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkinEntry {
    pub skin_id: i64,
    pub name: String,
    /// True when the `.fantome` for this skin is present in the local skins
    /// tree (i.e. it can actually be injected right now).
    pub downloaded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChampSkins {
    pub champ_id: i64,
    pub champ_name: String,
    pub skins: Vec<SkinEntry>,
}

/// The browsable catalog: every champion and its skins from the downloaded
/// `skin_ids.json` name DB, flagged with local availability. Resolves the
/// language folder like `skin_db` does: caller's `lang`, then `default`,
/// then `en`. Empty if skins haven't been downloaded yet.
pub fn catalog(lang: Option<&str>) -> Vec<ChampSkins> {
    let mut text = None;
    for l in [lang.unwrap_or("default"), "default", "en"] {
        let path = paths::resources_dir().join(l).join("skin_ids.json");
        if let Ok(t) = std::fs::read_to_string(&path) {
            text = Some(t);
            break;
        }
    }
    let Some(text) = text else { return Vec::new() };
    let Ok(map) = serde_json::from_str::<HashMap<String, Value>>(&text) else { return Vec::new() };

    // Group skin_id -> name by champion (champ_id = skin_id / 1000). Chromas
    // are excluded (can't meaningfully "favorite" one) and are identified by
    // name: a parenthesized suffix like "(Obsidian)" or a trailing " Chroma".
    let mut by_champ: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    for (id_str, name_val) in &map {
        let Ok(skin_id) = id_str.parse::<i64>() else { continue };
        let Some(name) = name_val.as_str() else { continue };
        if name.contains('(') || name.trim_end().ends_with("Chroma") {
            continue;
        }
        by_champ.entry(skin_id / 1000).or_default().push((skin_id, name.to_string()));
    }

    let skins_dir = paths::skins_dir();
    let mut out: Vec<ChampSkins> = by_champ
        .into_iter()
        .filter(|(champ, _)| *champ > 0)
        .map(|(champ_id, mut skins)| {
            skins.sort_by_key(|(id, _)| *id);
            // Champ name = the base skin's (num 0) name, else the first skin's.
            let champ_name = skins
                .iter()
                .find(|(id, _)| *id == champ_id * 1000)
                .or_else(|| skins.first())
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            let entries = skins
                .into_iter()
                .map(|(skin_id, name)| SkinEntry {
                    downloaded: skins_dir.join(champ_id.to_string()).join(skin_id.to_string()).exists(),
                    skin_id,
                    name,
                })
                .collect();
            ChampSkins { champ_id, champ_name, skins: entries }
        })
        .collect();
    out.sort_by(|a, b| a.champ_name.cmp(&b.champ_name));
    out
}
