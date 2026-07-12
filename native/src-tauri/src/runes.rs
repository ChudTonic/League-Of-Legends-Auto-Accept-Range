//! Rune / summoner-spell / item-build auto-import.
//!
//! No Riot Web API key is involved. Two ungated layers do all the work:
//!   * **LCU** (local client API, lockfile auth) writes the rune page, the
//!     champ-select summoner spells, and the item set — the exact mechanism
//!     OP.GG/Blitz/Championify use (a live client here already had an
//!     "OP.GG aram …" page, confirming the endpoint + shape).
//!   * A **Cloudflare Worker** (`runes_endpoint`) fetches the current-patch
//!     "best" build from a stats aggregator (u.gg / lolalytics), normalizes it
//!     to the [`RuneBuild`] shape below, and caches it. The client is decoupled
//!     from whichever site the Worker scrapes — it only speaks this contract:
//!
//! ```json
//! { "name": "Ahri — mid",
//!   "runes":  { "primary": 8100, "sub": 8000,
//!               "perks": [8112,8143,8138,8135, 8009,8014, 5008,5008,5001] },
//!   "spells": [4, 14],
//!   "items":  { "blocks": [ { "type": "Starting", "items": [1055,2003] },
//!                           { "type": "Core",     "items": [6653,3020,3157] } ] } }
//! ```
//!
//! `perks` is always 9 IDs: keystone, 3 primary, 2 secondary, 3 stat shards —
//! exactly what `POST /lol-perks/v1/pages`'s `selectedPerkIds` wants.

use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::lcu::{self, Auth};

/// Rune page name prefix — we reuse a single page tagged this way so imports
/// never pile up pages or clobber the user's own (their non-Chud pages are
/// left untouched; only a slot for OURS is reclaimed when at cap).
const PAGE_PREFIX: &str = "Chud";

