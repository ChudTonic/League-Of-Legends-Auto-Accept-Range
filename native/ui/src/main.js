// ============================================================
// Chud — Neon Glass front-end.
// Talks to the Rust core over the SAME Tauri IPC contract as the
// legacy Hextech UI (do not change command/event names — see README):
//   commands: get_state, toggle_tool, stop_all, set_injection_ack,
//             request_admin, exit_app, get_config, save_config,
//             get_diagnostics, get_profile
//   events:   state-changed  (payload = full state snapshot)
//             notification   (optional: {title, message, tone})
// Skins control panel (S9, see docs/SKINS_PORT.md) — its own command/event
// set, additive, does not touch anything above:
//   commands: skins_get_state, skins_save_settings, skins_download,
//             skins_activate_pengu, skins_set_enabled, skins_diagnostics,
//             skins_party_enable, skins_party_disable, skins_party_add_peer,
//             skins_party_get_state
//   events:   skins-download-progress ({phase, done, total})
//             skins-download-done     ({ok, error?})
//   Party has no push event: the backend's "party-state" broadcast goes out
//   over the in-client bridge WebSocket to the Pengu Loader plugins, not to
//   this webview — the Skins page instead polls skins_party_get_state while
//   it's the active page.
// Falls back to MOCK_STATE / MOCK data in a plain browser so index.html
// previews without a backend.
// ============================================================

const TAURI = window.__TAURI__?.core;

const MOCK_STATE = {
  clientOnline: true, adminReady: false, injectionAck: false, injectionBlocked: false,
  phase: "Lobby", activeToolCount: 1,
  summary: { sessionMatches: "4", totalMatches: "1539", uptime: "2h 14m 06s" },
  tools: [
    { id: "auto_accept", title: "Auto-Accept", safe: true, requiresAdmin: false, running: true,
      subtitle: "Watches the Riot client and accepts the ready check for you.",
      statusText: "ARMED", statusTone: "success", metricLabel: "Accepted · session", metricValue: "4",
      runtimeCopy: "Watching the client, ready to snap up the next ready check.", primaryActionText: "Stop Tool" },
    { id: "auto_range", title: "Auto-Range", safe: false, requiresAdmin: true, running: false,
      subtitle: "Holds the show-range key while a live game is focused; auto-disabled in ranked.",
      statusText: "READY", statusTone: "ice", metricLabel: "Range key", metricValue: "C",
      runtimeCopy: "Hold your attack-range indicator during live games.", primaryActionText: "Launch Auto-Range" },
  ],
};

let state = structuredClone(MOCK_STATE);
let currentPage = "dashboard";

const { esc, invoke } = window.ChudShared;

const TONE = { success: "#33e0a0", running: "#33e0a0", ice: "#35e4ff", info: "#35e4ff", gold: "#7ceeff", warning: "#e6a23c", danger: "#ff5470", neutral: "#6b6b96" };
const toneColor = (t) => TONE[t] || TONE.neutral;

// ── Glyphs (inline SVG so they inherit currentColor) ────────────────────────
const GLYPH_NAMES = ["dashboard", "profile", "settings", "activity", "diagnostics", "power", "bolt", "crosshair", "lock", "warning", "ping", "refresh", "copy", "chevron", "shield", "skin", "library"];
const GLYPHS = {};
async function loadGlyphs() {
  await Promise.all(GLYPH_NAMES.map(async (n) => {
    try { GLYPHS[n] = await (await fetch(`icons/${n}.svg`)).text(); } catch { GLYPHS[n] = ""; }
  }));
}
const ico = (n) => GLYPHS[n] || "";
const glyphForTool = (id) => (id === "auto_accept" ? "bolt" : "crosshair");

const NAV = [
  { page: "dashboard", label: "Dashboard", glyph: "dashboard" },
  { page: "profile", label: "Profile", glyph: "profile" },
  { page: "skins", label: "Skins", glyph: "skin" },
  { page: "activity", label: "Activity", glyph: "activity" },
  { page: "settings", label: "Settings", glyph: "settings" },
  { page: "diagnostics", label: "Diagnostics", glyph: "diagnostics" },
];

// Skin Library is a BETA feature — hidden until enabled from Settings.
let libraryEnabled = false;
let libraryEndpoint = "";
async function loadLibraryState() {
  try { const s = await invoke("library_get"); libraryEnabled = !!(s && s.enabled); libraryEndpoint = (s && s.endpoint) || ""; }
  catch { libraryEnabled = false; }
}
function navItems() {
  const items = NAV.slice();
  if (libraryEnabled) items.splice(2, 0, { page: "library", label: "Library", glyph: "library" });
  return items;
}

// ── Chrome ──────────────────────────────────────────────────────────────────
function renderNav() {
  document.getElementById("nav").innerHTML = navItems().map((n) => `
    <div class="nav-item ${currentPage === n.page ? "active" : ""}" data-page="${n.page}">
      <span class="nav-ico">${ico(n.glyph)}</span><span>${n.label}</span>
    </div>`).join("");
  document.querySelectorAll(".nav-item").forEach((el) => (el.onclick = () => navTo(el.dataset.page)));
}
let _topSig = null;
function renderTop() {
  const n = state.activeToolCount;
  const online = state.clientOnline;
  // The topbar only reflects tool count + client-online — skip the DOM writes
  // (fired ~1/s via state-changed) when neither changed.
  const sig = `${n}|${online ? 1 : 0}`;
  if (sig === _topSig) return;
  _topSig = sig;
  const col = n > 0 ? "#33e0a0" : "#6b6b96";
  const oc = online ? "#33e0a0" : "#6b6b96";
  const tc = document.getElementById("topClient");
  tc.style.color = oc; tc.style.borderColor = oc + "55"; tc.style.background = oc + "18";
  tc.innerHTML = `<span class="slight ${online ? "on" : ""}" style="width:6px;height:6px;background:${oc};color:${oc}"></span>${online ? "Client linked" : "Client offline"}`;
  const c = document.getElementById("topChip");
  c.style.color = col; c.style.borderColor = col + "55"; c.style.background = col + "18";
  c.textContent = `${n} tool${n === 1 ? "" : "s"} active`;
  const sa = document.getElementById("stopAllTop");
  sa.style.display = n > 0 ? "" : "none";
  sa.onclick = onStopAll;
  const ex = document.getElementById("exitIco"); if (ex) ex.innerHTML = ico("power");
  document.getElementById("exitBtn").onclick = () => invoke("exit_app");
}

