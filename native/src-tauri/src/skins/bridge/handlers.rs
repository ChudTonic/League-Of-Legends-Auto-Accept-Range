//! Inbound bridge message handlers (S4) — ported from `pengu\communication\
//! message_handler.py`'s `MessageHandler`. Every handler's response/side
//! effect goes out through `BridgeContext::handle`'s broadcast (never a
//! targeted reply — see `ws.rs`'s doc comment on the broadcast-only
//! contract).
//!
//! Party mode (`party-*`) is S6: these handlers call into the real
//! `skins::party::manager::PartyManager` (pulled from `AppState::
//! skins_party`, set once in `lib.rs`'s `setup()`) rather than reimplementing
//! any party logic here — see `handlers::party_manager` and the handlers
//! below it.
//!
//! A few Python collaborators this file would otherwise call into were never
//! ported to Rust and are out of this milestone's file scope to create
//! (`ui/core/user_interface.py`, `pengu/processing/skin_processor.py`,
//! `pengu/processing/flow_controller.py`, `utils/system/admin_utils.py`,
//! `utils/core/issue_reporter.py`). Where a handler depends on one, this
//! file reimplements the essential wire-visible behavior directly against
//! already-ported primitives (`lcu_ext`, `features::*`, `injection::*`)
//! rather than inventing a shadow copy of the missing module — each such
//! spot has a comment calling out the simplification.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Manager;

use crate::lcu;
use crate::skins::features::{chroma, historic, random as random_feature};
use crate::skins::injection::storage::{self, CategoryModEntry};
use crate::skins::injection::{base_skin_tracker, zips};
use crate::skins::lcu_ext::{self, ChampionSkinCache};
use crate::skins::paths;
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::state::{CategoryModSelection, CustomModSelection};
use crate::LockExt;

use super::protocol::{self, Inbound, InboundMessage};
use super::BridgeContext;

// ---------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------

/// Decode + route one WebSocket text frame (called from `ws::handle_socket`).
pub async fn dispatch(ctx: &BridgeContext, text: &str) {
    match protocol::decode(text) {
        Some(Inbound::SkinHover(skin)) => handle_skin_hover(ctx, skin).await,
        Some(Inbound::Message(msg)) => route(ctx, msg).await,
        None => {}
    }
}

async fn route(ctx: &BridgeContext, msg: InboundMessage) {
    match msg {
        InboundMessage::ChromaLog { source, event, message, data } => handle_chroma_log(source, event, message, data),
        InboundMessage::RequestLocalPreview { champion_id, skin_id, chroma_id } => {
            handle_request_local_preview(ctx, champion_id, skin_id, chroma_id).await
        }
        InboundMessage::RequestLocalAsset { asset_path, chroma_id } => {
            handle_request_local_asset(ctx, asset_path, chroma_id).await
        }
        InboundMessage::ChromaSelection { chroma_id, skin_id, chroma_name } => {
            handle_chroma_selection(ctx, chroma_id, skin_id, chroma_name).await
        }
        InboundMessage::DiceButtonClick { state } => handle_dice_button_click(ctx, state).await,
        InboundMessage::SettingsRequest {} => handle_settings_request(ctx).await,
        InboundMessage::PathValidate { game_path } => handle_path_validate(ctx, game_path).await,
        InboundMessage::OpenModsFolder {} => handle_open_mods_folder(),
        InboundMessage::RequestSkinMods { skin_id, champion_id } => {
            handle_request_skin_mods(ctx, skin_id, champion_id).await
        }
        InboundMessage::RequestMaps {} => handle_request_maps(ctx).await,
        InboundMessage::RequestFonts {} => handle_request_fonts(ctx).await,
        InboundMessage::RequestAnnouncers {} => handle_request_announcers(ctx).await,
        InboundMessage::RequestCategoryMods { category } => handle_request_category_mods(ctx, category).await,
        // Legacy alias — routed identically to `RequestCategoryMods { category: "others" }`
        // in the Python original (`_handle_request_others` itself is dead code, never
        // reached by the routing table — see this file's module doc).
        InboundMessage::RequestOthers {} => {
            handle_request_category_mods(ctx, Some(storage::CATEGORY_OTHERS.to_string())).await
        }
        InboundMessage::SelectSkinMod { champion_id, skin_id, mod_id, .. } => {
            handle_select_skin_mod(ctx, champion_id, skin_id, mod_id).await
        }
        InboundMessage::SelectMap { map_id, map_data } => {
            select_single_slot_mod(ctx, Slot::Map, map_id, map_data).await
        }
        InboundMessage::SelectFont { font_id, font_data } => {
            select_single_slot_mod(ctx, Slot::Font, font_id, font_data).await
        }
        InboundMessage::SelectAnnouncer { announcer_id, announcer_data } => {
            select_single_slot_mod(ctx, Slot::Announcer, announcer_id, announcer_data).await
        }
        InboundMessage::SelectOther { other_id, other_data, action } => {
            handle_select_other(ctx, other_id, other_data, action).await
        }
        InboundMessage::OpenLogsFolder {} => handle_open_logs_folder(),
        InboundMessage::DiagnosticsRequest {} => handle_diagnostics_request(ctx),
        InboundMessage::DiagnosticsClear {} => handle_diagnostics_clear(ctx),
        InboundMessage::DiagnosticsClearCategory { categories, category } => {
            handle_diagnostics_clear_category(ctx, categories, category)
        }
        InboundMessage::DiagnosticsClearTracker {} => handle_diagnostics_clear_tracker(ctx),
        InboundMessage::DiagnosticsApplyRecommended {} => handle_diagnostics_apply_recommended(ctx),
        InboundMessage::OpenPenguLoaderUi {} => handle_open_pengu_loader_ui(),
        InboundMessage::SettingsSave { threshold, monitor_auto_resume_timeout, autostart, game_path } => {
            handle_settings_save(ctx, threshold, monitor_auto_resume_timeout, autostart, game_path)
        }
        InboundMessage::AddCustomModsCategorySelected { category } => {
            handle_add_custom_mods_category_selected(ctx, category)
        }
        InboundMessage::AddCustomModsChampionSelected { action } => {
            handle_add_custom_mods_champion_selected(ctx, action).await
        }
        InboundMessage::AddCustomModsSkinSelected { action, champion_id, skin_id } => {
            handle_add_custom_mods_skin_selected(ctx, action, champion_id, skin_id).await
        }
        InboundMessage::FindMatchHover { .. } => handle_find_match_hover(ctx).await,
        InboundMessage::DismissCustomMod {} => handle_dismiss_custom_mod(ctx),
        InboundMessage::DismissHistoric {} => handle_dismiss_historic(ctx),
        InboundMessage::PartyEnable {} => handle_party_enable(ctx).await,
        InboundMessage::PartyDisable {} => handle_party_disable(ctx).await,
        InboundMessage::PartyAddPeer { token } => handle_party_add_peer(ctx, token).await,
        InboundMessage::PartyRemovePeer { summoner_id } => handle_party_remove_peer(ctx, summoner_id),
        InboundMessage::PartyGetState {} => handle_party_get_state(ctx),
    }
}

// ---------------------------------------------------------------------
// Legacy type-less skin-hover message — `_handle_skin_detection`.
//
// SIMPLIFICATION: Python delegated to `SkinProcessor`/`FlowController`
// (neither ported — see module doc). This reimplements the essential path
// directly: resolve the hover text to a skin ID via the already-ported
// `lcu_ext::find_skin_by_text` scraper helper, update `SkinsShared`, and
// broadcast `skin-state`. The `flow_controller.should_process_payload()`
// gate (phase/timing-based suppression) is not reproduced.
// ---------------------------------------------------------------------

