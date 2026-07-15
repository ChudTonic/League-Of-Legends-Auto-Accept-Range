//! Player profile aggregation from the local League Client (LCU).
//!
//! Pulls current-summoner, chat presence, ranked stats, champion mastery, and
//! the last 20 matches, then derives the op.gg-style figures (CS/min, KDA,
//! kill participation, MVP, champ pool, recent-performance summary) into the
//! JSON shape the Profile UI consumes. Image art is referenced by numeric id
//! and proxied through the `lcu://` asset scheme (see lib.rs) — no Data Dragon,
//! no id→key mapping, works offline against the client's own asset server.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::lcu::{self, Auth};

fn i(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn queue_names(queue_id: i64) -> (&'static str, &'static str) {
    match queue_id {
        420 => ("Ranked Solo/Duo", "Solo/Duo"),
        440 => ("Ranked Flex", "Flex"),
        400 => ("Normal Draft", "Normal"),
        430 => ("Normal Blind", "Normal"),
        450 => ("ARAM", "ARAM"),
        700 => ("Clash", "Clash"),
        830 | 840 | 850 => ("Co-op vs AI", "Bots"),
        900 => ("URF", "URF"),
        1700 => ("Arena", "Arena"),
        _ => ("Custom", "Custom"),
    }
}

fn time_ago(game_creation_ms: i64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let secs = ((now_ms - game_creation_ms) / 1000).max(0);
    if secs < 3600 {
        format!("{}m ago", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn duration_label(secs: i64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn role_label(lane: &str, role: &str) -> (&'static str, &'static str) {
    // (canonical role key, short label)
    match lane {
        "TOP" => ("TOP", "Top"),
        "JUNGLE" => ("JUNGLE", "Jungle"),
        "MIDDLE" | "MID" => ("MIDDLE", "Mid"),
        "BOTTOM" | "BOT" => {
            if role.contains("SUPPORT") {
                ("UTILITY", "Support")
            } else {
                ("BOTTOM", "ADC")
            }
        }
        _ => ("UNKNOWN", "Other"),
    }
}

/// Build the full profile JSON, or `{ "clientOnline": false }` when the client
/// isn't reachable.
pub async fn build_profile(client: &reqwest::Client, auth: &Auth) -> Value {
    let summoner = match lcu::get_json(client, auth, "/lol-summoner/v1/current-summoner").await {
        Some(v) => v,
        None => return json!({ "clientOnline": false }),
    };
    let puuid = s(&summoner, "puuid");
    let summoner_id = i(&summoner, "summonerId");

    let chat = lcu::get_json(client, auth, "/lol-chat/v1/me").await.unwrap_or(json!({}));
    let ranked_raw = lcu::get_json(client, auth, "/lol-ranked/v1/current-ranked-stats").await.unwrap_or(json!({}));
    let matches_raw = lcu::get_json(
        client,
        auth,
        "/lol-match-history/v1/products/lol/current-summoner/matches?begIndex=0&endIndex=20",
    )
    .await
    .unwrap_or(json!({}));
    let mastery_raw = lcu::get_json(
        client,
        auth,
        &format!("/lol-collections/v1/inventories/{summoner_id}/champion-mastery"),
    )
    .await
    .unwrap_or(json!([]));
    let champ_summary = lcu::get_json(client, auth, "/lol-game-data/assets/v1/champion-summary.json")
        .await
        .unwrap_or(json!([]));
    // Items and summoner spells have NO `/v1/{kind}/{id}.png` endpoint (those 400);
    // their icons must be resolved by id -> iconPath from these summary files.
    let items_summary = lcu::get_json(client, auth, "/lol-game-data/assets/v1/items.json")
        .await
        .unwrap_or(json!([]));
    let spells_summary = lcu::get_json(client, auth, "/lol-game-data/assets/v1/summoner-spells.json")
        .await
        .unwrap_or(json!([]));

    // championId -> display name
    let mut champ_name: HashMap<i64, String> = HashMap::new();
    if let Some(arr) = champ_summary.as_array() {
        for c in arr {
            champ_name.insert(i(c, "id"), s(c, "name"));
        }
    }
    let name_of = |id: i64| champ_name.get(&id).cloned().unwrap_or_else(|| id.to_string());

    // id -> iconPath (full LCU asset path, served via the lcu:// proxy)
    let icon_map = |arr: &Value| -> HashMap<i64, String> {
        let mut m = HashMap::new();
        if let Some(list) = arr.as_array() {
            for e in list {
                let path = s(e, "iconPath");
                if !path.is_empty() {
                    m.insert(i(e, "id"), path);
                }
            }
        }
        m
    };
    let item_icon_path = icon_map(&items_summary);
    let spell_icon_path = icon_map(&spells_summary);
    let item_icon = |id: i64| -> Value {
        match item_icon_path.get(&id) {
            Some(p) if id > 0 => json!(p),
            _ => Value::Null,
        }
    };
    let spell_icon = |id: i64| -> Value {
        match spell_icon_path.get(&id) {
            Some(p) if id > 0 => json!(p),
            _ => Value::Null,
        }
    };

    // championId -> mastery level
    let mut mastery_lvl: HashMap<i64, i64> = HashMap::new();
    if let Some(arr) = mastery_raw.as_array() {
        for m in arr {
            mastery_lvl.insert(i(m, "championId"), i(m, "championLevel"));
        }
    }

    // ── Identity ───────────────────────────────────────────────────────────
    let availability = match s(&chat, "availability").as_str() {
        "chat" | "online" => "online",
        "away" => "away",
        "dnd" | "inGame" => "inGame",
        _ if s(&summoner, "gameName").is_empty() => "offline",
        _ => "online",
    };
    let status_message = {
        let m = s(&chat, "statusMessage");
        if m.is_empty() {
            match availability {
                "inGame" => "In Game".into(),
                "away" => "Away".into(),
                _ => "Online".into(),
            }
        } else {
            m
        }
    };
    let summoner_json = json!({
        "gameName": s(&summoner, "gameName"),
        "tagLine": s(&summoner, "tagLine"),
        "summonerLevel": i(&summoner, "summonerLevel"),
        "profileIconId": i(&summoner, "profileIconId"),
        "xpSinceLastLevel": i(&summoner, "xpSinceLastLevel"),
        "xpUntilNextLevel": i(&summoner, "xpUntilNextLevel"),
        "availability": availability,
        "statusMessage": status_message,
        "syncedAgo": "now",
    });

    // ── Ranked ─────────────────────────────────────────────────────────────
    let queue_map = ranked_raw.get("queueMap").cloned().unwrap_or(json!({}));
    let mut ranked = Vec::new();
    for (key, label) in [("RANKED_SOLO_5x5", "Ranked Solo/Duo"), ("RANKED_FLEX_SR", "Ranked Flex")] {
        if let Some(q) = queue_map.get(key) {
            let tier = s(q, "tier");
            let tier = if tier.is_empty() { "UNRANKED".to_string() } else { tier };
            let series = q
                .get("miniSeriesProgress")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty() && *p != "NNN")
                .map(|p| json!({ "progress": p }));
            ranked.push(json!({
                "queue": key,
                "label": label,
                "tier": tier,
                "division": s(q, "division"),
                "lp": i(q, "leaguePoints"),
                "wins": i(q, "wins"),
                "losses": i(q, "losses"),
                "hotStreak": q.get("isHotStreak").and_then(Value::as_bool).unwrap_or(false),
                "series": series,
                "percentile": "",
            }));
        }
    }

    // ── Matches ──────────────────────────────────────────────────────────--
    let games = matches_raw
        .get("games")
        .and_then(|g| g.get("games"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // The match-history list only has the current player per game; scoreboard/kill
    // participation/MVP need the full roster from each game's detail endpoint
    // (a superset, so it replaces the list entry wholesale below; failed detail
    // fetches fall back to list data). Fetched in bounded batches so we don't
    // flood the LCU and stall the client.
    const DETAIL_CONCURRENCY: usize = 5;
    let mut detail_map: HashMap<i64, Value> = HashMap::new();
    for chunk in games.chunks(DETAIL_CONCURRENCY) {
        let mut handles: Vec<(i64, tauri::async_runtime::JoinHandle<Option<Value>>)> = Vec::new();
        for g in chunk {
            let gid = i(g, "gameId");
            let client = client.clone();
            let auth = auth.clone();
            handles.push((
                gid,
                tauri::async_runtime::spawn(async move {
                    lcu::get_json(&client, &auth, &format!("/lol-match-history/v1/games/{gid}")).await
                }),
            ));
        }
        for (gid, h) in handles {
            if let Ok(Some(detail)) = h.await {
                detail_map.insert(gid, detail);
            }
        }
    }

    let empty = json!({}); // longer-lived default for `.unwrap_or(&empty)`
    let mut matches = Vec::new();
    // champ pool accumulation: id -> [games, wins, k, d, a, csmin]
    let mut pool: HashMap<i64, [f64; 6]> = HashMap::new();
    // perf accumulation
    let (mut p_games, mut p_wins, mut p_k, mut p_d, mut p_a, mut p_kp, mut p_csm, mut p_vis, mut p_dmgm) =
        (0i64, 0i64, 0f64, 0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
    let mut role_counts: HashMap<&'static str, (i64, &'static str)> = HashMap::new();

    for (idx, g_list) in games.iter().enumerate() {
        // Prefer the full detail object (all 10 players); fall back to the list.
        let g = detail_map.get(&i(g_list, "gameId")).unwrap_or(g_list);
        // map participantId -> identity player
        let identities = g.get("participantIdentities").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut id_player: HashMap<i64, Value> = HashMap::new();
        let mut my_participant_id = -1i64;
        for ident in &identities {
            let pid = i(ident, "participantId");
            let player = ident.get("player").cloned().unwrap_or(json!({}));
            if (!puuid.is_empty() && s(&player, "puuid") == puuid)
                || (summoner_id != 0 && i(&player, "summonerId") == summoner_id)
            {
                my_participant_id = pid;
            }
            id_player.insert(pid, player);
        }

        let participants = g.get("participants").and_then(Value::as_array).cloned().unwrap_or_default();
        let me = participants.iter().find(|p| i(p, "participantId") == my_participant_id).cloned();
        let me = match me {
            Some(m) => m,
            None => continue,
        };
        let st = me.get("stats").cloned().unwrap_or(json!({}));
        let my_team = i(&me, "teamId");
        let win = st.get("win").and_then(Value::as_bool).unwrap_or(false);
        let (k, d, a) = (i(&st, "kills"), i(&st, "deaths"), i(&st, "assists"));
        let cs = i(&st, "totalMinionsKilled") + i(&st, "neutralMinionsKilled");
        let dur = i(g, "gameDuration").max(1);
        let cs_min = (cs as f64) / (dur as f64 / 60.0);
        let team_kills: i64 = participants
            .iter()
            .filter(|p| i(p, "teamId") == my_team)
            .map(|p| i(p.get("stats").unwrap_or(&empty), "kills"))
            .sum();
        let kill_p = if team_kills > 0 { ((k + a) as f64 / team_kills as f64 * 100.0).round() } else { 0.0 };
        let champ_id = i(&me, "championId");
        let queue_id = i(g, "queueId");
        let (queue, queue_short) = queue_names(queue_id);

        // MVP = highest KDA on the winning team, and it's me
        let mvp = {
            let win_team: i64 = participants
                .iter()
                .map(|p| (i(p, "teamId"), p.get("stats").unwrap_or(&empty).clone()))
                .filter(|(_, s)| s.get("win").and_then(Value::as_bool).unwrap_or(false))
                .map(|(t, _)| t)
                .next()
                .unwrap_or(-1);
            let best = participants
                .iter()
                .filter(|p| i(p, "teamId") == win_team)
                .map(|p| {
                    let s = p.get("stats").unwrap_or(&empty);
                    let kda = (i(s, "kills") + i(s, "assists")) as f64 / (i(s, "deaths").max(1)) as f64;
                    (i(p, "participantId"), kda)
                })
                .fold((-1i64, -1.0f64), |best, cur| if cur.1 > best.1 { cur } else { best });
            win && best.0 == my_participant_id
        };

        let timeline = me.get("timeline").cloned().unwrap_or(json!({}));
        let (role_key, role_short) = role_label(&s(&timeline, "lane"), &s(&timeline, "role"));

        // scoreboard teams
        let team_player = |team_id: i64| -> Vec<Value> {
            participants
                .iter()
                .filter(|p| i(p, "teamId") == team_id)
                .map(|p| {
                    let pst = p.get("stats").unwrap_or(&empty);
                    let pid = i(p, "participantId");
                    let player = id_player.get(&pid).cloned().unwrap_or(json!({}));
                    let pname = {
                        let gn = s(&player, "gameName");
                        if gn.is_empty() { s(&player, "summonerName") } else { gn }
                    };
                    json!({
                        "champ": i(p, "championId"),
                        "champName": name_of(i(p, "championId")),
                        "name": pname,
                        "isMe": pid == my_participant_id,
                        "k": i(pst, "kills"), "d": i(pst, "deaths"), "a": i(pst, "assists"),
                    })
                })
                .collect()
        };
        let enemy_team = if my_team == 100 { 200 } else { 100 };

        matches.push(json!({
            "id": i(g, "gameId"),
            "result": if win { "win" } else { "loss" },
            "queue": queue, "queueShort": queue_short,
            "champ": champ_id, "champName": name_of(champ_id),
            "role": role_short,
            "lvl": i(&st, "champLevel"),
            "lpDelta": null,
            "timeAgo": time_ago(i(g, "gameCreation")),
            "length": duration_label(dur),
            "k": k, "d": d, "a": a,
            "cs": cs, "csMin": (cs_min * 10.0).round() / 10.0,
            "killP": kill_p as i64,
            "dmg": i(&st, "totalDamageDealtToChampions"),
            "vision": i(&st, "visionScore"),
            "spells": [i(&me, "spell1Id"), i(&me, "spell2Id")],
            "spellIcons": [spell_icon(i(&me, "spell1Id")), spell_icon(i(&me, "spell2Id"))],
            "items": [i(&st,"item0"), i(&st,"item1"), i(&st,"item2"), i(&st,"item3"), i(&st,"item4"), i(&st,"item5"), i(&st,"item6")],
            "itemIcons": [
                item_icon(i(&st,"item0")), item_icon(i(&st,"item1")), item_icon(i(&st,"item2")),
                item_icon(i(&st,"item3")), item_icon(i(&st,"item4")), item_icon(i(&st,"item5")),
                item_icon(i(&st,"item6"))
            ],
            "mvp": mvp,
            "team": {
                "ally": team_player(my_team),
                "enemy": team_player(enemy_team),
            },
        }));

        // accumulate champ pool + perf
        let e = pool.entry(champ_id).or_insert([0.0; 6]);
        e[0] += 1.0;
        e[1] += if win { 1.0 } else { 0.0 };
        e[2] += k as f64;
        e[3] += d as f64;
        e[4] += a as f64;
        e[5] += cs_min;

        p_games += 1;
        p_wins += if win { 1 } else { 0 };
        p_k += k as f64; p_d += d as f64; p_a += a as f64;
        p_kp += kill_p; p_csm += cs_min; p_vis += i(&st, "visionScore") as f64;
        p_dmgm += i(&st, "totalDamageDealtToChampions") as f64 / (dur as f64 / 60.0);
        let rc = role_counts.entry(role_key).or_insert((0, role_short));
        rc.0 += 1;
        let _ = idx;
    }

    // ── Champion pool (top 6 by games) ───────────────────────────────────────
    let mut pool_vec: Vec<(i64, [f64; 6])> = pool.into_iter().collect();
    pool_vec.sort_by(|a, b| b.1[0].partial_cmp(&a.1[0]).unwrap_or(std::cmp::Ordering::Equal));
    let champ_pool: Vec<Value> = pool_vec
        .into_iter()
        .take(6)
        .map(|(id, e)| {
            let g = e[0].max(1.0);
            json!({
                "id": id,
                "name": name_of(id),
                "games": e[0] as i64,
                "wins": e[1] as i64,
                "k": (e[2] / g * 10.0).round() / 10.0,
                "d": (e[3] / g * 10.0).round() / 10.0,
                "a": (e[4] / g * 10.0).round() / 10.0,
                "cs": (e[5] / g * 10.0).round() / 10.0,
                "mastery": mastery_lvl.get(&id).copied().unwrap_or(0),
            })
        })
        .collect();

    // ── Recent performance ────────────────────────────────────────────────--
    let pg = p_games.max(1) as f64;
    let mut roles: Vec<Value> = role_counts
        .values()
        .filter(|(c, _)| *c > 0)
        .map(|(c, label)| json!({ "label": label, "pct": ((*c as f64 / pg) * 100.0).round() as i64 }))
        .collect();
    roles.sort_by(|a, b| i(b, "pct").cmp(&i(a, "pct")));
    roles.truncate(3);

    let perf = json!({
        "games": p_games,
        "wins": p_wins,
        "losses": p_games - p_wins,
        "k": (p_k / pg * 10.0).round() / 10.0,
        "d": (p_d / pg * 10.0).round() / 10.0,
        "a": (p_a / pg * 10.0).round() / 10.0,
        "killP": (p_kp / pg).round() as i64,
        "csMin": (p_csm / pg * 10.0).round() / 10.0,
        "vision": (p_vis / pg).round() as i64,
        "dmgMin": (p_dmgm / pg).round() as i64,
        "roles": roles,
    });

    json!({
        "clientOnline": true,
        "endpoint": "/lol-summoner/v1/current-summoner",
        "summoner": summoner_json,
        "ranked": ranked,
        "champPool": champ_pool,
        "perf": perf,
        "matches": matches,
    })
}