// ── Dashboard ────────────────────────────────────────────────────────────────
function riskstrip() {
  return `<div class="riskstrip">
    <div class="hazard">${ico("warning")}</div>
    <div class="grow">
      <div class="risk-title">Anti-cheat risk — read before enabling injection tools</div>
      <div class="risk-body">Auto-Range sends synthetic input. Vanguard can detect this and <b style="color:#ff92a4">ban the account</b>. It is auto-disabled in ranked games; the app operates openly (no evasion). Use at your own discretion.</div>
    </div>
    <button class="btn danger sm" id="ackBtn">I understand — unlock</button>
  </div>`;
}
function modCard(t) {
  const needsAck = !t.safe && !state.injectionAck;
  const needsAdmin = t.requiresAdmin && !state.adminReady;
  const locked = needsAck || needsAdmin;
  const tcol = toneColor(t.statusTone);
  const lockReason = needsAck ? "Acknowledge the Vanguard ban risk to unlock." : (needsAdmin ? "Administrator mode required." : "");
  const safeTag = t.safe ? `<span style="color:#33e0a0">SAFE · LCU</span>` : `<span style="color:#e6a23c;display:inline-flex;align-items:center;gap:5px"><span style="width:11px;height:11px;display:inline-flex">${ico("lock")}</span>ADMIN</span>`;
  return `
  <div class="mod ${t.running ? "armed" : ""} ${locked ? "locked" : ""}">
    ${locked ? `<span class="hazard-stripe"></span>` : ""}
    <div class="mod-head">
      <div class="mod-gem">${ico(glyphForTool(t.id))}</div>
      <div class="tog ${t.running ? "on" : ""} ${locked ? "disabled" : ""}" ${locked ? "" : `data-toggle="${t.id}"`}><div class="knob"></div></div>
    </div>
    <div class="mod-title">${esc(t.title)}</div>
    <div class="mod-sub">${esc(t.subtitle)}</div>
    ${locked ? `<div class="mod-lock"><span style="width:13px;height:13px;display:inline-flex">${ico("warning")}</span>${esc(lockReason)}</div>` : ""}
    <div class="mod-foot">
      <span class="mod-status" style="color:${tcol}"><span class="slight ${t.running ? "on" : ""}" style="width:6px;height:6px;background:${tcol};color:${tcol}"></span>${esc(t.statusText)}</span>
      <span class="mod-metric">${esc(t.metricLabel)} · <b>${esc(t.metricValue)}</b></span>
    </div>
    ${needsAdmin && !needsAck ? `<div class="mod-actions"><button class="btn sm block" data-admin="1">Elevate to Administrator</button></div>` : ""}
    <div class="mod-flow"><i></i></div>
  </div>`;
}
function dashboardHtml() {
  const s = state.summary;
  const aa = state.tools.find((x) => x.id === "auto_accept") || {};
  const armed = !!aa.running;
  const coreState = armed ? (state.clientOnline ? "ARMED" : "STANDBY") : "IDLE";
  const heroTitle = armed ? (state.clientOnline ? "Queue watcher is live" : "Waiting for the client…") : "Auto-Accept is idle";
  return `
  <div class="dash">
    <div class="hero ${armed ? "armed" : ""}">
      <div class="core ${armed ? "" : "core-idle"}">
        <div class="core-ring"></div><div class="core-inner"></div>
        <div class="core-label"><div class="core-state">${coreState}</div><div class="core-cap">auto-accept</div></div>
      </div>
      <div class="hero-body">
        <div class="hero-kicker">Session</div>
        <div class="hero-title">${esc(heroTitle)}</div>
        <div class="hero-stats">
          <div class="hstat"><div class="hstat-val grad" id="lc-session">${esc(s.sessionMatches)}</div><div class="hstat-lab">accepted · session</div></div>
          <div class="hstat"><div class="hstat-val" id="lc-total">${esc(s.totalMatches)}</div><div class="hstat-lab">accepted · total</div></div>
          <div class="hstat"><div class="hstat-val uptime" id="lc-uptime">${esc(s.uptime)}</div><div class="hstat-lab">session uptime</div></div>
          <div class="hstat"><div class="hstat-val ${state.activeToolCount > 0 ? "green" : ""}" id="lc-active">${state.activeToolCount}</div><div class="hstat-lab">tools active</div></div>
        </div>
      </div>
      <button class="btn primary" id="stopAllHero" style="align-self:stretch;padding:0 24px;${state.activeToolCount > 0 ? "" : "display:none"}">Stop All</button>
    </div>
    ${state.injectionAck ? "" : riskstrip()}
    <div class="dash-sec"><span class="section-label">Modules</span><span class="rule"></span>
      <button class="btn sm primary" id="startAllBtn">Start All</button>
      <button class="btn sm" id="stopAll2Btn" ${state.activeToolCount > 0 ? "" : "disabled"}>Stop</button>
    </div>
    <div class="modules">${state.tools.map(modCard).join("")}</div>
  </div>`;
}
// Signature of everything the module cards render. Rebuilding them on every
// state event (uptime ticks ~1/s) would restart their animations and could
// swallow a toggle click landing mid-rebuild, so we only rebuild on change.
let modulesSig = "";
const currentModulesSig = () =>
  (state.tools || []).map((t) => `${t.id}:${t.running}:${t.statusText}:${t.statusTone}:${t.metricValue}`).join("|") +
  `|${state.injectionAck}|${state.adminReady}`;

function wireDash() {
  modulesSig = currentModulesSig();
  document.querySelectorAll("#page [data-toggle]").forEach((el) => (el.onclick = () => onToggle(el.dataset.toggle)));
  const ack = document.getElementById("ackBtn"); if (ack) ack.onclick = onAck;
  const sa = document.getElementById("startAllBtn"); if (sa) sa.onclick = onStartAll;
  const sb = document.getElementById("stopAll2Btn"); if (sb) sb.onclick = onStopAll;
  const sh = document.getElementById("stopAllHero"); if (sh) sh.onclick = onStopAll;
  document.querySelectorAll("#page [data-admin]").forEach((b) => (b.onclick = () => invoke("request_admin")));
}

// ── Settings ──────────────────────────────────────────────────────────────
const DEFAULT_CONFIG = {
  auto_accept: { check_interval: 1.0, retry_delay: 5.0, max_retries: 3, max_backoff: 30.0 },
  autorange: { range_hold_key: "c", refresh_interval: 7.5 },
  safety: { block_in_ranked: true, injection_ack: false },
};
let cfg = null;
let appearOffline = false;
async function loadConfig() { cfg = (await invoke("get_config")) || structuredClone(DEFAULT_CONFIG); }
const cval = (s, k) => (cfg[s] && cfg[s][k] !== undefined ? cfg[s][k] : "");

function setField(label, hint, control) {
  return `<div class="set-field"><div><div class="set-flabel">${esc(label)}</div>${hint ? `<div class="set-fhint">${esc(hint)}</div>` : ""}</div><div class="set-control">${control}</div></div>`;
}
const numInput = (s, k, unit) => `<span class="set-input-wrap"><input class="set-input" data-sec="${s}" data-key="${k}" type="number" step="any" value="${esc(cval(s, k))}">${unit ? `<span class="set-unit">${unit}</span>` : ""}</span>`;
const keyInput = (s, k) => `<span class="set-input-wrap"><input class="set-input key" data-sec="${s}" data-key="${k}" type="text" value="${esc(cval(s, k))}"></span>`;
const segMode = (s, k, opts) => `<span class="set-seg">${opts.map((o) => `<button class="seg-btn ${cval(s, k) === o ? "on" : ""}" data-sec="${s}" data-key="${k}" data-val="${o}">${o}</button>`).join("")}</span>`;
const togCtl = (s, k) => `<div class="tog ${cval(s, k) ? "on" : ""}" data-sec="${s}" data-key="${k}"><div class="knob"></div></div>`;

