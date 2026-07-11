//! LCU skin-feature surface: JSON types, the champ-select cell/lock pure
//! functions, the per-champion skin scraper/cache, skin selection, game-mode
//! detection, and Swiftplay lobby digging. Ported from `lcu/data/*.py`,
//! `lcu/features/*.py`, and `lcu/core/lcu_api.py`'s PATCH/PUT contract (S2).
//!
//! Builds on `crate::lcu` (auth + the generic `get_json`/`get_phase` GETs) —
//! this module only adds the PATCH/PUT calls `lcu.rs` doesn't have and the
//! typed shapes the skins subsystem needs. Every LCU JSON struct field is
//! `Option` (+ `#[serde(default)]`): the client's schemas vary by patch and a
//! missing field must never fail the whole deserialize.
//!
//! Every endpoint helper reads/writes through `shared_cache()` (a per-path
//! TTL cache ported from `lcu/core/lcu_api.py::LCUAPI`) rather than calling
//! `lcu::get_json` directly, so bursty polls from independent callers
//! de-dupe into one request — see the "Cached GET layer" section below.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::header::AUTHORIZATION;
use serde::Deserialize;
use serde_json::{json, Value};
use sysinfo::{ProcessesToUpdate, System};

use crate::lcu::{self, Auth};
use crate::LockExt;

// ---------------------------------------------------------------------
// Magic values preserved verbatim (docs/SKINS_PORT.md "Magic values").
// ---------------------------------------------------------------------

/// `lcu/core/lockfile.py::SWIFTPLAY_QUEUE_ID`.
pub const SWIFTPLAY_QUEUE_ID: i64 = 480;
/// `lcu/core/lockfile.py::SWIFTPLAY_MODES`.
pub const SWIFTPLAY_MODES: [&str; 2] = ["SWIFTPLAY", "BRAWL"];
/// `config.py::LCU_API_TIMEOUT_S`.
pub const LCU_API_TIMEOUT_S: f64 = 2.0;
/// `config.py::LCU_SKIN_SCRAPER_TIMEOUT_S`.
pub const LCU_SKIN_SCRAPER_TIMEOUT_S: f64 = 3.0;

fn is_swiftplay_mode_str(game_mode: &str) -> bool {
    let upper = game_mode.to_uppercase();
    SWIFTPLAY_MODES.contains(&upper.as_str())
}

// ---------------------------------------------------------------------
// League install/game directory discovery — feeds
// `injection::InjectionManager::set_game_dir`, which `mkoverlay`'s
// `--game:<path>` argument needs (`injection/game/game_detector.py` was never
// ported as its own module; this is that lookup). Mirrors `lcu::find_auth`'s
// process scan (same running `LeagueClientUx.exe`, same
// `--install-directory=` cmdline fallback) rather than reusing it directly —
// `lcu.rs` only exposes lockfile auth, not the bare install directory.
// ---------------------------------------------------------------------

fn install_dir_from_cmd(cmd: &[OsString]) -> Option<PathBuf> {
    for arg in cmd {
        let s = arg.to_string_lossy();
        if let Some(rest) = s.strip_prefix("--install-directory=") {
            return Some(PathBuf::from(rest.trim_matches('"')));
        }
    }
    None
}

