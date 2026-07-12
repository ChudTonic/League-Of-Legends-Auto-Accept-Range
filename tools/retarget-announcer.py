"""Retarget an announcer .fantome so it works on every current map/mode.

Most community announcer packs replace the global female announcer banks
(`announcer_global_female1_vo_audio.wpk` + `_vo_events.bnk`) but only target
Summoner's Rift (Map11), and often ship them inside a prebuilt full-map
`.wad.client` member that forces cslol mkoverlay into a multi-GB map rebuild.

This tool rewrites the archive so the bank pair is delivered as loose
WAD-folder entries targeting exactly the small per-language WADs the banks
live in:

  Map11.en_US.wad  (Summoner's Rift)
  Map12.en_US.wad  (ARAM - classic Howling Abyss + global slot on variants)
  Map21.en_US.wad  (Nexus Blitz)
  + the ARAM "Bloom" map-skin variant slot in Map12.en_US
    (announcer_map12_bloom_vo_*) - the variant bank re-binds the standard
    announcement events, so without this the pack is silently out-voiced on
    Bloom. The "Crepe" variant uses a fully disjoint Wwise event set and is
    deliberately left vanilla (replacing it would mute it entirely).

Prebuilt `*.wad.client` FILE members are dropped (the loose entries replace
them). Everything under META/ is preserved. A `.bak` copy is written beside
the source on first run.

Usage: python tools/retarget-announcer.py "<pack.fantome>" [more packs...]
"""

import os
import shutil
import sys
import zipfile

SHARED = "assets/sounds/wwise2016/vo/en_us/shared/"
AUDIO = "announcer_global_female1_vo_audio.wpk"
EVENTS = "announcer_global_female1_vo_events.bnk"
TARGET_WADS = ("Map11.en_US.wad", "Map12.en_US.wad", "Map21.en_US.wad")
BLOOM = "announcer_map12_bloom_vo_%s"


def find_member(zin: zipfile.ZipFile, basename: str) -> str | None:
    for name in zin.namelist():
        if name.startswith("WAD/") and name.endswith("/" + SHARED + basename):
            return name
    return None


def is_prebuilt_wad_member(name: str) -> bool:
    # A literal packed WAD file, e.g. "WAD/Map11.wad.client" - as opposed to a
    # WAD-FOLDER entry like "WAD/Map11.en_US.wad.client/assets/...".
    return name.startswith("WAD/") and name.endswith(".wad.client") and name.count("/") == 1


def retarget(path: str) -> bool:
    zin = zipfile.ZipFile(path)
    audio_member = find_member(zin, AUDIO)
    events_member = find_member(zin, EVENTS)
    if not audio_member or not events_member:
        print(f"SKIP {path}: no {AUDIO}/{EVENTS} pair found (not a global-announcer pack?)")
        return False

    audio = zin.read(audio_member)
    events = zin.read(events_member)

    bak = path + ".bak"
    if not os.path.exists(bak):
        shutil.copy2(path, bak)

    tmp = path + ".new"
    with zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
        for item in zin.infolist():
            if item.filename.startswith("WAD/"):
                continue  # replaced by the retargeted entries below
            zout.writestr(item, zin.read(item.filename))
        for wad in TARGET_WADS:
            zout.writestr(f"WAD/{wad}/{SHARED}{AUDIO}", audio)
            zout.writestr(f"WAD/{wad}/{SHARED}{EVENTS}", events)
        zout.writestr(f"WAD/Map12.en_US.wad/{SHARED}" + BLOOM % "audio.wpk", audio)
        zout.writestr(f"WAD/Map12.en_US.wad/{SHARED}" + BLOOM % "events.bnk", events)
    zin.close()
    os.replace(tmp, path)
    print(f"OK   {path} (backup: {bak})")
    return True


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    ok = all(retarget(p) for p in sys.argv[1:])
    sys.exit(0 if ok else 1)