function settingsHtml() {
  const card = (title, glyph, body) => `<div class="glass set-card"><div class="set-card-title"><span class="ci">${ico(glyph)}</span>${title}</div><div class="set-list">${body}</div></div>`;
  return `<div class="set-wrap">
  ${card("Auto-Accept", "bolt", [
    setField("Check interval", "Queue poll cadence", numInput("auto_accept", "check_interval", "s")),
    setField("Retry delay", "Reconnect backoff (base)", numInput("auto_accept", "retry_delay", "s")),
    setField("Max retries", "", numInput("auto_accept", "max_retries", "")),
    setField("Max backoff", "Exponential backoff ceiling", numInput("auto_accept", "max_backoff", "s")),
  ].join(""))}
  ${card("Auto-Range", "crosshair", [
    setField("Range key", "League 'show range' key", keyInput("autorange", "range_hold_key")),
    setField("Refresh interval", "Range redraw cadence", numInput("autorange", "refresh_interval", "s")),
  ].join(""))}
  ${card("Safety", "shield", [
    setField("Block in ranked", "Disable injection tools in ranked games", togCtl("safety", "block_in_ranked")),
    setField("Risk acknowledged", "Vanguard ban risk accepted", togCtl("safety", "injection_ack")),
  ].join(""))}
  ${card("Rune Import", "bolt", [
    setField("Auto-import on champ pick", "Best runes + spells + starting build for your champ, current patch (via u.gg)", togCtl("runes", "enabled")),
    setField("Build source", "Highest win-rate or most popular", segMode("runes", "sort", ["winrate", "popular"])),
    setField("Import now", "Pull the build for your currently-picked champion", `<button class="btn ghost sm" id="runesImportNow">Import build</button>`),
  ].join(""))}
  ${card("Presence", "profile", [
    setField("Appear offline", "Hide from your friends list while you play. Chud sets your chat status to offline and keeps re-asserting it (the client otherwise resets it). Applies instantly.", `<div class="tog ${appearOffline ? "on" : ""}" id="appearOfflineTog"><div class="knob"></div></div>`),
  ].join(""))}
  ${card("Beta features", "bolt", [
    setField("Skin Library", "Enable the in-app skin browser (work in progress). Adds a Skin Library tab to browse and install community skins.", `<div class="tog ${libraryEnabled ? "on" : ""}" id="libraryBetaTog"><div class="knob"></div></div>`),
  ].join(""))}
  <div class="row"><button class="btn primary" id="saveCfg">Save settings</button><span class="dim mono" id="saveHint" style="font-size:11.5px"></span></div>
  </div>`;
}
async function renderSettings() {
  if (!cfg) await loadConfig();
  try { appearOffline = !!(await invoke("get_appear_offline")); } catch { /* keep last */ }
  const p = document.getElementById("page");
  p.innerHTML = settingsHtml();
  p.querySelectorAll("input").forEach((el) => (el.onchange = () => {
    const s = el.dataset.sec, k = el.dataset.key; cfg[s] = cfg[s] || {};
    if (el.type === "number") {
      // Ignore empty/invalid input: NaN would serialize to null and make the
      // Rust side silently reject the whole config on save.
      const v = parseFloat(el.value);
      if (Number.isFinite(v)) cfg[s][k] = v; else el.value = cfg[s][k];
    } else {
      cfg[s][k] = el.value;
    }
  }));
  p.querySelectorAll(".seg-btn").forEach((b) => (b.onclick = () => {
    const s = b.dataset.sec, k = b.dataset.key; cfg[s] = cfg[s] || {}; cfg[s][k] = b.dataset.val;
    b.parentElement.querySelectorAll(".seg-btn").forEach((x) => x.classList.toggle("on", x === b));
  }));
  p.querySelectorAll(".tog[data-sec]").forEach((t) => (t.onclick = () => {
    const s = t.dataset.sec, k = t.dataset.key; cfg[s] = cfg[s] || {}; cfg[s][k] = !cfg[s][k];
    t.classList.toggle("on", cfg[s][k]);
  }));
  document.getElementById("saveCfg").onclick = async () => {
    await invoke("save_config", { cfg });
    const h = document.getElementById("saveHint"); if (h) { h.textContent = "Saved ✓"; setTimeout(() => { if (h) h.textContent = ""; }, 1800); }
    toast("Settings saved", "Configuration written to disk.", "success");
  };
  const libTog = document.getElementById("libraryBetaTog");
  if (libTog) libTog.onclick = async () => {
    libraryEnabled = !libraryEnabled;
    libTog.classList.toggle("on", libraryEnabled);
    try { await invoke("set_library_enabled", { enabled: libraryEnabled }); } catch { /* ignore */ }
    renderNav(); // show/hide the Skin Library tab immediately
    toast(libraryEnabled ? "Skin Library enabled" : "Skin Library hidden", libraryEnabled ? "A Skin Library tab was added to the sidebar." : "The Skin Library tab was removed.", libraryEnabled ? "success" : "neutral");
  };
  const aoTog = document.getElementById("appearOfflineTog");
  if (aoTog) aoTog.onclick = async () => {
    appearOffline = !appearOffline;
    aoTog.classList.toggle("on", appearOffline);
    try { await invoke("set_appear_offline", { enabled: appearOffline }); } catch { /* client may be down; watcher applies when it's up */ }
    toast(appearOffline ? "Appearing offline" : "Back online", appearOffline ? "You're hidden from your friends list while you play." : "Your friends can see you again.", appearOffline ? "success" : "neutral");
  };
  const importBtn = document.getElementById("runesImportNow");
  if (importBtn) importBtn.onclick = async () => {
    // Save first so the (possibly just-toggled) enable/sort are in effect.
    await invoke("save_config", { cfg });
    importBtn.disabled = true; importBtn.textContent = "Importing…";
    const res = await invoke("runes_import_now");
    importBtn.disabled = false; importBtn.textContent = "Import build";
    if (res && (res.runes || res.spells || res.items)) {
      const parts = [res.runes && "runes", res.spells && "spells", res.items && "items"].filter(Boolean).join(" + ");
      toast("Build imported", `Applied ${parts} for your champion.`, "success");
    } else {
      toast("Couldn't import", "Enable Rune Import, and be in champ select with a champion picked.", "warning");
    }
  };
}

// ── Activity log ──────────────────────────────────────────────────────────────
const MAX_ACTIVITY = 200;
let activityLog = [];
function pushActivity(text, tone = "neutral", glyph = "activity") {
  const now = new Date();
  const t = now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  activityLog.unshift({ t, text, tone, glyph });
  if (activityLog.length > MAX_ACTIVITY) activityLog.length = MAX_ACTIVITY;
  if (currentPage === "activity") renderActivity();
}
function recordActivity(prev, next) {
  if (!prev || !next) return;
  if (prev.clientOnline !== next.clientOnline)
    pushActivity(next.clientOnline ? "League client connected" : "League client disconnected", next.clientOnline ? "success" : "neutral", "ping");
  if (prev.phase !== next.phase && next.phase)
    pushActivity(`Gameflow phase → ${next.phase}`, "ice", "activity");
  const pa = parseInt(prev.summary?.sessionMatches || "0", 10);
  const na = parseInt(next.summary?.sessionMatches || "0", 10);
  if (na > pa) pushActivity("Ready check accepted", "success", "bolt");
  for (const t of next.tools || []) {
    const p = (prev.tools || []).find((x) => x.id === t.id);
    if (p && p.running !== t.running)
      pushActivity(`${t.title} ${t.running ? "armed" : "stopped"}`, t.running ? "success" : "neutral", glyphForTool(t.id));
  }
  if (prev.injectionBlocked !== next.injectionBlocked)
    pushActivity(next.injectionBlocked ? "Ranked game detected — injection tools disabled" : "Ranked block cleared", next.injectionBlocked ? "danger" : "ice", "shield");
}
function activityHtml() {
  const rows = activityLog.length
    ? activityLog.map((a) => { const c = toneColor(a.tone);
        return `<div class="act-row"><span class="act-ico" style="color:${c}">${ico(a.glyph)}</span><span class="act-time">${esc(a.t)}</span><span class="act-text">${esc(a.text)}</span></div>`; }).join("")
    : `<div class="dim" style="padding:18px 4px">No activity yet. Events appear here as tools arm, the client connects, and ready checks are accepted.</div>`;
  return `<div class="glass">
    <div class="act-head"><span class="section-label">Activity Log</span>
      <span class="act-meta">${activityLog.length} event${activityLog.length === 1 ? "" : "s"} · this session ${activityLog.length ? `<button class="btn sm" id="actClear">Clear</button>` : ""}</span></div>
    <div class="act-list">${rows}</div></div>`;
}
function renderActivity() {
  const p = document.getElementById("page");
  p.innerHTML = activityHtml();
  const c = document.getElementById("actClear");
  if (c) c.onclick = () => { activityLog = []; renderActivity(); };
}

