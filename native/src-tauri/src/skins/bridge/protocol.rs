//! Bridge wire protocol — ported from `pengu\communication\message_handler.py`'s
//! routing table (35 inbound message types) and `pengu\communication\
//! broadcaster.py`'s 9 outbound state broadcasts (S4).
//!
//! CRITICAL two-stage inbound decode (`docs/SKINS_PORT.md` §3): the Pengu
//! plugin sends most messages `{"type": "...", ...}`-tagged, but the legacy
//! skin-hover message has NO `type` field at all — just `{"skin": "..."}`.
//! A strict serde-tagged enum would reject that shape outright, so decoding
//! is a first pass over `serde_json::Value` (does `"type"` exist? does
//! `"skin"` exist?) and only THEN a typed `serde_json::from_value` for the
//! tagged case. Message type strings are brand-neutral and kept VERBATIM —
//! the rebranded JS plugins still speak them unchanged.
//!
//! All timestamps are `i64` milliseconds since epoch (`Date.now()` on the JS
//! side; `int(time.time() * 1000)` in the Python original).

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::skins::slog::log_warn;

/// `Date.now()`-compatible milliseconds-since-epoch (ported from every
/// `int(time.time() * 1000)` call site in `broadcaster.py`/`message_handler.py`).
pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ---------------------------------------------------------------------
// Inbound — the 35-type routing table in `MessageHandler.handle_message`,
// plus the type-less legacy skin-hover fallback.
// ---------------------------------------------------------------------

/// Result of the two-stage decode.
#[derive(Debug, Clone)]
pub enum Inbound {
    /// `{"type": "...", ...}` — routes through `InboundMessage`.
    Message(InboundMessage),
    /// The legacy type-less `{"skin": "SkinName"}` hover message (no `type`
    /// key at all — `payload.get("skin")` in the Python fallback branch).
    SkinHover(String),
}

/// Decode one WebSocket text frame. Returns `None` on invalid JSON, an
/// unrecognized `type`, or a shape that matches neither stage (mirrors the
/// Python original silently dropping/logging unroutable payloads).
pub fn decode(text: &str) -> Option<Inbound> {
    let value: Value = serde_json::from_str(text).ok()?;

    if value.get("type").is_some() {
        return match serde_json::from_value::<InboundMessage>(value) {
            Ok(msg) => Some(Inbound::Message(msg)),
            Err(e) => {
                log_warn!("[bridge] Unrecognized/malformed typed message: {e}");
                None
            }
        };
    }

    if let Some(skin) = value.get("skin").and_then(Value::as_str) {
        if !skin.trim().is_empty() {
            return Some(Inbound::SkinHover(skin.to_string()));
        }
    }
    None
}

