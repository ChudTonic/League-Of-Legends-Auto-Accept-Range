/**
 * Chud catalog cache Worker. Serves a cached, self-hosted skin catalog + assets
 * from KV/R2. Endpoints: /catalog, /img/{key}, /download/{id}, /file/{id}, /meta.
 */

const RF = atob("aHR0cHM6Ly9ydW5lZm9yZ2UuZGV2");
const IMG = atob("aHR0cHM6Ly9yMi1pbWFnZXMtcHJvZC5ydW5lZm9yZ2UuZGV2");
const UA = "Chud-Desktop/1.0 (+https://github.com/ChudTonic; catalog cache)";
const PAGE_SIZE = 100;
const CHUNK_PAGES = 3;

function cors(resp) {
  const h = new Headers(resp.headers);
  h.set("Access-Control-Allow-Origin", "*");
  h.set("Access-Control-Allow-Methods", "GET, OPTIONS");
  h.set("Access-Control-Allow-Headers", "*");
  return new Response(resp.body, { status: resp.status, headers: h });
}
function json(obj, status = 200) {
  // no-store: these API responses are dynamic; the Cloudflare edge otherwise
  // caches GET responses and serves stale catalog/meta.
  return cors(new Response(JSON.stringify(obj), { status, headers: { "Content-Type": "application/json", "Cache-Control": "no-store" } }));
}

// Extract + archive a YouTube video id from a mod's description (many posts
// embed a showcase video). Stored so it survives even if the source post
// changes; the app plays it in an in-modal embed.
function ytId(text) {
  const m = (text || "").match(/(?:youtube\.com\/watch\?v=|youtu\.be\/|youtube\.com\/embed\/)([A-Za-z0-9_-]{11})/i);
  return m ? m[1] : null;
}

// Chud Originals — our own first-party packs, merged into the catalog at
// serve time (the daily upstream crawl overwrites catalog:v1, so these can't
// live in KV). Their files are uploaded to R2 at f/{id}.fantome, so the
// existing /file path serves them and they're always "ready".
const CURATED = [
  {
    id: "chud-burntpeanut",
    name: "BurntPeanut Announcer",
    video: null,
    champions: [],
    category: "announcer",
    themes: ["meme"],
    thumbKey: "chud/burntpeanut.jpg",
    author: "TheBurntPeanut",
    views: 0,
    installs: 0,
    likes: 0,
    trending: false,
    working: true,
    description: "Hear TheBurntPeanut call your kills, aces, and objectives. A Chud Original.",
    updatedAt: "2026-07-13T00:00:00.000Z",
    ready: true,
    chudOriginal: true,
  },
];

function normalize(m) {
  return {
    id: m.id,
    name: m.name,
    video: ytId(m.description),
    champions: (m.champions || []).map((c) => ({ id: c.id, name: c.name })),
    category: m.category || null,
    themes: (m.themes || []).map((t) => (t && t.name) || t).filter(Boolean),
    thumbKey: m.thumbnailKey || null,
    author: (m.publisher && m.publisher.username) || null,
    views: m.viewCount || 0,
    installs: m.downloadCount || 0,
    likes: m.likeCount || 0,
    trending: !!m.isTrending,
    working: (m.status || "working") === "working",
    description: (m.description || "").slice(0, 1600),
    updatedAt: m.updatedAt || null,
  };
}

// Returns { mods } on a successful 2xx response, or { error: status } on a
// non-2xx upstream response — callers must NOT treat an error the same as a
// genuine end-of-catalog (a short/empty 200 page).
async function fetchPage(page) {
  // Omit categories/champions[] params (the API 400s on empty-array strings).
  const u = `${RF}/api/mods?page=${page}&pageSize=${PAGE_SIZE}&sortBy=recently_updated`;
  const r = await fetch(u, { headers: { "User-Agent": UA } });
  if (!r.ok) return { error: r.status };
  const d = await r.json();
  return { mods: (d && d.mods) || [] };
}

function today() {
  return new Date().toISOString().slice(0, 10);
}