async fn handle_skin_hover(ctx: &BridgeContext, skin_name: String) {
    let trimmed = skin_name.trim().to_string();
    if trimmed.is_empty() {
        return;
    }

    // Always remember the raw hover text (mirrors Python's unconditional
    // `ui_last_text` write), and use it to dedupe identical back-to-back
    // hovers the same way `SkinProcessor.last_skin_name` did.
    let previous = {
        let mut shared = ctx.skins.shared.lock_safe();
        let previous = shared.ui_last_text.clone();
        shared.ui_last_text = Some(trimmed.clone());
        previous
    };
    if previous.as_deref() == Some(trimmed.as_str()) {
        return;
    }

    let cache = cache_for_locked_champion(ctx).await;
    let (skin_id, resolved_name) = match &cache {
        Some(c) => match lcu_ext::find_skin_by_text(c, &trimmed) {
            Some((id, name, _similarity)) => (Some(id), name),
            None => (None, trimmed.clone()),
        },
        None => (None, trimmed.clone()),
    };

    {
        let mut shared = ctx.skins.shared.lock_safe();
        shared.last_hovered_skin_key = Some(resolved_name.clone());
        shared.last_hovered_skin_id = skin_id;
        shared.last_hovered_skin_slug = skin_id.map(|id| format!("skin_{id}"));
    }

    let has_chromas = skin_has_chromas(cache.as_ref(), skin_id);
    ctx.handle.broadcast_skin_state(resolved_name, skin_id, has_chromas);
}

/// `Broadcaster._skin_has_chromas`, ported verbatim (special-case ID lists
/// first, then a fresh scrape's `chroma_id_map`/chromas-for-skin lookup).
const SPECIAL_BASE_SKIN_IDS: [i64; 3] = [99007, 145070, 103085];
const SPECIAL_CHROMA_SKIN_IDS: [i64; 5] = [145071, 100001, 103086, 103087, 88888];

fn skin_has_chromas(cache: Option<&ChampionSkinCache>, skin_id: Option<i64>) -> bool {
    let Some(skin_id) = skin_id else { return false };
    if skin_id == 99007 || (99991..=99999).contains(&skin_id) {
        return true;
    }
    if SPECIAL_BASE_SKIN_IDS.contains(&skin_id) || SPECIAL_CHROMA_SKIN_IDS.contains(&skin_id) {
        return true;
    }
    let Some(cache) = cache else { return false };
    if cache.is_chroma(skin_id) {
        return true;
    }
    cache.get_chromas_for_skin(skin_id).map(|c| !c.is_empty()).unwrap_or(false)
}

/// Fresh (uncached beyond `lcu_ext`'s own 200ms shared-GET cache) scrape for
/// the currently-locked champion — the champion skin cache the phase actor
/// (S2) holds is local to that task, not shared state this milestone's file
/// scope can reach into; `scrape_champion_skins` is designed for exactly
/// this always-fresh-fetch call pattern (see its own doc comment).
async fn cache_for_locked_champion(ctx: &BridgeContext) -> Option<ChampionSkinCache> {
    let champion_id = { ctx.skins.shared.lock_safe().locked_champ_id }?;
    let auth = lcu::cached_auth()?;
    lcu_ext::scrape_champion_skins(&ctx.http_client, &auth, champion_id).await
}

// ---------------------------------------------------------------------
// Chroma / random skin
// ---------------------------------------------------------------------

fn handle_chroma_log(source: Option<String>, event: Option<String>, message: Option<String>, data: Option<Value>) {
    let source = source.unwrap_or_else(|| "ChromaWheel".to_string());
    let event = event.or(message).unwrap_or_else(|| "unknown".to_string());
    match data {
        Some(d) => log_info!("[{source}] {event} | {d}"),
        None => log_info!("[{source}] {event}"),
    }
}

async fn handle_chroma_selection(ctx: &BridgeContext, chroma_id: Option<i64>, skin_id: Option<i64>, chroma_name: Option<String>) {
    let Some(chroma_id) = chroma_id.or(skin_id) else { return };
    let chroma_name = chroma_name.unwrap_or_else(|| "Unknown".to_string());

    let cache = cache_for_locked_champion(ctx).await;
    // "current_skin_id" (the base skin the wheel was shown for) has no
    // shared-state home in this port — the Python `ChromaPanelManager` that
    // tracked it wasn't ported (see `features::chroma`'s doc comment).
    // The locked champion's base skin is the closest available proxy.
    let current_skin_id = { ctx.skins.shared.lock_safe().locked_champ_id.map(|c| c * 1000).unwrap_or(0) };

    let selected_chroma_id = {
        let mut shared = ctx.skins.shared.lock_safe();
        chroma::handle_selection(&mut shared, cache.as_ref(), current_skin_id, chroma_id, &chroma_name);
        shared.selected_chroma_id
    };

    log_info!("[bridge] Chroma selected: {chroma_name} (ID: {chroma_id})");
    ctx.handle.broadcast_chroma_state(selected_chroma_id);
}

async fn handle_dice_button_click(ctx: &BridgeContext, state: Option<String>) {
    let state = state.unwrap_or_else(|| "disabled".to_string());
    log_info!("[bridge] Dice button clicked: state={state}");

    match state.as_str() {
        "enabled" => {
            let Some(cache) = cache_for_locked_champion(ctx).await else {
                log_warn!("[bridge] Dice click: no champion locked or LCU unavailable");
                return;
            };
            let (active, random_skin_id) = {
                let mut shared = ctx.skins.shared.lock_safe();
                random_feature::start_randomization(&mut shared, &cache);
                (shared.random_mode_active, shared.random_skin_id)
            };
            ctx.handle.broadcast_random_mode_state(active, random_skin_id);
        }
        "disabled" => {
            {
                let mut shared = ctx.skins.shared.lock_safe();
                random_feature::cancel_randomization(&mut shared);
            }
            ctx.handle.broadcast_random_mode_state(false, None);
        }
        other => log_warn!("[bridge] Unknown dice button state: {other}"),
    }
}

/// `_handle_find_match_hover`: force base skins immediately. SIMPLIFICATION:
/// Python called an external `force_base_skins_callback` (not ported); this
/// reimplements the intent directly against `lcu_ext`'s already-ported PATCH
/// helpers and starts the base-skin confirmation tracker.
async fn handle_find_match_hover(ctx: &BridgeContext) {
    let Some(auth) = lcu::cached_auth() else { return };
    let (champion_id, is_swiftplay, tracking, owned) = {
        let shared = ctx.skins.shared.lock_safe();
        (shared.locked_champ_id, shared.is_swiftplay_mode, shared.swiftplay_skin_tracking.clone(), shared.owned_skin_ids.clone())
    };
    let Some(champion_id) = champion_id else { return };
    let base_skin_id = champion_id * 1000;

    if lcu_ext::set_my_selection_skin(&ctx.http_client, &auth, base_skin_id).await {
        base_skin_tracker::start_tracking(base_skin_id);
    }
    if is_swiftplay {
        lcu_ext::force_base_skin_slots(&ctx.http_client, &auth, &tracking, &owned).await;
    }
}

// ---------------------------------------------------------------------
// Local preview / asset URLs
// ---------------------------------------------------------------------

async fn handle_request_local_preview(ctx: &BridgeContext, champion_id: Option<i64>, skin_id: Option<i64>, chroma_id: Option<i64>) {
    let (Some(champion_id), Some(skin_id), Some(chroma_id)) = (champion_id, skin_id, chroma_id) else { return };

    let preview_path = if chroma_id == skin_id {
        paths::skins_dir().join(champion_id.to_string()).join(skin_id.to_string()).join(format!("{skin_id}.png"))
    } else {
        paths::skins_dir()
            .join(champion_id.to_string())
            .join(skin_id.to_string())
            .join(chroma_id.to_string())
            .join(format!("{chroma_id}.png"))
    };
    if !preview_path.exists() {
        return;
    }

    let url = format!("http://localhost:{}/preview/{champion_id}/{skin_id}/{chroma_id}/{chroma_id}.png", ctx.handle.port());
    ctx.handle.broadcast_json(json!({
        "type": "local-preview-url",
        "championId": champion_id,
        "skinId": skin_id,
        "chromaId": chroma_id,
        "url": url,
        "timestamp": protocol::now_ms(),
    }));
}

