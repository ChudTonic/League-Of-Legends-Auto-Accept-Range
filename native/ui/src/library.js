// Chud — Library (skin/mod marketplace). Self-contained; main.js routes the
// "library" page here via window.renderLibrary(). Catalog comes from the app's
// own backend (Cloudflare Worker + R2); no upstream source is named in the UI.
(function () {
  "use strict";
  const S = window.ChudShared;
  const esc = S.esc;
  const inv = S.invoke;

  const CI = (id) => `https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/v1/champion-icons/${id}.png`;
  const CAT_DISPLAY = { champion_skin: "Champion skins", map_skin: "Maps", ui: "HUD & UI", vfx: "VFX", announcer: "Announcer", voiceover: "Voiceover", sfx: "Sound FX", font: "Fonts", loading_screen: "Loading screens", miscellaneous: "Other" };
  const THEME_DISPLAY = { anime: "Anime", meme: "Meme", fantasy: "Fantasy", scifi: "Sci-Fi", events: "Events" };
  const cap = (s) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);
  const fmtN = (n) => (n >= 1000 ? Math.round(n / 100) / 10 + "k" : String(n || 0));
  const fmtAgo = (h) => (h == null ? "" : h < 1 ? "just now" : h < 24 ? h + "h ago" : Math.round(h / 24) + "d ago");
  const hue = (str) => { let x = 0; for (let i = 0; i < str.length; i++) x = (x * 31 + str.charCodeAt(i)) % 360; return x; };
  const catShort = (c) => (c === "Champion skins" ? "SKIN" : c === "HUD & UI" ? "HUD" : c === "Announcer" ? "VOICE" : (c || "MOD").toUpperCase());
  function hoursSince(iso) { if (!iso) return null; const t = Date.parse(iso); if (isNaN(t)) return null; return Math.max(0, Math.round((Date.now() - t) / 3600000)); }

  const st = {
    catalog: null, tab: "browse", q: "", champ: "", cat: "", themes: [],
    workingOnly: true, sort: "trending", railAll: false,
    selId: null, installed: {}, favs: [], installing: {}, autoUpdate: true,
    cats: [], themesList: [],
  };
  let root = null;

  function adapt(m) {
    const champ = m.champions && m.champions[0];
    return {
      id: m.id, name: m.name || "Untitled", author: m.author || "unknown",
      champId: champ ? champ.id : null, champ: champ ? champ.name : null,
      category: CAT_DISPLAY[m.category] || cap(m.category) || "Other",
      rawCategory: m.category || "",
      themes: (m.themes || []).map((t) => THEME_DISPLAY[t] || cap(t)),
      views: m.views || 0, installs: m.installs || 0, likes: m.likes || 0,
      updatedHrs: hoursSince(m.updatedAt), trending: !!m.trending, working: m.working !== false,
      version: "1.0.0", sizeMB: null, modifies: "Base",
      description: m.description || "", hasVideo: false, thumb: m.thumb || null, ready: !!m.ready,
    };
  }

  async function load() {
    try {
      const [cat, state] = await Promise.all([inv("library_catalog_all"), inv("library_state")]);
      st.catalog = ((cat && cat.mods) || []).map(adapt);
      if (state) { st.installed = state.installed || {}; st.favs = state.favs || []; st.autoUpdate = state.autoUpdate !== false; }
    } catch (e) { console.error("library load failed", e); st.catalog = []; }
    const cs = new Set(), ts = new Set();
    st.catalog.forEach((m) => { cs.add(m.category); m.themes.forEach((t) => ts.add(t)); });
    st.cats = [...cs]; st.themesList = [...ts].sort();
  }

  // ── toasts (reuse the app's #toasts container) ──
  function toast(title, msg, tone) {
    const wrap = document.getElementById("toasts"); if (!wrap) return;
    const el = document.createElement("div");
    el.className = `toast ${tone === "success" ? "success" : tone === "danger" ? "danger" : tone === "warning" ? "warning" : ""}`;
    el.innerHTML = `<div class="toast-bar"></div><div><div class="toast-title">${esc(title)}</div>${msg ? `<div class="toast-msg">${esc(msg)}</div>` : ""}</div>`;
    wrap.appendChild(el);
    setTimeout(() => { el.classList.add("out"); setTimeout(() => el.remove(), 300); }, 3400);
  }

  // ── filtering / sorting ──
  function filtered() {
    const ql = st.q.trim().toLowerCase();
    let list = (st.catalog || [])
      .filter((m) => (st.workingOnly ? m.working : true))
      .filter((m) => (st.champ ? m.champ === st.champ : true))
      .filter((m) => (st.cat ? m.category === st.cat : true))
      .filter((m) => (st.themes.length === 0 || st.themes.some((t) => m.themes.includes(t))))
      .filter((m) => (ql ? `${m.name} ${m.author} ${m.champ || ""} ${m.category}`.toLowerCase().includes(ql) : true));
    if (st.sort === "recent") list = list.slice().sort((a, b) => (a.updatedHrs ?? 1e9) - (b.updatedHrs ?? 1e9));
    else if (st.sort === "installs") list = list.slice().sort((a, b) => b.installs - a.installs);
    else list = list.slice().sort((a, b) => (b.trending - a.trending) || (b.installs - a.installs));
    return list;
  }
  const filtersActive = () => !!(st.q.trim() || st.champ || st.cat || st.themes.length);

  // ── card ──
  function thumbStyle(m) {
    // Single-quotes inside url() — the style="" attribute is double-quoted, so
    // url("…") would terminate the attribute and drop the background entirely.
    if (m.thumb) return `background:url('${esc(m.thumb)}') center/cover no-repeat`;
    const h = hue(m.id);
    return `background:linear-gradient(135deg,hsl(${h} 42% 14%),hsl(${(h + 45) % 360} 52% 25%))`;
  }
  function thumbInner(m) {
    if (m.thumb) return "";
    if (m.champId) return `<img class="lb-ph-icon" loading="lazy" src="${CI(m.champId)}" alt="" onerror="this.style.display='none'">`;
    return `<span class="lb-ph-cat">${esc(catShort(m.category))}</span>`;
  }
  const DL_ICON = `<svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12m0 0 4-4m-4 4-4-4M4 21h16"/></svg>`;
  function installBtnState(m) {
    const pct = st.installing[m.id];
    if (pct != null) return `<span class="lb-qa lb-qpct">${Math.round(pct)}%</span>`;
    if (st.installed[m.id]) return `<span class="lb-qa lb-qcheck" title="Installed"><svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>`;
    if (!m.ready) return `<span class="lb-qa lb-qdl lb-disabled" title="Preparing this mod — check back soon">${DL_ICON}</span>`;
    return `<button class="lb-qa lb-qdl" data-install="${esc(m.id)}" title="Install">${DL_ICON}</button>`;
  }
  function cardHtml(m) {
    const isFav = st.favs.includes(m.id);
    const meta = `by <b>${esc(m.author)}</b> · ${esc(m.champ || m.category)}${m.themes[0] ? " · " + esc(m.themes[0]) : ""}`;
    return `<div class="lb-card" data-open="${esc(m.id)}">
      <div class="lb-thumb" style="${thumbStyle(m)}">
        ${thumbInner(m)}
        ${m.trending ? `<span class="lb-badge lb-trend">TRENDING</span>` : ""}
        ${!m.working ? `<span class="lb-badge lb-broken">BROKEN</span>` : ""}
        <div class="lb-actions">
          <button class="lb-qa lb-fav ${isFav ? "on" : ""}" data-fav="${esc(m.id)}" title="Favorite"><svg viewBox="0 0 24 24" width="13" height="13" fill="${isFav ? "currentColor" : "none"}" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20.3S3.5 15.4 2.6 9.9C2 6.6 4.6 4 7.5 4c1.9 0 3.4 1 4.5 2.6C13.1 5 14.6 4 16.5 4c2.9 0 5.5 2.6 4.9 5.9-.9 5.5-9.4 10.4-9.4 10.4z"/></svg></button>
          ${installBtnState(m)}
        </div>
      </div>
      <div class="lb-body">
        <div class="lb-name" title="${esc(m.name)}">${esc(m.name)}</div>
        <div class="lb-meta">${meta}</div>
        <div class="lb-stats"><span>${fmtN(m.views)} views</span><span>↓ ${fmtN(m.installs)}</span><span class="lb-ago">${esc(fmtAgo(m.updatedHrs))}</span></div>
      </div>
    </div>`;
  }

  // ── rail ──
  function railHtml() {
    const list = st.catalog || [];
    const champCounts = {};
    list.forEach((m) => { if (m.champ) champCounts[m.champ] = (champCounts[m.champ] || 0) + 1; });
    const champsAll = Object.keys(champCounts).map((name) => ({ name, count: champCounts[name], champId: (list.find((m) => m.champ === name) || {}).champId })).sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
    const vis = st.railAll ? champsAll : champsAll.slice(0, 6);
    const champRows = `<div class="lb-rail-row ${st.champ ? "" : "on"}" data-champ=""><span class="lb-ci lb-ci-all"></span><span class="lb-rn">All champions</span></div>` +
      vis.map((c) => `<div class="lb-rail-row ${st.champ === c.name ? "on" : ""}" data-champ="${esc(c.name)}"><img class="lb-ci" loading="lazy" src="${CI(c.champId)}" alt="" onerror="this.style.visibility='hidden'"><span class="lb-rn">${esc(c.name)}</span><span class="lb-rc">${c.count}</span></div>`).join("");
    const showAll = champsAll.length > 6 ? `<div class="lb-showall" data-railall="1">${st.railAll ? "Show less ▴" : `Show all ${champsAll.length} champions ▾`}</div>` : "";

    const catCounts = {};
    list.forEach((m) => { catCounts[m.category] = (catCounts[m.category] || 0) + 1; });
    const catList = [["", "All categories", list.length]].concat(st.cats.slice().sort((a, b) => (catCounts[b] || 0) - (catCounts[a] || 0)).map((c) => [c, c, catCounts[c] || 0]));
    const catRows = catList.map(([key, name, count]) => `<div class="lb-rail-row lb-cat ${st.cat === key ? "on" : ""}" data-cat="${esc(key)}"><span class="lb-dot"></span><span class="lb-rn">${esc(name)}</span><span class="lb-rc">${count}</span></div>`).join("");

    const themeChips = st.themesList.map((t) => `<span class="lb-chip ${st.themes.includes(t) ? "on" : ""}" data-theme="${esc(t)}">${esc(t)}</span>`).join("");

    return `<aside class="lb-rail glass">
      <div class="lb-search"><svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/></svg><input id="lbSearch" type="text" placeholder="Search mods…" value="${esc(st.q)}"><kbd>/</kbd></div>
      <div class="lb-sec-l">CHAMPIONS</div><div class="lb-rail-list">${champRows}${showAll}</div>
      <div class="lb-div"></div>
      <div class="lb-sec-l">CATEGORY</div><div class="lb-rail-list">${catRows}</div>
      <div class="lb-div"></div>
      <div class="lb-sec-l">THEMES</div><div class="lb-chips">${themeChips}</div>
      <div class="lb-div"></div>
      <div class="lb-working"><div><div class="lb-wl">Working only</div><div class="lb-wh">Hide mods flagged broken</div></div><div class="tog ${st.workingOnly ? "on" : ""}" data-working="1"><div class="knob"></div></div></div>
      ${filtersActive() ? `<div class="lb-reset" data-reset="1">Reset all filters</div>` : ""}
    </aside>`;
  }

  // ── main column ──
  function browseHtml() {
    const list = filtered();
    const parts = [`${list.length} result${list.length === 1 ? "" : "s"}`];
    if (st.champ) parts.push(st.champ);
    if (st.cat) parts.push(st.cat);
    if (st.themes.length) parts.push(st.themes.join(", "));
    if (st.q.trim()) parts.push(`"${st.q.trim()}"`);
    const sortSeg = ["trending", "recent", "installs"].map((k) => `<button class="lb-seg-b ${st.sort === k ? "on" : ""}" data-sort="${k}">${k === "trending" ? "Trending" : k === "recent" ? "Recent" : "Most installed"}</button>`).join("");
    const showShelf = !filtersActive() && st.sort === "trending";
    let shelf = "";
    if (showShelf) {
      const top = (st.catalog || []).filter((m) => m.trending && m.working).sort((a, b) => b.installs - a.installs).slice(0, 3);
      if (top.length) shelf = `<div class="lb-sec-l lb-shelf-l">TRENDING NOW</div><div class="lb-shelf">${top.map((m, i) => `<div class="lb-scard" data-open="${esc(m.id)}"><div class="lb-sthumb" style="${thumbStyle(m)}">${thumbInner(m)}</div><div class="lb-sbody"><div class="lb-skick">0${i + 1} · TRENDING</div><div class="lb-name" title="${esc(m.name)}">${esc(m.name)}</div><div class="lb-meta">by <b>${esc(m.author)}</b></div><div class="lb-stats"><span>↓ ${fmtN(m.installs)}</span></div></div></div>`).join("")}</div><div class="lb-sec-l">ALL MODS</div>`;
    }
    const grid = list.length
      ? `<div class="lb-grid">${list.slice(0, 240).map(cardHtml).join("")}</div>`
      : `<div class="lb-empty"><svg viewBox="0 0 24 24" width="30" height="30" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/></svg><div>No mods match your filters</div><button class="btn sm" data-reset="1">Reset all filters</button></div>`;
    return `<div class="lb-main">
      <div class="lb-toolbar"><div class="lb-results">${esc(parts.join(" · "))}</div><div class="lb-seg lb-seg-sm">${sortSeg}</div></div>
      ${shelf}${grid}
    </div>`;
  }

  function installedHtml() {
    const ids = Object.keys(st.installed);
    const byId = {}; (st.catalog || []).forEach((m) => (byId[m.id] = m));
    const totalMb = ids.reduce((a, id) => a + (st.installed[id].size_mb || 0), 0);
    if (!ids.length) return `<div class="lb-empty"><svg viewBox="0 0 24 24" width="30" height="30" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M12 3v12m0 0 4-4m-4 4-4-4M4 21h16"/></svg><div>Nothing installed yet</div><button class="btn sm primary" data-tab="browse">Browse mods</button></div>`;
    const rows = ids.map((id) => {
      const rec = st.installed[id], m = byId[id] || {};
      return `<div class="lb-irow"><div class="lb-ithumb" style="${thumbStyle(m.id ? m : { id, thumb: null, champId: null, category: "Other" })}" data-open="${esc(id)}">${thumbInner(m.id ? m : { id, thumb: null, champId: null, category: "Other" })}</div>
        <div><div class="lb-name" data-open="${esc(id)}">${esc(rec.name || id)}</div><div class="lb-meta">by <b>${esc(m.author || "unknown")}</b>${rec.champ ? " · " + esc(rec.champ) : ""} · ${(rec.size_mb || 0).toFixed(1)} MB</div></div>
        <div class="lb-ver">v${esc(rec.version || "1.0.0")}</div>
        <div><span class="chip lb-chip-ok"><span class="lb-dot on"></span>WORKING</span></div>
        <div class="lb-iactions"><span class="lb-inchamp" title="Open the Custom Mods button in champ select when this champion is up">In champ select ✓</span><button class="lb-trash" data-remove="${esc(id)}" title="Remove"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M3 6h18M8 6V4h8v2m-9 0 1 14h8l1-14"/></svg></button></div>
      </div>`;
    }).join("");
    return `<div class="lb-main">
      <div class="lb-toolbar"><div class="lb-results">${ids.length} mod${ids.length === 1 ? "" : "s"} · ${totalMb.toFixed(0)} MB</div></div>
      <div class="glass lb-ilist">${rows}
        <div class="lb-autoup"><div><div class="lb-wl">Auto-update</div><div class="lb-wh">Check for new versions when Chud launches</div></div><div class="tog ${st.autoUpdate ? "on" : ""}" data-autoup="1"><div class="knob"></div></div></div>
      </div></div>`;
  }

  function favsHtml() {
    const byId = {}; (st.catalog || []).forEach((m) => (byId[m.id] = m));
    const favMods = st.favs.map((id) => byId[id]).filter(Boolean);
    if (!favMods.length) return `<div class="lb-empty"><svg viewBox="0 0 24 24" width="30" height="30" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M12 20.3S3.5 15.4 2.6 9.9C2 6.6 4.6 4 7.5 4c1.9 0 3.4 1 4.5 2.6C13.1 5 14.6 4 16.5 4c2.9 0 5.5 2.6 4.9 5.9-.9 5.5-9.4 10.4-9.4 10.4z"/></svg><div>No favorites yet — tap the heart on any mod</div><button class="btn sm" data-tab="browse">Browse mods</button></div>`;
    return `<div class="lb-main"><div class="lb-grid lb-grid-4">${favMods.map(cardHtml).join("")}</div></div>`;
  }

  function pageHtml() {
    const nInst = Object.keys(st.installed).length, nFav = st.favs.length;
    const tabs = [["browse", "Browse"], ["installed", `Installed · ${nInst}`], ["favs", `Favorites · ${nFav}`]]
      .map(([k, l]) => `<button class="lb-seg-b ${st.tab === k ? "on" : ""}" data-tab="${k}">${l}</button>`).join("");
    let body;
    if (st.catalog === null) body = `<div class="lb-loading">${"<div class='lb-skel'></div>".repeat(6)}</div>`;
    else if (st.tab === "installed") body = installedHtml();
    else if (st.tab === "favs") body = favsHtml();
    else body = `<div class="lb-browse">${railHtml()}${browseHtml()}</div>`;
    return `<div class="lb-wrap">
      <div class="lb-head"><span class="section-label">SKIN LIBRARY</span><span class="lb-rule"></span><div class="lb-seg">${tabs}</div></div>
      <div class="fade-in">${body}</div>
    </div>`;
  }

  // ── detail modal ──
  function modalHtml(m) {
    const inst = st.installed[m.id]; const pct = st.installing[m.id]; const installing = pct != null;
    const isFav = st.favs.includes(m.id);
    let action;
    if (installing) action = `<div class="lb-mprog"><div class="lb-mprog-bar" style="width:${Math.round(pct)}%"></div></div><div class="lb-mprog-cap">Downloading… ${Math.round(pct)}%</div>`;
    else if (inst) action = `<div class="lb-minstalled"><span class="chip lb-chip-ok"><span class="lb-dot on"></span>INSTALLED v${esc(inst.version || "1.0.0")}</span><span class="lb-inchamp">Ready in champ select ✓</span><button class="lb-trash" data-remove="${esc(m.id)}"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M3 6h18M8 6V4h8v2m-9 0 1 14h8l1-14"/></svg></button></div>`;
    else if (!m.ready) action = `<button class="btn primary lb-minstall" disabled style="opacity:.5;cursor:default">Preparing this mod — check back soon</button>`;
    else action = `<button class="btn primary lb-minstall" data-install="${esc(m.id)}">↓ Install to Chud · v${esc(m.version)}</button>`;
    return `<div class="lb-backdrop" data-close="1"><div class="lb-modal" role="dialog">
      <div class="lb-mtop"></div>
      <div class="lb-mhead"><span class="lb-mtab">Overview</span><button class="lb-mx" data-close="1">✕</button></div>
      <div class="lb-mbody">
        <div class="lb-mleft"><div class="lb-mpreview" style="${thumbStyle(m)}">${thumbInner(m)}</div>
          <div class="lb-minfo">${m.champId ? `<img class="lb-ci" src="${CI(m.champId)}" alt="">` : ""}<span>Replaces <b>${esc(m.champ || m.category)}</b>. You (and synced party members) see it; opponents don't.</span></div>
        </div>
        <div class="lb-mright">
          <div class="lb-mtitle-row"><div class="lb-mtitle">${esc(m.name)}</div><button class="lb-fav ${isFav ? "on" : ""}" data-fav="${esc(m.id)}" title="Favorite"><svg viewBox="0 0 24 24" width="14" height="14" fill="${isFav ? "currentColor" : "none"}" stroke="currentColor" stroke-width="1.8"><path d="M12 20.3S3.5 15.4 2.6 9.9C2 6.6 4.6 4 7.5 4c1.9 0 3.4 1 4.5 2.6C13.1 5 14.6 4 16.5 4c2.9 0 5.5 2.6 4.9 5.9-.9 5.5-9.4 10.4-9.4 10.4z"/></svg></button></div>
          <div class="lb-mby">by <b>${esc(m.author)}</b>${m.updatedHrs != null ? " · updated " + esc(fmtAgo(m.updatedHrs)) : ""}</div>
          <div class="lb-mchips"><span class="chip ${m.working ? "lb-chip-ok" : "lb-chip-warn"}"><span class="lb-dot on"></span>${m.working ? "WORKING" : "BROKEN ON PATCH"}</span><span class="chip lb-chip-n">${esc(m.category)}</span></div>
          <div class="lb-mstats"><span>${fmtN(m.views)} views</span><span>↓ ${fmtN(m.installs)}</span><span>♥ ${fmtN(m.likes)}</span></div>
          <div class="lb-maction">${action}<div class="lb-mfoot">Installs straight to Chud. In champ select, click the <b>Custom Mods</b> button and pick it when this champion is up.</div></div>
        </div>
      </div>
    </div></div>`;
  }

  // ── render + events ──
  // The modal renders into document.body, NOT #page — #page is `.content-inner
  // .fade-in`, whose animated transform makes it a containing block for
  // position:fixed, which would clip the modal inside the page column.
  let modalRoot = null;
  function ensureModalRoot() {
    if (!modalRoot || !document.body.contains(modalRoot)) { modalRoot = document.createElement("div"); modalRoot.id = "lbModalRoot"; document.body.appendChild(modalRoot); }
    return modalRoot;
  }
  function paint() {
    if (!root) return;
    root.innerHTML = pageHtml();
    ensureModalRoot().innerHTML = st.selId ? modalHtml((st.catalog || []).find((m) => m.id === st.selId) || {}) : "";
    wire();
    const s = document.getElementById("lbSearch");
    if (s && st.tab === "browse" && !st.selId) { s.focus(); s.setSelectionRange(s.value.length, s.value.length); }
  }
  // patch just the browse main column (cheap re-render for filter/sort changes)
  function paintSoft() { paint(); }

  function wire() {
    const scopes = [root, modalRoot].filter(Boolean);
    const on = (sel, ev, fn) => scopes.forEach((sc) => sc.querySelectorAll(sel).forEach((el) => (el[ev] = fn)));
    on("[data-tab]", "onclick", (e) => { st.tab = e.currentTarget.dataset.tab; st.selId = null; paint(); });
    on("[data-sort]", "onclick", (e) => { st.sort = e.currentTarget.dataset.sort; paint(); });
    on("[data-champ]", "onclick", (e) => { const c = e.currentTarget.dataset.champ; st.champ = st.champ === c ? "" : c; paint(); });
    on("[data-cat]", "onclick", (e) => { const c = e.currentTarget.dataset.cat; st.cat = st.cat === c ? "" : c; paint(); });
    on("[data-theme]", "onclick", (e) => { const t = e.currentTarget.dataset.theme; st.themes = st.themes.includes(t) ? st.themes.filter((x) => x !== t) : [...st.themes, t]; paint(); });
    on("[data-working]", "onclick", () => { st.workingOnly = !st.workingOnly; paint(); });
    on("[data-railall]", "onclick", () => { st.railAll = !st.railAll; paint(); });
    on("[data-reset]", "onclick", () => { st.q = ""; st.champ = ""; st.cat = ""; st.themes = []; paint(); });
    on("[data-autoup]", "onclick", async () => { st.autoUpdate = !st.autoUpdate; try { await inv("library_set_auto_update", { on: st.autoUpdate }); } catch (e) {} paint(); });
    const search = document.getElementById("lbSearch");
    if (search) { let t = null; search.oninput = () => { clearTimeout(t); st.q = search.value; t = setTimeout(paintSoft, 160); }; }
    on("[data-open]", "onclick", (e) => { if (e.target.closest("[data-fav],[data-install],[data-remove],[data-apply]")) return; st.selId = e.currentTarget.dataset.open; paint(); });
    on("[data-close]", "onclick", (e) => { if (e.target === e.currentTarget || e.currentTarget.classList.contains("lb-mx")) { st.selId = null; paint(); } });
    on("[data-fav]", "onclick", async (e) => { e.stopPropagation(); const id = e.currentTarget.dataset.fav; const on2 = !st.favs.includes(id); try { const favs = await inv("library_set_favorite", { modId: id, on: on2 }); st.favs = favs || st.favs; } catch (er) {} paint(); });
    on("[data-install]", "onclick", (e) => { e.stopPropagation(); install(e.currentTarget.dataset.install); });
    on("[data-remove]", "onclick", async (e) => { e.stopPropagation(); const id = e.currentTarget.dataset.remove; try { const r = await inv("library_remove", { modId: id }); st.installed = (r && r.installed) || st.installed; } catch (er) {} const m = (st.catalog || []).find((x) => x.id === id); toast("Mod removed", `${(m && m.name) || "Mod"} deleted from your mods folder.`, "danger"); paint(); });
  }

  async function install(id) {
    if (st.installing[id] != null || st.installed[id]) return;
    const m = (st.catalog || []).find((x) => x.id === id) || { name: id, champ: "" };
    if (!m.ready) { toast("Not ready yet", "This mod is still being prepared — try again shortly.", "warning"); return; }
    // Indeterminate visual progress while the real download runs.
    st.installing[id] = 5; paint();
    const iv = setInterval(() => { const c = st.installing[id]; if (c == null) return clearInterval(iv); st.installing[id] = Math.min(94, c + 3 + Math.random() * 6); paint(); }, 180);
    try {
      const rec = await inv("library_install", { modId: id, name: m.name || id, champ: m.champ || "", champId: m.champId || null, category: m.rawCategory || "" });
      clearInterval(iv); delete st.installing[id];
      st.installed[id] = rec || { name: m.name, version: "1.0.0" };
      toast("Mod installed", `${m.name || "Mod"} — pick it from the Custom Mods button in champ select.`, "success");
    } catch (e) {
      clearInterval(iv); delete st.installing[id];
      toast("Install failed", String(e).slice(0, 120), "danger");
    }
    paint();
  }

  window.renderLibrary = async function (el) {
    root = el;
    if (st.catalog === null) { paint(); await load(); }
    paint();
  };
})();