// Fetch CHUNK_PAGES pages, advance the cursor; assemble + idle when done.
async function crawlSpurt(env) {
  let state;
  try { state = JSON.parse((await env.CATALOG.get("crawl:state")) || "{}"); } catch (e) { state = {}; }
  const day = today();
  if (state.day !== day) state = { day, nextPage: 0, done: false, lastPage: null };
  if (state.done) return { idle: true, day };

  let reachedEnd = false;
  for (let i = 0; i < CHUNK_PAGES; i++) {
    const page = state.nextPage + i;
    const res = await fetchPage(page);
    if (res.error) {
      // Transient upstream failure (429/500/etc) — NOT the end of the catalog.
      // Leave state untouched (nextPage/done unchanged) so this same chunk is
      // retried on the next cron tick instead of truncating the catalog.
      console.error(`crawlSpurt: upstream error ${res.error} at page ${page}, will retry`);
      return { error: res.error, page, day };
    }
    const mods = res.mods;
    await env.CATALOG.put(`pg:${page}`, JSON.stringify(mods.map(normalize)));
    if (mods.length < PAGE_SIZE) { reachedEnd = true; state.lastPage = page; break; }
  }
  if (!reachedEnd) {
    state.nextPage += CHUNK_PAGES;
    await env.CATALOG.put("crawl:state", JSON.stringify(state));
    return { spurt: true, nextPage: state.nextPage, day };
  }

  // Assemble pg:0..lastPage into the live catalog.
  const all = [];
  for (let p = 0; p <= state.lastPage; p++) {
    const raw = await env.CATALOG.get(`pg:${p}`);
    if (raw) all.push(...JSON.parse(raw));
  }
  await env.CATALOG.put("catalog:v1", JSON.stringify(all));
  await env.CATALOG.put("meta:v1", JSON.stringify({ count: all.length, crawledAt: new Date().toISOString() }));
  state.done = true;
  await env.CATALOG.put("crawl:state", JSON.stringify(state));
  console.log(`crawlSpurt: catalog assembled, ${all.length} mods across ${state.lastPage + 1} pages`);
  return { assembled: all.length, day };
}

// Full one-shot crawl (initial seed only — triggered manually, not by users).
async function crawlFull(env) {
  const all = [];
  for (let page = 0; page < 80; page++) {
    const res = await fetchPage(page);
    if (res.error) { console.error(`crawlFull: upstream error ${res.error} at page ${page}, stopping`); break; }
    all.push(...res.mods.map(normalize));
    if (res.mods.length < PAGE_SIZE) break;
  }
  if (all.length) {
    await env.CATALOG.put("catalog:v1", JSON.stringify(all));
    await env.CATALOG.put("meta:v1", JSON.stringify({ count: all.length, crawledAt: new Date().toISOString() }));
    await env.CATALOG.put("crawl:state", JSON.stringify({ day: today(), nextPage: 0, done: true, lastPage: null }));
    console.log(`crawlFull: catalog assembled, ${all.length} mods`);
  }
  return all.length;
}

// In-memory cache of the parsed catalog (per isolate) so we don't re-read +
// re-parse the ~1.6MB KV blob on every request.
let _catCache = null;
async function getCatalog(env) {
  const now = Date.now();
  if (_catCache && now - _catCache.ts < 60000) return _catCache.data;
  const raw = await env.CATALOG.get("catalog:v1");
  const crawled = raw ? JSON.parse(raw) : [];
  // Chud Originals first, then the crawled catalog (dedup by id just in case).
  const ids = new Set(CURATED.map((c) => c.id));
  const data = [...CURATED, ...crawled.filter((m) => !ids.has(m.id))];
  _catCache = { data, ts: now };
  return data;
}

// Set of mod IDs whose file is in R2 ("ready" — installs instantly from us).
let _mirCache = null;
async function getMirrored(env) {
  const now = Date.now();
  if (_mirCache && now - _mirCache.ts < 20000) return _mirCache.set;
  const raw = await env.CATALOG.get("mirrored:v1");
  const set = new Set(raw ? JSON.parse(raw) : []);
  _mirCache = { set, ts: now };
  return set;
}
async function addMirrored(env, id) {
  const raw = await env.CATALOG.get("mirrored:v1");
  const arr = raw ? JSON.parse(raw) : [];
  if (!arr.includes(id)) {
    arr.push(id);
    await env.CATALOG.put("mirrored:v1", JSON.stringify(arr));
  }
  _mirCache = null;
}

// Mirror one file into R2 (resolve -> fetch -> put -> mark ready). Returns bytes
// or null. Used by the cron trickle and the on-demand /file path.
async function mirrorFile(env, modId) {
  if (!env.FILES) return null;
  const already = await env.FILES.head(`f/${modId}.fantome`);
  if (already) { await addMirrored(env, modId); return already.size; } // in R2 already → just mark ready
  const asset = await resolveDownload(env, modId);
  if (!asset) return null;
  let fr;
  try { fr = await fetch(asset, { headers: { "User-Agent": UA } }); } catch (e) { return null; }
  if (!fr.ok || !fr.body) return null;
  // STREAM into R2 — never buffer (files reach 150MB; the Worker has 128MB mem).
  await env.FILES.put(`f/${modId}.fantome`, fr.body, { httpMetadata: { contentType: "application/zip" } });
  await addMirrored(env, modId);
  return parseInt(fr.headers.get("content-length") || "0", 10) || 0;
}

