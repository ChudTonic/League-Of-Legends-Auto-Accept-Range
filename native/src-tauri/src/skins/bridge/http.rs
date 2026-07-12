//! Bridge HTTP routes (S4) — ported from `pengu\core\http_handler.py`
//! (`HTTPHandler`): `/bridge-port`, `/port` (legacy), `/preview/{champion}/
//! {skin}/{chroma}`, `/asset/{path}`, `/plugin/{name}/{file}`.
//!
//! Traversal hardening deliberately strengthens the Python original: Python's
//! `_is_safe_path` calls `Path.resolve()` (which lexically collapses `..`
//! even on a non-existent path) and checks the result still starts with the
//! base directory. Rust's `Path::canonicalize()` requires the path to
//! *exist*, so instead every path SEGMENT taken from the request is rejected
//! outright if it's empty, `.`/`..`, or contains a path separator — the same
//! lexical-component-rejection idiom `paths::get_asset_path` already uses
//! elsewhere in this codebase. This is a strictly stronger guarantee than
//! the original (nothing to "resolve" at all), not a behavior change plugins
//! would ever notice (champion/skin/chroma IDs and plugin/file names are
//! never legitimately `..` or path-separator-bearing).

#![allow(dead_code)]

use std::path::Path;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::skins::paths;
use crate::skins::slog::log_warn;

use super::{is_loopback_origin, read_bridge_port_file, BridgeContext};

/// Route one plain-HTTP (non-upgrade) request. `origin` is the raw `Origin`
/// header value, if any — already verified loopback-or-absent by
/// `ws::dispatch` before this is called, so it's only used here to decide
/// whether to attach CORS headers (ported from `cors_headers_for_origin`).
pub async fn route(ctx: &BridgeContext, path: &str, origin: Option<&str>) -> Response {
    let path_clean = percent_decode(path);

    if path_clean == "/port" {
        return text_response(ctx.handle.port().to_string(), origin);
    }
    if path_clean == "/bridge-port" {
        // Prefer the on-disk file (matches Python's own fallback order:
        // read the file, and only use the in-memory port if that fails).
        let port = read_bridge_port_file().unwrap_or_else(|| ctx.handle.port());
        return text_response(port.to_string(), origin);
    }
    if let Some(rest) = path_clean.strip_prefix("/preview/") {
        return handle_preview(rest, origin).await;
    }
    if let Some(rest) = path_clean.strip_prefix("/asset/") {
        return handle_asset(rest, origin).await;
    }
    if let Some(rest) = path_clean.strip_prefix("/plugin/") {
        return handle_plugin(rest, origin).await;
    }
    if path_clean == "/client-customization" {
        return handle_client_customization(ctx, origin);
    }
    if path_clean == "/phase" {
        return handle_phase(origin).await;
    }

    not_found()
}

/// Current LCU gameflow phase (e.g. `Matchmaking`, `ReadyCheck`, `ChampSelect`),
/// as JSON `{"phase": "..."}` or `{"phase": null}`. Client plugins (CHUD-QueueArena)
/// poll this because a direct LCU fetch from the client CEF context isn't reliable;
/// the Chud app holds LCU auth so this always resolves.
async fn handle_phase(origin: Option<&str>) -> Response {
    let phase = if let Some(auth) = crate::lcu::cached_auth() {
        let client = crate::lcu::build_client(3.0);
        crate::lcu::get_phase(&client, &auth).await
    } else {
        None
    };
    let body = serde_json::json!({ "phase": phase }).to_string();
    let mut builder =
        Response::builder().status(StatusCode::OK).header(axum::http::header::CONTENT_TYPE, "application/json");
    builder = apply_cors(builder, origin);
    builder.body(Body::from(body)).unwrap_or_else(|_| not_found())
}

/// Serve the in-client declutter/customization config as JSON. The
/// `CHUD-Declutter` Pengu plugin polls this and (re)injects CSS accordingly.
fn handle_client_customization(ctx: &BridgeContext, origin: Option<&str>) -> Response {
    use crate::LockExt;
    use tauri::Manager;
    let state = ctx.app.state::<std::sync::Arc<crate::AppState>>();
    let client = { state.config.lock_safe().client.clone() };
    let json = serde_json::to_string(&client).unwrap_or_else(|_| "{}".to_string());
    let mut builder =
        Response::builder().status(StatusCode::OK).header(axum::http::header::CONTENT_TYPE, "application/json");
    builder = apply_cors(builder, origin);
    builder.body(Body::from(json)).unwrap_or_else(|_| not_found())
}

/// A path segment taken verbatim from the request URL: reject anything that
/// could escape the base directory it's about to be joined onto.
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\'])
}