async fn handle_request_local_asset(ctx: &BridgeContext, asset_path: Option<String>, chroma_id: Option<i64>) {
    let Some(asset_path) = asset_path else { return };
    let asset_file = paths::get_asset_path(&asset_path);
    if !asset_file.exists() {
        return;
    }

    let encoded = percent_encode_path(&asset_path);
    let url = format!("http://localhost:{}/asset/{encoded}", ctx.handle.port());
    ctx.handle.broadcast_json(json!({
        "type": "local-asset-url",
        "assetPath": asset_path,
        "chromaId": chroma_id,
        "url": url,
        "timestamp": protocol::now_ms(),
    }));
}

/// `urllib.parse.quote(asset_path.replace("\\", "/"), safe="/")` — a minimal
/// self-contained percent-encoder rather than a new crate dependency.
fn percent_encode_path(path: &str) -> String {
    path.replace('\\', "/").split('/').map(percent_encode_segment).collect::<Vec<_>>().join("/")
}

fn percent_encode_segment(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------
// Custom skin mod selection (select-time extraction)
// ---------------------------------------------------------------------

/// Extract `source` into `paths::injection_mods_dir()` immediately (ported
/// from the identical extraction block duplicated across `_handle_select_
/// skin_mod`/`_handle_select_map`/`_handle_select_font`/`_handle_select_
/// announcer`/`_handle_select_other`) — select-TIME extraction, not
/// inject-time, so the JS UI's "mod is ready" assumption holds the instant
/// selection succeeds.
struct ExtractedMod {
    mod_folder_name: String,
    mod_path: String,
    relative_path: String,
}

fn extract_selected_mod(ctx: &BridgeContext, source: &Path) -> Option<ExtractedMod> {
    if !source.exists() {
        log_error!("[bridge] Mod file not found: {}", source.display());
        return None;
    }

    let mod_folder_name = if source.is_dir() {
        source.file_name().map(|n| n.to_string_lossy().into_owned())
    } else {
        source.file_stem().map(|n| n.to_string_lossy().into_owned())
    }
    .unwrap_or_else(|| "mod".to_string());

    let dest = paths::injection_mods_dir().join(&mod_folder_name);
    if dest.exists() {
        zips::safe_remove_entry(&dest);
    }
    let cache_dir = paths::injection_extract_cache_dir();
    if let Err(e) = zips::link_or_extract(source, &dest, &cache_dir) {
        log_error!("[bridge] Failed to extract mod {}: {e}", source.display());
        return None;
    }
    log_info!("[bridge] Linked/extracted mod to: {}", dest.display());

    Some(ExtractedMod {
        mod_folder_name,
        mod_path: source.to_string_lossy().into_owned(),
        relative_path: relative_path_of(ctx, source),
    })
}

fn relative_path_of(ctx: &BridgeContext, path: &Path) -> String {
    path.strip_prefix(ctx.mod_storage.mods_root()).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

async fn handle_select_skin_mod(ctx: &BridgeContext, champion_id: Option<i64>, skin_id: Option<i64>, mod_id: Option<String>) {
    let Some(skin_id) = skin_id else { return };
    let champion_id = champion_id.unwrap_or_else(|| skin_id / 1000);
    if champion_id <= 0 {
        log_warn!("[bridge] Invalid mod selection payload: championId={champion_id} skinId={skin_id}");
        return;
    }

    let Some(mod_id) = mod_id else {
        deselect_skin_mod(ctx, skin_id);
        return;
    };

    let entries = ctx.mod_storage.list_mods_for_champion(champion_id);
    let Some(entry) = entries.iter().find(|e| e.mod_name == mod_id || relative_path_of(ctx, &e.path) == mod_id) else {
        log_warn!("[bridge] Mod not found: {mod_id} for champion {champion_id}");
        return;
    };

    let Some(extracted) = extract_selected_mod(ctx, &entry.path) else { return };

    let was_historic_active = {
        let mut shared = ctx.skins.shared.lock_safe();
        shared.selected_custom_mod = Some(CustomModSelection {
            skin_id: entry.skin_id,
            champion_id,
            mod_name: extracted.mod_folder_name.clone(),
            mod_path: extracted.mod_path.clone(),
            relative_path: extracted.relative_path.clone(),
        });
        let was_active = shared.historic_mode_active;
        if was_active {
            shared.historic_mode_active = false;
            shared.historic_skin_id = None;
        }
        was_active
    };
    if was_historic_active {
        log_info!("[bridge] Historic mode disabled due to custom mod selection");
        ctx.handle.broadcast_historic_state(false, None, None);
    }

    log_info!("[bridge] Custom mod selected and extracted: {} (target skin {})", extracted.mod_folder_name, entry.skin_id);
    ctx.handle.broadcast_custom_mod_state(true, Some(extracted.mod_folder_name), Some(entry.skin_id));
}

fn deselect_skin_mod(ctx: &BridgeContext, skin_id: i64) {
    let cleared = {
        let mut shared = ctx.skins.shared.lock_safe();
        match &shared.selected_custom_mod {
            Some(sel) if sel.skin_id == skin_id => shared.selected_custom_mod.take(),
            _ => None,
        }
    };
    let Some(sel) = cleared else { return };

    clear_historic_if_matches(sel.champion_id, &sel.relative_path);
    zips::safe_remove_entry(&paths::injection_mods_dir().join(&sel.mod_name));
    log_info!("[bridge] Custom mod deselected for skin {skin_id}");
    ctx.handle.broadcast_custom_mod_state(false, None, None);
}

fn clear_historic_if_matches(champion_id: i64, relative_path: &str) {
    let Some(historic_value) = historic::get_historic_skin_for_champion(champion_id) else { return };
    let Some(historic_path) = historic_value.custom_mod_path() else { return };
    if historic_path.replace('\\', "/") == relative_path.replace('\\', "/") {
        historic::clear_historic_entry(champion_id);
        log_info!("[bridge] Cleared saved custom mod for champion {champion_id}");
    }
}

fn handle_dismiss_custom_mod(ctx: &BridgeContext) {
    let selection = { ctx.skins.shared.lock_safe().selected_custom_mod.take() };
    let Some(selection) = selection else { return };

    zips::safe_remove_entry(&paths::injection_mods_dir().join(&selection.mod_name));
    clear_historic_if_matches(selection.champion_id, &selection.relative_path);

    log_info!("[bridge] Custom mod dismissed via popup close button");
    ctx.handle.broadcast_custom_mod_state(false, None, None);
}

fn handle_dismiss_historic(ctx: &BridgeContext) {
    {
        let mut shared = ctx.skins.shared.lock_safe();
        shared.historic_mode_active = false;
        shared.historic_skin_id = None;
        shared.historic_first_detection_done = false;
    }
    log_info!("[bridge] Historic mode dismissed via popup close button");
    ctx.handle.broadcast_historic_state(false, None, None);
}

// ---------------------------------------------------------------------
// Map / font / announcer / other mod selection
//
// MIGRATED (S5 follow-up): these selections used to live in a bridge-local
// `BridgeContext::mod_selections` (`bridge::ModSelections`) because
// `state.rs` wasn't in S4's file scope. They now write straight into
// `ctx.skins.shared.lock_safe().category_mods` (`state::CategoryModSelections`)
// — the field `trigger.rs`'s injection trigger actually reads as
// `extra_mod_names` — so a bridge-driven selection is visible to injection
// without a second copy to keep in sync.
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Slot {
    Map,
    Font,
    Announcer,
}

impl Slot {
    fn category(self) -> &'static str {
        match self {
            Slot::Map => storage::CATEGORY_MAPS,
            Slot::Font => storage::CATEGORY_FONTS,
            Slot::Announcer => storage::CATEGORY_ANNOUNCERS,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Slot::Map => "map",
            Slot::Font => "font",
            Slot::Announcer => "announcer",
        }
    }
}

fn get_historic_single(slot: Slot) -> Option<String> {
    let h = historic::load_mod_historic();
    match slot {
        Slot::Map => h.map,
        Slot::Font => h.font,
        Slot::Announcer => h.announcer,
    }
}

fn set_historic_single(slot: Slot, path: Option<String>) {
    let mut h = historic::load_mod_historic();
    match slot {
        Slot::Map => h.map = path,
        Slot::Font => h.font = path,
        Slot::Announcer => h.announcer = path,
    }
    historic::write_mod_historic(&h);
}

fn category_entry_json(e: &CategoryModEntry) -> Value {
    json!({
        "id": e.id,
        "name": e.name,
        "path": e.path,
        "updatedAt": (e.updated_at * 1000.0) as i64,
        "description": e.description,
    })
}

fn category_entries_json(entries: &[CategoryModEntry]) -> Vec<Value> {
    entries.iter().map(category_entry_json).collect()
}

async fn select_single_slot_mod(ctx: &BridgeContext, slot: Slot, mod_id: Option<String>, mod_data: Option<Value>) {
    let Some(mod_id) = mod_id else {
        let had = {
            let mut shared = ctx.skins.shared.lock_safe();
            let field = match slot {
                Slot::Map => &mut shared.category_mods.map,
                Slot::Font => &mut shared.category_mods.font,
                Slot::Announcer => &mut shared.category_mods.announcer,
            };
            field.take().is_some()
        };
        if had {
            set_historic_single(slot, None);
            log_info!("[bridge] {} mod deselected", slot.label());
        }
        return;
    };

    let entries = ctx.mod_storage.list_mods_for_category(slot.category());
    let mod_identifier = mod_data
        .as_ref()
        .and_then(|d| d.get("id").or_else(|| d.get("name")))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| mod_id.clone());

    let Some(entry) = entries.iter().find(|e| e.id == mod_identifier || e.name == mod_identifier) else {
        log_warn!("[bridge] {} mod not found: {mod_identifier}", slot.label());
        return;
    };

    let source = ctx.mod_storage.mods_root().join(entry.path.replace('/', "\\"));
    let Some(extracted) = extract_selected_mod(ctx, &source) else { return };

    let selection = CategoryModSelection {
        mod_name: entry.name.clone(),
        mod_path: extracted.mod_path,
        mod_folder_name: extracted.mod_folder_name,
        relative_path: extracted.relative_path,
    };
    {
        let mut shared = ctx.skins.shared.lock_safe();
        let field = match slot {
            Slot::Map => &mut shared.category_mods.map,
            Slot::Font => &mut shared.category_mods.font,
            Slot::Announcer => &mut shared.category_mods.announcer,
        };
        *field = Some(selection.clone());
    }
    set_historic_single(slot, Some(selection.relative_path.clone()));
    log_info!("[bridge] {} mod selected and extracted: {}", slot.label(), selection.mod_name);
}