// ── Diagnostics ───────────────────────────────────────────────────────────────
const DIAG_FALLBACK = {
  app: { name: "Chud", version: "1.0.0", build: "browser" },
  system: { admin: false, os: "windows", arch: "x86_64" },
  lcu: { clientOnline: false, authFound: false, endpoint: "", phase: "" },
  tools: { autoAccept: false, autoRange: false, injectionBlocked: false, injectionAck: false },
  hotkeys: { autoRange: "none (always-on while armed)" },
  config: { rangeHoldKey: "c", blockInRanked: true, checkInterval: 1.0 },
  paths: { config: "", data: "" },
};
const yn = (b) => (b ? "Yes" : "No");
const boolTone = (b) => (b ? "success" : "neutral");
function diagRow(label, value, tone) {
  const v = tone ? `<span style="color:${toneColor(tone)}">${esc(value)}</span>` : esc(value);
  return `<div class="diag-row"><span class="diag-k">${esc(label)}</span><span class="diag-v">${v}</span></div>`;
}
function diagnosticsHtml(d) {
  const card = (title, rows) => `<div class="glass"><div class="diag-card-title">${title}</div><div class="diag-list">${rows.join("")}</div></div>`;
  return `<div class="diag-grid">
  ${card("Application", [diagRow("Name", d.app.name), diagRow("Version", d.app.version), diagRow("Build", d.app.build)])}
  ${card("System", [diagRow("Administrator", yn(d.system.admin), d.system.admin ? "success" : "warning"), diagRow("Platform", `${d.system.os} · ${d.system.arch}`)])}
  ${card("League Client (LCU)", [diagRow("Client online", yn(d.lcu.clientOnline), boolTone(d.lcu.clientOnline)), diagRow("Auth (lockfile) found", yn(d.lcu.authFound), boolTone(d.lcu.authFound)), diagRow("Endpoint", d.lcu.endpoint || "—"), diagRow("Gameflow phase", d.lcu.phase || "—")])}
  ${card("Tools", [diagRow("Auto-Accept", d.tools.autoAccept ? "Running" : "Idle", boolTone(d.tools.autoAccept)), diagRow("Auto-Range", d.tools.autoRange ? "Running" : "Idle", boolTone(d.tools.autoRange)), diagRow("Ranked kill-switch", d.tools.injectionBlocked ? "ENGAGED" : "Clear", d.tools.injectionBlocked ? "danger" : "ice"), diagRow("Risk acknowledged", yn(d.tools.injectionAck), d.tools.injectionAck ? "success" : "warning")])}
  ${card("Configuration", [diagRow("Range hold key", String(d.config.rangeHoldKey).toUpperCase()), diagRow("Block in ranked", yn(d.config.blockInRanked), boolTone(d.config.blockInRanked)), diagRow("Check interval", `${d.config.checkInterval}s`), diagRow("Auto-Range hotkey", d.hotkeys.autoRange)])}
  ${card("Paths", [diagRow("Config file", d.paths.config || "—"), diagRow("Data folder", d.paths.data || "—")])}
  </div>
  <div class="diag-actions">
    <button class="btn sm primary" id="diagRefresh"><span style="width:14px;height:14px;display:inline-flex">${ico("refresh")}</span>Refresh</button>
    <button class="btn sm" id="diagCopy"><span style="width:14px;height:14px;display:inline-flex">${ico("copy")}</span>Copy report</button>
  </div>`;
}
async function renderDiagnostics() {
  const p = document.getElementById("page");
  p.innerHTML = `<div class="glass"><div class="muted">Gathering diagnostics…</div></div>`;
  const d = (await invoke("get_diagnostics")) || DIAG_FALLBACK;
  p.innerHTML = diagnosticsHtml(d);
  document.getElementById("diagRefresh").onclick = () => renderDiagnostics();
  document.getElementById("diagCopy").onclick = async () => {
    const btn = document.getElementById("diagCopy");
    try { await navigator.clipboard.writeText(JSON.stringify(d, null, 2)); btn.querySelector("svg") ? btn.lastChild.textContent = "Copied!" : (btn.textContent = "Copied!"); }
    catch { btn.textContent = "Copy failed"; }
    setTimeout(() => { if (currentPage === "diagnostics") renderDiagnostics(); }, 1400);
  };
}

// ── Skins ────────────────────────────────────────────────────────────────────
// Control panel for the skin-injection subsystem (S9): enable/settings,
// League path, download w/ progress, injection settings, party. The
// in-client chroma/forms picker itself is the bundled CHUD-* Pengu Loader
// plugins, not this page — see docs/SKINS_PORT.md.
const DEFAULT_SKINS_STATE = {
  enabled: false, bridgePort: null, penguActive: false, skinsDownloaded: false, hashesReady: false,
  leaguePath: "", injectionThresholdMs: 300, autoResumeSecs: 60, autoDownload: true,
  party: { enabled: false, my_token: null, my_summoner_id: null, my_summoner_name: "Unknown", peers: [] },
  diagnostics: { bridgePort: null, penguActive: false, toolsAvailable: false, dllValid: false, skinsDownloaded: false, hashesReady: false, dataDir: "" },
};
let skinsState = null;
let skinsCfg = null; // local editable copy of the injection-settings fields (snake_case — mirrors config::SkinsCfg, like DEFAULT_CONFIG/cfg above)
// Local-only risk gate for skin-injection actions, deliberately SEPARATE from
// the dashboard's `injectionAck` (that's the Vanguard input-injection risk;
// this is the "modifies game files & suspends the client" ToS risk) — see
// this feature's fix notes. Not persisted server-side (no config.skins field
// for it), so it resets per browser/profile; that's an acceptable trade for
// keeping the ack purely a front-end concern.
const SKINS_ACK_KEY = "chud_skins_ack";
let skinsAck = false;
try { skinsAck = localStorage.getItem(SKINS_ACK_KEY) === "1"; } catch { /* localStorage unavailable (e.g. sandboxed preview) */ }
let skinsDownloadActive = false;
let skinsPollTimer = null;

// Favorite-skin picker state. The catalog (every champ + its skins) is loaded
// once, lazily, the first time the Skins page opens; the favorites map is the
// champ_id -> favorite skin_id set the backend persists.
let skinsCatalog = null;        // [{champ_id, champ_name, skins:[{skin_id,name,downloaded}]}]
let skinsFavorites = {};        // { "<champ_id>": <skin_id> }
let favSearch = "";
let favExpandedChamp = null;    // champ_id whose skins are shown

// In-client declutter/customization toggles (mirrors config.client; applied by
// the CHUD-Declutter Pengu plugin).
let clientCustom = null;
async function loadClientCustom() {
  try { clientCustom = (await invoke("skins_get_customization")) || {}; }
  catch { clientCustom = {}; }
}
const CC_OPTS = [
  ["hide_notif_badges", "Do Not Disturb", "Silence the attention-nag pips & badges (activity-center dot, call-to-action pips, Clash pip, rewards badge)"],
  ["hide_promos", "Hide promos & ads", "Deep-link promos and the Riot Discord banner"],
  ["hide_rp_topup", "Hide RP top-up", "The “buy RP” nudge on the currency bar"],
  ["hide_pass", "Hide battle pass", "The pass-progression widget on the home screen"],
  ["hide_missions", "Hide missions", "Missions button / progression widget"],
  ["hide_challenges", "Hide challenge banners", "Challenge and lobby banners"],
  ["hide_event_timers", "Hide event timers", "Event countdown in the game-select bar"],
  ["hide_loot", "Hide Loot tab", "Removes the Loot tab from the nav bar"],
  ["hide_store", "Hide Store tab", "Removes the Store tab from the nav bar"],
  ["hide_home_video", "Static home background", "Stops the animated play-screen video"],
];

async function loadSkinsState() {
  skinsState = (await invoke("skins_get_state")) || structuredClone(DEFAULT_SKINS_STATE);
  skinsCfg = {
    league_path: skinsState.leaguePath || "",
    injection_threshold_ms: skinsState.injectionThresholdMs,
    monitor_auto_resume_timeout_secs: skinsState.autoResumeSecs,
    auto_download_skins: !!skinsState.autoDownload,
  };
}

function skinsRiskStrip() {
  return `<div class="riskstrip">
    <div class="hazard">${ico("warning")}</div>
    <div class="grow">
      <div class="risk-title">Skin injection risk — read before enabling</div>
      <div class="risk-body">Injecting skins modifies local game files and briefly suspends the League client while the overlay loads. This is against Riot's Terms of Service and carries <b style="color:#ff92a4">account risk</b>. Use at your own discretion.</div>
    </div>
    <button class="btn danger sm" id="skinsAckBtn">I understand — unlock</button>
  </div>`;
}

function skinsStatusRow(label, ok, okText, badText) {
  const c = toneColor(ok ? "success" : "danger");
  return `<div class="diag-row"><span class="diag-k">${esc(label)}</span><span class="diag-v" style="color:${c}">${esc(ok ? okText : badText)}</span></div>`;
}

function skinsStatusCard() {
  const s = skinsState, d = s.diagnostics || {};
  return `<div class="glass"><div class="diag-card-title"><span style="display:inline-flex;width:16px;height:16px;margin-right:8px;vertical-align:-3px;color:var(--magenta-soft)">${ico("diagnostics")}</span>Status</div>
    <div class="diag-list">
      ${skinsStatusRow("Client linked", state.clientOnline, "Connected", "Offline")}
      ${skinsStatusRow("Bridge server", !!s.bridgePort, `Listening · 127.0.0.1:${s.bridgePort}`, "Not running")}
      ${skinsStatusRow("Pengu Loader", s.penguActive, "Active", "Inactive")}
      ${skinsStatusRow("Skins downloaded", s.skinsDownloaded, "Ready", "Not downloaded")}
      ${skinsStatusRow("Game hashes", s.hashesReady, "Ready", "Not downloaded")}
      ${skinsStatusRow("CSLOL tools", d.toolsAvailable, "Present", "Missing")}
      ${skinsStatusRow("cslol-dll.dll", d.dllValid, "Verified", "Missing / unrecognized")}
    </div></div>`;
}

