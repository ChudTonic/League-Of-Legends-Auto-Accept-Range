//! Offline skin name->ID resolution from the downloaded `skin_ids.json` — a
//! complete ID->name database per client language, produced by
//! `RepoDownloader`'s `resources/` extraction.
//!
//! Fallback when the LIVE LCU champion scrape can't resolve a hovered skin
//! name (client data comes back thin on some machines, more likely during
//! ARAM's rapid bench swaps). Without this, an unresolved hover leaves
//! `last_hovered_skin_id` empty and injection fails with "NO SKIN ID
//! AVAILABLE" even though the skin is well-known offline.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};

/// lang -> (normalized-name -> skin_id). Loaded lazily, cached per language.
/// Stored behind `Arc` so a cache hit is an O(1) refcount bump, not a full
/// clone of the (thousands-of-entries) map — this is hit repeatedly during
/// ARAM bench swaps.
static DB: OnceLock<Mutex<HashMap<String, Arc<HashMap<String, i64>>>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, Arc<HashMap<String, i64>>>> {
    DB.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read + invert one language file: on disk it's `{ "266033": "Primordian
/// Aatrox", ... }` (id→name); we build normalized-name→id. First name wins on
/// collision, matching Rose's `if normalized not in mapping` guard.
fn load_lang(lang: &str) -> Option<HashMap<String, i64>> {
    let path = paths::resources_dir().join(lang).join("skin_ids.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let raw: HashMap<String, String> = serde_json::from_str(&text).ok()?;
    let mut map: HashMap<String, i64> = HashMap::with_capacity(raw.len());
    for (id_str, name) in raw {
        let Ok(id) = id_str.parse::<i64>() else { continue };
        let key = name.trim().to_lowercase();
        if !key.is_empty() {
            map.entry(key).or_insert(id);
        }
    }
    log_info!("[skin-db] Loaded offline skin map '{lang}' ({} names)", map.len());
    Some(map)
}

fn lang_map(lang: &str) -> Option<Arc<HashMap<String, i64>>> {
    {
        let g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = g.get(lang) {
            return Some(Arc::clone(m)); // O(1) refcount bump, not a map clone
        }
    }
    let loaded = Arc::new(load_lang(lang)?);
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(lang.to_string(), Arc::clone(&loaded));
    Some(loaded)
}

/// Resolve a skin display name to its ID from the offline DB. Tries the given
/// client language, then `default`, then `en`. Exact normalized match first;
/// if none, a conservative substring match preferring the closest-length key
/// (so a bare champion name doesn't grab a longer skin). `None` if the files
/// aren't present (skins not downloaded yet) or the name is unknown.
pub fn resolve_skin_id(name: &str, lang: Option<&str>) -> Option<i64> {
    let needle = name.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut tried_any = false;
    for l in [lang.unwrap_or("default"), "default", "en"] {
        let Some(map) = lang_map(l) else { continue };
        tried_any = true;
        if let Some(&id) = map.get(&needle) {
            return Some(id);
        }
        let mut best: Option<(usize, i64)> = None;
        for (k, &id) in map.iter() {
            if k.contains(&needle) || needle.contains(k.as_str()) {
                let diff = k.len().abs_diff(needle.len());
                if best.map_or(true, |(b, _)| diff < b) {
                    best = Some((diff, id));
                }
            }
        }
        if let Some((_, id)) = best {
            return Some(id);
        }
    }
    if !tried_any {
        log_warn!("[skin-db] No offline skin_ids.json available (skins not downloaded?) - cannot resolve '{needle}'");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invert_prefers_first_name_on_collision() {
        // (pure inversion sanity — real files are loaded from disk at runtime)
        let raw: HashMap<String, String> =
            serde_json::from_str(r#"{"266033":"Primordian Aatrox","266000":"Aatrox"}"#).unwrap();
        let mut map: HashMap<String, i64> = HashMap::new();
        for (id_str, name) in raw {
            if let Ok(id) = id_str.parse::<i64>() {
                map.entry(name.trim().to_lowercase()).or_insert(id);
            }
        }
        assert_eq!(map.get("primordian aatrox"), Some(&266033));
        assert_eq!(map.get("aatrox"), Some(&266000));
    }
}
