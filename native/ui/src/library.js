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
    bundles: null, bundleInstalling: {},
    // modId -> post-download phase ("converting"), set by the backend's
    // library-install-phase event; swaps the progress caption while the pack
    // is being rewritten (e.g. announcer packs retargeted for all modes).
    phase: {},
  };
  let root = null;

  // Backend phase events for in-flight installs (e.g. announcer packs get a
  // "Converting for all modes…" pass right after the download finishes).
  const EV = window.__TAURI__ && window.__TAURI__.event;
  if (EV && EV.listen) {
    EV.listen("library-install-phase", (e) => {
      const p = (e && e.payload) || {};
      if (!p.modId || st.installing[p.modId] == null) return;
      st.phase[p.modId] = p.phase || "converting";
      setInstallPhaseUI(p.modId);
    });
  }

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
      description: m.description || "", video: m.video || null, thumb: m.thumb || null, ready: !!m.ready,
    };
  }

  // ── browser-preview mocks (same pattern as MOCK_STATE / MOCK_PROFILE) ──
  // Raw worker shape (pre-adapt). Thumbs borrow official tiles from
  // CommunityDragon purely as stand-in art for the no-backend preview.
  const MOCK_ALIAS = { 103: "ahri", 157: "yasuo", 222: "jinx", 99: "lux", 238: "zed", 84: "akali", 147: "seraphine", 67: "vayne", 412: "thresh", 81: "ezreal", 875: "sett", 55: "katarina" };
  const TILE = (c, n) => { const a = MOCK_ALIAS[c]; return `https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/assets/characters/${a}/skins/skin${String(n).padStart(2, "0")}/images/${a}_splash_tile_${n}.jpg`; };
  const MOCK_MODS = [
    { id: "mock-starfall-ahri", name: "Starfall Ahri", author: "Mochi", champions: [{ id: 103, name: "Ahri" }], category: "champion_skin", themes: ["anime"], views: 48200, installs: 12400, likes: 3100, updatedAt: "2026-07-10T12:00:00Z", trending: true, working: true, ready: true, thumb: TILE(103, 1), description: "Celestial recolor with new trail VFX." },
    { id: "mock-cyber-yasuo", name: "Cyber Yasuo 2077", author: "NightOwl", champions: [{ id: 157, name: "Yasuo" }], category: "champion_skin", themes: ["scifi"], views: 39100, installs: 9800, likes: 2400, updatedAt: "2026-07-08T12:00:00Z", trending: true, working: true, ready: true, thumb: TILE(157, 2), description: "Neon-city blade with holo wind wall." },
    { id: "mock-bubblegum-jinx", name: "Bubblegum Jinx", author: "candyfloss", champions: [{ id: 222, name: "Jinx" }], category: "champion_skin", themes: ["meme"], views: 30500, installs: 8100, likes: 2050, updatedAt: "2026-07-11T12:00:00Z", trending: true, working: true, ready: true, thumb: TILE(222, 1), description: "Pastel rockets. Pow-Pow squeaks." },
    { id: "mock-sailor-lux", name: "Sailor Lux", author: "MoonPrism", champions: [{ id: 99, name: "Lux" }], category: "champion_skin", themes: ["anime"], views: 27800, installs: 7300, likes: 1900, updatedAt: "2026-07-05T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(99, 2), description: "Magical-girl ult beam and wand." },
    { id: "mock-void-zed", name: "Void Reaver Zed", author: "Umbra", champions: [{ id: 238, name: "Zed" }], category: "champion_skin", themes: ["fantasy"], views: 22100, installs: 6100, likes: 1400, updatedAt: "2026-06-28T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(238, 1), description: "Void-touched shadows and shurikens." },
    { id: "mock-oni-akali", name: "Oni Akali", author: "KaijuWorks", champions: [{ id: 84, name: "Akali" }], category: "champion_skin", themes: ["fantasy"], views: 19600, installs: 5400, likes: 1250, updatedAt: "2026-07-02T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(84, 1), description: "Demon mask, ember smoke bomb." },
    { id: "mock-kpop-seraphine", name: "Encore Seraphine", author: "stagelight", champions: [{ id: 147, name: "Seraphine" }], category: "champion_skin", themes: ["anime", "events"], views: 17400, installs: 4800, likes: 1150, updatedAt: "2026-07-09T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(147, 1), description: "Concert-stage platform and mic VFX." },
    { id: "mock-dragonfire-vayne", name: "Dragonfire Vayne", author: "emberfall", champions: [{ id: 67, name: "Vayne" }], category: "champion_skin", themes: ["fantasy"], views: 15900, installs: 4300, likes: 980, updatedAt: "2026-06-24T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(67, 1), description: "Flaming bolts, ember tumble." },
    { id: "mock-crimson-thresh", name: "Crimson Moon Thresh", author: "lantern", champions: [{ id: 412, name: "Thresh" }], category: "champion_skin", themes: ["fantasy", "events"], views: 14200, installs: 3900, likes: 900, updatedAt: "2026-06-30T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(412, 1), description: "Blood-moon lantern and hooks." },
    { id: "mock-retro-ezreal", name: "Retro Arcade Ezreal", author: "pixelpush", champions: [{ id: 81, name: "Ezreal" }], category: "champion_skin", themes: ["meme", "events"], views: 12800, installs: 3400, likes: 820, updatedAt: "2026-07-07T12:00:00Z", trending: false, working: true, ready: true, thumb: TILE(81, 1), description: "8-bit ult with coin pickups." },
    { id: "mock-shiba-teemo", name: "Shiba Teemo", author: "doge", champions: [{ id: 17, name: "Teemo" }], category: "champion_skin", themes: ["meme"], views: 11300, installs: 3100, likes: 760, updatedAt: "2026-07-03T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Much shroom. Very blind." },
    { id: "mock-gothic-morgana", name: "Gothic Morgana", author: "Nyx", champions: [{ id: 25, name: "Morgana" }], category: "champion_skin", themes: ["fantasy"], views: 9800, installs: 2600, likes: 610, updatedAt: "2026-06-21T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Lace, ravens, and a darker pool." },
    { id: "mock-mecha-sett", name: "Mecha Sett Prime", author: "ironclad", champions: [{ id: 875, name: "Sett" }], category: "champion_skin", themes: ["scifi"], views: 8900, installs: 0, likes: 540, updatedAt: "2026-07-12T12:00:00Z", trending: false, working: true, ready: false, thumb: TILE(875, 1), description: "Piston punches. Still mirroring." },
    { id: "mock-chroma-kat", name: "Chroma Crash Katarina", author: "bladeworks", champions: [{ id: 55, name: "Katarina" }], category: "champion_skin", themes: ["scifi"], views: 8100, installs: 2200, likes: 430, updatedAt: "2026-05-30T12:00:00Z", trending: false, working: false, ready: true, thumb: TILE(55, 1), description: "RGB daggers — broke on latest patch." },
    { id: "mock-winter-rift", name: "Winter Wonder Rift", author: "Frostbyte", champions: [], category: "map_skin", themes: ["events"], views: 26400, installs: 7900, likes: 2100, updatedAt: "2026-07-01T12:00:00Z", trending: true, working: true, ready: true, thumb: null, description: "Snow-covered Summoner's Rift, aurora skybox." },
    { id: "mock-abyss-aram", name: "Abyss Remastered (ARAM)", author: "Frostbyte", champions: [], category: "map_skin", themes: ["fantasy"], views: 13700, installs: 3600, likes: 880, updatedAt: "2026-06-18T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Deep-freeze Howling Abyss retexture." },
    { id: "mock-clean-hud", name: "Minimal HUD — Clean UI", author: "pixelpush", champions: [], category: "ui", themes: [], views: 21900, installs: 6800, likes: 1700, updatedAt: "2026-07-06T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Slim frames, bigger minimap, no clutter." },
    { id: "mock-anime-announcer", name: "Anime Announcer (JP)", author: "sakura_vx", champions: [], category: "announcer", themes: ["anime"], views: 18200, installs: 5100, likes: 1300, updatedAt: "2026-06-26T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Full JP voice pack for kills and objectives." },
    { id: "mock-glados-announcer", name: "GLaDOS Announcer", author: "aperture_fan", champions: [], category: "announcer", themes: ["meme", "scifi"], views: 16600, installs: 4600, likes: 1200, updatedAt: "2026-06-15T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Passive-aggressive science commentary." },
    { id: "mock-pixel-font", name: "Pixel Font Pack", author: "8bitforge", champions: [], category: "font", themes: ["meme"], views: 7400, installs: 2000, likes: 410, updatedAt: "2026-06-10T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Damage numbers in chunky 8-bit." },
    { id: "mock-neon-vfx", name: "Neon Ability Recolors", author: "glowstick", champions: [], category: "vfx", themes: ["scifi"], views: 10800, installs: 2900, likes: 690, updatedAt: "2026-07-04T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Synthwave palette for common spell VFX." },
    { id: "mock-ghibli-loading", name: "Painted Loading Screens", author: "totoro_", champions: [], category: "loading_screen", themes: ["anime"], views: 9200, installs: 2500, likes: 640, updatedAt: "2026-06-12T12:00:00Z", trending: false, working: true, ready: true, thumb: null, description: "Hand-painted loading art set." },
  ];
  const MOCK_BUNDLES = [
    { champ: "Ahri", champId: 103, skins: [{ id: "mock-starfall-ahri", name: "Starfall Ahri", thumb: TILE(103, 1), ready: true }, { id: "mock-b-ahri-2", name: "Midnight Spirit", thumb: TILE(103, 2), ready: true }, { id: "mock-b-ahri-3", name: "Nine Lives", thumb: TILE(103, 3), ready: true }, { id: "mock-b-ahri-4", name: "Foxfire Redux", thumb: TILE(103, 4), ready: true }] },
    { champ: "Yasuo", champId: 157, skins: [{ id: "mock-cyber-yasuo", name: "Cyber Yasuo 2077", thumb: TILE(157, 2), ready: true }, { id: "mock-b-yasuo-2", name: "Ronin Wanderer", thumb: TILE(157, 1), ready: true }, { id: "mock-b-yasuo-3", name: "Stormblade", thumb: TILE(157, 3), ready: true }, { id: "mock-b-yasuo-4", name: "Last Breath", thumb: TILE(157, 9), ready: true }] },
    { champ: "Jinx", champId: 222, skins: [{ id: "mock-bubblegum-jinx", name: "Bubblegum Jinx", thumb: TILE(222, 1), ready: true }, { id: "mock-b-jinx-2", name: "Powder Keg", thumb: TILE(222, 2), ready: true }, { id: "mock-b-jinx-3", name: "Get Jinxed", thumb: TILE(222, 3), ready: true }, { id: "mock-b-jinx-4", name: "Zap Happy", thumb: TILE(222, 4), ready: true }] },
    { champ: "Lux", champId: 99, skins: [{ id: "mock-sailor-lux", name: "Sailor Lux", thumb: TILE(99, 2), ready: true }, { id: "mock-b-lux-2", name: "Prism Guard", thumb: TILE(99, 1), ready: true }, { id: "mock-b-lux-3", name: "Final Spark", thumb: TILE(99, 3), ready: true }, { id: "mock-b-lux-4", name: "Lady of Light", thumb: TILE(99, 4), ready: true }] },
  ];

  async function load() {
    try {
      const [cat, state] = await Promise.all([inv("library_catalog_all"), inv("library_state")]);
      st.catalog = ((cat && cat.mods) || (S.hasBackend ? [] : MOCK_MODS)).map(adapt);
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
    if (pct != null) return `<span class="lb-qa lb-qpct" data-pctid="${esc(m.id)}">${Math.round(pct)}%</span>`;
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
      <div class="lb-sec-l">CHAMPIONS</div><div class="lb-rail-list ${st.railAll ? "lb-rail-scroll" : ""}">${champRows}</div>${showAll}
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

  // ── bundles ──
  function bundlesHtml() {
    if (st.bundles === null) { fetchBundles(); return `<div class="lb-loading">${"<div class='lb-skel'></div>".repeat(3)}</div>`; }
    if (!st.bundles.length) return `<div class="lb-empty"><div>No bundles available right now.</div></div>`;
    const cards = st.bundles.map((b) => {
      const nInst = b.skins.filter((s) => st.installed[s.id]).length;
      const nReady = b.skins.filter((s) => s.ready).length;
      const pct = st.bundleInstalling[b.champ];
      const collage = b.skins.slice(0, 4).map((s) =>
        `<div class="lb-bcell" style="${thumbStyle({ thumb: s.thumb, champId: b.champId, category: "Champion skins" })}">${s.thumb ? "" : thumbInner({ champId: b.champId })}</div>`).join("");
      let action;
      if (pct != null) action = `<div class="lb-mprog"><div class="lb-mprog-bar" style="width:${Math.round(pct)}%"></div></div><div class="lb-mprog-cap">Installing pack… ${Math.round(pct)}%</div>`;
      else if (nInst >= b.skins.length && b.skins.length) action = `<span class="chip lb-chip-ok"><span class="lb-dot on"></span>INSTALLED · ${b.skins.length} skins</span>`;
      else action = `<button class="btn primary lb-binstall" data-bundle="${esc(b.champ)}">↓ Install pack · ${b.skins.length} skins</button>`;
      const sub = nInst ? `${b.skins.length} top skins · ${nInst}/${b.skins.length} installed` : `${b.skins.length} top skins · ${nReady} ready now`;
      return `<div class="lb-bundle">
        <div class="lb-bcollage">${collage}<span class="lb-bcount">${b.skins.length}</span></div>
        <div class="lb-bbody">
          <div class="lb-btitle">${b.champId ? `<img class="lb-ci" src="${CI(b.champId)}" alt="" onerror="this.style.display='none'">` : ""}<span>${esc(b.champ)}</span></div>
          <div class="lb-bsub">${sub}</div>
          <div class="lb-bnames">${b.skins.map((s) => esc(s.name)).join(" · ")}</div>
          <div class="lb-baction">${action}</div>
        </div>
      </div>`;
    }).join("");
    return `<div class="lb-main">
      <div class="lb-btop"><div class="lb-btop-t">Champion packs</div><div class="lb-btop-s">One click installs the top custom skins for a champ — then pick between them on the in-client Custom Mods button in champ select.</div></div>
      <div class="lb-bgrid">${cards}</div>
    </div>`;
  }

  async function fetchBundles() {
    if (st._bundlesLoading) return; st._bundlesLoading = true;
    try { const r = await inv("library_bundles"); st.bundles = (r && r.bundles) || (S.hasBackend ? [] : MOCK_BUNDLES); }
    catch (e) { console.error("bundles load failed", e); st.bundles = []; }
    st._bundlesLoading = false; paint();
  }

  async function installBundle(champ) {
    const b = (st.bundles || []).find((x) => x.champ === champ);
    if (!b || st.bundleInstalling[champ] != null) return;
    st.bundleInstalling[champ] = 4; paint();
    const iv = setInterval(() => { const c = st.bundleInstalling[champ]; if (c == null) return clearInterval(iv); st.bundleInstalling[champ] = Math.min(93, c + 2 + Math.random() * 4); const bar = document.querySelector(".lb-bundle .lb-mprog-bar"); if (bar) bar.style.width = Math.round(st.bundleInstalling[champ]) + "%"; }, 220);
    try {
      const r = await inv("library_install_bundle", { champ: b.champ, champId: b.champId, skins: b.skins.map((s) => ({ id: s.id, name: s.name })) });
      clearInterval(iv); delete st.bundleInstalling[champ];
      const done = (r && r.installedRecords) || [];
      done.forEach((id) => { const s = b.skins.find((x) => x.id === id); st.installed[id] = { name: s ? s.name : id, champ: b.champ, version: "1.0.0" }; });
      const nFail = (r && r.failed && r.failed.length) || 0;
      if (nFail) toast(`${b.champ} pack — ${r.installed} of ${b.skins.length} installed`, `${nFail} skin${nFail === 1 ? "" : "s"} still mirroring — try again shortly for the rest.`, "warning");
      else toast(`${b.champ} pack installed`, `${r.installed} skins ready — pick them on the Custom Mods button in champ select.`, "success");
    } catch (e) {
      clearInterval(iv); delete st.bundleInstalling[champ];
      toast("Pack install failed", String(e).slice(0, 120), "danger");
    }
    paint();
  }

  function pageHtml() {
    const nInst = Object.keys(st.installed).length, nFav = st.favs.length;
    const tabs = [["browse", "Browse"], ["bundles", "★ Bundles"], ["installed", `Installed · ${nInst}`], ["favs", `Favorites · ${nFav}`]]
      .map(([k, l]) => `<button class="lb-seg-b ${st.tab === k ? "on" : ""}" data-tab="${k}">${l}</button>`).join("");
    let body;
    if (st.catalog === null) body = `<div class="lb-loading">${"<div class='lb-skel'></div>".repeat(6)}</div>`;
    else if (st.tab === "bundles") body = bundlesHtml();
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
    if (installing) action = st.phase[m.id]
      ? `<div class="lb-mprog"><div class="lb-mprog-bar" style="width:100%"></div></div><div class="lb-mprog-cap">Converting for all modes…</div>`
      : `<div class="lb-mprog"><div class="lb-mprog-bar" style="width:${Math.round(pct)}%"></div></div><div class="lb-mprog-cap">Downloading… ${Math.round(pct)}%</div>`;
    else if (inst) action = `<div class="lb-minstalled"><span class="chip lb-chip-ok"><span class="lb-dot on"></span>INSTALLED v${esc(inst.version || "1.0.0")}</span><span class="lb-inchamp">Ready in champ select ✓</span><button class="lb-trash" data-remove="${esc(m.id)}"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M3 6h18M8 6V4h8v2m-9 0 1 14h8l1-14"/></svg></button></div>`;
    else if (!m.ready) action = `<button class="btn primary lb-minstall" disabled style="opacity:.5;cursor:default">Preparing this mod — check back soon</button>`;
    else action = `<button class="btn primary lb-minstall" data-install="${esc(m.id)}">↓ Install to Chud · v${esc(m.version)}</button>`;
    return `<div class="lb-backdrop" data-close="1"><div class="lb-modal" role="dialog">
      <div class="lb-mtop"></div>
      <div class="lb-mhead"><span class="lb-mtab">Overview</span><button class="lb-mx" data-close="1">✕</button></div>
      <div class="lb-mbody">
        <div class="lb-mleft"><div class="lb-mpreview" style="${thumbStyle(m)}">${thumbInner(m)}${m.video ? `<button class="lb-vplay" data-video="${esc(m.video)}" title="Watch showcase"><svg viewBox="0 0 24 24" width="26" height="26" fill="currentColor"><path d="M8 5v14l11-7z"/></svg><span>Watch showcase</span></button>` : ""}</div>
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
    on("[data-video]", "onclick", (e) => {
      e.stopPropagation();
      const vid = e.currentTarget.dataset.video;
      const prev = e.currentTarget.closest(".lb-mpreview");
      if (prev && vid) prev.innerHTML = `<iframe class="lb-vframe" src="https://www.youtube-nocookie.com/embed/${encodeURIComponent(vid)}?autoplay=1&rel=0&modestbranding=1" title="Skin showcase" allow="autoplay; encrypted-media; fullscreen" allowfullscreen></iframe>`;
    });
    on("[data-fav]", "onclick", async (e) => { e.stopPropagation(); const id = e.currentTarget.dataset.fav; const on2 = !st.favs.includes(id); try { const favs = await inv("library_set_favorite", { modId: id, on: on2 }); st.favs = favs || (S.hasBackend ? st.favs : (on2 ? [...st.favs, id] : st.favs.filter((x) => x !== id))); } catch (er) {} paint(); });
    on("[data-install]", "onclick", (e) => { e.stopPropagation(); install(e.currentTarget.dataset.install); });
    on("[data-bundle]", "onclick", (e) => { e.stopPropagation(); installBundle(e.currentTarget.dataset.bundle); });
    on("[data-remove]", "onclick", async (e) => { e.stopPropagation(); const id = e.currentTarget.dataset.remove; try { const r = await inv("library_remove", { modId: id }); if (r && r.installed) st.installed = r.installed; else if (!S.hasBackend) { const n = { ...st.installed }; delete n[id]; st.installed = n; } } catch (er) {} const m = (st.catalog || []).find((x) => x.id === id); toast("Mod removed", `${(m && m.name) || "Mod"} deleted from your mods folder.`, "danger"); paint(); });
  }

  // Update just the progress-bar/percent elements in place so a download tick
  // doesn't re-paint (and reload the splash image) — that caused visible modal
  // flicker. paint() is only called on state transitions (start/finish).
  function setInstallProgressUI(id, pct) {
    if (st.phase[id]) return; // conversion phase owns the caption now
    const p = Math.round(pct);
    const bar = document.querySelector(".lb-mprog-bar");
    if (bar) bar.style.width = p + "%";
    const cap = document.querySelector(".lb-mprog-cap");
    if (cap) cap.textContent = `Downloading… ${p}%`;
    const chip = root && root.querySelector(`.lb-qpct[data-pctid="${id}"]`);
    if (chip) chip.textContent = p + "%";
  }

  // Download finished, backend is rewriting the pack (announcer retarget):
  // pin the bar full and swap the caption until the install call resolves.
  function setInstallPhaseUI(id) {
    const bar = document.querySelector(".lb-mprog-bar");
    if (bar) bar.style.width = "100%";
    const cap = document.querySelector(".lb-mprog-cap");
    if (cap) cap.textContent = "Converting for all modes…";
    const chip = root && root.querySelector(`.lb-qpct[data-pctid="${id}"]`);
    if (chip) chip.textContent = "CNV";
  }

  async function install(id) {
    if (st.installing[id] != null || st.installed[id]) return;
    const m = (st.catalog || []).find((x) => x.id === id) || { name: id, champ: "" };
    if (!m.ready) { toast("Not ready yet", "This mod is still being prepared — try again shortly.", "warning"); return; }
    // Indeterminate visual progress while the real download runs. Render the
    // progress UI once (paint), then tick the bar in place (no re-paint).
    st.installing[id] = 5; paint();
    const iv = setInterval(() => { const c = st.installing[id]; if (c == null) return clearInterval(iv); st.installing[id] = Math.min(94, c + 3 + Math.random() * 6); setInstallProgressUI(id, st.installing[id]); }, 180);
    try {
      const rec = await inv("library_install", { modId: id, name: m.name || id, champ: m.champ || "", champId: m.champId || null, category: m.rawCategory || "" });
      clearInterval(iv); delete st.installing[id]; delete st.phase[id];
      st.installed[id] = rec || { name: m.name, version: "1.0.0" };
      toast("Mod installed", `${m.name || "Mod"} — pick it from the Custom Mods button in champ select.`, "success");
    } catch (e) {
      clearInterval(iv); delete st.installing[id]; delete st.phase[id];
      toast("Install failed", String(e).slice(0, 120), "danger");
    }
    paint();
  }

  window.renderLibrary = async function (el) {
    root = el;
    if (st.catalog === null) { paint(); await load(); }
    paint();
  };

  // Let the Dashboard's featured-pack cards deep-link into the Bundles tab.
  window.ChudOpenBundles = function () {
    st.tab = "bundles";
    if (window.ChudNavTo) window.ChudNavTo("library");
  };
  // Shared fetch so the Dashboard can show the same pack data without duplicating it.
  window.ChudGetBundles = async function () {
    try { const r = await S.invoke("library_bundles"); return (r && r.bundles) || (S.hasBackend ? [] : MOCK_BUNDLES); }
    catch (e) { return []; }
  };
})();
