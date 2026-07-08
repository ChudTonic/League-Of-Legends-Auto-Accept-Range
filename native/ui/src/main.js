// ============================================================
// Chud — Neon Glass front-end.
// Talks to the Rust core over the SAME Tauri IPC contract as the
// legacy Hextech UI (do not change command/event names — see README):
//   commands: get_state, toggle_tool, stop_all, set_injection_ack,
//             request_admin, exit_app, get_config, save_config,
//             get_diagnostics, get_profile, capture_debug_frame
//   events:   state-changed  (payload = full state snapshot)
//             notification   (optional: {title, message, tone})
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
    { id: "camera_assist", title: "Camera Assist", safe: false, requiresAdmin: true, running: false,
      subtitle: "Recenters the camera on your champion when you drift; auto-disabled in ranked.",
      statusText: "READY", statusTone: "ice", metricLabel: "Recenter", metricValue: "pulse",
      runtimeCopy: "Auto-recenter the camera while playing unlocked.", primaryActionText: "Launch Camera Assist" },
  ],
};

let state = structuredClone(MOCK_STATE);
let currentPage = "dashboard";

async function invoke(cmd, args) {
  if (TAURI) { try { return await TAURI.invoke(cmd, args); } catch (e) { console.warn(cmd, e); } }
  return null;
}

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

const TONE = { success: "#33e0a0", running: "#33e0a0", ice: "#35e4ff", info: "#35e4ff", gold: "#7ceeff", warning: "#e6a23c", danger: "#ff5470", neutral: "#6b6b96" };
const toneColor = (t) => TONE[t] || TONE.neutral;
const chip = (text, tone) => { const c = toneColor(tone); return `<span class="chip" style="color:${c};border-color:${c}55;background:${c}18">${esc(text)}</span>`; };

// ── Glyphs (inline SVG so they inherit currentColor) ────────────────────────
const GLYPH_NAMES = ["dashboard", "profile", "settings", "activity", "diagnostics", "power", "bolt", "crosshair", "camera", "lock", "warning", "ping", "refresh", "copy", "chevron", "shield"];
const GLYPHS = {};
async function loadGlyphs() {
  await Promise.all(GLYPH_NAMES.map(async (n) => {
    try { GLYPHS[n] = await (await fetch(`icons/${n}.svg`)).text(); } catch { GLYPHS[n] = ""; }
  }));
}
const ico = (n) => GLYPHS[n] || "";
const glyphForTool = (id) => (id === "auto_accept" ? "bolt" : id === "auto_range" ? "crosshair" : "camera");

const NAV = [
  { page: "dashboard", label: "Dashboard", glyph: "dashboard" },
  { page: "profile", label: "Profile", glyph: "profile" },
  { page: "activity", label: "Activity", glyph: "activity" },
  { page: "settings", label: "Settings", glyph: "settings" },
  { page: "diagnostics", label: "Diagnostics", glyph: "diagnostics" },
];

