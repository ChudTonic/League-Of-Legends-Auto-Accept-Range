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

function normalize(m) {
  return {
    id: m.id,
    name: m.name,
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
    description: (m.description || "").slice(0, 700),
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
  if (!fr.ok) return null;
  const buf = await fr.arrayBuffer();
  await env.FILES.put(`f/${modId}.fantome`, buf, { httpMetadata: { contentType: "application/zip" } });
  await addMirrored(env, modId);
  return buf.byteLength;
}

// Gentle trickle: mirror up to N not-yet-mirrored files. Runs from the cron once
// the day's catalog crawl is done, so R2 fills in over time.
async function trickleMirror(env, n) {
  if (!env.FILES) return { mirrored: 0 };
  const all = await getCatalog(env);
  const done = await getMirrored(env);
  let count = 0;
  for (const m of all) {
    if (count >= n) break;
    if (done.has(m.id)) continue;
    const bytes = await mirrorFile(env, m.id);
    if (bytes != null) count++;
  }
  return { mirrored: count, total: all.length, ready: done.size + count };
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

export default {
  async scheduled(event, env, ctx) {
    ctx.waitUntil((async () => {
      const s = await crawlSpurt(env);
      // Once the day's catalog is assembled, spend the cron budget trickling a
      // couple of files into R2 so it fills in gradually.
      if (s.idle || s.assembled != null) await trickleMirror(env, 2);
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
        thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null,
        ready: ready.has(m.id), // file already in R2 → installs instantly from us
      }));
      return json({ total, page, pageSize: size, readyCount: ready.size, mods });
    }

    // Full catalog in one shot — the app filters/sorts/counts it client-side.
    if (path === "/all") {
      const all = await getCatalog(env);
      const ready = await getMirrored(env);
      const mods = all.map((m) => ({ ...m, thumb: m.thumbKey ? `${origin}/img/${m.thumbKey}` : null, ready: ready.has(m.id) }));
      return json({ total: mods.length, readyCount: ready.size, mods });
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
      // Not in R2 yet — fetch, serve, and persist (+ mark ready).
      const asset = await resolveDownload(env, modId);
      if (!asset) return cors(new Response("not found", { status: 404 }));
      let fr;
      try { fr = await fetch(asset, { headers: { "User-Agent": UA } }); } catch (e) { return cors(new Response("source unavailable", { status: 502 })); }
      if (!fr.ok) return cors(new Response("source unavailable", { status: 502 }));
      const buf = await fr.arrayBuffer();
      if (env.FILES) ctx.waitUntil((async () => {
        await env.FILES.put(fkey, buf.slice(0), { httpMetadata: { contentType: "application/zip" } });
        await addMirrored(env, modId);
      })());
      return cors(new Response(buf, { headers: attach }));
    }

    if (path === "/trickle") {
      if (!env.CRAWL_KEY || url.searchParams.get("key") !== env.CRAWL_KEY) return json({ error: "forbidden" }, 403);
      const n = Math.min(20, Math.max(1, parseInt(url.searchParams.get("n") || "3", 10) || 3));
      return json(await trickleMirror(env, n));
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