/// Find the running League Client and resolve its install directory's
/// `Game` subfolder. `None` when the client isn't running, its install dir
/// can't be determined, or `Game` doesn't exist yet (client running but the
/// game itself isn't installed/updated) — callers must treat `None` as
/// "injection unavailable right now", not an error.
pub fn resolve_game_dir() -> Option<PathBuf> {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    for proc in sys.processes().values() {
        if proc.name().to_string_lossy().to_lowercase() != "leagueclientux.exe" {
            continue;
        }
        let install_dir = proc
            .exe()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| install_dir_from_cmd(proc.cmd()));
        if let Some(install_dir) = install_dir {
            let game_dir = install_dir.join("Game");
            if game_dir.is_dir() {
                return Some(game_dir);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Cached GET layer — `lcu/core/lcu_api.py::LCUAPI`. A per-path TTL cache so
// bursty polls from independent callers hitting the same endpoint in the
// same tick share one request, exactly like the Python original's single
// `LCUAPI` instance shared by the whole app. CONTRACT: HTTP 404/405 (and any
// other failure) collapse to `None` — "no data", not an error; callers
// pattern-match on `None` rather than treating it as a fetch failure (same
// contract `lcu::get_json` already has — this layer only adds de-dup).
// ---------------------------------------------------------------------

/// Default GET cache TTL — de-dupes bursty polls (`config.py::LCU_GET_CACHE_TTL_S`).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_millis(200);

pub struct LcuCache {
    entries: Mutex<HashMap<String, (Instant, Option<Value>)>>,
}

impl LcuCache {
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    fn cached(&self, path: &str) -> Option<Option<Value>> {
        let mut entries = self.entries.lock_safe();
        match entries.get(path) {
            Some((expiry, value)) if *expiry > Instant::now() => Some(value.clone()),
            Some(_) => {
                entries.remove(path);
                None
            }
            None => None,
        }
    }

    fn store(&self, path: &str, value: Option<Value>, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        self.entries.lock_safe().insert(path.to_string(), (Instant::now() + ttl, value));
    }

    /// Cached authed GET — see the module-level CONTRACT note above.
    pub async fn get(&self, client: &reqwest::Client, auth: &Auth, path: &str, ttl: Duration) -> Option<Value> {
        if !ttl.is_zero() {
            if let Some(cached) = self.cached(path) {
                return cached;
            }
        }
        let value = lcu::get_json(client, auth, path).await;
        self.store(path, value.clone(), ttl);
        value
    }

    /// Invalidate `path_prefix` and every cached ancestor of it (a PATCH/PUT
    /// on a child resource logically mutates its parents too). An empty
    /// prefix clears the whole cache.
    pub fn invalidate(&self, path_prefix: &str) {
        let mut entries = self.entries.lock_safe();
        if path_prefix.is_empty() {
            entries.clear();
            return;
        }
        let ancestors = ancestors_of(path_prefix);
        entries.retain(|k, _| !(k.starts_with(path_prefix) || ancestors.contains(k)));
    }
}

impl Default for LcuCache {
    fn default() -> Self {
        Self::new()
    }
}

fn ancestors_of(path_prefix: &str) -> HashSet<String> {
    let trimmed = path_prefix.trim_end_matches('/');
    let segments: Vec<&str> = trimmed.split('/').collect();
    let mut ancestors = HashSet::new();
    for i in 1..segments.len() {
        let ancestor = segments[..i].join("/");
        if !ancestor.is_empty() {
            ancestors.insert(ancestor);
        }
    }
    ancestors
}

static SHARED_CACHE: OnceLock<LcuCache> = OnceLock::new();

/// Process-wide cache instance. Every endpoint helper below reads/writes
/// through this instead of calling `lcu::get_json` directly, so callers
/// never have to thread a cache handle through every signature — mirrors
/// the single `LCU` facade object the whole Python app shared.
pub fn shared_cache() -> &'static LcuCache {
    SHARED_CACHE.get_or_init(LcuCache::new)
}

// ---------------------------------------------------------------------
// LCU JSON types — mirrors `lcu/data/types.py`. All-Option, all-default:
// the LCU adds/removes fields across patches and a strict shape would break
// on the first client update.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChromaData {
    pub id: Option<i64>,
    pub name: Option<String>,
    #[serde(default, rename = "chromaPath")]
    pub chroma_path: Option<String>,
    #[serde(default)]
    pub colors: Option<Vec<String>>,
    pub disabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkinData {
    pub id: Option<i64>,
    #[serde(default, rename = "skinId")]
    pub skin_id: Option<i64>,
    pub name: Option<String>,
    #[serde(default, rename = "skinName")]
    pub skin_name: Option<String>,
    #[serde(default)]
    pub chromas: Option<Vec<ChromaData>>,
    #[serde(default, rename = "isBase")]
    pub is_base: Option<bool>,
    pub disabled: Option<bool>,
    pub num: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChampionData {
    pub id: Option<i64>,
    pub name: Option<String>,
    pub alias: Option<String>,
    #[serde(default)]
    pub skins: Option<Vec<SkinData>>,
}

/// One `myTeam`/`theirTeam` entry from the champ-select session — the
/// `map_cells`/`compute_locked` "cell" the doc refers to.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Cell {
    #[serde(default, rename = "cellId")]
    pub cell_id: Option<i64>,
    #[serde(default, rename = "championId")]
    pub champion_id: Option<i64>,
    #[serde(default, rename = "championPickIntent")]
    pub champion_pick_intent: Option<i64>,
    #[serde(default, rename = "pickIntentChampionId")]
    pub pick_intent_champion_id: Option<i64>,
    #[serde(default, rename = "isPickIntenting")]
    pub is_pick_intenting: Option<bool>,
    #[serde(default, rename = "selectedSkinId")]
    pub selected_skin_id: Option<i64>,
    #[serde(default, rename = "summonerId")]
    pub summoner_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionData {
    pub id: Option<i64>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    pub completed: Option<bool>,
    #[serde(default, rename = "actorCellId")]
    pub actor_cell_id: Option<i64>,
    #[serde(default, rename = "championId")]
    pub champion_id: Option<i64>,
}

/// Champion-select session (`/lol-champ-select/v1/session`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionData {
    #[serde(default)]
    pub actions: Option<Vec<Vec<ActionData>>>,
    #[serde(default, rename = "myTeam")]
    pub my_team: Option<Vec<Cell>>,
    #[serde(default, rename = "theirTeam")]
    pub their_team: Option<Vec<Cell>>,
    #[serde(default, rename = "localPlayerCellId")]
    pub local_player_cell_id: Option<i64>,
    #[serde(default, rename = "isSpectating")]
    pub is_spectating: Option<bool>,
    #[serde(default, rename = "queueId")]
    pub queue_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegionLocale {
    pub region: Option<String>,
    pub locale: Option<String>,
}

// ---------------------------------------------------------------------
// Cell / lock pure functions — `lcu/data/utils.py`. Kept free of any LCU
// I/O so they're unit-testable against a hand-built session fixture.
// ---------------------------------------------------------------------

/// Index every `myTeam`/`theirTeam` entry by cell ID.
pub fn map_cells(session: &SessionData) -> HashMap<i64, Cell> {
    let mut idx = HashMap::new();
    for side in [session.my_team.as_deref(), session.their_team.as_deref()]
        .into_iter()
        .flatten()
    {
        for cell in side {
            if let Some(cid) = cell.cell_id {
                idx.insert(cid, cell.clone());
            }
        }
    }
    idx
}

/// Compute locked champions by cell ID, including the "implicit lock"
/// heuristic: a cell with a champion assigned and no pick intent pending
/// (`intent == 0 && !is_pick_intenting`) is treated as locked even without a
/// completed `pick` action — this is how ARAM/Swiftplay (no ban/pick
/// actions) and late-joining spectator sessions surface a lock.
pub fn compute_locked(session: &SessionData) -> HashMap<i64, i64> {
    let mut locked: HashMap<i64, i64> = HashMap::new();
    let idx = map_cells(session);

    for round in session.actions.iter().flatten() {
        for action in round {
            if action.kind.as_deref() != Some("pick") || action.completed != Some(true) {
                continue;
            }
            let Some(cid) = action.actor_cell_id else { continue };
            let mut champ = action.champion_id.unwrap_or(0);
            if champ == 0 {
                champ = idx.get(&cid).and_then(|p| p.champion_id).unwrap_or(0);
            }
            if champ > 0 {
                locked.insert(cid, champ);
            }
        }
    }

    for (cid, cell) in &idx {
        let champ = cell.champion_id.unwrap_or(0);
        if champ <= 0 {
            continue;
        }
        let cpi = cell.champion_pick_intent.unwrap_or(0);
        let pici = cell.pick_intent_champion_id.unwrap_or(0);
        let intent = if cpi != 0 { cpi } else { pici };
        let is_intenting = cell.is_pick_intenting.unwrap_or(false);
        if intent == 0 && !is_intenting {
            locked.insert(*cid, champ);
        }
    }

    locked
}

// ---------------------------------------------------------------------
// Champion skin cache + scraper — `lcu/data/skin_cache.py` + `skin_scraper.py`.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ChromaInfo {
    pub id: i64,
    pub name: String,
    pub colors: Vec<String>,
    pub chroma_path: String,
    pub skin_id: i64,
}

#[derive(Debug, Clone, Default)]
pub struct SkinInfo {
    pub skin_id: i64,
    pub champion_id: i64,
    pub skin_name: String,
    pub is_base: bool,
    pub chromas: usize,
    pub chroma_details: Vec<ChromaInfo>,
    pub num: i64,
}

/// Cache for one champion's skins, scraped from the LCU (`ChampionSkinCache`
/// in Python). Owned by the phase actor (S2) rather than `SkinsShared` —
/// it's per-champion scratch data, not session state other subsystems need
/// to share, so it doesn't need the coarse mutex.
#[derive(Debug, Clone, Default)]
pub struct ChampionSkinCache {
    pub champion_id: Option<i64>,
    pub champion_name: Option<String>,
    pub skins: Vec<SkinInfo>,
    pub skin_id_map: HashMap<i64, SkinInfo>,
    pub skin_name_map: HashMap<String, SkinInfo>,
    pub chroma_id_map: HashMap<i64, ChromaInfo>,
}

impl ChampionSkinCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn is_loaded_for_champion(&self, champion_id: i64) -> bool {
        self.champion_id == Some(champion_id) && !self.skins.is_empty()
    }

    pub fn get_skin_by_id(&self, skin_id: i64) -> Option<&SkinInfo> {
        self.skin_id_map.get(&skin_id)
    }

    pub fn get_skin_by_name(&self, name: &str) -> Option<&SkinInfo> {
        self.skin_name_map.get(name)
    }

    pub fn get_chromas_for_skin(&self, skin_id: i64) -> Option<&[ChromaInfo]> {
        self.get_skin_by_id(skin_id).map(|s| s.chroma_details.as_slice())
    }

    /// True when `skin_id` is a real LCU chroma (a key in `chroma_id_map`) —
    /// `utils/core/utilities.py::is_chroma_id`.
    pub fn is_chroma(&self, skin_id: i64) -> bool {
        self.chroma_id_map.contains_key(&skin_id)
    }
}

/// Scrape all skins for `champion_id`, trying the game-data endpoint first
/// and falling back to the scouting-inventory endpoint (`skin_scraper.py`'s
/// two-endpoint fallback). Callers own the "already cached?" check
/// (`ChampionSkinCache::is_loaded_for_champion`) before calling this —
/// unlike the Python original, which held the cache internally, this is a
/// pure fetch that always returns a fresh cache.
pub async fn scrape_champion_skins(
    client: &reqwest::Client,
    auth: &Auth,
    champion_id: i64,
) -> Option<ChampionSkinCache> {
    let endpoints = [
        format!("/lol-game-data/assets/v1/champions/{champion_id}.json"),
        format!("/lol-champions/v1/inventories/scouting/champions/{champion_id}"),
    ];

    let mut champ: Option<ChampionData> = None;
    for endpoint in &endpoints {
        let Some(value) = shared_cache().get(client, auth, endpoint, DEFAULT_CACHE_TTL).await else { continue };
        if value.get("skins").is_none() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_value::<ChampionData>(value) {
            champ = Some(parsed);
            break;
        }
    }
    let champ = champ?;
    Some(build_skin_cache(champ, champion_id))
}

/// Pure transform from parsed `/champions/{id}.json` data into a
/// `ChampionSkinCache` — factored out of `scrape_champion_skins` so the
/// skin-name resolution pipeline can be replay-tested against recorded LCU
/// champion payloads without a live client (see `lcu_replay` tests).
pub fn build_skin_cache(champ: ChampionData, champion_id: i64) -> ChampionSkinCache {
    let mut cache = ChampionSkinCache {
        champion_id: Some(champion_id),
        champion_name: Some(champ.name.unwrap_or_else(|| format!("Champion{champion_id}"))),
        ..Default::default()
    };

    for skin in champ.skins.unwrap_or_default() {
        let Some(skin_id) = skin.id else { continue };
        let Some(skin_name) = skin.name.filter(|n| !n.is_empty()) else { continue };

        let raw_chromas = skin.chromas.unwrap_or_default();
        let mut chroma_details = Vec::with_capacity(raw_chromas.len());
        for chroma in &raw_chromas {
            let Some(chroma_id) = chroma.id else { continue };
            let info = ChromaInfo {
                id: chroma_id,
                name: chroma.name.clone().unwrap_or_default(),
                colors: chroma.colors.clone().unwrap_or_default(),
                chroma_path: chroma.chroma_path.clone().unwrap_or_default(),
                skin_id,
            };
            cache.chroma_id_map.insert(chroma_id, info.clone());
            chroma_details.push(info);
        }

        let info = SkinInfo {
            skin_id,
            champion_id,
            skin_name: skin_name.clone(),
            is_base: skin.is_base.unwrap_or(false),
            chromas: raw_chromas.len(),
            chroma_details,
            num: skin.num.unwrap_or(0),
        };
        cache.skin_id_map.insert(skin_id, info.clone());
        cache.skin_name_map.insert(skin_name, info.clone());
        cache.skins.push(info);
    }

    cache
}

/// The League client sometimes appends a chroma colour as a locale-specific
/// suffix — Portuguese `"SkinName (Renegado)"`, Russian `"SkinName – ''Пылкость''"`.
/// Ported literally from `skin_scraper.py::find_skin_by_text`: real skin
/// names can also use parentheses (Russian prestige skins), so this is only
/// ever tried as an ALTERNATE candidate, never a destructive rewrite.
fn locale_chroma_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s*(?:\([^)]*\)|–\s*'{0,2}[^']+'{0,2})\s*$").unwrap())
}