async fn auto_select_from_history(ctx: &BridgeContext, slot: Slot, historic_path: &str, entries: &[CategoryModEntry]) {
    let Some(entry) = entries.iter().find(|e| e.id.replace('\\', "/") == historic_path.replace('\\', "/")) else { return };
    let data = category_entry_json(entry);
    select_single_slot_mod(ctx, slot, Some(entry.id.clone()), Some(data)).await;
    log_info!("[bridge] Auto-selected historic {} mod: {}", slot.label(), entry.name);
}

/// `_is_safe_relative_path` (ported): a client-supplied relative path must
/// stay relative and cannot traverse upward or reference a UNC/device path.
fn is_safe_relative_path(path_value: &str) -> bool {
    let cleaned = path_value.trim().replace('/', "\\");
    if cleaned.is_empty() || cleaned.starts_with("\\\\") {
        return false;
    }
    let candidate = Path::new(&cleaned);
    if candidate.is_absolute() {
        return false;
    }
    !cleaned.split('\\').any(|part| part.is_empty() || part == "." || part == "..")
}

async fn handle_select_other(ctx: &BridgeContext, other_id: Option<String>, other_data: Option<Value>, action: Option<String>) {
    let action = action.unwrap_or_else(|| "select".to_string());

    if action == "deselect" || other_id.is_none() {
        let removed_path = other_data.as_ref().and_then(|d| d.get("id")).and_then(Value::as_str).map(String::from);
        {
            let mut shared = ctx.skins.shared.lock_safe();
            if let Some(rp) = &removed_path {
                shared.category_mods.others.retain(|m| &m.relative_path != rp);
            } else {
                shared.category_mods.others.clear();
            }
        }
        rebuild_other_historic(ctx);
        log_info!("[bridge] Other mod(s) deselected");
        return;
    }

    let Some(other_id) = other_id else { return };
    let rel_path = other_data
        .as_ref()
        .and_then(|d| d.get("path").or_else(|| d.get("id")))
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| other_id.clone());
    if !is_safe_relative_path(&rel_path) {
        log_warn!("[bridge] Blocked unsafe other mod path: {rel_path}");
        return;
    }

    let source = ctx.mod_storage.mods_root().join(rel_path.replace('/', "\\"));
    let mod_name =
        other_data.as_ref().and_then(|d| d.get("name")).and_then(Value::as_str).map(String::from).unwrap_or_else(|| other_id.clone());
    let Some(extracted) = extract_selected_mod(ctx, &source) else { return };

    let selection =
        CategoryModSelection { mod_name, mod_path: extracted.mod_path, mod_folder_name: extracted.mod_folder_name, relative_path: extracted.relative_path };

    let already_selected = {
        let mut shared = ctx.skins.shared.lock_safe();
        let exists = shared.category_mods.others.iter().any(|m| m.relative_path == selection.relative_path);
        if !exists {
            shared.category_mods.others.push(selection.clone());
        }
        exists
    };
    if !already_selected {
        log_info!("[bridge] Other mod selected and extracted: {}", selection.mod_name);
    } else {
        log_info!("[bridge] Other mod already selected: {}", selection.mod_name);
    }
    rebuild_other_historic(ctx);
}

/// Rebuild `mod_historic.json`'s six list categories from the current
/// `others` selection, keyed by each path's leading segment (ported from
/// `_handle_select_other`'s `by_cat` rebuild).
fn rebuild_other_historic(ctx: &BridgeContext) {
    let others = ctx.skins.shared.lock_safe().category_mods.others.clone();
    let mut h = historic::load_mod_historic();
    for cat in historic::MOD_HISTORIC_CATEGORIES {
        h.clear_category(cat);
    }
    for m in &others {
        let rp = m.relative_path.replace('\\', "/");
        let leading = rp.split('/').next().unwrap_or("others").to_lowercase();
        let cat = if historic::MOD_HISTORIC_CATEGORIES.contains(&leading.as_str()) { leading.as_str() } else { "others" };
        h.add_to_category(cat, rp);
    }
    historic::write_mod_historic(&h);
}

// ---------------------------------------------------------------------
// Mod list requests (skins / maps / fonts / announcers / category)
// ---------------------------------------------------------------------

