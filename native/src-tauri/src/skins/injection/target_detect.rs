//! Target-skin detection for custom skin mods.
//!
//! Library mods are all filed under the champion's BASE skin folder
//! (`skins/<champId*1000>/`), so at injection time the app doesn't know which
//! skin slot the mod's WAD chunks are actually keyed to. Forcing base for a
//! mod built over a real skin (e.g. a Soul Fighter Viego VFX edit) makes the
//! game load base-skin chunks the mod never overrides — the user had to race
//! the champ-select timer to flip their selection back by hand.
//!
//! Detection order:
//! 1. WAD chunk scan — a skin mod overrides `data/characters/<alias>/skins/
//!    skin<NN>.bin`; those path hashes are recoverable by forward-hashing the
//!    candidate paths (xxh64, lowercase) for NN in 0..=999 and intersecting
//!    with the mod's chunk table. No hashtable file needed.
//! 2. Mod-name match against the champion's skin catalog ("SOUL FIGHTER VIEGO
//!    VFX CHROMA" contains "Soul Fighter Viego").
//!
//! Returns ALL matched slots as ABSOLUTE skin ids (championId*1000 + NN) so
//! the caller can pick by ownership — a chroma-VFX mod can cover six chroma
//! slots of which the user owns one. `None` means unknown — the caller can
//! then fall back to the user's live champ-select selection.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use xxhash_rust::xxh64::xxh64;

use crate::lcu::Auth;
use crate::skins::champ_alias;
use crate::skins::favorites;
use crate::skins::lcu_ext::{self, ChampionData, SkinData};
use crate::skins::slog::{log_info, log_warn};

/// Highest skin/chroma number probed per champion (chroma ids run well past
/// the skin count; 999 covers the full NN range of an absolute skin id).
const MAX_SKIN_NUM: i64 = 999;
/// Sanity cap on a WAD's chunk count — real mods are a few hundred chunks.
const MAX_WAD_ENTRIES: u64 = 200_000;

/// Slots a custom mod's assets are keyed to (absolute skin ids, ascending).
/// `via_name` marks a weaker name-only match — the mod's actual chunk layout
/// is unknown, only its title referenced the skin.
#[derive(Debug, Clone)]
pub struct Detection {
    pub slots: Vec<i64>,
    pub via_name: bool,
}

pub async fn detect_target_skin(
    mod_path: &Path,
    champion_id: i64,
    client: &reqwest::Client,
    auth: &Auth,
) -> Option<Detection> {
    let champ = fetch_champion_data(client, auth, champion_id).await;

    let hashes = collect_chunk_hashes(mod_path);
    if let Some(champ) = &champ {
        if !hashes.is_empty() {
            let slots = match_skin_bins(&hashes, champ, champion_id);
            if !slots.is_empty() {
                return Some(Detection { slots, via_name: false });
            }
        }
    }

    let by_name = champ.as_ref().and_then(|c| match_mod_name(mod_path, c, champion_id));
    if let Some(id) = by_name {
        log_info!("[TARGET] name-matched custom mod '{}' -> skin {id}", mod_path.file_name().unwrap_or_default().to_string_lossy());
        return Some(Detection { slots: vec![id], via_name: true });
    }
    log_info!("[TARGET] no target skin detected for '{}' ({} chunk hashes)", mod_path.file_name().unwrap_or_default().to_string_lossy(), hashes.len());
    None
}

/// Download-time (and startup-migration) variant for the Library installer —
/// fully offline now. Champion data prefers the live LCU (authoritative) when
/// the client happens to be running, but never waits for it: with League
/// closed it falls back to the bundled `champ_alias.json` static table for the
/// WAD alias, plus the local downloaded skin catalog for name-match. `None`
/// only when the champion is unresolvable both ways (e.g. a brand-new
/// champion with League closed and the static table not yet updated). Only
/// returns a slot from the chunk scan when it finds exactly one non-base
/// match — an ambiguous multi-slot hit (e.g. a chroma-VFX mod covering several
/// chromas) is left for the user to resolve via `library_set_target_skin`.
pub async fn detect_target_skin_offline(mod_path: &Path, champion_id: i64) -> Option<i64> {
    let champ = match crate::lcu::cached_auth() {
        Some(auth) => {
            let client = crate::lcu::build_lcu_client(6.0);
            fetch_champion_data(&client, &auth, champion_id).await
        }
        None => None,
    };
    let champ = champ.or_else(|| offline_champion_data(champion_id))?;

    let hashes = collect_chunk_hashes(mod_path);
    let by_hash = if hashes.is_empty() { None } else { pick_single_slot(&match_skin_bins(&hashes, &champ, champion_id)) };
    by_hash.or_else(|| match_mod_name(mod_path, &champ, champion_id))
}

