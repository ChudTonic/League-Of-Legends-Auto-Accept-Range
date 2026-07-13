//! Announcer Studio — build a custom announcer `.fantome` from user-supplied
//! audio, entirely in-app with no external encoder.
//!
//! The frontend decodes any dropped/recorded audio (mp3/wav/ogg/mic) via the
//! WebAudio API, resamples to mono 16-bit 44.1kHz PCM, and hands us the raw
//! samples per slot as base64. We wrap each into a Wwise-PCM `.wem`, swap it
//! into the bundled vanilla announcer WPK for every game wem-id that slot
//! covers, flip those sound objects' codec Vorbis->PCM in the bundled events
//! bank, and write the pack into the announcers mod folder. The existing
//! `mod_scope`/`announcer_fix` sweep then retargets it for SR/ARAM/NexusBlitz.
//!
//! CODEC NOTE (learned 2026-07-13): PCM plays for the COMMON announcements but
//! the game preloads MILESTONE lines (First Blood, Ace, Victory, Penta,
//! Godlike, Legendary...) at match start and rejects PCM for them — they go
//! silent. So milestone slots are marked `milestone: true`; the UI warns, and
//! by default we DON'T patch them (they keep the official announcer instead of
//! going silent). Full milestone coverage needs real Wwise Vorbis, which is
//! not bundle-able. See `memory/announcer-studio.md`.

#![allow(dead_code)]

use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::skins::injection::tools::resources_root;
use crate::skins::paths;
use crate::skins::slog::{log_info, log_warn};

/// One assignable announcer slot: a user-facing line that maps to the set of
/// vanilla game wem-ids that voice it (kept in sync with the game version the
/// bundled template was extracted from).
#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub key: &'static str,
    pub category: &'static str,
    pub label: &'static str,
    /// Game preloads this line and rejects PCM (goes silent) — UI warns, and
    /// `build` skips it unless the caller opts in.
    pub milestone: bool,
    pub wems: &'static [u64],
}

/// The slot map (generated from the extracted announcer bank — see
/// `announcer-studio-lab/slots_rust.txt`). Order defines UI display order.
pub const SLOTS: &[Slot] = &[
    Slot { key: "welcome", category: "Game Start", label: "Welcome / loading in", milestone: false, wems: &[133352710, 275092472, 661954766, 847997631] },
    Slot { key: "minions", category: "Game Start", label: "Minions have spawned", milestone: false, wems: &[696717127] },
    Slot { key: "first_blood", category: "Kills", label: "First Blood", milestone: true, wems: &[835992869] },
    Slot { key: "you_slain", category: "Kills", label: "You slay an enemy", milestone: false, wems: &[142371233, 764169913, 819443807] },
    Slot { key: "enemy_slain", category: "Kills", label: "An enemy is slain", milestone: false, wems: &[530328154, 667553938, 830275014, 880045185, 1060851641] },
    Slot { key: "shutdown", category: "Kills", label: "Shutdown", milestone: false, wems: &[7615127, 140192580] },
    Slot { key: "double", category: "Multikills", label: "Double Kill", milestone: false, wems: &[38776155, 72637943, 247555986, 655441407, 752669781, 961525497] },
    Slot { key: "triple", category: "Multikills", label: "Triple Kill", milestone: false, wems: &[411304003, 433327575, 457215657, 983404894, 1058636111] },
    Slot { key: "quadra", category: "Multikills", label: "Quadra Kill", milestone: true, wems: &[391206161, 688775583, 772685139, 1006055594] },
    Slot { key: "penta", category: "Multikills", label: "Pentakill", milestone: true, wems: &[265009690, 265378219, 284666514, 299975130, 963502603] },
    Slot { key: "spree", category: "Sprees", label: "Killing Spree", milestone: false, wems: &[254310945, 459275864] },
    Slot { key: "rampage", category: "Sprees", label: "Rampage", milestone: false, wems: &[40291998, 283092143] },
    Slot { key: "unstoppable", category: "Sprees", label: "Unstoppable", milestone: false, wems: &[263629164] },
    Slot { key: "dominating", category: "Sprees", label: "Dominating", milestone: false, wems: &[268856538] },
    Slot { key: "godlike", category: "Sprees", label: "Godlike", milestone: true, wems: &[122337671, 172470563] },
    Slot { key: "legendary", category: "Sprees", label: "Legendary", milestone: true, wems: &[77372174, 110237757, 202559117, 226653189, 578494770, 913201361] },
    Slot { key: "ace", category: "Team", label: "Your team Aces", milestone: true, wems: &[742885774, 748986976, 864760881] },
    Slot { key: "you_slain_death", category: "Deaths", label: "You are slain", milestone: false, wems: &[5903427, 328256082] },
    Slot { key: "ally_slain", category: "Deaths", label: "An ally is slain", milestone: false, wems: &[286811631, 1010378437] },
    Slot { key: "turret_kill", category: "Objectives", label: "You destroy a turret", milestone: false, wems: &[598034052] },
    Slot { key: "turret_yours", category: "Objectives", label: "Your turret destroyed", milestone: false, wems: &[368627200, 558365122] },
    Slot { key: "inhib_kill", category: "Objectives", label: "You destroy an inhibitor", milestone: false, wems: &[486061461, 863956123] },
    Slot { key: "inhib_yours", category: "Objectives", label: "Your inhibitor destroyed", milestone: false, wems: &[447246580, 701263321] },
    Slot { key: "nexus", category: "Objectives", label: "Nexus under attack", milestone: false, wems: &[688496515, 786182387, 1002918651] },
    Slot { key: "victory", category: "End", label: "Victory", milestone: true, wems: &[16244311, 139054080, 619095156, 933211269] },
    Slot { key: "defeat", category: "End", label: "Defeat", milestone: true, wems: &[741535347] },
];