/// Levenshtein edit distance (character-based), ported from
/// `utils/core/normalization.py::levenshtein_distance`.
pub fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = if s1.chars().count() < s2.chars().count() {
        (s2.chars().collect(), s1.chars().collect())
    } else {
        (s1.chars().collect(), s2.chars().collect())
    };
    if b.is_empty() {
        return a.len();
    }

    let mut previous_row: Vec<usize> = (0..=b.len()).collect();
    for (i, &c1) in a.iter().enumerate() {
        let mut current_row = vec![i + 1];
        for (j, &c2) in b.iter().enumerate() {
            let insertions = previous_row[j + 1] + 1;
            let deletions = current_row[j] + 1;
            let substitutions = previous_row[j] + usize::from(c1 != c2);
            current_row.push(insertions.min(deletions).min(substitutions));
        }
        previous_row = current_row;
    }
    previous_row[previous_row.len() - 1]
}

/// Find the best-matching skin by (possibly locale-suffixed) detected text.
/// Exact match wins outright (both the raw and locale-stripped candidate);
/// otherwise falls back to a global-minimum Levenshtein distance across
/// every skin × candidate pair — ported 1:1 from `find_skin_by_text`,
/// including its "first strictly-smaller distance wins" tie-break.
pub fn find_skin_by_text(cache: &ChampionSkinCache, text: &str) -> Option<(i64, String, f64)> {
    if text.is_empty() || cache.skins.is_empty() {
        return None;
    }

    let mut candidates = vec![text.to_string()];
    let stripped = locale_chroma_suffix_re().replace(text, "").to_string();
    if stripped != text {
        candidates.push(stripped);
    }

    for candidate in &candidates {
        if let Some(skin) = cache.get_skin_by_name(candidate) {
            return Some((skin.skin_id, skin.skin_name.clone(), 1.0));
        }
    }

    let mut best: Option<(&SkinInfo, f64)> = None;
    let mut best_distance = usize::MAX;
    for skin in &cache.skins {
        for candidate in &candidates {
            let distance = levenshtein_distance(candidate, &skin.skin_name);
            let max_len = candidate.chars().count().max(skin.skin_name.chars().count());
            let similarity = if max_len > 0 { 1.0 - (distance as f64 / max_len as f64) } else { 0.0 };
            if distance < best_distance {
                best_distance = distance;
                best = Some((skin, similarity));
            }
        }
    }
    best.map(|(skin, similarity)| (skin.skin_id, skin.skin_name.clone(), similarity))
}