function skinsSetupCard() {
  const s = skinsState;
  const lockedActivate = !skinsAck;
  return `<div class="glass set-card">
    <div class="set-card-title"><span class="ci">${ico("bolt")}</span>Setup</div>
    <div class="set-list">
      ${setField("League install path", "Folder containing League of Legends.exe — leave blank to auto-detect from the running client. Saved with Injection Settings below.",
        `<span class="set-input-wrap"><input class="set-input" id="skinsLeaguePath" type="text" style="width:230px;text-align:left" value="${esc(s.leaguePath || "")}" placeholder="auto-detect"></span>`)}
    </div>
    <div class="row" style="margin-top:6px;flex-wrap:wrap;gap:10px">
      <button class="btn sm primary" id="skinsDownloadBtn" ${skinsDownloadActive ? "disabled" : ""}>${skinsDownloadActive ? "Downloading…" : "Download skins"}</button>
      <button class="btn sm" id="skinsActivateBtn" ${lockedActivate ? "disabled" : ""}>Activate Pengu Loader</button>
    </div>
    ${lockedActivate ? `<div class="mod-lock"><span style="width:13px;height:13px;display:inline-flex">${ico("warning")}</span>Acknowledge the risk above to unlock activation.</div>` : ""}
    <div class="rc-bar" id="skinsProgressBar" style="margin-top:12px;${skinsDownloadActive ? "" : "display:none"}"><div class="rc-fill" id="skinsProgressFill" style="width:8%"></div></div>
    <div class="dim mono" id="skinsDownloadHint" style="font-size:11px;margin-top:6px;${skinsDownloadActive ? "" : "display:none"}"></div>
  </div>`;
}

const skinsNumInput = (key, unit, step) => `<span class="set-input-wrap"><input class="set-input" data-skey="${key}" type="number" step="${step || "any"}" value="${esc(skinsCfg[key])}">${unit ? `<span class="set-unit">${unit}</span>` : ""}</span>`;
const skinsToggleCtl = (key) => `<div class="tog ${skinsCfg[key] ? "on" : ""}" data-skey="${key}"><div class="knob"></div></div>`;

function skinsSettingsCard() {
  return `<div class="glass set-card">
    <div class="set-card-title"><span class="ci">${ico("settings")}</span>Injection Settings</div>
    <div class="set-list">
      ${setField("Injection threshold", "How close to the loadout deadline to inject", skinsNumInput("injection_threshold_ms", "ms", "1"))}
      ${setField("Auto-resume timeout", "Never leave the client suspended longer than this", skinsNumInput("monitor_auto_resume_timeout_secs", "s"))}
      ${setField("Auto-download skins", "Fetch new skins/hashes automatically on launch", skinsToggleCtl("auto_download_skins"))}
    </div>
    <div class="row" style="margin-top:4px"><button class="btn primary" id="skinsSaveCfg">Save settings</button><span class="dim mono" id="skinsSaveHint" style="font-size:11.5px"></span></div>
  </div>`;
}

function skinsPartyCardInner() {
  const p = skinsState.party || DEFAULT_SKINS_STATE.party;
  const peers = p.peers || [];
  const peerRows = peers.length
    ? peers.map((pr) => `<div class="diag-row"><span class="diag-k">${esc(pr.summoner_name || "Unknown")}${pr.in_lobby ? " · in lobby" : ""}</span><span class="diag-v">${pr.skin_selection ? `Champion ${esc(pr.skin_selection.champion_id)} · Skin ${esc(pr.skin_selection.skin_id)}` : "No selection yet"}</span></div>`).join("")
    : `<div class="dim" style="padding:10px 2px;font-size:12.5px">No peers connected yet. Share your token or paste a friend's below.</div>`;
  return `
    <div class="set-card-title"><span class="ci">${ico("profile")}</span>Party Mode</div>
    <div class="set-list">
      ${setField("Enable party mode", "Share your skin picks with your lobby in real time", `<div class="tog ${p.enabled ? "on" : ""}" id="skinsPartyToggle"><div class="knob"></div></div>`)}
    </div>
    ${p.enabled ? `
    <div class="set-field" style="align-items:flex-start">
      <div><div class="set-flabel">Your token</div><div class="set-fhint">Share this with a friend so they can join your party</div></div>
      <div class="set-control" style="gap:8px">
        <span class="set-input-wrap"><input class="set-input" id="skinsPartyToken" type="text" readonly style="width:220px;text-align:left" value="${esc(p.my_token || "")}"></span>
        <button class="btn sm" id="skinsCopyToken"><span style="width:14px;height:14px;display:inline-flex">${ico("copy")}</span>Copy</button>
      </div>
    </div>
    <div class="set-field" style="align-items:flex-start">
      <div><div class="set-flabel">Join a friend's party</div><div class="set-fhint">Paste their token</div></div>
      <div class="set-control" style="gap:8px">
        <span class="set-input-wrap"><input class="set-input" id="skinsPeerToken" type="text" style="width:220px;text-align:left" placeholder="CHUD:..."></span>
        <button class="btn sm primary" id="skinsAddPeer">Add peer</button>
      </div>
    </div>
    <div class="diag-list" style="margin-top:6px">${peerRows}</div>
    ` : ""}`;
}
function skinsPartyCard() {
  return `<div class="glass set-card" id="skinsPartyCard">${skinsPartyCardInner()}</div>`;
}

// ── Favorite skins ────────────────────────────────────────────────────────────
async function loadFavoritesData() {
  // Load the two independently — a failure in one must not blank the other.
  try {
    const cat = await invoke("skins_catalog");
    skinsCatalog = (cat && cat.champions) || [];
  } catch (e) {
    console.error("skins_catalog failed", e);
    skinsCatalog = [];
  }
  try {
    skinsFavorites = (await invoke("skins_get_favorites")) || {};
  } catch (e) {
    console.error("skins_get_favorites failed", e);
    skinsFavorites = {};
  }
}

function favSkinNameFor(champId, skinId) {
  const c = (skinsCatalog || []).find((x) => x.champ_id === champId);
  const s = c && c.skins.find((k) => k.skin_id === skinId);
  return s ? s.name : `Skin ${skinId}`;
}
// CommunityDragon art (CSP is null in this app, so remote images load fine).
// Champ square icon = champ id; skin tile = champ id + skin number (skinId % 1000).
const champIconUrl = (champId) => `https://raw.communitydragon.org/latest/plugins/rcp-be-lol-game-data/global/default/v1/champion-icons/${champId}.png`;
const skinTileUrl = (skinId) => `https://cdn.communitydragon.org/latest/champion/${Math.floor(skinId / 1000)}/tile/skin/${skinId % 1000}`;