const SHARED: &str = "assets/sounds/wwise2016/vo/en_us/shared/";
const AUDIO_WPK: &str = "announcer_global_female1_vo_audio.wpk";
const EVENTS_BNK: &str = "announcer_global_female1_vo_events.bnk";
const VORBIS: u32 = 0x0004_0001;
const PCM: u32 = 0x0001_0001;

fn studio_res_dir() -> PathBuf {
    resources_root().join("announcer-studio")
}

/// One slot's assigned audio, from the UI.
#[derive(Debug, Deserialize)]
pub struct SlotAudio {
    pub key: String,
    /// base64 of raw mono 16-bit-LE 44.1kHz PCM samples (WebAudio-decoded).
    pub pcm_base64: String,
    pub sample_rate: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct BuildResult {
    pub ok: bool,
    pub file: Option<String>,
    pub slots_filled: usize,
    pub milestones_skipped: usize,
    pub error: Option<String>,
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    // Minimal base64 decoder (avoid pulling a new dep just for this).
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut inv = [255u8; 256];
    for (i, &c) in T.iter().enumerate() {
        inv[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for &c in s.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = inv[c as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Wrap raw mono 16-bit PCM into a Wwise-PCM `.wem` (RIFF, fmt 0xFFFE
/// extensible). Matches the format proven to play for common lines.
fn pcm_to_wem(pcm: &[u8], rate: u32) -> Vec<u8> {
    let block: u16 = 2; // mono, 16-bit
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes()); // format tag: extensible
    fmt.extend_from_slice(&1u16.to_le_bytes()); // channels
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate * block as u32).to_le_bytes()); // byte rate
    fmt.extend_from_slice(&block.to_le_bytes()); // block align
    fmt.extend_from_slice(&16u16.to_le_bytes()); // bits
    fmt.extend_from_slice(&6u16.to_le_bytes()); // cbSize
    fmt.extend_from_slice(&16u16.to_le_bytes()); // valid bits
    fmt.extend_from_slice(&0x4u32.to_le_bytes()); // channel mask (mono)

    let mut chunks = Vec::new();
    chunks.extend_from_slice(b"fmt ");
    chunks.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    chunks.extend_from_slice(&fmt);
    chunks.extend_from_slice(b"data");
    chunks.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    chunks.extend_from_slice(pcm);

    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + chunks.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(&chunks);
    out
}

/// Read the bundled vanilla WPK and return (version, [(wem_id, bytes)]).
fn read_vanilla_wpk() -> Result<Vec<(u64, Vec<u8>)>, String> {
    let path = studio_res_dir().join("vanilla_audio.wpk");
    let data = std::fs::read(&path).map_err(|e| format!("bundled wpk missing ({}): {e}", path.display()))?;
    if &data[..4] != b"r3d2" {
        return Err("bundled wpk not a WPK".into());
    }
    let count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let eoff = u32::from_le_bytes(data[12 + i * 4..16 + i * 4].try_into().unwrap()) as usize;
        if eoff == 0 || eoff + 12 > data.len() {
            continue;
        }
        let doff = u32::from_le_bytes(data[eoff..eoff + 4].try_into().unwrap()) as usize;
        let dsize = u32::from_le_bytes(data[eoff + 4..eoff + 8].try_into().unwrap()) as usize;
        let nlen = u32::from_le_bytes(data[eoff + 8..eoff + 12].try_into().unwrap()) as usize;
        let name = String::from_utf16_lossy(
            &data[eoff + 12..eoff + 12 + nlen * 2]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        );
        let wid: u64 = name.trim_end_matches(".wem").parse().unwrap_or(0);
        out.push((wid, data[doff..doff + dsize].to_vec()));
    }
    Ok(out)
}

/// Serialize wems back into a WPK v1 container.
fn write_wpk(entries: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let n = entries.len();
    let mut header = Vec::new();
    header.extend_from_slice(b"r3d2");
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&(n as u32).to_le_bytes());
    let off_table = header.len();
    let meta_start = off_table + 4 * n;
    let names: Vec<Vec<u16>> = entries.iter().map(|(id, _)| format!("{id}.wem").encode_utf16().collect()).collect();
    let mut data_start = meta_start;
    for name in &names {
        data_start += 12 + name.len() * 2;
    }
    let mut offsets = Vec::new();
    let mut metas = Vec::new();
    let mut blobs = Vec::new();
    let mut cur_meta = meta_start;
    let mut cur_data = data_start;
    for (i, (_, data)) in entries.iter().enumerate() {
        offsets.push(cur_meta as u32);
        metas.extend_from_slice(&(cur_data as u32).to_le_bytes());
        metas.extend_from_slice(&(data.len() as u32).to_le_bytes());
        metas.extend_from_slice(&(names[i].len() as u32).to_le_bytes());
        for u in &names[i] {
            metas.extend_from_slice(&u.to_le_bytes());
        }
        cur_meta += 12 + names[i].len() * 2;
        cur_data += data.len();
        blobs.extend_from_slice(data);
    }
    let mut out = header;
    for o in offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&metas);
    out.extend_from_slice(&blobs);
    out
}

