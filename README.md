<p align="center">
  <img src="native/ui/icons/app.png" width="96" alt="Chud" />
</p>

<h1 align="center">Chud</h1>

<p align="center">
  A small desktop companion for League of Legends — auto-accept your queues, keep range
  indicators on screen, and let the camera find you again. Built with Rust + Tauri,
  ships as one tiny <code>.exe</code>.
</p>

---

![Chud dashboard — Auto-Accept and Auto-Range armed, Camera Assist ready](docs/screenshot-dashboard.png)

## ✨ What it does

| Tool | What it does | Risk |
|------|--------------|------|
| 🛡️ **Auto-Accept** | Watches the League Client (LCU API) and accepts ready checks the instant they pop. | **Lower** — local client API only, no game memory, no input injection. |
| 📏 **Auto-Range** | Holds the *Show Advanced Player Stats* key during a match so range indicators stay visible. Chat-aware, always-on while armed. | **High** — synthetic keyboard input. |
| 🎥 **Camera Assist** *(experimental)* | Spots your champion's health bar on screen and recenters the camera when you drift. | **High** — screen capture + synthetic input. |

Everything runs in one process with a neon-glass dashboard, system tray support, and
live client detection. There's also a built-in **Profile** screen pulled live from the
League Client — rank, recent performance, champion pool, and match history:

![Chud profile screen — ranked stats, recent performance, and match history synced from the League Client](docs/screenshot-profile.png)

> 🧪 **Camera Assist is experimental.** The health-bar detection works on the test
> setups it was tuned on, but hasn't been broadly validated across resolutions, skins,
> and lighting. Expect the occasional missed detection — feedback welcome.

## ⚠️ Read this before using it

**Auto-Range and Camera Assist synthesize input (`SendInput`) and scan the screen.
Riot's Vanguard anti-cheat can detect this and ban the account.** The app operates
openly — there is no anti-cheat evasion, and none will ever be added.

Built-in safeguards:

- 🔒 **Ranked kill-switch** — the injection tools refuse to arm in a confirmed ranked game.
- ✋ **Acknowledgment gate** — they stay locked until you explicitly accept the risk once,
  and they require running as Administrator.
- 🛡️ Auto-Accept is never gated — it's the safe core of the app.

These reduce the risk, they don't remove it. **Don't use this on an account you aren't
willing to lose.**

## 🚀 Getting started

You'll need Windows 10/11, the [Rust toolchain](https://rustup.rs) (MSVC), and the build
prerequisites in [`native/BUILD.md`](native/BUILD.md) (VS Build Tools, NASM, CMake).

```powershell
cargo install tauri-cli --version "^2"   # one-time
cd native
cargo tauri dev                          # run it
```

Build the installer:

```powershell
cd native
cargo tauri build   # MSI + NSIS under src-tauri/target/release/bundle/
```

Or preview just the UI in a browser (no Rust build, mock data):

```powershell
cd native/ui
python -m http.server 8137   # then open http://localhost:8137
```

## ⚙️ Configuration

Settings live in the app's **Settings** screen and persist to
`%APPDATA%\LeagueOfLegendsTools\config.json`. The `safety` section is worth knowing about:

```json
"safety": {
    "block_in_ranked": true,   // refuse to arm injection tools in ranked
    "injection_ack": false,    // flips to true once you accept the ban risk in-app
    "check_interval": 2.5
}
```

## 📁 Project layout

```
native/
  src-tauri/   ← Rust core (Tauri 2): LCU client, tools, tray, safety gates
  ui/          ← front-end — plain HTML/CSS/JS, no bundler (Neon Glass design)
  BUILD.md     ← build prerequisites, step by step
```

## 🙏 Notes

Made for personal use as an accessibility aid. Not affiliated with or endorsed by
Riot Games. League of Legends is a trademark of Riot Games, Inc.
