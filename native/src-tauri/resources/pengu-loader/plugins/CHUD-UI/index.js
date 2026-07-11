/**
 * @name Chud-UI
 * @author Chud Team
 * @description Interface unlocker for Pengu Loader
 * @link https://github.com/joshua-ivy/League-Of-Legends-Auto-Accept-Range
 */
(function enableLockedSkinPreview() {
  const LOG_PREFIX = "[Chud-UI][skin-preview]";
  const INLINE_ID = "lpp-ui-unlock-skins-css-inline";
  const BORDER_CLASS = "lpp-skin-border";
  const HIDDEN_CLASS = "lpp-skin-hidden";
  const CHROMA_CONTAINER_CLASS = "lpp-chroma-container";
  const VISIBLE_OFFSETS = new Set([0, 1, 2, 3, 4]);

  // Welcome popup
  const WELCOME_STYLE_ID = "chud-welcome-css-inline";
  const WELCOME_DIALOG_ID = "chud-welcome-dialog";
  const WELCOME_STORAGE_KEY = "chud_welcome_dismissed";
  // Set once the user clicks OK, so the popup stays dismissed for the rest of
  // this client session even if they didn't tick "Do not show again" and the
  // plugin's init re-runs on client navigation.
  let welcomeHandledThisSession = false;
  const WELCOME_DISCORD_URL = "https://discord.gg/a2QTg7btaT";
  const WELCOME_GITHUB_URL = "https://github.com/joshua-ivy/League-Of-Legends-Auto-Accept-Range";

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

  let lastBaseSkinSkipRequest = 0;
  const BASE_SKIN_SKIP_REQUEST_TIME_WINDOW_MS = 5000;

  function handleSkipBaseSkin(payload) {
    lastBaseSkinSkipRequest = Date.now();
    log.info("received base skin skip request from chud");
  }

  // TODO Preferably move bridge communication logic and websocket interception to a separate Pengu plugin like CHUD-CORE,
  // which provides a simple interface for adding custom observers for bridge and socket instead of duplicating this kind
  // of code over all the plugins; this will do for now though
  function interceptChampSelectWebsocket() {
    window.rcp.postInit("rcp-fe-lol-champ-select", (api) => {
      try {
        const ws = api.champSelectBinding.socket._websocket;
        const parentOnMessage = ws.onmessage;

        ws.onmessage = function (event) {
          try {
            const payload = JSON.parse(event.data);
            if (payload[1] == "OnJsonApiEvent") {
              const eventData = payload[2];
              if (eventData["uri"] == "/lol-champ-select/v1/skin-selector-info") {
                // // **Bridge-less implementation** (don't use: bridge implementation is more reliable)
                //
                // const data = eventData["data"];
                // 
                // data is null in event type DELETE
                // check if base skin
                // if (data?.["selectedSkinId"] % 1000 == 0) {
                //   log.info("skipping base skin");
                //   // skip delegation
                //   return;
                // }

                // Not a DELETE event
                if (eventData["data"]?.["selectedSkinId"] != 0) {
                  if (Date.now() - lastBaseSkinSkipRequest < BASE_SKIN_SKIP_REQUEST_TIME_WINDOW_MS) {
                    log.info("skipping base skin");
                    // skip delegation
                    return;
                  } else {
                    log.info("not skipping base skin: no request received from chud (in time)");
                  }
                }
              }
            }

            return parentOnMessage.call(this, event);
          } catch(e) {
            log.error("Error during WebSocket response parse: ", e);
          }
        };
        log.info("Websocket Interception successful");
      } catch (e) {
        log.error("Failed WebSocket interception: ", e);
      }
    });
  }

  const INLINE_RULES = `
    lol-uikit-navigation-item.menu_item_Golden\\ Chud {
      position: relative;
    }

    /* Prevent active state styling for Golden Chud */
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section.active::before,
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section.active::after,
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section.active,
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section.active .section-glow,
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section.active .section-glow-container {
      display: none !important;
      background: none !important;
      background-image: none !important;
    }

    /* Prevent hover state from showing navigation pointer */
    lol-uikit-navigation-item.menu_item_Golden\\ Chud .section:hover::after {
      opacity: 0 !important;
      background: none !important;
      background-image: none !important;
    }

    .skin-selection-carousel .skin-selection-item {
      position: relative;
      z-index: 1;
    }

    .skin-selection-carousel .skin-selection-item .skin-selection-item-information {
      position: relative;
      z-index: 2;
    }

    .skin-selection-carousel .skin-selection-item.disabled,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"] {
      filter: grayscale(0) saturate(1.1) contrast(1.05) !important;
      -webkit-filter: grayscale(0) saturate(1.1) contrast(1.05) !important;
      pointer-events: auto !important;
      cursor: pointer !important;
    }

    .skin-selection-carousel .skin-selection-item.disabled .skin-selection-thumbnail,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"] .skin-selection-thumbnail {
      filter: grayscale(0) saturate(1.15) contrast(1.05) !important;
      -webkit-filter: grayscale(0) saturate(1.15) contrast(1.05) !important;
      transition: filter 0.25s ease;
    }

    /* Hover glow effect for owned skins (matching official client) */
    .skin-selection-carousel .skin-selection-item:not(.disabled):not([aria-disabled="true"]):not(.skin-selection-item-selected):hover .skin-selection-thumbnail {
      filter: brightness(1.2) saturate(1.1) !important;
      -webkit-filter: brightness(1.2) saturate(1.1) !important;
      transition: filter 0.25s ease;
    }

    /* Hover glow effect for unowned skins (identical to owned - override base filters on hover) */
    .skin-selection-carousel .skin-selection-item.disabled:not(.skin-selection-item-selected):hover .skin-selection-thumbnail,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"]:not(.skin-selection-item-selected):hover .skin-selection-thumbnail {
      filter: brightness(1.2) saturate(1.1) !important;
      -webkit-filter: brightness(1.2) saturate(1.1) !important;
      transition: filter 0.25s ease;
    }

    .skin-selection-carousel .skin-selection-item.disabled::before,
    .skin-selection-carousel .skin-selection-item.disabled::after,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"]::before,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"]::after,
    .skin-selection-carousel .skin-selection-item.disabled .skin-selection-thumbnail::before,
    .skin-selection-carousel .skin-selection-item.disabled .skin-selection-thumbnail::after,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"] .skin-selection-thumbnail::before,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"] .skin-selection-thumbnail::after {
      display: none !important;
    }

    .skin-selection-carousel .skin-selection-item.disabled .locked-state,
    .skin-selection-carousel .skin-selection-item[aria-disabled="true"] .locked-state {
      display: none !important;
    }

    .skin-selection-carousel .skin-selection-item.${HIDDEN_CLASS} {
      pointer-events: none !important;
    }

    .champion-select .uikit-background-switcher.locked:after {
      background: none !important;
    }

    .unlock-skin-hit-area {
      display: none !important;
      pointer-events: none !important;
    }

    .unlock-skin-hit-area .locked-state {
      display: none !important;
    }

    .skin-selection-carousel-container .skin-selection-carousel .skin-selection-item .skin-selection-thumbnail {
      height: 100% !important;
      margin: 0 !important;
      transition: filter 0.25s ease !important;
      transform: none !important;
    }

    .skin-selection-carousel-container .skin-selection-carousel .skin-selection-item.skin-selection-item-selected {
      background: #2a3350 !important;
    }

    .skin-selection-carousel-container .skin-selection-carousel .skin-selection-item.skin-selection-item-selected .skin-selection-thumbnail {
      height: 100% !important;
      margin: 0 !important;
    }

    .skin-selection-carousel .skin-selection-item .lpp-skin-border {
      position: absolute;
      inset: -2px;
      border: 2px solid transparent;
      border-image-source: linear-gradient(0deg, #4f4f54 0%, #2a3350 50%, #29272b 100%);
      border-image-slice: 1;
      border-radius: inherit;
      box-sizing: border-box;
      pointer-events: none;
      z-index: 0;
    }

    .skin-selection-carousel .skin-selection-item.skin-carousel-offset-2 .lpp-skin-border {
      border: 2px solid transparent;
      border-image-source: linear-gradient(0deg, #35e4ff 0%, #2ea6d6 44%, #2389a8 59%, #1b5566 100%);
      border-image-slice: 1;
      box-shadow: inset 0 0 0 1px rgba(1, 10, 19, 0.6);
    }

    /* Golden border on hover for all skins (matching official client) */
    .skin-selection-carousel .skin-selection-item:not(.skin-selection-item-selected):hover .lpp-skin-border {
      border: 2px solid transparent;
      border-image-source: linear-gradient(0deg, #35e4ff 0%, #2ea6d6 44%, #2389a8 59%, #1b5566 100%);
      border-image-slice: 1;
      box-shadow: inset 0 0 0 1px rgba(1, 10, 19, 0.6);
    }

    .skin-selection-carousel .skin-selection-item .${CHROMA_CONTAINER_CLASS} {
      position: absolute;
      inset: 0;
      display: flex;
      align-items: flex-end;
      justify-content: center;
      pointer-events: none;
      z-index: 4;
      overflow: hidden;
    }

    .skin-selection-carousel .skin-selection-item .${CHROMA_CONTAINER_CLASS} .chroma-button {
      pointer-events: auto;
    }

    .chroma-button.chroma-selection {
      display: none !important;
    }

    /* Remove grey filters and locks */
    .thumbnail-wrapper {
      filter: grayscale(0) saturate(1) contrast(1) !important;
      -webkit-filter: grayscale(0) saturate(1) contrast(1) !important;
    }

    .skin-thumbnail-img {
      filter: grayscale(0) saturate(1) contrast(1) !important;
      -webkit-filter: grayscale(0) saturate(1) contrast(1) !important;
    }

    .locked-state {
      display: none !important;
    }

    .unlock-skin-hit-area {
      display: none !important;
      pointer-events: none !important;
    }

    .skin-selection-carousel-container {
      clip-path: inset(-200px -9999px -9999px -9999px) !important;
    }
  `;

  const log = {
    info: (msg, extra) => console.info(`${LOG_PREFIX} ${msg}`, extra ?? ""),
    warn: (msg, extra) => console.warn(`${LOG_PREFIX} ${msg}`, extra ?? ""),
    error: (msg, extra) => console.error(`${LOG_PREFIX} ${msg}`, extra ?? ""),
  };

  function injectInlineRules() {
    if (document.getElementById(INLINE_ID)) {
      return;
    }

    const styleTag = document.createElement("style");
    styleTag.id = INLINE_ID;
    styleTag.textContent = INLINE_RULES;
    document.head.appendChild(styleTag);
    log.info("inline styles applied");
  }

  const WELCOME_STYLES = `
    #${WELCOME_DIALOG_ID} {
      position: fixed;
      top: 0;
      left: 0;
      width: 100%;
      height: 100%;
      z-index: 2147483000;
      pointer-events: auto;
      display: flex;
      align-items: center;
      justify-content: center;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-backdrop {
      position: absolute;
      top: 0;
      left: 0;
      width: 100%;
      height: 100%;
      background: rgba(7, 11, 22, 0.75);
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-panel {
      position: relative;
      width: 360px;
      max-width: 90vw;
      background: #0b1120;
      border: 1px solid #35e4ff;
      border-radius: 6px;
      box-shadow: 0 10px 40px rgba(0, 0, 0, 0.6), 0 0 30px rgba(53, 228, 255, 0.15);
      padding: 24px;
      text-align: center;
      font-family: "JetBrains Mono", monospace;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-logo {
      width: 56px;
      height: 56px;
      margin: 0 auto 12px;
      display: block;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-title {
      color: #35e4ff;
      font-size: 20px;
      font-weight: 700;
      letter-spacing: 1px;
      margin-bottom: 8px;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-message {
      color: #dff3ff;
      font-size: 13px;
      line-height: 1.5;
      margin-bottom: 18px;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-links {
      display: flex;
      gap: 10px;
      margin-bottom: 18px;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-link {
      flex: 1;
      display: inline-block;
      padding: 8px 0;
      border-radius: 4px;
      font-size: 12px;
      font-weight: 700;
      text-decoration: none;
      cursor: pointer;
      transition: filter 0.2s, opacity 0.2s;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-link:hover {
      filter: brightness(1.15);
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-link-discord {
      background: linear-gradient(90deg, #35e4ff, #ff5cc8);
      color: #070b16;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-link-github {
      background: transparent;
      border: 1px solid #35e4ff;
      color: #35e4ff;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-footer {
      display: flex;
      align-items: center;
      justify-content: space-between;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-checkbox-label {
      display: flex;
      align-items: center;
      gap: 6px;
      color: #7a93a8;
      font-size: 11px;
      cursor: pointer;
      user-select: none;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-ok {
      padding: 6px 20px;
      border-radius: 4px;
      border: none;
      background: #35e4ff;
      color: #070b16;
      font-family: "JetBrains Mono", monospace;
      font-size: 12px;
      font-weight: 700;
      cursor: pointer;
      transition: background 0.2s;
    }

    #${WELCOME_DIALOG_ID} .chud-welcome-ok:hover {
      background: #7ceeff;
    }
  `;

  function injectWelcomeStyles() {
    if (document.getElementById(WELCOME_STYLE_ID)) {
      return;
    }

    const styleTag = document.createElement("style");
    styleTag.id = WELCOME_STYLE_ID;
    styleTag.textContent = WELCOME_STYLES;
    document.head.appendChild(styleTag);
  }

  function isWelcomeDismissed() {
    try {
      return localStorage.getItem(WELCOME_STORAGE_KEY) === "true";
    } catch (e) {
      return false;
    }
  }

  function showWelcomePopup(bridgePort) {
    if (welcomeHandledThisSession || document.getElementById(WELCOME_DIALOG_ID) || isWelcomeDismissed()) {
      return;
    }

    injectWelcomeStyles();

    const dialog = document.createElement("div");
    dialog.id = WELCOME_DIALOG_ID;

    const backdrop = document.createElement("div");
    backdrop.className = "chud-welcome-backdrop";
    dialog.appendChild(backdrop);

    const panel = document.createElement("div");
    panel.className = "chud-welcome-panel";

    const logo = document.createElement("img");
    logo.className = "chud-welcome-logo";
    logo.src = `http://127.0.0.1:${bridgePort}/asset/chud_logo.png`;
    logo.alt = "Chud";
    panel.appendChild(logo);

    const title = document.createElement("div");
    title.className = "chud-welcome-title";
    title.textContent = "Chud";
    panel.appendChild(title);

    const message = document.createElement("div");
    message.className = "chud-welcome-message";
    message.textContent = "Your League skins are unlocked. Pick any skin in champ select.";
    panel.appendChild(message);

    const links = document.createElement("div");
    links.className = "chud-welcome-links";

    const discordLink = document.createElement("a");
    discordLink.className = "chud-welcome-link chud-welcome-link-discord";
    discordLink.href = WELCOME_DISCORD_URL;
    discordLink.target = "_blank";
    discordLink.textContent = "Discord";
    links.appendChild(discordLink);

    const githubLink = document.createElement("a");
    githubLink.className = "chud-welcome-link chud-welcome-link-github";
    githubLink.href = WELCOME_GITHUB_URL;
    githubLink.target = "_blank";
    githubLink.textContent = "GitHub";
    links.appendChild(githubLink);

    panel.appendChild(links);

    const footer = document.createElement("div");
    footer.className = "chud-welcome-footer";

    const checkboxLabel = document.createElement("label");
    checkboxLabel.className = "chud-welcome-checkbox-label";

    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.id = "chud-welcome-dismiss-checkbox";

    checkboxLabel.appendChild(checkbox);
    checkboxLabel.appendChild(document.createTextNode("Do not show again"));
    footer.appendChild(checkboxLabel);

    const okButton = document.createElement("button");
    okButton.type = "button";
    okButton.className = "chud-welcome-ok";
    okButton.textContent = "OK";
    const dismissWelcome = () => {
      welcomeHandledThisSession = true;
      if (checkbox.checked) {
        try {
          localStorage.setItem(WELCOME_STORAGE_KEY, "true");
        } catch (e) {
          // ignore storage failures (e.g. private mode)
        }
      }
      // Remove EVERY welcome dialog, not just this closure's `dialog` — if the
      // plugin init ran more than once, several identical dialogs can stack and
      // removing one just reveals another, so it looks like OK does nothing.
      document.querySelectorAll(`#${WELCOME_DIALOG_ID}`).forEach((el) => el.remove());
    };
    okButton.addEventListener("click", dismissWelcome);
    // Clicking the dimmed backdrop also closes it — an escape hatch.
    backdrop.addEventListener("click", dismissWelcome);
    footer.appendChild(okButton);

    panel.appendChild(footer);
    dialog.appendChild(panel);
    document.body.appendChild(dialog);

    // Auto-dismiss after 3s so the popup can never get stuck, regardless of
    // whether the OK/backdrop clicks land in the client's Ember-managed DOM.
    setTimeout(dismissWelcome, 3000);

    log.info("welcome popup shown");
  }

  function ensureBorderFrame(skinItem) {
    if (!skinItem) {
      return;
    }

    let border = skinItem.querySelector(`.${BORDER_CLASS}`);
    if (!border) {
      border = document.createElement("div");
      border.className = BORDER_CLASS;
      border.setAttribute("aria-hidden", "true");
    }

    const chromaContainer = skinItem.querySelector(
      `.${CHROMA_CONTAINER_CLASS}`
    );
    if (chromaContainer && border.nextSibling !== chromaContainer) {
      skinItem.insertBefore(border, chromaContainer);
      return;
    }

    if (border.parentElement !== skinItem || border !== skinItem.firstChild) {
      skinItem.insertBefore(border, skinItem.firstChild || null);
    }
  }

  function ensureChromaContainer(skinItem) {
    if (!skinItem) {
      return;
    }

    const chromaButton = skinItem.querySelector(".outer-mask .chroma-button");
    if (!chromaButton) {
      return;
    }

    let container = skinItem.querySelector(`.${CHROMA_CONTAINER_CLASS}`);
    if (!container) {
      container = document.createElement("div");
      container.className = CHROMA_CONTAINER_CLASS;
      container.setAttribute("aria-hidden", "true");
      skinItem.appendChild(container);
    } else if (container.parentElement !== skinItem) {
      skinItem.appendChild(container);
    }

    if (
      container.previousSibling &&
      !container.previousSibling.classList?.contains(BORDER_CLASS)
    ) {
      const border = skinItem.querySelector(`.${BORDER_CLASS}`);
      if (border) {
        skinItem.insertBefore(border, container);
      }
    }

    if (chromaButton.parentElement !== container) {
      container.appendChild(chromaButton);
    }
  }

  function parseCarouselOffset(skinItem) {
    const offsetClass = Array.from(skinItem.classList).find((cls) =>
      cls.startsWith("skin-carousel-offset")
    );
    if (!offsetClass) {
      return null;
    }

    const match = offsetClass.match(/skin-carousel-offset-(-?\d+)/);
    if (!match) {
      return null;
    }

    const value = Number.parseInt(match[1], 10);
    return Number.isNaN(value) ? null : value;
  }

  function isOffsetVisible(offset) {
    if (offset === null) {
      return true;
    }

    return VISIBLE_OFFSETS.has(offset);
  }

  function applyOffsetVisibility(skinItem) {
    if (!skinItem) {
      return;
    }

    const offset = parseCarouselOffset(skinItem);
    const shouldBeVisible = isOffsetVisible(offset);

    skinItem.classList.toggle("lpp-visible-skin", shouldBeVisible);
    skinItem.classList.toggle(HIDDEN_CLASS, !shouldBeVisible);

    if (shouldBeVisible) {
      skinItem.style.removeProperty("pointer-events");
    } else {
      skinItem.style.setProperty("pointer-events", "none", "important");
    }
  }

  function markSkinsAsOwned() {
    // Remove unowned class and add owned class to thumbnail-wrapper elements
    document
      .querySelectorAll(".thumbnail-wrapper.unowned")
      .forEach((wrapper) => {
        wrapper.classList.remove("unowned");
        wrapper.classList.add("owned");
      });

    // Replace purchase-available with active
    document.querySelectorAll(".purchase-available").forEach((element) => {
      element.classList.remove("purchase-available");
      element.classList.add("active");
    });

    // Remove purchase-disabled class from any element
    document.querySelectorAll(".purchase-disabled").forEach((element) => {
      element.classList.remove("purchase-disabled");
    });
  }

  function removeAgeRatingInChampSelect() {
    if (!document.querySelector(".champion-select") && !document.querySelector(".skin-selection-carousel")) {
      return;
    }
    document.querySelectorAll(".vng-age-rating").forEach((el) => el.remove());
    document.querySelectorAll(".vng-age-rating-container").forEach((el) => el.remove());
  }

  function scanSkinSelection() {
    injectInlineRules();

    document.querySelectorAll(".skin-selection-item").forEach((skinItem) => {
      ensureChromaContainer(skinItem);
      ensureBorderFrame(skinItem);
      applyOffsetVisibility(skinItem);
    });

    // Mark skins as owned in Swiftplay
    markSkinsAsOwned();

    // Remove age rating classes when in champ select
    removeAgeRatingInChampSelect();
  }

  function setupSkinObserver() {
    const observer = new MutationObserver(() => {
      scanSkinSelection();
      markSkinsAsOwned();
    });
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["class"],
    });

    // Re-scan periodically as a safety net (LCU sometimes swaps DOM wholesale)
    const intervalId = setInterval(() => {
      scanSkinSelection();
      markSkinsAsOwned();
    }, 500);

    const handleResize = () => {
      scanSkinSelection();
    };
    window.addEventListener("resize", handleResize, { passive: true });

    document.addEventListener(
      "visibilitychange",
      () => {
        if (document.visibilityState === "visible") {
          scanSkinSelection();
        }
      },
      false
    );

    // Return cleanup in case we ever need it
    return () => {
      observer.disconnect();
      clearInterval(intervalId);
      window.removeEventListener("resize", handleResize);
    };
  }

  // Observer lifecycle - only run during ChampSelect/FINALIZATION.
  // See GitHub issue #22: the 500ms poll + MutationObserver steal CPU
  // from the League game process during matches.
  let skinObserverCleanup = null;

  function startSkinObserverGated() {
    if (skinObserverCleanup) return;
    skinObserverCleanup = setupSkinObserver();
  }

  function stopSkinObserverGated() {
    if (!skinObserverCleanup) return;
    try {
      skinObserverCleanup();
    } catch (e) {
      // ignore cleanup errors
    }
    skinObserverCleanup = null;
  }

  function handlePhaseChangeFromPython(data) {
    const phase = data && data.phase;
    if (!phase) return;
    // Stop only during actively-playing InProgress.  markSkinsAsOwned() is
    // Swiftplay-specific and runs during Lobby phase, so we can't restrict
    // to ChampSelect.  See GitHub issue #22.
    if (phase === "InProgress") {
      stopSkinObserverGated();
    } else {
      startSkinObserverGated();
    }
  }

  function attachGoldenChudListeners(navItem) {
    // Check if listeners already attached
    if (navItem.dataset.lppDiscordAttached === "true") {
      return;
    }

    // Add click handler to nav item - open settings panel
    navItem.addEventListener(
      "click",
      (e) => {
        const lastActiveNavItem = document.querySelector(".main-nav-bar > * > lol-uikit-navigation-item[active]");
        if (lastActiveNavItem) {
          lastActiveNavItem.setAttribute("chudLastActive", true);
        }

        // Dispatch event to open settings panel
        const event = new CustomEvent("chud-open-settings", {
          detail: { navItem: navItem },
          bubbles: true,
          cancelable: true,
        });
        window.dispatchEvent(event);
        log.info("Dispatched chud-open-settings event from Golden Chud button");
      },
      true
    ); // Use capture phase to intercept early

    // Also prevent section click from bubbling up - wait for section to exist
    const setupSectionHandlers = () => {
      const section = navItem.querySelector(".section");
      if (section && !section.dataset.lppDiscordHandler) {
        section.dataset.lppDiscordHandler = "true";

        section.addEventListener(
          "click",
          (e) => {
            e.stopPropagation();
            e.preventDefault();

            // Dispatch event to open settings panel
            const event = new CustomEvent("chud-open-settings", {
              detail: { navItem: navItem },
              bubbles: true,
              cancelable: true,
            });
            window.dispatchEvent(event);
            log.info(
              "Dispatched chud-open-settings event from Golden Chud section"
            );

            // Prevent active class
            section.classList.remove("active");
          },
          true
        );

        // Watch for active class being added and remove it immediately
        const activeObserver = new MutationObserver((mutations) => {
          mutations.forEach((mutation) => {
            if (
              mutation.type === "attributes" &&
              mutation.attributeName === "class"
            ) {
              if (section.classList.contains("active")) {
                section.classList.remove("active");
              }
            }
          });
        });

        activeObserver.observe(section, {
          attributes: true,
          attributeFilter: ["class"],
        });

        // Store observer reference for cleanup if needed
        navItem.dataset.lppActiveObserver = "true";
        return true;
      }
      return false;
    };

    // Try immediately, then watch for section to appear
    if (!setupSectionHandlers()) {
      const sectionObserver = new MutationObserver(() => {
        if (setupSectionHandlers()) {
          sectionObserver.disconnect();
        }
      });

      sectionObserver.observe(navItem, {
        childList: true,
        subtree: true,
      });

      // Also try after a short delay (Ember might take time to initialize)
      setTimeout(() => {
        setupSectionHandlers();
        sectionObserver.disconnect();
      }, 500);
    }

    // Mark as attached
    navItem.dataset.lppDiscordAttached = "true";
  }

  function injectGoldenChudNavItem() {
    const rightNavMenu = document.querySelector(".right-nav-menu");
    if (!rightNavMenu) {
      return false;
    }

    // Check if Golden Chud item already exists by checking for the chud_emblem.png image
    const existingItem = rightNavMenu.querySelector(
      'lol-uikit-navigation-item .menu-item-icon[style*="chud_emblem.png"]'
    );
    if (existingItem) {
      const navItem = existingItem.closest("lol-uikit-navigation-item");
      if (navItem) {
        attachGoldenChudListeners(navItem);
      }
      return true;
    }

    // Create the navigation item
    const navItem = document.createElement("lol-uikit-navigation-item");
    navItem.id = `ember${Date.now()}`;
    navItem.className =
      "main-navigation-menu-item menu_item_Golden Chud ember-view";

    // Create icon wrapper structure
    const iconWrapper = document.createElement("div");
    iconWrapper.className = "menu-item-icon-wrapper";

    const glow = document.createElement("div");
    glow.className = "menu-item-glow";

    const icon = document.createElement("div");
    icon.className = "menu-item-icon";
    // Render the actual gold emblem art (not a flat mask silhouette) so the
    // ornate detail shows. Keep "chud_emblem.png" in the style string so the
    // existing dedup check (`[style*="chud_emblem.png"]`) still matches.
    const emblemUrl = `http://127.0.0.1:${window.__chudBridge ? window.__chudBridge.port : 50000}/asset/chud_emblem.png`;
    icon.style.webkitMaskImage = "none";
    icon.style.maskImage = "none";
    icon.style.backgroundColor = "transparent";
    icon.style.backgroundImage = `url(${emblemUrl})`;
    icon.style.backgroundSize = "contain";
    icon.style.backgroundRepeat = "no-repeat";
    icon.style.backgroundPosition = "center";
    icon.style.width = "28px";
    icon.style.height = "28px";

    iconWrapper.appendChild(glow);
    iconWrapper.appendChild(icon);
    navItem.appendChild(iconWrapper);

    // Insert at the beginning of the nav menu
    const firstChild = rightNavMenu.firstChild;
    if (firstChild) {
      rightNavMenu.insertBefore(navItem, firstChild);
    } else {
      rightNavMenu.appendChild(navItem);
    }

    // Add separator after the Golden Chud item
    const separator = document.createElement("div");
    separator.className = "right-nav-vertical-rule";
    rightNavMenu.insertBefore(separator, navItem.nextSibling);

    // Attach Discord click listeners
    attachGoldenChudListeners(navItem);

    log.info("Golden Chud navigation item injected");
    return true;
  }

  function setupNavObserver() {
    // Try to inject immediately
    if (injectGoldenChudNavItem()) {
      return;
    }

    // If not found, observe for nav menu creation
    const observer = new MutationObserver(() => {
      if (injectGoldenChudNavItem()) {
        observer.disconnect();
      }
    });

    observer.observe(document.body, {
      childList: true,
      subtree: true,
    });

    // Also check periodically as a safety net
    const intervalId = setInterval(() => {
      if (injectGoldenChudNavItem()) {
        clearInterval(intervalId);
        observer.disconnect();
      }
    }, 500);

    // Cleanup after a reasonable time
    setTimeout(() => {
      observer.disconnect();
      clearInterval(intervalId);
    }, 30000);
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
          log.error(
            `Init failed: Maximum retry count (${MAX_RETRIES}) reached. Document still not ready.`
          );
          _initializing = false;
          _retryCount = 0; // Reset for next attempt
          return;
        }
        _retryCount++;
        // Still not ready, schedule another retry
        requestAnimationFrame(() => {
          init().catch((err) => {
            log.error("Init failed:", err);
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
          log.error(
            `Init failed: Maximum retry count (${MAX_RETRIES}) reached. Document still not ready.`
          );
          _initializing = false;
          _retryCount = 0; // Reset for next attempt
          return;
        }
        _retryCount++;
        // Use synchronous wrapper to prevent multiple concurrent schedules
        requestAnimationFrame(() => {
          init().catch((err) => {
            log.error("Init failed:", err);
            _initializing = false;
          });
        });
        return;
      }
    }
    
    try {
      // Wait for bridge to be available (provides port)
      const bridge = await waitForBridge();

      // Subscribe to skip-base-skin messages from the shared bridge
      bridge.subscribe("skip-base-skin", handleSkipBaseSkin);
      bridge.subscribe("phase-change", handlePhaseChangeFromPython);

      // Show the Chud welcome popup once per client session (persisted via localStorage)
      showWelcomePopup(bridge.port);

      interceptChampSelectWebsocket();
      injectInlineRules();
      scanSkinSelection();
      // Default-on: first phase-change from Python will shut the observer
      // off again if we're already in-game.  See issue #22.
      startSkinObserverGated();
      setupNavObserver();
      log.info("skin preview overrides active");
      _initialized = true;
      _retryCount = 0; // Reset retry counter on success
    } catch (err) {
      log.error("Init failed:", err);
      throw err; // Re-throw to propagate error to .catch() handlers
    } finally {
      _initializing = false;
    }
  }

  if (typeof document === "undefined") {
    log.warn("document unavailable; aborting");
    return;
  }

  if (document.readyState === "loading") {
    document.addEventListener(
      "DOMContentLoaded",
      () => {
        init().catch((err) => {
          log.error("Init failed:", err);
        });
      },
      { once: true }
    );
  } else {
    init().catch((err) => {
      log.error("Init failed:", err);
    });
  }
})();
