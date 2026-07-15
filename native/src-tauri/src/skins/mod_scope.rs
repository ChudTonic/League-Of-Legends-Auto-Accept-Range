//! Background auto-fix for user-imported custom mods — the in-app port of
//! `tools/scope-champion-mod.py` (champion skins) plus a sweep-time hook for
//! `announcer_fix` (announcer packs dropped into the folder by hand rather
//! than downloaded through the Library).
//!
//! WHY: cslol `mkoverlay` merges every mod entry into EVERY game WAD whose
//! table-of-contents contains that entry's path-hash. Community fantomes
//! routinely include copies of widely-shared assets (a particle texture in
//! 227 game WADs, `item_metadata.rec` in 173...), turning a single-champion
//! skin into a 20+ GB, 2+ minute full-game rebuild at loadout time — the
//! League-session-wedging incident this fixes. Measured on one real mod:
//! raw 205 WADs / 22 GB / 164 s -> scoped 21 WADs / 4 GB / 4 s.
//!
//! Per-entry rule (path-hash = xxh64 of the lowercased path):
//!   KEEP  brand-new hashes, and entries in <= `MAX_ENTRY_WADS` game WADs —
//!         cheap and almost certainly intentional (own WAD, locale VO
//!         sibling, event twins, or ANOTHER champion in a multi-champion
//!         pack — learned the hard way: an "Ahri" pack legitimately carried
//!         Akali files in exactly 1 WAD; an ownership/family test wrongly strips that)
//!   DROP  only entries present in many WADs — truly shared assets the game
//!         already has everywhere. A skin never intends a global override.
//!
//! Sweeps run on a blocking thread at app startup and on every ChampSelect
//! entry, tracked in `state/mod_scope_state.json` so an already-processed
//! mod costs one metadata stat. Originals are kept as `<name>.bak`.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use crate::skins::injection::tools::cslol_tools_dir;
use crate::skins::injection::zips::safe_extractall;
use crate::skins::slog::{log_error, log_info, log_warn};
use crate::skins::{announcer_fix, lcu_ext, paths};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// An entry in <= this many game WADs is specifically-targetable content
/// (own WAD, locale sibling, event twins, other champs in a multi-champ
/// pack) — presumed intentional. Beyond it, shared junk (34-227 WADs observed).
const MAX_ENTRY_WADS: u32 = 5;
/// Bump to force every mod through the scoper again after a rule change.
/// v2: count-only rule — v1's WAD-family membership requirement wrongly
/// stripped cross-champion content from multi-champion packs.
const SCOPE_RULE_VERSION: u32 = 2;

/// One sweep at a time — startup and ChampSelect entry can race.
static SWEEP_RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Processed-state tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScopeState {
    #[serde(default)]
    version: u32,
    /// mod path -> "mtime_secs:size" stamp at the time it was processed.
    #[serde(default)]
    processed: HashMap<String, String>,
}

fn state_path() -> PathBuf {
    paths::state_dir().join("mod_scope_state.json")
}

fn load_state() -> ScopeState {
    let state: ScopeState = std::fs::read_to_string(state_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if state.version != SCOPE_RULE_VERSION {
        return ScopeState { version: SCOPE_RULE_VERSION, processed: HashMap::new() };
    }
    state
}

fn save_state(state: &ScopeState) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::create_dir_all(paths::state_dir());
        let _ = std::fs::write(state_path(), text);
    }
}

fn stamp(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{mtime}:{}", meta.len()))
}

// ---------------------------------------------------------------------------
// Game WAD index
// ---------------------------------------------------------------------------

struct GameIndex {
    /// path-hash -> number of game WADs containing it.
    counts: HashMap<u64, u32>,
}

