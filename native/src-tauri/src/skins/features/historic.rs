//! Historic-mode persistence: `historic.json` (last injected unowned skin
//! per champion) and `mod_historic.json` (last-selected map/font/announcer +
//! category mods), ported from `utils/core/historic.py`/`mod_historic.py`.
//! Both formats are kept byte-for-byte compatible so the JS plugins, which
//! read/write these files directly, stay untouched.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::skins::paths;
use crate::skins::state::HistoricSelection;

// ---------------------------------------------------------------------
// historic.json — last injected unowned skin per champion.
// ---------------------------------------------------------------------

/// One `historic.json` value: either an official skin/chroma ID, or a
/// custom mod's relative path with the `"path:"` prefix preserved literally
/// (untagged so the JSON stays `{"234": 234000}` / `{"234": "path:..."}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HistoricEntry {
    Skin(i64),
    Path(String),
}

impl HistoricEntry {
    pub fn is_custom_mod_path(&self) -> bool {
        matches!(self, HistoricEntry::Path(p) if p.starts_with("path:"))
    }

    /// The mod's relative path with the `"path:"` prefix stripped, or `None`
    /// if this isn't a custom-mod entry.
    pub fn custom_mod_path(&self) -> Option<&str> {
        match self {
            HistoricEntry::Path(p) if p.starts_with("path:") => Some(&p[5..]),
            _ => None,
        }
    }

    /// Convert to the prefix-stripped runtime `HistoricSelection` other
    /// modules match on. A `Path` entry without the `"path:"` prefix
    /// (malformed/legacy) is still treated as a custom-mod path verbatim,
    /// keeping the raw string for diagnosis.
    pub fn to_selection(&self) -> HistoricSelection {
        match self {
            HistoricEntry::Skin(id) => HistoricSelection::SkinId(*id),
            HistoricEntry::Path(raw) => HistoricSelection::CustomMod(raw.strip_prefix("path:").unwrap_or(raw).to_string()),
        }
    }
}

impl From<&HistoricSelection> for HistoricEntry {
    fn from(selection: &HistoricSelection) -> Self {
        match selection {
            HistoricSelection::SkinId(id) => HistoricEntry::Skin(*id),
            HistoricSelection::CustomMod(path) => HistoricEntry::Path(format!("path:{path}")),
        }
    }
}

fn historic_file_path() -> PathBuf {
    paths::state_dir().join("historic.json")
}

/// Load the historic mapping (champion ID -> entry). Returns an empty map on
/// any read/parse failure — best-effort, matches the Python contract.
pub fn load_historic_map() -> HashMap<String, HistoricEntry> {
    let Ok(text) = std::fs::read_to_string(historic_file_path()) else { return HashMap::new() };
    let Ok(raw) = serde_json::from_str::<HashMap<String, HistoricEntry>>(&text) else { return HashMap::new() };
    // Re-key through i64 like the Python `str(int(k))` normalization so a
    // malformed key can't silently shadow a well-formed one.
    raw.into_iter().filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id.to_string(), v))).collect()
}

fn save_historic_map(map: &HashMap<String, HistoricEntry>) {
    let path = historic_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = std::fs::write(path, json);
    }
}

pub fn get_historic_skin_for_champion(champion_id: i64) -> Option<HistoricEntry> {
    load_historic_map().get(&champion_id.to_string()).cloned()
}

pub fn write_historic_entry(champion_id: i64, entry: HistoricEntry) {
    let mut map = load_historic_map();
    map.insert(champion_id.to_string(), entry);
    save_historic_map(&map);
}

pub fn clear_historic_entry(champion_id: i64) {
    let mut map = load_historic_map();
    if map.remove(&champion_id.to_string()).is_some() {
        save_historic_map(&map);
    }
}

// ---------------------------------------------------------------------
// mod_historic.json — last-selected map/font/announcer + category mods
// (ui/voiceover/loading_screen/vfx/sfx/others). The legacy single-string
// `"other"` key migration is intentionally NOT ported (no prior installs to migrate).
// ---------------------------------------------------------------------