function skinsFavoritesCardInner() {
  const title = `<div class="set-card-title"><span class="ci">${ico("skin")}</span>Favorite Skins</div>
    <div class="dim" style="font-size:12px;margin:-4px 0 10px">Pick a go-to skin per champion. Chud auto-applies it every game you play them — no in-client hovering. A manual pick in the client still wins for that game.</div>`;

  if (skinsCatalog === null) return `${title}<div class="dim" style="padding:8px 2px">Loading champions…</div>`;
  if (!skinsCatalog.length)
    return `${title}<div class="dim" style="padding:8px 2px">No skins downloaded yet. Download skins in Setup above, then favorites will appear here.</div>`;

  const q = favSearch.trim().toLowerCase();
  const champs = q ? skinsCatalog.filter((c) => c.champ_name.toLowerCase().includes(q)) : skinsCatalog;

  const rows = champs.slice(0, 400).map((c) => {
    const favId = skinsFavorites[String(c.champ_id)];
    const badge = favId != null
      ? `<span class="fav-badge">${esc(favSkinNameFor(c.champ_id, favId))}</span>`
      : `<span class="dim" style="font-size:11.5px">no favorite</span>`;
    const expanded = favExpandedChamp === c.champ_id;
    const caret = expanded ? "▾" : "▸";
    let sub = "";
    if (expanded) {
      // Skin thumbnails only render here — in the expanded per-champ list —
      // so the champ list itself stays light (no hundreds of images at once).
      const skinRows = c.skins.map((s) => {
        const isFav = favId === s.skin_id;
        const cls = `fav-skin${isFav ? " on" : ""}${s.downloaded ? "" : " undl"}`;
        const note = s.downloaded ? "" : `<span class="dim" style="font-size:10.5px;margin-left:auto">not downloaded</span>`;
        return `<div class="${cls}" data-fav-champ="${c.champ_id}" data-fav-skin="${s.skin_id}" data-dl="${s.downloaded ? 1 : 0}">
          <img class="fav-thumb" loading="lazy" src="${skinTileUrl(s.skin_id)}" alt="" onerror="this.classList.add('broken')">
          <span class="fav-dot">${isFav ? "●" : "○"}</span><span class="fav-sname">${esc(s.name)}</span>${note}</div>`;
      }).join("");
      sub = `<div class="fav-skinlist">${skinRows}</div>`;
    }
    return `<div class="fav-champ">
      <div class="fav-champ-head" data-fav-toggle="${c.champ_id}">
        <span class="fav-caret">${caret}</span>
        <img class="fav-cicon" loading="lazy" src="${champIconUrl(c.champ_id)}" alt="" onerror="this.classList.add('broken')">
        <span class="fav-cname">${esc(c.champ_name)}</span>
        <span class="fav-cbadge">${badge}</span>
        ${favId != null ? `<button class="btn sm ghost fav-clear" data-fav-clear="${c.champ_id}">Clear</button>` : ""}
      </div>${sub}</div>`;
  }).join("");

  return `${title}
    <span class="set-input-wrap" style="width:100%;margin-bottom:8px"><input class="set-input" id="favSearch" type="text" style="width:100%;text-align:left" placeholder="Search champion…" value="${esc(favSearch)}"></span>
    <div class="fav-list">${rows || `<div class="dim" style="padding:8px 2px">No champion matches “${esc(favSearch)}”.</div>`}</div>`;
}
function skinsFavoritesCard() {
  return `<div class="glass set-card" id="skinsFavCard">${skinsFavoritesCardInner()}</div>`;
}
function rerenderFavCard() {
  const card = document.getElementById("skinsFavCard");
  if (!card) return;
  card.innerHTML = skinsFavoritesCardInner();
  wireFavorites();
}
function wireFavorites() {
  const search = document.getElementById("favSearch");
  if (search) search.oninput = () => {
    favSearch = search.value;
    // Re-render list only, preserving the input's focus/caret.
    const list = document.querySelector("#skinsFavCard .fav-list");
    if (list) {
      const tmp = document.createElement("div");
      tmp.innerHTML = skinsFavoritesCardInner();
      const fresh = tmp.querySelector(".fav-list");
      if (fresh) { list.innerHTML = fresh.innerHTML; bindFavListEvents(); }
    }
  };
  bindFavListEvents();
}
function bindFavListEvents() {
  document.querySelectorAll("[data-fav-toggle]").forEach((el) => el.onclick = (e) => {
    if (e.target.closest("[data-fav-clear]")) return; // Clear button handled below
    const id = parseInt(el.dataset.favToggle, 10);
    favExpandedChamp = favExpandedChamp === id ? null : id;
    rerenderFavCard();
  });
  document.querySelectorAll("[data-fav-clear]").forEach((el) => el.onclick = async (e) => {
    e.stopPropagation();
    const champ = parseInt(el.dataset.favClear, 10);
    skinsFavorites = await invoke("skins_set_favorite", { champId: champ, skinId: null });
    rerenderFavCard();
    toast("Favorite cleared", "That champion will use whatever you pick in-client.", "neutral");
  });
  document.querySelectorAll("[data-fav-skin]").forEach((el) => el.onclick = async () => {
    if (el.dataset.dl !== "1") { toast("Not downloaded", "That skin isn't downloaded, so it can't be injected. Download skins in Setup.", "warning"); return; }
    const champ = parseInt(el.dataset.favChamp, 10), skin = parseInt(el.dataset.favSkin, 10);
    const already = skinsFavorites[String(champ)] === skin;
    skinsFavorites = await invoke("skins_set_favorite", { champId: champ, skinId: already ? null : skin });
    rerenderFavCard();
    toast(already ? "Favorite cleared" : "Favorite set", already ? "Back to in-client picks for this champ." : `${favSkinNameFor(champ, skin)} will auto-apply every game.`, "success");
  });
}

// ── Client declutter card ─────────────────────────────────────────────────────
function clientCustomCardInner() {
  const cc = clientCustom || {};
  const sub = cc.enabled
    ? CC_OPTS.map(([k, label, hint]) => setField(label, hint, `<div class="tog ${cc[k] ? "on" : ""}" data-cc="${k}"><div class="knob"></div></div>`)).join("")
    : `<div class="dim" style="padding:6px 2px;font-size:12px">Turn on to choose what to hide.</div>`;
  return `<div class="set-card-title"><span class="ci">${ico("settings")}</span>Client Declutter</div>
    <div class="dim" style="font-size:12px;margin:-4px 0 10px">Hide the League client's promos, store pushes, battle-pass and event clutter. Applies live — needs Pengu Loader active.</div>
    <div class="set-list">
      ${setField("Enable declutter", "Master switch for all the toggles below", `<div class="tog ${cc.enabled ? "on" : ""}" data-cc="enabled"><div class="knob"></div></div>`)}
      ${sub}
    </div>`;
}
function clientCustomCard() { return `<div class="glass set-card" id="ccCard">${clientCustomCardInner()}</div>`; }
function rerenderCcCard() { const c = document.getElementById("ccCard"); if (!c) return; c.innerHTML = clientCustomCardInner(); wireClientCustom(); }
function wireClientCustom() {
  document.querySelectorAll("#ccCard [data-cc]").forEach((t) => t.onclick = async () => {
    const k = t.dataset.cc;
    clientCustom = clientCustom || {};
    clientCustom[k] = !clientCustom[k];
    const fresh = await invoke("skins_set_customization", { customization: clientCustom });
    if (fresh && typeof fresh === "object") clientCustom = fresh;
    rerenderCcCard();
  });
}

async function renderSkins() {
  const p = document.getElementById("page");
  if (!skinsState) {
    p.innerHTML = `<div class="glass"><div class="muted">Loading skins state…</div></div>`;
    await loadSkinsState();
  }
  if (clientCustom === null) await loadClientCustom();
  // Load the favorites catalog BEFORE first paint (same as skinsState above), so
  // the champ list renders populated and A-Z on the first frame — no lazy
  // rerender race that could leave the default (unsearched) view blank.
  if (skinsCatalog === null) await loadFavoritesData();
  if (currentPage !== "skins") return; // navigated away while the await above was in flight
  p.innerHTML = `<div class="set-wrap">
    ${skinsAck ? "" : skinsRiskStrip()}
    <div class="glass" style="display:flex;align-items:center;gap:16px">
      <div class="grow">
        <div class="set-card-title" style="margin-bottom:2px"><span class="ci">${ico("skin")}</span>Skin Injection</div>
        <div class="dim" style="font-size:12px">Master switch — persists your preference; deeper gameflow gating is future work.</div>
      </div>
      <div class="tog ${skinsState.enabled ? "on" : ""} ${skinsAck ? "" : "disabled"}" ${skinsAck ? `data-skins-enable="1"` : ""}><div class="knob"></div></div>
    </div>
    <div class="diag-grid">${skinsStatusCard()}${skinsSetupCard()}</div>
    ${skinsSettingsCard()}
    ${skinsFavoritesCard()}
    ${clientCustomCard()}
    ${skinsPartyCard()}
  </div>`;
  wireSkins();
  wireFavorites();
  wireClientCustom();
  startSkinsPoll();
}

function wireSkins() {
  const ack = document.getElementById("skinsAckBtn");
  if (ack) ack.onclick = () => { skinsAck = true; try { localStorage.setItem(SKINS_ACK_KEY, "1"); } catch { /* ignore */ } renderSkins(); };

  const enableTog = document.querySelector("[data-skins-enable]");
  if (enableTog) enableTog.onclick = async () => {
    const fresh = await invoke("skins_set_enabled", { enabled: !skinsState.enabled });
    if (fresh) skinsState = fresh; else skinsState.enabled = !skinsState.enabled;
    renderSkins();
  };

  const pathInput = document.getElementById("skinsLeaguePath");
  if (pathInput) pathInput.onchange = () => { skinsCfg.league_path = pathInput.value; };

  const dlBtn = document.getElementById("skinsDownloadBtn");
  if (dlBtn) dlBtn.onclick = onSkinsDownload;
  const actBtn = document.getElementById("skinsActivateBtn");
  if (actBtn) actBtn.onclick = onSkinsActivate;
  const saveBtn = document.getElementById("skinsSaveCfg");
  if (saveBtn) saveBtn.onclick = onSkinsSaveSettings;

  document.querySelectorAll("#page [data-skey]").forEach((el) => {
    if (el.tagName === "INPUT") {
      el.onchange = () => {
        const v = parseFloat(el.value);
        if (Number.isFinite(v)) skinsCfg[el.dataset.skey] = v; else el.value = skinsCfg[el.dataset.skey];
      };
    } else {
      el.onclick = () => { skinsCfg[el.dataset.skey] = !skinsCfg[el.dataset.skey]; el.classList.toggle("on", skinsCfg[el.dataset.skey]); };
    }
  });

  wirePartyControls();
}

