//! "Skin name on the loading screen" — the game's loadscreen card shows no name
//! text, so we bake one on: fetch the skin's loadscreen card from
//! CommunityDragon, draw the full skin name across the bottom, re-encode it to
//! Riot's `.tex` (BC1) at the exact WAD path the game reads, and drop the result
//! into the injection mods dir so `mkoverlay` folds it into the overlay.
//!
//! Verified against a real extracted WAD (2026-07-18): the card lives at
//! `assets/characters/{key}/skins/skin{NN:02}/{key}loadscreen_{N}.tex` (base:
//! `skins/base/{key}loadscreen.tex`), format 308×560 BC1, `.tex` = a 12-byte
//! `TEX\0` header (`magic4 | u16 w | u16 h | 01 | 0x0A(BC1) | 00 | 00`) then the
//! raw BCn payload. Best-effort throughout — a label failure must NEVER block
//! the skin itself from injecting.

use std::collections::HashSet;
use std::path::PathBuf;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use image::{Rgba, RgbaImage};

use crate::skins::slog::{log_info, log_warn};

const CDRAGON: &str = "raw.communitydragon.org";
/// Canonical loadscreen card size (both divisible by 4 for BC1 blocks).
const CARD_W: u32 = 308;
const CARD_H: u32 = 560;
/// Folder name of the generated overlay mod (single-slot; rebuilt each pick).
pub const MOD_NAME: &str = "chud_loadscreen";
/// cslol mod manifest — mkoverlay rejects a mod folder without `META/info.json`.
const MOD_INFO_JSON: &str =
    r#"{"Author":"Chud","Description":"Loadscreen skin-name label","Name":"Chud Loadscreen","Version":"1.0.0"}"#;

/// Riot's actual League display font — the same "Beaufort for LOL" bold used for
/// champion/skin names in-client, so the baked card matches the game's own type.
/// It's proprietary, so we do NOT bundle it: we fetch the game asset from
/// CommunityDragon (same host/posture as the loadscreen art) and cache it on
/// disk, loading from cache on every subsequent card.
const RIOT_FONT_FILE: &str = "beaufortforlol-bold.otf";
const RIOT_FONT_URL: &str = "https://raw.communitydragon.org/latest/game/assets/ux/fonts/beaufortforlol-bold.otf";

fn font_cache_path() -> PathBuf {
    crate::skins::paths::data_root().join("cache").join("fonts").join(RIOT_FONT_FILE)
}

/// Load Riot's loadscreen font: from the on-disk cache if present, otherwise
/// fetch it once from CommunityDragon and cache it. `None` on any failure
/// (network down, parse error) so the caller falls back to no label — never a
/// wrong font.
async fn load_riot_font(http: &reqwest::Client, allowed: &HashSet<String>) -> Option<FontVec> {
    let cache = font_cache_path();
    if let Ok(bytes) = std::fs::read(&cache) {
        if let Ok(font) = FontVec::try_from_vec(bytes) {
            return Some(font);
        }
    }
    let bytes = match crate::net::get_bytes_checked(http, RIOT_FONT_URL, allowed, 4 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            log_warn!("[LOADSCREEN] Riot font fetch failed ({RIOT_FONT_URL}): {e}");
            return None;
        }
    };
    // Cache best-effort; a write failure just means we refetch next time.
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, &bytes);
    FontVec::try_from_vec(bytes).ok()
}

/// CommunityDragon URL + the in-WAD `.tex` path for a skin's loadscreen card.
fn loadscreen_paths(champ_key: &str, num: i64) -> (String, String) {
    if num == 0 {
        (
            format!("https://{CDRAGON}/latest/game/assets/characters/{champ_key}/skins/base/{champ_key}loadscreen.png"),
            format!("assets/characters/{champ_key}/skins/base/{champ_key}loadscreen.tex"),
        )
    } else {
        (
            format!("https://{CDRAGON}/latest/game/assets/characters/{champ_key}/skins/skin{num:02}/{champ_key}loadscreen_{num}.png"),
            format!("assets/characters/{champ_key}/skins/skin{num:02}/{champ_key}loadscreen_{num}.tex"),
        )
    }
}

/// The base-skin loadscreen `.tex` path. An UNOWNED skin injects with the client
/// forced to the base slot (the `.fantome` swaps the art in), so the game may
/// request THIS path rather than the skin-numbered one — we write the card to
/// both so it shows regardless of which the game asks for.
fn base_tex_path(champ_key: &str) -> String {
    format!("assets/characters/{champ_key}/skins/base/{champ_key}loadscreen.tex")
}