pub const MOD_HISTORIC_CATEGORIES: [&str; 6] = ["ui", "voiceover", "loading_screen", "vfx", "sfx", "others"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModHistoric {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announcer: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voiceover: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loading_screen: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfx: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sfx: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<String>,
}

impl ModHistoric {
    fn category_mut(&mut self, category: &str) -> Option<&mut Vec<String>> {
        match category {
            "ui" => Some(&mut self.ui),
            "voiceover" => Some(&mut self.voiceover),
            "loading_screen" => Some(&mut self.loading_screen),
            "vfx" => Some(&mut self.vfx),
            "sfx" => Some(&mut self.sfx),
            "others" => Some(&mut self.others),
            _ => None,
        }
    }

    pub fn category(&self, category: &str) -> Option<&[String]> {
        match category {
            "ui" => Some(&self.ui),
            "voiceover" => Some(&self.voiceover),
            "loading_screen" => Some(&self.loading_screen),
            "vfx" => Some(&self.vfx),
            "sfx" => Some(&self.sfx),
            "others" => Some(&self.others),
            _ => None,
        }
    }

    /// Add `relative_path` to `category`'s list, de-duplicated and keeping
    /// insertion order (`mod_historic.py::_dedupe_keep_order`).
    pub fn add_to_category(&mut self, category: &str, relative_path: String) {
        if let Some(list) = self.category_mut(category) {
            if !list.contains(&relative_path) {
                list.push(relative_path);
            }
        }
    }

    pub fn clear_category(&mut self, category: &str) {
        if let Some(list) = self.category_mut(category) {
            list.clear();
        }
    }
}

fn mod_historic_file_path() -> PathBuf {
    paths::state_dir().join("mod_historic.json")
}

pub fn load_mod_historic() -> ModHistoric {
    std::fs::read_to_string(mod_historic_file_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn write_mod_historic(historic: &ModHistoric) {
    let path = mod_historic_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(historic) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historic_entry_untagged_round_trip() {
        let skin = HistoricEntry::Skin(234000);
        let json = serde_json::to_string(&skin).unwrap();
        assert_eq!(json, "234000");
        assert_eq!(serde_json::from_str::<HistoricEntry>(&json).unwrap(), skin);

        let path = HistoricEntry::Path("path:skins/234000/old-aatrox-viego_1.2.0.fantome".to_string());
        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(serde_json::from_str::<HistoricEntry>(&json).unwrap(), path);
    }

    #[test]
    fn custom_mod_path_strips_prefix_only_for_path_entries() {
        let skin = HistoricEntry::Skin(1000);
        assert!(!skin.is_custom_mod_path());
        assert_eq!(skin.custom_mod_path(), None);

        let path = HistoricEntry::Path("path:foo/bar.fantome".to_string());
        assert!(path.is_custom_mod_path());
        assert_eq!(path.custom_mod_path(), Some("foo/bar.fantome"));
    }

    /// Mirrors `historic.json`'s on-disk shape with both a skin-ID entry and
    /// a `"path:<rel>"` custom-mod entry present at once.
    #[test]
    fn historic_map_round_trips_int_and_path_values_together() {
        let mut map = HashMap::new();
        map.insert("103".to_string(), HistoricEntry::Skin(103000));
        map.insert("234".to_string(), HistoricEntry::Path("path:skins/234000/old-aatrox-viego_1.2.0.fantome".to_string()));

        let json = serde_json::to_string(&map).unwrap();
        let parsed: HashMap<String, HistoricEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, map);
        assert_eq!(parsed.get("103"), Some(&HistoricEntry::Skin(103000)));
        assert_eq!(
            parsed.get("234"),
            Some(&HistoricEntry::Path("path:skins/234000/old-aatrox-viego_1.2.0.fantome".to_string()))
        );
    }

    #[test]
    fn to_selection_strips_path_prefix_and_passes_skin_ids_through() {
        assert_eq!(HistoricEntry::Skin(103000).to_selection(), HistoricSelection::SkinId(103000));
        assert_eq!(
            HistoricEntry::Path("path:skins/234000/mod.fantome".to_string()).to_selection(),
            HistoricSelection::CustomMod("skins/234000/mod.fantome".to_string())
        );
    }

    #[test]
    fn historic_entry_from_selection_round_trips_through_to_selection() {
        let skin = HistoricSelection::SkinId(103000);
        assert_eq!(HistoricEntry::from(&skin).to_selection(), skin);

        let custom_mod = HistoricSelection::CustomMod("skins/234000/mod.fantome".to_string());
        let entry = HistoricEntry::from(&custom_mod);
        assert_eq!(entry, HistoricEntry::Path("path:skins/234000/mod.fantome".to_string()));
        assert_eq!(entry.to_selection(), custom_mod);
    }

    #[test]
    fn mod_historic_category_add_dedupes_and_keeps_order() {
        let mut historic = ModHistoric::default();
        historic.add_to_category("ui", "ui/a.fantome".to_string());
        historic.add_to_category("ui", "ui/b.fantome".to_string());
        historic.add_to_category("ui", "ui/a.fantome".to_string()); // duplicate, ignored
        assert_eq!(historic.category("ui"), Some(&["ui/a.fantome".to_string(), "ui/b.fantome".to_string()][..]));
    }
}
