// Chud "runes" Worker — fetches current-patch best-build data from u.gg's
// public overview JSON, normalizes it to Chud's client contract, and caches it
// at the Cloudflare edge. No Riot Web API key involved.
//
// Client contract (what Chud's runes.rs::RuneBuild expects):
//   GET /runes?champ=<id>&role=<top|jungle|mid|bot|support|"">&sort=<winrate|popular>
//   -> { name, runes:{primary,sub,perks:[9]}, spells:[2], items:{blocks:[...]} }
//
// Why a proxy (not client-direct): (1) u.gg blocks header-less fetches, so we
// add a browser UA here; (2) if u.gg changes shape we fix it server-side with
// no app update; (3) one cached origin fetch serves every user + every role.

const UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

// Chud role -> u.gg position enum (1=jungle 2=support 3=adc 4=top 5=mid 6=none).
const ROLE_POS = { top: 4, jungle: 1, mid: 5, bot: 3, support: 2 };
// u.gg aggregate slices: server 12 = World/all-regions. Tier differs by mode
// (ranked exposes 10 = Platinum+, ARAM exposes 8), so we pick robustly below.
const SERVER = "12";
const TIER_PREF = ["10", "8"];

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const cors = { "access-control-allow-origin": "*", "cache-control": "public, max-age=3600" };

    if (url.pathname.replace(/\/+$/, "") !== "/runes") {
      return json({ error: "not found" }, 404, cors);
    }
    const champ = parseInt(url.searchParams.get("champ") || "0", 10);
    const role = (url.searchParams.get("role") || "").toLowerCase();
    // ARAM has its own rune meta (different tier key + ARAM spells/items), so
    // the client passes mode=aram and we hit u.gg's ARAM data instead.
    const gameMode = (url.searchParams.get("mode") || "").toLowerCase() === "aram" ? "normal_aram" : "ranked_solo_5x5";
    if (!champ) return json({ error: "missing champ" }, 400, cors);

    try {
      const patch = await currentPatch();
      let overview = await fetchOverview(champ, patch, gameMode);
      // Trackers can lag a day post-patch; fall back one minor version.
      if (!overview) overview = await fetchOverview(champ, prevPatch(patch), gameMode);
      if (!overview) return json({ error: "upstream unavailable" }, 502, cors);

      const build = normalize(overview, role);
      if (!build) return json({ error: "no build for champ/role" }, 404, cors);
      return json(build, 200, cors);
    } catch (e) {
      return json({ error: String(e && e.message ? e.message : e) }, 502, cors);
    }
  },
};

async function currentPatch() {
  const r = await fetch("https://ddragon.leagueoflegends.com/api/versions.json", { cf: { cacheTtl: 3600 } });
  const v = (await r.json())[0]; // e.g. "16.13.1"
  return v.split(".").slice(0, 2).join("_"); // "16_13"
}

function prevPatch(p) {
  const [maj, min] = p.split("_").map(Number);
  return min > 1 ? `${maj}_${min - 1}` : `${maj - 1}_24`;
}

async function fetchOverview(champ, patch, gameMode) {
  const u = `https://stats2.u.gg/lol/1.5/overview/${patch}/${gameMode}/${champ}/1.5.0.json`;
  const r = await fetch(u, { headers: { "user-agent": UA }, cf: { cacheTtl: 21600, cacheEverything: true } });
  if (!r.ok) return null; // 403 = blocked OR missing patch/champ file; either way, no data
  return r.json();
}

// u.gg overview -> Chud contract. Shape (empirically mapped):
//   overview[server][tier][position] = [positionDataArray, isoTimestamp]
//   positionDataArray[0] perks:  [games, wins, primaryTree, subTree, [6 perkIds]]
//   positionDataArray[1] spells: [games, wins, [spell1, spell2]]
//   positionDataArray[2] start:  [games, wins, [itemIds]]
//   positionDataArray[8] shards: [games, wins, ["s1","s2","s3"]]
function normalize(overview, role) {
  const server = overview?.[SERVER] || overview?.[Object.keys(overview || {})[0]];
  if (!server) return null;
  // Pick the tier: ranked exposes 10 (Plat+), ARAM exposes 8; else take
  // whatever's present rather than 404 the request.
  let tier = null;
  for (const t of TIER_PREF) if (server[t]) { tier = server[t]; break; }
  if (!tier) tier = server[Object.keys(server)[0]];
  if (!tier) return null;

  let data = null;
  const pos = ROLE_POS[role];
  if (pos && tier[pos]) data = tier[pos][0];
  if (!data) {
    // Unknown role (blind pick / ARAM) -> pick the most-played position.
    let bestGames = -1;
    for (const key of Object.keys(tier)) {
      const d = tier[key]?.[0];
      const games = d?.[0]?.[0] || 0;
      if (games > bestGames) {
        bestGames = games;
        data = d;
      }
    }
  }
  if (!data) return null;

  const perks = data[0] || [];
  const primary = perks[2];
  const sub = perks[3];
  const perkIds = (perks[4] || []).map((p) => (Array.isArray(p) ? p[0] : p));
  const shards = (data[8]?.[2] || []).map((s) => parseInt(s, 10));
  const selected = perkIds.concat(shards); // 6 + 3 = 9
  if (!primary || !sub || selected.length !== 9) return null;

  const spells = (data[1]?.[2] || []).slice(0, 2);
  const startItems = data[2]?.[2] || [];

  const blocks = [];
  if (startItems.length) blocks.push({ type: "Starting", items: startItems });

  return {
    name: "",
    runes: { primary, sub, perks: selected },
    spells,
    items: { blocks },
  };
}

function json(obj, status = 200, extra = {}) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: { "content-type": "application/json", ...extra },
  });
}
