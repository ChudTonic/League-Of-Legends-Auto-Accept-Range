# Credits

Chud Skins is a Rust rebuild of the skin-changer feature set pioneered by the
**Rose** project. None of the underlying approach — LCU polling, cslol-based
WAD overlay injection, a Pengu Loader bridge to the League client's CEF UI —
originated with us. We ported the design to Rust, gave it a single-writer
threading model, and rebranded the bundled JS plugins; the credit for the
feature set belongs to the projects below.

## Rose

Original author: **Alban and Florent** — https://github.com/Alban1911/Rose

> MIT License, Copyright (c) 2026 Alban and Florent.

Rose is the original open-source LoL skin changer this project is derived
from: the LCU integration, injection pipeline, WebSocket bridge protocol, and
Pengu Loader plugin set (chroma wheel, custom wheel, forms wheel, historic
mode, party mode, random skin, settings panel, skin monitor) were designed and
built by the Rose team. Chud Skins ports that design to Rust and rebrands the
bundled plugins (`ROSE-*` → `CHUD-*`); the underlying logic and protocol are
theirs.

## Rose-Remastered

A community reliability fork of Rose (race-condition and stability fixes on
top of the original) — the fork this repository's Rust port and bundled
resources were sourced from. All credit for the underlying feature set still
traces back to Rose above.

## CSLOL-manager (cslol-tools)

`mod-tools.exe`, `wad-extract.exe`, `wad-make.exe` — https://github.com/LeagueToolkit/cslol-manager

> MIT License.

The WAD-overlay mod injection toolchain used to apply skin mods without
touching the client's real files. Bundled unmodified. `cslol-dll.dll` (the
runtime hook) is intentionally **not** bundled or distributed by this project;
users supply their own copy.

## Pengu Loader

`Pengu Loader.exe` and its runtime DLLs — https://github.com/PenguLoader/PenguLoader

> MIT License.

The CEF-injection framework that loads our JavaScript plugins into the League
client UI. Bundled unmodified; only the plugins under `pengu-loader/plugins/`
(originally Rose's `ROSE-*` plugins) are ours to rebrand, and even those are
ports of Rose's original JavaScript, not new work.

---

Per the MIT License, the above copyright notices and permission notices are
preserved. The brand names (`Rose`, `ROSE-*`) have been replaced throughout
this project's own code and bundled plugins with `Chud`/`CHUD-*`; the
attribution above is not affected by that rebrand.