function wirePartyControls() {
  const tog = document.getElementById("skinsPartyToggle");
  if (tog) tog.onclick = onSkinsPartyToggle;
  const copyBtn = document.getElementById("skinsCopyToken");
  if (copyBtn) copyBtn.onclick = async () => {
    try {
      await navigator.clipboard.writeText((skinsState.party && skinsState.party.my_token) || "");
      copyBtn.querySelector("svg") ? copyBtn.lastChild.textContent = "Copied!" : (copyBtn.textContent = "Copied!");
    } catch { toast("Copy failed", "", "danger"); }
  };
  const addBtn = document.getElementById("skinsAddPeer");
  if (addBtn) addBtn.onclick = onSkinsAddPeer;
}

function refreshPartyCard() {
  const card = document.getElementById("skinsPartyCard");
  if (card) { card.innerHTML = skinsPartyCardInner(); wirePartyControls(); }
}

async function onSkinsDownload() {
  if (skinsDownloadActive) return;
  skinsDownloadActive = true;
  if (currentPage === "skins") renderSkins();
  if (!TAURI) {
    setTimeout(() => {
      skinsDownloadActive = false; skinsState.skinsDownloaded = true; skinsState.hashesReady = true;
      toast("Skins downloaded", "(preview mode — no backend)", "success");
      if (currentPage === "skins") renderSkins();
    }, 900);
    return;
  }
  await invoke("skins_download", { force: false });
  // Real progress/completion arrive via skins-download-progress/-done events.
}

function onSkinsDownloadProgress(payload) {
  if (!payload) return;
  const pct = payload.total ? Math.min(100, Math.round((payload.done / payload.total) * 100)) : null;
  const fill = document.getElementById("skinsProgressFill");
  if (fill) fill.style.width = (pct ?? 8) + "%";
  const hint = document.getElementById("skinsDownloadHint");
  if (hint) hint.textContent = `${payload.phase === "hashes" ? "Downloading game hashes" : "Downloading skins"}${pct !== null ? ` · ${pct}%` : "…"}`;
}
async function onSkinsDownloadDone(payload) {
  skinsDownloadActive = false;
  if (payload && payload.ok) toast("Skins ready", "Skins and hashes are up to date.", "success");
  else toast("Download failed", (payload && payload.error) || "Unknown error", "danger");
  await loadSkinsState();
  if (currentPage === "skins") renderSkins();
}

// `skins_activate_pengu`/`skins_party_*` return `Result<_, String>` on the
// Rust side — shared.js's `invoke` swallows the error text (by design, see
// shared.js), so these call `TAURI.invoke` directly to surface the real
// reason in the toast.
async function onSkinsActivate() {
  if (!skinsAck) return;
  const btn = document.getElementById("skinsActivateBtn");
  if (btn) btn.disabled = true;
  try {
    if (TAURI) {
      await TAURI.invoke("skins_activate_pengu");
      toast("Pengu Loader activated", "League will restart if it's running.", "success");
    } else {
      toast("Pengu Loader activated", "(preview mode — no backend)", "success");
    }
  } catch (e) {
    toast("Activation failed", String(e || "Could not activate Pengu Loader."), "danger");
  }
  if (btn) btn.disabled = false;
  await loadSkinsState();
  if (currentPage === "skins") renderSkins();
}

async function onSkinsSaveSettings() {
  const fresh = await invoke("skins_save_settings", { settings: skinsCfg });
  if (fresh) {
    skinsState = fresh;
    skinsCfg = {
      league_path: skinsState.leaguePath || "",
      injection_threshold_ms: skinsState.injectionThresholdMs,
      monitor_auto_resume_timeout_secs: skinsState.autoResumeSecs,
      auto_download_skins: !!skinsState.autoDownload,
    };
  }
  const h = document.getElementById("skinsSaveHint");
  if (h) { h.textContent = "Saved ✓"; setTimeout(() => { if (h) h.textContent = ""; }, 1800); }
  toast("Skins settings saved", "Configuration written to disk.", "success");
}

async function onSkinsPartyToggle() {
  const enabling = !(skinsState.party && skinsState.party.enabled);
  try {
    if (TAURI) {
      skinsState.party = enabling ? await TAURI.invoke("skins_party_enable") : await TAURI.invoke("skins_party_disable");
    } else {
      skinsState.party = { enabled: enabling, my_token: enabling ? "CHUD:preview-token" : null, my_summoner_id: null, my_summoner_name: "Unknown", peers: [] };
    }
    toast(enabling ? "Party mode enabled" : "Party mode disabled", "", enabling ? "success" : "info");
  } catch (e) {
    toast("Party mode error", String(e || "Failed to toggle party mode."), "danger");
  }
  if (currentPage === "skins") refreshPartyCard();
}

async function onSkinsAddPeer() {
  const input = document.getElementById("skinsPeerToken");
  const value = input ? input.value.trim() : "";
  if (!value) return;
  try {
    if (TAURI) {
      skinsState.party = await TAURI.invoke("skins_party_add_peer", { token: value });
    }
    toast("Peer added", "", "success");
  } catch (e) {
    toast("Could not add peer", String(e || "Invalid token."), "danger");
  }
  if (input) input.value = "";
  if (currentPage === "skins") refreshPartyCard();
}

// While the Skins page is open, poll the party state (no push event reaches
// this webview — see the file-header IPC contract note).
function startSkinsPoll() {
  stopSkinsPoll();
  skinsPollTimer = setInterval(async () => {
    if (currentPage !== "skins") { stopSkinsPoll(); return; }
    // Party mode is opt-in — no need to poll its state when it's off (the
    // enable/add-peer handlers refresh it directly). Saves an IPC call every 3s.
    if (!skinsState || !skinsState.party || !skinsState.party.enabled) return;
    const fresh = await invoke("skins_party_get_state");
    if (fresh) { skinsState.party = fresh; refreshPartyCard(); }
  }, 3000);
}
function stopSkinsPoll() { if (skinsPollTimer) { clearInterval(skinsPollTimer); skinsPollTimer = null; } }

// ── Toasts ──────────────────────────────────────────────────────────────────
function toast(title, message, tone = "info") {
  const wrap = document.getElementById("toasts");
  const el = document.createElement("div");
  el.className = `toast ${tone === "success" ? "success" : tone === "danger" ? "danger" : tone === "warning" ? "warning" : ""}`;
  el.innerHTML = `<div class="toast-bar"></div><div><div class="toast-title">${esc(title)}</div>${message ? `<div class="toast-msg">${esc(message)}</div>` : ""}</div>`;
  wrap.appendChild(el);
  setTimeout(() => { el.classList.add("out"); setTimeout(() => el.remove(), 300); }, 3600);
}

// ── Ready-check overlay ───────────────────────────────────────────────────────
let rcShown = false;
function syncReadyCheck() {
  const phase = state.phase;
  if (phase === "ReadyCheck" && !rcShown) {
    rcShown = true;
    const ov = document.createElement("div");
    ov.className = "rc-overlay"; ov.id = "rcOverlay";
    ov.innerHTML = `<div class="rc-card"><div class="core" style="margin:0 auto 6px"><div class="core-ring"></div><div class="core-inner"></div></div>
      <div class="rc-title">Match Found</div><div class="rc-sub">Auto-Accept is handling the ready check…</div>
      <div class="rc-bar"><div class="rc-fill" id="rcFill" style="width:8%"></div></div></div>`;
    document.body.appendChild(ov);
    let w = 8; const iv = setInterval(() => { w = Math.min(96, w + 12); const f = document.getElementById("rcFill"); if (f) f.style.width = w + "%"; }, 220);
    ov._iv = iv;
  } else if (phase !== "ReadyCheck" && rcShown) {
    rcShown = false;
    const ov = document.getElementById("rcOverlay");
    if (ov) { clearInterval(ov._iv); ov.remove(); }
  }
}