/// Parse a WAD v3 table of contents into its entry path-hash set.
fn wad_toc(path: &Path) -> Option<HashSet<u64>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic).ok()?;
    if &magic != b"RW" {
        return None;
    }
    f.seek(SeekFrom::Start(268)).ok()?;
    let mut count_buf = [0u8; 4];
    f.read_exact(&mut count_buf).ok()?;
    let count = u32::from_le_bytes(count_buf) as usize;
    if count > 1_000_000 {
        return None; // implausible — refuse rather than allocate wild
    }
    let mut table = vec![0u8; count * 32];
    f.read_exact(&mut table).ok()?;
    Some((0..count).map(|i| u64::from_le_bytes(table[i * 32..i * 32 + 8].try_into().unwrap())).collect())
}

fn collect_wads(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_wads(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_lowercase().ends_with(".wad.client")) {
            out.push(path);
        }
    }
}

fn build_game_index(game_dir: &Path) -> Option<GameIndex> {
    let root = game_dir.join("DATA").join("FINAL");
    let mut wads = Vec::new();
    collect_wads(&root, &mut wads);
    if wads.is_empty() {
        log_warn!("[MOD_SCOPE] No game WADs under {} - skipping sweep", root.display());
        return None;
    }
    let mut counts: HashMap<u64, u32> = HashMap::new();
    let mut indexed = 0u32;
    for w in &wads {
        let Some(hashes) = wad_toc(w) else { continue };
        for &h in &hashes {
            *counts.entry(h).or_insert(0) += 1;
        }
        indexed += 1;
    }
    log_info!("[MOD_SCOPE] Indexed {indexed} game WADs ({} distinct entries)", counts.len());
    Some(GameIndex { counts })
}

impl GameIndex {
    fn count(&self, h: u64) -> u32 {
        self.counts.get(&h).copied().unwrap_or(0)
    }
}

/// Path-hash for a mod entry: hex-named files carry their hash literally;
/// everything else hashes its lowercased relative path.
fn path_hash(rel: &str) -> u64 {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    if stem.len() == 16 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(h) = u64::from_str_radix(stem, 16) {
            return h;
        }
    }
    xxhash_rust::xxh64::xxh64(rel.replace('\\', "/").to_lowercase().as_bytes(), 0)
}

// ---------------------------------------------------------------------------
// Per-mod scoping
// ---------------------------------------------------------------------------

