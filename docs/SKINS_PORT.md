# Chud Skins — Rust port of Rose-Remastered (design + spec)

Source: `C:\Users\geaux\League\Rose-Remastered` (Python, ~150 modules).
Target: this repo, `native/src-tauri/src/skins/` (Rust, Tauri 2) + `native/ui/` pages + bundled Pengu Loader with `CHUD-*` JS plugins.

This doc is the single source of truth for the port. Fixer agents implement against it;
deviations require updating this doc.

## 0. Scope decisions (final)

**Ported to Rust** (everything Python): config/paths/logging foundation, LCU skin features,
injection pipeline (cslol mod-tools orchestration), Pengu websocket/HTTP bridge + full message
protocol, chroma/forms/historic/random/swiftplay feature logic, phase engine, loadout ticker +
injection trigger, party mode (relay client + token codec), skin/hash downloaders.

**Stays JS (rebranded only)**: the Pengu Loader plugins — they execute inside the League
client's CEF, not our process. `ROSE-*` → `CHUD-*`.

**Rewritten in Rust**: relay worker → `workers-rs` Durable Object (`chud-party-relay`).

**Bundled unchanged**: cslol binaries (`mod-tools.exe`, `wad-extract.exe`, `wad-make.exe`),
`Pengu Loader.exe` + its DLLs. `cslol-dll.dll` is NEVER committed/distributed (DMCA) — user
supplies it; SHA-256 allowlist check ported intact (`4a00961…ad1c90` pinned set from Rose
`main/__init__.py`). `hashes.game.txt` (207 MB) is never committed — downloaded at runtime.

**Dropped (dead or superseded — confirmed by review)**:
- `analytics/` entirely (third-party tracking to `leagueunlocked.net`; machine-ID beacon. Gone.)
- `launcher/` + `updater` (Rose self-update from `Alban1911/Rose` — wrong repo even for Rose;
  Chud distributes itself). The *hash-file* updater logic survives (moved to downloads).
- STUN/UDP hole-punch (`stun_client.py`, `udp_transport.py`, `peer_connection.py`,
  `message_types.py`) — fully written but unreferenced; live path is the WS relay.