async fn handle_request_skin_mods(ctx: &BridgeContext, skin_id: Option<i64>, champion_id: Option<i64>) {
    let Some(skin_id) = skin_id else { return };
    let champion_id = champion_id.unwrap_or_else(|| skin_id / 1000);

    let entries = ctx.mod_storage.list_mods_for_champion(champion_id);
    let mods: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "modName": e.mod_name,
                "skinId": e.skin_id,
                "description": e.description,
                "updatedAt": (e.updated_at * 1000.0) as i64,
                "relativePath": relative_path_of(ctx, &e.path),
            })
        })
        .collect();

    let historic_mod_path =
        historic::get_historic_skin_for_champion(champion_id).and_then(|v| v.custom_mod_path().map(str::to_string));

    ctx.handle.broadcast_json(json!({
        "type": "skin-mods-response",
        "championId": champion_id,
        "skinId": skin_id,
        "mods": mods,
        "historicMod": historic_mod_path,
        "timestamp": protocol::now_ms(),
    }));
}

async fn handle_request_single_slot_category(ctx: &BridgeContext, slot: Slot, response_type: &str, list_key: &str) {
    let entries = ctx.mod_storage.list_mods_for_category(slot.category());
    let historic_path = get_historic_single(slot);

    let mut payload = json!({
        "type": response_type,
        "historicMod": historic_path,
        "timestamp": protocol::now_ms(),
    });
    payload[list_key] = json!(category_entries_json(&entries));
    ctx.handle.broadcast_json(payload);

    if let Some(hp) = &historic_path {
        let already = {
            let shared = ctx.skins.shared.lock_safe();
            match slot {
                Slot::Map => shared.category_mods.map.is_some(),
                Slot::Font => shared.category_mods.font.is_some(),
                Slot::Announcer => shared.category_mods.announcer.is_some(),
            }
        };
        if !already {
            auto_select_from_history(ctx, slot, hp, &entries).await;
        }
    }
}

async fn handle_request_maps(ctx: &BridgeContext) {
    handle_request_single_slot_category(ctx, Slot::Map, "maps-response", "maps").await;
}

async fn handle_request_fonts(ctx: &BridgeContext) {
    handle_request_single_slot_category(ctx, Slot::Font, "fonts-response", "fonts").await;
}

async fn handle_request_announcers(ctx: &BridgeContext) {
    handle_request_single_slot_category(ctx, Slot::Announcer, "announcers-response", "announcers").await;
}

/// `_handle_request_category_mods` — no auto-select (the Python original has
/// none here either; only the single-slot map/font/announcer handlers do).
async fn handle_request_category_mods(ctx: &BridgeContext, category: Option<String>) {
    let Some(category) = category else { return };
    if !historic::MOD_HISTORIC_CATEGORIES.contains(&category.as_str()) {
        log_warn!("[bridge] Invalid category for request-category-mods: {category}");
        return;
    }

    let mods = ctx.mod_storage.list_mods_for_category(&category);
    let h = historic::load_mod_historic();
    let historic_paths: Vec<String> = h.category(&category).map(|s| s.to_vec()).unwrap_or_default();

    ctx.handle.broadcast_json(json!({
        "type": "category-mods-response",
        "category": category,
        "mods": category_entries_json(&mods),
        "historicMod": historic_paths,
        "timestamp": protocol::now_ms(),
    }));
}

// ---------------------------------------------------------------------
// Custom mods folder browsing (champion/skin pickers)
// ---------------------------------------------------------------------