// ── Chrome ──────────────────────────────────────────────────────────────────
function renderNav() {
  document.getElementById("nav").innerHTML = NAV.map((n) => `
    <div class="nav-item ${currentPage === n.page ? "active" : ""}" data-page="${n.page}">
      <span class="nav-ico">${ico(n.glyph)}</span><span>${n.label}</span>
    </div>`).join("");
  document.querySelectorAll(".nav-item").forEach((el) => (el.onclick = () => navTo(el.dataset.page)));
}
function renderTop() {
  const n = state.activeToolCount, col = n > 0 ? "#33e0a0" : "#6b6b96";
  const online = state.clientOnline, oc = online ? "#33e0a0" : "#6b6b96";
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
      <div class="risk-body">Auto-Range &amp; Camera Assist send synthetic input / read the screen. Vanguard can detect this and <b style="color:#ff92a4">ban the account</b>. They are auto-disabled in ranked games; the app operates openly (no evasion). Use at your own discretion.</div>
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
const currentModulesSig = () => JSON.stringify([state.tools, state.injectionAck, state.adminReady]);

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
  camera: { recenter_mode: "pulse", camera_hold_key: "space", center_radius_px: 260, vision_interval: 0.08 },
  safety: { block_in_ranked: true, injection_ack: false },
};
let cfg = null;
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
  ${card("Camera Assist", "camera", [
    setField("Recenter mode", "Pulse the key or hold", segMode("camera", "recenter_mode", ["pulse", "hold"])),
    setField("Camera key", "Recenter key", keyInput("camera", "camera_hold_key")),
    setField("Center radius", "Allowed drift before recentering", numInput("camera", "center_radius_px", "px")),
    setField("Vision interval", "Screen scan cadence", numInput("camera", "vision_interval", "s")),
  ].join(""))}
  ${card("Safety", "shield", [
    setField("Block in ranked", "Disable injection tools in ranked games", togCtl("safety", "block_in_ranked")),
    setField("Risk acknowledged", "Vanguard ban risk accepted", togCtl("safety", "injection_ack")),
  ].join(""))}
  <div class="row"><button class="btn primary" id="saveCfg">Save settings</button><span class="dim mono" id="saveHint" style="font-size:11.5px"></span></div>
  </div>`;
}
async function renderSettings() {
  if (!cfg) await loadConfig();
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
  tools: { autoAccept: false, autoRange: false, cameraAssist: false, injectionBlocked: false, injectionAck: false },
  hotkeys: { autoRange: "none (always-on while armed)", cameraAssist: "none (always-on while armed)" },
  config: { rangeHoldKey: "c", cameraHoldKey: "space", recenterMode: "pulse", blockInRanked: true, checkInterval: 1.0 },
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
  ${card("Tools", [diagRow("Auto-Accept", d.tools.autoAccept ? "Running" : "Idle", boolTone(d.tools.autoAccept)), diagRow("Auto-Range", d.tools.autoRange ? "Running" : "Idle", boolTone(d.tools.autoRange)), diagRow("Camera Assist", d.tools.cameraAssist ? "Running" : "Idle", boolTone(d.tools.cameraAssist)), diagRow("Ranked kill-switch", d.tools.injectionBlocked ? "ENGAGED" : "Clear", d.tools.injectionBlocked ? "danger" : "ice"), diagRow("Risk acknowledged", yn(d.tools.injectionAck), d.tools.injectionAck ? "success" : "warning")])}
  ${card("Configuration", [diagRow("Range hold key", String(d.config.rangeHoldKey).toUpperCase()), diagRow("Camera hold key", String(d.config.cameraHoldKey).toUpperCase()), diagRow("Recenter mode", d.config.recenterMode), diagRow("Block in ranked", yn(d.config.blockInRanked), boolTone(d.config.blockInRanked)), diagRow("Check interval", `${d.config.checkInterval}s`), diagRow("Auto-Range hotkey", d.hotkeys.autoRange), diagRow("Camera hotkey", d.hotkeys.cameraAssist)])}
  ${card("Paths", [diagRow("Config file", d.paths.config || "—"), diagRow("Data folder", d.paths.data || "—")])}
  </div>
  <div class="diag-actions">
    <button class="btn sm primary" id="diagRefresh"><span style="width:14px;height:14px;display:inline-flex">${ico("refresh")}</span>Refresh</button>
    <button class="btn sm" id="diagCopy"><span style="width:14px;height:14px;display:inline-flex">${ico("copy")}</span>Copy report</button>
    <button class="btn sm" id="diagCapture"><span style="width:14px;height:14px;display:inline-flex">${ico("camera")}</span>Capture camera debug frame</button>
  </div>
  <div id="diagCaptureOut" class="diag-capout dim"></div>`;
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
    setTimeout(() => renderDiagnostics(), 1400);
  };
  document.getElementById("diagCapture").onclick = async () => {
    const out = document.getElementById("diagCaptureOut");
    out.textContent = "Capturing primary monitor…";
    const r = await invoke("capture_debug_frame");
    if (!r) { out.textContent = "Capture unavailable in browser preview."; return; }
    if (r.ok) out.innerHTML = `Captured ${r.frame?.[0]}×${r.frame?.[1]} · ${r.candidateCount} health-bar candidate(s).<br>Saved to: ${esc(r.path)}`;
    else out.textContent = `Capture failed: ${esc(r.error || "unknown error")}`;
  };
}

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
  else if (currentPage === "activity") { renderActivity(); }
  else if (currentPage === "diagnostics") { renderDiagnostics(); }
  else { page.innerHTML = `<div class="glass"><div class="muted">${esc(NAV.find((n) => n.page === currentPage)?.label || "")} — coming soon.</div></div>`; }
}
function navTo(page) { currentPage = page; renderNav(); renderTop(); renderPage(); }

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

// ── Boot ─────────────────────────────────────────────────────────────────────
async function boot() {
  await loadGlyphs();
  const real = await invoke("get_state");
  if (real) state = real;
  renderNav(); renderTop(); renderPage(); syncReadyCheck();
  pushActivity("Launcher ready", "ice", "dashboard");
  pushActivity(state.clientOnline ? "League client connected" : "Waiting for League client", state.clientOnline ? "success" : "neutral", "ping");
  const ev = window.__TAURI__?.event;
  if (ev) {
    ev.listen("state-changed", (e) => { if (e && e.payload) { const prev = state; state = e.payload; recordActivity(prev, state); onStateChanged(); } });
    ev.listen("notification", (e) => { const n = e?.payload; if (n) toast(n.title || "Chud", n.message || n.msg || "", n.tone || "info"); });
  }
}
boot();