/// Normalized build, produced by the Worker, applied by the client.
#[derive(Debug, Clone, Deserialize)]
pub struct RuneBuild {
    #[serde(default)]
    pub name: String,
    pub runes: Runes,
    /// `[spell1Id, spell2Id]`. Optional — some sources omit spells.
    #[serde(default)]
    pub spells: Vec<i64>,
    #[serde(default)]
    pub items: Option<ItemBlocks>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Runes {
    pub primary: i64,
    pub sub: i64,
    /// 9 IDs: keystone, 3 primary, 2 secondary, 3 stat shards.
    pub perks: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemBlocks {
    pub blocks: Vec<ItemBlock>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemBlock {
    #[serde(rename = "type", default)]
    pub label: String,
    pub items: Vec<i64>,
}

impl RuneBuild {
    fn is_sane(&self) -> bool {
        // A valid League rune page is exactly 9 perks with real style IDs.
        self.runes.primary > 0 && self.runes.sub > 0 && self.runes.perks.len() == 9
    }
}

/// Fetch the current-patch best build for `champion_id` in `role` from the
/// Worker. `role` is one of top/jungle/mid/bot/support (empty = let the Worker
/// pick the champ's most-played role). Returns `None` on any failure.
pub async fn fetch_build(
    http: &reqwest::Client,
    runes_endpoint: &str,
    champion_id: i64,
    role: &str,
    sort: &str,
) -> Option<RuneBuild> {
    if runes_endpoint.trim().is_empty() {
        return None;
    }
    // Patch is intentionally NOT sent — the Worker resolves the current patch
    // itself (from Data Dragon) so the client can't send a stale one.
    let url = format!(
        "{}?champ={champion_id}&role={}&sort={}",
        runes_endpoint.trim_end_matches('/'),
        urlencode(role),
        urlencode(sort),
    );
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let build: RuneBuild = resp.json().await.ok()?;
    build.is_sane().then_some(build)
}

/// Apply a build to the running client: rune page (always), summoner spells
/// (only meaningful during champ select), and item set. Best-effort per part —
/// a failure in one doesn't abort the others. Returns which parts succeeded.
pub async fn apply_build(http: &reqwest::Client, auth: &Auth, build: &RuneBuild) -> Applied {
    let runes = apply_runes(http, auth, build).await;
    let spells = if build.spells.len() == 2 { apply_spells(http, auth, &build.spells).await } else { false };
    let items = match &build.items {
        Some(blocks) if !blocks.blocks.is_empty() => apply_items(http, auth, &build.name, blocks).await,
        _ => false,
    };
    Applied { runes, spells, items }
}

#[derive(Debug, Clone, Copy)]
pub struct Applied {
    pub runes: bool,
    pub spells: bool,
    pub items: bool,
}

/// Create + activate the rune page. Reuses a single `Chud …` page so repeated
/// imports never accumulate; frees a slot at cap by deleting our old page (or,
/// only if we have none and you're at cap, the current editable+deletable one).
async fn apply_runes(http: &reqwest::Client, auth: &Auth, build: &RuneBuild) -> bool {
    let pages = lcu::get_json(http, auth, "/lol-perks/v1/pages").await.and_then(|v| v.as_array().cloned());
    let owned = lcu::get_json(http, auth, "/lol-perks/v1/inventory")
        .await
        .and_then(|v| v.get("ownedPageCount").and_then(Value::as_i64))
        .unwrap_or(2);

    if let Some(pages) = &pages {
        // Delete any prior Chud page so we keep exactly one.
        for p in pages.iter().filter(|p| page_name(p).starts_with(PAGE_PREFIX)) {
            if let Some(id) = p.get("id").and_then(Value::as_i64) {
                let _ = lcu::request_json(http, auth, Method::DELETE, &format!("/lol-perks/v1/pages/{id}"), None).await;
            }
        }
        // If still at cap after that, reclaim a slot from the current editable
        // page (never a non-deletable default one).
        let remaining = pages.iter().filter(|p| !page_name(p).starts_with(PAGE_PREFIX)).count() as i64;
        if remaining >= owned {
            if let Some(id) = pages
                .iter()
                .find(|p| {
                    p.get("current").and_then(Value::as_bool).unwrap_or(false)
                        && p.get("isDeletable").and_then(Value::as_bool).unwrap_or(false)
                        && !page_name(p).starts_with(PAGE_PREFIX)
                })
                .and_then(|p| p.get("id").and_then(Value::as_i64))
            {
                let _ = lcu::request_json(http, auth, Method::DELETE, &format!("/lol-perks/v1/pages/{id}"), None).await;
            }
        }
    }

    let body = json!({
        "name": format!("{PAGE_PREFIX} — {}", if build.name.is_empty() { "build" } else { &build.name }),
        "primaryStyleId": build.runes.primary,
        "subStyleId": build.runes.sub,
        "selectedPerkIds": build.runes.perks,
        "current": true,
    });
    lcu::request_json(http, auth, Method::POST, "/lol-perks/v1/pages", Some(&body)).await.is_some()
}

/// Set summoner spells via champ-select `my-selection` (no-op outside champ
/// select — the endpoint 404s, which we treat as "not applied").
async fn apply_spells(http: &reqwest::Client, auth: &Auth, spells: &[i64]) -> bool {
    let body = json!({ "spell1Id": spells[0], "spell2Id": spells[1] });
    lcu::request_json(http, auth, Method::PATCH, "/lol-champ-select/v1/session/my-selection", Some(&body))
        .await
        .is_some()
}

/// Write an item set for the champion so the in-game shop shows the build.
async fn apply_items(http: &reqwest::Client, auth: &Auth, name: &str, blocks: &ItemBlocks) -> bool {
    let Some(summoner_id) =
        lcu::get_json(http, auth, "/lol-summoner/v1/current-summoner").await.and_then(|v| v.get("summonerId").and_then(Value::as_i64))
    else {
        return false;
    };
    if summoner_id <= 0 {
        return false;
    }
    let set_blocks: Vec<Value> = blocks
        .blocks
        .iter()
        .map(|b| {
            json!({
                "type": b.label,
                "items": b.items.iter().map(|id| json!({ "id": id.to_string(), "count": 1 })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let body = json!({
        "itemSets": [ { "title": format!("{PAGE_PREFIX} — {name}"), "type": "custom", "map": "any", "mode": "any", "blocks": set_blocks } ],
        "timestamp": 0,
    });
    lcu::request_json(http, auth, Method::PUT, &format!("/lol-item-sets/v1/item-sets/{summoner_id}/sets"), Some(&body))
        .await
        .is_some()
}

/// Read the champion you've locked/hovered + your assigned role from the live
/// champ-select session. Returns `None` when not in champ select or no champ is
/// picked yet.
pub async fn locked_champ_and_role(http: &reqwest::Client, auth: &Auth) -> Option<(i64, String)> {
    let session = lcu::get_json(http, auth, "/lol-champ-select/v1/session").await?;
    let my_cell = session.get("localPlayerCellId").and_then(Value::as_i64)?;
    let team = session.get("myTeam").and_then(Value::as_array)?;
    let me = team.iter().find(|m| m.get("cellId").and_then(Value::as_i64) == Some(my_cell))?;
    let champ = me.get("championId").and_then(Value::as_i64).filter(|c| *c > 0)?;
    let role = normalize_role(me.get("assignedPosition").and_then(Value::as_str).unwrap_or(""));
    Some((champ, role))
}

/// Map LCU `assignedPosition` values to the roles the Worker expects. Empty =
/// unknown (blind pick / ARAM); the Worker then falls back to the champion's
/// most-played role.
fn normalize_role(pos: &str) -> String {
    match pos.to_ascii_lowercase().as_str() {
        "top" => "top",
        "jungle" => "jungle",
        "middle" | "mid" => "mid",
        "bottom" | "bot" | "adc" => "bot",
        "utility" | "support" => "support",
        _ => "",
    }
    .to_string()
}

/// End-to-end: detect the locked champion + role, fetch the current-patch best
/// build from the Worker, and apply it. Returns what was applied (all-false if
/// not in champ select, no champ picked, or the Worker had no build).
pub async fn import_now(http: &reqwest::Client, auth: &Auth, endpoint: &str, sort: &str) -> Applied {
    let none = Applied { runes: false, spells: false, items: false };
    let Some((champ, role)) = locked_champ_and_role(http, auth).await else { return none };
    let Some(build) = fetch_build(http, endpoint, champ, &role, sort).await else { return none };
    apply_build(http, auth, &build).await
}

fn page_name(p: &Value) -> &str {
    p.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Minimal query-string escaping for the few characters that appear in a role
/// or patch string — avoids pulling in a URL-encoding dependency for this.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => c.to_string(),
            ' ' => "%20".to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sanity_requires_nine_perks_and_styles() {
        let ok: RuneBuild = serde_json::from_value(json!({
            "name": "Ahri",
            "runes": {"primary": 8100, "sub": 8000, "perks": [8112,8143,8138,8135,8009,8014,5008,5008,5001]},
            "spells": [4, 14]
        }))
        .unwrap();
        assert!(ok.is_sane());

        let short: RuneBuild = serde_json::from_value(json!({
            "runes": {"primary": 8100, "sub": 8000, "perks": [8112,8143]}
        }))
        .unwrap();
        assert!(!short.is_sane());

        let no_style: RuneBuild = serde_json::from_value(json!({
            "runes": {"primary": 0, "sub": 8000, "perks": [1,2,3,4,5,6,7,8,9]}
        }))
        .unwrap();
        assert!(!no_style.is_sane());
    }

    #[test]
    fn urlencode_keeps_safe_and_escapes_rest() {
        assert_eq!(urlencode("mid"), "mid");
        assert_eq!(urlencode("14.20"), "14.20");
        assert_eq!(urlencode("a b"), "a%20b");
    }
}
