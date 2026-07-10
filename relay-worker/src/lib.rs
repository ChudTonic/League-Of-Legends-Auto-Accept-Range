//! Chud party relay — Cloudflare Worker + Durable Object room.
//!
//! One DO instance per room key (derived client-side from the party token, so
//! knowing the key IS the room secret — no other auth). Members hold a single
//! hibernatable WebSocket each; per-member state lives in the socket
//! attachment, so the room needs no storage and evaporates with its sockets.
//!
//! Wire contract (the desktop client depends on it exactly):
//!   client → server: {"type":"join",summoner_id,summoner_name}
//!                    {"type":"skin","skin":{...}|null}
//!                    {"type":"leave"}
//!                    bare text "ping" (keepalive — answered with bare "pong")
//!   server → client: {"type":"members","members":[{summoner_id,summoner_name,skin?},..]}
//!                    (full list, broadcast to everyone on every join/skin/close/error)

use serde::{Deserialize, Serialize};
use worker::*;

const MAX_MEMBERS: usize = 10;

/// Per-socket member state, persisted in the hibernatable-socket attachment.
#[derive(Serialize, Deserialize, Clone)]
struct Member {
    summoner_id: u64,
    summoner_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    skin: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    summoner_id: Option<u64>,
    summoner_name: Option<String>,
    skin: Option<serde_json::Value>,
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    let upgrade = req.headers().get("Upgrade")?.unwrap_or_default();
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Response::from_json(&serde_json::json!({
            "status": "ok",
            "service": "chud-party-relay",
        }));
    }

    let url = req.url()?;
    let key = url
        .query_pairs()
        .find(|(k, _)| k == "key")
        .map(|(_, v)| v.to_string())
        .unwrap_or_default();
    if key.len() < 8 || key.len() > 64 {
        return Response::error("Missing or invalid room key", 400);
    }

    let ns = env.durable_object("ROOMS")?;
    let stub = ns.id_from_name(&key)?.get_stub()?;
    stub.fetch_with_request(req).await
}

#[durable_object]
pub struct PartyRoom {
    state: State,
}

impl PartyRoom {
    fn members(&self) -> Vec<Member> {
        self.state
            .get_websockets()
            .iter()
            .filter_map(|ws| ws.deserialize_attachment::<Member>().ok().flatten())
            .collect()
    }

    /// Full member list to every socket — the protocol has no diffs or
    /// targeted replies, by design.
    fn broadcast_members(&self) {
        let payload = serde_json::json!({ "type": "members", "members": self.members() });
        let text = payload.to_string();
        for ws in self.state.get_websockets() {
            let _ = ws.send_with_str(&text);
        }
    }
}

impl DurableObject for PartyRoom {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, _req: Request) -> Result<Response> {
        if self.state.get_websockets().len() >= MAX_MEMBERS {
            return Response::error("Room is full", 409);
        }
        let pair = WebSocketPair::new()?;
        self.state.accept_web_socket(&pair.server);
        Response::from_websocket(pair.client)
    }

    async fn websocket_message(
        &self,
        ws: WebSocket,
        message: WebSocketIncomingMessage,
    ) -> Result<()> {
        let WebSocketIncomingMessage::String(text) = message else {
            return Ok(());
        };
        // Keepalive is a bare TEXT frame, not a WS control frame — the desktop
        // client sends literal "ping" and string-matches the "pong" reply.
        if text == "ping" {
            let _ = ws.send_with_str("pong");
            return Ok(());
        }
        let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else {
            return Ok(());
        };
        match msg.kind.as_str() {
            "join" => {
                let member = Member {
                    summoner_id: msg.summoner_id.unwrap_or_default(),
                    summoner_name: msg.summoner_name.unwrap_or_default(),
                    skin: None,
                };
                ws.serialize_attachment(&member)?;
                self.broadcast_members();
            }
            "skin" => {
                if let Ok(Some(mut member)) = ws.deserialize_attachment::<Member>() {
                    member.skin = msg.skin;
                    ws.serialize_attachment(&member)?;
                    self.broadcast_members();
                }
            }
            "leave" => {
                // A server-initiated close does NOT fire websocket_close, so
                // void the attachment and broadcast here or the remaining
                // members keep a stale roster until the next event.
                let _ = ws.serialize_attachment(&serde_json::Value::Null);
                let _ = ws.close(Some(1000), Some("client left"));
                self.broadcast_members();
            }
            _ => {}
        }
        Ok(())
    }

    async fn websocket_close(
        &self,
        _ws: WebSocket,
        _code: usize,
        _reason: String,
        _was_clean: bool,
    ) -> Result<()> {
        self.broadcast_members();
        Ok(())
    }

    async fn websocket_error(&self, _ws: WebSocket, _error: Error) -> Result<()> {
        self.broadcast_members();
        Ok(())
    }
}
