// Chud — Announcer Studio. Build a custom League announcer from your own
// audio (drop a file or record your mic) with no external tools. The webview
// decodes any audio to mono 16-bit 44.1kHz PCM and hands raw samples to the
// Rust builder, which wraps them as Wwise-PCM wems and installs the pack.
(function () {
  "use strict";
  const S = window.ChudShared;
  const esc = S.esc;
  const inv = S.invoke;

  const RATE = 44100;
  const st = { slots: null, name: "My Announcer", audio: {}, includeMilestones: false, building: false };
  let root = null;
  let ac = null; // shared AudioContext

  function actx() { return ac || (ac = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: RATE })); }

  // Decode any audio bytes -> mono Int16 PCM at 44.1k, trim silence, cap 5s.
  async function toPcm(arrayBuf) {
    const buf = await actx().decodeAudioData(arrayBuf.slice(0));
    const n = buf.length;
    const chs = buf.numberOfChannels;
    const mono = new Float32Array(n);
    for (let c = 0; c < chs; c++) { const d = buf.getChannelData(c); for (let i = 0; i < n; i++) mono[i] += d[i] / chs; }
    // resample to 44.1k if needed
    let samples = mono, sr = buf.sampleRate;
    if (sr !== RATE) {
      const outLen = Math.round(n * RATE / sr);
      const rs = new Float32Array(outLen);
      for (let i = 0; i < outLen; i++) { const t = i * sr / RATE; const i0 = Math.floor(t), f = t - i0; rs[i] = (mono[i0] || 0) * (1 - f) + (mono[i0 + 1] || 0) * f; }
      samples = rs;
    }
    // trim silence (thr 0.012), 30ms pad, 5s cap
    const thr = 0.012, pad = Math.round(RATE * 0.03);
    let lo = 0, hi = samples.length;
    while (lo < hi && Math.abs(samples[lo]) < thr) lo++;
    while (hi > lo && Math.abs(samples[hi - 1]) < thr) hi--;
    lo = Math.max(0, lo - pad); hi = Math.min(samples.length, hi + pad);
    samples = samples.subarray(lo, Math.min(hi, lo + RATE * 5));
    const pcm = new Int16Array(samples.length);
    let peak = 0;
    for (let i = 0; i < samples.length; i++) { const v = Math.max(-1, Math.min(1, samples[i])); pcm[i] = v * 32767; if (Math.abs(v) > peak) peak = Math.abs(v); }
    return { pcm, dur: samples.length / RATE, peak };
  }

  function b64(int16) {
    const bytes = new Uint8Array(int16.buffer);
    let s = "", CH = 0x8000;
    for (let i = 0; i < bytes.length; i += CH) s += String.fromCharCode.apply(null, bytes.subarray(i, i + CH));
    return btoa(s);
  }

  async function assign(key, arrayBuf) {
    try {
      const { pcm, dur, peak } = await toPcm(arrayBuf);
      if (peak < 0.02 || dur < 0.15) { toast("That clip is silent", "Try a louder or longer sound.", "warning"); return; }
      st.audio[key] = { pcm, dur, b64: b64(pcm) };
      paint();
    } catch (e) { toast("Couldn't read that audio", String(e).slice(0, 120), "danger"); }
  }

  function preview(key) {
    const a = st.audio[key]; if (!a) return;
    const c = actx(); const b = c.createBuffer(1, a.pcm.length, RATE); const d = b.getChannelData(0);
    for (let i = 0; i < a.pcm.length; i++) d[i] = a.pcm[i] / 32767;
    const src = c.createBufferSource(); src.buffer = b; src.connect(c.destination); src.start();
  }

  // Mic recording via MediaRecorder -> decode like a dropped file.
  let rec = null, recKey = null, chunks = [];
  async function toggleRec(key) {
    if (rec && rec.state === "recording") { rec.stop(); return; }
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      chunks = []; recKey = key; rec = new MediaRecorder(stream);
      rec.ondataavailable = (e) => chunks.push(e.data);
      rec.onstop = async () => {
        stream.getTracks().forEach((t) => t.stop());
        const blob = new Blob(chunks, { type: chunks[0] && chunks[0].type || "audio/webm" });
        await assign(recKey, await blob.arrayBuffer());
        rec = null; recKey = null; paint();
      };
      rec.start(); paint();
    } catch (e) { toast("Mic unavailable", "Allow microphone access to record.", "warning"); }
  }

  const toast = (t, m, tone) => window.ChudToast ? window.ChudToast(t, m, tone) : 0;

  function slotRow(sl) {
    const a = st.audio[sl.key];
    const has = !!a;
    const recording = rec && rec.state === "recording" && recKey === sl.key;
    return `<div class="stu-slot ${has ? "filled" : ""} ${sl.milestone ? "milestone" : ""}" data-slotrow="${sl.key}">
      <div class="stu-slot-main">
        <div class="stu-slot-label">${esc(sl.label)}${sl.milestone ? `<span class="stu-ms" title="The game preloads this line and only plays official (Vorbis) audio for it — custom audio here needs Wwise. Left as the normal announcer by default.">milestone</span>` : ""}</div>
        <div class="stu-slot-state">${has ? `▸ ${a.dur.toFixed(1)}s assigned` : "empty — drop audio or record"}</div>
      </div>
      <div class="stu-slot-acts">
        ${has ? `<button class="stu-mini" data-play="${sl.key}">▶</button>` : ""}
        <button class="stu-mini ${recording ? "rec" : ""}" data-rec="${sl.key}">${recording ? "◼ stop" : "● rec"}</button>
        <label class="stu-mini file"><input type="file" accept="audio/*" data-file="${sl.key}" hidden>＋ file</label>
        ${has ? `<button class="stu-mini" data-clear="${sl.key}">✕</button>` : ""}
      </div>
    </div>`;
  }

  function paint() {
    if (!root) return;
    if (st.slots === null) { root.innerHTML = `<div class="stu-wrap"><div class="muted">Loading Studio…</div></div>`; return; }
    const cats = [];
    const byCat = {};
    st.slots.forEach((s) => { if (!byCat[s.category]) { byCat[s.category] = []; cats.push(s.category); } byCat[s.category].push(s); });
    const filled = Object.keys(st.audio).length;
    const groups = cats.map((c) => `<div class="stu-cat"><div class="stu-cat-h">${esc(c)}</div>${byCat[c].map(slotRow).join("")}</div>`).join("");
    root.innerHTML = `<div class="stu-wrap">
      <div class="stu-head">
        <div><div class="section-label">ANNOUNCER STUDIO</div><div class="stu-tag">Build a custom League announcer from your own audio — drop a clip or record your mic for any line, then install it straight to your announcer wheel.</div></div>
      </div>
      <div class="stu-bar">
        <input class="stu-name" id="stuName" value="${esc(st.name)}" placeholder="Pack name" maxlength="40">
        <span class="stu-count">${filled} line${filled === 1 ? "" : "s"} assigned</span>
        <button class="btn primary" id="stuBuild" ${filled === 0 || st.building ? "disabled" : ""}>${st.building ? "Building…" : "Build & Install"}</button>
      </div>
      <div class="stu-hint">Milestone lines (First Blood, Ace, Victory, Penta…) stay the official announcer — the game only accepts its own audio format for those. Everything else is all you.</div>
      ${groups}
    </div>`;
    wire();
  }

  function wire() {
    const nm = document.getElementById("stuName"); if (nm) nm.oninput = () => { st.name = nm.value; };
    root.querySelectorAll("[data-play]").forEach((b) => b.onclick = () => preview(b.dataset.play));
    root.querySelectorAll("[data-rec]").forEach((b) => b.onclick = () => toggleRec(b.dataset.rec));
    root.querySelectorAll("[data-clear]").forEach((b) => b.onclick = () => { delete st.audio[b.dataset.clear]; paint(); });
    root.querySelectorAll("[data-file]").forEach((inp) => inp.onchange = async (e) => { const f = e.target.files[0]; if (f) await assign(inp.dataset.file, await f.arrayBuffer()); });
    // drag & drop onto a slot row
    root.querySelectorAll("[data-slotrow]").forEach((row) => {
      row.ondragover = (e) => { e.preventDefault(); row.classList.add("drop"); };
      row.ondragleave = () => row.classList.remove("drop");
      row.ondrop = async (e) => { e.preventDefault(); row.classList.remove("drop"); const f = e.dataTransfer.files[0]; if (f && f.type.startsWith("audio")) await assign(row.dataset.slotrow, await f.arrayBuffer()); };
    });
    const build = document.getElementById("stuBuild"); if (build) build.onclick = doBuild;
  }

  async function doBuild() {
    const slots = Object.keys(st.audio).map((key) => ({ key, pcm_base64: st.audio[key].b64, sample_rate: RATE }));
    if (!slots.length) return;
    st.building = true; paint();
    try {
      const r = await inv("announcer_studio_build", { name: st.name || "My Announcer", slots, includeMilestones: st.includeMilestones });
      if (r && r.ok) {
        toast("Announcer built!", `${r.slots_filled} line(s) installed as "${st.name}". Pick it from the announcer wheel in champ select.`, "success");
      } else {
        toast("Build failed", (r && r.error) || "Unknown error", "danger");
      }
    } catch (e) { toast("Build failed", String(e).slice(0, 140), "danger"); }
    st.building = false; paint();
  }

  window.renderStudio = async function (el) {
    root = el;
    paint();
    if (st.slots === null) {
      try { st.slots = await inv("announcer_studio_slots"); }
      catch (e) { st.slots = []; }
      paint();
    }
  };
})();
