# Art TODO — DONE (2026-07-10)

RESOLVED: all the rose-flower assets below were replaced with a generated Chud
neon-"C" emblem set (magenta→cyan gradient + glow, matching the Tauri app icon
at `native/src-tauri/icons/`). Regenerated via `scratchpad/gen_chud_art.py`:
`chud_emblem.png`, `chud_logo.png`, `icon.png`, `icon.ico`, `tray_ready.png`,
`tray_starting.png`, and `CHUD-Jade/assets/logo.png`. If Josh later wants
bespoke/higher-fidelity brand art, it drops in over these same filenames.

The original inventory (now historical) follows.

---

S8 renamed files and rebranded plugin text, but the *pixels* underneath a few
renamed assets were still literal rose-flower artwork inherited from the Rose
project. Everything else in `native/src-tauri/resources/assets/` (dice icons,
historic/random flag icons, HOL tier badges, champ-select backgrounds, the
per-skin button folders) is neutral or League-official-style art, not
Rose-branded, and needs no changes.

## Needs replacement art

| File | Current content | Used for |
|---|---|---|
| `native/src-tauri/resources/assets/chud_emblem.png` (was `golden_rose.png`) | Gold/yellow rose silhouette on black | League client nav-bar "settings" button icon — served at `/asset/chud_emblem.png` by the bridge, consumed by the `CHUD-UI` plugin (`injectGoldenChudNavItem`) |
| `native/src-tauri/resources/assets/chud_logo.png` (was `rose_without_bg.png`) | Red rose, transparent background | App logo/splash asset (not currently referenced by any bundled plugin — likely used by the Tauri UI or window icon) |
| `native/src-tauri/resources/assets/tray_ready.png` | Red rose | System tray icon, "ready"/idle state |
| `native/src-tauri/resources/assets/tray_starting.png` | Near-black rose | System tray icon, "starting"/busy state |
| `native/src-tauri/resources/assets/icon.png` | Rose flower on dark green background | Executable/app icon (legacy Rose exe icon — confirm whether Chud's Tauri build still references this vs. `native/src-tauri/icons/`) |
| `native/src-tauri/resources/assets/icon.ico` | Same source art as `icon.png` (not independently viewable, but same origin) | Windows `.ico` counterpart of the above |
| `native/src-tauri/resources/pengu-loader/plugins/CHUD-Jade/assets/logo.png` | Same red rose as `chud_logo.png` | Referenced by `CHUD-Jade`'s disabled Regalia sub-plugins (`RegaliaBackground.js`, `RegaliaBanner.js`, `RegaliaBorder.js`, `RegaliaIcon.js`, `RegaliaTitle.js`) via `/plugins/CHUD-Jade/assets/logo.png`. `CHUD-Jade` ships disabled (`index.js_`), so this is not user-visible until/unless the plugin is enabled — low priority, but the file should still be swapped for consistency before Jade is ever turned on. |

## Confirmed NOT needing changes (checked visually)

`dice-enabled.png` / `dice-disabled.png` (generic dice icon), `historic_flag.png`
(clock icon), `random_flag.png` (refresh/swirl icon), `immortal.png` /
`risen.png` (HOL chroma tier badges — League-style art, no Rose branding),
`tooltip.png`, `red-warning.png`, `hol-button.png` / `hol-button-hover.png`,
`tftm_promotebutton_default.png` / `tftm_promotebutton_pressed.png`,
`champ-select-flyout-background-*`, and all per-champion button subfolders
(`arcanejinx_buttons/`, `djsona_buttons/`, `elementalist_buttons/`,
`fakerahri_buttons/`, `kdasera_buttons/`, `radiantsett_buttons/`,
`rrviego_buttons/`, `sbmorg_buttons/`, `uzal_buttons/`, `uzikaisa_buttons/`).

## Not art, but flagged for completeness

`native/src-tauri/resources/assets/BeaufortforLOL-Bold.ttf` /
`BeaufortforLOL-Regular.ttf` and `sfx-soc-ui-click-generic.ogg` are Riot's own
client font/sound assets (bundled unmodified for UI parity), not Rose
branding — no action needed, listed here only so this doc is a complete
inventory of what was reviewed.
