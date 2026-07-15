//! WebSocket upgrade + connection loop (S4) — ported from `pengu\core\
//! websocket_server.py`. One axum `fallback` handler serves both HTTP and
//! the WS upgrade on the same port: `Upgrade: websocket` promotes to a
//! socket, everything else falls through to `http::route`.
//!
//! BROADCAST-ONLY (hard contract, `docs/SKINS_PORT.md` §3): every inbound
//! message's response goes to ALL connected clients via `BridgeHandle::subscribe`
//! — no per-connection targeted reply anywhere in this module.
//!
//! Keepalive: ping every 20s, drop after 40s of no frames (20s cadence + 20s
//! grace = one missed cycle) — tuned for AV/VPN compatibility, preserve.
//! Axum has no built-in periodic-ping (unlike Python's `websockets` lib), so
//! this is reimplemented explicitly.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};

use crate::skins::slog::{log_info, log_warn};

use super::{handlers, http, is_loopback_origin, BridgeContext};

const PING_INTERVAL: Duration = Duration::from_secs(20);
const PING_TIMEOUT: Duration = Duration::from_secs(20);

/// Build the axum router: a single fallback dispatches every request.
pub fn router(ctx: BridgeContext) -> Router {
    Router::new().fallback(get(dispatch)).with_state(ctx)
}

async fn dispatch(
    State(ctx): State<BridgeContext>,
    headers: HeaderMap,
    uri: Uri,
    ws: Option<WebSocketUpgrade>,
) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if let Some(origin) = origin {
        if !is_loopback_origin(origin) {
            log_warn!("[bridge] Blocked request from non-loopback origin: {origin}");
            return (StatusCode::FORBIDDEN, "Forbidden").into_response();
        }
    }

    if let Some(upgrade) = ws {
        return upgrade.on_upgrade(move |socket| handle_socket(socket, ctx));
    }

    http::route(&ctx, uri.path(), origin).await
}

/// Per-connection loop: forwards the broadcast fanout to this client, feeds
/// inbound text frames to `handlers::dispatch`, and drives the ping/timeout
/// keepalive described in the module doc comment.
async fn handle_socket(socket: WebSocket, ctx: BridgeContext) {
    log_info!("[bridge] Client connected");
    let (mut sender, mut receiver) = socket.split();
    let mut broadcast_rx = ctx.handle.subscribe();
    let mut last_activity = Instant::now();
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    ping_timer.tick().await; // first tick fires immediately; consume it so the loop starts idle

    loop {
        tokio::select! {
            broadcasted = broadcast_rx.recv() => {
                match broadcasted {
                    Ok(text) => {
                        if sender.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        log_warn!("[bridge] Client lagged behind broadcast fanout, skipped {skipped} message(s)");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        last_activity = Instant::now();
                        handlers::dispatch(&ctx, &text).await;
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {
                        // Ping/Pong/Binary: no payload to route, but still counts as activity.
                        last_activity = Instant::now();
                    }
                    Some(Err(e)) => {
                        log_warn!("[bridge] Client connection error: {e}");
                        break;
                    }
                    None => break, // client closed the TCP stream
                }
            }
            _ = ping_timer.tick() => {
                if last_activity.elapsed() > PING_INTERVAL + PING_TIMEOUT {
                    log_warn!("[bridge] Client timed out (no activity for {:?}) - closing", last_activity.elapsed());
                    break;
                }
                if sender.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
    log_info!("[bridge] Client disconnected");
}
