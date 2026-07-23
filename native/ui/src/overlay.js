// Chud overlay — floats over the League client during champ select so
// single-monitor users pick a skin (and maps/announcer/fonts/other mods,
// party status) without alt-tabbing. Talks to the same Tauri commands as the
// main window; Rust shows/hides this window by phase, we only ever
// collapse/expand within it.
const invoke = window.__TAURI__.core.invoke;
const thisWindow = window.__TAURI__.window.getCurrentWindow();

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const skinTileUrl = (id) => `https://cdn.communitydragon.org/latest/champion/${Math.floor(id / 1000)}/tile/skin/${id % 1000}`;
// Per-chroma character render (green Emerald Ashe, etc.). Ownership-independent,
// unlike the champ-select model which Riot only previews for owned skins.
const chromaImgUrl = (id) => `https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/v1/champion-chroma-images/${Math.floor(id / 1000)}/${id}.png`;

// Tint a chroma pill with its real LCU swatch colours: solid for one, a hard
// 50/50 split for two (many chromas are two-tone) so it reads at a glance.
function swatchStyle(colors) {
  const hex = (colors || []).filter((c) => /^#[0-9a-fA-F]{6}$/.test(c));
  if (!hex.length) return "";
  if (hex.length === 1) return `background:${hex[0]};`;
  return `background:linear-gradient(135deg, ${hex[0]} 0%, ${hex[0]} 50%, ${hex[1]} 50%, ${hex[1]} 100%);`;
}

// Deterministic pseudo-color from a name — used for map thumbs / community
// announcer avatars / form swatches, none of which have a real image source.
// Pure CSS (no network image), so it's CSP-safe as a decorative fallback.
function hashHue(str) {
  let h = 0;
  const s = String(str);
  for (let i = 0; i < s.length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h % 360;
}
const hueColor = (str) => `hsl(${hashHue(str)}, 60%, 50%)`;
const gradFor = (str) => {
  const h = hashHue(str);
  return `linear-gradient(135deg, hsl(${h}, 55%, 30%), hsl(${(h + 50) % 360}, 60%, 48%))`;
};

const TABS = ["skins", "maps", "ann", "fonts", "other", "party"];
const OTHER_CATS = ["ui", "vfx", "sfx", "voiceover", "loading_screen", "others"];
const CAT_CHIP = { ui: "UI", vfx: "VFX", sfx: "SFX", voiceover: "VO", loading_screen: "LOAD", others: "OTHER" };

let champId = null;
let pickId = null;
let chromaId = null;
let formId = null;         // client-tracked only — no backend field echoes the active form (see report)
let skinsList = [];
let randomId = null;       // currentRandomSkinId — a rolled random skin is active
let customMod = null;      // currentCustomMod — a custom .fantome pick is active
let customMods = [];       // this champ's available custom mods
let customModChromas = []; // chroma slots the picked mod's WAD targets (from the pick response)
let favSkinId = null;      // this champ's saved favorite skin (auto-applies every game)
let formsCache = {};       // skinId -> forms array, fetched once per champ lock (cheap static-table lookup)
const customPreviewCache = {}; // relativePath -> resolved preview URL (sidecar data URL or R2 thumb; null if none)
let hoverPreviewKey = null;    // guards async preview swaps against fast re-hovers

let expanded = false;
let paused = true;         // starts hidden — Rust's overlay-visibility event turns polling on
let tab = "maps";          // resting default before any champ lock, per design (skins tab needs a champ)
let historic = false;      // historicEnabled from the backend
let histNote = false;      // true only when the backend actually restored a skin this lock (see histRestoredId)
let histRestoredId = null; // st.historicRestoredSkinId — non-null only on a real historic restore

let categoryMods = { map: null, font: null, announcer: null, others: [] };
let mapsList = [];
let annsList = [];
let fontsList = [];
let otherRows = [];        // aggregated across OTHER_CATS, each tagged with its source category
let qMap = "";
let qAnn = "";
let partyState = null;
let cardCols = 2;          // overlayCardCols — skin card size tier (1=large … 3=small), persisted per user
// Min card width per tier — auto-fit wraps to as many full-tile columns as fit
// (≈1/2/3 across the overlay). A minmax width, NOT a repeat() column count, so
// cards can never squish into thin strips if the grid re-lays out.
const CARD_MIN = { 1: "320px", 2: "200px", 3: "135px" };
function applyCardSize() { document.documentElement.style.setProperty("--sk-min", CARD_MIN[cardCols] || "200px"); }

// Signature of the last render, so the 1.5s poll skips rebuilding the DOM when
// nothing changed (otherwise it yanks pills/preview out from under the cursor).
let lastSig = null;

// League's bottom-right control bar (QUIT + chat/social/mic icons) is ~90px
// tall; anchor the overlay's bottom edge just above it so it never covers those.
const BOTTOM_BAR_CLEARANCE = 100;
let curW = 240, curH = 52, lastPosKey = "";
// A transparent window still captures clicks over its whole area, so the
// collapsed launcher must shrink the real OS window or it blocks the game.
async function applyWindowSize() {
  try {
    const winApi = window.__TAURI__.window;
    let w, h;
    if (expanded) {
      w = 460; h = 640;
    } else {
      // Measure the pill and pad generously so it never clips or overflows
      // (overflow:hidden means an undersized window would clip, not scroll).
      const el = document.getElementById("launcher");
      w = Math.ceil((el && el.offsetWidth ? el.offsetWidth : 150) + 10);
      h = Math.ceil((el && el.offsetHeight ? el.offsetHeight : 44) + 10);
    }
    if (w !== curW || h !== curH) {
      curW = w; curH = h;
      await thisWindow.setSize(new winApi.LogicalSize(w, h));
      lastPosKey = ""; // size changed → force a reposition
    }
    await positionOverlay();
  } catch { /* best-effort — never block the UI on a resize failure */ }
}

// Anchor the overlay to the BOTTOM-RIGHT of the LEAGUE CLIENT window (it's
// usually windowed, not fullscreen), sitting ABOVE its QUIT + icon bar. Falls
// back to the monitor corner if the client isn't found. Called on resize AND on
// every poll so it tracks the client window if the user moves it.
async function positionOverlay() {
  try {
    const winApi = window.__TAURI__.window;
    const mon = await winApi.currentMonitor();
    const sf = mon ? (mon.scaleFactor || 1) : 1;
    let right, bottom, minTop;
    let rect = null;
    try { rect = await invoke("league_client_rect"); } catch { /* not open */ }
    if (rect) {
      right = rect.right / sf; bottom = rect.bottom / sf; minTop = rect.top / sf;
    } else if (mon) {
      const left = mon.position.x / sf, top = mon.position.y / sf;
      right = left + mon.size.width / sf; bottom = top + mon.size.height / sf; minTop = top;
    } else return;
    const x = Math.round(right - curW - 12);                                  // right edge, 12px margin
    const y = Math.round(Math.max(minTop + 8, bottom - curH - BOTTOM_BAR_CLEARANCE)); // above QUIT/icon bar
    const key = `${x},${y}`;
    if (key === lastPosKey) return; // unchanged — don't thrash / fight nothing
    lastPosKey = key;
    await thisWindow.setPosition(new winApi.LogicalPosition(x, y));
  } catch { /* best-effort */ }
}

// Resolve a custom mod's preview image once: local sidecar the user dropped in,
// else the Library (R2) catalog thumbnail matched by name. Cached per mod.
async function resolveCustomPreview(rel, hasPrev, modName) {
  if (rel in customPreviewCache) return customPreviewCache[rel];
  let url = null;
  let errored = false;
  if (hasPrev) { try { url = await invoke("skins_custom_mod_preview", { championId: champId, modId: rel }); } catch { errored = true; } }
  // `errored` reflects only the LAST attempted call — a sidecar-lookup error
  // must not block caching a definitive negative from the thumb lookup below.
  if (!url) { errored = false; try { url = await invoke("skins_custom_mod_thumb", { championId: champId, modName }); } catch { errored = true; } }
  // Only cache a CONFIRMED result: an image, or a genuine "no preview exists"
  // (both invokes returned without throwing). A transient invoke failure (worker
  // hiccup) is left uncached so the next hover/prefetch retries — else a single
  // blip would pin the slot-art fallback until an app restart.
  if (url || !errored) customPreviewCache[rel] = url || null;
  return url || null;
}

// `skins_pick_form` routes through the Rust chroma state machine, which (unlike
// `skins_pick_skin`) echoes the form's FAKE id back as both currentPickSkinId
// AND currentChromaId (features::special::FORMS entries are never real skin
// ids). Map that fake id back to its base skin so the grid/chroma-bar lookups
// (which key on real skinIds) keep resolving after a form pick.
function findFormMatch(fakeIdCandidate) {
  if (fakeIdCandidate == null) return null;
  for (const skinIdKey of Object.keys(formsCache)) {
    const f = (formsCache[skinIdKey] || []).find((x) => x.fakeId === fakeIdCandidate);
    if (f) return { baseSkinId: Number(skinIdKey), fakeId: f.fakeId };
  }
  return null;
}

async function tick() {
  if (paused) return; // window hidden — Rust's overlay-visibility event resumes us
  // Default to the CURRENT champ + an ok=false flag so a transient client error
  // (LCU port flap, brief restart) keeps the last state instead of blanking the
  // picker — only a successful poll may change champ/derived state.
  let ok = false;
  let newChamp = champId;
  let newHistoric = historic;
  let newCatMods = categoryMods;
  let rawPick = pickId;
  let rawChroma = chromaId;
  try {
    const st = await invoke("skins_get_state");
    ok = true;
    newChamp = st ? st.currentChampId : null;
    rawPick = st ? st.currentPickSkinId : null;
    rawChroma = st ? st.currentChromaId : null;
    randomId = st ? st.currentRandomSkinId : null;
    customMod = st ? st.currentCustomMod : null;
    if (!customMod) customModChromas = [];
    newHistoric = st ? !!st.historicEnabled : false;
    newCatMods = (st && st.categoryMods) || { map: null, font: null, announcer: null, others: [] };
    histRestoredId = st ? st.historicRestoredSkinId : null;
    histNote = histRestoredId != null;
    if (st && st.overlayCardCols) { cardCols = st.overlayCardCols; applyCardSize(); }
  } catch { /* client down — keep last */ }

  if (ok && newChamp !== champId) {
    const wasLocked = champId != null;
    champId = newChamp;
    skinsList = []; customMods = []; favSkinId = null; formId = null; formsCache = {};
    if (champId) {
      // Heavy LCU scrape — fetch once per champ lock.
      try {
        const r = await invoke("skins_list_champion_skins", { championId: champId });
        skinsList = (r && r.skins) || [];
      } catch { skinsList = []; }
      try {
        const f = await invoke("skins_get_favorites");
        favSkinId = f && f[String(champId)] != null ? f[String(champId)] : null;
      } catch { favSkinId = null; }
      // Forms table lookup is a static in-memory filter (not an LCU call) —
      // cheap enough to resolve for every skin so grid tiles show their badge.
      await Promise.all(skinsList.map(async (s) => {
        try { const r = await invoke("skins_list_forms", { skinId: s.skinId }); formsCache[s.skinId] = (r && r.forms) || []; } catch { formsCache[s.skinId] = []; }
      }));
      expanded = true;
      tab = "skins";
    } else if (wasLocked) {
      tab = "maps";
    }
  }
  historic = newHistoric;
  categoryMods = newCatMods;

  if (ok) {
    const fm = findFormMatch(rawPick);
    if (fm) { pickId = fm.baseSkinId; chromaId = null; formId = fm.fakeId; }
    else { pickId = rawPick; chromaId = rawChroma; formId = null; }
  }

  // Custom mods are a cheap local disk scan — poll every tick so a mod you
  // install/enable mid-champ-select shows up within ~1.5s (no re-lock needed).
  if (champId) {
    try {
      const m = await invoke("skins_list_custom_mods", { championId: champId });
      customMods = (m && m.mods) || [];
      // Warm each mod's preview into the cache now (disk-cached in Rust), so the
      // first hover shows the real image instantly instead of flashing slot art.
      customMods.forEach((cm) => { resolveCustomPreview(cm.relativePath, cm.hasPreview, cm.modName); });
    } catch { /* keep last */ }
  } else {
    customMods = [];
  }

  // Global (champion-independent) category mods — also cheap local scans,
  // polled every tick regardless of which tab is active so badges/chips/the
  // launcher pill stay accurate no matter where the user is looking.
  try { const r = await invoke("skins_list_category_mods", { category: "maps" }); mapsList = (r && r.mods) || []; } catch { mapsList = []; }
  try { const r = await invoke("skins_list_category_mods", { category: "announcers" }); annsList = (r && r.mods) || []; } catch { annsList = []; }
  try { const r = await invoke("skins_list_category_mods", { category: "fonts" }); fontsList = (r && r.mods) || []; } catch { fontsList = []; }
  const catResults = await Promise.all(OTHER_CATS.map(async (cat) => {
    try {
      const r = await invoke("skins_list_category_mods", { category: cat });
      return ((r && r.mods) || []).map((m) => ({ ...m, cat }));
    } catch { return []; /* keep last for this category */ }
  }));
  otherRows = catResults.flat();

  // Only worth polling on the Party tab, or once party is actually on (so it
  // keeps updating live in the background loadout chips/badges).
  if (tab === "party" || (partyState && partyState.enabled)) {
    try { partyState = await invoke("skins_party_get_state"); } catch { partyState = null; }
  }

  render();
  // Re-fit + re-anchor each poll: resizes only when the pill's content changed
  // (idempotent otherwise) and tracks the League client window if it moves.
  applyWindowSize();
}

// Fields shared by the launcher pill, tab badges, and loadout strip — computed
// once per render so all three stay consistent with each other.
function computeDerived() {
  const sel = skinsList.find((s) => s.skinId === pickId) || null;
  const skinActive = !!(champId && (customMod || randomId != null || pickId != null));
  const mapName = categoryMods.map || null;
  const annName = categoryMods.announcer || null;
  const fontName = categoryMods.font || null;
  const otherCount = (categoryMods.others || []).length;
  const totalCount = (skinActive ? 1 : 0) + (mapName ? 1 : 0) + (annName ? 1 : 0) + (fontName ? 1 : 0) + otherCount;

  const dots = [];
  if (skinActive) dots.push({ c: "var(--ov-acc)", name: "Skin" });
  if (mapName) dots.push({ c: "var(--ov-acc-soft)", name: "Map" });
  if (annName) dots.push({ c: "var(--ov-gold)", name: "Announcer" });
  if (fontName) dots.push({ c: "var(--ov-text)", name: "Font" });
  if (otherCount) dots.push({ c: "var(--ov-acc2)", name: "Other mods" });

  const chips = [];
  if (skinActive) {
    let label = customMod ? "🧩 " + customMod
      : randomId != null ? "🎲 Random: " + ((skinsList.find((s) => s.skinId === randomId) || {}).skinName || "skin " + randomId)
      : (sel ? sel.skinName : "");
    if (!customMod && randomId == null) {
      const forms = (sel && formsCache[sel.skinId]) || [];
      const curForm = forms.find((f) => f.fakeId === formId);
      const chromas = (sel && sel.chromas) || [];
      const cn = chromas.find((c) => c.id === chromaId);
      if (curForm) label += " · " + curForm.display; else if (cn) label += " · " + cn.name;
    }
    chips.push({ tab: "skins", dot: "var(--ov-acc)", label, title: "Skin — click to view" });
  }
  if (mapName) chips.push({ tab: "maps", dot: "var(--ov-acc-soft)", label: mapName, title: "Map mod" });
  if (annName) chips.push({ tab: "ann", dot: "var(--ov-gold)", label: annName, title: "Announcer" });
  if (fontName) chips.push({ tab: "fonts", dot: "var(--ov-text)", label: fontName, title: "Font mod" });
  if (otherCount) chips.push({ tab: "other", dot: "var(--ov-acc2)", label: otherCount + " other", title: "Stacking mods" });

  return { sel, skinActive, mapName, annName, fontName, otherCount, totalCount, dots, chips };
}

function render() {
  const d = computeDerived();
  const sig = [
    expanded, tab, champId, pickId, chromaId, formId, randomId, customMod, favSkinId,
    historic, histNote, histRestoredId,
    customMods.map((m) => m.relativePath).join(","),
    skinsList.map((s) => s.skinId + (s.downloaded ? "d" : "")).join(","),
    categoryMods.map, categoryMods.font, categoryMods.announcer, (categoryMods.others || []).join(","),
    mapsList.map((m) => m.id).join(","), annsList.map((m) => m.id).join(","), fontsList.map((m) => m.id).join(","),
    otherRows.map((m) => m.cat + ":" + m.id).join(","),
    qMap, qAnn,
    partyState ? JSON.stringify(partyState) : "",
  ].join("|");
  if (sig === lastSig) return;
  lastSig = sig;

  document.getElementById("launcher").style.display = expanded ? "none" : "inline-flex";
  document.getElementById("wrap").style.display = expanded ? "flex" : "none";
  renderLauncher(d);
  if (!expanded) return;

  renderBar();
  renderTabs(d);
  TABS.forEach((t) => document.getElementById("tab-" + t).classList.toggle("on", tab === t));

  if (tab === "skins") renderSkinsPanel();
  else if (tab === "maps") renderMapsPanel();
  else if (tab === "ann") renderAnnPanel();
  else if (tab === "fonts") renderFontsPanel();
  else if (tab === "other") renderOtherPanel();
  else if (tab === "party") renderPartyPanel(d);

  renderLoadout(d);
}

function renderLauncher(d) {
  const badge = document.getElementById("lbadge");
  if (d.totalCount > 0) { badge.textContent = String(d.totalCount); badge.style.display = ""; } else { badge.style.display = "none"; }
  document.getElementById("ldots").innerHTML = d.dots.map((x) => `<span title="${esc(x.name)}" style="background:${x.c};box-shadow:0 0 4px ${x.c};"></span>`).join("");
  document.getElementById("lhist").style.display = historic ? "" : "none";
}

function renderBar() {
  const champEl = document.getElementById("champ");
  if (!champId) {
    champEl.textContent = "Lock a champion…";
  } else {
    const base = skinsList.find((s) => s.skinId % 1000 === 0);
    champEl.textContent = base ? base.skinName : (skinsList.length ? "" : "loading…");
  }
  const dice = document.getElementById("dice");
  dice.style.display = tab === "skins" ? "" : "none";
  dice.classList.toggle("dis", !champId);
  dice.title = champId ? "Roll a random skin you can inject" : "Lock a champion first";
  const scz = document.getElementById("skcardsize");
  if (scz) {
    scz.style.display = tab === "skins" ? "" : "none";
    scz.title = "Skin card size: " + (cardCols === 1 ? "large" : cardCols === 3 ? "small" : "medium") + " — click to change";
  }
}

function setTabBadge(name, on) {
  document.getElementById("tabbadge-" + name).style.display = on ? "" : "none";
}
function renderTabs(d) {
  TABS.forEach((t) => document.getElementById("tabbtn-" + t).classList.toggle("on", tab === t));
  setTabBadge("skins", d.skinActive);
  setTabBadge("maps", !!d.mapName);
  setTabBadge("ann", !!d.annName);
  setTabBadge("fonts", !!d.fontName);
  setTabBadge("party", false);
  const ob = document.getElementById("tabbadge-other");
  if (d.otherCount > 0) { ob.textContent = String(d.otherCount); ob.style.display = ""; } else { ob.style.display = "none"; }
}

// ── Skins tab (existing behavior, kept — extended with forms) ──────────────

function renderSkinsPanel() {
  const grid = document.getElementById("grid");
  const clear = document.getElementById("clear");

  renderCustomBar();

  if (!champId) {
    grid.innerHTML = `<div class="emptycol"><span class="icon">🔒</span><span>Lock a champion in champ select and their skins appear here.</span><span class="sub">Maps, voice, fonts &amp; other mods work any time — use the tabs above.</span></div>`;
    clear.style.display = "none";
    document.getElementById("formbar").style.display = "none";
    document.getElementById("chromabar").style.display = "none";
    renderStatusBar();
    return;
  }
  if (!skinsList.length) {
    grid.innerHTML = `<div class="empty">Loading skins…</div>`;
    clear.style.display = "none";
    document.getElementById("formbar").style.display = "none";
    document.getElementById("chromabar").style.display = "none";
    renderStatusBar();
    return;
  }

  grid.innerHTML = skinsList.map((s) => {
    const on = pickId === s.skinId;
    // Historic restore doesn't touch pickId (no manual pick was made), so give
    // its tile the same ring when there's no manual pick to highlight instead.
    const histOn = !on && histRestoredId != null && histRestoredId === s.skinId;
    const undl = s.downloaded ? "" : " undl";
    const note = s.downloaded ? "" : `<span class="undlnote">not downloaded</span>`;
    const forms = formsCache[s.skinId] || [];
    const hasForms = forms.length > 0;
    const hasChromas = s.chromas && s.chromas.length;
    // Forms badge takes precedence over the chroma badge on the tile.
    const badge = hasForms
      ? `<span class="formbadge" title="${forms.length} forms">◆ ${forms.length}</span>`
      : hasChromas ? `<span class="chrbadge" title="${s.chromas.length} chromas available">◈ ${s.chromas.length}</span>` : "";
    const check = on && chromaId == null && formId == null ? '<span class="chk">✓</span>' : "";
    const isFav = favSkinId === s.skinId;
    const star = `<span class="fav${isFav ? " on" : ""}" data-fav="${s.skinId}" title="${isFav ? "Unset favorite" : "Set favorite — auto-applies every game"}">${isFav ? "★" : "☆"}</span>`;
    // A custom mod filed under this skin's id — badge nudges users who'd
    // otherwise pick the tile and miss the .ccustom chip below the grid.
    const mod = customMods.find((cm) => cm.skinId === s.skinId);
    const modBadge = mod
      ? `<span class="skin-modbadge" data-cmod="${esc(mod.relativePath)}" data-cskin="${mod.skinId}" data-hasprev="${mod.hasPreview ? 1 : 0}" data-modname="${esc(mod.modName)}" title="Custom mod: ${esc(mod.modName)} — click to apply">🧩 Mod</span>`
      : "";
    return `<div class="sk${on || histOn ? " on" : ""}${undl}" data-skin="${s.skinId}" data-dl="${s.downloaded ? 1 : 0}">
      <img loading="lazy" src="${skinTileUrl(s.skinId)}" alt="" data-imgerr="hide">
      ${badge}${modBadge}${note}${star}<span class="nm">${esc(s.skinName)}</span>${check}</div>`;
  }).join("");

  renderFormBar();
  renderChromaBar();
  clear.style.display = pickId != null ? "block" : "none";
  renderStatusBar();
}

// Your own .fantome mods for this champ (violet pills below the grid).
function renderCustomBar() {
  const bar = document.getElementById("custombar");
  // Once a skin is picked, custom mods appear in the chroma bar (as variants of
  // that skin) — so this bar only shows pre-pick, to apply a custom skin directly.
  if (!champId || !customMods.length || pickId) { bar.style.display = "none"; return; }
  bar.innerHTML = `<span class="cmlbl">Your mods:</span>` + customMods.map((m) =>
    `<span class="cmod${customMod && m.modName === customMod ? " on" : ""}" data-cmod="${esc(m.relativePath)}" data-cskin="${m.skinId}" data-hasprev="${m.hasPreview ? 1 : 0}" title="${esc(m.description || m.modName)}">${esc(m.modName)}</span>`
  ).join("") + customModChromaRow();
  bar.style.display = "flex";
}

// Swatch row for the chroma slots the picked custom mod targets — lets the
// user choose which chroma the injector loads (first slot when none picked).
function customModChromaRow() {
  if (!customMod || !customModChromas.length) return "";
  const autoOn = !customModChromas.some((c) => c.id === chromaId);
  return `<span class="cbnm cbcustom">◈ chroma</span>
    <span class="chr${autoOn ? " on" : ""}" data-cmodchroma="" title="Let Chud pick (${esc(customModChromas[0].name)})">Auto</span>
    ${customModChromas.map((c, i) => `<span class="chrsw${chromaId === c.id ? " on" : ""}" data-cmodchroma="${c.id}" title="${esc(c.name)}" style="${swatchStyle(c.colors)}"><b>${i + 1}</b></span>`).join("")}`;
}

// Alternate forms (Elementalist Lux etc.) for the currently selected skin —
// stacks ABOVE the chroma row per the design.
function renderFormBar() {
  const fbar = document.getElementById("formbar");
  const sel = skinsList.find((s) => s.skinId === pickId);
  const forms = sel ? (formsCache[sel.skinId] || []) : [];
  if (!sel || !sel.downloaded || !forms.length) { fbar.style.display = "none"; return; }
  fbar.innerHTML = `<span class="fbnm">◆ ${esc(sel.skinName)} — form</span>
    <div class="fbtiles">
      ${forms.map((f) => {
        const on = formId === f.fakeId;
        const initial = String(f.display || "?").charAt(0).toUpperCase();
        return `<div class="fbt${on ? " on" : ""}" data-form="${f.fakeId}" data-display="${esc(f.display)}" data-formskin="${sel.skinId}" title="${esc(f.display)} form">
          <div class="fbsw" style="background:${gradFor(f.display)}"><b>${esc(initial)}</b></div>
          <span class="fbnm2">${esc(f.display)}</span>
        </div>`;
      }).join("")}
    </div>`;
  fbar.style.display = "block";
}

// Chroma selection lives in a persistent bar pinned above the footer — never
// clipped by the scrolling grid regardless of where the picked skin sits.
function renderChromaBar() {
  const cbar = document.getElementById("chromabar");
  const sel = skinsList.find((s) => s.skinId === pickId);
  if (!sel) { cbar.style.display = "none"; return; }
  let html = "";
  // Official Riot chromas for the picked skin.
  if (sel.chromas && sel.chromas.length) {
    const baseOn = chromaId == null && !customMod;
    html += `<span class="cbnm">◈ ${esc(sel.skinName)} — chroma</span>
      <div class="cbpills">
        <span class="chr${baseOn ? " on" : ""}" data-skin="${sel.skinId}" data-chroma="" title="Base skin">Base</span>
        ${sel.chromas.map((c, i) => `<span class="chrsw${chromaId === c.id ? " on" : ""}" data-skin="${sel.skinId}" data-chroma="${c.id}" title="${esc(c.name)}" style="${swatchStyle(c.colors)}"><b>${i + 1}</b></span>`).join("")}
      </div>`;
  }
  // Installed custom chromas / mods for this champion (violet) — a custom
  // .fantome overlaid on the picked skin. Same data as the "Your mods" bar,
  // surfaced here so it reads as a variant of the selected skin.
  if (customMods.length) {
    html += `<span class="cbnm cbcustom">✦ Custom${sel.chromas && sel.chromas.length ? "" : " — " + esc(sel.skinName)}</span>
      <div class="cbpills">
        ${customMods.map((m) => `<span class="ccustom${customMod && m.modName === customMod ? " on" : ""}" data-cmod="${esc(m.relativePath)}" data-cskin="${m.skinId}" data-hasprev="${m.hasPreview ? 1 : 0}" title="${esc(m.description || m.modName)}">${esc(m.modName)}</span>`).join("")}
      </div>`;
    const row = customModChromaRow();
    if (row) html += `<div class="cbpills">${row}</div>`;
  }
  if (!html) {
    html = `<span class="cbnm">${esc(sel.skinName)}</span><span class="cbnone">No chromas for this skin</span>`;
  }
  cbar.innerHTML = html;
  cbar.style.display = "block";
}

// Active random-roll / custom-mod / historic-restore indicator — priority
// custom (violet) > random (gold) > historic notice (gold).
function renderStatusBar() {
  const sb = document.getElementById("statusbar");
  if (customMod) {
    sb.className = "cst";
    sb.innerHTML = `<span class="stx">🧩 Custom mod: ${esc(customMod)}</span><button class="stx0" id="clearcustom">Clear</button>`;
    sb.style.display = "flex";
  } else if (randomId != null) {
    const s = skinsList.find((x) => x.skinId === randomId);
    sb.className = "rnd";
    sb.innerHTML = `<span class="stx">🎲 Random: ${esc(s ? s.skinName : "skin " + randomId)}</span><button class="stx0" id="cancelrandom">Cancel</button>`;
    sb.style.display = "flex";
  } else if (histNote && historic && champId) {
    sb.className = "rnd"; // gold accent, same as random — matches the design's historic color
    sb.innerHTML = `<span class="stx">⟲ Historic restored your last pick</span><button class="stx0" id="undo">Undo</button>`;
    sb.style.display = "flex";
  } else {
    sb.className = "";
    sb.style.display = "none";
  }
}

// ── Maps tab (single-select) ────────────────────────────────────────────────

function renderMapsPanel() {
  const input = document.getElementById("mapsearch");
  if (input.value !== qMap) input.value = qMap;
  const q = qMap.trim().toLowerCase();
  const list = mapsList.filter((m) => !q || m.name.toLowerCase().includes(q));
  const noneOn = !categoryMods.map;
  let html = `<div class="lrow${noneOn ? " on" : ""}" data-map="">
    <span class="radio${noneOn ? " on" : ""}"></span>
    <span class="noname">None — Riot default map</span>
  </div>`;
  html += list.map((m) => {
    const on = categoryMods.map === m.name;
    return `<div class="lrow${on ? " on" : ""}" data-map="${esc(m.id)}" title="${esc(m.description || m.name)}">
      <span class="radio${on ? " on" : ""}"></span>
      <span class="mthumb" style="background:${gradFor(m.name)}"></span>
      <span class="mname"><span class="t1">${esc(m.name)}</span>${m.description ? `<span class="t2">${esc(m.description)}</span>` : ""}</span>
    </div>`;
  }).join("");
  if (q && !list.length) html += `<div class="nomatch">No map mods match "${esc(qMap)}"</div>`;
  document.getElementById("mapslist").innerHTML = html;
}

// ── Announcer tab (single-select + stubbed preview) ─────────────────────────

function renderAnnPanel() {
  const input = document.getElementById("annsearch");
  if (input.value !== qAnn) input.value = qAnn;
  const q = qAnn.trim().toLowerCase();
  const list = annsList.filter((a) => !q || a.name.toLowerCase().includes(q));
  const noneOn = !categoryMods.announcer;
  let html = `<div class="lrow${noneOn ? " on" : ""}" data-ann="">
    <span class="radio${noneOn ? " on" : ""}"></span>
    <span class="noname">None — Riot default announcer</span>
  </div>`;
  html += list.map((a) => {
    const on = categoryMods.announcer === a.name;
    const isPeanut = /burnt ?peanut/i.test(a.name);
    const avatar = isPeanut
      ? `<span class="avimg" style="background-image:url('img/burntpeanut.png')"></span>`
      : `<span class="avinit" style="background:${hueColor(a.name)}">${esc(String(a.name).charAt(0).toUpperCase())}</span>`;
    return `<div class="lrow${on ? " on" : ""}" data-ann="${esc(a.id)}" title="${esc(a.description || a.name)}">
      <span class="radio${on ? " on" : ""}"></span>
      ${avatar}
      <span class="mname"><span class="t1">${esc(a.name)}</span><span class="t2" style="color:${isPeanut ? "var(--ov-gold)" : "var(--ov-muted)"}">${isPeanut ? "Chud Original" : "community"}</span></span>
      <button class="playbtn dis" data-play="${esc(a.id)}" title="Preview coming soon">▶</button>
    </div>`;
  }).join("");
  if (q && !list.length) html += `<div class="nomatch">No announcer packs match "${esc(qAnn)}"</div>`;
  document.getElementById("annlist").innerHTML = html;
}

// ── Fonts tab (single-select; empty state when nothing is installed) ───────

function renderFontsPanel() {
  const body = document.getElementById("fontsbody");
  if (!fontsList.length) {
    body.innerHTML = `<div class="emptyfont">
      <span class="glyph">Aa</span>
      <span class="t1">No font mods installed</span>
      <span class="t2">Font packs replace the in-game font. Install some from the Chud Library and they show up here.</span>
      <button class="libbtn" id="openlib" title="Opens the Library in the main Chud window">Open Library ↗</button>
    </div>`;
    return;
  }
  const noneOn = !categoryMods.font;
  let html = `<div class="listwrap"><div class="lrow${noneOn ? " on" : ""}" data-font="">
    <span class="radio${noneOn ? " on" : ""}"></span>
    <span class="noname">None — Riot default font</span>
  </div>`;
  html += fontsList.map((f) => {
    const on = categoryMods.font === f.name;
    return `<div class="lrow${on ? " on" : ""}" data-font="${esc(f.id)}" title="${esc(f.description || f.name)}">
      <span class="radio${on ? " on" : ""}"></span>
      <span class="mname"><span class="t1">${esc(f.name)}</span>${f.description ? `<span class="t2">${esc(f.description)}</span>` : ""}</span>
    </div>`;
  }).join("");
  html += `</div>`;
  body.innerHTML = html;
}

// ── Other tab (multi-select stacking bucket, violet) ────────────────────────

function renderOtherPanel() {
  const el = document.getElementById("otherlist");
  const activeSet = new Set(categoryMods.others || []);
  if (!otherRows.length) {
    el.innerHTML = `<div class="empty">No other mods installed.</div>`;
  } else {
    el.innerHTML = otherRows.map((o) => {
      const on = activeSet.has(o.relativePath);
      return `<div class="orow${on ? " on" : ""}" data-other="${esc(o.relativePath)}" data-cat="${esc(o.cat)}" title="${esc(o.description || o.name)}">
        <span class="ocat">${esc(CAT_CHIP[o.cat] || o.cat)}</span>
        <span class="mname"><span class="t1">${esc(o.name)}</span>${o.description ? `<span class="t2">${esc(o.description)}</span>` : ""}</span>
        <span class="ochk${on ? " on" : ""}">${on ? "✓" : ""}</span>
      </div>`;
    }).join("");
  }
  const foot = document.getElementById("otherfoot");
  const n = (categoryMods.others || []).length;
  if (n > 0) {
    foot.innerHTML = `<span class="otx">🧩 ${n} stacking — all inject together</span><button class="ghostbtn" id="clearother">Clear all</button>`;
    foot.style.display = "flex";
  } else {
    foot.style.display = "none";
  }
}

// ── Party tab (status only) ─────────────────────────────────────────────────

function renderPartyPanel(d) {
  const hint = document.getElementById("partyhint");
  const list = document.getElementById("partylist");
  const p = partyState;
  if (!p) {
    hint.textContent = "";
    list.innerHTML = `<div class="empty">Party status unavailable.</div>`;
    return;
  }
  const peers = Array.isArray(p.peers) ? p.peers : [];
  hint.textContent = !p.enabled
    ? "Party sync is off."
    : (peers.length ? `${peers.length} lobbymate${peers.length === 1 ? "" : "s"} running Chud — picks sync automatically.` : "No other lobbymates running Chud yet.");

  const youName = p.my_summoner_name || "You";
  const youSub = d.chips.length ? d.chips.map((c) => c.label).join(" · ") : "nothing yet";
  let html = `<div class="prow you">
    <span class="pav" style="background:var(--ov-acc)">${esc(youName.charAt(0).toUpperCase())}</span>
    <span class="pname"><span class="t1" style="color:var(--ov-acc)">${esc(youName)}</span><span class="t2">${esc(youSub)}</span></span>
    <span class="pstatus" style="color:var(--ov-acc)">✓ signed</span>
  </div>`;
  html += peers.map((peer) => {
    const name = peer.summoner_name || "Peer";
    const connected = !!peer.connected;
    const sel = peer.skin_selection;
    const sub = sel ? `champ ${sel.champion_id} · skin ${sel.skin_id}${sel.chroma_id ? " · chroma " + sel.chroma_id : ""}` : "no pick yet";
    return `<div class="prow${connected ? "" : " dim"}">
      <span class="pav" style="background:${connected ? "var(--ov-acc2)" : "var(--ov-dim)"}">${esc(String(name).charAt(0).toUpperCase())}</span>
      <span class="pname"><span class="t1">${esc(name)}</span><span class="t2">${esc(sub)}</span></span>
      <span class="pstatus" style="color:${connected ? "var(--ov-acc)" : "var(--ov-dim)"}">${connected ? "✓ synced" : "—"}</span>
    </div>`;
  }).join("");
  html += `<div class="pnote">No summoner IDs leave your machine — ephemeral, signed session identities only.</div>`;
  list.innerHTML = html;
}

// ── Loadout strip (persistent, all tabs) ────────────────────────────────────

function renderLoadout(d) {
  let html = d.chips.map((c) => `<span class="chip" data-tab="${c.tab}" title="${esc(c.title)}"><span class="cdot" style="background:${c.dot};box-shadow:0 0 4px ${c.dot};"></span>${esc(c.label)}</span>`).join("");
  if (!d.chips.length) html += `<span class="nochips">Nothing injecting yet</span>`;
  html += `<span id="historic" class="histpill${historic ? " on" : ""}" title="Historic — remember my picks: whatever you injected last per champion auto-applies next game">⟲ Historic</span>`;
  document.getElementById("loadout").innerHTML = html;
}

// Single delegated click handler (CSP-safe — no inline onclick).
document.addEventListener("click", async (e) => {
  if (e.target.closest("#launcher")) {
    expanded = true;
    render();
    applyWindowSize();
    return;
  }
  if (e.target.id === "collapse" || e.target.id === "close") {
    // Just collapse to the pill — hiding the whole window is the Rust
    // gameflow poller's job, not ours.
    expanded = false;
    render();
    applyWindowSize();
    return;
  }
  const tabBtn = e.target.closest(".tab[data-tab]");
  if (tabBtn) { tab = tabBtn.dataset.tab; render(); return; }
  const chipEl = e.target.closest(".chip[data-tab]");
  if (chipEl) { tab = chipEl.dataset.tab; render(); return; }

  if (e.target.id === "skcardsize") {
    cardCols = cardCols === 1 ? 2 : cardCols === 2 ? 3 : 1;
    applyCardSize();
    const scz = document.getElementById("skcardsize");
    if (scz) scz.title = "Skin card size: " + (cardCols === 1 ? "large" : cardCols === 3 ? "small" : "medium") + " — click to change";
    try { await invoke("skins_set_overlay_card_cols", { cols: cardCols }); } catch {}
    return;
  }
  if (e.target.id === "dice") {
    if (!champId) return;
    try { const r = await invoke("skins_roll_random", { championId: champId }); if (r) { randomId = r.skinId; histNote = false; } } catch {}
    render();
    return;
  }
  if (e.target.id === "cancelrandom") {
    try { await invoke("skins_cancel_random"); } catch {}
    randomId = null;
    render();
    return;
  }
  if (e.target.id === "clearcustom") {
    try { await invoke("skins_clear_custom_mod"); } catch {}
    customMod = null; customModChromas = [];
    render();
    return;
  }
  if (e.target.id === "undo") {
    try { await invoke("skins_clear_pick"); } catch {}
    pickId = null; chromaId = null; formId = null; histNote = false;
    render();
    return;
  }
  const cm = e.target.closest("[data-cmod]");
  if (cm) {
    try {
      const r = await invoke("skins_pick_custom_mod", { championId: champId, modId: cm.dataset.cmod });
      if (r) { customMod = r.modName; customModChromas = r.chromaSlots || []; }
    } catch {}
    randomId = null; histNote = false;
    render();
    return;
  }
  // Chroma swatch on the picked custom mod — sets the slot the injector loads
  // without dropping the mod (the normal chroma pill would supersede it).
  const cmc = e.target.closest("[data-cmodchroma]");
  if (cmc) {
    const id = cmc.dataset.cmodchroma ? parseInt(cmc.dataset.cmodchroma, 10) : null;
    try { await invoke("skins_set_custom_mod_chroma", { chromaId: id }); chromaId = id; } catch {}
    render();
    return;
  }
  // Favorite star — set/unset the champ's set-and-forget skin (auto-applies).
  const fav = e.target.closest("[data-fav]");
  if (fav) {
    const id = parseInt(fav.dataset.fav, 10);
    const next = favSkinId === id ? null : id;
    // Only reflect the new star state if the backend actually accepted it —
    // otherwise the star would lie until the next champ re-lock.
    try { await invoke("skins_set_favorite", { champId: champId, skinId: next }); favSkinId = next; } catch {}
    render();
    return;
  }
  // Form tile — re-tapping the selected form deselects (falls back to base).
  const fm = e.target.closest("[data-form]");
  if (fm) {
    const skinId = parseInt(fm.dataset.formskin, 10);
    const fakeId = parseInt(fm.dataset.form, 10);
    const display = fm.dataset.display || "";
    const same = formId === fakeId;
    try {
      if (same) { await invoke("skins_pick_skin", { skinId, chromaId, skinName: null }); formId = null; customMod = null; customModChromas = []; }
      else { await invoke("skins_pick_form", { skinId, fakeId, display }); formId = fakeId; }
    } catch {}
    histNote = false;
    render();
    return;
  }
  // Chroma pill (checked before the skin-tile fallback — it carries data-skin too).
  const chr = e.target.closest("[data-chroma]");
  if (chr) {
    const skin = parseInt(chr.dataset.skin, 10);
    const chroma = chr.dataset.chroma ? parseInt(chr.dataset.chroma, 10) : null;
    try {
      await invoke("skins_pick_skin", { skinId: skin, chromaId: chroma });
      pickId = skin; chromaId = chroma; formId = null; histNote = false;
      // The backend drops an active custom mod on a normal pick (last pick
      // wins) — mirror it or the mod pill reads as still active.
      customMod = null; customModChromas = [];
    } catch {}
    render();
    return;
  }
  const sk = e.target.closest("[data-skin]");
  if (sk) {
    if (sk.dataset.dl !== "1") return; // not downloaded → can't inject
    const id = parseInt(sk.dataset.skin, 10);
    const already = pickId === id && chromaId == null && formId == null;
    try {
      if (already) { await invoke("skins_clear_pick"); pickId = null; chromaId = null; }
      else { await invoke("skins_pick_skin", { skinId: id, chromaId: null }); pickId = id; chromaId = null; customMod = null; customModChromas = []; }
    } catch {}
    formId = null; histNote = false;
    render();
    return;
  }
  if (e.target.id === "clear") {
    try { await invoke("skins_clear_pick"); } catch {}
    pickId = null; chromaId = null; formId = null; histNote = false;
    render();
    return;
  }

  // Announcer preview — no audio-extraction backend yet, so guard the row
  // click and stop right there (stubbed, per handoff).
  if (e.target.closest("[data-play]")) return;

  const mapEl = e.target.closest("[data-map]");
  if (mapEl) {
    const id = mapEl.dataset.map;
    try {
      if (!id) { await invoke("skins_clear_category_mod", { category: "maps" }); categoryMods.map = null; }
      else { const m = mapsList.find((x) => x.id === id); await invoke("skins_pick_category_mod", { category: "maps", modId: id }); categoryMods.map = m ? m.name : id; }
    } catch {}
    render();
    return;
  }
  const annEl = e.target.closest("[data-ann]");
  if (annEl) {
    const id = annEl.dataset.ann;
    try {
      if (!id) { await invoke("skins_clear_category_mod", { category: "announcers" }); categoryMods.announcer = null; }
      else { const a = annsList.find((x) => x.id === id); await invoke("skins_pick_category_mod", { category: "announcers", modId: id }); categoryMods.announcer = a ? a.name : id; }
    } catch {}
    render();
    return;
  }
  const fontEl = e.target.closest("[data-font]");
  if (fontEl) {
    const id = fontEl.dataset.font;
    try {
      if (!id) { await invoke("skins_clear_category_mod", { category: "fonts" }); categoryMods.font = null; }
      else { const f = fontsList.find((x) => x.id === id); await invoke("skins_pick_category_mod", { category: "fonts", modId: id }); categoryMods.font = f ? f.name : id; }
    } catch {}
    render();
    return;
  }
  const otherEl = e.target.closest("[data-other]");
  if (otherEl) {
    const id = otherEl.dataset.other;
    const cat = otherEl.dataset.cat;
    categoryMods.others = categoryMods.others || [];
    const on = categoryMods.others.includes(id);
    try {
      if (on) { await invoke("skins_clear_category_mod", { category: cat, modId: id }); categoryMods.others = categoryMods.others.filter((x) => x !== id); }
      else { await invoke("skins_pick_category_mod", { category: cat, modId: id }); categoryMods.others = [...categoryMods.others, id]; }
    } catch {}
    render();
    return;
  }
  if (e.target.id === "clearother") {
    const cats = new Set(otherRows.map((r) => r.cat));
    try { await Promise.all([...cats].map((c) => invoke("skins_clear_category_mod", { category: c }))); } catch {}
    categoryMods.others = [];
    render();
    return;
  }
  if (e.target.id === "openlib") {
    // Best-effort — focus the main window and tell it to navigate to Library;
    // no-op if either fails.
    try {
      const wins = await window.__TAURI__.window.getAllWindows();
      const main = wins.find((w) => w.label === "main");
      if (main) {
        await main.setFocus();
        try { await main.emit("open-library", {}); } catch {}
      }
    } catch {}
    return;
  }
  if (e.target.closest("#historic")) {
    const next = !historic;
    try { await invoke("skins_set_historic_mode", { enabled: next }); } catch {}
    historic = next; histNote = false;
    render();
    return;
  }
});

// Live substring search — kept as dedicated 'input' listeners (not the click
// delegate) so typing never fights a render() rebuild of the input itself;
// render() only ever touches the results list next to it.
["mapsearch", "annsearch"].forEach((id) => {
  document.getElementById(id).addEventListener("input", (e) => {
    if (id === "mapsearch") qMap = e.target.value; else qAnn = e.target.value;
    render();
  });
});

// Hover a chroma pill → show its render in our own preview panel. No LCU call, so
// it works for unowned skins and can't fight the champ-select model (no loop).
const cpreview = document.getElementById("cpreview");
document.addEventListener("mouseover", (e) => {
  const chromaPill = e.target.closest("#chromabar [data-chroma]");
  const customPill = e.target.closest("[data-cmod]");
  const cmodChroma = e.target.closest("[data-cmodchroma]");
  const img = cpreview.querySelector("img");
  const nm = cpreview.querySelector(".pvnm");
  if (cmodChroma && cmodChroma.dataset.cmodchroma) {
    img.style.display = "";
    img.src = chromaImgUrl(parseInt(cmodChroma.dataset.cmodchroma, 10));
    nm.textContent = cmodChroma.getAttribute("title") || "Chroma";
    cpreview.style.display = "block";
  } else if (chromaPill) {
    const skin = parseInt(chromaPill.dataset.skin, 10);
    const chroma = chromaPill.dataset.chroma ? parseInt(chromaPill.dataset.chroma, 10) : null;
    img.style.display = "";
    if (chroma == null) { img.src = skinTileUrl(skin); nm.textContent = "Base skin"; }
    else { img.src = chromaImgUrl(chroma); nm.textContent = chromaPill.getAttribute("title") || "Chroma"; }
    cpreview.style.display = "block";
  } else if (customPill) {
    // A .fantome has no embedded image: resolve a real preview (sidecar or the
    // Library R2 thumbnail), falling back to the slot it replaces while it loads.
    const skin = parseInt(customPill.dataset.cskin, 10);
    const rel = customPill.dataset.cmod;
    // Grid-tile badges carry the real name in data-modname (their textContent
    // is just the generic "🧩 Mod" label); the .ccustom/.cmod chips don't set
    // it, so they keep falling back to their own textContent.
    const modName = customPill.dataset.modname || customPill.textContent || "Custom mod";
    img.style.display = "";
    nm.textContent = "🧩 " + modName;
    cpreview.style.display = "block";
    hoverPreviewKey = rel;
    // Already resolved? Show it with no flash. Otherwise slot art, then swap in.
    const cached = customPreviewCache[rel];
    if (cached) {
      img.src = cached;
    } else {
      img.src = skinTileUrl(skin);
      resolveCustomPreview(rel, customPill.dataset.hasprev === "1", modName).then((url) => {
        if (url && hoverPreviewKey === rel && cpreview.style.display === "block") img.src = url;
      });
    }
  }
});
["chromabar", "custombar"].forEach((id) =>
  document.getElementById(id).addEventListener("mouseleave", () => { cpreview.style.display = "none"; })
);

// Broken-image hide (CSP-safe — replaces inline onerror).
document.addEventListener("error", (e) => {
  const t = e.target;
  if (t && t.tagName === "IMG" && t.getAttribute("data-imgerr") === "hide") t.style.display = "none";
}, true);

(async () => {
  await applyWindowSize(); // shrink to the launcher pill immediately — a transparent window still captures clicks over its whole area
  // Rust only shows this window during champ select — pause polling the rest
  // of the game, and reset to the collapsed launcher on every fresh show so a
  // stale expanded/mid-tab state from the last game never pops up unprompted.
  thisWindow.listen("overlay-visibility", (e) => {
    const vis = !!e.payload;
    if (vis) {
      paused = false;
      expanded = false;
      tab = "maps";
      lastSig = null;
      applyWindowSize();
      tick();
    } else {
      paused = true;
    }
  });
  // Live theme sync: the main window broadcasts on every theme switch. Swapping
  // the attribute re-cascades every --ov-* token, so the overlay restyles with
  // no re-render. theme-boot.js already applied the saved theme on load.
  window.__TAURI__.event.listen("theme-changed", (e) => {
    const id = e && e.payload;
    if (!id) return;
    document.documentElement.dataset.theme = id;
    try { localStorage.setItem("chud-theme", id); } catch { /* private mode */ }
  });
  // A broken mod (overrides champion ability data) was skipped at inject time.
  // Surface it IN-GAME so the user knows the SKIN failed, not Chud — heads off
  // "your app broke my game" tickets. The overlay is often collapsed to its pill
  // at inject time, so force it open to make the banner visible.
  let blockedTimer = null;
  window.__TAURI__.event.listen("skin-blocked", (e) => {
    const p = e && e.payload;
    const msg = (p && p.message) || "A skin was blocked to protect your game.";
    if (!expanded) { expanded = true; render(); applyWindowSize(); }
    const el = document.getElementById("blockedBanner");
    if (!el) return;
    el.innerHTML = `<span class="bb-ico">⚠️</span><span class="bb-txt">${esc(msg)}</span><button class="bb-x" id="bbDismiss">Dismiss</button>`;
    el.style.display = "flex";
    const x = document.getElementById("bbDismiss");
    if (x) x.onclick = () => { el.style.display = "none"; };
    if (blockedTimer) clearTimeout(blockedTimer);
    blockedTimer = setTimeout(() => { el.style.display = "none"; }, 9000);
  });
  await tick();
  setInterval(tick, 1500);
})();