// ---------------------------------------------------------------------
// Property helpers — `lcu/features/lcu_properties.py`. Thin wrappers over
// `lcu::get_json` (already has the 404/405 -> None + connection-retry
// contract); this layer only adds the endpoint path + typed parse.
// ---------------------------------------------------------------------

pub async fn champ_select_session(client: &reqwest::Client, auth: &Auth) -> Option<SessionData> {
    let value = shared_cache().get(client, auth, "/lol-champ-select/v1/session", DEFAULT_CACHE_TTL).await?;
    serde_json::from_value(value).ok()
}

pub async fn hovered_champion_id(client: &reqwest::Client, auth: &Auth) -> Option<i64> {
    let value = shared_cache()
        .get(client, auth, "/lol-champ-select/v1/hovered-champion-id", DEFAULT_CACHE_TTL)
        .await?;
    value.as_i64().or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// `/session/my-selection`, falling back to the legacy `/selection` endpoint.
pub async fn my_selection(client: &reqwest::Client, auth: &Auth) -> Option<Value> {
    if let Some(v) = shared_cache()
        .get(client, auth, "/lol-champ-select/v1/session/my-selection", DEFAULT_CACHE_TTL)
        .await
    {
        return Some(v);
    }
    shared_cache().get(client, auth, "/lol-champ-select/v1/selection", DEFAULT_CACHE_TTL).await
}

pub async fn owned_champions_minimal(client: &reqwest::Client, auth: &Auth) -> Option<Value> {
    shared_cache().get(client, auth, "/lol-champions/v1/owned-champions-minimal", DEFAULT_CACHE_TTL).await
}

/// All owned skin IDs from the inventory (expensive — call explicitly, not
/// on a poll tick; matches the Python docstring's warning).
pub async fn owned_skin_ids(client: &reqwest::Client, auth: &Auth) -> Option<HashSet<i64>> {
    let value = shared_cache()
        .get(client, auth, "/lol-inventory/v2/inventory/CHAMPION_SKIN", DEFAULT_CACHE_TTL)
        .await?;
    let items = value.as_array()?;
    let mut ids = HashSet::new();
    for item in items {
        if let Some(id) = item.get("itemId").and_then(Value::as_i64) {
            ids.insert(id);
        }
    }
    Some(ids)
}

pub async fn current_summoner(client: &reqwest::Client, auth: &Auth) -> Option<Value> {
    shared_cache().get(client, auth, "/lol-summoner/v1/current-summoner", DEFAULT_CACHE_TTL).await
}

pub async fn region_locale(client: &reqwest::Client, auth: &Auth) -> Option<RegionLocale> {
    let value = shared_cache().get(client, auth, "/riotclient/region-locale", DEFAULT_CACHE_TTL).await?;
    serde_json::from_value(value).ok()
}

pub async fn client_language(client: &reqwest::Client, auth: &Auth) -> Option<String> {
    region_locale(client, auth).await.and_then(|r| r.locale)
}

/// Champion name by ID: game-data endpoint first, scouting-inventory
/// fallback — mirrors `LCUProperties.get_champion_name_by_id`.
pub async fn champion_name_by_id(client: &reqwest::Client, auth: &Auth, champion_id: i64) -> Option<String> {
    let endpoints = [
        format!("/lol-game-data/assets/v1/champions/{champion_id}.json"),
        format!("/lol-champions/v1/inventories/scouting/champions/{champion_id}"),
    ];
    for endpoint in &endpoints {
        if let Some(value) = shared_cache().get(client, auth, endpoint, DEFAULT_CACHE_TTL).await {
            if let Some(name) = value.get("name").and_then(Value::as_str) {
                return Some(name.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------
// Game mode detection — `lcu/features/lcu_game_mode.py`. No Python TypedDict
// backs `/lol-gameflow/v1/session` (the original just chains `dict.get`s),
// so this stays `serde_json::Value` digging rather than a typed struct.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GameModeInfo {
    pub game_mode: Option<String>,
    /// 12 = Howling Abyss (ARAM), 11 = Summoner's Rift.
    pub map_id: Option<i64>,
    pub queue_id: Option<i64>,
    pub is_swiftplay: bool,
}

impl GameModeInfo {
    pub fn is_aram(&self) -> bool {
        self.map_id == Some(12) || self.game_mode.as_deref() == Some("ARAM")
    }

    pub fn is_sr(&self) -> bool {
        self.map_id == Some(11) || self.game_mode.as_deref() == Some("CLASSIC")
    }
}

/// Detect the game mode/map/queue once (call on ChampSelect entry) —
/// `GameModeDetector.detect_game_mode`'s three-location queue-ID fallback:
/// `gameData.queue`, then top-level `queueId`, then the champ-select
/// session's `queueId`.
pub async fn detect_game_mode(client: &reqwest::Client, auth: &Auth) -> GameModeInfo {
    let Some(session) = shared_cache().get(client, auth, "/lol-gameflow/v1/session", DEFAULT_CACHE_TTL).await else {
        return GameModeInfo::default();
    };

    let mut game_mode = None;
    let mut map_id = None;
    let mut queue_id = None;

    if let Some(queue) = session.get("gameData").and_then(|gd| gd.get("queue")) {
        game_mode = queue.get("gameMode").and_then(Value::as_str).map(String::from);
        map_id = queue.get("mapId").and_then(Value::as_i64);
        queue_id = queue.get("queueId").and_then(Value::as_i64);
    }
    if queue_id.is_none() {
        queue_id = session.get("queueId").and_then(Value::as_i64);
    }
    if queue_id.is_none() {
        if let Some(champ_session) =
            shared_cache().get(client, auth, "/lol-champ-select/v1/session", DEFAULT_CACHE_TTL).await
        {
            queue_id = champ_session.get("queueId").and_then(Value::as_i64);
        }
    }

    let is_swiftplay = queue_id == Some(SWIFTPLAY_QUEUE_ID)
        || game_mode.as_deref().map(is_swiftplay_mode_str).unwrap_or(false);

    GameModeInfo { game_mode, map_id, queue_id, is_swiftplay }
}

// ---------------------------------------------------------------------
// Skin selection PATCH — `lcu/features/lcu_skin_selection.py`. `lcu.rs` only
// has GET helpers, so the PATCH request is built inline here with the same
// auth header convention.
// ---------------------------------------------------------------------

async fn patch_json(client: &reqwest::Client, auth: &Auth, path: &str, body: Value) -> Option<reqwest::StatusCode> {
    // A successful PATCH mutates server state the cache may be holding stale
    // (e.g. the champ-select session's `myTeam[].selectedSkinId`) — drop it
    // and its ancestors so the next GET refetches (`LCUAPI.patch`'s
    // `self.invalidate(path)` before the request).
    shared_cache().invalidate(path);
    let resp = client
        .patch(format!("{}{}", auth.base_url, path))
        .header(AUTHORIZATION, &auth.header)
        .json(&body)
        .send()
        .await
        .ok()?;
    Some(resp.status())
}

fn is_patch_success(status: Option<reqwest::StatusCode>) -> bool {
    matches!(status, Some(s) if s.as_u16() == 200 || s.as_u16() == 204)
}

/// PATCH the champ-select action's `selectedSkinId` (works before lock).
pub async fn set_selected_skin(client: &reqwest::Client, auth: &Auth, action_id: i64, skin_id: i64) -> bool {
    let path = format!("/lol-champ-select/v1/session/actions/{action_id}");
    is_patch_success(patch_json(client, auth, &path, json!({ "selectedSkinId": skin_id })).await)
}

/// PATCH `my-selection`'s `selectedSkinId` (works after champion lock).
pub async fn set_my_selection_skin(client: &reqwest::Client, auth: &Auth, skin_id: i64) -> bool {
    let path = "/lol-champ-select/v1/session/my-selection";
    is_patch_success(patch_json(client, auth, path, json!({ "selectedSkinId": skin_id })).await)
}

// ---------------------------------------------------------------------
// Swiftplay — `lcu/features/lcu_swiftplay.py`. Lobby data has no fixed LCU
// schema across the endpoints tried, so this stays `Value` digging too
// (matches the Python original's defensive `dict.get` chains).
// ---------------------------------------------------------------------

const SWIFTPLAY_LOBBY_ENDPOINTS: [&str; 3] =
    ["/lol-lobby/v2/lobby", "/lol-lobby/v2/lobby/matchmaking/search-state", "/lol-lobby/v1/parties/me"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChampionSelection {
    pub champion_id: i64,
    pub skin_id: i64,
    pub position: String,
    pub spell1: i64,
    pub spell2: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DualChampionSelection {
    pub champions: Vec<ChampionSelection>,
    pub champion_1: Option<ChampionSelection>,
    pub champion_2: Option<ChampionSelection>,
}

fn value_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn value_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn is_swiftplay_lobby_data(data: &Value, already_swiftplay: bool) -> bool {
    if let Some(gm) = data.get("gameMode").and_then(Value::as_str) {
        if is_swiftplay_mode_str(gm) {
            return true;
        }
    }
    if data.get("queueId").and_then(Value::as_i64) == Some(SWIFTPLAY_QUEUE_ID) {
        return true;
    }
    already_swiftplay
}

/// The current lobby's `partyId` (a GUID shared identically by every member of
/// the lobby), or `None` when not in a lobby. This is the anchor for
/// auto-party: all Chud users in the same lobby derive the same relay room from
/// it, so they converge automatically with no token exchange.
pub async fn get_lobby_party_id(client: &reqwest::Client, auth: &Auth) -> Option<String> {
    let lobby = shared_cache().get(client, auth, "/lol-lobby/v2/lobby", DEFAULT_CACHE_TTL).await?;
    lobby.get("partyId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Try each lobby-ish endpoint until one looks like Swiftplay lobby data.
/// `already_swiftplay` mirrors the Python `self.game_mode.is_swiftplay`
/// short-circuit (any lobby data is treated as Swiftplay once already
/// detected).
pub async fn get_swiftplay_lobby_data(client: &reqwest::Client, auth: &Auth, already_swiftplay: bool) -> Option<Value> {
    for endpoint in SWIFTPLAY_LOBBY_ENDPOINTS {
        if let Some(data) = shared_cache().get(client, auth, endpoint, DEFAULT_CACHE_TTL).await {
            if is_swiftplay_lobby_data(&data, already_swiftplay) {
                return Some(data);
            }
        }
    }
    None
}

fn build_selection(mut champ_id: i64, skin_id: i64, slot: &Value) -> Option<ChampionSelection> {
    if champ_id <= 0 && skin_id > 0 {
        champ_id = skin_id / 1000;
    }
    if champ_id <= 0 && skin_id <= 0 {
        return None;
    }
    Some(ChampionSelection {
        champion_id: champ_id,
        skin_id,
        position: value_str(slot, "positionPreference"),
        spell1: value_i64(slot, "spell1"),
        spell2: value_i64(slot, "spell2"),
    })
}

fn extract_champion_selection(data: &Value) -> Option<ChampionSelection> {
    if let Some(local_member) = data.get("localMember") {
        if let Some(slot) = local_member.get("playerSlots").and_then(Value::as_array).and_then(|s| s.first()) {
            if let Some(sel) = build_selection(value_i64(slot, "championId"), value_i64(slot, "skinId"), slot) {
                return Some(sel);
            }
        }
        let mut skin_id = value_i64(local_member, "selectedSkinId");
        if skin_id <= 0 {
            skin_id = value_i64(local_member, "primarySkinId");
        }
        let mut champ_id = value_i64(local_member, "primaryChampionId");
        if champ_id <= 0 {
            champ_id = value_i64(local_member, "secondaryChampionId");
        }
        if let Some(sel) = build_selection(champ_id, skin_id, local_member) {
            return Some(sel);
        }
    }

    if let Some(members) = data.get("members").and_then(Value::as_array) {
        let local_summoner_id = data.get("localMember").and_then(|lm| lm.get("summonerId")).cloned();
        for member in members {
            let is_leader = member.get("isLeader").and_then(Value::as_bool).unwrap_or(false);
            let is_local = member.get("isLocalPlayer").and_then(Value::as_bool).unwrap_or(false);
            let same_summoner = match (&local_summoner_id, member.get("summonerId")) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            };
            if !(is_leader || is_local || same_summoner) {
                continue;
            }
            if let Some(slot) = member.get("playerSlots").and_then(Value::as_array).and_then(|s| s.first()) {
                if let Some(sel) = build_selection(value_i64(slot, "championId"), value_i64(slot, "skinId"), slot) {
                    return Some(sel);
                }
            }
            let mut skin_id = value_i64(member, "selectedSkinId");
            if skin_id <= 0 {
                skin_id = value_i64(member, "primarySkinId");
            }
            let mut champ_id = value_i64(member, "primaryChampionId");
            if champ_id <= 0 {
                champ_id = value_i64(member, "secondaryChampionId");
            }
            if let Some(sel) = build_selection(champ_id, skin_id, member) {
                return Some(sel);
            }
        }
    }
    None
}

/// Single (backward-compat) champion selection from Swiftplay lobby data.
pub async fn get_swiftplay_champion_selection(client: &reqwest::Client, auth: &Auth) -> Option<ChampionSelection> {
    for endpoint in SWIFTPLAY_LOBBY_ENDPOINTS {
        if let Some(data) = shared_cache().get(client, auth, endpoint, DEFAULT_CACHE_TTL).await {
            if let Some(sel) = extract_champion_selection(&data) {
                return Some(sel);
            }
        }
    }
    None
}

fn slot_selection_at(slots: Option<&Vec<Value>>, index: usize, champ_id: i64) -> ChampionSelection {
    let slot = slots.and_then(|s| s.get(index));
    ChampionSelection {
        champion_id: champ_id,
        skin_id: slot.map(|s| value_i64(s, "skinId")).unwrap_or(0),
        position: slot.map(|s| value_str(s, "positionPreference")).unwrap_or_default(),
        spell1: slot.map(|s| value_i64(s, "spell1")).unwrap_or(0),
        spell2: slot.map(|s| value_i64(s, "spell2")).unwrap_or(0),
    }
}

fn extract_dual_champion_selection(data: &Value) -> Option<DualChampionSelection> {
    let local_member = data.get("localMember")?;
    let slots = local_member.get("playerSlots").and_then(Value::as_array);

    let mut champions = Vec::new();
    let primary = value_i64(local_member, "primaryChampionId");
    let secondary = value_i64(local_member, "secondaryChampionId");
    if primary > 0 {
        champions.push(slot_selection_at(slots, 0, primary));
    }
    if secondary > 0 {
        champions.push(slot_selection_at(slots, 1, secondary));
    }

    if champions.is_empty() {
        if let Some(slots) = slots {
            for slot in slots {
                let champ_id = value_i64(slot, "championId");
                if champ_id > 0 {
                    champions.push(ChampionSelection {
                        champion_id: champ_id,
                        skin_id: value_i64(slot, "skinId"),
                        position: value_str(slot, "positionPreference"),
                        spell1: value_i64(slot, "spell1"),
                        spell2: value_i64(slot, "spell2"),
                    });
                }
            }
        }
    }

    if champions.is_empty() {
        return None;
    }
    let champion_1 = champions.first().cloned();
    let champion_2 = champions.get(1).cloned();
    Some(DualChampionSelection { champions, champion_1, champion_2 })
}

/// Both (primary + secondary) champion selections from Swiftplay lobby data.
pub async fn get_swiftplay_dual_champion_selection(client: &reqwest::Client, auth: &Auth) -> Option<DualChampionSelection> {
    for endpoint in SWIFTPLAY_LOBBY_ENDPOINTS {
        if let Some(data) = shared_cache().get(client, auth, endpoint, DEFAULT_CACHE_TTL).await {
            if let Some(sel) = extract_dual_champion_selection(&data) {
                return Some(sel);
            }
        }
    }
    None
}

async fn put_json_with_header(
    client: &reqwest::Client,
    auth: &Auth,
    path: &str,
    body: &Value,
    header_name: &str,
    header_value: &str,
) -> Option<reqwest::StatusCode> {
    shared_cache().invalidate(path);
    let resp = client
        .put(format!("{}{}", auth.base_url, path))
        .header(AUTHORIZATION, &auth.header)
        .header(header_name, header_value)
        .json(body)
        .send()
        .await
        .ok()?;
    Some(resp.status())
}

/// Force base skins (`championId * 1000`) onto tracked-but-unowned Swiftplay
/// player slots, then PUT the modified slots back with the
/// `x-riot-source: rcp-fe-lol-parties` header the LCU requires for this
/// endpoint — ported from `LCUSwiftplay.force_base_skin_slots`.
pub async fn force_base_skin_slots(
    client: &reqwest::Client,
    auth: &Auth,
    skin_tracking: &HashMap<i64, i64>,
    owned_skin_ids: &HashSet<i64>,
) -> bool {
    if skin_tracking.is_empty() {
        return false;
    }
    let Some(lobby) = shared_cache().get(client, auth, "/lol-lobby/v2/lobby", DEFAULT_CACHE_TTL).await else {
        return false;
    };
    let Some(local_member) = lobby.get("localMember") else { return false };
    let Some(slots) = local_member.get("playerSlots").and_then(Value::as_array) else { return false };
    if slots.is_empty() {
        return false;
    }

    let mut modified_slots = slots.clone();
    let mut modified = false;
    for slot in modified_slots.iter_mut() {
        let Some(champ_id) = slot.get("championId").and_then(Value::as_i64) else { continue };
        let Some(&tracked_skin) = skin_tracking.get(&champ_id) else { continue };
        if owned_skin_ids.contains(&tracked_skin) {
            continue;
        }
        let base_skin_id = champ_id * 1000;
        let current_skin = slot.get("skinId").and_then(Value::as_i64);
        if current_skin != Some(base_skin_id) {
            if let Some(obj) = slot.as_object_mut() {
                obj.insert("skinId".to_string(), json!(base_skin_id));
            }
            modified = true;
        }
    }

    if !modified {
        return true;
    }

    let status = put_json_with_header(
        client,
        auth,
        "/lol-lobby/v1/lobby/members/localMember/player-slots",
        &json!(modified_slots),
        "x-riot-source",
        "rcp-fe-lol-parties",
    )
    .await;
    matches!(status, Some(s) if matches!(s.as_u16(), 200 | 201 | 204))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built champ-select session fixture: cell 0 has a completed
    /// pick action (explicit lock, champion 238); cell 1 has no action but
    /// a champion assigned with no pending intent (implicit lock, champion
    /// 103); cell 5 (enemy team) has an intent pending and must NOT lock.
    fn fixture_session() -> SessionData {
        let json = serde_json::json!({
            "localPlayerCellId": 0,
            "myTeam": [
                {"cellId": 0, "championId": 0, "championPickIntent": 0, "isPickIntenting": false},
                {"cellId": 1, "championId": 103, "championPickIntent": 0, "isPickIntenting": false}
            ],
            "theirTeam": [
                {"cellId": 5, "championId": 0, "championPickIntent": 22, "isPickIntenting": true}
            ],
            "actions": [
                [ {"id": 1, "type": "pick", "completed": true, "actorCellId": 0, "championId": 238} ],
                [ {"id": 2, "type": "pick", "completed": false, "actorCellId": 1, "championId": 0} ]
            ]
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn map_cells_indexes_both_teams_by_cell_id() {
        let cells = map_cells(&fixture_session());
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[&0].champion_id, Some(0));
        assert_eq!(cells[&1].champion_id, Some(103));
        assert_eq!(cells[&5].champion_id, Some(0));
    }

    #[test]
    fn compute_locked_combines_explicit_and_implicit_locks() {
        let locked = compute_locked(&fixture_session());
        // Cell 0: explicit lock from the completed pick action (championId 238).
        assert_eq!(locked.get(&0), Some(&238));
        // Cell 1: implicit lock — championId set, no pending intent.
        assert_eq!(locked.get(&1), Some(&103));
        // Cell 5: championId 0 with a pending intent — never locked.
        assert!(!locked.contains_key(&5));
    }

    #[test]
    fn compute_locked_ignores_intenting_cells() {
        let json = serde_json::json!({
            "myTeam": [
                {"cellId": 2, "championId": 55, "championPickIntent": 0, "isPickIntenting": true}
            ],
            "theirTeam": [],
            "actions": []
        });
        let session: SessionData = serde_json::from_value(json).unwrap();
        assert!(compute_locked(&session).is_empty());
    }

    #[test]
    fn levenshtein_matches_known_distances() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("same", "same"), 0);
    }

    #[test]
    fn find_skin_by_text_exact_and_locale_suffix() {
        let mut cache = ChampionSkinCache { champion_id: Some(1), ..Default::default() };
        let skin = SkinInfo { skin_id: 1000, champion_id: 1, skin_name: "Prestige Skin".to_string(), ..Default::default() };
        cache.skin_id_map.insert(1000, skin.clone());
        cache.skin_name_map.insert("Prestige Skin".to_string(), skin.clone());
        cache.skins.push(skin);

        assert_eq!(find_skin_by_text(&cache, "Prestige Skin"), Some((1000, "Prestige Skin".to_string(), 1.0)));
        // Locale chroma suffix stripped as an alternate candidate.
        assert_eq!(
            find_skin_by_text(&cache, "Prestige Skin (Renegado)"),
            Some((1000, "Prestige Skin".to_string(), 1.0))
        );
    }

    // ---------------------------------------------------------------------
    // LCU replay: drive the real skin-name resolution pipeline against a
    // champion payload captured verbatim from a live client
    // (`/lol-game-data/assets/v1/champions/360.json`, Samira). This is the
    // headless regression guard for the exact bug that hit a real user — the
    // client reports a skin by its localized *display name*, and we must
    // resolve it back to the right skin id before injection.
    // ---------------------------------------------------------------------
    const SAMIRA_FIXTURE: &str = include_str!("test_fixtures/champion_360_samira.json");

    fn samira_cache() -> ChampionSkinCache {
        let champ: ChampionData = serde_json::from_str(SAMIRA_FIXTURE).expect("fixture parses as ChampionData");
        build_skin_cache(champ, 360)
    }

    #[test]
    fn lcu_replay_samira_fixture_builds_the_expected_cache() {
        let cache = samira_cache();
        assert_eq!(cache.champion_id, Some(360));
        assert_eq!(cache.champion_name.as_deref(), Some("Samira"));
        assert_eq!(cache.skins.len(), 7, "captured Samira payload had 7 skins");
        // Every skin is round-trippable by its exact name.
        for skin in &cache.skins {
            assert_eq!(
                cache.get_skin_by_name(&skin.skin_name).map(|s| s.skin_id),
                Some(skin.skin_id),
                "skin {:?} must be resolvable by name",
                skin.skin_name
            );
        }
    }

    #[test]
    fn lcu_replay_resolves_soul_fighter_samira_the_way_the_client_reports_it() {
        let cache = samira_cache();
        // Exact display name -> confident id (the wife's case).
        assert_eq!(
            find_skin_by_text(&cache, "Soul Fighter Samira"),
            Some((360030, "Soul Fighter Samira".to_string(), 1.0))
        );
        // Client sometimes appends a localized chroma suffix; still exact id.
        let (id, name, sim) =
            find_skin_by_text(&cache, "Soul Fighter Samira (Renegado)").expect("suffixed name still resolves");
        assert_eq!((id, name.as_str()), (360030, "Soul Fighter Samira"));
        assert_eq!(sim, 1.0);
        // A near-miss (dropped a letter) still lands on the right skin via the
        // Levenshtein fallback, just below full confidence.
        let (fuzzy_id, _, fuzzy_sim) =
            find_skin_by_text(&cache, "Soul Fighter Samra").expect("typo resolves fuzzily");
        assert_eq!(fuzzy_id, 360030);
        assert!(fuzzy_sim > 0.9 && fuzzy_sim < 1.0, "fuzzy match should be high-but-not-perfect, got {fuzzy_sim}");
    }

    #[test]
    fn lcu_replay_resolves_base_samira() {
        let cache = samira_cache();
        let (id, _, sim) = find_skin_by_text(&cache, "Samira").expect("base skin resolves");
        assert_eq!(id, 360000, "base Samira is skin id 360000");
        assert_eq!(sim, 1.0);
    }

    #[test]
    fn locale_chroma_suffix_regex_strips_parenthetical_and_em_dash_quote() {
        let re = locale_chroma_suffix_re();
        assert_eq!(re.replace("PROJECT: Ashe (Prestige)", ""), "PROJECT: Ashe");
        assert_eq!(re.replace("SkinName – ''Пылкость''", ""), "SkinName");
        // No trailing suffix -> untouched.
        assert_eq!(re.replace("Base Ashe", ""), "Base Ashe");
    }

    #[test]
    fn lcu_cache_invalidate_clears_prefix_ancestors_and_descendants() {
        let cache = LcuCache::new();
        {
            let mut entries = cache.entries.lock_safe();
            let far_future = Instant::now() + Duration::from_secs(60);
            entries.insert("/lol-champ-select/v1".to_string(), (far_future, None));
            entries.insert("/lol-champ-select/v1/session".to_string(), (far_future, None));
            entries.insert("/lol-champ-select/v1/session/actions/1".to_string(), (far_future, None));
            entries.insert("/lol-gameflow/v1/gameflow-phase".to_string(), (far_future, None));
        }
        cache.invalidate("/lol-champ-select/v1/session");
        let entries = cache.entries.lock_safe();
        assert!(!entries.contains_key("/lol-champ-select/v1")); // ancestor cleared
        assert!(!entries.contains_key("/lol-champ-select/v1/session")); // exact match cleared
        assert!(!entries.contains_key("/lol-champ-select/v1/session/actions/1")); // descendant cleared
        assert!(entries.contains_key("/lol-gameflow/v1/gameflow-phase")); // unrelated path kept
    }

    #[test]
    fn lcu_cache_expires_entries_after_ttl() {
        let cache = LcuCache::new();
        {
            let mut entries = cache.entries.lock_safe();
            let already_expired = Instant::now() - Duration::from_secs(1);
            entries.insert("/expired".to_string(), (already_expired, Some(json!({"a": 1}))));
        }
        assert_eq!(cache.cached("/expired"), None); // treated as a miss, and evicted
        assert!(!cache.entries.lock_safe().contains_key("/expired"));
    }

    #[test]
    fn game_mode_info_matches_magic_map_ids() {
        let aram = GameModeInfo { map_id: Some(12), ..Default::default() };
        assert!(aram.is_aram());
        let sr = GameModeInfo { map_id: Some(11), ..Default::default() };
        assert!(sr.is_sr());
    }
}