/// Best-effort offline `ChampionData` synthesis when the LCU isn't reachable:
/// the alias (required for the WAD chunk-hash scan) comes from the bundled
/// static table; the skin name list (only used by `match_mod_name`'s
/// fallback) comes from the locally downloaded skin catalog, if any. `None`
/// if even the static table doesn't know the champion.
fn offline_champion_data(champion_id: i64) -> Option<ChampionData> {
    let alias = champ_alias::champ_alias(champion_id)?;
    let skins = favorites::catalog(None).into_iter().find(|c| c.champ_id == champion_id).map(|c| {
        c.skins
            .into_iter()
            .map(|s| SkinData { id: Some(s.skin_id), name: Some(s.name), ..Default::default() })
            .collect::<Vec<_>>()
    });
    Some(ChampionData { id: Some(champion_id), name: None, alias: Some(alias), skins })
}

async fn fetch_champion_data(client: &reqwest::Client, auth: &Auth, champion_id: i64) -> Option<ChampionData> {
    let path = format!("/lol-game-data/assets/v1/champions/{champion_id}.json");
    let value = lcu_ext::shared_cache().get(client, auth, &path, lcu_ext::DEFAULT_CACHE_TTL).await?;
    serde_json::from_value::<ChampionData>(value).ok()
}

/// Intersect the mod's chunk hashes with the champion's possible skin-bin
/// paths, returning every matched slot as an absolute skin id (ascending).
fn match_skin_bins(hashes: &HashSet<u64>, champ: &ChampionData, champion_id: i64) -> Vec<i64> {
    let Some(alias) = champ.alias.clone().or_else(|| champ.name.clone()) else {
        return Vec::new();
    };
    let alias = alias.to_lowercase();
    let mut found: Vec<i64> = Vec::new();
    for nn in 0..=MAX_SKIN_NUM {
        let path = format!("data/characters/{alias}/skins/skin{nn}.bin");
        if hashes.contains(&xxh64(path.as_bytes(), 0)) {
            found.push(champion_id * 1000 + nn);
        }
    }
    if !found.is_empty() {
        log_info!("[TARGET] WAD chunk scan matched skin slots {found:?} (champion {alias})");
    }
    found
}

/// Danger scan: does this mod override the champion's ROOT character record
/// (`data/characters/<alias>/<alias>.bin`)? That file defines the champion's
/// spells/abilities/stats — a purely cosmetic skin never ships it. A mod that
/// does (typically an old, full-champion port) replaces the live ability data
/// with its bundled copy; on a newer game patch the abilities break in-game
/// (missing/unusable, can't level up) and only a client repair restores them.
/// Returns the offending game path when found, so the caller can tell the user
/// exactly why the skin was blocked. Offline-safe: the alias comes from the
/// bundled static table, so it works with League closed (import-time scans).
pub fn overrides_ability_data(mod_path: &Path, champion_id: i64) -> Option<String> {
    let alias = champ_alias::champ_alias(champion_id)?.to_lowercase();
    let hashes = collect_chunk_hashes(mod_path);
    ability_bin_override(&hashes, &alias)
}

/// Core of `overrides_ability_data` with the alias already resolved — split out
/// so it's unit-testable without the bundled alias table (which reads from disk).
fn ability_bin_override(hashes: &HashSet<u64>, alias: &str) -> Option<String> {
    if hashes.is_empty() {
        return None;
    }
    let root_bin = format!("data/characters/{alias}/{alias}.bin");
    hashes
        .contains(&xxh64(root_bin.as_bytes(), 0))
        .then_some(root_bin)
}

/// Single-slot pick from a chunk-hash match: a mod whose WAD references
/// exactly one skin slot resolves to it, base included. Empty (no match) or
/// multi-slot (ambiguous, e.g. a chroma-VFX mod covering several chromas) both
/// fall through to `None` for the caller to resolve another way.
fn pick_single_slot(matches: &[i64]) -> Option<i64> {
    match matches {
        [id] => Some(*id),
        _ => None,
    }
}

