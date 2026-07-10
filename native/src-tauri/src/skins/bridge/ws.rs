//! WebSocket upgrade + connection loop (S4) — ported from `pengu\core\
//! websocket_server.py`'s `WebSocketServer`. One axum `fallback` handler
//! serves both HTTP and the WebSocket upgrade on the same port (mirroring
//! Python's `serve(..., process_request=self._process_http_request)`
//! dual-purpose handler): a request with `Upgrade: websocket` is promoted to
//! a socket, everything else falls through to `http::route`.
//!
//! BROADCAST-ONLY fanout (hard behavior contract, `docs/SKINS_PORT.md` §3):
//! every inbound message's response/side-effect broadcast goes to ALL
//! connected clients via `BridgeHandle::subscribe`'s fanout channel — there
//! is no per-connection targeted reply anywhere in this module, matching
//! Python's `_send_response`/`Broadcaster` both calling the same
//! `websocket_server.broadcast(...)` regardless of which client sent the
//! triggering message.
//!
//! Keepalive: `ping_interval=20s` / `ping_timeout=20s` (ported from Python's
//! `serve(..., ping_interval=20, ping_timeout=20)` — tuned for AV/VPN
//! compatibility, preserve). Axum's `WebSocketUpgrade` has no built-in
//! periodic-ping option (unlike the Python `websockets` library), so this is
//! reimplemented explicitly: a `Ping` frame is sent every 20s, and the
//! connection is dropped if no frame of any kind (including the client's
//! automatic `Pong`) has been seen for 40s (20s ping cadence + 20s grace,
//! i.e. one missed ping cycle).

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

/// Build the axum router: a single fallback dispatches every request
/// (mirrors Python's one `_handler`/`_process_http_request` pair covering the
/// whole port).
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
                        // Ping/Pong/Binary frames don't carry a payload we
                        // route, but they do count as activity for the
                        // timeout check below.
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
