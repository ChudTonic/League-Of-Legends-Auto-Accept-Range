// Chud — Library (skin/mod marketplace). Self-contained; main.js routes the
// "library" page here via window.renderLibrary(). The catalog is a 1:1 index
// of RuneForge's mods (served through Chud's Cloudflare Worker); skin files
// download directly from RuneForge's R2, and each mod links back to its
// RuneForge page ("View on RuneForge") for attribution.
(function () {
  "use strict";
  const S = window.ChudShared;
  const esc = S.esc;
  const inv = S.invoke;
  // `import_mod` returns `Result<(), String>` — shared.js's `invoke` swallows
  // the error text (by design, see shared.js), so its failure path calls
  // `TAURI.invoke` directly to surface the real reason in the toast (same
  // pattern main.js uses for the party-mode commands).
  const TAURI = window.__TAURI__ && window.__TAURI__.core;

  const CI = (id) => `https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/v1/champion-icons/${Number(id) || 0}.png`;
  const CAT_DISPLAY = { champion_skin: "Champion skins", map_skin: "Maps", ui: "HUD & UI", vfx: "VFX", announcer: "Announcer", voiceover: "Voiceover", sfx: "Sound FX", font: "Fonts", loading_screen: "Loading screens", miscellaneous: "Other" };
  // Import Mod's Category dropdown — same backend category keys as CAT_DISPLAY,
  // ordered with champion_skin first (the common case) and its own singular
  // labels (distinct wording from the browse-filter labels above).
  const IMPORT_CATEGORIES = [
    ["champion_skin", "Champion Skin"],
    ["map_skin", "Map"],
    ["font", "Font"],
    ["announcer", "Announcer"],
    ["ui", "HUD / UI"],
    ["vfx", "VFX"],
    ["sfx", "SFX"],
    ["voiceover", "Voiceover"],
    ["loading_screen", "Loading Screen"],
    ["miscellaneous", "Other"],
  ];
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
    // Set when `library_install` comes back `{status:"blocked"}` — ModScan
    // flagged the download before it ever touched disk. `{id, name, scan}`;
    // rendered instead of the detail modal (see `paint()`) until the user
    // cancels or explicitly installs anyway.
    scanBlock: null,
    // Set when the user clicks "Pick skin" on an installed mod whose
    // download-time target detection came back empty. `{modId, champId, name}`;
    // rendered on top of everything else (see `paint()`) until closed or a skin
    // is chosen. `pickSkinsCache` is champId -> skins[] (from `skins_catalog`),
    // fetched once per champion and reused across opens.
    pickTarget: null,
    pickSkinsCache: {},
    // ── Import Mod (guided local install of a .fantome/.zip) ──
    // `importCatalog` is `skins_catalog`'s champions list (id/name/skins),
    // fetched once and cached — it doubles as both the Champion dropdown and
    // the Skin dropdown's source, so opening the modal a second time is instant.
    importOpen: false,
    importFile: null, // absolute path returned by `pick_mod_file`
    importCategory: "champion_skin", // one of IMPORT_CATEGORIES' keys
    importChampId: null,
    importSkinId: "auto", // "auto" | "base" | a skin_id string
    importName: "",
    importBusy: false,
    importCatalog: null,
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
      chudOriginal: !!m.chudOriginal, view: m.view || null,
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
    if (m.champId) return `<img class="lb-ph-icon" loading="lazy" src="${CI(m.champId)}" alt="" data-imgerr="hide">`;
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
        ${m.chudOriginal ? `<span class="lb-badge lb-original">CHUD ORIGINAL</span>` : m.trending ? `<span class="lb-badge lb-trend">TRENDING</span>` : ""}
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
      vis.map((c) => `<div class="lb-rail-row ${st.champ === c.name ? "on" : ""}" data-champ="${esc(c.name)}"><img class="lb-ci" loading="lazy" src="${CI(c.champId)}" alt="" data-imgerr="vis"><span class="lb-rn">${esc(c.name)}</span><span class="lb-rc">${c.count}</span></div>`).join("");
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
      // Only a champion_skin mod ever goes through download-time target
      // detection (see `place_library_mod`) — a null `target_skin_id` on
      // anything else (vfx/sfx/etc. filed per-champion) is normal, not a
      // pending pick, so this only fires with a confirmed catalog match.
      const needsPick = rec.target_skin_id == null && m.rawCategory === "champion_skin";
      const statusChip = needsPick
        ? `<span class="chip lb-chip-warn" title="Couldn't auto-detect which skin this mod targets"><span class="lb-dot"></span>NEEDS SKIN</span>`
        : `<span class="chip lb-chip-ok"><span class="lb-dot on"></span>WORKING</span>`;
      const actionCell = needsPick
        ? `<button class="btn sm" data-pick="${esc(id)}" data-champid="${esc(m.champId || "")}" data-champname="${esc(rec.name || id)}">Pick skin</button>`
        : `<span class="lb-inchamp" title="Open the Custom Mods button in champ select when this champion is up">In champ select ✓</span>`;
      return `<div class="lb-irow"><div class="lb-ithumb" style="${thumbStyle(m.id ? m : { id, thumb: null, champId: null, category: "Other" })}" data-open="${esc(id)}">${thumbInner(m.id ? m : { id, thumb: null, champId: null, category: "Other" })}</div>
        <div><div class="lb-name" data-open="${esc(id)}">${esc(rec.name || id)}</div><div class="lb-meta">by <b>${esc(m.author || "unknown")}</b>${rec.champ ? " · " + esc(rec.champ) : ""} · ${(rec.size_mb || 0).toFixed(1)} MB</div></div>
        <div class="lb-ver">v${esc(rec.version || "1.0.0")}</div>
        <div>${statusChip}</div>
        <div class="lb-iactions">${actionCell}<button class="lb-trash" data-remove="${esc(id)}" title="Remove"><svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M3 6h18M8 6V4h8v2m-9 0 1 14h8l1-14"/></svg></button></div>
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
          <div class="lb-btitle">${b.champId ? `<img class="lb-ci" src="${CI(b.champId)}" alt="" data-imgerr="hide">` : ""}<span>${esc(b.champ)}</span></div>
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
      const blocked = (r && r.blocked) || [];
      // Bundles don't get a force-all override (v1) — a blocked skin is
      // surfaced here so the user knows to review it individually, never
      // silently dropped.
      if (blocked.length) {
        const names = blocked.map((x) => x.name || x.id).join(", ");
        toast(`${b.champ} pack — ${r.installed} of ${b.skins.length} installed`, `${blocked.length} blocked by ModScan (${names}) — install individually to review and override.`, "danger");
      } else if (nFail) {
        toast(`${b.champ} pack — ${r.installed} of ${b.skins.length} installed`, `${nFail} skin${nFail === 1 ? "" : "s"} still mirroring — try again shortly for the rest.`, "warning");
      } else {
        toast(`${b.champ} pack installed`, `${r.installed} skins ready — pick them on the Custom Mods button in champ select.`, "success");
      }
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
      <div class="lb-head"><span class="section-label">SKIN LIBRARY</span><span class="lb-rule"></span><div class="lb-seg">${tabs}</div><button class="btn sm primary" data-import="1">+ Import Mod</button></div>
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
          ${m.description ? `<div class="lb-mdesc">${esc(m.description)}</div>` : ""}
          ${m.view ? `<a href="#" class="lb-rflink" data-view="${esc(m.view)}"><svg viewBox="0 0 24 24" width="13" height="13" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7 17 17 7M9 7h8v8"/></svg>View on RuneForge</a>` : ""}
          <div class="lb-maction">${action}<div class="lb-mfoot">Installs straight to Chud. In champ select, click the <b>Custom Mods</b> button and pick it when this champion is up. <span class="lb-scan-tag">🛡 Scanned by ModScan</span></div></div>
        </div>
      </div>
    </div></div>`;
  }

  // ── ModScan block modal ──
  // Rendered instead of the detail modal (see `paint()`) whenever
  // `library_install` comes back `{status:"blocked"}` — the file was never
  // written to disk. "Install anyway" is the only override, and it's only
  // ever reachable from here (a user-initiated install click); the bundle
  // path never force-installs (see `installBundle`).
  function scanBlockModalHtml(block) {
    const scan = block.scan || {};
    const verdict = scan.verdict || "suspicious";
    const isMalicious = verdict === "malicious";
    const findings = scan.findings || [];
    const vt = scan.vt && scan.vt.vt;
    const vtLine = vt
      ? `<div class="lb-scan-vt">VirusTotal: ${(vt.malicious || 0) + (vt.suspicious || 0)}/${vt.total || 0} engines flagged this</div>`
      : "";
    const findingRows = findings.length
      ? findings.map((f) => `<div class="lb-scan-finding lb-scan-sev-${esc(f.severity || "info")}">
          <span class="lb-scan-fdot"></span>
          <div><div class="lb-scan-fcode">${esc((f.code || "finding").toUpperCase())}${f.entry ? ` · <span class="lb-scan-fentry">${esc(f.entry)}</span>` : ""}</div>
          <div class="lb-scan-fdetail">${esc(f.detail || "")}</div></div>
        </div>`).join("")
      : `<div class="lb-scan-finding lb-scan-sev-info"><span class="lb-scan-fdot"></span><div class="lb-scan-fdetail">No individual findings — the file could not be verified as a normal archive.</div></div>`;
    return `<div class="lb-backdrop" data-close="1"><div class="lb-modal lb-scan-modal ${isMalicious ? "lb-scan-malicious" : "lb-scan-suspicious"}" role="dialog">
      <div class="lb-mtop"></div>
      <div class="lb-mhead"><span class="lb-mtab">ModScan</span><button class="lb-mx" data-close="1">✕</button></div>
      <div class="lb-scan-body">
        <div class="lb-scan-banner">
          <span class="lb-scan-icon">⚠</span>
          <div><div class="lb-scan-title">ModScan blocked this mod</div>
          <div class="lb-scan-sub">${esc(block.name || block.id)} was flagged <b>${esc(verdict.toUpperCase())}</b> and was NOT installed.</div></div>
        </div>
        <div class="lb-scan-findings">${findingRows}</div>
        ${vtLine}
        <div class="lb-scan-actions">
          <button class="btn" data-close="1">Cancel</button>
          <button class="btn danger" data-install-force="${esc(block.id)}">Install anyway — I accept the risk</button>
        </div>
        <div class="lb-scan-foot">Scanned by ModScan — Chud's built-in structural + reputation check for downloaded mods.</div>
      </div>
    </div></div>`;
  }

  // ── Pick-skin modal ──
  // Rendered instead of the detail/ModScan modals whenever the user clicks
  // "Pick skin" on an installed mod (see `installedHtml`) — download-time
  // target detection couldn't confidently resolve which skin the mod's WAD
  // chunks override, so the user picks it manually. Reuses the ModScan
  // modal's narrow layout and the rail's clickable-row styling rather than
  // introducing new CSS.
  function pickSkinModalHtml(pt) {
    const skins = st.pickSkinsCache[pt.champId];
    let body;
    if (skins === undefined) body = `<div class="lb-loading" style="grid-template-columns:1fr">${"<div class='lb-skel' style='height:34px'></div>".repeat(5)}</div>`;
    else if (!skins.length) body = `<div class="lb-empty">Couldn't load this champion's skins — make sure the League client is running and try again.</div>`;
    else {
      const rows = skins
        .filter((s) => s.skin_id % 1000 !== 0)
        .map((s) => `<div class="lb-rail-row" data-setskin="${s.skin_id}" style="cursor:pointer"><span class="lb-rn">${esc(s.name)}</span></div>`)
        .join("");
      body = `<div class="lb-rail-list">${rows}</div>`;
    }
    return `<div class="lb-backdrop" data-close="1"><div class="lb-modal lb-scan-modal" role="dialog">
      <div class="lb-mtop"></div>
      <div class="lb-mhead"><span class="lb-mtab">Pick target skin</span><button class="lb-mx" data-close="1">✕</button></div>
      <div class="lb-scan-body">
        <div class="lb-scan-sub">"${esc(pt.name)}" — pick the skin this mod is built on so it applies in champ select instead of staying on base.</div>
        ${body}
      </div>
    </div></div>`;
  }

  async function openPickSkin(modId, champId, name) {
    const cid = Number(champId) || 0;
    st.pickTarget = { modId, champId: cid, name };
    paint();
    if (st.pickSkinsCache[cid] !== undefined) return;
    try {
      const r = await inv("skins_catalog");
      const champs = (r && r.champions) || [];
      const c = champs.find((x) => x.champ_id === cid);
      st.pickSkinsCache[cid] = (c && c.skins) || [];
    } catch (e) {
      console.error("skins_catalog failed", e);
      st.pickSkinsCache[cid] = [];
    }
    if (st.pickTarget && st.pickTarget.champId === cid) paint();
  }

  async function setTargetSkin(skinId) {
    const pt = st.pickTarget;
    if (!pt) return;
    st.pickTarget = null;
    try {
      await inv("library_set_target_skin", { modId: pt.modId, skinId });
      if (st.installed[pt.modId]) st.installed[pt.modId] = { ...st.installed[pt.modId], target_skin_id: skinId };
      toast("Target skin set", `${pt.name} now applies to the right skin in champ select.`, "success");
    } catch (e) {
      toast("Couldn't set target skin", String(e).slice(0, 120), "danger");
    }
    paint();
  }

  // ── Import Mod ──
  // Replaces the old "drop it in mods\skins\{champId*1000}\ yourself" workflow:
  // pick a file, pick the champion + skin in a small modal (skin dropdown
  // auto-prefilled by an offline chunk-hash scan when possible), then
  // `import_mod` files it and registers it exactly like a Library install.

  // Best-effort filename -> display name: strip the extension and a trailing
  // version-looking suffix ("Cool Skin v1.2", "Cool Skin-1.0.3"). Good enough
  // for a prefill the user can still edit; never returns an empty string.
  function baseNameFromPath(p) {
    const base = String(p || "").split(/[\\/]/).pop() || "";
    const noExt = base.replace(/\.(fantome|zip)$/i, "");
    const stripped = noExt.replace(/[\s_.-]*v?\d+(?:\.\d+){0,3}$/i, "").trim();
    return stripped || noExt || base;
  }

  async function fetchImportCatalog() {
    if (st._importCatalogLoading) return;
    st._importCatalogLoading = true;
    try {
      const r = await inv("skins_catalog");
      const champs = (r && r.champions) || [];
      st.importCatalog = champs.slice().sort((a, b) => a.champ_name.localeCompare(b.champ_name));
    } catch (e) {
      console.error("skins_catalog failed", e);
      st.importCatalog = [];
    }
    st._importCatalogLoading = false;
    if (st.importOpen) paint();
  }

  async function openImportModal() {
    const path = await inv("pick_mod_file");
    if (!path) return;
    st.importOpen = true;
    st.importFile = path;
    st.importCategory = "champion_skin";
    st.importChampId = null;
    st.importSkinId = "auto";
    st.importName = baseNameFromPath(path);
    st.importBusy = false;
    paint();
    if (st.importCatalog === null) fetchImportCatalog();
  }

  function closeImportModal() {
    st.importOpen = false;
    st.importFile = null;
    st.importCategory = "champion_skin";
    st.importChampId = null;
    st.importSkinId = "auto";
    st.importName = "";
    st.importBusy = false;
  }

  // Category changed: champion/skin only apply to champion_skin, so switching
  // away from it clears them — a stale champ pick from a previous category
  // would otherwise silently ride along into a global-category import.
  function onImportCatChange(category) {
    st.importCategory = category || "champion_skin";
    if (st.importCategory !== "champion_skin") {
      st.importChampId = null;
      st.importSkinId = "auto";
    }
    paint();
  }

  // Champion changed: reset the skin choice to Auto, then re-run detection
  // for the new champion — a stale guess from the previous champion would be
  // actively wrong here, unlike leaving it on Auto.
  async function onImportChampChange(champId) {
    st.importChampId = champId || null;
    st.importSkinId = "auto";
    paint();
    if (!champId || !st.importFile) return;
    try {
      const detected = await inv("detect_mod_target", { filePath: st.importFile, championId: champId });
      // Also bail if the user picked a skin by hand while detection was still
      // in flight — a manual pick must win over a late auto-detect result.
      if (detected == null || st.importChampId !== champId || st.importSkinId !== "auto") return;
      const champ = (st.importCatalog || []).find((c) => c.champ_id === champId);
      const known = champ && (champ.skins || []).some((s) => s.skin_id === detected);
      if (known) { st.importSkinId = String(detected); paint(); }
    } catch (e) { /* best-effort — stays on Auto */ }
  }

  async function submitImport() {
    const category = st.importCategory || "champion_skin";
    const isChampSkin = category === "champion_skin";
    if (st.importBusy || (isChampSkin && !st.importChampId)) return;
    const champId = isChampSkin ? st.importChampId : null;
    const nameEl = document.getElementById("lbImportName");
    const name = ((nameEl && nameEl.value) || st.importName || "").trim() || "Imported mod";
    const skinSel = st.importSkinId;
    const skinId = !isChampSkin ? null : skinSel === "auto" ? null : skinSel === "base" ? champId * 1000 : Number(skinSel);
    st.importBusy = true; paint();
    try {
      if (TAURI) {
        await TAURI.invoke("import_mod", { filePath: st.importFile, category, championId: champId, skinId, name });
      } else {
        // Browser-preview (no Tauri backend): mock the installed record locally.
        const champ = isChampSkin ? (st.importCatalog || []).find((c) => c.champ_id === champId) : null;
        st.installed[`local-preview-${Date.now()}`] = { name, champ: champ ? champ.champ_name : "", version: "1.0.0", size_mb: 0, target_skin_id: skinId };
      }
      closeImportModal();
      toast("Mod imported", `${name} — pick it from the Custom Mods button in champ select.`, "success");
      if (TAURI) { try { const state = await inv("library_state"); if (state) { st.installed = state.installed || st.installed; st.favs = state.favs || st.favs; } } catch (e) {} }
      st.tab = "installed";
    } catch (e) {
      st.importBusy = false;
      toast("Import failed", String(e || "Couldn't import that mod."), "danger");
    }
    paint();
  }

  function importModalHtml() {
    const category = st.importCategory || "champion_skin";
    const isChampSkin = category === "champion_skin";
    const catOptions = IMPORT_CATEGORIES.map(([val, label]) => `<option value="${val}" ${category === val ? "selected" : ""}>${esc(label)}</option>`).join("");
    const champs = st.importCatalog;
    const champOptions = champs === null
      ? `<option value="">Loading champions…</option>`
      : `<option value="">Select a champion…</option>` + champs.map((c) => `<option value="${c.champ_id}" ${st.importChampId === c.champ_id ? "selected" : ""}>${esc(c.champ_name)}</option>`).join("");
    const champ = champs && st.importChampId ? champs.find((c) => c.champ_id === st.importChampId) : null;
    // Real skins only (base excluded) — "Auto" and "Base skin" cover the base
    // case explicitly, same split `pickSkinModalHtml` uses above.
    const realSkins = champ ? (champ.skins || []).filter((s) => s.skin_id % 1000 !== 0) : [];
    const skinOptions = `<option value="auto" ${st.importSkinId === "auto" ? "selected" : ""}>Auto — let Chud decide</option>` +
      `<option value="base" ${st.importSkinId === "base" ? "selected" : ""}>Base skin</option>` +
      realSkins.map((s) => `<option value="${s.skin_id}" ${st.importSkinId === String(s.skin_id) ? "selected" : ""}>${esc(s.name)}</option>`).join("");
    const fileName = (st.importFile || "").split(/[\\/]/).pop();
    // Champion + Skin fields only make sense for champion_skin — a font/map/
    // announcer import is just Category + Name.
    const champSkinFields = isChampSkin ? `
        <div class="lb-import-field">
          <label class="lb-import-label" for="lbImportChamp">Champion</label>
          <select class="lb-import-select" id="lbImportChamp" data-import-champ="1" ${champs === null ? "disabled" : ""}>${champOptions}</select>
        </div>
        <div class="lb-import-field">
          <label class="lb-import-label" for="lbImportSkin">Skin</label>
          <select class="lb-import-select" id="lbImportSkin" data-import-skin="1" ${champ ? "" : "disabled"}>${skinOptions}</select>
        </div>` : "";
    return `<div class="lb-backdrop" data-close="1"><div class="lb-modal lb-scan-modal" role="dialog">
      <div class="lb-mtop"></div>
      <div class="lb-mhead"><span class="lb-mtab">Import Mod</span><button class="lb-mx" data-close="1">✕</button></div>
      <div class="lb-scan-body">
        <div class="lb-scan-sub">${esc(fileName || "")}</div>
        <div class="lb-import-field">
          <label class="lb-import-label" for="lbImportCat">Category</label>
          <select class="lb-import-select" id="lbImportCat" data-import-cat="1">${catOptions}</select>
        </div>${champSkinFields}
        <div class="lb-import-field">
          <label class="lb-import-label" for="lbImportName">Name</label>
          <input class="lb-import-input" id="lbImportName" type="text" value="${esc(st.importName || "")}" maxlength="80" placeholder="Mod name">
        </div>
        <div class="lb-scan-actions">
          <button class="btn" data-close="1" ${st.importBusy ? "disabled" : ""}>Cancel</button>
          <button class="btn primary" data-import-submit="1" ${st.importBusy || (isChampSkin && !st.importChampId) ? "disabled" : ""}>${st.importBusy ? "Importing…" : "Import"}</button>
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
    // Only one modal is ever shown at a time: import > ModScan block > pick-skin > detail.
    ensureModalRoot().innerHTML = st.importOpen
      ? importModalHtml()
      : st.scanBlock
      ? scanBlockModalHtml(st.scanBlock)
      : st.pickTarget
      ? pickSkinModalHtml(st.pickTarget)
      : (st.selId ? modalHtml((st.catalog || []).find((m) => m.id === st.selId) || {}) : "");
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
    // Closing a modal only dismisses the top one — if the detail modal was
    // open underneath (e.g. "Pick skin" was reached from there), it
    // reappears rather than also getting dismissed.
    on("[data-close]", "onclick", (e) => { if (e.target === e.currentTarget || e.currentTarget.classList.contains("lb-mx")) { if (st.importOpen) closeImportModal(); else if (st.scanBlock) st.scanBlock = null; else if (st.pickTarget) st.pickTarget = null; else st.selId = null; paint(); } });
    on("[data-pick]", "onclick", (e) => { e.stopPropagation(); const el = e.currentTarget; openPickSkin(el.dataset.pick, el.dataset.champid, el.dataset.champname); });
    on("[data-setskin]", "onclick", (e) => { e.stopPropagation(); setTargetSkin(Number(e.currentTarget.dataset.setskin)); });
    on("[data-install-force]", "onclick", (e) => { e.stopPropagation(); const id = e.currentTarget.dataset.installForce; st.scanBlock = null; install(id, true); });
    on("[data-import]", "onclick", (e) => { e.stopPropagation(); openImportModal(); });
    on("[data-import-cat]", "onchange", (e) => { onImportCatChange(e.currentTarget.value); });
    on("[data-import-champ]", "onchange", (e) => { onImportChampChange(Number(e.currentTarget.value) || null); });
    on("[data-import-skin]", "onchange", (e) => { st.importSkinId = e.currentTarget.value; });
    on("[data-import-submit]", "onclick", (e) => { e.stopPropagation(); submitImport(); });
    // Live-bound (no paint() on keystroke) so typing isn't interrupted by the
    // full-innerHTML repaint a champion/skin change triggers elsewhere in the modal.
    const importName = document.getElementById("lbImportName");
    if (importName) importName.oninput = () => { st.importName = importName.value; };
    on("[data-video]", "onclick", (e) => {
      e.stopPropagation();
      const vid = e.currentTarget.dataset.video;
      const prev = e.currentTarget.closest(".lb-mpreview");
      if (prev && vid) prev.innerHTML = `<iframe class="lb-vframe" src="https://www.youtube-nocookie.com/embed/${encodeURIComponent(vid)}?autoplay=1&rel=0&modestbranding=1" title="Skin showcase" allow="autoplay; encrypted-media; fullscreen" allowfullscreen></iframe>`;
    });
    on("[data-view]", "onclick", (e) => { e.preventDefault(); e.stopPropagation(); const u = e.currentTarget.dataset.view; if (u) inv("open_external_url", { url: u }).catch(() => {}); });
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

  // `force` re-issues the install after the user has seen the ModScan block
  // modal and explicitly clicked "Install anyway" — it's never set on the
  // first attempt, so a bare install() call can never silently install
  // something flagged.
  async function install(id, force) {
    if (st.installing[id] != null || st.installed[id]) return;
    const m = (st.catalog || []).find((x) => x.id === id) || { name: id, champ: "" };
    if (!m.ready) { toast("Not ready yet", "This mod is still being prepared — try again shortly.", "warning"); return; }
    // Indeterminate visual progress while the real download runs. Render the
    // progress UI once (paint), then tick the bar in place (no re-paint).
    st.installing[id] = 5; paint();
    const iv = setInterval(() => { const c = st.installing[id]; if (c == null) return clearInterval(iv); st.installing[id] = Math.min(94, c + 3 + Math.random() * 6); setInstallProgressUI(id, st.installing[id]); }, 180);
    try {
      const r = await inv("library_install", { modId: id, name: m.name || id, champ: m.champ || "", champId: m.champId || null, category: m.rawCategory || "", force: !!force });
      clearInterval(iv); delete st.installing[id]; delete st.phase[id];
      if (r && r.status === "blocked") {
        // ModScan flagged it and (without force) nothing was written to disk —
        // show the warning modal instead of marking it installed.
        st.scanBlock = { id, name: m.name || id, scan: r.scan || {} };
        paint();
        return;
      }
      st.installed[id] = (r && r.record) || { name: m.name, version: "1.0.0" };
      // Positive scan confirmation: a clean scan is otherwise invisible, which
      // makes "scanned & clean" look identical to "never scanned". Surface the
      // verdict on every successful install.
      const scan = (r && r.scan) || {};
      const vt = scan.vt && scan.vt.known ? ` · VirusTotal ${((scan.vt.vt || {}).malicious) || 0}/${((scan.vt.vt || {}).total) || 0}` : "";
      const scanNote = scan.verdict
        ? (force ? `Installed despite a ${esc(scan.verdict)} ModScan verdict — at your request.` : `🛡 ModScan: clean${vt}. Pick it from the Custom Mods button in champ select.`)
        : `${m.name || "Mod"} — pick it from the Custom Mods button in champ select.`;
      toast("Mod installed", scanNote, force ? "warning" : "success");
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
  // Dashboard BurntPeanut feature card -> Library, filtered to announcers,
  // searching for the pack so it's the first thing shown.
  window.ChudOpenAnnouncers = function () {
    st.tab = "browse";
    st.cat = "Announcer";
    st.q = "BurntPeanut";
    if (window.ChudNavTo) window.ChudNavTo("library");
    if (root) { paint(); }
  };
  // Shared fetch so the Dashboard can show the same pack data without duplicating it.
  window.ChudGetBundles = async function () {
    try { const r = await S.invoke("library_bundles"); return (r && r.bundles) || (S.hasBackend ? [] : MOCK_BUNDLES); }
    catch (e) { return []; }
  };
})();
