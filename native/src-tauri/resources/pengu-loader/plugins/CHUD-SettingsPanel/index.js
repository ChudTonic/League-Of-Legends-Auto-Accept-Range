/**
 * @name Chud-SettingsPanel
 * @author Chud Team
 * @description Settings panel for Chud
 * @link https://github.com/ChudTonic/League-Of-Legends-Auto-Accept-Range
 */
(function initSettingsPanel() {
  const LOG_PREFIX = "[Chud-SettingsPanel]";
  const GITHUB_URL = "https://github.com/ChudTonic/League-Of-Legends-Auto-Accept-Range";
  const DISCORD_URL = "https://discord.gg/a2QTg7btaT";

  const PANEL_ID = "chud-settings-panel";
  const FLYOUT_ID = "chud-settings-flyout";

  /**
   * Escape HTML special characters to prevent XSS (CWE-79)
   * @param {string} str - String to escape
   * @returns {string} Escaped string safe for innerHTML
   */
  function escapeHtml(str) {
    if (typeof str !== 'string') return str;
    return str
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#039;');
  }

  let bridge = null;

  function waitForBridge() {
    return new Promise((resolve, reject) => {
      const timeout = 10000;
      const interval = 50;
      let elapsed = 0;
      const check = () => {
        if (window.__chudBridge) return resolve(window.__chudBridge);
        elapsed += interval;
        if (elapsed >= timeout) return reject(new Error("Bridge not available"));
        setTimeout(check, interval);
      };
      check();
    });
  }

  let settingsPanel = null;
  let currentSettings = {
    threshold: 0.5,
    monitorAutoResumeTimeout: 25,
    autostart: false,
    gamePath: "",
    gamePathValid: false,
    version: "",
  };
  let pathValidationTimeout = null;

  function getCSSRules() {
    return `
    @keyframes chudWarningPulse {
      0%   { filter: drop-shadow(0 0 0 rgba(255, 70, 70, 0.00)) drop-shadow(0 0 0 rgba(255, 70, 70, 0.00)); opacity: 0.95; }
      50%  { filter: drop-shadow(0 0 6px rgba(255, 70, 70, 0.90)) drop-shadow(0 0 12px rgba(255, 70, 70, 0.45)); opacity: 1.00; }
      100% { filter: drop-shadow(0 0 0 rgba(255, 70, 70, 0.00)) drop-shadow(0 0 0 rgba(255, 70, 70, 0.00)); opacity: 0.95; }
    }

    .chud-warning-glow {
      animation: chudWarningPulse 1.35s ease-in-out infinite;
      will-change: filter, opacity;
    }

    @font-face {
      font-family: "JetBrains Mono";
      src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/JetBrainsMono-Regular.ttf") format("truetype");
      font-weight: normal;
      font-style: normal;
      font-display: swap;
    }
    
    @font-face {
      font-family: "JetBrains Mono";
      src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/JetBrainsMono-Bold.ttf") format("truetype");
      font-weight: bold;
      font-style: normal;
      font-display: swap;
    }

    /* Diagnostics / Troubleshooting dialog scrollbar (avoid native Windows scrollbar look) */
    #chud-diagnostics-body {
      scrollbar-width: thin;
      scrollbar-color: #0d1420 rgba(0, 0, 0, 0.25);
    }

    #chud-diagnostics-body::-webkit-scrollbar {
      width: 10px;
    }

    #chud-diagnostics-body::-webkit-scrollbar:horizontal {
      display: none !important;
      height: 0 !important;
    }

    #chud-diagnostics-body::-webkit-scrollbar-track {
      background: rgba(0, 0, 0, 0.25);
      border-left: 1px solid rgba(70, 55, 20, 0.55);
    }

    #chud-diagnostics-body::-webkit-scrollbar-thumb {
      background: linear-gradient(to bottom, rgba(200, 155, 60, 0.22), rgba(70, 55, 20, 0.85));
      border: 1px solid rgba(70, 55, 20, 0.95);
      border-radius: 10px;
      min-height: 28px;
    }

    #chud-diagnostics-body::-webkit-scrollbar-thumb:hover {
      background: linear-gradient(to bottom, rgba(200, 155, 60, 0.32), rgba(70, 55, 20, 0.95));
    }

    #chud-diagnostics-body::-webkit-scrollbar-corner {
      background: transparent;
    }
    
    #${PANEL_ID} {
      position: fixed;
      top: 0;
      left: 0;
      width: 100%;
      height: 100%;
      z-index: 10000;
      pointer-events: none;
    }
    /* Prevent padding/borders from pushing children past the card width (the
       cause of the horizontal scrollbar). */
    #${PANEL_ID}, #${PANEL_ID} * { box-sizing: border-box; }

    /* ===== Neon Glass design tokens (see design_handoff_settings_redesign/README.md) ===== */
    #${PANEL_ID} {
      --magenta: #ff3d9a;
      --magenta-soft: #ff8ac4;
      --cyan: #35e4ff;
      --cyan-soft: #7ceeff;
      --neon-grad: linear-gradient(135deg, #ff3d9a, #35e4ff);
      --neon-grad-h: linear-gradient(90deg, #ff3d9a, #35e4ff);
      --accent-glow: rgba(255, 61, 154, .5);
      --bg-base: #0a0a1f;
      --bg-stage: #0b0b22;
      --glass-border: rgba(255, 255, 255, .10);
      --bg-inset: rgba(6, 6, 20, .55);
      --text-primary: #f2f2ff;
      --text-secondary: #9a9ac8;
      --text-muted: #6b6b96;
      --text-dark: #0a0a1f;
      --red: #ff5470;
      --green: #33e0a0;
      --amber: #e6a23c;
      --r-sm: 8px;
      --r-md: 12px;
      --r-lg: 18px;
    }

    /* Fonts served by the local bridge (see native/pengu-loader plugin resources). */
    @font-face { font-family: "Cinzel"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/cinzel-600.woff2") format("woff2"); font-weight: 600; font-style: normal; font-display: swap; }
    @font-face { font-family: "Cinzel"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/cinzel-700.woff2") format("woff2"); font-weight: 700; font-style: normal; font-display: swap; }
    @font-face { font-family: "Marcellus"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/marcellus-400.woff2") format("woff2"); font-weight: 400; font-style: normal; font-display: swap; }
    @font-face { font-family: "Barlow"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/barlow-400.woff2") format("woff2"); font-weight: 400; font-style: normal; font-display: swap; }
    @font-face { font-family: "Barlow"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/barlow-500.woff2") format("woff2"); font-weight: 500; font-style: normal; font-display: swap; }
    @font-face { font-family: "Space Mono"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/spacemono-400.woff2") format("woff2"); font-weight: 400; font-style: normal; font-display: swap; }
    @font-face { font-family: "Space Mono"; src: url("http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/fonts/spacemono-700.woff2") format("woff2"); font-weight: 700; font-style: normal; font-display: swap; }

    @keyframes hxFloat {
      0% { transform: translateY(0) scale(.7); opacity: 0; }
      15% { opacity: .85; }
      70% { opacity: .5; }
      100% { transform: translateY(-150px) scale(1); opacity: 0; }
    }
    @keyframes hxPulse { 0%, 100% { opacity: .28; } 50% { opacity: .5; } }
    @keyframes hxReveal { from { opacity: 0; transform: translateY(-6px); } to { opacity: 1; transform: translateY(0); } }
    @keyframes hxToast { from { opacity: 0; transform: translate(-50%, 10px); } to { opacity: 1; transform: translate(-50%, 0); } }
    @keyframes hxHandle {
      0%, 100% { box-shadow: 0 0 7px var(--accent-glow, rgba(255,61,154,.5)), 0 1px 2px rgba(0,0,0,.6); }
      50% { box-shadow: 0 0 16px var(--accent-glow, rgba(255,61,154,.9)), 0 1px 2px rgba(0,0,0,.6); }
    }
    @keyframes hxShimmer {
      0% { transform: translateX(-160%) skewX(-18deg); }
      55%, 100% { transform: translateX(320%) skewX(-18deg); }
    }

    /* ---- Backdrop: full-screen animated midnight scrim behind the centered modal ---- */
    #${PANEL_ID} .chud-backdrop {
      position: fixed;
      inset: 0;
      width: 100%;
      height: 100%;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 44px 20px;
      overflow: hidden;
      /* In-client: a semi-transparent, blurred scrim so the League client
         stays visible (dimmed) behind the centered card — not an opaque
         full-screen takeover. The neon glows sit on top of the scrim. */
      background:
        radial-gradient(1100px 640px at 84% -12%, rgba(255,61,154,.16), transparent 60%),
        radial-gradient(1000px 620px at 4% 112%, rgba(53,228,255,.14), transparent 58%),
        linear-gradient(160deg, rgba(10,10,31,.60), rgba(16,15,46,.66) 60%, rgba(11,11,34,.70));
      backdrop-filter: blur(6px);
      -webkit-backdrop-filter: blur(6px);
      pointer-events: all;
    }
    #${PANEL_ID} .chud-decor {
      position: absolute;
      inset: 0;
      z-index: 0;
      overflow: hidden;
      pointer-events: none;
    }
    #${PANEL_ID} .chud-grid-layer {
      position: absolute;
      inset: 0;
      opacity: .5;
      background-image:
        repeating-linear-gradient(60deg, transparent 0 43px, rgba(255,255,255,.05) 43px 44px),
        repeating-linear-gradient(-60deg, transparent 0 43px, rgba(255,255,255,.05) 43px 44px);
      -webkit-mask-image: radial-gradient(circle at 50% 42%, #000 0%, transparent 68%);
      mask-image: radial-gradient(circle at 50% 42%, #000 0%, transparent 68%);
    }
    #${PANEL_ID} .chud-glow-magenta {
      position: absolute;
      width: 640px;
      height: 640px;
      left: 50%;
      top: 31%;
      transform: translate(-50%, -50%);
      border-radius: 50%;
      background: radial-gradient(circle, var(--accent-glow, rgba(255,61,154,.5)) 0%, transparent 62%);
      filter: blur(34px);
      animation: hxPulse 7s ease-in-out infinite;
    }
    #${PANEL_ID} .chud-glow-cyan {
      position: absolute;
      width: 560px;
      height: 560px;
      left: 50%;
      top: 66%;
      transform: translate(-50%, -50%);
      border-radius: 50%;
      background: radial-gradient(circle, rgba(53,228,255,.42) 0%, transparent 64%);
      filter: blur(46px);
      opacity: .55;
    }
    #${PANEL_ID} .chud-mote {
      position: absolute;
      border-radius: 50%;
      pointer-events: none;
      animation-name: hxFloat;
      animation-timing-function: ease-out;
      animation-iteration-count: infinite;
    }

    /* ---- Modal card shell ---- */
    #${FLYOUT_ID} {
      position: relative;
      z-index: 2;
      margin: auto;
      width: min(600px, 94vw);
      /* Cap at a comfortable card height (centered with margin on tall
         screens) and only scroll vertically inside the card — never the
         viewport, and never horizontally. */
      max-height: min(840px, calc(100vh - 56px));
      overflow-y: auto;
      overflow-x: hidden;
      scrollbar-width: thin;
      scrollbar-color: #35e4ff rgba(255,255,255,.06);
      background: linear-gradient(180deg, rgba(18,16,44,.82) 0%, rgba(12,11,33,.9) 100%);
      border: 1px solid rgba(255,255,255,.12);
      backdrop-filter: blur(18px);
      -webkit-backdrop-filter: blur(18px);
      box-shadow: 0 0 0 1px rgba(0,0,0,.4), 0 40px 90px -30px rgba(0,0,0,.9), 0 0 70px -18px var(--accent-glow, rgba(255,61,154,.42));
      color: #f2f2ff;
      pointer-events: all;
    }
    /* Neon-themed scrollbar to match the menu (replaces the grey Windows one). */
    #${FLYOUT_ID}::-webkit-scrollbar { width: 10px; }
    #${FLYOUT_ID}::-webkit-scrollbar-track { background: rgba(255,255,255,.04); border-radius: 8px; margin: 8px 0; }
    #${FLYOUT_ID}::-webkit-scrollbar-thumb { background: linear-gradient(180deg, #ff3d9a, #35e4ff); border-radius: 8px; border: 2px solid rgba(11,11,34,.92); }
    #${FLYOUT_ID}::-webkit-scrollbar-thumb:hover { background: linear-gradient(180deg, #ff5cc8, #7ceeff); }
    #${FLYOUT_ID}::-webkit-scrollbar-corner { background: transparent; }
    #${FLYOUT_ID} .chud-rule {
      position: absolute;
      top: 0;
      left: 8%;
      right: 8%;
      height: 2px;
      background: var(--neon-grad-h, linear-gradient(90deg, #ff3d9a, #35e4ff));
      opacity: .85;
      pointer-events: none;
    }
    #${FLYOUT_ID} .chud-corner { position: absolute; width: 18px; height: 18px; pointer-events: none; }
    #${FLYOUT_ID} .chud-corner-tl { top: -1px; left: -1px; border-top: 2px solid var(--cyan); border-left: 2px solid var(--cyan); }
    #${FLYOUT_ID} .chud-corner-tr { top: -1px; right: -1px; border-top: 2px solid var(--cyan); border-right: 2px solid var(--cyan); }
    #${FLYOUT_ID} .chud-corner-bl { bottom: -1px; left: -1px; border-bottom: 2px solid var(--cyan); border-left: 2px solid var(--cyan); }
    #${FLYOUT_ID} .chud-corner-br { bottom: -1px; right: -1px; border-bottom: 2px solid var(--cyan); border-right: 2px solid var(--cyan); }

    /* ---- Header ---- */
    #${FLYOUT_ID} .chud-header {
      position: relative;
      padding: 30px 34px 20px;
      text-align: center;
      border-bottom: 1px solid rgba(255,255,255,.08);
    }
    #${FLYOUT_ID} #chud-version-badge {
      position: absolute;
      top: 22px;
      left: 24px;
      padding: 3px 9px 4px;
      border: 1px solid rgba(53,228,255,.4);
      color: var(--cyan);
      font-family: 'Space Mono', monospace;
      font-size: 10.5px;
      font-weight: 400;
      letter-spacing: .04em;
    }
    #${FLYOUT_ID} .chud-close-btn {
      position: absolute;
      top: 20px;
      right: 20px;
      width: 30px;
      height: 30px;
      display: flex;
      align-items: center;
      justify-content: center;
      background: transparent;
      border: 1px solid rgba(255,255,255,.14);
      color: #9a9ac8;
      font-size: 17px;
      line-height: 1;
      cursor: pointer;
      transition: all .2s ease;
    }
    #${FLYOUT_ID} .chud-close-btn:hover {
      border-color: var(--cyan);
      color: var(--text-primary, #f2f2ff);
      box-shadow: 0 0 12px -2px var(--accent-glow, rgba(255,61,154,.6));
    }
    #${FLYOUT_ID} .chud-emblem {
      position: relative;
      width: 44px;
      height: 44px;
      margin: 2px auto 15px;
      filter: drop-shadow(0 0 10px var(--accent-glow, rgba(255,61,154,.6)));
    }
    #${FLYOUT_ID} .chud-emblem-outer { position: absolute; inset: 0; transform: rotate(45deg); border: 1.5px solid var(--cyan); }
    #${FLYOUT_ID} .chud-emblem-mid { position: absolute; inset: 8px; transform: rotate(45deg); border: 1px solid rgba(53,228,255,.65); }
    #${FLYOUT_ID} .chud-emblem-inner { position: absolute; inset: 15px; transform: rotate(45deg); background: var(--neon-grad); }
    #${FLYOUT_ID} .chud-title {
      margin: 0;
      font-family: 'Cinzel', serif;
      font-weight: 600;
      font-size: 29px;
      line-height: 1;
      letter-spacing: .34em;
      text-indent: .34em;
      background: var(--neon-grad-h);
      -webkit-background-clip: text;
      background-clip: text;
      color: transparent;
      -webkit-text-fill-color: transparent;
    }
    #${FLYOUT_ID} .chud-subtitle {
      margin-top: 10px;
      font-family: 'Marcellus', serif;
      font-size: 11px;
      letter-spacing: .3em;
      text-transform: uppercase;
      color: #9a9ac8;
    }

    /* ---- Body / sections ---- */
    #${FLYOUT_ID} .chud-body { padding: 24px 34px 26px; display: flex; flex-direction: column; gap: 26px; }
    #${FLYOUT_ID} .chud-section { display: flex; flex-direction: column; gap: 20px; }
    #${FLYOUT_ID} .chud-section-label-row { display: flex; align-items: center; gap: 13px; }
    #${FLYOUT_ID} .chud-section-label {
      font-family: 'Cinzel', serif;
      font-size: 12px;
      font-weight: 600;
      letter-spacing: .24em;
      text-transform: uppercase;
      color: var(--text-primary, #f2f2ff);
      white-space: nowrap;
    }
    #${FLYOUT_ID} .chud-section-rule { flex: 1; height: 1px; background: linear-gradient(90deg, rgba(53,228,255,.6), transparent); }

    /* ---- Diamond sliders (TIMING) ---- */
    #${FLYOUT_ID} .chud-sliders { display: flex; flex-direction: column; gap: 13px; user-select: none; }
    #${FLYOUT_ID} .chud-slider-label-row { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }
    #${FLYOUT_ID} .chud-slider-label-left { display: flex; align-items: center; gap: 9px; min-width: 0; }
    #${FLYOUT_ID} .chud-tooltip-wrapper { display: inline-flex; align-items: center; justify-content: center; position: relative; flex: 0 0 auto; }
    #${FLYOUT_ID} .chud-tooltip-icon {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 18px;
      height: 18px;
      border-radius: 50%;
      border: 1px solid rgba(53,228,255,.55);
      color: var(--cyan);
      background: transparent;
      font-family: 'Space Mono', monospace;
      font-size: 10px;
      line-height: 1;
      cursor: help;
      padding: 0;
    }
    #${FLYOUT_ID} .chud-tooltip-icon:focus-visible {
      outline: 1px solid #35e4ff;
      outline-offset: 2px;
      border-radius: 50%;
    }
    #${FLYOUT_ID} .chud-slider-name { font-family: 'Marcellus', serif; font-size: 15.5px; color: #f2f2ff; }
    #${FLYOUT_ID} .chud-slider-unit { font-family: 'Space Mono', monospace; font-size: 10px; text-transform: uppercase; letter-spacing: .12em; color: #6b6b96; }
    #${FLYOUT_ID} .chud-slider-value { font-family: 'Space Mono', monospace; font-weight: 700; font-size: 15px; color: var(--cyan); white-space: nowrap; }
    #${FLYOUT_ID} .chud-slider-track { position: relative; height: 24px; display: flex; align-items: center; cursor: pointer; touch-action: none; }
    #${FLYOUT_ID} .chud-slider-rail { position: absolute; left: 0; right: 0; height: 2px; background: rgba(255,255,255,.12); }
    #${FLYOUT_ID} .chud-slider-fill {
      position: absolute;
      left: 0;
      height: 2px;
      background: var(--neon-grad-h);
      box-shadow: 0 0 9px var(--accent-glow, rgba(255,61,154,.6));
    }
    #${FLYOUT_ID} .chud-slider-handle {
      position: absolute;
      top: 50%;
      width: 16px;
      height: 16px;
      transform: translate(-50%, -50%) rotate(45deg);
      background: var(--neon-grad);
      border: 1px solid var(--text-primary, #f2f2ff);
      animation: hxHandle 2.8s ease-in-out infinite;
    }
    #${FLYOUT_ID} .chud-slider-handle-inner { position: absolute; inset: 4px; border: 1px solid rgba(10,10,31,.5); }

    /* ---- Startup: auto-start toggle ---- */
    #${FLYOUT_ID} .chud-startup-row { display: flex; align-items: center; justify-content: space-between; gap: 18px; user-select: none; }
    #${FLYOUT_ID} .chud-startup-copy { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
    #${FLYOUT_ID} .chud-startup-title { font-family: 'Marcellus', serif; font-size: 15.5px; color: #f2f2ff; }
    #${FLYOUT_ID} .chud-startup-hint { font-family: 'Barlow', sans-serif; font-size: 12.5px; color: #9a9ac8; }
    #${FLYOUT_ID} .chud-toggle {
      position: relative;
      flex: 0 0 auto;
      width: 54px;
      height: 27px;
      border-radius: 999px;
      border: 1px solid rgba(255,255,255,.16);
      background: rgba(255,255,255,.08);
      cursor: pointer;
      padding: 0;
      transition: background .3s ease, border-color .3s ease, box-shadow .3s ease;
    }
    #${FLYOUT_ID} .chud-toggle.on { border-color: transparent; background: var(--neon-grad-h); box-shadow: 0 0 16px rgba(255,61,154,.5); }
    #${FLYOUT_ID} .chud-toggle-knob {
      position: absolute;
      top: 50%;
      left: 16px;
      width: 19px;
      height: 19px;
      border-radius: 50%;
      transform: translate(-50%, -50%);
      background: #5a5a86;
      transition: left .3s cubic-bezier(.4,1.3,.6,1), background .3s ease;
    }
    #${FLYOUT_ID} .chud-toggle.on .chud-toggle-knob { left: 38px; background: #ffffff; }

    /* ---- Startup: game path ---- */
    #${FLYOUT_ID} .chud-path-group { display: flex; flex-direction: column; gap: 10px; }
    #${FLYOUT_ID} .chud-path-label { font-family: 'Marcellus', serif; font-size: 15.5px; color: #f2f2ff; }
    #${FLYOUT_ID} .chud-path-input-row { display: flex; align-items: stretch; }
    #${FLYOUT_ID} #game-path-input {
      flex: 1;
      min-width: 0;
      height: 44px;
      padding: 0 14px;
      background: rgba(8,7,26,.55);
      border: 1px solid rgba(255,255,255,.12);
      border-right: none;
      border-radius: 10px 0 0 10px;
      color: #e9e9ff;
      font-family: 'Space Mono', monospace;
      font-size: 12.5px;
      letter-spacing: .01em;
      outline: none;
      box-sizing: border-box;
      transition: border-color .2s ease, box-shadow .2s ease;
    }
    #${FLYOUT_ID} #game-path-input::placeholder { color: #55557a; }
    #${FLYOUT_ID} #game-path-input:focus {
      border-color: var(--cyan);
      box-shadow: inset 0 0 0 1px var(--cyan), 0 0 0 3px rgba(53,228,255,.15);
    }
    #${FLYOUT_ID} #path-status {
      flex: 0 0 auto;
      display: flex;
      align-items: center;
      justify-content: center;
      width: 30px;
      height: 44px;
      background: rgba(8,7,26,.55);
      border-top: 1px solid rgba(255,255,255,.12);
      border-bottom: 1px solid rgba(255,255,255,.12);
      font-size: 13px;
    }
    #${FLYOUT_ID} .chud-browse-btn {
      flex: 0 0 auto;
      width: 46px;
      height: 44px;
      display: flex;
      align-items: center;
      justify-content: center;
      background: rgba(255,255,255,.06);
      border: 1px solid rgba(255,255,255,.12);
      border-radius: 0 10px 10px 0;
      color: var(--cyan);
      cursor: pointer;
      transition: color .2s ease, box-shadow .2s ease, background .2s ease;
    }
    #${FLYOUT_ID} .chud-browse-btn:hover { background: rgba(255,255,255,.1); box-shadow: 0 0 12px -3px var(--accent-glow, rgba(255,61,154,.6)); }

    /* ---- Mods & Tools: "Add custom mods" expander ---- */
    #${FLYOUT_ID} .chud-expander { border: 1px solid rgba(255,255,255,.12); border-radius: 12px; background: rgba(255,255,255,.03); overflow: hidden; }
    #${FLYOUT_ID} .chud-expander-head {
      width: 100%;
      height: 52px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      padding: 0 15px;
      background: transparent;
      border: none;
      cursor: pointer;
      color: #f2f2ff;
      transition: background .2s ease, color .2s ease;
    }
    #${FLYOUT_ID} .chud-expander-head:hover { background: rgba(255,255,255,.05); color: var(--text-primary, #f2f2ff); }
    #${FLYOUT_ID} .chud-expander-head-left { display: flex; align-items: center; gap: 11px; }
    #${FLYOUT_ID} .chud-expander-icon { display: inline-flex; width: 18px; height: 18px; color: var(--cyan); }
    #${FLYOUT_ID} .chud-expander-name { font-family: 'Marcellus', serif; font-size: 15.5px; }
    #${FLYOUT_ID} .chud-expander-chevron { display: inline-flex; width: 16px; height: 16px; color: var(--cyan); transition: transform .3s ease; }
    #${FLYOUT_ID} .chud-expander-chevron.open { transform: rotate(180deg); }
    #${FLYOUT_ID} .chud-expander-body {
      padding: 0 15px 16px;
      border-top: 1px solid rgba(255,255,255,.08);
      animation: hxReveal .3s ease;
      display: flex;
      flex-direction: column;
      gap: 8px;
    }
    #${FLYOUT_ID} .chud-expander-hint {
      margin-top: 15px;
      padding: 14px 16px;
      border: 1px dashed rgba(53,228,255,.34);
      border-radius: 10px;
      background: rgba(8,7,26,.4);
      text-align: center;
      font-family: 'Barlow', sans-serif;
      font-size: 12.5px;
      color: #9a9ac8;
    }
    #${FLYOUT_ID} .chud-mod-row {
      display: flex;
      align-items: center;
      gap: 11px;
      padding: 9px 12px;
      background: rgba(255,255,255,.04);
      border: 1px solid rgba(255,255,255,.1);
      border-radius: 9px;
      cursor: pointer;
      transition: border-color .2s ease, background .2s ease;
    }
    #${FLYOUT_ID} .chud-mod-row:hover { border-color: var(--cyan); background: rgba(255,255,255,.07); }
    #${FLYOUT_ID} .chud-mod-bullet { width: 9px; height: 9px; transform: rotate(45deg); background: var(--neon-grad); flex: 0 0 auto; }
    #${FLYOUT_ID} .chud-mod-name { flex: 1; min-width: 0; font-family: 'Barlow', sans-serif; font-size: 13.5px; color: #e6e6ff; }

    /* ---- Mods & Tools: tool grid ---- */
    #${FLYOUT_ID} .chud-tool-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
    #${FLYOUT_ID} .chud-tool-card {
      position: relative;
      cursor: pointer;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: 10px;
      padding: 16px 8px;
      min-height: 86px;
      text-align: center;
      background: linear-gradient(180deg, rgba(255,255,255,.06), rgba(255,255,255,.03));
      border: 1px solid rgba(255,255,255,.12);
      border-radius: 12px;
      color: #c9c9e6;
      transition: transform .18s ease, box-shadow .18s ease, border-color .18s ease, color .18s ease;
    }
    #${FLYOUT_ID} .chud-tool-card:hover {
      border-color: var(--cyan);
      color: var(--text-primary, #f2f2ff);
      transform: translateY(-2px);
      box-shadow: 0 10px 24px -10px rgba(0,0,0,.6), 0 0 16px -4px var(--accent-glow, rgba(255,61,154,.55));
    }
    #${FLYOUT_ID} .chud-tool-card:active { transform: translateY(0); }
    #${FLYOUT_ID} .chud-tool-icon { display: inline-flex; width: 24px; height: 24px; color: var(--cyan); }
    #${FLYOUT_ID} .chud-tool-label { font-family: 'Barlow', sans-serif; font-size: 12.5px; }

    /* ---- Footer ---- */
    #${FLYOUT_ID} .chud-footer { display: flex; align-items: center; justify-content: space-between; padding: 20px 34px 26px; border-top: 1px solid rgba(255,255,255,.08); }
    #${FLYOUT_ID} .chud-footer-links { display: flex; align-items: center; gap: 18px; }
    #${FLYOUT_ID} .chud-github-link,
    #${FLYOUT_ID} .chud-discord-link {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      font-family: 'Barlow', sans-serif;
      font-size: 13px;
      font-weight: 500;
      letter-spacing: .02em;
      color: #9a9ac8;
      text-decoration: none;
      transition: color .2s ease;
    }
    #${FLYOUT_ID} .chud-github-link:hover,
    #${FLYOUT_ID} .chud-discord-link:hover { color: var(--cyan); }
    #${FLYOUT_ID} .chud-footer-icon { display: inline-flex; width: 16px; height: 16px; }
    #${FLYOUT_ID} .chud-save-btn {
      position: relative;
      overflow: hidden;
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 7px;
      min-width: 152px;
      height: 46px;
      padding: 0 28px;
      cursor: pointer;
      border: none;
      border-radius: 10px;
      color: #0a0a1f;
      font-family: 'Cinzel', serif;
      font-weight: 700;
      font-size: 13px;
      letter-spacing: .12em;
      text-transform: uppercase;
      background: var(--neon-grad);
      box-shadow: 0 6px 22px -4px var(--accent-glow, rgba(255,61,154,.5)), inset 0 1px 0 rgba(255,255,255,.35);
      transition: filter .18s ease, box-shadow .18s ease, transform .12s ease;
    }
    #${FLYOUT_ID} .chud-save-btn:hover {
      filter: brightness(1.08);
      box-shadow: 0 8px 30px -2px var(--accent-glow, rgba(255,61,154,.7)), inset 0 1px 0 rgba(255,255,255,.4);
      transform: translateY(-1px);
    }
    #${FLYOUT_ID} .chud-save-btn:active { transform: translateY(0); filter: brightness(.97); }
    #${FLYOUT_ID} .chud-save-shimmer {
      position: absolute;
      top: 0;
      bottom: 0;
      left: 0;
      width: 34%;
      background: linear-gradient(90deg, transparent, rgba(255,255,255,.6), transparent);
      transform: translateX(-160%) skewX(-18deg);
      animation: hxShimmer 4.8s ease-in-out 1.5s infinite;
      pointer-events: none;
    }
    #${FLYOUT_ID} .chud-save-icon { display: inline-flex; width: 16px; height: 16px; }

    /* ---- Toast ---- */
    #${PANEL_ID} .chud-toast {
      position: fixed;
      left: 50%;
      bottom: 40px;
      z-index: 10005;
      display: flex;
      align-items: center;
      gap: 9px;
      padding: 10px 16px;
      border-radius: 10px;
      background: rgba(12,11,33,.94);
      border: 1px solid rgba(53,228,255,.45);
      box-shadow: 0 12px 30px -8px rgba(0,0,0,.7), 0 0 20px -6px var(--accent-glow, rgba(255,61,154,.6));
      animation: hxToast .3s ease forwards;
      pointer-events: none;
    }
    #${PANEL_ID} .chud-toast-icon { display: inline-flex; width: 15px; height: 15px; color: var(--cyan); }
    #${PANEL_ID} .chud-toast-text { font-family: 'Barlow', sans-serif; font-size: 12.5px; color: #f2f2ff; }

    /* Tooltip bubble is rendered globally (outside flyout) */
    #chud-global-tooltip {
      position: fixed;
      left: 0;
      top: 0;
      width: 340px;
      max-width: 340px;
      box-sizing: border-box;
      padding: 10px 12px;
      background: #0b1a2a;
      border: 1px solid #3d4a68;
      color: #7ceeff;
      font-size: 12px;
      line-height: 1.35;
      white-space: pre-line;
      text-align: justify;
      text-justify: inter-word;
      box-shadow: 0 10px 28px rgba(0, 0, 0, 0.65);
      opacity: 0;
      visibility: hidden;
      transform: translateY(2px);
      transition: opacity 0.12s ease, transform 0.12s ease;
      z-index: 100050;
      pointer-events: none;
      font-family: "JetBrains Mono", monospace;
    }

    #chud-global-tooltip[data-show="true"] {
      opacity: 1;
      visibility: visible;
      transform: translateY(0px);
    }

    #chud-global-tooltip::after {
      content: "";
      position: absolute;
      left: var(--chud-tooltip-arrow-x, 50%);
      transform: translateX(-50%);
      width: 0;
      height: 0;
      border-left: 7px solid transparent;
      border-right: 7px solid transparent;
    }

    #chud-global-tooltip::before {
      content: "";
      position: absolute;
      left: var(--chud-tooltip-arrow-x, 50%);
      transform: translateX(-50%);
      width: 0;
      height: 0;
      border-left: 8px solid transparent;
      border-right: 8px solid transparent;
      z-index: -1;
    }

    /* Tooltip ABOVE the icon (arrow on bottom) */
    #chud-global-tooltip[data-placement="top"]::after {
      top: 100%;
      border-top: 7px solid #0b1a2a;
    }

    #chud-global-tooltip[data-placement="top"]::before {
      top: 100%;
      border-top: 8px solid #3d4a68;
      margin-top: 1px;
    }

    /* Tooltip BELOW the icon (arrow on top) */
    #chud-global-tooltip[data-placement="bottom"]::after {
      top: -7px;
      border-bottom: 7px solid #0b1a2a;
    }

    #chud-global-tooltip[data-placement="bottom"]::before {
      top: -8px;
      border-bottom: 8px solid #3d4a68;
      margin-top: -1px;
    }
    
    /* Add Custom Mods Dialog Styles */
    #add-custom-mods-dialog,
    #champion-selection-dialog,
    #skin-selection-dialog {
      position: fixed;
      top: 0;
      left: 0;
      width: 100%;
      height: 100%;
      z-index: 10001;
      background: rgba(0, 0, 0, 0.5);
      display: flex;
      align-items: center;
      justify-content: center;
    }

    #add-custom-mods-dialog .backdrop,
    #champion-selection-dialog .backdrop,
    #skin-selection-dialog .backdrop {
      position: fixed;
      top: 0;
      left: 0;
      width: 100%;
      height: 100%;
      z-index: 10001;
      background: rgba(0, 0, 0, 0.5);
      pointer-events: all;
    }

    
    #add-custom-mods-flyout,
    #champion-selection-flyout,
    #skin-selection-flyout {
      min-width: 600px !important;
      max-width: 800px !important;
      background: transparent !important;
      background-color: transparent !important;
      background-image: none !important;
      border-radius: 0 !important;
      padding: 0 !important;
      color: #7ceeff;
      font-family: "JetBrains Mono", monospace;
      display: flex !important;
      flex-direction: column !important;
      align-items: center !important;
      box-shadow: none !important;
      border: none !important;
      margin: 0 !important;
      overflow: visible !important;
      overflow-x: hidden !important;
      overflow-y: hidden !important;
    }

    #skin-selection-flyout {
      min-width: 700px !important;
    }

    #champion-selection-flyout::-webkit-scrollbar,
    #skin-selection-flyout::-webkit-scrollbar,
    #champion-selection-dialog::-webkit-scrollbar,
    #skin-selection-dialog::-webkit-scrollbar {
      display: none !important;
      width: 0 !important;
      height: 0 !important;
    }
    
    #champion-selection-flyout *::-webkit-scrollbar,
    #skin-selection-flyout *::-webkit-scrollbar {
      display: none !important;
      width: 0 !important;
      height: 0 !important;
    }
    
    #add-custom-mods-flyout lc-flyout-content,
    #add-custom-mods-flyout .lc-flyout-content,
    #champion-selection-flyout lc-flyout-content,
    #champion-selection-flyout .lc-flyout-content,
    #skin-selection-flyout lc-flyout-content,
    #skin-selection-flyout .lc-flyout-content {
      overflow-x: hidden !important;
    }
    
    #add-custom-mods-flyout lc-flyout-content,
    #add-custom-mods-flyout .lc-flyout-content,
    #champion-selection-flyout lc-flyout-content,
    #champion-selection-flyout .lc-flyout-content,
    #skin-selection-flyout lc-flyout-content,
    #skin-selection-flyout .lc-flyout-content {
      background: #070b16 !important;
      background-color: #070b16 !important;
      background-image: none !important;
      border-radius: 0 !important;
      padding: 20px !important;
      width: 100% !important;
      box-sizing: border-box !important;
      border: 1px solid #35e4ff !important;
      box-shadow: 0 4px 12px rgba(0, 0, 0, 0.5) !important;
      margin: 0 !important;
      overflow-x: hidden !important;
    }
    
    #champion-selection-dialog,
    #skin-selection-dialog {
      overflow-x: hidden !important;
      overflow-y: hidden !important;
    }
    
    #champion-selection-flyout::-webkit-scrollbar,
    #skin-selection-flyout::-webkit-scrollbar,
    #champion-selection-flyout::-webkit-scrollbar:horizontal,
    #skin-selection-flyout::-webkit-scrollbar:horizontal,
    #champion-selection-dialog::-webkit-scrollbar,
    #skin-selection-dialog::-webkit-scrollbar {
      display: none !important;
      width: 0 !important;
      height: 0 !important;
    }
    
    #add-custom-mods-flyout::before,
    #add-custom-mods-flyout::after {
      display: none !important;
      content: none !important;
    }

    #add-custom-mods-flyout *::before,
    #add-custom-mods-flyout *::after {
      display: none !important;
      content: none !important;
      background: none !important;
      background-image: none !important;
    }
    
    #add-custom-mods-flyout .settings-title,
    #champion-selection-flyout .settings-title,
    #skin-selection-flyout .settings-title {
      font-size: 18px;
      font-weight: bold !important;
      margin-bottom: 12px;
      color: #35e4ff;
      text-align: center;
      width: 100%;
      position: relative;
      display: flex;
      align-items: center;
      justify-content: center;
    }
    
    .dialog-header {
      display: flex;
      align-items: center;
      justify-content: center;
      width: 100%;
      margin-bottom: 16px;
      position: relative;
    }

    .back-button {
      position: absolute;
      left: 0;
      background: transparent;
      border: none;
      color: #7a93a8;
      width: 32px;
      height: 32px;
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 0;
      transition: color 0.2s ease;
      flex-shrink: 0;
    }
    .back-button svg {
      width: 20px;
      height: 20px;
      fill: none;
      stroke: currentColor;
      stroke-width: 2;
      stroke-linecap: round;
      stroke-linejoin: round;
    }
    .back-button:hover {
      color: #35e4ff;
    }
    .back-button:active {
      color: #dff3ff;
    }
    
    .dialog-title-wrapper {
      flex: 1;
      text-align: center;
      font-size: 18px;
      font-weight: bold;
      color: #35e4ff;
      font-family: "JetBrains Mono", monospace;
    }
    
    #champion-selection-flyout .champion-search-input,
    #champion-selection-flyout lol-uikit-flat-input.champion-search-input {
      width: 100%;
      margin-bottom: 12px;
    }
    
    #champion-selection-flyout .champion-search-input input,
    #champion-selection-flyout lol-uikit-flat-input.champion-search-input input {
      width: 100%;
      box-sizing: border-box;
    }
    
    #champions-grid-wrapper,
    #skins-list {
      scrollbar-width: none;
    }
    #champions-grid-wrapper::-webkit-scrollbar,
    #skins-list::-webkit-scrollbar {
      display: none;
      width: 0;
      height: 0;
    }

    #champions-grid-wrapper {
      max-height: 45vh;
      margin-top: 12px;
    }
    
    #champions-grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(90px, 1fr));
      gap: 8px;
      padding-right: 8px;
    }

    .champion-card {
      display: flex;
      flex-direction: column;
      align-items: center;
      cursor: pointer;
      padding: 6px;
      border: 1px solid transparent;
      border-radius: 4px;
      transition: border-color 0.2s, background 0.2s;
      background: transparent;
    }
    .champion-card:hover {
      border-color: #35e4ff;
      background: rgba(53, 228, 255, 0.08);
    }
    .champion-card img {
      width: 60px;
      height: 60px;
      border-radius: 50%;
      border: 2px solid #3d4a68;
      object-fit: cover;
      transition: border-color 0.2s;
    }
    .champion-card:hover img {
      border-color: #35e4ff;
    }
    .champion-card .champion-name {
      margin-top: 6px;
      font-size: 11px;
      color: #7a93a8;
      text-align: center;
      font-family: "JetBrains Mono", monospace;
      line-height: 1.2;
      max-width: 80px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .champion-card:hover .champion-name {
      color: #7ceeff;
    }

    #skins-list {
      max-height: 60vh;
    }

    #skins-list .skins-list-container {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
      gap: 10px;
      padding-right: 8px;
    }

    .skin-card {
      display: flex;
      flex-direction: column;
      cursor: pointer;
      border: 1px solid #3d4a68;
      border-radius: 4px;
      overflow: hidden;
      transition: border-color 0.2s, box-shadow 0.2s;
      background: #131a2b;
    }
    .skin-card:hover {
      border-color: #35e4ff;
      box-shadow: 0 0 8px rgba(53, 228, 255, 0.3);
    }
    .skin-card img {
      width: 100%;
      aspect-ratio: 308 / 560;
      object-fit: cover;
      display: block;
      background: #0a0a0d;
    }
    .skin-card .skin-name {
      padding: 8px;
      font-size: 12px;
      color: #7a93a8;
      text-align: center;
      font-family: "JetBrains Mono", monospace;
      line-height: 1.3;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .skin-card:hover .skin-name {
      color: #7ceeff;
    }
  `;
  }

  function log(level, message, data = null) {
    const consoleMethod =
      level === "error"
        ? console.error
        : level === "warn"
          ? console.warn
          : console.log;
    consoleMethod(`${LOG_PREFIX} ${message}`, data || "");
  }

  function handleSettingsData(payload) {
    currentSettings = {
      threshold: payload.threshold || 0.5,
      monitorAutoResumeTimeout: payload.monitorAutoResumeTimeout || 25,
      autostart: payload.autostart || false,
      gamePath: payload.gamePath || "",
      gamePathValid: payload.gamePathValid || false,
      version: payload.version || "",
    };
    // Update version badge if the panel is already open
    const badge = document.getElementById("chud-version-badge");
    if (badge && payload.version) {
      badge.textContent = `v${payload.version}`;
    }
    updateSettingsForm();
    // Badge count should reflect what's actually in diagnostics (and not change while dragging sliders).
    const localCount = Array.isArray(diagnosticsState.errors) ? diagnosticsState.errors.length : 0;
    if (localCount > 0) {
      updateErrorBadges(true, localCount);
    } else {
      updateErrorBadges(!!payload.hasErrors, payload.errorsCount || 0);
    }
    // If backend reports errors but we don't have the list yet, fetch it once so we can
    // show per-category guidance and clear it after Save (not while dragging).
    if (payload.hasErrors && (!Array.isArray(diagnosticsState.errors) || diagnosticsState.errors.length === 0)) {
      requestDiagnostics();
    }
    log("info", "Settings data received", currentSettings);
  }

  let diagnosticsDialog = null;
  let diagnosticsState = { errors: [], path: "", settingsSnapshot: null, baseSkinStats: null };
  let errorBadgeState = { hasErrors: false, count: 0 };
  let _badgeObserverStarted = false;
  let _pendingSave = null;
  let _diagnosticsPollId = null;
  let _flyoutRepositionTimer = null;

  function _clamp(n, min, max) {
    return Math.max(min, Math.min(max, n));
  }

  function _diagnosticsCategory(e) {
    const raw = String(e?.text || e?.msg || "").trim();
    const code = String(e?.code || "").trim();

    if (code === "BASE_SKIN_FORCE_SLOW" || code === "BASE_SKIN_VERIFY_FAILED") return "injection_threshold";
    if (code === "AUTO_RESUME_TRIGGERED" || code === "MONITOR_AUTO_RESUME_TIMEOUT") return "monitor_timeout";

    if (/Injection\s*Threshold/i.test(raw)) return "injection_threshold";
    if (/Auto-Resume Timeout/i.test(raw) || /Monitor Auto-Resume Timeout/i.test(raw)) return "monitor_timeout";

    return "other";
  }

  function _getRecommendedForCategory(category, errors) {
    const snap = diagnosticsState?.settingsSnapshot || null;
    const snapThreshold =
      typeof snap?.threshold === "number" && Number.isFinite(snap.threshold) ? snap.threshold : null;
    const snapTimeout =
      typeof snap?.monitorAutoResumeTimeout === "number" && Number.isFinite(snap.monitorAutoResumeTimeout)
        ? snap.monitorAutoResumeTimeout
        : null;

    if (category === "injection_threshold") {
      // Prefer explicit recommendation if present.
      const recs = (errors || [])
        .map((e) => e?.recommendedThresholdS)
        .filter((v) => typeof v === "number" && Number.isFinite(v));
      if (recs.length) return _clamp(Math.max(...recs), 0.3, 2.0);
      // Otherwise: stable heuristic based on the settings at the time diagnostics were fetched.
      if (typeof snapThreshold === "number") return _clamp(snapThreshold + 0.25, 0.3, 2.0);
      return null;
    }

    if (category === "monitor_timeout") {
      // Prefer explicit recommendation if present (support multiple field names defensively).
      const recs = (errors || [])
        .map((e) => e?.recommendedMonitorTimeoutS ?? e?.recommendedTimeoutS ?? e?.recommendedAutoResumeTimeoutS)
        .filter((v) => typeof v === "number" && Number.isFinite(v));
      if (recs.length) return _clamp(Math.max(...recs), 20, 180);
      if (typeof snapTimeout === "number") return _clamp(Math.max(snapTimeout + 30, 90), 20, 180);
      return null;
    }

    return null;
  }

  function getEffectiveDiagnosticsErrors() {
    const errors = Array.isArray(diagnosticsState.errors) ? diagnosticsState.errors : [];
    if (errors.length === 0) return [];

    // Group by category so we can drop the whole category once resolved.
    const byCat = new Map();
    for (const e of errors) {
      const cat = _diagnosticsCategory(e);
      if (!byCat.has(cat)) byCat.set(cat, []);
      byCat.get(cat).push(e);
    }

    const curThreshold = typeof currentSettings?.threshold === "number" ? currentSettings.threshold : null;
    const curTimeout =
      typeof currentSettings?.monitorAutoResumeTimeout === "number" ? currentSettings.monitorAutoResumeTimeout : null;

    const resolved = new Set();
    for (const [cat, list] of byCat.entries()) {
      const rec = _getRecommendedForCategory(cat, list);
      if (rec == null) continue;

      if (cat === "injection_threshold" && typeof curThreshold === "number" && curThreshold >= (rec - 1e-6)) {
        resolved.add(cat);
      } else if (cat === "monitor_timeout" && typeof curTimeout === "number" && curTimeout >= (rec - 1e-6)) {
        resolved.add(cat);
      }
    }

    if (resolved.size === 0) return errors;
    return errors.filter((e) => !resolved.has(_diagnosticsCategory(e)));
  }

  function getResolvedDiagnosticsCategories() {
    const all = Array.isArray(diagnosticsState?.errors) ? diagnosticsState.errors : [];
    if (all.length === 0) return [];
    const allCats = new Set(all.map(_diagnosticsCategory));
    const remainingCats = new Set(getEffectiveDiagnosticsErrors().map(_diagnosticsCategory));

    const resolved = [];
    for (const cat of allCats) {
      if (cat === "other") continue;
      if (!remainingCats.has(cat)) resolved.push(cat);
    }
    return resolved;
  }

  function handleDiagnosticsData(payload) {
    // Snapshot the settings at the time we fetched diagnostics so "recommended" targets stay stable
    // while the user is dragging sliders.
    const snapshot =
      currentSettings && typeof currentSettings === "object"
        ? {
            threshold: currentSettings.threshold,
            monitorAutoResumeTimeout: currentSettings.monitorAutoResumeTimeout,
          }
        : null;
    diagnosticsState = {
      errors: Array.isArray(payload.errors) ? payload.errors : [],
      path: payload.path || "",
      settingsSnapshot: snapshot,
      baseSkinStats: payload.baseSkinStats || null,
    };
    updateErrorBadges(diagnosticsState.errors.length > 0, diagnosticsState.errors.length);
    renderDiagnosticsDialog();
    renderThresholdBenchmark();
  }

  function getResolvedCategoriesForSavedValues(values) {
    // Only consider a category "fixed" if:
    // - the saved value meets/exceeds the recommended target, AND
    // - the user actually increased it compared to the snapshot from when diagnostics were fetched.
    const eps = 1e-6;
    const all = Array.isArray(diagnosticsState?.errors) ? diagnosticsState.errors : [];
    if (!all.length || !values) return [];

    const snap = diagnosticsState?.settingsSnapshot || null;
    const snapThreshold = typeof snap?.threshold === "number" ? snap.threshold : null;
    const snapTimeout = typeof snap?.monitorAutoResumeTimeout === "number" ? snap.monitorAutoResumeTimeout : null;

    const byCat = new Map();
    for (const e of all) {
      const cat = _diagnosticsCategory(e);
      if (!byCat.has(cat)) byCat.set(cat, []);
      byCat.get(cat).push(e);
    }

    const resolved = [];
    for (const [cat, list] of byCat.entries()) {
      if (cat === "other") continue;
      const rec = _getRecommendedForCategory(cat, list);
      if (rec == null) continue;

      if (cat === "injection_threshold") {
        const saved = typeof values.threshold === "number" ? values.threshold : null;
        const increased = typeof snapThreshold === "number" ? saved != null && saved > (snapThreshold + eps) : true;
        if (saved != null && saved >= (rec - eps) && increased) resolved.push(cat);
      } else if (cat === "monitor_timeout") {
        const saved = typeof values.monitorAutoResumeTimeout === "number" ? values.monitorAutoResumeTimeout : null;
        const increased = typeof snapTimeout === "number" ? saved != null && saved > (snapTimeout + eps) : true;
        if (saved != null && saved >= (rec - eps) && increased) resolved.push(cat);
      }
    }

    return resolved;
  }

  function updateErrorBadges(hasErrors, count) {
    errorBadgeState = { hasErrors: !!hasErrors, count: Number(count) || 0 };
    applyErrorBadges();
  }

  function startBadgeObserver() {
    if (_badgeObserverStarted) return;
    _badgeObserverStarted = true;

    // Re-apply badges when the Golden Chud nav item is injected by CHUD-UI (or recreated by Ember).
    const tryApply = () => {
      try {
        applyErrorBadges();
      } catch (e) {}
    };

    try {
      const obs = new MutationObserver(() => {
        // Only bother if we actually have errors to show (keeps it cheap)
        if (!errorBadgeState.hasErrors) return;
        tryApply();
      });
      obs.observe(document.body, { childList: true, subtree: true });

      // Also retry a few times after startup (covers cases where body observer misses early churn)
      let attempts = 0;
      const id = setInterval(() => {
        attempts += 1;
        tryApply();
        if (attempts >= 20) clearInterval(id); // ~10s max
      }, 500);
    } catch (e) {
      // Fallback: periodic best-effort if MutationObserver fails
      let attempts = 0;
      const id = setInterval(() => {
        attempts += 1;
        tryApply();
        if (attempts >= 20) clearInterval(id);
      }, 500);
    }
  }

  function applyErrorBadges() {
    // Sidebar "Golden Chud" nav icon badge
    const navItem = document.querySelector(
      "lol-uikit-navigation-item.menu_item_Golden.Chud"
    );
    if (navItem) {
      const host =
        navItem.querySelector(".menu-item-icon-wrapper") ||
        navItem.querySelector(".menu-item-icon") ||
        navItem;

      host.style.position = host.style.position || "relative";
      // Use warning image overlay (assets/red-warning.png) on the top-right of the Chud icon.
      let badge = host.querySelector("#chud-errors-badge");
      if (errorBadgeState.hasErrors) {
        if (!badge) {
          badge = document.createElement("div");
          badge.id = "chud-errors-badge";
          badge.classList.add("chud-warning-glow");
          // Position + size for the warning overlay
          badge.style.position = "absolute";
          badge.style.top = "-10px";
          badge.style.right = "-10px";
          badge.style.width = "14px";
          badge.style.height = "14px";
          badge.style.backgroundImage = `url(http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/red-warning.png)`;
          badge.style.backgroundSize = "contain";
          badge.style.backgroundRepeat = "no-repeat";
          badge.style.backgroundPosition = "center";
          badge.style.pointerEvents = "none";
          host.appendChild(badge);
        }
        // Keep text empty; this overlay is purely visual.
      } else if (badge) {
        badge.remove();
      }
    }

    // Troubleshooting button warning overlay (only when settings flyout is open)
    const tb = document.getElementById("troubleshoot-button");
    if (tb) {
      tb.style.position = tb.style.position || "relative";
      let warn = tb.querySelector("#chud-troubleshoot-warning");
      if (errorBadgeState.hasErrors) {
        if (!warn) {
          warn = document.createElement("div");
          warn.id = "chud-troubleshoot-warning";
          warn.classList.add("chud-warning-glow");

          warn.style.position = "absolute";
          warn.style.top = "-15px";
          warn.style.right = "-9px";
          warn.style.width = "14px";
          warn.style.height = "14px";
          warn.style.backgroundImage = `url(http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/red-warning.png)`;
          warn.style.backgroundSize = "contain";
          warn.style.backgroundRepeat = "no-repeat";
          warn.style.backgroundPosition = "center";
          warn.style.pointerEvents = "none";

          tb.appendChild(warn);
        }
      } else if (warn) {
        warn.remove();
      }
    }
  }

  function handlePathValidationResult(payload) {
    const pathInput = document.getElementById("game-path-input");
    const pathStatus = document.getElementById("path-status");

    if (!pathInput || !pathStatus) {
      return;
    }

    // Only update if this validation is for the current path value
    const currentPath = pathInput.value.trim();
    if (payload.gamePath === currentPath) {
      const isValid = payload.valid === true;
      pathStatus.textContent = isValid ? "✅" : "❌";

      // Update current settings if this is the saved path
      if (currentPath === currentSettings.gamePath) {
        currentSettings.gamePathValid = isValid;
      }
    }
  }

  function handleSettingsSaved(payload) {
    if (payload.success) {
      log("info", "Settings saved successfully", payload);
      // Show success message to user ("Saved" + check icon for ~2.2s, per the redesign spec)
      const saveLabel = document.getElementById("save-button-label");
      const saveIcon = document.getElementById("save-button-icon");
      if (saveLabel) {
        const originalText = saveLabel.textContent;
        saveLabel.textContent = "Saved";
        if (saveIcon) saveIcon.style.display = "inline-flex";
        setTimeout(() => {
          saveLabel.textContent = originalText;
          if (saveIcon) saveIcon.style.display = "none";
        }, 2200);
      }
      showChudToast("Settings saved");

      // After a successful save: if the user actually increased a value enough to satisfy the
      // recommendation, clear all diagnostics entries from that category so they stay gone.
      try {
        if (_pendingSave) {
          const cats = getResolvedCategoriesForSavedValues(_pendingSave);
          if (cats.length > 0) {
            if (bridge) bridge.send({ type: "diagnostics-clear-category", categories: cats });
          }
        }
      } catch (e) {}
      _pendingSave = null;

      // Refresh settings + diagnostics + badges after save
      requestSettings();
      requestDiagnostics();
    } else {
      log("error", "Settings save failed", payload);
      // Show error message to user
      const saveLabel = document.getElementById("save-button-label");
      const saveBtn = document.getElementById("save-button");
      if (saveLabel) {
        const originalText = saveLabel.textContent;
        saveLabel.textContent = payload.error || "Error saving settings";
        if (saveBtn) saveBtn.style.background = "#8b0000";
        setTimeout(() => {
          saveLabel.textContent = originalText;
          if (saveBtn) saveBtn.style.background = "";
        }, 3000);
      }
      showChudToast(payload.error || "Failed to save settings");
    }
  }

  function validateGamePath(path) {
    if (!path || !path.trim()) {
      return false;
    }
    // Basic validation - check if path contains "League of Legends"
    // Full validation is done on Python side
    return path.trim().length > 0;
  }

  function createSettingsFlyout(navItem) {
    // Remove existing panel if any
    const existingPanel = document.getElementById(PANEL_ID);
    if (existingPanel) {
      existingPanel.remove();
    }

    // Create panel container (fixed positioning for viewport-relative coordinates)
    const panel = document.createElement("div");
    panel.id = PANEL_ID;
    panel.style.position = "fixed";
    panel.style.top = "0";
    panel.style.left = "0";
    panel.style.width = "100%";
    panel.style.height = "100%";
    panel.style.zIndex = "10000";
    panel.style.pointerEvents = "none";
    document.body.appendChild(panel);

    // Backdrop: full-screen animated midnight scrim, click-outside-to-close, flex-centers the modal card.
    const backdrop = document.createElement("div");
    backdrop.className = "chud-backdrop";
    backdrop.addEventListener("click", (e) => {
      // Only close if clicking directly on the backdrop, not the card
      if (e.target === backdrop) {
        closeSettingsPanel();
      }
    });
    panel.appendChild(backdrop);

    // Decorative layers (diamond grid + glows + drifting motes) — pointer-events:none so they
    // never steal the backdrop's click-to-close.
    const decor = document.createElement("div");
    decor.className = "chud-decor";
    const gridLayer = document.createElement("div");
    gridLayer.className = "chud-grid-layer";
    decor.appendChild(gridLayer);
    const glowMagenta = document.createElement("div");
    glowMagenta.className = "chud-glow-magenta";
    decor.appendChild(glowMagenta);
    const glowCyan = document.createElement("div");
    glowCyan.className = "chud-glow-cyan";
    decor.appendChild(glowCyan);
    const MOTE_COUNT = 14;
    for (let i = 0; i < MOTE_COUNT; i++) {
      const mote = document.createElement("div");
      mote.className = "chud-mote";
      const size = 2 + Math.random() * 2.5;
      const isMagenta = i % 2 === 0;
      mote.style.left = `${4 + Math.random() * 92}%`;
      mote.style.bottom = `${-10 - Math.random() * 20}px`;
      mote.style.width = `${size}px`;
      mote.style.height = `${size}px`;
      mote.style.background = isMagenta ? "#ff3d9a" : "#35e4ff";
      mote.style.boxShadow = isMagenta ? "0 0 6px rgba(255,61,154,.8)" : "0 0 6px rgba(53,228,255,.8)";
      mote.style.animationDuration = `${8.5 + Math.random() * 4.5}s`;
      mote.style.animationDelay = `${Math.random() * 10}s`;
      decor.appendChild(mote);
    }
    backdrop.appendChild(decor);

    // ===== Modal card =====
    const card = document.createElement("div");
    card.id = FLYOUT_ID;
    card.addEventListener("click", (e) => {
      e.stopPropagation();
    });
    backdrop.appendChild(card);

    const rule = document.createElement("div");
    rule.className = "chud-rule";
    card.appendChild(rule);

    ["tl", "tr", "bl", "br"].forEach((pos) => {
      const corner = document.createElement("div");
      corner.className = `chud-corner chud-corner-${pos}`;
      card.appendChild(corner);
    });

    // ---- Header ----
    const header = document.createElement("div");
    header.className = "chud-header";

    const versionBadge = document.createElement("div");
    versionBadge.id = "chud-version-badge";
    versionBadge.textContent = currentSettings.version ? `v${currentSettings.version}` : "v0.0.0";
    header.appendChild(versionBadge);

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "chud-close-btn";
    closeBtn.title = "Close";
    closeBtn.setAttribute("aria-label", "Close");
    closeBtn.textContent = "×";
    closeBtn.addEventListener("click", () => closeSettingsPanel());
    header.appendChild(closeBtn);

    const emblem = document.createElement("div");
    emblem.className = "chud-emblem";
    const emblemOuter = document.createElement("div");
    emblemOuter.className = "chud-emblem-outer";
    const emblemMid = document.createElement("div");
    emblemMid.className = "chud-emblem-mid";
    const emblemInner = document.createElement("div");
    emblemInner.className = "chud-emblem-inner";
    emblem.appendChild(emblemOuter);
    emblem.appendChild(emblemMid);
    emblem.appendChild(emblemInner);
    header.appendChild(emblem);

    const title = document.createElement("h1");
    title.className = "chud-title";
    title.textContent = "SETTINGS";
    header.appendChild(title);

    const subtitle = document.createElement("div");
    subtitle.className = "chud-subtitle";
    subtitle.textContent = "Pengu Loader · Configuration";
    header.appendChild(subtitle);

    card.appendChild(header);

    // ---- Body ----
    const body = document.createElement("div");
    body.className = "chud-body";
    card.appendChild(body);

    function sectionLabel(text) {
      const row = document.createElement("div");
      row.className = "chud-section-label-row";
      const label = document.createElement("span");
      label.className = "chud-section-label";
      label.textContent = text;
      const rule2 = document.createElement("span");
      rule2.className = "chud-section-rule";
      row.appendChild(label);
      row.appendChild(rule2);
      return row;
    }

    // ---- Section 1: TIMING (diamond sliders) ----
    const timingSection = document.createElement("div");
    timingSection.className = "chud-section";
    timingSection.appendChild(sectionLabel("Timing"));

    const slidersWrap = document.createElement("div");
    slidersWrap.className = "chud-sliders";

    // Real functional bounds: the backend clamps threshold to [0.3, 2.0] and the auto-resume
    // timeout to [20, 180] (see saveSettings()/renderDiagnosticsDialog()'s "at max" checks). The
    // slider ranges intentionally match those clamps rather than a wider on-paper range, so a
    // value the user picks in the UI is never silently bumped after Save.
    const thresholdSlider = createDiamondSlider({
      idBase: "threshold",
      name: "Injection Threshold",
      tooltip:
        "Injection threshold is the time window during which the app considers your last hovered skin as the one to inject.\n\nFor example, if your injection threshold is set to 1 second, whichever skin you were hovering 1 second before champ select ends will be the one injected.\n\nIf your PC or connection is on the slower side, you may need to fine-tune this value.",
      min: 0.3,
      max: 2.0,
      step: 0.05,
      value: currentSettings.threshold,
      format: (v) => `${v.toFixed(2)} s`,
      toStoredValue: (v) => Math.round(v * 100),
      fromStoredValue: (v) => v / 100,
    });
    slidersWrap.appendChild(thresholdSlider.row);

    // Benchmark hint line (existing diagnostics feature — keep wired to renderThresholdBenchmark()).
    const benchmarkInfo = document.createElement("div");
    benchmarkInfo.id = "chud-threshold-benchmark";
    benchmarkInfo.style.cssText = "font-family:'Barlow',sans-serif; font-size:11px; color:#9a9ac8;";
    slidersWrap.appendChild(benchmarkInfo);

    const timeoutSlider = createDiamondSlider({
      idBase: "timeout",
      name: "Monitor Auto-Resume Timeout",
      tooltip:
        "Auto-resume is a safety feature.\n\nIf the injection process takes longer than the value you set, the app will automatically cancel the injection and let the game start normally.\n\nThis prevents the injection from looping and blocking the game from launching.\n\nIf you use a lot of custom mods, you may need to adjust this value.",
      min: 20,
      max: 180,
      step: 5,
      value: currentSettings.monitorAutoResumeTimeout,
      format: (v) => `${Math.round(v)} s`,
      toStoredValue: (v) => Math.round(v),
      fromStoredValue: (v) => v,
    });
    slidersWrap.appendChild(timeoutSlider.row);

    timingSection.appendChild(slidersWrap);
    body.appendChild(timingSection);

    // ---- Section 2: STARTUP ----
    const startupSection = document.createElement("div");
    startupSection.className = "chud-section";
    startupSection.appendChild(sectionLabel("Startup"));

    const startupRow = document.createElement("div");
    startupRow.className = "chud-startup-row";

    const startupCopy = document.createElement("div");
    startupCopy.className = "chud-startup-copy";
    const startupTitle = document.createElement("div");
    startupTitle.className = "chud-startup-title";
    startupTitle.textContent = "Start automatically with Windows";
    const startupHint = document.createElement("div");
    startupHint.className = "chud-startup-hint";
    startupHint.textContent = "Launch Pengu Loader when your PC starts";
    startupCopy.appendChild(startupTitle);
    startupCopy.appendChild(startupHint);
    startupRow.appendChild(startupCopy);

    // Kept as id="autostart-checkbox" (with a boolean `.checked` property) so saveSettings()/
    // updateSettingsForm() keep reading/writing it exactly as before.
    const autostartToggle = document.createElement("button");
    autostartToggle.type = "button";
    autostartToggle.id = "autostart-checkbox";
    autostartToggle.className = "chud-toggle";
    autostartToggle.title = "Toggle auto-start";
    autostartToggle.setAttribute("aria-pressed", "false");
    autostartToggle.checked = false;
    const autostartKnob = document.createElement("span");
    autostartKnob.className = "chud-toggle-knob";
    autostartToggle.appendChild(autostartKnob);
    autostartToggle.addEventListener("click", () => {
      autostartToggle.checked = !autostartToggle.checked;
      autostartToggle.classList.toggle("on", autostartToggle.checked);
      autostartToggle.setAttribute("aria-pressed", String(autostartToggle.checked));
    });
    startupRow.appendChild(autostartToggle);
    startupSection.appendChild(startupRow);

    // Game path
    const pathGroup = document.createElement("div");
    pathGroup.className = "chud-path-group";
    const pathLabel = document.createElement("div");
    pathLabel.className = "chud-path-label";
    pathLabel.textContent = "League of Legends Game Path";
    pathGroup.appendChild(pathLabel);

    const pathInputRow = document.createElement("div");
    pathInputRow.className = "chud-path-input-row";

    const pathInput = document.createElement("input");
    pathInput.type = "text";
    pathInput.id = "game-path-input";
    pathInput.placeholder = "C:\\Riot Games\\League of Legends\\Game";
    pathInput.spellcheck = false;
    pathInput.addEventListener("input", () => {
      updatePathStatus();
    });
    pathInputRow.appendChild(pathInput);

    const pathStatus = document.createElement("span");
    pathStatus.id = "path-status";
    pathStatus.textContent = "";
    pathInputRow.appendChild(pathStatus);

    const browseBtn = document.createElement("button");
    browseBtn.type = "button";
    browseBtn.className = "chud-browse-btn";
    browseBtn.title = "Locate game folder";
    browseBtn.innerHTML =
      '<svg width="18" height="18" viewBox="0 0 24 24" style="display:block;"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z" style="fill:none; stroke:currentColor; stroke-width:1.7px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';
    browseBtn.addEventListener("click", () => {
      // No Tauri folder-picker dialog is reachable from inside the League client webview, so the
      // browse button focuses the path input and re-runs validation on whatever is already there.
      pathInput.focus();
      const val = pathInput.value.trim();
      if (val) requestPathValidation(val);
    });
    pathInputRow.appendChild(browseBtn);

    pathGroup.appendChild(pathInputRow);
    startupSection.appendChild(pathGroup);
    body.appendChild(startupSection);

    // ---- Section 3: MODS & TOOLS ----
    const modsSection = document.createElement("div");
    modsSection.className = "chud-section";
    modsSection.appendChild(sectionLabel("Mods & Tools"));

    // "Add custom mods" expander — drives the same category -> champion -> skin picker flow as
    // before (handleCategorySelection/openChampionSelection/openSkinSelection are unchanged).
    const expander = document.createElement("div");
    expander.className = "chud-expander";

    const expanderHead = document.createElement("button");
    expanderHead.type = "button";
    expanderHead.className = "chud-expander-head";

    const expanderHeadLeft = document.createElement("span");
    expanderHeadLeft.className = "chud-expander-head-left";
    const expanderIcon = document.createElement("span");
    expanderIcon.className = "chud-expander-icon";
    expanderIcon.innerHTML =
      '<svg width="18" height="18" viewBox="0 0 24 24" style="display:block;"><path d="M12 5v14M5 12h14" style="fill:none; stroke:currentColor; stroke-width:1.7px; stroke-linecap:round;"></path></svg>';
    const expanderName = document.createElement("span");
    expanderName.className = "chud-expander-name";
    expanderName.textContent = "Add custom mods";
    expanderHeadLeft.appendChild(expanderIcon);
    expanderHeadLeft.appendChild(expanderName);

    const expanderChevron = document.createElement("span");
    expanderChevron.className = "chud-expander-chevron";
    expanderChevron.innerHTML =
      '<svg width="16" height="16" viewBox="0 0 24 24" style="display:block;"><path d="M6 9l6 6 6-6" style="fill:none; stroke:currentColor; stroke-width:1.7px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';

    expanderHead.appendChild(expanderHeadLeft);
    expanderHead.appendChild(expanderChevron);
    expander.appendChild(expanderHead);

    const categories = [
      { id: "skins", name: "Skins" },
      { id: "maps", name: "Maps" },
      { id: "fonts", name: "Fonts" },
      { id: "announcers", name: "Announcers" },
      { id: "ui", name: "UI" },
      { id: "voiceover", name: "Voiceover" },
      { id: "loading_screen", name: "Loading Screen" },
      { id: "vfx", name: "VFX" },
      { id: "sfx", name: "SFX" },
      { id: "others", name: "Others" },
    ];

    let expanderBody = null;
    expanderHead.addEventListener("click", () => {
      const isOpen = expanderChevron.classList.toggle("open");
      if (isOpen) {
        if (!expanderBody) {
          expanderBody = document.createElement("div");
          expanderBody.className = "chud-expander-body";

          const hint = document.createElement("div");
          hint.className = "chud-expander-hint";
          hint.textContent = "Choose what kind of mod you want to add";
          expanderBody.appendChild(hint);

          categories.forEach((category) => {
            const row = document.createElement("div");
            row.className = "chud-mod-row";
            const bullet = document.createElement("span");
            bullet.className = "chud-mod-bullet";
            const name = document.createElement("span");
            name.className = "chud-mod-name";
            name.textContent = category.name;
            row.appendChild(bullet);
            row.appendChild(name);
            row.addEventListener("click", () => {
              handleCategorySelection(category.id);
            });
            expanderBody.appendChild(row);
          });

          expander.appendChild(expanderBody);
        }
        expanderBody.style.display = "flex";
      } else if (expanderBody) {
        expanderBody.style.display = "none";
      }
    });

    modsSection.appendChild(expander);

    // Tool grid
    const toolGrid = document.createElement("div");
    toolGrid.className = "chud-tool-grid";

    function toolCard(id, iconSvg, label, onClick) {
      const tCard = document.createElement("div");
      tCard.id = id;
      tCard.className = "chud-tool-card";
      const icon = document.createElement("span");
      icon.className = "chud-tool-icon";
      icon.innerHTML = iconSvg;
      const lbl = document.createElement("span");
      lbl.className = "chud-tool-label";
      lbl.textContent = label;
      tCard.appendChild(icon);
      tCard.appendChild(lbl);
      tCard.addEventListener("click", onClick);
      return tCard;
    }

    const DOC_ICON =
      '<svg width="24" height="24" viewBox="0 0 24 24" style="display:block;"><path d="M13 3H6a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8l-6-5z" style="fill:none; stroke:currentColor; stroke-width:1.6px; stroke-linecap:round; stroke-linejoin:round;"></path><path d="M13 3v5h5" style="fill:none; stroke:currentColor; stroke-width:1.6px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';
    const GEAR_ICON =
      '<svg width="24" height="24" viewBox="0 0 24 24" style="display:block;"><circle cx="12" cy="12" r="3" style="fill:none; stroke:currentColor; stroke-width:1.6px;"></circle><path d="M19.4 13a7.6 7.6 0 0 0 0-2l2-1.6-2-3.4-2.4 1a7.4 7.4 0 0 0-1.7-1l-.4-2.5H9.1l-.4 2.5a7.4 7.4 0 0 0-1.7 1l-2.4-1-2 3.4L4.6 11a7.6 7.6 0 0 0 0 2l-2 1.6 2 3.4 2.4-1c.5.4 1.1.8 1.7 1l.4 2.5h4.8l.4-2.5c.6-.2 1.2-.6 1.7-1l2.4 1 2-3.4-2-1.6z" style="fill:none; stroke:currentColor; stroke-width:1.3px; stroke-linejoin:round;"></path></svg>';
    const EXT_ICON =
      '<svg width="24" height="24" viewBox="0 0 24 24" style="display:block;"><path d="M18 13v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h6" style="fill:none; stroke:currentColor; stroke-width:1.6px; stroke-linecap:round; stroke-linejoin:round;"></path><path d="M15 3h6v6M10 14 21 3" style="fill:none; stroke:currentColor; stroke-width:1.6px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';

    toolGrid.appendChild(toolCard("logs-folder-button", DOC_ICON, "Open Logs Folder", () => openLogsFolder()));
    toolGrid.appendChild(toolCard("troubleshoot-button", GEAR_ICON, "Troubleshooting", () => openDiagnosticsDialog()));
    toolGrid.appendChild(toolCard("pengu-ui-button", EXT_ICON, "Open Pengu Loader UI", () => openPenguLoaderUI()));

    modsSection.appendChild(toolGrid);
    body.appendChild(modsSection);

    // ---- Footer ----
    const footer = document.createElement("div");
    footer.className = "chud-footer";

    const footerLinks = document.createElement("div");
    footerLinks.className = "chud-footer-links";

    const githubLink = document.createElement("a");
    githubLink.className = "chud-github-link";
    githubLink.href = GITHUB_URL;
    githubLink.target = "_blank";
    githubLink.rel = "noopener noreferrer";
    githubLink.innerHTML =
      '<span class="chud-footer-icon"><svg width="16" height="16" viewBox="0 0 24 24" style="display:block;"><path d="M9 8l-4 4 4 4M15 8l4 4-4 4" style="fill:none; stroke:currentColor; stroke-width:1.7px; stroke-linecap:round; stroke-linejoin:round;"></path></svg></span><span>GitHub</span>';
    footerLinks.appendChild(githubLink);

    const discordLink = document.createElement("a");
    discordLink.className = "chud-discord-link";
    discordLink.href = DISCORD_URL;
    discordLink.target = "_blank";
    discordLink.rel = "noopener noreferrer";
    discordLink.title = "Discord";
    discordLink.innerHTML =
      '<span class="chud-footer-icon"><svg width="16" height="16" viewBox="0 0 24 24" style="display:block;"><circle cx="8" cy="12" r="1.4" style="fill:currentColor;"></circle><circle cx="16" cy="12" r="1.4" style="fill:currentColor;"></circle><path d="M7 5.5c2.5-.8 7.5-.8 10 0 1.6 2 2.5 5 2.5 8.7-1.7 1.6-3.6 2.4-5.5 2.6l-.7-1.4c1-.3 1.9-.7 2.7-1.3-2.9 1.4-6.4 1.4-9.4 0 .8.6 1.7 1 2.7 1.3l-.6 1.4c-1.9-.2-3.8-1-5.5-2.6C4.5 10.5 5.4 7.5 7 5.5z" style="fill:none; stroke:currentColor; stroke-width:1.3px; stroke-linejoin:round;"></path></svg></span><span>Discord</span>';
    footerLinks.appendChild(discordLink);

    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.id = "save-button";
    saveBtn.className = "chud-save-btn";

    const saveShimmer = document.createElement("span");
    saveShimmer.className = "chud-save-shimmer";
    saveBtn.appendChild(saveShimmer);

    const saveIconWrap = document.createElement("span");
    saveIconWrap.id = "save-button-icon";
    saveIconWrap.className = "chud-save-icon";
    saveIconWrap.style.display = "none";
    saveIconWrap.innerHTML =
      '<svg width="16" height="16" viewBox="0 0 24 24" style="display:block;"><path d="M5 12l4.5 4.5L19 7" style="fill:none; stroke:currentColor; stroke-width:2.4px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';
    saveBtn.appendChild(saveIconWrap);

    const saveLabel = document.createElement("span");
    saveLabel.id = "save-button-label";
    saveLabel.textContent = "Save Changes";
    saveBtn.appendChild(saveLabel);

    saveBtn.addEventListener("click", () => saveSettings());

    footer.appendChild(footerLinks);
    footer.appendChild(saveBtn);
    card.appendChild(footer);

    settingsPanel = panel;

    // Paint the current in-memory values immediately; the "settings-data" response (already in
    // flight below) will refresh everything again once it lands.
    updateSettingsForm();

    // Request current settings and benchmark data
    requestSettings();
    requestDiagnostics();
  }

  // ===== Neon Glass UI helpers (tooltip bubble, diamond slider, toast) =====

  function getOrCreateGlobalTooltip() {
    let el = document.getElementById("chud-global-tooltip");
    if (el) return el;

    el = document.createElement("div");
    el.id = "chud-global-tooltip";
    el.setAttribute("role", "tooltip");
    el.setAttribute("data-show", "false");
    document.body.appendChild(el);
    return el;
  }

  function hideGlobalTooltip() {
    const el = document.getElementById("chud-global-tooltip");
    if (!el) return;
    el.setAttribute("data-show", "false");
  }

  function showGlobalTooltipFor(anchorEl, text) {
    const tooltip = getOrCreateGlobalTooltip();
    tooltip.textContent = text;
    tooltip.setAttribute("data-show", "true");

    // Measure after setting text
    const margin = 10;
    const rect = anchorEl.getBoundingClientRect();
    const tRect = tooltip.getBoundingClientRect();

    // Prefer above, fallback below if not enough room
    const preferredTop = rect.top - tRect.height - margin;
    const belowTop = rect.bottom + margin;
    const useTop = preferredTop >= 8;
    const top = useTop ? preferredTop : belowTop;
    tooltip.setAttribute("data-placement", useTop ? "top" : "bottom");

    // Center horizontally on icon, clamp to viewport
    let left = rect.left + rect.width / 2 - tRect.width / 2;
    const maxLeft = window.innerWidth - tRect.width - 8;
    left = Math.max(8, Math.min(maxLeft, left));

    tooltip.style.left = `${Math.round(left)}px`;
    tooltip.style.top = `${Math.round(top)}px`;

    // Nudge the arrow towards the anchor if clamped
    const anchorCenterX = rect.left + rect.width / 2;
    const arrowX = Math.max(12, Math.min(tRect.width - 12, anchorCenterX - left));
    tooltip.style.setProperty("--chud-tooltip-arrow-x", `${Math.round(arrowX)}px`);
  }

  // Round "i" info dot (README §1 TIMING) — same global tooltip bubble as before, new visuals.
  function createTooltipButton(tooltipText, ariaLabel) {
    const wrapper = document.createElement("span");
    wrapper.className = "chud-tooltip-wrapper";

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "chud-tooltip-icon";
    btn.textContent = "i";
    btn.setAttribute("aria-label", ariaLabel || "Info");

    // prevent accidental focus/drag interactions with nearby controls
    btn.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
    });

    const show = () => showGlobalTooltipFor(btn, tooltipText);
    const hide = () => hideGlobalTooltip();

    btn.addEventListener("mouseenter", show);
    btn.addEventListener("mouseleave", hide);
    btn.addEventListener("focus", show);
    btn.addEventListener("blur", hide);

    // Keep tooltip in correct position while resizing/scrolling
    const reposition = () => {
      const tt = document.getElementById("chud-global-tooltip");
      if (!tt || tt.getAttribute("data-show") !== "true") return;
      showGlobalTooltipFor(btn, tooltipText);
    };
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);

    wrapper.appendChild(btn);
    return wrapper;
  }

  // Shared math for painting a diamond-slider's fill/handle/value label from a raw value.
  function applyChudSliderVisual(min, max, rawValue, fillEl, handleEl, valueEl, format) {
    const clamped = Math.max(min, Math.min(max, rawValue));
    const pct = max > min ? ((clamped - min) / (max - min)) * 100 : 0;
    if (fillEl) fillEl.style.width = `${pct}%`;
    if (handleEl) handleEl.style.left = `${pct}%`;
    if (valueEl) valueEl.textContent = format(clamped);
    return clamped;
  }

  // Builds one TIMING row: info dot + name + unit + value, and a custom track+fill+diamond-handle
  // slider (README §1 TIMING / §Interactions — native <input type=range> can't give the diamond
  // handle, so this is pointerdown/pointermove/pointerup on window, plus click-to-set on the track).
  // `toStoredValue`/`fromStoredValue` bridge to the legacy #<idBase>-slider value convention that
  // saveSettings()/updateSettingsForm() already expect (threshold is stored as hundredths).
  function createDiamondSlider(opts) {
    const { idBase, name, tooltip, min, max, step, value, format, toStoredValue, fromStoredValue } = opts;

    const row = document.createElement("div");
    row.className = "chud-slider-row";

    const labelRow = document.createElement("div");
    labelRow.className = "chud-slider-label-row";

    const left = document.createElement("span");
    left.className = "chud-slider-label-left";
    left.appendChild(createTooltipButton(tooltip, `${name} info`));
    const nameEl = document.createElement("span");
    nameEl.className = "chud-slider-name";
    nameEl.textContent = name;
    const unitEl = document.createElement("span");
    unitEl.className = "chud-slider-unit";
    unitEl.textContent = "sec";
    left.appendChild(nameEl);
    left.appendChild(unitEl);

    const valueEl = document.createElement("span");
    valueEl.className = "chud-slider-value";
    valueEl.id = `${idBase}-value`;

    labelRow.appendChild(left);
    labelRow.appendChild(valueEl);
    row.appendChild(labelRow);

    const track = document.createElement("div");
    track.className = "chud-slider-track";
    const rail = document.createElement("div");
    rail.className = "chud-slider-rail";
    const fill = document.createElement("div");
    fill.className = "chud-slider-fill";
    fill.id = `${idBase}-fill`;
    const handle = document.createElement("div");
    handle.className = "chud-slider-handle";
    handle.id = `${idBase}-handle`;
    const handleInner = document.createElement("div");
    handleInner.className = "chud-slider-handle-inner";
    handle.appendChild(handleInner);
    track.appendChild(rail);
    track.appendChild(fill);
    track.appendChild(handle);
    row.appendChild(track);

    // Value store only (not rendered) — same #threshold-slider/#timeout-slider ids/semantics that
    // saveSettings()/updateSettingsForm() already read/write.
    const hiddenInput = document.createElement("input");
    hiddenInput.type = "range";
    hiddenInput.id = `${idBase}-slider`;
    hiddenInput.min = String(toStoredValue(min));
    hiddenInput.max = String(toStoredValue(max));
    hiddenInput.style.display = "none";
    row.appendChild(hiddenInput);

    const snap = (v) => {
      const snapped = Math.round((v - min) / step) * step + min;
      return Math.max(min, Math.min(max, snapped));
    };

    const setValue = (raw) => {
      const snapped = snap(raw);
      hiddenInput.value = String(toStoredValue(snapped));
      applyChudSliderVisual(min, max, snapped, fill, handle, valueEl, format);
      return snapped;
    };

    setValue(value);

    const valueFromClientX = (clientX) => {
      const rect = track.getBoundingClientRect();
      const pct = rect.width > 0 ? Math.max(0, Math.min(1, (clientX - rect.left) / rect.width)) : 0;
      return min + pct * (max - min);
    };

    let dragging = false;
    const onPointerMove = (e) => {
      if (!dragging) return;
      setValue(valueFromClientX(e.clientX));
    };
    const onPointerUp = () => {
      if (!dragging) return;
      dragging = false;
      window.removeEventListener("pointermove", onPointerMove);
      window.removeEventListener("pointerup", onPointerUp);
    };
    track.addEventListener("pointerdown", (e) => {
      dragging = true;
      setValue(valueFromClientX(e.clientX));
      window.addEventListener("pointermove", onPointerMove);
      window.addEventListener("pointerup", onPointerUp);
      e.preventDefault();
    });

    return { row, setValue, fromStoredValue };
  }

  // Bottom-center toast pill (README §Toast) — the app has no shared neon.css toast system in this
  // in-client plugin, so this is a small self-contained equivalent.
  function showChudToast(message) {
    try {
      const panelEl = document.getElementById(PANEL_ID);
      if (!panelEl) return;
      const existing = panelEl.querySelector(".chud-toast");
      if (existing) existing.remove();
      const toast = document.createElement("div");
      toast.className = "chud-toast";
      const icon = document.createElement("span");
      icon.className = "chud-toast-icon";
      icon.innerHTML =
        '<svg width="15" height="15" viewBox="0 0 24 24" style="display:block;"><path d="M5 12l4.5 4.5L19 7" style="fill:none; stroke:currentColor; stroke-width:2.4px; stroke-linecap:round; stroke-linejoin:round;"></path></svg>';
      const text = document.createElement("span");
      text.className = "chud-toast-text";
      text.textContent = message;
      toast.appendChild(icon);
      toast.appendChild(text);
      panelEl.appendChild(toast);
      setTimeout(() => {
        try {
          toast.remove();
        } catch (e) {}
      }, 2400);
    } catch (e) {}
  }

  function updateSettingsForm() {
    // Diamond sliders — same value ranges as createDiamondSlider() uses when building them.
    applyChudSliderVisual(
      0.3,
      2.0,
      currentSettings.threshold,
      document.getElementById("threshold-fill"),
      document.getElementById("threshold-handle"),
      document.getElementById("threshold-value"),
      (v) => `${v.toFixed(2)} s`
    );
    const thresholdSlider = document.getElementById("threshold-slider");
    if (thresholdSlider) thresholdSlider.value = String(Math.round(currentSettings.threshold * 100));

    applyChudSliderVisual(
      20,
      180,
      currentSettings.monitorAutoResumeTimeout,
      document.getElementById("timeout-fill"),
      document.getElementById("timeout-handle"),
      document.getElementById("timeout-value"),
      (v) => `${Math.round(v)} s`
    );
    const timeoutSlider = document.getElementById("timeout-slider");
    if (timeoutSlider) timeoutSlider.value = String(Math.round(currentSettings.monitorAutoResumeTimeout));

    // Auto-start toggle
    const autostartToggle = document.getElementById("autostart-checkbox");
    if (autostartToggle) {
      autostartToggle.checked = !!currentSettings.autostart;
      autostartToggle.classList.toggle("on", autostartToggle.checked);
      autostartToggle.setAttribute("aria-pressed", String(autostartToggle.checked));
    }

    const pathInput = document.getElementById("game-path-input");
    if (pathInput) {
      pathInput.value = currentSettings.gamePath || "";
      // Update status based on validation result from settings data
      const pathStatus = document.getElementById("path-status");
      if (pathStatus) {
        const path = pathInput.value.trim();
        if (path.length === 0) {
          pathStatus.textContent = "";
        } else if (currentSettings.gamePathValid) {
          pathStatus.textContent = "✅";
        } else {
          // Request validation for the loaded path
          requestPathValidation(path);
        }
      }
    }

    // Update version badge
    const versionBadge = document.getElementById("chud-version-badge");
    if (versionBadge && currentSettings.version) {
      versionBadge.textContent = `v${currentSettings.version}`;
    }
  }

  function updatePathStatus() {
    const pathInput = document.getElementById("game-path-input");
    const pathStatus = document.getElementById("path-status");

    if (!pathInput || !pathStatus) {
      return;
    }

    const path = pathInput.value.trim();
    if (path.length === 0) {
      pathStatus.textContent = "";
      return;
    }

    // Show loading indicator while validating
    pathStatus.textContent = "⏳";

    // Clear any existing timeout
    if (pathValidationTimeout) {
      clearTimeout(pathValidationTimeout);
    }

    // Debounce validation request (wait 500ms after user stops typing)
    pathValidationTimeout = setTimeout(() => {
      requestPathValidation(path);
    }, 500);
  }

  function requestPathValidation(path) {
    if (!path || !path.trim()) {
      return;
    }

    if (bridge) bridge.send({
      type: "path-validate",
      gamePath: path.trim(),
    });
  }

  function requestSettings() {
    if (bridge) bridge.send({
      type: "settings-request",
    });
  }

  function saveSettings() {
    const thresholdSlider = document.getElementById("threshold-slider");
    const timeoutSlider = document.getElementById("timeout-slider");
    const autostartCheckbox = document.getElementById("autostart-checkbox");
    const pathInput = document.getElementById("game-path-input");

    const threshold = thresholdSlider
      ? parseFloat(thresholdSlider.value) / 100
      : 0.5;
    const monitorAutoResumeTimeout = timeoutSlider
      ? parseInt(timeoutSlider.value)
      : 60;
    const autostart = autostartCheckbox ? autostartCheckbox.checked : false;
    const gamePath = pathInput ? pathInput.value.trim() : "";

    // Clamp threshold between 0.30 and 2.0
    const clampedThreshold = Math.max(0.3, Math.min(2.0, threshold));
    // Clamp timeout between 20 and 180
    const clampedTimeout = Math.max(20, Math.min(180, monitorAutoResumeTimeout));

    // Track what we're trying to save; we only clear warnings after the save succeeds.
    _pendingSave = { threshold: clampedThreshold, monitorAutoResumeTimeout: clampedTimeout };

    if (bridge) bridge.send({
      type: "settings-save",
      threshold: clampedThreshold,
      monitorAutoResumeTimeout: clampedTimeout,
      autostart: autostart,
      gamePath: gamePath,
    });

    log("info", "Settings save requested", {
      threshold: clampedThreshold,
      monitorAutoResumeTimeout: clampedTimeout,
      autostart,
      gamePath,
    });
  }

  function handleCategorySelection(category) {
    if (category === "skins") {
      // Open champion selection for skins
      openChampionSelection();
    } else {
      // Directly open folder for other categories
      if (bridge) bridge.send({
        type: "add-custom-mods-category-selected",
        category: category,
      });
      log("info", `Category selected: ${category}`);
    }
  }

  function openChampionSelection() {
    // Remove existing dialog if any
    const existingDialog = document.getElementById("champion-selection-dialog");
    if (existingDialog) {
      existingDialog.remove();
    }

    // Dialog is the backdrop itself — no extra wrapper
    const dialog = document.createElement("div");
    dialog.id = "champion-selection-dialog";
    dialog.addEventListener("click", (e) => {
      if (e.target === dialog) {
        closeChampionSelection();
      }
    });
    document.body.appendChild(dialog);

    // Create flyout frame
    const flyoutFrame = document.createElement("div");
    flyoutFrame.id = "champion-selection-flyout";
    flyoutFrame.className = "flyout";
    flyoutFrame.style.maxHeight = "75vh";
    flyoutFrame.style.width = "700px";
    flyoutFrame.style.overflowY = "hidden";
    flyoutFrame.style.overflowX = "hidden";
    flyoutFrame.addEventListener("click", (e) => e.stopPropagation());

    // Create flyout content
    const flyoutContent = document.createElement("div");
    flyoutContent.className = "lc-flyout-content";

    // Header with back button and title
    const header = document.createElement("div");
    header.className = "dialog-header";

    // Back button
    const backButton = document.createElement("button");
    backButton.className = "back-button";
    backButton.innerHTML = '<svg viewBox="0 0 24 24"><polyline points="15 18 9 12 15 6"></polyline></svg>';
    backButton.setAttribute("aria-label", "Go back");
    backButton.addEventListener("click", () => {
      closeChampionSelection();
    });
    header.appendChild(backButton);

    // Title text
    const titleWrapper = document.createElement("div");
    titleWrapper.className = "dialog-title-wrapper";
    titleWrapper.textContent = "Select Champion";
    header.appendChild(titleWrapper);

    flyoutContent.appendChild(header);

    // Search input using League UI component
    const searchContainer = document.createElement("div");
    searchContainer.className = "settings-section";

    let flatInput;
    try {
      flatInput = document.createElement("lol-uikit-flat-input");
    } catch (e) {
      flatInput = document.createElement("div");
      flatInput.className = "lol-uikit-flat-input";
    }
    flatInput.className = "champion-search-input";
    flyoutContent.style.width = "700px";

    const searchInput = document.createElement("input");
    searchInput.type = "search";
    searchInput.name = "champion_search";
    searchInput.id = "champion-search-input";
    searchInput.placeholder = "Search champions...";
    searchInput.autocomplete = "off";
    searchInput.autocorrect = "off";
    searchInput.autocapitalize = "off";
    searchInput.spellcheck = "false";

    flatInput.appendChild(searchInput);
    searchContainer.appendChild(flatInput);
    flyoutContent.appendChild(searchContainer);

    // Loading indicator
    const loadingIndicator = document.createElement("div");
    loadingIndicator.id = "champion-loading";
    loadingIndicator.textContent = "Loading champions...";
    loadingIndicator.style.color = "#7ceeff";
    loadingIndicator.style.textAlign = "center";
    loadingIndicator.style.padding = "20px";
    loadingIndicator.style.fontFamily = '"JetBrains Mono", monospace';
    flyoutContent.appendChild(loadingIndicator);

    // Champions grid wrapper
    const championsGridWrapper = document.createElement("div");
    championsGridWrapper.id = "champions-grid-wrapper";
    championsGridWrapper.style.overflowY = "auto";
    championsGridWrapper.style.overflowX = "hidden";
    championsGridWrapper.style.maxHeight = "45vh";
    championsGridWrapper.style.marginTop = "12px";

    // Champions grid container
    const championsGrid = document.createElement("div");
    championsGrid.id = "champions-grid";
    championsGridWrapper.appendChild(championsGrid);
    flyoutContent.appendChild(championsGridWrapper);

    flyoutFrame.appendChild(flyoutContent);
    dialog.appendChild(flyoutFrame);

    // Request champions list
    if (bridge) bridge.send({
      type: "add-custom-mods-champion-selected",
      action: "list",
    });

    // Search functionality
    searchInput.addEventListener("input", (e) => {
      const searchTerm = e.target.value.toLowerCase().trim();
      const allChampions = window.__chudAllChampions || [];
      const filtered = allChampions.filter((champ) =>
        champ.name.toLowerCase().includes(searchTerm)
      );
      renderChampionsGrid(filtered);
    });

    // Store render function for bridge response
    window.__chudChampionRenderer = renderChampionsGrid;
  }

  function closeChampionSelection() {
    const dialog = document.getElementById("champion-selection-dialog");
    if (dialog) {
      dialog.remove();
    }
    delete window.__chudChampionRenderer;
    delete window.__chudAllChampions;
  }

  function renderChampionsGrid(champions) {
    const championsGrid = document.getElementById("champions-grid");
    if (!championsGrid) return;

    championsGrid.innerHTML = "";

    if (champions.length === 0) {
      championsGrid.innerHTML = `<div style="grid-column: 1 / -1; color: #7ceeff; text-align: center; padding: 20px; font-family: 'JetBrains Mono', monospace;">No champions found matching your search.</div>`;
      return;
    }

    champions.forEach((champion) => {
      const card = document.createElement("div");
      card.className = "champion-card";

      const img = document.createElement("img");
      img.src = `/lol-game-data/assets/v1/champion-icons/${champion.id}.png`;
      img.alt = champion.name;
      img.loading = "lazy";
      img.onerror = function () { this.style.display = "none"; };
      card.appendChild(img);

      const name = document.createElement("div");
      name.className = "champion-name";
      name.textContent = champion.name;
      card.appendChild(name);

      card.addEventListener("click", () => handleChampionSelection(champion.id));
      championsGrid.appendChild(card);
    });
  }

  function handleChampionSelection(championId) {
    closeChampionSelection();
    openSkinSelection(championId);
  }

  function openSkinSelection(championId) {
    // Remove existing dialog if any
    const existingDialog = document.getElementById("skin-selection-dialog");
    if (existingDialog) {
      existingDialog.remove();
    }

    // Dialog is the backdrop itself
    const dialog = document.createElement("div");
    dialog.id = "skin-selection-dialog";
    dialog.addEventListener("click", (e) => {
      if (e.target === dialog) {
        closeSkinSelection();
      }
    });
    document.body.appendChild(dialog);

    // Create flyout frame
    const flyoutFrame = document.createElement("div");
    flyoutFrame.id = "skin-selection-flyout";
    flyoutFrame.className = "flyout";
    flyoutFrame.style.maxHeight = "75vh";
    flyoutFrame.style.width = "700px";
    flyoutFrame.style.overflowY = "hidden";
    flyoutFrame.style.overflowX = "hidden";
    flyoutFrame.addEventListener("click", (e) => e.stopPropagation());

    // Create flyout content
    const flyoutContent = document.createElement("div");
    flyoutContent.className = "lc-flyout-content";

    // Header with back button and title
    const header = document.createElement("div");
    header.className = "dialog-header";
    header.id = "skin-selection-header";

    // Back button
    const backButton = document.createElement("button");
    backButton.className = "back-button";
    backButton.innerHTML = '<svg viewBox="0 0 24 24"><polyline points="15 18 9 12 15 6"></polyline></svg>';
    backButton.setAttribute("aria-label", "Go back");
    backButton.addEventListener("click", (e) => {
      e.stopPropagation();
      closeSkinSelection();
      openChampionSelection();
    });
    header.appendChild(backButton);

    // Title text
    const titleWrapper = document.createElement("div");
    titleWrapper.className = "dialog-title-wrapper";
    titleWrapper.textContent = "Select Skin";
    header.appendChild(titleWrapper);

    flyoutContent.appendChild(header);

    // Loading indicator
    const loadingIndicator = document.createElement("div");
    loadingIndicator.id = "skin-loading";
    loadingIndicator.textContent = "Loading skins...";
    loadingIndicator.style.color = "#7ceeff";
    loadingIndicator.style.textAlign = "center";
    loadingIndicator.style.padding = "20px";
    loadingIndicator.style.fontFamily = '"JetBrains Mono", monospace';
    flyoutContent.appendChild(loadingIndicator);

    // Skins list container
    const skinsList = document.createElement("div");
    skinsList.style.overflowY = "auto";
    skinsList.style.overflowX = "hidden";
    skinsList.id = "skins-list";

    // Create inner container for flex layout
    const skinsListContainer = document.createElement("div");
    skinsListContainer.className = "skins-list-container";
    skinsList.appendChild(skinsListContainer);

    flyoutContent.appendChild(skinsList);

    flyoutFrame.appendChild(flyoutContent);
    dialog.appendChild(flyoutFrame);

    // Request skins for champion
    if (bridge) bridge.send({
      type: "add-custom-mods-skin-selected",
      action: "list",
      championId: championId,
    });

    // Store champion ID for later use
    window.__chudSelectedChampionId = championId;
  }

  function closeSkinSelection() {
    const dialog = document.getElementById("skin-selection-dialog");
    if (dialog) {
      dialog.remove();
    }
    delete window.__chudSelectedChampionId;
  }

  function handleSkinSelection(championId, skinId) {
    closeSkinSelection();

    if (bridge) bridge.send({
      type: "add-custom-mods-skin-selected",
      action: "create",
      championId: championId,
      skinId: skinId,
    });
    log("info", `Skin selected: champion=${championId}, skin=${skinId}`);
  }

  function handleChampionsListResponse(payload) {
    const loadingIndicator = document.getElementById("champion-loading");
    if (loadingIndicator) {
      loadingIndicator.style.display = "none";
    }

    const championsGrid = document.getElementById("champions-grid");
    if (!championsGrid) return;

    if (payload.error) {
      championsGrid.innerHTML = `<div style="color: #ff6b6b; text-align: center; padding: 20px; font-family: 'JetBrains Mono', monospace;">${escapeHtml(payload.error)}</div>`;
      return;
    }

    const champions = payload.champions || [];
    if (champions.length === 0) {
      championsGrid.innerHTML = `<div style="color: #7ceeff; text-align: center; padding: 20px; font-family: 'JetBrains Mono', monospace;">No champions found. Please ensure League of Legends client is running.</div>`;
      return;
    }

    // Store champions for search functionality
    window.__chudAllChampions = champions;

    // Render champions
    if (window.__chudChampionRenderer) {
      window.__chudChampionRenderer(champions);
    } else {
      // Fallback: render directly
      renderChampionsGrid(champions);
    }
  }

  function handleChampionSkinsResponse(payload) {
    const loadingIndicator = document.getElementById("skin-loading");
    if (loadingIndicator) {
      loadingIndicator.style.display = "none";
    }

    const skinsList = document.getElementById("skins-list");
    if (!skinsList) return;

    if (payload.error) {
      let skinsListContainer = skinsList.querySelector(".skins-list-container");
      if (!skinsListContainer) {
        skinsListContainer = document.createElement("div");
        skinsListContainer.className = "skins-list-container";
        skinsList.innerHTML = "";
        skinsList.appendChild(skinsListContainer);
      } else {
        skinsListContainer.innerHTML = "";
      }
      skinsListContainer.innerHTML = `<div style="color: #ff6b6b; text-align: center; padding: 20px; font-family: 'JetBrains Mono', monospace;">${escapeHtml(payload.error)}</div>`;
      return;
    }

    const skins = payload.skins || [];
    const championId = payload.championId;

    // Update title with champion name if available
    const header = document.getElementById("skin-selection-header");
    if (header && payload.championName) {
      const titleWrapper = header.querySelector(".dialog-title-wrapper");
      if (titleWrapper) {
        titleWrapper.textContent = `Select Skin - ${payload.championName}`;
      }
    }

    // Get or create the container inside the scrollable
    let skinsListContainer = skinsList.querySelector(".skins-list-container");
    if (!skinsListContainer) {
      skinsListContainer = document.createElement("div");
      skinsListContainer.className = "skins-list-container";
      skinsList.innerHTML = "";
      skinsList.appendChild(skinsListContainer);
    } else {
      skinsListContainer.innerHTML = "";
    }

    if (skins.length === 0) {
      skinsListContainer.innerHTML = `<div style="color: #7ceeff; text-align: center; padding: 20px; font-family: 'JetBrains Mono', monospace;">No skins found for this champion.</div>`;
      return;
    }

    skins.forEach((skin) => {
      const card = document.createElement("div");
      card.className = "skin-card";

      const img = document.createElement("img");
      const skinId = skin.skinId || skin.id;
      img.src = skin.tilePath || `/lol-game-data/assets/v1/champion-tiles/${skinId}.jpg`;
      img.alt = skin.name || `Skin ${skinId}`;
      img.loading = "lazy";
      img.onerror = function () { this.style.display = "none"; };
      card.appendChild(img);

      const nameEl = document.createElement("div");
      nameEl.className = "skin-name";
      nameEl.textContent = skin.name || `Skin ${skinId}`;
      card.appendChild(nameEl);

      card.addEventListener("click", () => handleSkinSelection(championId, skinId));
      skinsListContainer.appendChild(card);
    });
  }

  function handleFolderOpenedResponse(payload) {
    if (payload.error) {
      log("error", `Failed to open folder: ${escapeHtml(payload.error)}`);
      // Could show an error message to user here
    } else {
      log("info", `Folder opened: ${payload.path}`);
    }
  }

  function openLogsFolder() {
    if (bridge) bridge.send({
      type: "open-logs-folder",
    });
    log("info", "Open logs folder requested");
  }

  function requestDiagnostics() {
    if (bridge) bridge.send({ type: "diagnostics-request" });
  }

  function openDiagnosticsDialog() {
    // If already open, close it
    const existing = document.getElementById("chud-diagnostics-dialog");
    if (existing) {
      existing.remove();
      diagnosticsDialog = null;
      return;
    }

    const dialog = document.createElement("div");
    dialog.id = "chud-diagnostics-dialog";
    dialog.style.position = "fixed";
    dialog.style.top = "0";
    dialog.style.left = "0";
    dialog.style.width = "100%";
    dialog.style.height = "100%";
    dialog.style.zIndex = "10002";
    dialog.style.pointerEvents = "none";
    document.body.appendChild(dialog);

    const backdrop = document.createElement("div");
    backdrop.style.position = "absolute";
    backdrop.style.top = "0";
    backdrop.style.left = "0";
    backdrop.style.width = "100%";
    backdrop.style.height = "100%";
    backdrop.style.background = "rgba(0, 0, 0, 0.6)";
    backdrop.style.pointerEvents = "auto";
    backdrop.addEventListener("click", (e) => {
      if (e.target === backdrop) {
        dialog.remove();
        diagnosticsDialog = null;
      }
    });
    dialog.appendChild(backdrop);

    const panel = document.createElement("div");
    panel.style.position = "absolute";
    // Center relative to the Settings flyout (not the whole client window)
    // Fallback to viewport center if the flyout can't be found.
    let centerX = window.innerWidth / 2;
    let centerY = window.innerHeight / 2;
    try {
      const settingsFlyout = document.getElementById(FLYOUT_ID);
      if (settingsFlyout) {
        const r = settingsFlyout.getBoundingClientRect();
        centerX = r.left + r.width / 2;
        centerY = r.top + r.height / 2;
      }
    } catch (e) {}

    panel.style.left = `${centerX}px`;
    panel.style.top = `${centerY}px`;
    panel.style.transform = "translate(-50%, -50%)";
    panel.style.width = "520px";
    panel.style.maxWidth = "92vw";
    panel.style.background = "#0b0f14";
    panel.style.border = "1px solid #0d1420";
    panel.style.boxShadow = "0 10px 30px rgba(0,0,0,0.6)";
    panel.style.padding = "14px";
    panel.style.pointerEvents = "auto";
    panel.style.position = "absolute";

    const title = document.createElement("div");
    title.textContent = "Troubleshooting";
    title.style.color = "#7ceeff";
    title.style.fontFamily = "'JetBrains Mono', monospace";
    title.style.fontSize = "16px";
    title.style.marginBottom = "10px";
    panel.appendChild(title);

    // Top-right close button
    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.setAttribute("aria-label", "Close");
    closeBtn.textContent = "×";
    closeBtn.style.position = "absolute";
    closeBtn.style.top = "6px";
    closeBtn.style.right = "8px";
    closeBtn.style.width = "26px";
    closeBtn.style.height = "26px";
    closeBtn.style.lineHeight = "24px";
    closeBtn.style.padding = "0";
    closeBtn.style.border = "none";
    closeBtn.style.background = "#0b0f14";
    closeBtn.style.color = "#7ceeff";
    closeBtn.style.cursor = "pointer";
    closeBtn.style.borderRadius = "4px";
    closeBtn.style.fontFamily = "'JetBrains Mono', monospace";
    closeBtn.style.fontSize = "18px";
    closeBtn.addEventListener("click", () => {
      dialog.remove();
      diagnosticsDialog = null;
    });
    panel.appendChild(closeBtn);

    const body = document.createElement("div");
    body.id = "chud-diagnostics-body";
    body.style.color = "#7ceeff";
    body.style.fontFamily = "'JetBrains Mono', monospace";
    body.style.fontSize = "12px";
    body.style.whiteSpace = "normal";
    body.style.border = "1px solid #070b16";
    body.style.background = "#070a0e";
    body.style.padding = "10px";
    body.style.maxHeight = "220px";
    body.style.overflow = "auto";
    body.style.lineHeight = "1.35";
    body.textContent = "Loading…";
    panel.appendChild(body);

    const foot = document.createElement("div");
    foot.id = "chud-diagnostics-foot";
    foot.style.marginTop = "8px";
    foot.style.color = "#7a93a8";
    foot.style.fontFamily = "'JetBrains Mono', monospace";
    foot.style.fontSize = "11px";
    panel.appendChild(foot);

    backdrop.appendChild(panel);
    diagnosticsDialog = dialog;

    // After layout, clamp the panel inside the viewport (avoids off-screen when flyout is near an edge).
    try {
      requestAnimationFrame(() => {
        try {
          const pr = panel.getBoundingClientRect();
          const margin = 12;
          let dx = 0;
          let dy = 0;
          if (pr.left < margin) dx = margin - pr.left;
          if (pr.right > window.innerWidth - margin) dx = (window.innerWidth - margin) - pr.right;
          if (pr.top < margin) dy = margin - pr.top;
          if (pr.bottom > window.innerHeight - margin) dy = (window.innerHeight - margin) - pr.bottom;
          if (dx || dy) {
            const curLeft = parseFloat(panel.style.left) || centerX;
            const curTop = parseFloat(panel.style.top) || centerY;
            panel.style.left = `${curLeft + dx}px`;
            panel.style.top = `${curTop + dy}px`;
          }
        } catch (e) {}
      });
    } catch (e) {}

    requestDiagnostics();
    renderDiagnosticsDialog();
  }

  function renderDiagnosticsDialog() {
    if (!diagnosticsDialog) return;
    const body = document.getElementById("chud-diagnostics-body");
    const foot = document.getElementById("chud-diagnostics-foot");
    if (!body || !foot) return;

    const errors = Array.isArray(diagnosticsState.errors) ? diagnosticsState.errors : [];
    if (errors.length === 0) {
      body.innerHTML = `
        <div style="opacity:0.85; margin-bottom:8px;">No recent errors.</div>
        <div style="opacity:0.75;">If something feels off, open the logs folder and share the latest log in a discord ticket.</div>
      `.trim();
    } else {
      const clamp = (n, min, max) => Math.max(min, Math.min(max, n));
      const fmtS = (n, digits = 2) => (typeof n === "number" && Number.isFinite(n) ? `${n.toFixed(digits)} s` : "");
      const curThreshold = typeof currentSettings?.threshold === "number" ? currentSettings.threshold : null;
      const curMonitorTimeout =
        typeof currentSettings?.monitorAutoResumeTimeout === "number"
          ? currentSettings.monitorAutoResumeTimeout
          : null;

      const escapeHtml = (value) =>
        String(value ?? "")
          .replace(/&/g, "&amp;")
          .replace(/</g, "&lt;")
          .replace(/>/g, "&gt;")
          .replace(/"/g, "&quot;")
          .replace(/'/g, "&#39;");

      const describe = (e) => {
        const raw = String(e?.text || "").trim();
        const code = String(e?.code || "").trim();

        const isInjectionThreshold =
          code === "BASE_SKIN_FORCE_SLOW" ||
          code === "BASE_SKIN_VERIFY_FAILED" ||
          /Injection\s*Threshold/i.test(raw);
        const isMonitorTimeout =
          code === "AUTO_RESUME_TRIGGERED" ||
          code === "MONITOR_AUTO_RESUME_TIMEOUT" ||
          /Auto-Resume Timeout/i.test(raw) ||
          /Monitor Auto-Resume Timeout/i.test(raw);

        if (isInjectionThreshold) {
          const thresholdAtMax =
            typeof curThreshold === "number" && Number.isFinite(curThreshold) && curThreshold >= (2.0 - 1e-6);
          const stats = diagnosticsState.baseSkinStats;
          const hasTrackerData = stats && typeof stats.p90_ms === "number" && stats.confirmed_count > 0;
          const recMs = hasTrackerData ? stats.recommended_threshold_ms : (e.recommendedThresholdMs || null);
          const recS = typeof recMs === "number" ? (recMs / 1000).toFixed(2) : null;

          let fixText;
          if (thresholdAtMax) {
            fixText = `Fix: you're already at the maximum Injection Threshold. This usually means the injection is extremely slow. Try lighter mods, close heavy apps, move League/mods to an SSD, and consider adding antivirus exclusions for the League and Chud folders. Then retry.`;
          } else if (hasTrackerData) {
            fixText = `Fix: based on ${stats.confirmed_count} game(s), base skin confirmation takes up to ${stats.p90_ms}ms (p90). Recommended threshold: ${recS}s. Use the "Apply recommended" button below, or increase "Injection Threshold" manually.`;
          } else {
            fixText = `Fix: increase "Injection Threshold (seconds)" and click Save. If the warning is still there, increase it again and Save again. Once the warning is gone, retry your skin selection.`;
          }

          return {
            title:
              code === "BASE_SKIN_VERIFY_FAILED"
                ? "Base skin verification failed (selected skin may not apply)"
                : "Base skin forcing took too long (skin may not appear)",
            details: [
              code === "BASE_SKIN_VERIFY_FAILED"
                ? `What it means: the client didn't confirm the base skin change in time.`
                : `What it means: forcing the base skin took too long, so the selected skin may not show.`,
              fixText,
            ],
          };
        }

        if (isMonitorTimeout) {
          const timeoutAtMax =
            typeof curMonitorTimeout === "number" &&
            Number.isFinite(curMonitorTimeout) &&
            curMonitorTimeout >= (180 - 1e-6);
          return {
            title: "Injection exceeded the timeout (process was stopped)",
            details: [
              `What it means: injection took longer than the allowed time, so CHUD stopped the process.`,
              timeoutAtMax
                ? `Fix: you're already at the maximum Monitor Auto-Resume Timeout. This usually means the injection is extremely slow. Try lighter mods, close heavy apps, move League/mods to an SSD, and consider adding antivirus exclusions for the League and Chud folders. Then retry.`
                : `Fix: increase "Monitor Auto-Resume Timeout (seconds)" and click Save. If the warning is still there, increase it again and Save again. Once the warning is gone, try again.`,
            ],
          };
        }

        // Fallback: show raw error text as-is.
        return {
          title: raw || "(unknown error)",
          details: [],
        };
      };

      const headerHtml = `
        <div style="display:flex; flex-direction:column; gap:4px; margin-bottom:10px;">
          <div style="font-weight:700;">Errors (most recent first)</div>
          <div style="opacity:0.75;">Tip: after changing a setting, click <span style="font-weight:700;">Save</span>, then retry.</div>
        </div>
      `.trim();

      const itemsHtml = errors
        .map((e, idx) => {
          const ts = String(e?.ts || "").trim();
          const desc = describe(e);
          const title = escapeHtml(desc.title);
          const tsHtml = ts ? `<span style="opacity:0.75;">${escapeHtml(ts)}</span>` : "";

          const detailsHtml = (desc.details || [])
            .map((d) => `<li style="margin:2px 0;">${escapeHtml(d)}</li>`)
            .join("");

          return `
            <div style="border:1px solid rgba(70,55,20,0.55); background: rgba(1,10,19,0.35); padding:8px; margin-bottom:8px;">
              <div style="display:flex; gap:8px; align-items:baseline; margin-bottom:6px;">
                <span style="font-weight:800; color:#2ea6d6;">${idx + 1}.</span>
                ${tsHtml}
                <span style="font-weight:700;">${title}</span>
              </div>
              ${
                detailsHtml
                  ? `<ul style="margin:0; padding-left:18px;">${detailsHtml}</ul>`
                  : `<div style="opacity:0.8;">${escapeHtml(String(e?.text || "").trim() || "No additional details.")}</div>`
              }
            </div>
          `.trim();
        })
        .join("");

      body.innerHTML = `${headerHtml}${itemsHtml}`;
    }

    foot.innerHTML = "";
  }

  function renderThresholdBenchmark() {
    const el = document.getElementById("chud-threshold-benchmark");
    if (!el) return;

    const stats = diagnosticsState.baseSkinStats;
    const hasStats = stats && typeof stats.confirmed_count === "number" && stats.confirmed_count > 0;

    if (!hasStats) {
      el.innerHTML = "";
      return;
    }

    const recMs = stats.recommended_threshold_ms;
    const recS = typeof recMs === "number" ? (recMs / 1000).toFixed(2) : null;
    const curThresholdVal = typeof currentSettings?.threshold === "number" ? currentSettings.threshold : null;
    const needsIncrease = recS !== null && curThresholdVal !== null && curThresholdVal < parseFloat(recS) - 0.001;
    const games = stats.confirmed_count;
    const label = `${games} game${games > 1 ? "s" : ""}`;

    let html;
    if (needsIncrease) {
      html = `<span style="color:#35e4ff;">Based on ${label}, we recommend <span style="color:#2ea6d6; font-weight:700;">${recS}s</span></span>`;
      html += ` <button id="chud-apply-recommended-btn" style="
        margin-left:4px; padding:1px 8px; border:1px solid #0d1420; background:#131a2b;
        color:#7ceeff; cursor:pointer; font-family:'JetBrains Mono', monospace; font-size:11px;
        vertical-align:middle;
      ">Apply</button>`;
    } else {
      html = `<span style="color:#5b9a32;">Your threshold looks good (based on ${label})</span>`;
    }

    el.innerHTML = html;

    const applyBtn = document.getElementById("chud-apply-recommended-btn");
    if (applyBtn) {
      applyBtn.addEventListener("click", () => {
        if (bridge) {
          bridge.send({ type: "diagnostics-apply-recommended" });
          applyBtn.textContent = "Applied!";
          applyBtn.disabled = true;
          applyBtn.style.opacity = "0.6";
          setTimeout(() => {
            if (bridge) bridge.send({ type: "settings-request" });
            requestDiagnostics();
          }, 500);
        }
      });
    }
  }

  function openPenguLoaderUI() {
    if (bridge) bridge.send({
      type: "open-pengu-loader-ui",
    });
    log("info", "Open Pengu Loader UI requested");
  }

  function closeSettingsPanel() {
    if (!settingsPanel) return;

    // Disable selected nav item
    const navItem = document.querySelector(".menu_item_Golden");
    if (navItem) {
      navItem.removeAttribute("active")
    }

    // Restore last active item
    const lastActiveNavItem = document.querySelector(".main-nav-bar > * > lol-uikit-navigation-item[chudLastActive]");
    if (lastActiveNavItem) {
      lastActiveNavItem.removeAttribute("chudLastActive")
      lastActiveNavItem.setAttribute("active", true);
    }

    // Cancel any pending reposition timer to avoid a "one-frame" flicker after closing.
    try {
      if (_flyoutRepositionTimer) {
        clearTimeout(_flyoutRepositionTimer);
        _flyoutRepositionTimer = null;
      }
    } catch (e) {}

    // If troubleshooting dialog is open, close it too (it is a separate fixed overlay).
    try {
      const diag = document.getElementById("chud-diagnostics-dialog");
      if (diag) diag.remove();
      diagnosticsDialog = null;
    } catch (e) {}

    const cleanup = () => {
      try {
        if (settingsPanel) settingsPanel.remove();
      } catch (e) {}
      settingsPanel = null;
    };

    // Prefer the built-in flyout animation when available.
    let flyout = null;
    try {
      flyout = document.getElementById(FLYOUT_ID);
    } catch (e) {
      flyout = null;
    }

    if (flyout) {
      // Disable interactions immediately while closing.
      try {
        flyout.style.pointerEvents = "none";
      } catch (e) {}

      // Smooth close (avoid scale/pop + avoid one-frame re-appearance).
      try {
        const baseTransform = flyout.style.transform || "";
        flyout.style.willChange = "opacity, transform";
        flyout.style.transition =
          "opacity 180ms cubic-bezier(0.22, 1, 0.36, 1), transform 180ms cubic-bezier(0.22, 1, 0.36, 1)";

        // Apply end-state on next frame so the transition reliably runs.
        requestAnimationFrame(() => {
          try {
            flyout.style.opacity = "0";
            flyout.style.transform = `${baseTransform} translateY(-6px)`;
          } catch (e) {}
        });

        // Cleanup after the transition.
        setTimeout(cleanup, 220);
        return;
      } catch (e) {
        // If something goes wrong, fall back to immediate cleanup.
        cleanup();
        return;
      }
    }

    cleanup();
  }

  // Listen for open settings event from CHUD-UI
  window.addEventListener("chud-open-settings", (e) => {
    const navItem =
      e.detail?.navItem ||
      document.querySelector(
        "lol-uikit-navigation-item.menu_item_Golden.Chud"
      );
    if (navItem) {
      // Toggle: if panel is already open, close it
      if (settingsPanel && document.getElementById(PANEL_ID)) {
        closeSettingsPanel();
      } else {
        createSettingsFlyout(navItem);
      }
    } else {
      log(
        "warn",
        "Could not find Golden Chud nav item to position settings panel"
      );
    }
  });

  // Inject CSS
  function injectCSS() {
    // Remove existing CSS if it exists (to update with correct port)
    const existingStyle = document.getElementById("chud-settings-panel-css");
    if (existingStyle) {
      existingStyle.remove();
    }

    const style = document.createElement("style");
    style.id = "chud-settings-panel-css";
    style.textContent = getCSSRules();
    document.head.appendChild(style);
  }

  let _initializing = false;
  let _initialized = false;
  let _retryCount = 0;
  const MAX_RETRIES = 100; // Maximum number of retry attempts

  async function init() {
    // Prevent multiple concurrent initializations (but allow recursive retry)
    if (_initialized) {
      return;
    }
    // If already initializing, only proceed if this is a recursive retry call
    // (indicated by document being ready now when it wasn't before)
    if (_initializing) {
      // Allow recursive call to proceed only if document is now ready
      if (!document || !document.head) {
        // Check retry limit to prevent unbounded retries
        if (_retryCount >= MAX_RETRIES) {
          log("error", `Init failed: Maximum retry count (${MAX_RETRIES}) reached. Document still not ready.`);
          _initializing = false;
          _retryCount = 0; // Reset for next attempt
          return;
        }
        _retryCount++;
        // Still not ready, schedule another retry
        requestAnimationFrame(() => {
          init().catch(err => {
            log("error", "Init failed:", err);
            _initializing = false;
          });
        });
        return;
      }
      // Document is now ready, proceed with initialization
    } else {
      // First call - set flag BEFORE document check to prevent race condition
      _initializing = true;
      // Don't reset retry counter here - it should persist across retries
      // Only reset on successful initialization

      if (!document || !document.head) {
        // Check retry limit BEFORE incrementing to prevent unbounded retries
        if (_retryCount >= MAX_RETRIES) {
          log("error", `Init failed: Maximum retry count (${MAX_RETRIES}) reached. Document still not ready.`);
          _initializing = false;
          _retryCount = 0; // Reset for next attempt
          return;
        }
        _retryCount++;
        // Use synchronous wrapper to prevent multiple concurrent schedules
        requestAnimationFrame(() => {
          init().catch(err => {
            log("error", "Init failed:", err);
            _initializing = false;
          });
        });
        return;
      }
    }
    try {
      // Wait for the shared bridge to become available
      bridge = await waitForBridge();

      // Inject CSS after bridge is loaded (so it has the correct port number)
      injectCSS();

      // Subscribe to all message types
      bridge.subscribe("settings-data", handleSettingsData);
      bridge.subscribe("settings-saved", handleSettingsSaved);
      bridge.subscribe("diagnostics-data", handleDiagnosticsData);
      bridge.subscribe("diagnostics-cleared-category", () => requestDiagnostics());
      bridge.subscribe("diagnostics-tracker-cleared", () => requestDiagnostics());
      bridge.subscribe("diagnostics-applied-recommended", () => requestDiagnostics());
      bridge.subscribe("path-validation-result", handlePathValidationResult);
      bridge.subscribe("champions-list-response", handleChampionsListResponse);
      bridge.subscribe("champion-skins-response", handleChampionSkinsResponse);
      bridge.subscribe("folder-opened-response", handleFolderOpenedResponse);

      // On every (re)connect, sync state
      bridge.onReady(() => {
        requestSettings();
        requestDiagnostics();
        startBadgeObserver();

        // Poll diagnostics so warnings appear without opening the panel.
        if (!_diagnosticsPollId) {
          _diagnosticsPollId = setInterval(() => {
            try {
              if (!bridge || !bridge.ready) return;
              if (typeof document !== "undefined" && document.hidden) return;
              requestDiagnostics();
            } catch (e) {}
          }, 15000);
        }
      });

      log("info", "Settings panel plugin initialized");
      _initialized = true;
      _retryCount = 0; // Reset retry counter on success
    } catch (err) {
      log("error", "Init failed:", err);
      throw err; // Re-throw to propagate error to .catch() handlers
    } finally {
      _initializing = false;
    }
  }

  if (typeof document === "undefined") {
    log("warn", "document unavailable; aborting");
    return;
  }

  if (document.readyState === "loading") {
    document.addEventListener(
      "DOMContentLoaded",
      () => {
        init().catch((err) => {
          log("error", "Init failed:", err);
        });
      },
      { once: true }
    );
  } else {
    init().catch((err) => {
      log("error", "Init failed:", err);
    });
  }
})();