/// Draw `name` near the lower third of the card: a dark gradient scrim so text
/// is legible over any splash, then the name centered with a hard shadow. The
/// text baseline sits well above the bottom edge — the in-game loadscreen frame
/// and summoner-name bar overlap the card's bottom ~12%, which would clip a
/// bottom-anchored label.
fn draw_skin_name(img: &mut RgbaImage, font: &FontVec, name: &str) {
    // Fraction of the card height reserved below the text for the game's frame.
    const BOTTOM_INSET: f32 = 0.14;
    let bottom = (CARD_H as f32 * (1.0 - BOTTOM_INSET)) as u32; // text sits at/above here
    // Scrim: fade a translucent black band up to `bottom` so the raised text
    // still reads over a bright splash.
    let band_h = (CARD_H as f32 * 0.24) as u32;
    let band_top = bottom.saturating_sub(band_h);
    for y in band_top..bottom {
        let t = (y - band_top) as f32 / band_h as f32; // 0 at band top → 1 at band bottom
        let a = (t * t * 200.0) as u16; // ease-in, up to ~0.78 alpha
        for x in 0..CARD_W {
            let px = img.get_pixel_mut(x, y);
            for c in 0..3 {
                px[c] = ((px[c] as u16 * (255 - a)) / 255) as u8;
            }
        }
    }
    // Auto-fit the font size so the name fits the card width with margins.
    let max_w = CARD_W as f32 - 24.0;
    let mut size = 34.0f32;
    while size > 14.0 && text_width(font, size, name) > max_w {
        size -= 1.0;
    }
    let tw = text_width(font, size, name);
    let x = ((CARD_W as f32 - tw) / 2.0).max(6.0);
    let y = bottom as f32 - size - 6.0; // baseline sits just inside the safe area
    // Hard shadow then the fill.
    draw_text(img, font, name, x + 1.5, y + 1.5, size, Rgba([0, 0, 0, 220]));
    draw_text(img, font, name, x, y, size, Rgba([255, 255, 255, 255]));
}

fn text_width(font: &FontVec, size: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        let g = font.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, g);
        }
        w += scaled.h_advance(g);
        prev = Some(g);
    }
    w
}

/// Minimal glyph rasterizer (avoids pulling imageproc's text path) — alpha-blend
/// each glyph's coverage onto the image.
fn draw_text(img: &mut RgbaImage, font: &FontVec, text: &str, x: f32, y: f32, size: f32, color: Rgba<u8>) {
    let scaled = font.as_scaled(PxScale::from(size));
    let ascent = scaled.ascent();
    let mut cx = x;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        let gid = font.glyph_id(ch);
        if let Some(p) = prev {
            cx += scaled.kern(p, gid);
        }
        let glyph = gid.with_scale_and_position(size, ab_glyph::point(cx, y + ascent));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && (px as u32) < img.width() && (py as u32) < img.height() {
                    let dst = img.get_pixel_mut(px as u32, py as u32);
                    let a = cov * (color[3] as f32 / 255.0);
                    for c in 0..3 {
                        dst[c] = (dst[c] as f32 * (1.0 - a) + color[c] as f32 * a) as u8;
                    }
                }
            });
        }
        cx += scaled.h_advance(gid);
        prev = Some(gid);
    }
}

/// Encode an RGBA card to a Riot `.tex`: 12-byte header + raw BC1 payload.
fn encode_tex_bc1(img: &RgbaImage) -> Option<Vec<u8>> {
    let surface = image_dds::SurfaceRgba8::from_image(img)
        .encode(image_dds::ImageFormat::BC1RgbaUnorm, image_dds::Quality::Slow, image_dds::Mipmaps::Disabled)
        .ok()?;
    let mut out = Vec::with_capacity(12 + surface.data.len());
    out.extend_from_slice(b"TEX\0");
    out.extend_from_slice(&(img.width() as u16).to_le_bytes());
    out.extend_from_slice(&(img.height() as u16).to_le_bytes());
    out.extend_from_slice(&[0x01, 0x0A, 0x00, 0x00]); // unk=1, format=BC1(0x0A), unk=0, mips=0
    out.extend_from_slice(&surface.data);
    Some(out)
}