fn run_tool(exe: &Path, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().map_err(|e| format!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!("{} exited {:?}: {}", exe.display(), out.status.code(), String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// Walk `dir`, deleting entries that fail the scoping rule.
/// Returns (kept, dropped-descriptions).
fn scope_dir(dir: &Path, index: &GameIndex) -> (u32, Vec<String>) {
    fn walk(dir: &Path, root: &Path, index: &GameIndex, kept: &mut u32, dropped: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, index, kept, dropped);
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            if rel == "hashed_files.json" {
                continue;
            }
            let n = index.count(path_hash(&rel));
            if n <= MAX_ENTRY_WADS {
                *kept += 1;
            } else {
                dropped.push(format!("{rel} ({n} wads)"));
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let mut kept = 0;
    let mut dropped = Vec::new();
    walk(dir, dir, index, &mut kept, &mut dropped);
    (kept, dropped)
}

/// Scope one champion-skin fantome in place. Returns the number of dropped
/// shared entries (0 = mod was already clean and is left byte-identical).
fn scope_fantome(path: &Path, index: &GameIndex) -> Result<usize, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let tmp = std::env::temp_dir().join(format!("chud_scope_{}", std::process::id())).join(
        path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mod".into()),
    );
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let cleanup = TempCleanup(tmp.clone());

    let staging = tmp.join("x");
    // Write to a temp file first — safe_extractall takes a path.
    let zip_copy = tmp.join("src.zip");
    std::fs::write(&zip_copy, &bytes).map_err(|e| e.to_string())?;
    safe_extractall(&zip_copy, &staging).map_err(|e| e.to_string())?;

    let wad_root = staging.join("WAD");
    if !wad_root.is_dir() {
        return Ok(0); // nothing recognizable — leave alone
    }

    let tools = cslol_tools_dir();
    let hashes_file = tools.join("hashes.game.txt");
    let mut total_dropped: Vec<String> = Vec::new();
    let mut packed_members: Vec<(String, PathBuf)> = Vec::new();

    let members: Vec<PathBuf> = std::fs::read_dir(&wad_root).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    for member_path in members {
        let member = member_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if !member.to_lowercase().ends_with(".wad.client") {
            continue;
        }

        let filter_dir = if member_path.is_file() {
            // Packed member: unpack to filter its entries.
            let extracted = tmp.join("unpacked").join(&member);
            let mut args: Vec<&std::ffi::OsStr> = vec![member_path.as_os_str(), extracted.as_os_str()];
            if hashes_file.exists() {
                args.push(hashes_file.as_os_str());
            }
            run_tool(&tools.join("wad-extract.exe"), &args)?;
            extracted
        } else {
            member_path.clone()
        };

        let (kept, dropped) = scope_dir(&filter_dir, index);
        log_info!("[MOD_SCOPE] {member}: kept {kept}, dropped {}", dropped.len());
        let member_dropped = dropped.len();
        total_dropped.extend(dropped);

        // Repack from the filtered dir when anything was dropped or the
        // member was RAW — a packed member with zero drops keeps its file.
        if member_path.is_file() && member_dropped == 0 {
            packed_members.push((member, member_path));
        } else {
            let packed = tmp.join("packed").join(&member);
            std::fs::create_dir_all(packed.parent().unwrap()).map_err(|e| e.to_string())?;
            run_tool(&tools.join("wad-make.exe"), &[filter_dir.as_os_str(), packed.as_os_str()])?;
            packed_members.push((member, packed));
        }
    }

    if total_dropped.is_empty() {
        drop(cleanup);
        return Ok(0);
    }

    // Rewrap: everything outside WAD/ preserved, WAD members repacked.
    let bak = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|e| e.to_str()).unwrap_or("fantome")
    ));
    if !bak.exists() {
        std::fs::copy(path, &bak).map_err(|e| e.to_string())?;
    }

    use std::io::Write as _;
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).map_err(|e| e.to_string())?;
    let names: Vec<String> = archive.file_names().map(str::to_string).collect();
    for name in names {
        if name.starts_with("WAD/") || name.ends_with('/') {
            continue;
        }
        let mut data = Vec::new();
        use std::io::Read as _;
        archive.by_name(&name).map_err(|e| e.to_string())?.read_to_end(&mut data).map_err(|e| e.to_string())?;
        zw.start_file(name, opts).map_err(|e| e.to_string())?;
        zw.write_all(&data).map_err(|e| e.to_string())?;
    }
    for (member, packed_path) in packed_members {
        let data = std::fs::read(&packed_path).map_err(|e| e.to_string())?;
        zw.start_file(format!("WAD/{member}"), opts).map_err(|e| e.to_string())?;
        zw.write_all(&data).map_err(|e| e.to_string())?;
    }
    let rewritten = zw.finish().map_err(|e| e.to_string())?.into_inner();

    let tmp_out = path.with_extension("chud_new");
    std::fs::write(&tmp_out, &rewritten).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp_out, path).map_err(|e| e.to_string())?;

    for d in total_dropped.iter().take(6) {
        log_info!("[MOD_SCOPE]   dropped: {d}");
    }
    drop(cleanup);
    Ok(total_dropped.len())
}

struct TempCleanup(PathBuf);
impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

fn is_mod_archive(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).as_deref(),
        Some("fantome") | Some("zip")
    )
}

fn collect_mod_archives(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_mod_archives(&path, out);
        } else if is_mod_archive(&path) {
            out.push(path);
        }
    }
}