// Run `fn` over `items` with at most `concurrency` in flight. Cloudflare
// caps simultaneous connections, so this keeps the mirror fast without
// tripping subrequest/connection limits.
async function pool(items, concurrency, fn) {
  let i = 0;
  const workers = Array.from({ length: Math.min(concurrency, items.length) }, async () => {
    while (i < items.length) {
      const idx = i++;
      await fn(items[idx]);
    }
  });
  await Promise.all(workers);
}

// Mirror up to N not-yet-mirrored files into R2, in parallel. Runs every cron
// tick; since it only mirrors what's missing, it bursts through the backlog
// then self-idles once the whole catalog is in R2.
async function trickleMirror(env, n) {
  if (!env.FILES) return { mirrored: 0 };
  const all = await getCatalog(env);
  const done = await getMirrored(env);
  // Prioritize the curated bundle skins so the featured packs become fully
  // installable first, then fill in the rest of the catalog.
  const bundleIds = new Set(BUNDLES.flatMap((b) => b.skinIds));
  const order = [...all].sort((a, b) => (bundleIds.has(b.id) ? 1 : 0) - (bundleIds.has(a.id) ? 1 : 0));
  const todo = order.filter((m) => !done.has(m.id)).slice(0, n);
  let count = 0;
  await pool(todo, 15, async (m) => {
    const bytes = await mirrorFile(env, m.id);
    if (bytes != null) count++;
  });
  if (count) console.log(`trickleMirror: mirrored ${count} file(s)`);
  // Health canary: if we TRIED a healthy batch and mirrored NONE, the most
  // likely cause is the upstream source changing its download URL format
  // again (the resolver stops finding artifacts). Record it so it's loud, not
  // silent — exposed on /meta as mirrorHealth and logged as an error.
  if (todo.length >= 10) {
    const stalled = count === 0;
    if (stalled) console.error(`trickleMirror STALL: tried ${todo.length}, mirrored 0 — resolveDownload may be broken (upstream format change?)`);
    await env.CATALOG.put(
      "mirror:health",
      JSON.stringify({ ts: new Date().toISOString(), tried: todo.length, mirrored: count, stalled }),
    );
  }
  return { mirrored: count, total: all.length, ready: done.size + count };
}

// Pre-warm thumbnails into R2 so the card grid loads instantly (images are
// small; the first browse otherwise pays a slow source fetch per tile).
async function warmImages(env, n) {
  if (!env.IMAGES) return { warmed: 0 };
  const all = await getCatalog(env);
  // Candidate thumbs (skip ones we already have) up to N, then warm in parallel.
  const withThumbs = all.filter((m) => m.thumbKey);
  const heads = await Promise.all(withThumbs.slice(0, n * 3).map(async (m) => ({ m, have: !!(await env.IMAGES.head(m.thumbKey)) })));
  const todo = heads.filter((h) => !h.have).slice(0, n).map((h) => h.m);
  let count = 0;
  await pool(todo, 15, async (m) => {
    try {
      const r = await fetch(`${IMG}/${m.thumbKey}`, { headers: { "User-Agent": UA } });
      if (r.ok && r.body) { await env.IMAGES.put(m.thumbKey, r.body, { httpMetadata: { contentType: r.headers.get("content-type") || "image/png" } }); count++; }
    } catch (e) {}
  });
  if (count) console.log(`warmImages: warmed ${count} thumb(s)`);
  return { warmed: count };
}

async function resolveDownload(env, modId) {
  const cacheKey = `dl:${modId}`;
  const cached = await env.CATALOG.get(cacheKey);
  if (cached) return cached;
  const u = `${RF}/mods/${encodeURIComponent(modId)}/releases.data?_routes=routes%2Fmods%2F%24modId%2Flayout%2Croutes%2Fmods%2F%24modId%2Freleases%2Findex`;
  let r;
  try { r = await fetch(u, { headers: { "User-Agent": UA } }); } catch (e) { return null; }
  if (!r.ok) return null;
  const text = await r.text();
  // Match any upstream artifact URL ending in .fantome (with optional query).
  // The source moved its storage path layout once already, so we no longer
  // pin the path segment — just find the .fantome URL (incl. `&` for query
  // strings). This survives future path changes without code edits.
  const m = text.match(/https:\/\/[A-Za-z0-9._%/?=+&-]*\.fantome[A-Za-z0-9._%/?=+&-]*/);
  if (!m) return null;
  await env.CATALOG.put(cacheKey, m[0], { expirationTtl: 604800 });
  return m[0];
}