/// Build the loadscreen-name overlay for `skin_id` under the injection mods dir,
/// returning the mod folder name to fold into the overlay (or None on any
/// failure — always best-effort so the skin still injects).
pub async fn build(
    skin_id: i64,
    skin_name: &str,
    champ_key: &str,
    champ_alias: &str,
    http: &reqwest::Client,
    allowed: &HashSet<String>,
) -> Option<String> {
    let num = skin_id % 1000;
    let (url, inner_tex) = loadscreen_paths(champ_key, num);
    log_info!("[LOADSCREEN] build skin_id={skin_id} num={num} name='{skin_name}' alias='{champ_alias}' -> {inner_tex}");

    let png = match crate::net::get_bytes_checked(http, &url, allowed, 8 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            log_warn!("[LOADSCREEN] card source unavailable for {skin_name} ({url}): {e}");
            return None;
        }
    };
    let Some(font) = load_riot_font(http, allowed).await else {
        log_warn!("[LOADSCREEN] font unavailable — skipping card for '{skin_name}'");
        return None;
    };
    let mut img = match image::load_from_memory(&png) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            log_warn!("[LOADSCREEN] card PNG decode failed for '{skin_name}' ({url}): {e}");
            return None;
        }
    };
    if img.width() != CARD_W || img.height() != CARD_H {
        img = image::imageops::resize(&img, CARD_W, CARD_H, image::imageops::FilterType::Lanczos3);
    }
    draw_skin_name(&mut img, &font, skin_name);
    let Some(tex) = encode_tex_bc1(&img) else {
        log_warn!("[LOADSCREEN] .tex BC1 encode failed for '{skin_name}'");
        return None;
    };

    // Write the card into <injection mods>/<MOD_NAME>/WAD/<Alias>.wad.client/.
    // Owned skins load the skin-numbered loadscreen; an unowned skin is forced
    // to the base slot (its `.fantome` swaps the art), so the game may request
    // the base path instead — write both, de-duped (base skins only have one).
    let mod_root = crate::skins::paths::injection_mods_dir().join(MOD_NAME);
    // mkoverlay rejects a mod folder without a META/info.json ("Not valid mod!",
    // exit 1) — which fails the WHOLE overlay build, dropping the real skin back
    // to base. Write the manifest before the WAD payload.
    let meta_dir = mod_root.join("META");
    if let Err(e) = std::fs::create_dir_all(&meta_dir) {
        log_warn!("[LOADSCREEN] META mkdir failed for {}: {e}", meta_dir.display());
        return None;
    }
    if let Err(e) = std::fs::write(meta_dir.join("info.json"), MOD_INFO_JSON) {
        log_warn!("[LOADSCREEN] META/info.json write failed: {e}");
        return None;
    }
    let wad_root = mod_root
        .join("WAD")
        .join(format!("{champ_alias}.wad.client"));
    let base_tex = base_tex_path(champ_key);
    let mut targets = vec![inner_tex.clone()];
    if num != 0 && base_tex != inner_tex {
        targets.push(base_tex);
    }
    let mut wrote = 0;
    for rel in &targets {
        let dest = wad_root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log_warn!("[LOADSCREEN] mkdir failed for {}: {e}", parent.display());
                continue;
            }
        }
        if let Err(e) = std::fs::write(&dest, &tex) {
            log_warn!("[LOADSCREEN] write failed for {}: {e}", dest.display());
            continue;
        }
        wrote += 1;
        log_info!("[LOADSCREEN] baked name card '{skin_name}' ({} bytes) -> {rel}", tex.len());
    }
    if wrote == 0 {
        return None;
    }
    Some(MOD_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Manual proof (needs network): fetches Aatrox's base loadscreen + Riot's
    // real Beaufort font from CommunityDragon, bakes a long skin name, encodes
    // .tex, and checks the header. Also confirms the OTF/CFF font actually
    // rasterizes glyphs (non-empty outline) under ab_glyph.
    // Run with: cargo test --lib loadscreen_proof -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn loadscreen_proof() {
        let http = reqwest::Client::new();
        let (url, _) = loadscreen_paths("aatrox", 0);
        let png = http.get(&url).send().await.unwrap().bytes().await.unwrap().to_vec();
        let fb = http.get(RIOT_FONT_URL).send().await.unwrap().bytes().await.unwrap().to_vec();
        let font = FontVec::try_from_vec(fb).expect("Riot Beaufort OTF parses");
        // The name must actually rasterize under ab_glyph (CFF outlines present).
        let g = font.glyph_id('A').with_scale(48.0);
        assert!(font.outline_glyph(g).map(|o| o.px_bounds().width() > 0.0).unwrap_or(false), "Beaufort 'A' rasterizes");
        let mut img = image::load_from_memory(&png).unwrap().to_rgba8();
        img = image::imageops::resize(&img, CARD_W, CARD_H, image::imageops::FilterType::Lanczos3);
        draw_skin_name(&mut img, &font, "Battle Queen Katarina");
        img.save(std::env::temp_dir().join("chud_loadscreen_proof.png")).unwrap();
        let tex = encode_tex_bc1(&img).expect("encode");
        assert_eq!(&tex[0..4], b"TEX\0", "TEX magic");
        assert_eq!(u16::from_le_bytes([tex[4], tex[5]]), CARD_W as u16, "width");
        assert_eq!(u16::from_le_bytes([tex[6], tex[7]]), CARD_H as u16, "height");
        assert_eq!(tex[9], 0x0A, "BC1 format byte");
        // BC1 = 8 bytes / 4x4 block → (308/4)*(560/4)*8 = 86,240 payload bytes.
        assert_eq!(tex.len(), 12 + (CARD_W / 4 * CARD_H / 4 * 8) as usize, "payload size");
        let out = std::env::temp_dir().join("chud_loadscreen_proof.tex");
        std::fs::write(&out, &tex).unwrap();
        eprintln!("wrote {} ({} bytes)", out.display(), tex.len());
    }
}