/// The 35 inbound message types (`message_handler.py`'s `elif payload_type
/// == "..."` chain), tagged on the wire by `"type"` — variant names are
/// `PascalCase` of the kebab-case wire string (e.g. `ChromaSelection` <->
/// `"chroma-selection"`) via `rename_all = "kebab-case"`; per-variant
/// `rename_all = "camelCase"` maps snake_case Rust fields to the JS payload's
/// camelCase keys (exceptions get an explicit `#[serde(rename = "...")]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum InboundMessage {
    #[serde(rename_all = "camelCase")]
    ChromaLog {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        data: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    RequestLocalPreview {
        #[serde(default)]
        champion_id: Option<i64>,
        #[serde(default)]
        skin_id: Option<i64>,
        #[serde(default)]
        chroma_id: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    RequestLocalAsset {
        #[serde(default)]
        asset_path: Option<String>,
        #[serde(default)]
        chroma_id: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    ChromaSelection {
        #[serde(default)]
        chroma_id: Option<i64>,
        #[serde(default)]
        skin_id: Option<i64>,
        #[serde(default)]
        chroma_name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DiceButtonClick {
        #[serde(default)]
        state: Option<String>,
    },
    SettingsRequest {},
    #[serde(rename_all = "camelCase")]
    PathValidate {
        #[serde(default)]
        game_path: Option<String>,
    },
    OpenModsFolder {},
    #[serde(rename_all = "camelCase")]
    RequestSkinMods {
        #[serde(default)]
        skin_id: Option<i64>,
        #[serde(default)]
        champion_id: Option<i64>,
    },
    RequestMaps {},
    RequestFonts {},
    RequestAnnouncers {},
    #[serde(rename_all = "camelCase")]
    RequestCategoryMods {
        #[serde(default)]
        category: Option<String>,
    },
    /// Backward-compat alias: treated identically to
    /// `RequestCategoryMods { category: Some("others") }`.
    RequestOthers {},
    #[serde(rename_all = "camelCase")]
    SelectSkinMod {
        #[serde(default)]
        champion_id: Option<i64>,
        #[serde(default)]
        skin_id: Option<i64>,
        #[serde(default)]
        mod_id: Option<String>,
        #[serde(default)]
        mod_data: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    SelectMap {
        #[serde(default)]
        map_id: Option<String>,
        #[serde(default)]
        map_data: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    SelectFont {
        #[serde(default)]
        font_id: Option<String>,
        #[serde(default)]
        font_data: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    SelectAnnouncer {
        #[serde(default)]
        announcer_id: Option<String>,
        #[serde(default)]
        announcer_data: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    SelectOther {
        #[serde(default)]
        other_id: Option<String>,
        #[serde(default)]
        other_data: Option<Value>,
        #[serde(default)]
        action: Option<String>,
    },
    OpenLogsFolder {},
    DiagnosticsRequest {},
    DiagnosticsClear {},
    #[serde(rename_all = "camelCase")]
    DiagnosticsClearCategory {
        #[serde(default)]
        categories: Option<Value>,
        #[serde(default)]
        category: Option<String>,
    },
    DiagnosticsClearTracker {},
    DiagnosticsApplyRecommended {},
    OpenPenguLoaderUi {},
    #[serde(rename_all = "camelCase")]
    SettingsSave {
        #[serde(default)]
        threshold: Option<f64>,
        #[serde(default)]
        monitor_auto_resume_timeout: Option<i64>,
        #[serde(default)]
        autostart: Option<bool>,
        #[serde(default)]
        game_path: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AddCustomModsCategorySelected {
        #[serde(default)]
        category: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AddCustomModsChampionSelected {
        #[serde(default)]
        action: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    AddCustomModsSkinSelected {
        #[serde(default)]
        action: Option<String>,
        #[serde(default)]
        champion_id: Option<i64>,
        #[serde(default)]
        skin_id: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    FindMatchHover {
        #[serde(default)]
        timestamp: Option<i64>,
    },
    DismissCustomMod {},
    DismissHistoric {},
    PartyEnable {},
    PartyDisable {},
    #[serde(rename_all = "camelCase")]
    PartyAddPeer {
        #[serde(default)]
        token: Option<String>,
    },
    PartyRemovePeer {
        // Wire field is literally snake_case in the Python original
        // (`payload.get("summoner_id")`) — preserved verbatim, NOT camelCase.
        #[serde(default, rename = "summoner_id")]
        summoner_id: Option<Value>,
    },
    PartyGetState {},
}

// ---------------------------------------------------------------------
// Outbound — the 9 named state broadcasts (`Broadcaster` in Python) that
// `bridge::broadcast::BridgeHandle` exposes as the S5/S6 seam, plus the
// generic raw-JSON primitive (`BridgeHandle::broadcast_json`) request/response
// payloads route through — together the "10 outbound" surfaces this bridge
// exposes.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub skin_name: String,
    pub skin_id: Option<i64>,
    pub champion_id: Option<i64>,
    pub has_chromas: bool,
}

impl SkinStateMsg {
    pub fn new(skin_name: String, skin_id: Option<i64>, has_chromas: bool) -> Self {
        Self { kind: "skin-state", champion_id: skin_id.map(|id| id / 1000), skin_name, skin_id, has_chromas }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChromaStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub selected_chroma_id: Option<i64>,
    pub chroma_color: Option<String>,
    pub chroma_colors: Option<Vec<String>>,
    pub current_skin_id: Option<i64>,
    pub timestamp: i64,
}

impl ChromaStateMsg {
    /// `chroma_color`/`chroma_colors`/`current_skin_id` are always `None` —
    /// the ChromaPanelManager the Python original could optionally source
    /// them from (`ui/chroma/panel.py`) is Qt-era UI bookkeeping that was
    /// never ported (see `features::chroma`'s module doc); this is the
    /// `else` fallback branch of `Broadcaster.broadcast_chroma_state`, which
    /// is the only branch reachable here.
    pub fn new(selected_chroma_id: Option<i64>) -> Self {
        Self {
            kind: "chroma-state",
            selected_chroma_id,
            chroma_color: None,
            chroma_colors: None,
            current_skin_id: None,
            timestamp: now_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoricStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub active: bool,
    pub historic_skin_id: Option<i64>,
    pub historic_skin_name: Option<String>,
    pub timestamp: i64,
}

impl HistoricStateMsg {
    pub fn new(active: bool, historic_skin_id: Option<i64>, historic_skin_name: Option<String>) -> Self {
        Self { kind: "historic-state", active, historic_skin_id, historic_skin_name, timestamp: now_ms() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomModStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub active: bool,
    pub mod_name: Option<String>,
    pub skin_id: Option<i64>,
    pub timestamp: i64,
}

impl CustomModStateMsg {
    pub fn new(active: bool, mod_name: Option<String>, skin_id: Option<i64>) -> Self {
        Self { kind: "custom-mod-state", active, mod_name, skin_id, timestamp: now_ms() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseChangeMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub phase: Option<String>,
    pub game_mode: Option<String>,
    pub map_id: Option<i64>,
    pub queue_id: Option<i64>,
    pub timestamp: i64,
}

impl PhaseChangeMsg {
    pub fn new(phase: Option<String>, game_mode: Option<String>, map_id: Option<i64>, queue_id: Option<i64>) -> Self {
        Self { kind: "phase-change", phase, game_mode, map_id, queue_id, timestamp: now_ms() }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChampionLockedMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub locked: bool,
    pub timestamp: i64,
}

impl ChampionLockedMsg {
    pub fn new(locked: bool) -> Self {
        Self { kind: "champion-locked", locked, timestamp: now_ms() }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RandomModeStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub active: bool,
    pub random_skin_id: Option<i64>,
    pub dice_state: &'static str,
    pub timestamp: i64,
}

impl RandomModeStateMsg {
    pub fn new(active: bool, random_skin_id: Option<i64>) -> Self {
        Self {
            kind: "random-mode-state",
            active,
            random_skin_id,
            dice_state: if active { "enabled" } else { "disabled" },
            timestamp: now_ms(),
        }
    }
}

/// No `timestamp` field — ported verbatim (`Broadcaster.broadcast_skip_base_skin`
/// sends `{"type": "skip-base-skin"}` alone).
#[derive(Debug, Clone, Serialize)]
pub struct SkipBaseSkinMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl SkipBaseSkinMsg {
    pub fn new() -> Self {
        Self { kind: "skip-base-skin" }
    }
}

impl Default for SkipBaseSkinMsg {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PartyStateMsg {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub enabled: bool,
    // Wire field is literally `my_token` (snake_case) in the Python
    // original's payload dict — preserved verbatim.
    pub my_token: Option<String>,
    pub peers: Vec<Value>,
    pub timestamp: i64,
}

impl PartyStateMsg {
    /// Party mode is S6 — always the disabled/empty shape here (see
    /// `bridge::handlers`'s party stubs).
    pub fn disabled() -> Self {
        Self { kind: "party-state", enabled: false, my_token: None, peers: Vec::new(), timestamp: now_ms() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_message_decodes_by_type_tag() {
        let json = r#"{"type":"dice-button-click","state":"enabled"}"#;
        match decode(json) {
            Some(Inbound::Message(InboundMessage::DiceButtonClick { state })) => {
                assert_eq!(state.as_deref(), Some("enabled"));
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn legacy_typeless_skin_message_falls_back_to_skin_hover() {
        let json = r#"{"skin":"PROJECT: Ashe"}"#;
        match decode(json) {
            Some(Inbound::SkinHover(skin)) => assert_eq!(skin, "PROJECT: Ashe"),
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn neither_type_nor_skin_decodes_to_none() {
        assert!(decode(r#"{"foo":"bar"}"#).is_none());
    }

    #[test]
    fn invalid_json_decodes_to_none() {
        assert!(decode("not json").is_none());
    }

    #[test]
    fn camel_case_fields_decode_correctly() {
        let json = r#"{"type":"chroma-selection","chromaId":103001,"skinId":103000,"chromaName":"Foxfire"}"#;
        match decode(json) {
            Some(Inbound::Message(InboundMessage::ChromaSelection { chroma_id, skin_id, chroma_name })) => {
                assert_eq!(chroma_id, Some(103001));
                assert_eq!(skin_id, Some(103000));
                assert_eq!(chroma_name.as_deref(), Some("Foxfire"));
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn party_remove_peer_keeps_snake_case_wire_field() {
        let json = r#"{"type":"party-remove-peer","summoner_id":12345}"#;
        match decode(json) {
            Some(Inbound::Message(InboundMessage::PartyRemovePeer { summoner_id })) => {
                assert_eq!(summoner_id, Some(Value::from(12345)));
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    #[test]
    fn skip_base_skin_serializes_without_timestamp() {
        let json = serde_json::to_string(&SkipBaseSkinMsg::new()).unwrap();
        assert_eq!(json, r#"{"type":"skip-base-skin"}"#);
    }

    #[test]
    fn skin_state_derives_champion_id_from_skin_id() {
        let msg = SkinStateMsg::new("Ahri".to_string(), Some(103000), false);
        assert_eq!(msg.champion_id, Some(103));
    }
}
