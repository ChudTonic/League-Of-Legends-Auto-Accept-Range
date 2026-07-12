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

async function fetchPage(page) {
  // Omit categories/champions[] params (the API 400s on empty-array strings).
  const u = `${RF}/api/mods?page=${page}&pageSize=${PAGE_SIZE}&sortBy=recently_updated`;
  const r = await fetch(u, { headers: { "User-Agent": UA } });
  if (!r.ok) return null;
  const d = await r.json();
  return (d && d.mods) || [];
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
    const mods = await fetchPage(page);
    if (mods === null) { reachedEnd = true; state.lastPage = page - 1; break; }
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
  return { assembled: all.length, day };
}

// Full one-shot crawl (initial seed only — triggered manually, not by users).
async function crawlFull(env) {
  const all = [];
  for (let page = 0; page < 80; page++) {
    const mods = await fetchPage(page);
    if (mods === null) break;
    all.push(...mods.map(normalize));
    if (mods.length < PAGE_SIZE) break;
  }
  if (all.length) {
    await env.CATALOG.put("catalog:v1", JSON.stringify(all));
    await env.CATALOG.put("meta:v1", JSON.stringify({ count: all.length, crawledAt: new Date().toISOString() }));
    await env.CATALOG.put("crawl:state", JSON.stringify({ day: today(), nextPage: 0, done: true, lastPage: null }));
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
  const data = raw ? JSON.parse(raw) : [];
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

// Gentle trickle: mirror up to N not-yet-mirrored files. Runs from the cron once
// the day's catalog crawl is done, so R2 fills in over time.
async function trickleMirror(env, n) {
  if (!env.FILES) return { mirrored: 0 };
  const all = await getCatalog(env);
  const done = await getMirrored(env);
  // Prioritize the curated bundle skins so the featured packs become fully
  // installable first, then fill in the rest of the catalog.
  const bundleIds = new Set(BUNDLES.flatMap((b) => b.skinIds));
  const order = [...all].sort((a, b) => (bundleIds.has(b.id) ? 1 : 0) - (bundleIds.has(a.id) ? 1 : 0));
  let count = 0;
  for (const m of order) {
    if (count >= n) break;
    if (done.has(m.id)) continue;
    if (env.FILES && (await env.FILES.head(`f/${m.id}.fantome`))) { await addMirrored(env, m.id); continue; }
    const bytes = await mirrorFile(env, m.id);
    if (bytes != null) count++;
  }
  return { mirrored: count, total: all.length, ready: done.size + count };
}

// Pre-warm thumbnails into R2 so the card grid loads instantly (images are
// small; the first browse otherwise pays a slow source fetch per tile).
async function warmImages(env, n) {
  if (!env.IMAGES) return { warmed: 0 };
  const all = await getCatalog(env);
  let count = 0;
  for (const m of all) {
    if (count >= n) break;
    if (!m.thumbKey) continue;
    if (await env.IMAGES.head(m.thumbKey)) continue;
    try {
      const r = await fetch(`${IMG}/${m.thumbKey}`, { headers: { "User-Agent": UA } });
      if (r.ok && r.body) { await env.IMAGES.put(m.thumbKey, r.body, { httpMetadata: { contentType: r.headers.get("content-type") || "image/png" } }); count++; }
    } catch (e) {}
  }
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
  const m = text.match(/https:\/\/[A-Za-z0-9._%/?=+-]*mod_release_artifacts[A-Za-z0-9._%/?=+-]*\.fantome[A-Za-z0-9._%/?=+-]*/);
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
  { champ: "Yasuo", champId: 157, skinIds: ["b1355371-eb88-460d-a89a-beb4dae1400a", "fb42adb4-165c-4bbc-aa05-18e876bddd74", "08292f3d-5882-43ed-996c-32c9147f6f63", "83a4379d-fc9a-4133-89d3-8835b787f548", "193a6ae3-af69-451d-85b1-8b45bc3ca856"] },
  { champ: "Ahri", champId: 103, skinIds: ["b6d8931d-d094-4cfd-b066-3b4926fcdf66", "d55eb66d-b26d-4158-9f59-8c804da04b9b", "18c364c6-6ad4-4ceb-9085-f503a1738de2", "ede331de-6956-4a26-a7ac-0474e6041523", "bafe8ce1-a6c6-4c38-8c49-de2a4d4d6b77"] },
  { champ: "Zed", champId: 238, skinIds: ["5e7e9cb3-1779-45ea-926b-8bed90e811e6", "ebb87563-4f92-46eb-bcfa-2ae1eac29716", "8d141157-5984-4089-a52f-887646ee8e2e", "4503854d-d56e-42bb-bd39-ccd492261de7", "f354ceff-7a13-4d4f-b76a-314dd9457223"] },
  { champ: "Jinx", champId: 222, skinIds: ["8d6d6c08-340b-45e3-8cd5-e45609b85365", "72f1df0b-7bac-400e-b320-f9c3f4b04199", "33d36a7f-b0b9-4a7a-aa02-20de40962ddd", "8dd4787c-f17e-4c2b-8010-995dbf5d79d4", "60a8b44d-6a21-4390-bdf2-080519636078"] },
  { champ: "Katarina", champId: 55, skinIds: ["589ce9c0-7d1c-43b7-a381-e3c7fa069368", "9f957e1d-a96c-4251-b736-a23d859134da", "2f6c6e8d-423b-4dc1-9e1a-42253be8ecf2", "ebf2d318-6c1f-44fd-828b-f94912697df5", "9841715c-cd94-41f2-bc57-55d5bbfb1972"] },
];

export default {
  async scheduled(event, env, ctx) {
    ctx.waitUntil((async () => {
      const s = await crawlSpurt(env);
      if (s.idle || s.assembled != null) {
        // Alternate so neither starves: even minute = mirror files (unlocks
        // install), odd = warm thumbnails (browse visuals).
        if (new Date().getUTCMinutes() % 2 === 0) await trickleMirror(env, 6);
        else await warmImages(env, 30);
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
        return json({ url: u, status: r.status, ct: r.headers.get("content-type"), bodyLen: body.length, bodyHead: body.slice(0, 200), fetchPageLen: fp ? fp.length : fp, fetchPageErr: fperr });
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
        ready: ready.has(m.id), // file already in R2 → installs instantly from us
      }));
      return json({ total, page, pageSize: size, readyCount: ready.size, mods });
    }

    // Full catalog in one shot — the app filters/sorts/counts it client-side.
    if (path === "/all") {
      const all = await getCatalog(env);
      const ready = await getMirrored(env);
      const mods = all.map((m) => ({ ...m, video: m.video || ytId(m.description), thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null, ready: ready.has(m.id) }));
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
      const n = Math.min(20, Math.max(1, parseInt(url.searchParams.get("n") || "3", 10) || 3));
      return json(await trickleMirror(env, n));
    }

    if (path === "/warm") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const n = Math.min(120, Math.max(1, parseInt(url.searchParams.get("n") || "50", 10) || 50));
      return json(await warmImages(env, n));
    }

    if (path === "/meta") {
      const meta = await env.CATALOG.get("meta:v1");
      let progress = null;
      try { progress = JSON.parse((await env.CATALOG.get("crawl:state")) || "null"); } catch (e) {}
      const base = meta ? JSON.parse(meta) : { count: 0 };
      const ready = await getMirrored(env);
      return json({ ...base, ready: ready.size, crawlProgress: progress });
    }

    return json({ service: "chud-skins catalog", endpoints: ["/catalog", "/img/{key}", "/download/{modId}", "/meta"] });
  },
};