- `ChampThread` (not wired in `initialize_threads`), `SkinDownloader`/`SmartSkinDownloader`
  (superseded by `RepoDownloader`), `hashes_downloader.py` (duplicate of `hash_updater.py`,
  which is the wired one via launcher), `cslol-diag.exe` wiring, `mods_map.json`,
  `validation.validated_method`, `issue_reporter.clear_issue`, `resolution_utils.py` +
  click-catcher tables (legacy Qt-era UI detection; detection is DOM-side in plugins now),
  win32 tray/settings dialogs + console setup (replaced by Chud's Tauri tray + web UI),
  PyInstaller/_MEIPASS path branching (collapses to exe-relative resources).
- ROSE-Jade plugin ships disabled (`index.js_` upstream, i.e. already off) → rebrand but keep
  disabled; its unpinned `unpkg.com/blank-settings-utils@latest` import must be vendored if
  ever enabled.

## 1. Rebrand map (complete)

The bridge message protocol is already brand-neutral (types like `chroma-selection`,
`skin-state`) — types are kept verbatim. Only branded surfaces change, and we control both
ends (Rust server + our plugins), so renames land in lockstep.

| Rose | Chud |
|---|---|
| `%LOCALAPPDATA%\Rose\…` (mods, skins, state, resources, logs, Pengu Loader) | `%LOCALAPPDATA%\Chud\…` |
| Plugin folders `ROSE-<Name>` | `CHUD-<Name>` |
| `window.__roseBridge` | `window.__chudBridge` |
| localStorage `rose_bridge_port` | `chud_bridge_port` |
| CustomEvents `rose-custom-wheel-*`, `rose-open-settings`, `lu-skin-monitor-state` | `chud-custom-wheel-*`, `chud-open-settings`, `chud-skin-monitor-state` |
| CSS/DOM `rose-*`, `lu-*`, `--rose-*` | `chud-*`, `--chud-*` |
| Party token prefix `"ROSE:"` | `"CHUD:"` (v2 binary layout unchanged) |
| Relay worker `rose-party-relay` | `chud-party-relay` |
| Env `ROSE_RELAY_URL` | `CHUD_RELAY_URL` |
| Logs `rose_*.log`, `rose_diagnostics.txt` | `chud_skins_*.log`, `chud_diagnostics.txt` |
| Assets `golden_rose.png`, `rose_without_bg.png`, tray flower icons | Chud icon set (interim: existing Chud icons; final art = Josh) |
| `Rose/{ver}` User-Agent | `Chud/{ver}` |
| Discord/Ko-fi/`Alban1911/Rose` URLs in plugins/README | removed; repo link → this repo |
| Mutex/installer/schtasks `Rose*` | n/a — Chud's Tauri single-instance + bundler own these |
| Analytics `api.leagueunlocked.net` | removed |

MIT attribution to the original authors (Alban & Florent, Rose contributors, CSLOL,
Pengu Loader) is PRESERVED in README/credits — license requires notice retention; the
*brand* goes, the *credit* stays.

Riot names (`LeagueClient.exe`, `League of Legends.exe`, lockfile format) are NOT branding —
never rename.

## 2. Rust architecture (`native/src-tauri/src/skins/`)

```
skins/
  mod.rs           SkinsState (Arc), subsystem start/stop, Tauri commands surface
  paths.rs         %LOCALAPPDATA%\Chud tree + elevation-aware desktop-user resolution (FFI)
  slog.rs          skins file logger: bounded-channel non-blocking writer, size rotation
  config.rs        [skins] settings extension of Chud Config (threshold, league path, flags)
  state.rs         SkinsShared (ONE Mutex; ~60 fields from Rose SharedState) + reset fns
  lcu_ext.rs       skin scraper + cache, skin selection PATCH, game mode, swiftplay, cell/lock
                   pure fns (map_cells/compute_locked), LCU JSON serde types (all Option'd)
  phase.rs         THE phase engine: single tokio actor owns phase; LCU WS events + poll
                   fallback feed it via mpsc; consolidates Rose's PhaseThread/WSEventThread/
                   LCUMonitorThread dual-writer races into one writer. Emits PhaseEvent to
                   subscribers (broadcast channel).
  ticker.rs        loadout deadline ticker (generation-counter, FINALIZATION-only start,
                   probe loop, monotonic deadline, anti-jitter clamp)
  trigger.rs       injection decision engine (Rose injection_trigger.py): historic > random >
                   hovered priority, owned-skin force, unowned zip extract, category mods,
                   party mods, base-skin force + verify + tracker telemetry
  swiftplay.rs     swiftplay/brawl pipeline (tracking map, early extraction, overlay on
                   GameStart, exit cleanup preserving same-queue requeue)
  features/
    chroma.rs      selector/panel state, selection handler, base-skin math (id/1000, %1000)
    special.rs     ONE static forms/HOL table (Lux/Morde/Morg/Sett/Sera/Viego/Kai'Sa/Ahri)
                   — single source replacing Rose's 3 duplicated copies
    historic.rs    historic.json + mod_historic.json (HistoricEntry enum, "path:" untagged
                   serde format preserved)
    random.rs      dice/random skin selection
  injection/
    mod.rs         InjectionManager (cooldown, lock, champion-lock tracking)
    injector.rs    per-injection orchestration (resolve → clean → extract → overlay)
    overlay.rs     mod-tools mkoverlay/runoverlay — EXACT CLI: mkoverlay <mods> <overlay>
                   --game:<g> --mods:<a>/<b> --noTFT --ignoreConflict ; runoverlay <overlay>
                   <overlay>/cslol-config.json --game:<g> --opts:configless ;
                   CREATE_NO_WINDOW, stdout/stderr drain tasks, priority boost
    game_monitor.rs NtSuspendProcess/NtResumeProcess FFI + UNCONDITIONAL auto-resume timeout
    process.rs     terminate→wait→kill helper, runoverlay/mod-tools sweeps
    storage.rs     mods category tree; NO destructive unknown-folder wipe (Rose data-loss
                   trap softened: unknown dirs are logged + left alone)
    zips.rs        zip_resolver + safe_extract (Path::starts_with component-aware) +
                   junction-or-extract cache (FSCTL via `junction` crate)
    tools.rs       tool presence checks, dll hash gate
  bridge/
    mod.rs         axum server on 127.0.0.1:{50000..50010 free}, port → state/bridge_port.txt
    http.rs        /bridge-port, /port, /preview/…, /asset/…, /plugin/… (traversal-hardened,
                   loopback-Origin check)
    ws.rs          WS upgrade, broadcast-only fanout (NEVER targeted replies — contract),
                   ping_interval=20/ping_timeout=20 preserved
    protocol.rs    serde two-stage decode (typed by "type", fallback bare {"skin"}),
                   i64 ms-epoch timestamps
    handlers.rs    all 35 inbound handlers (Rose message_handler.py)
    broadcast.rs   the 10 outbound state broadcasts
  party/
    token.rs       CHUD: prefix, v2 >BIQ + 32B key, zlib(urlsafe-b64-no-pad), 3600s expiry,
                   room key = sha256(summoner_id_str + key)[:32] hex
    relay.rs       WS client to worker; join/skin/leave JSON; TEXT-frame "ping"/"pong"
                   keepalive @25s (NOT WS ping frames)
    manager.rs     enable/add_peer(join peer's room)/broadcast loops, anti-spoof champion
                   cross-check vs lobby, custom-mod share by sha256[..16] content hash
  downloads/
    repo.rs        LeagueSkins full-zip + GitHub-compare incremental; extract skins/ +
                   resources/ only, strip archive root, guarded cleanup pass
    hashes.rs      CommunityDragon hashes.game.txt.{0..8} shard merge, commit-SHA state file
  pengu.rs         Pengu Loader lifecycle: copy-to-appdata preserving index.js/index.js_
                   enable-state snapshot/restore, --set-league-path/--force-activate/
                   --force-deactivate --silent, dirty-flag crash recovery, IFEO cleanup
```

Integration points in existing code:
- `lib.rs`: `mod skins;` + `skins::commands()` registered; `AppState` gains
  `skins: skins::SkinsState`. Skins tool card appears in `snapshot()` tools array.
- `lcu.rs` stays the auth/base client; `lcu_ext` builds on it.
- The existing LCU WS task (`lcu_ws.rs`) gains fan-out of gameflow/champ-select events into
  `phase.rs`'s channel (one WS connection total, not two).
- Ranked kill-switch: injection honors the existing `injection_blocked` atomic.

### Threading model (the big fix)
Rose: 5+ OS threads mutating a 60-field GIL-guarded god object; documented races.
Chud: tokio tasks; ALL shared skin state in one `Mutex<SkinsShared>` (coarse first, split
later only if contention shows). Phase has exactly ONE writer (phase actor). Cross-thread
"poke booleans" become channels/watch. Generation counters stay (`AtomicU64`) for ticker and
stale-loop invalidation — same pattern lib.rs already uses.

### Magic values preserved verbatim
base skin = champion_id*1000; chroma window = base+1..+99; queue 480 + {SWIFTPLAY,BRAWL};
null-phase debounce 3 polls; LCU disconnect debounce 3; language retries 5; ticker 250 Hz
clamp 10–2000; probe 8×60 ms; GET cache TTL 0.2 s; 404/405→None contract; locale chroma
regex from skin_scraper.py ported literally; lenient semver only where still needed.

## 3. Wire/disk contracts that must not drift
1. mod-tools CLI strings (above) — character-for-character.
2. Bridge protocol: broadcast-only replies; bare `{"skin":…}` legacy message; all three port
   discovery paths (file + /bridge-port + /port); ms timestamps.
3. `historic.json` / `mod_historic.json` formats ("path:" prefix union) — kept (cheap), even
   though Chud has no Rose users to migrate.
4. Party token binary layout v2 (only the ASCII prefix changes) + relay JSON messages.
5. Select-time (not inject-time) mod extraction — the JS UI assumes it.

## 4. New crates
`axum` (bridge http+ws), `zip`, `flate2`, `sha2`, `rand`, `regex`, `junction`,
`byteorder`(or to_be_bytes), `indexmap` (skin-name map ordering), `tracing`+`tracing-appender`
(or hand-rolled slog.rs), `windows` feature adds: Win32_System_Threading, Win32_Storage_FileSystem,
Win32_System_Registry, Win32_Security. Relay worker: separate crate `relay-worker-rs` with
`worker` (workers-rs).

## 5. Milestones
Status: ALL MILESTONES COMPLETE. S1–S9 ✅ + reconciliation pass ✅ + S10 gate ✅.
Relay worker deployed + protocol-tested (`https://chud-party-relay.jivy26.workers.dev`;
old `rose-party-relay` deleted). 112 unit tests pass; `cargo build --release` clean;
whole-repo `rose` sweep clean (only docs/CREDITS attribution remain, by design).

END-TO-END PROVEN (2026-07-10, live Practice Tool run): full injection pipeline verified
against a real League client — trigger fired, resolved skin_266033 (Primordian Aatrox),
NtSuspendProcess suspended the live game (pid 26548), mkoverlay built the overlay in 1.14s,
runoverlay started, NtResumeProcess resumed cleanly. Skin visible in-game. The game-suspend
FFI (the highest-risk piece) works on a real process. Deferred-verification debt CLEARED.

Bug found + fixed during that run: injection was gated on a gameflow phase "FINALIZATION"
that never fires (FINALIZATION is a champ-select session-timer subphase, not a gameflow
phase), so the loadout ticker never armed. Fixed by triggering injection at the GameStart
phase (with an InProgress fallback + last_hover_written dedup guard) — the game monitor
suspends League on launch so the overlay builds before assets load. Works in Practice Tool
(no loadout countdown) and normal games alike. See `ticker::inject_for_game` + phase.rs.

Remaining functional gaps (non-blocking): custom-mod historic (path: enum), late-lock
bootstrap (both documented under Open reconciliation items). Optional future polish: also
arm the loadout ticker on the champ-select session-timer FINALIZATION for earlier injection
in normal games (currently GameStart-time injection covers all modes via the game-suspend).

Open reconciliation items (from S2/S3 agent notes, address before S10):
- phase.rs: late-lock bootstrap on mid-ChampSelect start NOT ported; Swiftplay skips
  champ-select reset not honored (game-mode detect runs after reset); no distinct
  champion-exchange event (emits ChampionLocked). InjectionManager: `update_skin`
  secondary entry not ported. ~~GameMonitor auto-resume timeout defaults 60s — wire
  config `monitor_auto_resume_timeout` at InjectionManager construction.~~ DONE
  (reconciliation pass): `config::SkinsCfg::monitor_auto_resume_timeout_secs`
  (default 60.0) is now applied via `InjectionManager::set_auto_resume_timeout`
  in `lib.rs`'s `setup()`, right after construction.
- `historic_skin_id: Option<i64>` can't represent the Python "path:<rel>" custom-mod
  historic value — historic-mode auto-reapply of a CUSTOM mod is not ported (only
  skin/chroma-ID historic works). Needs `historic_skin_id` → an enum in a future pass.
- Late-lock bootstrap: if the app starts mid-ChampSelect after the user already
  locked, the phase engine doesn't proactively fetch the session to detect the
  existing lock (only WS-fed session deltas trigger it). Port
  `lcu_monitor_thread.py::_bootstrap_late_locked_champion` in a future pass.

- **S1 foundation**: paths/slog/config/state/special + Cargo deps + `skins::mod` skeleton — compiles.
- **S2 LCU + phase**: lcu_ext, phase actor, ws fan-out — compiles + unit tests for map_cells/compute_locked.
- **S3 injection**: injection/* + pengu.rs lifecycle.
- **S4 bridge**: bridge/* full protocol + features/* (chroma/historic/random).
- **S5 game flow**: ticker/trigger/swiftplay.
- **S6 party**: party/* + relay worker crate.
- **S7 downloads**: repo/hashes.
- **S8 plugins**: copy + rebrand `CHUD-*` plugins, bundle Pengu Loader + tools, resources wiring.
- **S9 UI**: Skins pages in native/ui (wheel config, party, diagnostics, settings).
- **S10 gate**: zero-`rose`-grep sweep (case-insensitive, excluding upstream credits), cargo
  release build, end-to-end smoke (bridge serves, plugins connect, dry-run injection path).

## 6. Deferred / needs-Josh
- Relay deploy: `wrangler deploy` needs Josh's Cloudflare login → until then
  `CHUD_RELAY_URL` env/config; party mode is gated on it.
- Final art: Chud-branded replacements for golden_rose/flag/dice/tray imagery (interim:
  Chud icon + neutral SVGs).
- cslol-dll.dll stays untracked; Josh's local copy is referenced from the Rose checkout.