fn cleanup_empty_skin_folders(skins_dir: std::path::PathBuf) {
    let Ok(entries) = std::fs::read_dir(&skins_dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && std::fs::read_dir(&path).map(|mut it| it.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

fn extract_champions_from_data(data: &Value, out: &mut BTreeMap<i64, String>) {
    match data {
        Value::Object(map) => {
            let champ_id = map.get("id").or_else(|| map.get("championId")).or_else(|| map.get("itemId")).or_else(|| map.get("item_id"));
            let champ_name =
                map.get("name").or_else(|| map.get("title")).or_else(|| map.get("localizedName")).and_then(Value::as_str);
            if let (Some(id_val), Some(name)) = (champ_id, champ_name) {
                let parsed_id = id_val.as_i64().or_else(|| id_val.as_str().and_then(|s| s.parse().ok()));
                if let Some(id) = parsed_id {
                    if id < 1000 && !name.to_lowercase().contains("skin") {
                        out.entry(id).or_insert_with(|| name.to_string());
                    }
                }
            }
            for v in map.values() {
                extract_champions_from_data(v, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                extract_champions_from_data(item, out);
            }
        }
        _ => {}
    }
}

async fn handle_add_custom_mods_champion_selected(ctx: &BridgeContext, action: Option<String>) {
    if action.as_deref() != Some("list") {
        return;
    }
    cleanup_empty_skin_folders(ctx.mod_storage.skins_dir());

    let Some(auth) = lcu::cached_auth() else {
        ctx.handle.broadcast_json(json!({
            "type": "champions-list-response",
            "champions": Vec::<Value>::new(),
            "error": "LCU is not available. Please ensure League of Legends client is running.",
        }));
        return;
    };

    let mut champions: BTreeMap<i64, String> = BTreeMap::new();
    for attempt in 0..3 {
        if let Some(data) = lcu::get_json(&ctx.http_client, &auth, "/lol-store/v1/champions").await {
            extract_champions_from_data(&data, &mut champions);
            if !champions.is_empty() {
                break;
            }
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    if champions.is_empty() {
        log_warn!("[bridge] Failed to fetch champions from shop endpoint after 3 attempts");
    }

    let mut list: Vec<(String, i64)> = champions.into_iter().map(|(id, name)| (name, id)).collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    let champions_json: Vec<Value> = list.into_iter().map(|(name, id)| json!({"id": id, "name": name})).collect();

    log_info!("[bridge] Sent champions list: {} champions", champions_json.len());
    ctx.handle.broadcast_json(json!({ "type": "champions-list-response", "champions": champions_json }));
}

async fn handle_add_custom_mods_skin_selected(ctx: &BridgeContext, action: Option<String>, champion_id: Option<i64>, skin_id: Option<i64>) {
    match action.as_deref() {
        Some("list") => {
            let Some(champion_id) = champion_id else {
                ctx.handle.broadcast_json(json!({
                    "type": "champion-skins-response", "championId": null, "skins": Vec::<Value>::new(),
                    "error": "Champion ID is required",
                }));
                return;
            };
            let Some(auth) = lcu::cached_auth() else {
                ctx.handle.broadcast_json(json!({
                    "type": "champion-skins-response", "championId": champion_id, "skins": Vec::<Value>::new(),
                    "error": "LCU is not available. Please ensure League of Legends client is running.",
                }));
                return;
            };

            let endpoint = format!("/lol-game-data/assets/v1/champions/{champion_id}.json");
            let data = lcu::get_json(&ctx.http_client, &auth, &endpoint).await;
            let mut champion_name = None;
            let mut skins: Vec<Value> = Vec::new();
            if let Some(data) = &data {
                champion_name = data.get("name").and_then(Value::as_str).map(String::from);
                if let Some(raw_skins) = data.get("skins").and_then(Value::as_array) {
                    for skin in raw_skins {
                        let skin_id = skin
                            .get("id")
                            .and_then(Value::as_i64)
                            .unwrap_or_else(|| champion_id * 1000 + skin.get("num").and_then(Value::as_i64).unwrap_or(0));
                        let name = skin.get("name").and_then(Value::as_str).unwrap_or("Skin").to_string();
                        let mut entry = json!({"id": skin_id, "skinId": skin_id, "name": name});
                        if let Some(tile) = skin.get("tilePath").and_then(Value::as_str) {
                            entry["tilePath"] = json!(tile);
                        }
                        skins.push(entry);
                    }
                }
            }
            skins.sort_by_key(|s| s["skinId"].as_i64().unwrap_or(0));

            log_info!("[bridge] Sent skins list for champion {champion_id}: {} skins", skins.len());
            ctx.handle.broadcast_json(json!({
                "type": "champion-skins-response", "championId": champion_id, "championName": champion_name, "skins": skins,
            }));
        }
        Some("create") => {
            let (Some(_champion_id), Some(skin_id)) = (champion_id, skin_id) else {
                ctx.handle.broadcast_json(json!({
                    "type": "folder-opened-response", "success": false, "error": "Champion ID and Skin ID are required",
                }));
                return;
            };
            let folder = ctx.mod_storage.get_skin_dir(skin_id);
            open_folder_and_respond(ctx, &folder);
        }
        _ => {}
    }
}

fn handle_add_custom_mods_category_selected(ctx: &BridgeContext, category: Option<String>) {
    let Some(category) = category else { return };
    let valid = [
        storage::CATEGORY_MAPS,
        storage::CATEGORY_FONTS,
        storage::CATEGORY_ANNOUNCERS,
        storage::CATEGORY_OTHERS,
        storage::CATEGORY_UI,
        storage::CATEGORY_VOICEOVER,
        storage::CATEGORY_LOADING_SCREEN,
        storage::CATEGORY_VFX,
        storage::CATEGORY_SFX,
    ];
    if !valid.contains(&category.as_str()) {
        log_warn!("[bridge] Invalid category: {category}");
        return;
    }
    let folder = ctx.mod_storage.mods_root().join(&category);
    open_folder_and_respond(ctx, &folder);
}

fn open_folder_and_respond(ctx: &BridgeContext, folder: &Path) {
    let _ = std::fs::create_dir_all(folder);
    match std::process::Command::new("explorer").arg(folder).spawn() {
        Ok(_) => {
            log_info!("[bridge] Opened folder: {}", folder.display());
            ctx.handle.broadcast_json(json!({"type": "folder-opened-response", "success": true, "path": folder.to_string_lossy()}));
        }
        Err(e) => {
            log_error!("[bridge] Failed to open folder {}: {e}", folder.display());
            ctx.handle.broadcast_json(json!({"type": "folder-opened-response", "success": false, "error": e.to_string()}));
        }
    }
}

// ---------------------------------------------------------------------
// Folder / UI shortcuts
// ---------------------------------------------------------------------

fn open_in_explorer(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    if let Err(e) = std::process::Command::new("explorer").arg(path).spawn() {
        log_error!("[bridge] Failed to open folder {}: {e}", path.display());
    } else {
        log_info!("[bridge] Opened folder: {}", path.display());
    }
}

fn handle_open_mods_folder() {
    open_in_explorer(&paths::mods_dir());
}

fn handle_open_logs_folder() {
    open_in_explorer(&paths::logs_dir());
}

fn handle_open_pengu_loader_ui() {
    let exe = paths::pengu_loader_dir().join("Pengu Loader.exe");
    if !exe.exists() {
        log_warn!("[bridge] Pengu Loader executable not found: {}", exe.display());
        return;
    }
    match std::process::Command::new(&exe).arg("--ui").current_dir(paths::pengu_loader_dir()).spawn() {
        Ok(_) => log_info!("[bridge] Launched Pengu Loader UI"),
        Err(e) => log_error!("[bridge] Failed to launch Pengu Loader UI: {e}"),
    }
}

// ---------------------------------------------------------------------
// Settings / path validation
//
// DEVIATION (flagged for the lead): `config::SkinsCfg` (out of this
// milestone's file scope) has `league_path`/`injection_threshold_ms` but no
// `monitor_auto_resume_timeout`/`autostart` fields, and there is no ported
// Windows autostart registration anywhere in this codebase
// (`utils/system/admin_utils.py`'s `register_autostart`/`unregister_
// autostart` equivalent). `monitorAutoResumeTimeout`/`autostart` persist to
// a small bridge-local JSON file and round-trip on request/save, but
// `autostart` is NOT actually enforced (no Task Scheduler registration) —
// recommend adding the missing `SkinsCfg` field and a real autostart port in
// a follow-up.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct BridgeLocalSettings {
    monitor_auto_resume_timeout: i64,
    autostart_requested: bool,
}

impl Default for BridgeLocalSettings {
    fn default() -> Self {
        Self { monitor_auto_resume_timeout: 60, autostart_requested: false }
    }
}

fn bridge_local_settings_path() -> std::path::PathBuf {
    paths::state_dir().join("bridge_local_settings.json")
}

fn load_bridge_local_settings() -> BridgeLocalSettings {
    std::fs::read_to_string(bridge_local_settings_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_bridge_local_settings(settings: &BridgeLocalSettings) {
    let path = bridge_local_settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, text);
    }
}

fn is_unc_path(path_value: &str) -> bool {
    let stripped = path_value.trim();
    stripped.starts_with("\\\\") || stripped.starts_with("//")
}

/// `_is_valid_local_league_path` (ported).
fn is_valid_local_league_path(game_path: &str) -> bool {
    let cleaned = game_path.trim();
    if cleaned.is_empty() || is_unc_path(cleaned) {
        return false;
    }
    let dir = Path::new(cleaned);
    if !dir.is_absolute() {
        return false;
    }
    dir.is_dir() && dir.join("League of Legends.exe").is_file()
}

async fn handle_settings_request(ctx: &BridgeContext) {
    let (threshold, game_path) = {
        let app_state = ctx.app.state::<std::sync::Arc<crate::AppState>>();
        let cfg = app_state.config.lock_safe();
        (cfg.skins.injection_threshold_ms as f64 / 1000.0, cfg.skins.league_path.clone())
    };
    let local = load_bridge_local_settings();
    let path_valid = !game_path.is_empty() && is_valid_local_league_path(&game_path);
    let errors = compute_diagnostics_errors();

    ctx.handle.broadcast_json(json!({
        "type": "settings-data",
        "threshold": threshold,
        "monitorAutoResumeTimeout": local.monitor_auto_resume_timeout,
        "autostart": local.autostart_requested,
        "gamePath": game_path,
        "gamePathValid": path_valid,
        "hasErrors": !errors.is_empty(),
        "errorsCount": errors.len(),
        "version": env!("CARGO_PKG_VERSION"),
    }));
    log_info!("[bridge] Settings data sent: threshold={threshold} gamePath={game_path} valid={path_valid}");
}

async fn handle_path_validate(ctx: &BridgeContext, game_path: Option<String>) {
    let game_path = game_path.unwrap_or_default();
    let valid = !game_path.trim().is_empty() && is_valid_local_league_path(&game_path);
    ctx.handle.broadcast_json(json!({"type": "path-validation-result", "gamePath": game_path, "valid": valid}));
}

fn handle_settings_save(
    ctx: &BridgeContext,
    threshold: Option<f64>,
    monitor_auto_resume_timeout: Option<i64>,
    autostart: Option<bool>,
    game_path: Option<String>,
) {
    let threshold_s = threshold.unwrap_or(0.5).clamp(0.0, 2.0);
    let monitor_s = monitor_auto_resume_timeout.unwrap_or(60).clamp(1, 180);
    let autostart = autostart.unwrap_or(false);
    let game_path = game_path.unwrap_or_default();
    let trimmed_path = game_path.trim();

    if !trimmed_path.is_empty() && !is_valid_local_league_path(trimmed_path) {
        ctx.handle.broadcast_json(json!({
            "type": "settings-saved", "success": false,
            "error": "League path must be a valid local League of Legends folder.",
        }));
        return;
    }

    {
        let app_state = ctx.app.state::<std::sync::Arc<crate::AppState>>();
        let mut cfg = app_state.config.lock_safe();
        cfg.skins.injection_threshold_ms = (threshold_s * 1000.0).round() as u64;
        cfg.skins.league_path = trimmed_path.to_string();
        let _ = cfg.save();
    }
    ctx.injection.refresh_injection_threshold(threshold_s);

    let mut local = load_bridge_local_settings();
    local.monitor_auto_resume_timeout = monitor_s;
    local.autostart_requested = autostart;
    save_bridge_local_settings(&local);

    log_info!("[bridge] Settings saved: threshold={threshold_s:.2}s monitor_timeout={monitor_s}s autostart_requested={autostart}");
    ctx.handle.broadcast_json(json!({"type": "settings-saved", "success": true}));
}

// ---------------------------------------------------------------------
// Diagnostics
//
// SIMPLIFICATION: `utils/core/issue_reporter.py` isn't ported (out of
// scope). This reads/writes `chud_diagnostics.txt` directly (the rebranded
// diagnostics-log filename per `docs/SKINS_PORT.md` §1) using the same
// "ts | msg" + optional "Fix:" line block format, but the deep numeric-hint
// regex extraction (`observedTimeoutS`, `baseSkinForceTimeMs`, etc.) is
// dropped in favor of the tracker-based stats (`injection::base_skin_
// tracker`, which IS fully ported and wired) — the `code`/`text` contract
// (`AUTO_RESUME_TRIGGERED`, `BASE_SKIN_FORCE_SLOW`, `BASE_SKIN_VERIFY_FAILED`)
// and the dedupe/cap-8/reverse-chronological behavior are preserved.
// ---------------------------------------------------------------------

fn diagnostics_file_path() -> std::path::PathBuf {
    paths::data_root().join("chud_diagnostics.txt")
}

/// Parse "ts | msg" blocks (each optionally followed by a "Fix: ..." line),
/// returning the raw joined-message line (not yet split) plus the fix line —
/// this shape is what the clear-category rewrite needs to preserve exact
/// on-disk formatting.
fn parse_diagnostic_blocks(text: &str) -> Vec<(String, Option<String>)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        if line.contains(" | ") {
            let mut fix = None;
            if i + 1 < lines.len() {
                let next = lines[i + 1].trim_end();
                if next.starts_with("Fix:") {
                    fix = Some(next.to_string());
                    i += 1;
                }
            }
            blocks.push((line.to_string(), fix));
        }
        i += 1;
    }
    blocks
}

struct DiagEntry {
    ts: String,
    msg: String,
    fix: Option<String>,
}

fn read_diagnostics_entries() -> Vec<DiagEntry> {
    let Ok(text) = std::fs::read_to_string(diagnostics_file_path()) else { return Vec::new() };
    parse_diagnostic_blocks(&text)
        .into_iter()
        .filter_map(|(line, fix)| line.split_once(" | ").map(|(ts, msg)| DiagEntry { ts: ts.trim().to_string(), msg: msg.trim().to_string(), fix }))
        .collect()
}

fn categorize(msg: &str, fix: Option<&str>) -> Option<(&'static str, &'static str)> {
    let ml = msg.to_lowercase();
    let fl = fix.unwrap_or("").to_lowercase();
    if ml.contains("injection skipped") && ml.contains("base skin selected") {
        return None; // not actually an error
    }
    if fl.contains("monitor auto-resume timeout") || ml.contains("auto-resume safety") {
        return Some(("AUTO_RESUME_TRIGGERED", "Monitor Auto-Resume Timeout - increase"));
    }
    if ml.contains("injection threshold")
        || fl.contains("injection threshold")
        || fl.contains("base skin force time")
        || fl.contains("base skin confirmation")
        || ml.contains("verification failed")
    {
        let code = if ml.contains("verification failed") { "BASE_SKIN_VERIFY_FAILED" } else { "BASE_SKIN_FORCE_SLOW" };
        return Some((code, "Injection Threshold - increase"));
    }
    None
}

fn compute_diagnostics_errors() -> Vec<Value> {
    let entries = read_diagnostics_entries();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in entries.iter().rev() {
        let Some((code, text)) = categorize(&e.msg, e.fix.as_deref()) else { continue };
        if !seen.insert(text) {
            continue;
        }
        out.push(json!({"ts": e.ts, "code": code, "text": text}));
        if out.len() >= 8 {
            break;
        }
    }
    out.reverse();
    out
}

fn handle_diagnostics_request(ctx: &BridgeContext) {
    let errors = compute_diagnostics_errors();
    let tracker_stats = base_skin_tracker::get_stats();
    ctx.handle.broadcast_json(json!({
        "type": "diagnostics-data",
        "errors": errors,
        "path": diagnostics_file_path().to_string_lossy(),
        "baseSkinStats": serde_json::to_value(&tracker_stats).unwrap_or_default(),
    }));
}

fn clear_diagnostics_file() -> bool {
    std::fs::write(diagnostics_file_path(), "").is_ok()
}

fn handle_diagnostics_clear(ctx: &BridgeContext) {
    let ok = clear_diagnostics_file();
    ctx.handle.broadcast_json(json!({"type": "diagnostics-cleared", "success": ok}));
}

fn category_for(msg: &str, fix: Option<&str>) -> &'static str {
    let ml = msg.to_lowercase();
    let fl = fix.unwrap_or("").to_lowercase();
    if ml.contains("auto-resume safety") || fl.contains("monitor auto-resume timeout") {
        return "monitor_timeout";
    }
    if ml.contains("base skin force time")
        || ml.contains("injection threshold")
        || fl.contains("injection threshold")
        || fl.contains("base skin force time")
        || fl.contains("base skin confirmation")
        || ml.contains("verification failed")
    {
        return "injection_threshold";
    }
    "other"
}

fn normalize_diagnostic_categories(categories: Option<&Value>, category: Option<&str>) -> HashSet<&'static str> {
    let mut raw: Vec<String> = Vec::new();
    match categories {
        Some(Value::String(s)) => raw.push(s.clone()),
        Some(Value::Array(arr)) => raw.extend(arr.iter().filter_map(|v| v.as_str().map(String::from))),
        _ => {}
    }
    if let Some(c) = category {
        raw.push(c.to_string());
    }

    let mut norm = HashSet::new();
    for c in raw {
        match c.trim().to_lowercase().as_str() {
            "injection_threshold" | "threshold" | "injection" => {
                norm.insert("injection_threshold");
            }
            "monitor_timeout" | "monitor" | "monitor_auto_resume_timeout" | "auto_resume" => {
                norm.insert("monitor_timeout");
            }
            _ => {}
        }
    }
    norm
}

fn clear_diagnostics_categories(categories: &HashSet<&'static str>) -> bool {
    if categories.is_empty() {
        return false;
    }
    let path = diagnostics_file_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return true };

    let blocks = parse_diagnostic_blocks(&text);
    let mut kept = Vec::new();
    for (msg_line, fix_line) in &blocks {
        let msg_only = msg_line.split_once(" | ").map(|(_, m)| m).unwrap_or(msg_line.as_str());
        if categories.contains(category_for(msg_only, fix_line.as_deref())) {
            continue;
        }
        kept.push(msg_line.clone());
        if let Some(f) = fix_line {
            kept.push(f.clone());
        }
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&path, out).is_ok()
}