// ── Page routing + actions ───────────────────────────────────────────────────
function renderPage() {
  const page = document.getElementById("page");
  if (currentPage === "dashboard") { page.innerHTML = dashboardHtml(); wireDash(); }
  else if (currentPage === "settings") { renderSettings(); }
  else if (currentPage === "profile") { window.renderProfile?.(page); }
  else if (currentPage === "skins") { renderSkins(); }
  else if (currentPage === "library") { window.renderLibrary?.(page); }
  else if (currentPage === "activity") { renderActivity(); }
  else if (currentPage === "diagnostics") { renderDiagnostics(); }
  else { page.innerHTML = `<div class="glass"><div class="muted">${esc(NAV.find((n) => n.page === currentPage)?.label || "")} — coming soon.</div></div>`; }
}
window.ChudNavTo = (p) => navTo(p);
function navTo(page) {
  if (currentPage === "skins" && page !== "skins") stopSkinsPoll();
  currentPage = page; renderNav(); renderTop(); renderPage();
}

function onStateChanged() {
  renderTop(); syncReadyCheck();
  if (currentPage === "dashboard") patchDashboard();
}
const setVal = (id, v) => { const el = document.getElementById(id); if (el) el.textContent = String(v); };
function patchDashboard() {
  const page = document.getElementById("page");
  const hero = page.querySelector(".hero");
  const riskShown = !!page.querySelector(".riskstrip");
  if (!hero || riskShown !== !state.injectionAck) { page.innerHTML = dashboardHtml(); wireDash(); return; }
  const aa = state.tools.find((x) => x.id === "auto_accept") || {};
  const armed = !!aa.running;
  hero.classList.toggle("armed", armed);
  const core = hero.querySelector(".core"); if (core) core.classList.toggle("core-idle", !armed);
  const cs = hero.querySelector(".core-state"); if (cs) cs.textContent = armed ? (state.clientOnline ? "ARMED" : "STANDBY") : "IDLE";
  const ht = hero.querySelector(".hero-title"); if (ht) ht.textContent = armed ? (state.clientOnline ? "Queue watcher is live" : "Waiting for the client…") : "Auto-Accept is idle";
  const s = state.summary;
  setVal("lc-session", s.sessionMatches); setVal("lc-total", s.totalMatches); setVal("lc-uptime", s.uptime); setVal("lc-active", state.activeToolCount);
  const sh = document.getElementById("stopAllHero"); if (sh) sh.style.display = state.activeToolCount > 0 ? "" : "none";
  if (currentModulesSig() !== modulesSig) {
    const modules = page.querySelector(".modules"); if (modules) modules.innerHTML = state.tools.map(modCard).join("");
    wireDash();
  }
}

async function onToggle(id) {
  await invoke("toggle_tool", { id });
  if (TAURI) return;
  const t = state.tools.find((x) => x.id === id);
  if (t) { t.running = !t.running; t.statusText = t.running ? "ARMED" : "READY"; t.statusTone = t.running ? "success" : "ice"; t.primaryActionText = t.running ? "Stop Tool" : t.primaryActionText;
    toast(`${t.title} ${t.running ? "armed" : "stopped"}`, "", t.running ? "success" : "info"); }
  state.activeToolCount = state.tools.filter((x) => x.running).length;
  onStateChanged();
}
async function onAck() {
  await invoke("set_injection_ack", { accepted: true });
  if (TAURI) return;
  state.injectionAck = true; toast("Risk acknowledged", "Injection tools unlocked.", "warning"); onStateChanged();
}
async function onStartAll() {
  for (const t of state.tools) {
    const blocked = (!t.safe && !state.injectionAck) || (t.requiresAdmin && !state.adminReady);
    if (!t.running && !blocked) { await invoke("toggle_tool", { id: t.id }); if (!TAURI) { t.running = true; t.statusText = "ARMED"; t.statusTone = "success"; } }
  }
  if (TAURI) return;
  state.activeToolCount = state.tools.filter((x) => x.running).length; onStateChanged();
}
async function onStopAll() {
  await invoke("stop_all");
  if (TAURI) return;
  state.tools.forEach((t) => { if (t.running) { t.running = false; t.statusText = "READY"; t.statusTone = "ice"; } });
  state.activeToolCount = 0; toast("All tools stopped", "", "info"); onStateChanged();
}

// ── In-app updater ────────────────────────────────────────────────────────────
// A themed "update available" pill in the top bar. Click it (or the pill) to
// download+install on your own schedule — no manual installer, no forced
// mid-game downtime. The backend kills stale mod-tools processes first so the
// install never fails on a locked file, then relaunches when done.
let pendingUpdate = null;
let updating = false;

function showUpdatePill(info) {
  if (!info || !info.version) return;
  pendingUpdate = info;
  const pill = document.getElementById("updatePill");
  if (!pill) return;
  pill.innerHTML = `${ico("refresh")}<span>Update ${esc(info.version)}</span>`;
  pill.style.display = "";
  pill.onclick = () => openUpdateOverlay();
}

function openUpdateOverlay() {
  if (!pendingUpdate) return;
  const ov = document.getElementById("updateOverlay");
  document.getElementById("updateTitle").textContent = `Update to v${pendingUpdate.version}`;
  const notes = (pendingUpdate.notes || "").trim();
  document.getElementById("updateSub").textContent = notes || "A new version of Chud is ready to install.";
  document.getElementById("updateActions").style.display = "";
  document.getElementById("updateBar").style.width = "0%";
  document.getElementById("updatePct").textContent = "";
  ov.style.display = "";
  document.getElementById("updateLater").onclick = () => { if (!updating) ov.style.display = "none"; };
  document.getElementById("updateNow").onclick = () => startUpdate();
}

async function startUpdate() {
  if (updating) return;
  updating = true;
  document.getElementById("updateActions").style.display = "none";
  document.getElementById("updateTitle").textContent = "Updating Chud…";
  document.getElementById("updateSub").textContent = "Chud will restart automatically when it's done.";
  document.getElementById("updatePct").textContent = "Preparing…";
  if (!TAURI) { // browser preview: simulate
    let p = 0; const t = setInterval(() => { p += 8; onUpdateProgress({ downloaded: p, total: 100 }); if (p >= 100) { clearInterval(t); document.getElementById("updatePct").textContent = "Restarting…"; } }, 120);
    return;
  }
  const res = await invoke("updater_install");
  // On success the backend relaunches the app, so we never really get here;
  // a returned value means the install errored before restart.
  if (res === null) {
    document.getElementById("updateTitle").textContent = "Update failed";
    document.getElementById("updateSub").textContent = "Couldn't install the update. You can try again, or grab the latest installer from GitHub.";
    document.getElementById("updateActions").style.display = "";
    document.getElementById("updateNow").textContent = "Retry";
    updating = false;
  }
}

function onUpdateProgress(p) {
  if (!p) return;
  const total = Number(p.total) || 0, done = Number(p.downloaded) || 0;
  const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : null;
  document.getElementById("updateBar").style.width = (pct == null ? 8 : pct) + "%";
  document.getElementById("updatePct").textContent =
    pct == null ? `${(done / 1048576).toFixed(1)} MB` : (pct >= 100 ? "Restarting…" : `${pct}%`);
}

// ── Boot ─────────────────────────────────────────────────────────────────────
async function boot() {
  await loadGlyphs();
  await loadLibraryState();
  const real = await invoke("get_state");
  if (real) state = real;
  renderNav(); renderTop(); renderPage(); syncReadyCheck();
  pushActivity("Launcher ready", "ice", "dashboard");
  pushActivity(state.clientOnline ? "League client connected" : "Waiting for League client", state.clientOnline ? "success" : "neutral", "ping");
  const ev = window.__TAURI__?.event;
  if (ev) {
    ev.listen("state-changed", (e) => { if (e && e.payload) { const prev = state; state = e.payload; recordActivity(prev, state); onStateChanged(); } });
    ev.listen("notification", (e) => { const n = e?.payload; if (n) toast(n.title || "Chud", n.message || n.msg || "", n.tone || "info"); });
    ev.listen("skins-download-progress", (e) => { if (currentPage === "skins") onSkinsDownloadProgress(e?.payload); });
    ev.listen("skins-download-done", (e) => { onSkinsDownloadDone(e?.payload); });
    ev.listen("update-available", (e) => showUpdatePill(e?.payload));
    ev.listen("update-progress", (e) => onUpdateProgress(e?.payload));
  }
  // Belt-and-suspenders: the startup `update-available` event can fire before
  // this webview attaches its listener, so ask directly too.
  invoke("updater_check").then((info) => { if (info) showUpdatePill(info); });
}
boot();