/// Flip the codec of every Sound object whose source id is in `pcm_ids` from
/// Vorbis to PCM, in the bundled events bank. Returns the patched bank.
fn patch_bank_codecs(pcm_ids: &std::collections::HashSet<u64>) -> Result<Vec<u8>, String> {
    let path = studio_res_dir().join("template_events.bnk");
    let mut bnk = std::fs::read(&path).map_err(|e| format!("bundled bank missing: {e}"))?;
    let mut pos = 0usize;
    while pos + 8 <= bnk.len() {
        let tag = &bnk[pos..pos + 4];
        let length = u32::from_le_bytes(bnk[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if tag == b"HIRC" {
            let cnt = u32::from_le_bytes(bnk[pos + 8..pos + 12].try_into().unwrap()) as usize;
            let mut p = pos + 12;
            for _ in 0..cnt {
                if p + 5 > bnk.len() {
                    break;
                }
                let typ = bnk[p];
                let sz = u32::from_le_bytes(bnk[p + 1..p + 5].try_into().unwrap()) as usize;
                if typ == 2 && p + 5 + 13 <= bnk.len() {
                    let src = u64::from(u32::from_le_bytes(bnk[p + 5 + 9..p + 5 + 13].try_into().unwrap()));
                    if pcm_ids.contains(&src) {
                        bnk[p + 5 + 4..p + 5 + 8].copy_from_slice(&PCM.to_le_bytes());
                    }
                }
                p += 5 + sz;
            }
            break;
        }
        pos += 8 + length;
    }
    Ok(bnk)
}

/// Build + install a custom announcer pack. `include_milestones` attempts the
/// milestone slots too (they'll likely be silent — see module note); default
/// false leaves them as the official announcer.
pub fn build_pack(name: &str, slots: &[SlotAudio], include_milestones: bool) -> BuildResult {
    let fail = |e: String| BuildResult { ok: false, file: None, slots_filled: 0, milestones_skipped: 0, error: Some(e) };

    let mut vanilla = match read_vanilla_wpk() {
        Ok(v) => v,
        Err(e) => return fail(e),
    };

    let mut pcm_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut filled = 0usize;
    let mut skipped = 0usize;

    for sa in slots {
        let Some(slot) = SLOTS.iter().find(|s| s.key == sa.key) else {
            log_warn!("[STUDIO] unknown slot key '{}' - ignored", sa.key);
            continue;
        };
        if slot.milestone && !include_milestones {
            skipped += 1;
            continue;
        }
        let Some(pcm) = b64_decode(&sa.pcm_base64) else {
            log_warn!("[STUDIO] bad base64 for slot '{}' - skipped", sa.key);
            continue;
        };
        if pcm.len() < 64 {
            continue;
        }
        let wem = pcm_to_wem(&pcm, sa.sample_rate.unwrap_or(44100));
        for &wid in slot.wems {
            if let Some(entry) = vanilla.iter_mut().find(|(id, _)| *id == wid) {
                entry.1 = wem.clone();
                pcm_ids.insert(wid);
            }
        }
        filled += 1;
    }

    if filled == 0 {
        return fail("No audio was assigned to any slot.".into());
    }

    let wpk = write_wpk(&vanilla);
    let bnk = match patch_bank_codecs(&pcm_ids) {
        Ok(b) => b,
        Err(e) => return fail(e),
    };

    let stem = sanitize(name);
    let dest = paths::mods_dir().join("announcers").join(format!("{stem}.fantome"));
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = match std::fs::File::create(&dest) {
        Ok(f) => f,
        Err(e) => return fail(format!("could not write pack: {e}")),
    };
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    let info = format!(
        "{{\"Name\":\"{}\",\"Author\":\"Chud Announcer Studio\",\"Version\":\"1.0.0\",\"Description\":\"Custom announcer built in Chud\"}}",
        stem.replace('"', "'")
    );
    let mut write_entry = |zip: &mut zip::ZipWriter<std::fs::File>, path: String, data: &[u8]| -> Result<(), String> {
        zip.start_file(path, opts).map_err(|e| e.to_string())?;
        zip.write_all(data).map_err(|e| e.to_string())?;
        Ok(())
    };
    let r = (|| {
        write_entry(&mut zip, "META/info.json".into(), info.as_bytes())?;
        write_entry(&mut zip, format!("WAD/Map11.en_US.wad/{SHARED}{AUDIO_WPK}"), &wpk)?;
        write_entry(&mut zip, format!("WAD/Map11.en_US.wad/{SHARED}{EVENTS_BNK}"), &bnk)?;
        zip.finish().map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })();
    if let Err(e) = r {
        return fail(format!("could not pack pack: {e}"));
    }

    log_info!("[STUDIO] Built announcer pack '{stem}': {filled} slot(s), {skipped} milestone(s) left vanilla");
    BuildResult {
        ok: true,
        file: Some(dest.to_string_lossy().into_owned()),
        slots_filled: filled,
        milestones_skipped: skipped,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_wem_is_valid_riff_with_extensible_fmt() {
        let pcm = vec![0u8; 4000];
        let wem = pcm_to_wem(&pcm, 44100);
        assert_eq!(&wem[..4], b"RIFF");
        assert_eq!(&wem[8..12], b"WAVE");
        let tag = u16::from_le_bytes(wem[20..22].try_into().unwrap());
        assert_eq!(tag, 0xFFFE, "extensible fmt tag");
    }

    #[test]
    fn wpk_roundtrips_names_and_data() {
        let entries = vec![(111u64, vec![1u8, 2, 3]), (222u64, vec![9u8; 10])];
        let wpk = write_wpk(&entries);
        assert_eq!(&wpk[..4], b"r3d2");
        let count = u32::from_le_bytes(wpk[8..12].try_into().unwrap()) as usize;
        assert_eq!(count, 2);
        // parse back the first entry
        let eoff = u32::from_le_bytes(wpk[12..16].try_into().unwrap()) as usize;
        let doff = u32::from_le_bytes(wpk[eoff..eoff + 4].try_into().unwrap()) as usize;
        let dsize = u32::from_le_bytes(wpk[eoff + 4..eoff + 8].try_into().unwrap()) as usize;
        let nlen = u32::from_le_bytes(wpk[eoff + 8..eoff + 12].try_into().unwrap()) as usize;
        let name: String = String::from_utf16_lossy(
            &wpk[eoff + 12..eoff + 12 + nlen * 2].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>(),
        );
        assert_eq!(name, "111.wem");
        assert_eq!(&wpk[doff..doff + dsize], &[1u8, 2, 3]);
    }

    #[test]
    fn b64_decode_matches_known_vector() {
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode("AAAA").unwrap(), vec![0u8, 0, 0]);
    }

    #[test]
    fn slots_have_no_duplicate_wem_across_groups() {
        let mut seen = std::collections::HashSet::new();
        for s in SLOTS {
            for &w in s.wems {
                assert!(seen.insert(w), "wem {w} appears in more than one slot (slot {})", s.key);
            }
        }
    }
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name.chars().filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_').collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Custom Announcer".into()
    } else {
        cleaned.to_string()
    }
}