fn handle_diagnostics_clear_category(ctx: &BridgeContext, categories: Option<Value>, category: Option<String>) {
    let norm = normalize_diagnostic_categories(categories.as_ref(), category.as_deref());
    let ok = !norm.is_empty() && clear_diagnostics_categories(&norm);
    let mut cats: Vec<&str> = norm.into_iter().collect();
    cats.sort_unstable();
    ctx.handle.broadcast_json(json!({"type": "diagnostics-cleared-category", "success": ok, "categories": cats}));
}

fn handle_diagnostics_clear_tracker(ctx: &BridgeContext) {
    base_skin_tracker::clear_samples();
    ctx.handle.broadcast_json(json!({"type": "diagnostics-tracker-cleared", "success": true}));
}

fn handle_diagnostics_apply_recommended(ctx: &BridgeContext) {
    let stats = base_skin_tracker::get_stats();
    let Some(rec_ms) = stats.recommended_threshold_ms else {
        ctx.handle.broadcast_json(json!({"type": "diagnostics-applied-recommended", "success": false, "reason": "no data"}));
        return;
    };
    let rec_s = (rec_ms as f64 / 1000.0 * 100.0).round() / 100.0;

    {
        let app_state = ctx.app.state::<std::sync::Arc<crate::AppState>>();
        let mut cfg = app_state.config.lock_safe();
        cfg.skins.injection_threshold_ms = rec_ms.max(0) as u64;
        let _ = cfg.save();
    }
    ctx.injection.refresh_injection_threshold(rec_s);

    log_info!("[bridge] Applied recommended threshold: {rec_s}s ({rec_ms}ms)");
    ctx.handle.broadcast_json(json!({
        "type": "diagnostics-applied-recommended", "success": true, "appliedThresholdS": rec_s, "appliedThresholdMs": rec_ms,
    }));
}