/// Longest catalog skin name contained in the mod's file name (both sides
/// normalized to lowercase alphanumerics). Base is excluded — its name is the
/// champion name and would match nearly every mod title for the champ.
fn match_mod_name(mod_path: &Path, champ: &ChampionData, champion_id: i64) -> Option<i64> {
    let stem = mod_path.file_stem()?.to_string_lossy().into_owned();
    let stem_norm = normalize(&stem);
    if stem_norm.is_empty() {
        return None;
    }
    let mut best: Option<(usize, i64)> = None;
    for skin in champ.skins.as_deref().unwrap_or_default() {
        let Some(id) = skin.id.or(skin.skin_id) else { continue };
        if id % 1000 == 0 || id / 1000 != champion_id {
            continue;
        }
        let Some(name) = skin.name.as_deref().or(skin.skin_name.as_deref()) else { continue };
        let name_norm = normalize(name);
        if name_norm.len() >= 4 && stem_norm.contains(&name_norm) && best.is_none_or(|(len, _)| name_norm.len() > len) {
            best = Some((name_norm.len(), id));
        }
    }
    best.map(|(_, id)| id)
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

// ---------------------------------------------------------------------
// Chunk-hash collection (.fantome/.zip archive or extracted mod folder)
// ---------------------------------------------------------------------

fn collect_chunk_hashes(mod_path: &Path) -> HashSet<u64> {
    let mut hashes = HashSet::new();
    if mod_path.is_dir() {
        collect_from_dir(mod_path, &mut hashes);
    } else {
        collect_from_archive(mod_path, &mut hashes);
    }
    hashes
}

fn collect_from_archive(path: &Path, hashes: &mut HashSet<u64>) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let Ok(mut archive) = zip::ZipArchive::new(file) else {
        log_warn!("[TARGET] not a readable archive: {}", path.display());
        return;
    };
    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else { continue };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_lowercase().replace('\\', "/");
        if name.ends_with(".wad.client") {
            let size = entry.size();
            read_wad_hashes(&mut entry, size, hashes);
        } else if let Some(pos) = name.find(".wad.client/") {
            // Raw-folder WAD stored inside the archive (`WAD/X.wad.client/` as
            // a directory of loose files) — the file's path relative to the
            // wad dir stands in for a chunk hash.
            insert_raw_chunk(&name[pos + ".wad.client/".len()..], hashes);
        }
    }
}

/// A loose file inside a raw-folder WAD: hex-named files carry their chunk
/// hash directly; anything else hashes by its relative path.
fn insert_raw_chunk(rel: &str, hashes: &mut HashSet<u64>) {
    let stem = rel.rsplit('/').next().unwrap_or(rel).split('.').next().unwrap_or("");
    if stem.len() == 16 {
        if let Ok(h) = u64::from_str_radix(stem, 16) {
            hashes.insert(h);
            return;
        }
    }
    hashes.insert(xxh64(rel.as_bytes(), 0));
}

fn collect_from_dir(dir: &Path, hashes: &mut HashSet<u64>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_wad = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.to_lowercase().ends_with(".wad.client"));
        if path.is_dir() {
            if is_wad {
                // Raw-folder WAD: loose files stand in for chunks; hashing each
                // file's relative path is equivalent to a packed chunk hash.
                collect_raw_wad_dir(&path, &path, hashes);
            } else {
                collect_from_dir(&path, hashes);
            }
        } else if is_wad {
            if let Ok(mut file) = std::fs::File::open(&path) {
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                read_wad_hashes(&mut file, size, hashes);
            }
        }
    }
}

fn collect_raw_wad_dir(root: &Path, dir: &Path, hashes: &mut HashSet<u64>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_raw_wad_dir(root, &path, hashes);
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/").to_lowercase();
        insert_raw_chunk(&rel, hashes);
    }
}

