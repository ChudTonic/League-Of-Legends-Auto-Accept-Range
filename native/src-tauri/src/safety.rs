//! Anti-cheat guard. `isRanked` is the authoritative signal; queue-id is a fallback.
#![allow(dead_code)] // wired in by later injection tools

/// Ranked Solo/Duo (420) and Ranked Flex SR (440). 470 (Ranked Flex TT) was
/// removed with Twisted Treeline.
pub const RANKED_QUEUE_IDS: [i64; 2] = [420, 440];

/// `Some(true/false)` if the live game's ranked state is known, `None` if it
/// can't be determined.
pub fn queue_is_ranked(session: &serde_json::Value) -> Option<bool> {
    let queue = session.get("gameData")?.get("queue")?;
    if let Some(is_ranked) = queue.get("isRanked").and_then(|v| v.as_bool()) {
        return Some(is_ranked);
    }
    if let Some(id) = queue.get("id").and_then(|v| v.as_i64()) {
        return Some(RANKED_QUEUE_IDS.contains(&id));
    }
    None
}

/// Gameflow phases where a match is actually live (champ pick onward) — only
/// then does the ranked kill-switch apply; outside a game there's nothing to protect.
fn game_is_live(session: &serde_json::Value) -> bool {
    matches!(
        session.get("phase").and_then(|v| v.as_str()),
        Some("ChampSelect") | Some("GameStart") | Some("InProgress") | Some("Reconnect")
    )
}

/// Whether injection tools should block right now. Fails safe: if live but ranked
/// state is unknown, block. Never blocks outside a live game.
pub fn should_block(session: &serde_json::Value) -> bool {
    if !game_is_live(session) {
        return false;
    }
    match queue_is_ranked(session) {
        Some(is_ranked) => is_ranked,
        None => true, // live game, unknown queue -> fail safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_ranked_flag_wins() {
        let s = json!({"gameData": {"queue": {"isRanked": true, "id": 430}}});
        assert_eq!(queue_is_ranked(&s), Some(true));
    }

    #[test]
    fn falls_back_to_queue_id() {
        let s = json!({"gameData": {"queue": {"id": 420}}});
        assert_eq!(queue_is_ranked(&s), Some(true));
        let s = json!({"gameData": {"queue": {"id": 430}}});
        assert_eq!(queue_is_ranked(&s), Some(false));
    }

    #[test]
    fn unknown_when_missing() {
        assert_eq!(queue_is_ranked(&json!({})), None);
    }
}