// Serve image from cache -> R2 -> source (mirrored once).
async function serveImage(req, env, ctx, key) {
  const cache = caches.default;
  const cacheKey = new Request(new URL(req.url).toString());
  let hit = await cache.match(cacheKey);
  if (hit) return hit;

  if (env.IMAGES) {
    const obj = await env.IMAGES.get(key);
    if (obj) {
      const resp = cors(new Response(obj.body, { headers: { "Content-Type": (obj.httpMetadata && obj.httpMetadata.contentType) || "image/png", "Cache-Control": "public, max-age=2592000" } }));
      ctx.waitUntil(cache.put(cacheKey, resp.clone()));
      return resp;
    }
  }

  let r;
  try { r = await fetch(`${IMG}/${key}`, { headers: { "User-Agent": UA } }); } catch (e) { return cors(new Response("img fetch failed", { status: 502 })); }
  if (!r.ok) return cors(new Response("not found", { status: 404 }));
  const buf = await r.arrayBuffer();
  const ct = r.headers.get("Content-Type") || "image/png";
  if (env.IMAGES) ctx.waitUntil(env.IMAGES.put(key, buf.slice(0), { httpMetadata: { contentType: ct } }));
  const resp = cors(new Response(buf, { headers: { "Content-Type": ct, "Cache-Control": "public, max-age=2592000" } }));
  ctx.waitUntil(cache.put(cacheKey, resp.clone()));
  return resp;
}

// Curated "cosmetic loadout" bundles: the top-installed skins per marquee
// champion, shipped as one-click packs. Skin ids are catalog mod ids.
const BUNDLES = [
  { champ: "Yasuo", champId: 157, skinIds: ["b1355371-eb88-460d-a89a-beb4dae1400a", "fb42adb4-165c-4bbc-aa05-18e876bddd74", "08292f3d-5882-43ed-996c-32c9147f6f63", "193a6ae3-af69-451d-85b1-8b45bc3ca856", "0b3781e2-e51a-4eac-8300-e427e4f16345"] },
  { champ: "Ahri", champId: 103, skinIds: ["b6d8931d-d094-4cfd-b066-3b4926fcdf66", "791956da-f56d-490d-95f0-c2dd79dbd925", "c644062f-9cfc-4f37-ab8d-0748924cad28", "3290df62-2d96-4af0-bb54-2bcc61a07364", "379bbafe-00eb-4c48-9689-f6c3e3e8eb59"] },
  { champ: "Zed", champId: 238, skinIds: ["5e7e9cb3-1779-45ea-926b-8bed90e811e6", "4503854d-d56e-42bb-bd39-ccd492261de7", "f354ceff-7a13-4d4f-b76a-314dd9457223", "d8775b81-0994-4882-b1fc-517bc29c7f86", "2071a302-5986-40d6-a6b8-e385121d1607"] },
  { champ: "Jinx", champId: 222, skinIds: ["8d6d6c08-340b-45e3-8cd5-e45609b85365", "72f1df0b-7bac-400e-b320-f9c3f4b04199", "76af7b33-a5fa-4700-91d0-f86c60ac32b1"] },
  { champ: "Katarina", champId: 55, skinIds: ["589ce9c0-7d1c-43b7-a381-e3c7fa069368", "ebf2d318-6c1f-44fd-828b-f94912697df5", "6591a98e-cf56-426f-82b0-fa10afb2114f", "801fb568-4c41-4fa9-9f0a-847e13b9c9dc", "32ab1af1-5c4f-4be0-9850-8726a7aa8abf"] },
];