/// Parse a WAD v3 header + chunk table from a stream, inserting each chunk's
/// xxh64 path hash. Layout: magic "RW" (2) + major/minor (2) + signature (256)
/// + checksum (8) + entry count (u32 LE) = 272 bytes, then 32-byte entries
/// whose first 8 bytes are the path hash.
fn read_wad_hashes<R: Read>(reader: &mut R, stream_size: u64, hashes: &mut HashSet<u64>) {
    let mut header = [0u8; 272];
    if reader.read_exact(&mut header).is_err() {
        return;
    }
    if &header[0..2] != b"RW" || header[2] != 3 {
        return;
    }
    let count = u32::from_le_bytes([header[268], header[269], header[270], header[271]]) as u64;
    let max_by_size = if stream_size > 272 { (stream_size - 272) / 32 } else { 0 };
    let count = count.min(max_by_size).min(MAX_WAD_ENTRIES) as usize;
    let mut table = vec![0u8; count * 32];
    if reader.read_exact(&mut table).is_err() {
        return;
    }
    for entry in table.chunks_exact(32) {
        hashes.insert(u64::from_le_bytes(entry[0..8].try_into().unwrap()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_punctuation() {
        assert_eq!(normalize("Soul Fighter Viego (VFX)"), "soulfighterviegovfx");
        assert_eq!(normalize("K/DA ALL OUT Kai'Sa"), "kdaalloutkaisa");
    }

    #[test]
    fn wad_v3_hash_extraction() {
        let mut wad = Vec::new();
        wad.extend_from_slice(b"RW");
        wad.push(3); // major
        wad.push(1); // minor
        wad.extend_from_slice(&[0u8; 256]); // signature
        wad.extend_from_slice(&[0u8; 8]); // checksum
        wad.extend_from_slice(&2u32.to_le_bytes()); // entry count
        for h in [0x1122334455667788u64, 0xAABBCCDDEEFF0011u64] {
            let mut entry = [0u8; 32];
            entry[0..8].copy_from_slice(&h.to_le_bytes());
            wad.extend_from_slice(&entry);
        }
        let mut hashes = HashSet::new();
        read_wad_hashes(&mut wad.as_slice(), wad.len() as u64, &mut hashes);
        assert_eq!(hashes, HashSet::from([0x1122334455667788u64, 0xAABBCCDDEEFF0011u64]));
    }

    fn viego() -> ChampionData {
        ChampionData { id: Some(234), name: Some("Viego".into()), alias: Some("Viego".into()), skins: None }
    }

    #[test]
    fn skin_bin_hash_matches_known_path() {
        // The forward-hash must be over the lowercase forward-slash path.
        let mut hashes = HashSet::new();
        hashes.insert(xxh64(b"data/characters/viego/skins/skin37.bin", 0));
        assert_eq!(match_skin_bins(&hashes, &viego(), 234), vec![234037]);
    }

    #[test]
    fn all_matched_slots_returned_ascending() {
        let mut hashes = HashSet::new();
        for nn in [36, 0, 31] {
            hashes.insert(xxh64(format!("data/characters/viego/skins/skin{nn}.bin").as_bytes(), 0));
        }
        assert_eq!(match_skin_bins(&hashes, &viego(), 234), vec![234000, 234031, 234036]);
    }

    #[test]
    fn raw_folder_wad_inside_zip_is_scanned() {
        // Mirrors the real "Soul Fighter Viego chroma VFX.fantome" layout:
        // WAD/Viego.wad.client/ is a DIRECTORY of loose files in the archive.
        let path = std::env::temp_dir().join("chud_target_detect_rawzip_test.fantome");
        let file = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        use std::io::Write as _;
        zw.start_file("META/info.json", opts).unwrap();
        zw.write_all(b"{}").unwrap();
        zw.start_file("WAD/Viego.wad.client/data/characters/Viego/skins/skin31.bin", opts).unwrap();
        zw.write_all(b"bin").unwrap();
        zw.start_file("WAD/Viego.wad.client/assets/SFFix/whatever.tex", opts).unwrap();
        zw.write_all(b"tex").unwrap();
        zw.finish().unwrap();

        let hashes = collect_chunk_hashes(&path);
        let _ = std::fs::remove_file(&path);
        assert!(hashes.contains(&xxh64(b"data/characters/viego/skins/skin31.bin", 0)));
        assert_eq!(match_skin_bins(&hashes, &viego(), 234), vec![234031]);
    }

    #[test]
    fn hex_named_raw_chunk_parses_hash() {
        let mut hashes = HashSet::new();
        insert_raw_chunk("data/aabbccdd00112233.dds", &mut hashes);
        assert!(hashes.contains(&0xaabbccdd00112233));
    }

    #[test]
    fn pick_single_slot_accepts_base() {
        assert_eq!(pick_single_slot(&[523000]), Some(523000));
    }

    #[test]
    fn pick_single_slot_accepts_specific_skin() {
        assert_eq!(pick_single_slot(&[523001]), Some(523001));
    }

    #[test]
    fn pick_single_slot_empty_is_none() {
        assert_eq!(pick_single_slot(&[]), None);
    }

    #[test]
    fn pick_single_slot_ambiguous_is_none() {
        assert_eq!(pick_single_slot(&[523000, 523001]), None);
    }

    #[test]
    fn ability_bin_override_flags_root_character_bin() {
        // A mod whose WAD ships the champion ROOT record breaks abilities.
        let mut hashes = HashSet::new();
        hashes.insert(xxh64(b"data/characters/gangplank/gangplank.bin", 0));
        assert_eq!(
            ability_bin_override(&hashes, "gangplank"),
            Some("data/characters/gangplank/gangplank.bin".to_string())
        );
    }

    #[test]
    fn ability_bin_override_ignores_cosmetic_only_mod() {
        // Skin/chroma bins + assets are cosmetic — never the root record.
        let mut hashes = HashSet::new();
        hashes.insert(xxh64(b"data/characters/gangplank/skins/skin2.bin", 0));
        hashes.insert(xxh64(b"assets/characters/gangplank/skins/skin02/whatever.dds", 0));
        assert_eq!(ability_bin_override(&hashes, "gangplank"), None);
    }

    #[test]
    fn ability_bin_override_empty_is_none() {
        assert_eq!(ability_bin_override(&HashSet::new(), "gangplank"), None);
    }
}
