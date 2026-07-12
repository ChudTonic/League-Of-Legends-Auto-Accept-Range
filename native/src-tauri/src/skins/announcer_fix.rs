//! Auto-fix downloaded announcer packs so they work on every current
//! map/mode — the in-app port of `tools/retarget-announcer.py` (see that
//! file for the full background), run at Library download time so packs are
//! fixed once on disk instead of during a live champ select.
//!
//! Community announcer packs replace the global female announcer banks
//! (`announcer_global_female1_vo_audio.wpk` + `_vo_events.bnk`) but almost
//! always target only Summoner's Rift (Map11) — and often ship the banks
//! inside a prebuilt full-map `.wad.client` member, which trips cslol
//! mkoverlay's fuzzy WAD matching into a multi-GB map rebuild. This rewrite
//! delivers the bank pair as loose WAD-folder entries targeting exactly the
//! small per-language WADs the banks live in:
//!
//!   Map11.en_US.wad  (Summoner's Rift)
//!   Map12.en_US.wad  (ARAM — classic Howling Abyss + global slot on variants)
//!   Map21.en_US.wad  (Nexus Blitz)
//!   + the ARAM "Bloom" map-skin announcer slot (`announcer_map12_bloom_vo_*`)
//!     — the variant bank re-binds the standard announcement events, so
//!     without this the pack is silently out-voiced on Bloom.
//!
//! The "Crepe" ARAM variant is deliberately left vanilla: its Wwise event
//! set is fully disjoint from the global announcer's, so replacing its bank
//! would mute that announcer entirely rather than reskin it.

use std::io::{Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::skins::slog::{log_info, log_warn};

const SHARED: &str = "assets/sounds/wwise2016/vo/en_us/shared/";
const AUDIO: &str = "announcer_global_female1_vo_audio.wpk";
const EVENTS: &str = "announcer_global_female1_vo_events.bnk";
const TARGET_WADS: [&str; 3] = ["Map11.en_US.wad", "Map12.en_US.wad", "Map21.en_US.wad"];
const BLOOM_AUDIO: &str = "announcer_map12_bloom_vo_audio.wpk";
const BLOOM_EVENTS: &str = "announcer_map12_bloom_vo_events.bnk";

/// Rewrite a downloaded announcer `.fantome`/`.zip` so its global-announcer
/// banks cover SR, ARAM (classic + Bloom variant), and Nexus Blitz.
///
/// Returns `Some(rewritten archive bytes)` when the pack contains the global
/// bank pair, `None` when it doesn't (not a recognizable global-announcer
/// pack — caller keeps the original bytes) or on any read error (a malformed
/// archive is left untouched rather than half-converted).
pub fn retarget_announcer_pack(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = match ZipArchive::new(Cursor::new(bytes)) {
        Ok(a) => a,
        Err(e) => {
            log_warn!("[LIBRARY] announcer-fix: unreadable archive ({e}) - leaving as downloaded");
            return None;
        }
    };

    let member_names: Vec<String> = archive.file_names().map(str::to_string).collect();
    let find = |basename: &str| {
        member_names
            .iter()
            .find(|n| n.starts_with("WAD/") && n.ends_with(&format!("/{SHARED}{basename}")))
            .cloned()
    };
    let (audio_member, events_member) = (find(AUDIO)?, find(EVENTS)?);

    let read_member = |archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str| -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        archive.by_name(name).ok()?.read_to_end(&mut buf).ok()?;
        Some(buf)
    };
    let audio = read_member(&mut archive, &audio_member)?;
    let events = read_member(&mut archive, &events_member)?;

    let mut out = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default();

    // Preserve everything outside WAD/ (META, previews...); every WAD entry
    // is replaced by the retargeted set below — this also drops prebuilt
    // `*.wad.client` file members (the multi-GB rebuild trap).
    for name in &member_names {
        if name.starts_with("WAD/") || name.ends_with('/') {
            continue;
        }
        let data = read_member(&mut archive, name)?;
        out.start_file(name.as_str(), opts).ok()?;
        out.write_all(&data).ok()?;
    }
    for wad in TARGET_WADS {
        for (basename, data) in [(AUDIO, &audio), (EVENTS, &events)] {
            out.start_file(format!("WAD/{wad}/{SHARED}{basename}"), opts).ok()?;
            out.write_all(data).ok()?;
        }
    }
    for (basename, data) in [(BLOOM_AUDIO, &audio), (BLOOM_EVENTS, &events)] {
        out.start_file(format!("WAD/Map12.en_US.wad/{SHARED}{basename}"), opts).ok()?;
        out.write_all(data).ok()?;
    }

    let rewritten = out.finish().ok()?.into_inner();
    log_info!(
        "[LIBRARY] announcer-fix: retargeted global announcer banks to SR/ARAM/Bloom/Nexus Blitz ({} -> {} bytes)",
        bytes.len(),
        rewritten.len()
    );
    Some(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_pack(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zw = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            zw.start_file(*name, SimpleFileOptions::default()).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap().into_inner()
    }

    fn names_of(bytes: &[u8]) -> Vec<String> {
        ZipArchive::new(Cursor::new(bytes)).unwrap().file_names().map(str::to_string).collect()
    }

    #[test]
    fn retargets_sr_only_pack_to_all_modes_and_drops_prebuilt_wad() {
        let pack = build_pack(&[
            ("META/info.json", b"{}"),
            ("WAD/Map11.wad.client", b"prebuilt-wad-bytes"),
            (&format!("WAD/Map11.wad/{SHARED}{AUDIO}"), b"rick-and-morty-audio"),
            (&format!("WAD/Map11.wad/{SHARED}{EVENTS}"), b"rick-and-morty-events"),
        ]);
        let fixed = retarget_announcer_pack(&pack).expect("global pack must convert");
        let names = names_of(&fixed);

        assert!(names.contains(&"META/info.json".to_string()));
        assert!(!names.iter().any(|n| n == "WAD/Map11.wad.client"), "prebuilt wad member must be dropped");
        for wad in TARGET_WADS {
            assert!(names.contains(&format!("WAD/{wad}/{SHARED}{AUDIO}")), "missing audio for {wad}");
            assert!(names.contains(&format!("WAD/{wad}/{SHARED}{EVENTS}")), "missing events for {wad}");
        }
        assert!(names.contains(&format!("WAD/Map12.en_US.wad/{SHARED}{BLOOM_AUDIO}")));
        assert!(names.contains(&format!("WAD/Map12.en_US.wad/{SHARED}{BLOOM_EVENTS}")));

        // Idempotent: converting a converted pack yields the same entry set.
        let again = retarget_announcer_pack(&fixed).expect("converted pack still recognizable");
        let mut a = names_of(&again);
        let mut b = names_of(&fixed);
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    #[test]
    fn non_announcer_pack_is_left_alone() {
        let pack = build_pack(&[
            ("META/info.json", b"{}"),
            ("WAD/Ahri.wad.client/assets/characters/ahri/skins/base/ahri.skn", b"model"),
        ]);
        assert!(retarget_announcer_pack(&pack).is_none());
        assert!(retarget_announcer_pack(b"not a zip at all").is_none());
    }
}