export default {
  async scheduled(event, env, ctx) {
    ctx.waitUntil((async () => {
      const s = await crawlSpurt(env);
      if (s.idle || s.assembled != null) {
        // The upstream source confirmed our load is negligible, so mirror aggressively:
        // BOTH files and thumbnails every tick, in parallel. This is a burst
        // that self-idles once the whole catalog is in R2 (it only fetches
        // what's missing), so the high rate costs nothing once caught up.
        await Promise.all([trickleMirror(env, 60), warmImages(env, 200)]);
      }
    })());
  },

  async fetch(req, env, ctx) {
    if (req.method === "OPTIONS") return cors(new Response(null, { status: 204 }));
    const url = new URL(req.url);
    const origin = url.origin;
    const path = url.pathname.replace(/\/+$/, "") || "/";

    if (path === "/debug") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const u = `${RF}/api/mods?page=0&pageSize=${PAGE_SIZE}&sortBy=recently_updated`;
      try {
        const r = await fetch(u, { headers: { "User-Agent": UA } });
        const body = await r.text();
        let fp = null, fperr = null;
        try { fp = (await fetchPage(0)); } catch (e) { fperr = String(e); }
        return json({ url: u, status: r.status, ct: r.headers.get("content-type"), bodyLen: body.length, bodyHead: body.slice(0, 200), fetchPageLen: fp && fp.mods ? fp.mods.length : null, fetchPageUpstreamError: fp ? fp.error || null : null, fetchPageErr: fperr });
      } catch (e) {
        return json({ error: String(e) });
      }
    }

    if (path.startsWith("/mirror/")) {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const modId = decodeURIComponent(path.slice("/mirror/".length));
      const asset = await resolveDownload(env, modId);
      if (!asset) return json({ step: "resolve", error: "no asset" });
      let fr;
      try { fr = await fetch(asset, { headers: { "User-Agent": UA } }); } catch (e) { return json({ step: "fetch", error: String(e), asset }); }
      if (!fr.ok) return json({ step: "fetch", status: fr.status, asset });
      const buf = await fr.arrayBuffer();
      if (!env.FILES) return json({ step: "r2", error: "no FILES binding", bytes: buf.byteLength });
      try { await env.FILES.put(`f/${modId}.fantome`, buf, { httpMetadata: { contentType: "application/zip" } }); } catch (e) { return json({ step: "put", error: String(e), bytes: buf.byteLength }); }
      const head = await env.FILES.head(`f/${modId}.fantome`);
      return json({ ok: true, bytes: buf.byteLength, r2size: head ? head.size : null });
    }

    if (path === "/crawl") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      if (url.searchParams.get("full") === "1") return json({ full: await crawlFull(env) });
      return json(await crawlSpurt(env));
    }

    if (path === "/catalog") {
      const all = await getCatalog(env);
      const ready = await getMirrored(env);
      const q = (url.searchParams.get("search") || "").toLowerCase().trim();
      const champ = url.searchParams.get("champion");
      const cat = url.searchParams.get("category");
      const readyOnly = url.searchParams.get("ready") === "1";
      const page = Math.max(0, parseInt(url.searchParams.get("page") || "0", 10) || 0);
      const size = Math.min(60, Math.max(1, parseInt(url.searchParams.get("pageSize") || "48", 10) || 48));
      let items = all;
      if (q) items = items.filter((m) => m.name.toLowerCase().includes(q) || m.champions.some((c) => (c.name || "").toLowerCase().includes(q)) || (m.publisher || "").toLowerCase().includes(q));
      if (champ) items = items.filter((m) => m.champions.some((c) => String(c.id) === champ || (c.name || "").toLowerCase() === champ.toLowerCase()));
      if (cat) items = items.filter((m) => m.category === cat);
      if (readyOnly) items = items.filter((m) => ready.has(m.id));
      const total = items.length;
      const mods = items.slice(page * size, page * size + size).map((m) => ({
        ...m,
        video: m.video || ytId(m.description),
        thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null,
        ready: ready.has(m.id) || !!m.ready, // in R2 (or a curated Original) → installs instantly
      }));
      return json({ total, page, pageSize: size, readyCount: ready.size, mods });
    }

    // Full catalog in one shot — the app filters/sorts/counts it client-side.
    if (path === "/all") {
      const all = await getCatalog(env);
      const ready = await getMirrored(env);
      const mods = all.map((m) => ({ ...m, video: m.video || ytId(m.description), thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null, ready: ready.has(m.id) || !!m.ready }));
      return json({ total: mods.length, readyCount: ready.size, mods });
    }

    // Curated champion bundles, enriched with live catalog details. `ready`
    // is checked against R2 DIRECTLY (source of truth) — the KV mirrored-set
    // loses updates under rapid read-modify-write, and it's only 25 files.
    if (path === "/bundles") {
      const all = await getCatalog(env);
      const byId = new Map(all.map((m) => [m.id, m]));
      const bundles = await Promise.all(BUNDLES.map(async (b) => ({
        champ: b.champ,
        champId: b.champId,
        skins: (await Promise.all(b.skinIds.map(async (id) => {
          const m = byId.get(id);
          if (!m) return null;
          const ready = env.FILES ? !!(await env.FILES.head(`f/${id}.fantome`)) : false;
          return {
            id,
            name: m.name,
            author: m.author,
            installs: m.installs || 0,
            thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null,
            ready,
          };
        }))).filter(Boolean),
      })));
      return json({ bundles });
    }

    // Pre-mirror every bundle skin so the packs install instantly (guarded).
    if (path === "/warmbundles") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const ids = BUNDLES.flatMap((b) => b.skinIds);
      let inR2 = 0, failed = 0;
      for (const id of ids) {
        try {
          // mirrorFile no-ops if already in R2; run it for all so the ready-set
          // stays marked too. Confirm against R2 head afterward (truth).
          await mirrorFile(env, id);
          if (env.FILES && (await env.FILES.head(`f/${id}.fantome`))) inR2++; else failed++;
        } catch (e) { failed++; }
      }
      return json({ total: ids.length, inR2, failed });
    }

    if (path.startsWith("/img/")) {
      const key = decodeURIComponent(path.slice("/img/".length));
      if (!key) return cors(new Response("no key", { status: 400 }));
      return serveImage(req, env, ctx, key);
    }

    if (path.startsWith("/download/")) {
      // Return our /file URL; the client downloads from us (R2).
      const modId = decodeURIComponent(path.slice("/download/".length));
      if (!modId) return json({ error: "no mod id" }, 400);
      let mirrored = false;
      if (env.FILES) { const head = await env.FILES.head(`f/${modId}.fantome`); mirrored = !!head; }
      const our = `${origin}/file/${modId}`;
      if (url.searchParams.get("redirect") === "1") return cors(Response.redirect(our, 302));
      return json({ url: our, mirrored });
    }

    if (path.startsWith("/file/")) {
      const modId = decodeURIComponent(path.slice("/file/".length)).replace(/\.fantome$/, "");
      const fkey = `f/${modId}.fantome`;
      const attach = { "Content-Type": "application/zip", "Content-Disposition": `attachment; filename="${modId}.fantome"`, "Cache-Control": "public, max-age=2592000" };
      // Serve from our R2 if we have it.
      if (env.FILES) {
        const obj = await env.FILES.get(fkey);
        if (obj) return cors(new Response(obj.body, { headers: attach }));
      }
      // Not in R2 yet — fetch, serve, and persist (+ mark ready). Tee the
      // stream so we serve the client AND write to R2 without buffering.
      const asset = await resolveDownload(env, modId);
      if (!asset) return cors(new Response("not found", { status: 404 }));
      let fr;
      try { fr = await fetch(asset, { headers: { "User-Agent": UA } }); } catch (e) { return cors(new Response("source unavailable", { status: 502 })); }
      if (!fr.ok || !fr.body) return cors(new Response("source unavailable", { status: 502 }));
      if (env.FILES) {
        const [toClient, toR2] = fr.body.tee();
        ctx.waitUntil((async () => {
          try { await env.FILES.put(fkey, toR2, { httpMetadata: { contentType: "application/zip" } }); await addMirrored(env, modId); } catch (e) {}
        })());
        return cors(new Response(toClient, { headers: attach }));
      }
      return cors(new Response(fr.body, { headers: attach }));
    }

    if (path === "/trickle") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const n = Math.min(300, Math.max(1, parseInt(url.searchParams.get("n") || "60", 10) || 60));
      return json(await trickleMirror(env, n));
    }

    if (path === "/warm") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const n = Math.min(500, Math.max(1, parseInt(url.searchParams.get("n") || "200", 10) || 200));
      return json(await warmImages(env, n));
    }

    if (path === "/meta") {
      const meta = await env.CATALOG.get("meta:v1");
      let progress = null;
      try { progress = JSON.parse((await env.CATALOG.get("crawl:state")) || "null"); } catch (e) {}
      const base = meta ? JSON.parse(meta) : { count: 0 };
      const ready = await getMirrored(env);
      let mirrorHealth = null;
      try { mirrorHealth = JSON.parse((await env.CATALOG.get("mirror:health")) || "null"); } catch (e) {}
      return json({ ...base, ready: ready.size, crawlProgress: progress, mirrorHealth });
    }

    return json({ service: "chud-skins catalog", endpoints: ["/catalog", "/img/{key}", "/download/{modId}", "/meta"] });
  },
};