async fn handle_preview(rest: &str, origin: Option<&str>) -> Response {
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 3 {
        return not_found();
    }
    let (champion_id, skin_id, chroma_id) = (parts[0], parts[1], parts[2]);
    if ![champion_id, skin_id, chroma_id].into_iter().all(is_safe_segment) {
        log_warn!("[bridge] Blocked path traversal attempt in /preview/{rest}");
        return forbidden();
    }

    let skins_dir = paths::skins_dir();
    let file_path = if chroma_id == skin_id {
        // Base skin preview.
        skins_dir.join(champion_id).join(skin_id).join(format!("{skin_id}.png"))
    } else {
        // Chroma preview.
        skins_dir.join(champion_id).join(skin_id).join(chroma_id).join(format!("{chroma_id}.png"))
    };

    serve_file(&file_path, "image/png", origin).await
}

async fn handle_asset(rest: &str, origin: Option<&str>) -> Response {
    // `paths::get_asset_path` already does the full traversal-hardening
    // dance (lexical component rejection + canonicalize-under-check); this
    // is a thin re-use, not a reimplementation.
    let asset_file = paths::get_asset_path(rest);
    if !asset_file.exists() {
        return not_found();
    }
    let content_type = content_type_for(&asset_file);
    serve_file(&asset_file, content_type, origin).await
}

async fn handle_plugin(rest: &str, origin: Option<&str>) -> Response {
    let mut parts = rest.splitn(2, '/');
    let (Some(plugin_name), Some(file_name)) = (parts.next(), parts.next()) else {
        return not_found();
    };
    if !is_safe_segment(plugin_name) {
        log_warn!("[bridge] Blocked path traversal attempt in /plugin/{rest}");
        return forbidden();
    }
    // `file_name` may itself contain sub-path separators (plugin assets in
    // nested folders) — validate every component instead of the whole
    // segment being separator-free.
    let file_path_rel = Path::new(file_name);
    let bad_component = file_path_rel.components().any(|c| {
        matches!(c, std::path::Component::ParentDir | std::path::Component::Prefix(_) | std::path::Component::RootDir)
    });
    if bad_component {
        log_warn!("[bridge] Blocked path traversal attempt in /plugin/{rest}");
        return forbidden();
    }

    let plugins_dir = paths::pengu_loader_dir().join("plugins");
    let file_path = plugins_dir.join(plugin_name).join(file_path_rel);
    let content_type = content_type_for(&file_path);
    serve_file(&file_path, content_type, origin).await
}

async fn serve_file(path: &Path, content_type: &str, origin: Option<&str>) -> Response {
    // Async read — a blocking std::fs::read here stalls the shared tokio reactor
    // thread (which also drives the Pengu-plugin WS ping/pong) on an AV-scanned
    // or OneDrive-redirected path.
    match tokio::fs::read(path).await {
        Ok(bytes) => file_response(bytes, content_type, origin),
        Err(_) => not_found(),
    }
}

/// Content-type table (ported verbatim from `HTTPHandler._get_content_type`).
fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("ttf") => "font/ttf",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ogg") => "audio/ogg",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        _ => "application/octet-stream",
    }
}

fn file_response(bytes: Vec<u8>, content_type: &str, origin: Option<&str>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "public, max-age=3600");
    builder = apply_cors(builder, origin);
    builder.body(Body::from(bytes)).unwrap_or_else(|_| not_found())
}

fn text_response(text: String, origin: Option<&str>) -> Response {
    let mut builder =
        Response::builder().status(StatusCode::OK).header(axum::http::header::CONTENT_TYPE, "text/plain");
    builder = apply_cors(builder, origin);
    builder.body(Body::from(text)).unwrap_or_else(|_| not_found())
}

/// Ported from `cors_headers_for_origin`: only a loopback origin gets
/// `Access-Control-Allow-Origin` echoed back (plus `Vary: Origin`); an
/// absent/non-loopback origin gets no CORS headers at all.
fn apply_cors(builder: axum::http::response::Builder, origin: Option<&str>) -> axum::http::response::Builder {
    match origin {
        Some(o) if is_loopback_origin(o) => {
            builder.header(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, o).header(axum::http::header::VARY, "Origin")
        }
        _ => builder,
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

/// Minimal `%XX` percent-decoder (ported from `urllib.parse.unquote`'s
/// effect on the request path) — self-contained rather than a new crate
/// dependency for this one use site.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_encoded_and_plain_text() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn safe_segment_rejects_traversal_and_separators() {
        assert!(!is_safe_segment(".."));
        assert!(!is_safe_segment("."));
        assert!(!is_safe_segment(""));
        assert!(!is_safe_segment("a/b"));
        assert!(!is_safe_segment("a\\b"));
        assert!(is_safe_segment("103000"));
    }

    #[test]
    fn content_type_table_matches_known_extensions() {
        assert_eq!(content_type_for(Path::new("x.png")), "image/png");
        assert_eq!(content_type_for(Path::new("x.JPG")), "image/jpeg");
        assert_eq!(content_type_for(Path::new("x.ttf")), "font/ttf");
        assert_eq!(content_type_for(Path::new("x.ogg")), "audio/ogg");
        assert_eq!(content_type_for(Path::new("x.js")), "application/javascript");
        assert_eq!(content_type_for(Path::new("x.css")), "text/css");
        assert_eq!(content_type_for(Path::new("x.bin")), "application/octet-stream");
    }
}
