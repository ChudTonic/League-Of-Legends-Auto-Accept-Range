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

/// Windows-bundled fonts we can load at runtime (the user's own font — no
/// redistribution), heaviest-first for a bold loadscreen label.
const FONT_CANDIDATES: [&str; 3] = ["segoeuib.ttf", "arialbd.ttf", "segoeui.ttf"];

fn load_font() -> Option<FontVec> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:/Windows".into());
    for name in FONT_CANDIDATES {
        let p = PathBuf::from(&root).join("Fonts").join(name);
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(font) = FontVec::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }
    None
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

/// Draw `name` across the bottom of the card: a dark gradient scrim so text is
/// legible over any splash, then the name centered with a hard shadow.
fn draw_skin_name(img: &mut RgbaImage, font: &FontVec, name: &str) {
    // Bottom scrim — fade a translucent black band up from the bottom edge.
    let band_h = (CARD_H as f32 * 0.22) as u32;
    for y in (CARD_H - band_h)..CARD_H {
        let t = (y - (CARD_H - band_h)) as f32 / band_h as f32; // 0 at top of band → 1 at bottom
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
    let y = CARD_H as f32 - size - 14.0;
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

    let png = match crate::net::get_bytes_checked(http, &url, allowed, 8 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            log_warn!("[LOADSCREEN] card source unavailable for {skin_name} ({url}): {e}");
            return None;
        }
    };
    let font = load_font()?;
    let mut img = image::load_from_memory(&png).ok()?.to_rgba8();
    if img.width() != CARD_W || img.height() != CARD_H {
        img = image::imageops::resize(&img, CARD_W, CARD_H, image::imageops::FilterType::Lanczos3);
    }
    draw_skin_name(&mut img, &font, skin_name);
    let tex = encode_tex_bc1(&img)?;

    // Write into <injection mods>/<MOD_NAME>/WAD/<Alias>.wad.client/<inner_tex>.
    let dest = crate::skins::paths::injection_mods_dir()
        .join(MOD_NAME)
        .join("WAD")
        .join(format!("{champ_alias}.wad.client"))
        .join(inner_tex.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::write(&dest, &tex).ok()?;
    log_info!("[LOADSCREEN] baked name card '{skin_name}' -> {}", dest.display());
    Some(MOD_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Manual proof (needs network + a Windows font): fetches Aatrox's base
    // loadscreen, bakes a long skin name, encodes .tex, and checks the header.
    // Run with: cargo test --lib loadscreen_proof -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn loadscreen_proof() {
        let http = reqwest::Client::new();
        let (url, _) = loadscreen_paths("aatrox", 0);
        let png = http.get(&url).send().await.unwrap().bytes().await.unwrap().to_vec();
        let font = load_font().expect("a Windows font");
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