// ---------------------------------------------------------------------
// Party mode (S6) — real `PartyManager` calls, pulled from `AppState::
// skins_party` (set once in `lib.rs`'s `setup()`, after the bridge). Every
// handler answers via the same message shapes `party/integration/ui_bridge.py`
// used (`party-enabled`/`party-disabled`/`party-peer-added`/
// `party-peer-removed`); `PartyManager` itself pushes `party-state`
// proactively (on enable/disable/add-peer/remove-peer and whenever the
// relay's member list changes) — see `PartyManager::broadcast_state`.
// ---------------------------------------------------------------------

/// Pull the party manager out of `AppState`, if `setup()` has built it yet.
fn party_manager(ctx: &BridgeContext) -> Option<std::sync::Arc<crate::skins::party::manager::PartyManager>> {
    let app_state = ctx.app.state::<std::sync::Arc<crate::AppState>>();
    let manager = app_state.skins_party.lock_safe().clone();
    manager
}

async fn handle_party_enable(ctx: &BridgeContext) {
    let Some(manager) = party_manager(ctx) else {
        log_warn!("[bridge] party-enable: PartyManager not initialized");
        ctx.handle.broadcast_json(json!({"type": "party-enabled", "success": false, "error": "Party mode is not available yet"}));
        return;
    };
    match manager.enable().await {
        Ok(token) => {
            log_info!("[bridge] Party mode enabled");
            ctx.handle.broadcast_json(json!({"type": "party-enabled", "success": true, "token": token}));
        }
        Err(e) => {
            log_warn!("[bridge] party-enable failed: {e}");
            ctx.handle.broadcast_json(json!({"type": "party-enabled", "success": false, "error": e}));
        }
    }
}

async fn handle_party_disable(ctx: &BridgeContext) {
    if let Some(manager) = party_manager(ctx) {
        manager.disable().await;
    }
    log_info!("[bridge] Party mode disabled");
    ctx.handle.broadcast_json(json!({"type": "party-disabled", "success": true}));
}

async fn handle_party_add_peer(ctx: &BridgeContext, token: Option<String>) {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        ctx.handle.broadcast_json(json!({"type": "party-peer-added", "success": false, "error": "No token provided"}));
        return;
    };
    let Some(manager) = party_manager(ctx) else {
        ctx.handle.broadcast_json(json!({"type": "party-peer-added", "success": false, "error": "Party mode is not available yet"}));
        return;
    };
    match manager.add_peer(&token).await {
        Ok(()) => {
            log_info!("[bridge] Party peer added");
            ctx.handle.broadcast_json(json!({"type": "party-peer-added", "success": true, "error": Value::Null}));
        }
        Err(e) => {
            log_warn!("[bridge] party-add-peer failed: {e}");
            ctx.handle.broadcast_json(json!({"type": "party-peer-added", "success": false, "error": e}));
        }
    }
}

/// Wire field is literally `summoner_id` (snake_case, not camelCase) in the
/// Python original's payload — see `InboundMessage::PartyRemovePeer`'s own
/// doc comment; it arrives as a generic `Value` since the JS side doesn't
/// consistently send it as a JSON number.
fn handle_party_remove_peer(ctx: &BridgeContext, summoner_id: Option<Value>) {
    let Some(summoner_id) = summoner_id.and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))) else {
        log_warn!("[bridge] party-remove-peer: missing/invalid summoner_id");
        return;
    };
    if let Some(manager) = party_manager(ctx) {
        manager.remove_peer(summoner_id as u64);
    }
    log_info!("[bridge] Party peer removed: {summoner_id}");
    ctx.handle.broadcast_json(json!({"type": "party-peer-removed", "success": true, "summoner_id": summoner_id}));
}

fn handle_party_get_state(ctx: &BridgeContext) {
    let Some(manager) = party_manager(ctx) else {
        ctx.handle.broadcast_party_state_disabled();
        return;
    };
    let mut state = manager.get_state();
    state["type"] = json!("party-state");
    state["timestamp"] = json!(protocol::now_ms());
    ctx.handle.broadcast_json(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_has_chromas_matches_special_ids_without_a_cache() {
        assert!(skin_has_chromas(None, Some(99007)));
        assert!(skin_has_chromas(None, Some(99995)));
        assert!(skin_has_chromas(None, Some(145071)));
        assert!(!skin_has_chromas(None, Some(103000)));
        assert!(!skin_has_chromas(None, None));
    }

    #[test]
    fn percent_encode_path_preserves_slashes_and_encodes_spaces() {
        assert_eq!(percent_encode_path("a b/c.png"), "a%20b/c.png");
        assert_eq!(percent_encode_path("a\\b"), "a/b");
    }

    #[test]
    fn is_safe_relative_path_rejects_traversal_and_absolute() {
        assert!(!is_safe_relative_path("../evil"));
        assert!(!is_safe_relative_path("C:\\Windows\\evil.dll"));
        assert!(!is_safe_relative_path("\\\\server\\share"));
        assert!(is_safe_relative_path("ui/My Mod.zip"));
    }

    #[test]
    fn is_valid_local_league_path_rejects_unc_and_relative() {
        assert!(!is_valid_local_league_path(""));
        assert!(!is_valid_local_league_path("\\\\server\\share"));
        assert!(!is_valid_local_league_path("relative/path"));
    }

    #[test]
    fn normalize_diagnostic_categories_maps_aliases() {
        let norm = normalize_diagnostic_categories(None, Some("threshold"));
        assert!(norm.contains("injection_threshold"));
        let norm2 = normalize_diagnostic_categories(None, Some("auto_resume"));
        assert!(norm2.contains("monitor_timeout"));
    }

    #[test]
    fn categorize_matches_known_diagnostic_patterns() {
        assert_eq!(categorize("Auto-resume safety triggered after 60s", None).map(|(c, _)| c), Some("AUTO_RESUME_TRIGGERED"));
        assert_eq!(categorize("Base skin verification failed", None).map(|(c, _)| c), Some("BASE_SKIN_VERIFY_FAILED"));
        assert_eq!(categorize("injection skipped - base skin selected", None), None);
    }

    #[test]
    fn bridge_local_settings_default_matches_python_defaults() {
        let s = BridgeLocalSettings::default();
        assert_eq!(s.monitor_auto_resume_timeout, 60);
        assert!(!s.autostart_requested);
    }
}
