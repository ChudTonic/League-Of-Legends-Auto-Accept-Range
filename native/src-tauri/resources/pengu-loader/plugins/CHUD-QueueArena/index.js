/**
 * @name CHUD-QueueArena
 * @author Chud Team
 * @description A skillshot-dodge minigame that pops up in the League client while you're in queue, so the dead time isn't dead. Auto-pauses when a match is found. Toggle from the Chud app. Pure client-side — never touches the game.
 * @link https://github.com/ChudTonic/League-Of-Legends-Auto-Accept-Range
 */
(function chudQueueArena() {
  "use strict";
  let bridgePort = 50000;
  let enabled = true;          // from Chud /client-customization -> queue_arena
  let dismissed = false;       // user closed it this queue (resets when queue ends)
  let inQueue = false;

  // ---- diagnostics: log boot + phase reads to the Chud file log ----
  let ws = null; const wsQ = [];
  function wsFlush() { if (ws && ws.readyState === 1) while (wsQ.length) ws.send(JSON.stringify(wsQ.shift())); }
  function report(event, data) { wsQ.push({ type: "chroma-log", source: "CHUD-QueueArena", event, data: data || {}, timestamp: Date.now() }); wsFlush(); }
  function wsConnect() { try { ws = new WebSocket(`ws://127.0.0.1:${bridgePort}`); ws.onopen = () => { report("online", { enabled }); wsFlush(); }; ws.onclose = () => setTimeout(wsConnect, 3000); ws.onerror = () => {}; } catch (e) {} }

  // ---- bridge (port discovery + config) ----
  async function discoverPort() {
    for (let p = 50000; p <= 50010; p++) {
      try { const r = await fetch(`http://127.0.0.1:${p}/bridge-port`, { cache: "no-store" }); if (r.ok) { bridgePort = parseInt((await r.text()).trim(), 10) || p; return; } }
      catch (e) {}
    }
  }
  async function fetchEnabled() {
    try { const r = await fetch(`http://127.0.0.1:${bridgePort}/client-customization`, { cache: "no-store" }); if (r.ok) { const c = await r.json(); return c && c.queue_arena !== false; } }
    catch (e) {}
    return enabled;
  }

  // ---- gameflow phase from the Chud bridge (the app polls the LCU with auth,
  // so this is reliable and reachable from the plugin, unlike a direct LCU fetch). ----
  async function fetchPhase() {
    try { const r = await fetch(`http://127.0.0.1:${bridgePort}/phase`, { cache: "no-store" }); if (r.ok) { const d = await r.json(); return (d && d.phase) || null; } }
    catch (e) {}
    return null;
  }

  // ---- styles ----
  const CSS = `
    #chud-qa { position: fixed; right: 18px; bottom: 18px; z-index: 2147483000; width: 384px;
      font-family: "Spiegel","LoL Body",Arial,sans-serif; color: #eaf2ff; user-select: none;
      background: linear-gradient(180deg,#0c1424,#080e1c); border: 1px solid rgba(255,255,255,.10);
      border-radius: 14px; box-shadow: 0 18px 50px rgba(0,0,0,.6); overflow: hidden; display: none; }
    #chud-qa.show { display: block; animation: chudQaIn .22s ease; }
    @keyframes chudQaIn { from { transform: translateY(14px); opacity: 0; } to { transform: none; opacity: 1; } }
    #chud-qa .qa-head { display: flex; align-items: center; gap: 8px; padding: 9px 11px; cursor: default; border-bottom: 1px solid rgba(255,255,255,.08); }
    #chud-qa .qa-t { font-weight: 800; font-size: 13px; letter-spacing: .01em;
      background: linear-gradient(90deg,#ff3d9a,#7ceeff); -webkit-background-clip: text; background-clip: text; -webkit-text-fill-color: transparent; }
    #chud-qa .qa-sc { margin-left: auto; font-family: ui-monospace,Consolas,monospace; font-size: 11px; color: #7ceeff; }
    #chud-qa .qa-x { width: 22px; height: 22px; border-radius: 6px; display: grid; place-items: center; cursor: pointer; color: #8a97b8; font-size: 15px; }
    #chud-qa .qa-x:hover { background: rgba(255,255,255,.08); color: #fff; }
    #chud-qa .qa-body { position: relative; }
    #chud-qa canvas { display: block; width: 100%; height: 216px; cursor: none; background: radial-gradient(120% 120% at 50% 0%,#0c1424,#0a1020); }
    #chud-qa .qa-ov { position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 9px;
      background: rgba(6,10,20,.72); text-align: center; padding: 14px; }
    #chud-qa .qa-ov.hide { display: none; }
    #chud-qa .qa-ov-t { font-weight: 800; font-size: 17px; text-wrap: balance; }
    #chud-qa .qa-ov-s { color: #8a97b8; font-size: 11.5px; max-width: 30ch; }
    #chud-qa .qa-ov-s b { color: #eaf2ff; }
    #chud-qa .qa-big { font-family: ui-monospace,Consolas,monospace; font-size: 26px; font-weight: 800; color: #7ceeff; }
    #chud-qa .qa-btn { font-family: ui-monospace,Consolas,monospace; font-size: 12px; font-weight: 700; color: #08101f;
      background: linear-gradient(90deg,#ff3d9a,#7ceeff); border: none; border-radius: 8px; padding: 8px 18px; cursor: pointer; }
    #chud-qa .qa-btn:active { transform: scale(.96); }
    #chud-qa.min .qa-body { display: none; }
    #chud-qa .qa-collapse { color: #8a97b8; font-size: 11px; cursor: pointer; }
    #chud-qa .qa-collapse:hover { color: #fff; }
  `;

  // ---- DOM ----
  let root, cv, ctx, ovEl, ovT, ovBig, ovS, startBtn, scEl, minBtn;
  function build() {
    const style = document.createElement("style"); style.textContent = CSS; document.head.appendChild(style);
    root = document.createElement("div"); root.id = "chud-qa";
    root.innerHTML = `
      <div class="qa-head">
        <span class="qa-t">⚔ Queue Arena</span>
        <span class="qa-sc" data-sc>0.0s</span>
        <span class="qa-collapse" data-min>–</span>
        <span class="qa-x" data-x title="Close">✕</span>
      </div>
      <div class="qa-body">
        <canvas></canvas>
        <div class="qa-ov" data-ov>
          <div class="qa-ov-t" data-ovt>Skillshot Dodge</div>
          <div class="qa-big" data-ovbig style="display:none">0.0s</div>
          <div class="qa-ov-s" data-ovs>Move with your <b>mouse</b>. Dodge the skillshots while you wait.</div>
          <button class="qa-btn" data-start>Play</button>
        </div>
      </div>`;
    document.body.appendChild(root);
    cv = root.querySelector("canvas"); ctx = cv.getContext("2d");
    ovEl = root.querySelector("[data-ov]"); ovT = root.querySelector("[data-ovt]"); ovBig = root.querySelector("[data-ovbig]");
    ovS = root.querySelector("[data-ovs]"); startBtn = root.querySelector("[data-start]"); scEl = root.querySelector("[data-sc]"); minBtn = root.querySelector("[data-min]");
    startBtn.addEventListener("click", () => game.start());
    root.querySelector("[data-x]").addEventListener("click", () => { dismissed = true; hide(); });
    minBtn.addEventListener("click", () => { root.classList.toggle("min"); minBtn.textContent = root.classList.contains("min") ? "+" : "–"; if (root.classList.contains("min")) game.pause(); });
  }

  function show() { if (!root) build(); root.classList.add("show"); }
  function hide() { if (root) { root.classList.remove("show"); game.stop(); } }

  // ---- game engine ----
  const game = (() => {
    let W = 384, H = 216, DPR = 1, running = false, raf = 0, tPrev = 0;
    let elapsed = 0, dodges = 0, shake = 0, best = 0;
    let proj = [], beams = [], circles = [], parts = [], floats = [], spawnT = 0;
    const mouse = { x: 192, y: 108, has: false };
    try { best = parseFloat(localStorage.getItem("chud_qa_best") || "0") || 0; } catch (e) {}
    const rnd = (a, b) => a + Math.random() * (b - a);
    function size() { const r = cv.getBoundingClientRect(); DPR = Math.min(2, window.devicePixelRatio || 1); W = r.width || 384; H = r.height || 216; cv.width = W * DPR; cv.height = H * DPR; ctx.setTransform(DPR, 0, 0, DPR, 0, 0); }
    cv.addEventListener("mousemove", (e) => { const r = cv.getBoundingClientRect(); mouse.x = e.clientX - r.left; mouse.y = e.clientY - r.top; mouse.has = true; });
    cv.addEventListener("mouseleave", () => { mouse.has = false; });
    const player = { x: 192, y: 108, r: 11, hit: 4 };
    const edge = () => { const s = (Math.random() * 4) | 0; if (s === 0) return { x: rnd(0, W), y: -16 }; if (s === 1) return { x: W + 16, y: rnd(0, H) }; if (s === 2) return { x: rnd(0, W), y: H + 16 }; return { x: -16, y: rnd(0, H) }; };
    const diff = () => 1 + elapsed / 20;
    function reset() { player.x = mouse.x = W / 2; player.y = mouse.y = H / 2; proj = []; beams = []; circles = []; parts = []; floats = []; elapsed = 0; dodges = 0; spawnT = .6; shake = 0; }
    function sLine() { const p = edge(), tx = rnd(W * .15, W * .85), ty = rnd(H * .15, H * .85), a = Math.atan2(ty - p.y, tx - p.x), sp = rnd(150, 200) * diff(); proj.push({ t: "h", x: p.x, y: p.y, vx: Math.cos(a) * sp, vy: Math.sin(a) * sp, r: 4, len: 22, col: "#ffcf5c", near: 0 }); }
    function sOrb() { const p = edge(), a = Math.atan2(player.y - p.y, player.x - p.x), sp = rnd(74, 96) * diff(); proj.push({ t: "o", x: p.x, y: p.y, vx: Math.cos(a) * sp, vy: Math.sin(a) * sp, r: 8, col: "#b06bff", home: 1.0, near: 0 }); }
    function sBeam() { const hz = Math.random() < .5; beams.push({ hz, pos: hz ? rnd(H * .12, H * .88) : rnd(W * .12, W * .88), w: 18, warn: .9, fire: 0, ph: "w" }); }
    function sCirc() { circles.push({ x: rnd(W * .15, W * .85), y: rnd(H * .15, H * .85), r: rnd(30, 48), warn: .95, boom: 0, ph: "w" }); }
    function wave() { const d = diff(), r = Math.random(); if (r < .4) sLine(); else if (r < .62) sOrb(); else if (r < .82) sBeam(); else sCirc(); if (d > 1.6 && Math.random() < .35) sLine(); spawnT = Math.max(.26, rnd(.7, 1.05) / d); }
    function die() { running = false; shake = 12; for (let i = 0; i < 26; i++) { const a = rnd(0, 6.283), s = rnd(30, 190); parts.push({ x: player.x, y: player.y, vx: Math.cos(a) * s, vy: Math.sin(a) * s, life: 1, col: Math.random() < .5 ? "#35e4ff" : "#ff8ac4" }); } const nb = elapsed > best; if (nb) { best = elapsed; try { localStorage.setItem("chud_qa_best", String(best)); } catch (e) {} } setTimeout(() => { ovEl.classList.remove("hide"); ovT.textContent = "Defeated"; ovBig.style.display = ""; ovBig.textContent = elapsed.toFixed(1) + "s"; ovS.innerHTML = `Dodged <b>${dodges}</b>. ${nb ? "New best!" : "Best <b>" + best.toFixed(1) + "s</b>"}`; startBtn.textContent = "Run it back"; }, 380); }
    function upd(dt) {
      elapsed += dt; scEl.textContent = elapsed.toFixed(1) + "s";
      const gx = mouse.has ? mouse.x : player.x, gy = mouse.has ? mouse.y : player.y;
      player.x += (gx - player.x) * Math.min(1, dt * 16); player.y += (gy - player.y) * Math.min(1, dt * 16);
      player.x = Math.max(player.r, Math.min(W - player.r, player.x)); player.y = Math.max(player.r, Math.min(H - player.r, player.y));
      spawnT -= dt; if (spawnT <= 0) wave();
      for (const p of proj) {
        if (p.t === "o" && p.home > 0) { const a = Math.atan2(player.y - p.y, player.x - p.x), c = Math.atan2(p.vy, p.vx); let da = a - c; while (da > Math.PI) da -= 6.283; while (da < -Math.PI) da += 6.283; const na = c + da * Math.min(1, dt * 1.6), sp = Math.hypot(p.vx, p.vy); p.vx = Math.cos(na) * sp; p.vy = Math.sin(na) * sp; p.home -= dt; }
        p.x += p.vx * dt; p.y += p.vy * dt; const d = Math.hypot(p.x - player.x, p.y - player.y);
        if (!p.near && d < player.r + p.r + 8 && d > player.hit + p.r) { p.near = 1; dodges++; floats.push({ x: player.x, y: player.y - 14, life: .6, txt: "+1" }); }
        if (d < player.hit + p.r) return die();
      }
      proj = proj.filter((p) => p.x > -50 && p.x < W + 50 && p.y > -50 && p.y < H + 50);
      for (const b of beams) { if (b.ph === "w") { b.warn -= dt; if (b.warn <= 0) { b.ph = "f"; b.fire = .3; } } else if (b.ph === "f") { b.fire -= dt; const on = b.hz ? Math.abs(player.y - b.pos) < b.w / 2 + player.hit : Math.abs(player.x - b.pos) < b.w / 2 + player.hit; if (on) return die(); if (b.fire <= 0) { b.ph = "d"; dodges++; } } }
      beams = beams.filter((b) => b.ph !== "d");
      for (const c of circles) { if (c.ph === "w") { c.warn -= dt; if (c.warn <= 0) { c.ph = "b"; c.boom = .26; } } else if (c.ph === "b") { c.boom -= dt; if (Math.hypot(player.x - c.x, player.y - c.y) < c.r + player.hit) return die(); if (c.boom <= 0) { c.ph = "d"; dodges++; } } }
      circles = circles.filter((c) => c.ph !== "d");
      for (const f of floats) { f.y -= 28 * dt; f.life -= dt; } floats = floats.filter((f) => f.life > 0);
      for (const p of parts) { p.x += p.vx * dt; p.y += p.vy * dt; p.vx *= .93; p.vy *= .93; p.life -= dt * 1.5; } parts = parts.filter((p) => p.life > 0);
      if (shake > 0) shake = Math.max(0, shake - dt * 50);
    }
    function draw() {
      ctx.clearRect(0, 0, W, H); ctx.save(); if (shake > 0) ctx.translate(rnd(-shake, shake) * .4, rnd(-shake, shake) * .4);
      ctx.strokeStyle = "rgba(255,255,255,.04)"; ctx.lineWidth = 1; for (let x = 0; x <= W; x += 32) { ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke(); } for (let y = 0; y <= H; y += 32) { ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(W, y); ctx.stroke(); }
      for (const b of beams) { const f = b.ph === "f"; ctx.save(); if (b.hz) { ctx.translate(0, b.pos); if (f) { ctx.fillStyle = "rgba(255,61,154,.9)"; ctx.shadowColor = "#ff3d9a"; ctx.shadowBlur = 18; ctx.fillRect(0, -b.w / 2, W, b.w); } else { ctx.fillStyle = "rgba(255,61,154,.15)"; ctx.fillRect(0, -b.w / 2, W, b.w); ctx.strokeStyle = "rgba(255,138,196,.8)"; ctx.setLineDash([6, 6]); ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(W, 0); ctx.stroke(); } } else { ctx.translate(b.pos, 0); if (f) { ctx.fillStyle = "rgba(255,61,154,.9)"; ctx.shadowColor = "#ff3d9a"; ctx.shadowBlur = 18; ctx.fillRect(-b.w / 2, 0, b.w, H); } else { ctx.fillStyle = "rgba(255,61,154,.15)"; ctx.fillRect(-b.w / 2, 0, b.w, H); ctx.strokeStyle = "rgba(255,138,196,.8)"; ctx.setLineDash([6, 6]); ctx.beginPath(); ctx.moveTo(0, 0); ctx.lineTo(0, H); ctx.stroke(); } } ctx.restore(); }
      for (const c of circles) { ctx.beginPath(); ctx.arc(c.x, c.y, c.r, 0, 6.283); if (c.ph === "b") { ctx.fillStyle = "rgba(255,146,69,.85)"; ctx.shadowColor = "#ff9245"; ctx.shadowBlur = 20; ctx.fill(); ctx.shadowBlur = 0; } else { ctx.fillStyle = "rgba(255,146,69,.12)"; ctx.fill(); ctx.lineWidth = 2; ctx.strokeStyle = "rgba(255,146,69,.85)"; ctx.setLineDash([5, 5]); ctx.stroke(); ctx.setLineDash([]); ctx.beginPath(); ctx.arc(c.x, c.y, c.r * (1 - c.warn / .95), 0, 6.283); ctx.strokeStyle = "rgba(255,207,92,.9)"; ctx.stroke(); } }
      for (const p of proj) { ctx.shadowColor = p.col; ctx.shadowBlur = 10; if (p.t === "h") { const a = Math.atan2(p.vy, p.vx); ctx.strokeStyle = p.col; ctx.lineWidth = 4; ctx.lineCap = "round"; ctx.beginPath(); ctx.moveTo(p.x, p.y); ctx.lineTo(p.x - Math.cos(a) * p.len, p.y - Math.sin(a) * p.len); ctx.stroke(); } else { ctx.fillStyle = p.col; ctx.beginPath(); ctx.arc(p.x, p.y, p.r, 0, 6.283); ctx.fill(); } ctx.shadowBlur = 0; }
      if (running || parts.length) { ctx.save(); ctx.shadowColor = "#35e4ff"; ctx.shadowBlur = 14; ctx.strokeStyle = "rgba(53,228,255,.55)"; ctx.lineWidth = 2; ctx.beginPath(); ctx.arc(player.x, player.y, player.r, 0, 6.283); ctx.stroke(); const g = ctx.createRadialGradient(player.x, player.y, 1, player.x, player.y, player.r); g.addColorStop(0, "#eafcff"); g.addColorStop(.5, "#7ceeff"); g.addColorStop(1, "rgba(53,228,255,.2)"); ctx.fillStyle = g; ctx.beginPath(); ctx.arc(player.x, player.y, player.r - 2, 0, 6.283); ctx.fill(); ctx.shadowBlur = 0; ctx.fillStyle = "#060a14"; ctx.beginPath(); ctx.arc(player.x, player.y, player.hit, 0, 6.283); ctx.fill(); ctx.fillStyle = "#ff3d9a"; ctx.beginPath(); ctx.arc(player.x, player.y, player.hit - 1.5, 0, 6.283); ctx.fill(); ctx.restore(); }
      for (const p of parts) { ctx.globalAlpha = Math.max(0, p.life); ctx.fillStyle = p.col; ctx.beginPath(); ctx.arc(p.x, p.y, 2.5, 0, 6.283); ctx.fill(); } ctx.globalAlpha = 1;
      for (const f of floats) { ctx.globalAlpha = Math.max(0, f.life / .6); ctx.fillStyle = "#33e0a0"; ctx.font = "700 13px ui-monospace,Consolas,monospace"; ctx.textAlign = "center"; ctx.fillText(f.txt, f.x, f.y); } ctx.globalAlpha = 1;
      ctx.restore();
    }
    function loop(t) { const dt = Math.min(.04, (t - tPrev) / 1000); tPrev = t; if (running) upd(dt); else { for (const p of parts) { p.x += p.vx * dt; p.y += p.vy * dt; p.vx *= .93; p.vy *= .93; p.life -= dt * 1.5; } parts = parts.filter((p) => p.life > 0); if (shake > 0) shake = Math.max(0, shake - dt * 50); } draw(); if (running || parts.length || shake > 0) raf = requestAnimationFrame(loop); else raf = 0; }
    return {
      start() { size(); reset(); ovEl.classList.add("hide"); running = true; tPrev = performance.now(); if (!raf) raf = requestAnimationFrame(loop); },
      pause() { running = false; },
      stop() { running = false; if (raf) cancelAnimationFrame(raf); raf = 0; ovEl.classList.remove("hide"); ovT.textContent = "Skillshot Dodge"; ovBig.style.display = "none"; ovS.innerHTML = "Move with your <b>mouse</b>. Dodge the skillshots while you wait."; startBtn.textContent = "Play"; scEl.textContent = "0.0s"; },
    };
  })();

  // ---- phase-driven show/hide ----
  let lastPhase = " ";
  async function tick() {
    const phase = await fetchPhase();
    if (phase !== lastPhase) { lastPhase = phase; report("phase", { phase, enabled }); }
    const nowQueue = phase === "Matchmaking";
    if (nowQueue && !inQueue) { dismissed = false; }        // fresh queue → allow it again
    inQueue = nowQueue;
    if (phase === "ReadyCheck") { game.pause(); return; }    // match found → don't obscure the accept
    if (nowQueue && enabled && !dismissed) { show(); }
    else { hide(); }
  }

  (async function boot() {
    await discoverPort();
    wsConnect();
    enabled = await fetchEnabled();
    report("boot", { bridgePort, enabled });
    setInterval(async () => { enabled = await fetchEnabled(); }, 4000); // pick up Chud toggle
    setInterval(tick, 1500);
    tick();
  })();
})();