/// Sweep user-imported mods: scope champion skins, retarget hand-dropped
/// announcer packs. Cheap when nothing changed (one metadata stat per mod).
/// Runs at app startup and on every ChampSelect entry; call from a blocking
/// thread. `app` (when present) gets a toast about what was fixed.
pub fn sweep_imported_mods(app: Option<&AppHandle>) {
    if SWEEP_RUNNING.swap(true, Ordering::SeqCst) {
        return; // a sweep is already in flight
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep_inner(app)));
    SWEEP_RUNNING.store(false, Ordering::SeqCst);
    if result.is_err() {
        log_error!("[MOD_SCOPE] Sweep panicked - state untouched, will retry next sweep");
    }
}

fn sweep_inner(app: Option<&AppHandle>) {
    let mods_root = paths::mods_dir();
    let mut skins = Vec::new();
    collect_mod_archives(&mods_root.join("skins"), &mut skins);
    let mut announcers = Vec::new();
    collect_mod_archives(&mods_root.join("announcers"), &mut announcers);

    let mut state = load_state();
    let pending: Vec<(PathBuf, bool)> = skins
        .into_iter()
        .map(|p| (p, false))
        .chain(announcers.into_iter().map(|p| (p, true)))
        .filter(|(p, _)| {
            let key = p.to_string_lossy().into_owned();
            match (stamp(p), state.processed.get(&key)) {
                (Some(s), Some(prev)) => s != *prev,
                (Some(_), None) => true,
                (None, _) => false,
            }
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    log_info!("[MOD_SCOPE] {} new/changed imported mod(s) to check", pending.len());

    // Champion skins need the game index; announcers don't.
    let needs_index = pending.iter().any(|(_, is_announcer)| !is_announcer);
    let index = if needs_index {
        match lcu_ext::resolve_game_dir() {
            Some(dir) => build_game_index(&dir),
            None => {
                // Announcers below still process; skins stay pending for the
                // next sweep (ChampSelect entry guarantees the client is up).
                log_info!("[MOD_SCOPE] League game dir unavailable (client not running?) - skin scoping deferred");
                None
            }
        }
    } else {
        None
    };

    let mut fixed: Vec<String> = Vec::new();
    for (path, is_announcer) in &pending {
        let key = path.to_string_lossy().into_owned();
        let display = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| key.clone());

        if *is_announcer {
            match std::fs::read(path) {
                Ok(bytes) => {
                    if let Some(rewritten) = announcer_fix::retarget_announcer_pack(&bytes) {
                        let bak = path.with_extension(format!(
                            "{}.bak",
                            path.extension().and_then(|e| e.to_str()).unwrap_or("fantome")
                        ));
                        if !bak.exists() {
                            let _ = std::fs::copy(path, &bak);
                        }
                        if std::fs::write(path, &rewritten).is_ok() {
                            log_info!("[MOD_SCOPE] Retargeted announcer pack for all modes: {display}");
                            fixed.push(format!("{display} (announcer, all modes)"));
                        }
                    }
                }
                Err(e) => log_warn!("[MOD_SCOPE] Could not read {display}: {e}"),
            }
            if let Some(s) = stamp(path) {
                state.processed.insert(key, s);
            }
            continue;
        }

        let Some(index) = index.as_ref() else { continue }; // skins deferred without index
        match scope_fantome(path, index) {
            Ok(0) => {}
            Ok(dropped) => {
                log_info!("[MOD_SCOPE] Scoped '{display}': dropped {dropped} shared entries");
                fixed.push(format!("{display} ({dropped} shared entries removed)"));
            }
            Err(e) => log_error!("[MOD_SCOPE] Failed to scope '{display}' (left untouched): {e}"),
        }
        if let Some(s) = stamp(path) {
            state.processed.insert(key, s);
        }
    }

    save_state(&state);

    if !fixed.is_empty() {
        if let Some(app) = app {
            let _ = app.emit(
                "notification",
                serde_json::json!({
                    "title": "Custom mods optimized",
                    "message": format!("Fixed for fast injection: {}", fixed.join(", ")),
                    "tone": "success",
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_hash_uses_hex_stem_or_xxh64_of_lowercased_path() {
        assert_eq!(path_hash("12d723dc0e745f8a.bin"), 0x12d723dc0e745f8a);
        assert_eq!(path_hash("sub/dir/AbCdEf0123456789.dds"), 0xabcdef0123456789);
        // Known pair from hashes.game.txt: the en_us global announcer wpk.
        assert_eq!(
            path_hash("assets/sounds/wwise2016/vo/en_us/shared/announcer_global_female1_vo_audio.wpk"),
            0xccdbafd023095999
        );
        // Case-insensitive + separator-normalizing.
        assert_eq!(
            path_hash("Assets\\Sounds\\wwise2016\\vo\\en_us\\shared\\announcer_global_female1_vo_audio.wpk"),
            0xccdbafd023095999
        );
    }


    /// Full-pipeline proof against a real mod + real League install. Skips
    /// silently when either is absent (CI); run manually with
    /// `cargo test --lib mod_scope -- --ignored`.
    #[test]
    #[ignore = "requires a local League install and a .bak'd real mod"]
    fn scope_real_fantome_end_to_end() {
        let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let bak = Path::new(&local).join("Chud/mods/skins/4000/Rouxls Kaard Twisted Fate.fantome.bak");
        let game = Path::new(r"C:\Riot Games\League of Legends\Game");
        if !bak.exists() || !game.is_dir() {
            return;
        }
        let Some(index) = build_game_index(game) else { return };

        let work = std::env::temp_dir().join("chud_mod_scope_e2e");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let target = work.join("Rouxls.fantome");
        std::fs::copy(&bak, &target).unwrap();

        let dropped = scope_fantome(&target, &index).expect("scoping must succeed");
        assert!(dropped >= 20, "expected the known ~24 shared entries, got {dropped}");

        // Result must be a valid zip whose WAD member is now a packed file.
        let bytes = std::fs::read(&target).unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.as_slice())).unwrap();
        let names: Vec<String> = z.file_names().map(str::to_string).collect();
        assert!(names.iter().any(|n| n == "WAD/TwistedFate.wad.client"), "packed member missing: {names:?}");
        assert!(names.iter().any(|n| n.starts_with("META/")), "META lost");
        use std::io::Read as _;
        let mut wad = Vec::new();
        z.by_name("WAD/TwistedFate.wad.client").unwrap().read_to_end(&mut wad).unwrap();
        assert_eq!(&wad[..2], b"RW", "member must be a packed WAD");

        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn scope_dir_keeps_new_own_and_cross_champion_entries_drops_shared() {
        let root = std::env::temp_dir().join("chud_mod_scope_test_dir");
        let _ = std::fs::remove_dir_all(&root);
        for rel in [
            "assets/characters/jhin/skins/base/jhin.skn",   // own champion (2 wads)
            "assets/characters/akali/skins/base/akali.skn", // cross-champ in a multi-champ pack (1 wad)
            "assets/shared/particles/defaultfalloff.tex",   // shared junk (226 wads)
            "newfile.custom",                               // brand-new (0 wads)
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        let mut counts = HashMap::new();
        counts.insert(path_hash("assets/characters/jhin/skins/base/jhin.skn"), 2u32);
        counts.insert(path_hash("assets/characters/akali/skins/base/akali.skn"), 1u32);
        counts.insert(path_hash("assets/shared/particles/defaultfalloff.tex"), 226u32);
        let idx = GameIndex { counts };

        let (kept, dropped) = scope_dir(&root, &idx);
        assert_eq!(kept, 3, "own + cross-champion + brand-new entries all survive");
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].contains("defaultfalloff.tex"));
        assert!(root.join("assets/characters/akali/skins/base/akali.skn").exists(), "v1 regression: multi-champ pack content must survive");
        assert!(!root.join("assets/shared/particles/defaultfalloff.tex").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
