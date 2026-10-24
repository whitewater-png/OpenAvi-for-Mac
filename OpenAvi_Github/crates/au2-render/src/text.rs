//! テキストのラスタライズ（CPU）。結果は RGBA 画像にして、通常のテクスチャスプライトとして描きます。
//!
//! 現状は単色・左揃え・複数行のみ。影・縁取り・文字間隔・太字/斜体などは未対応（docs/RENDER_DESIGN.md）。

use ab_glyph::{point, Font, FontVec, OutlinedGlyph, PxScale, ScaleFont};
use au2_media::RgbaFrame;

/// 日本語を含むフォントの探索候補。
pub const SYSTEM_FONT_CANDIDATES: &[&str] = &[
    r"C:\Windows\Fonts\YuGothM.ttc",
    r"C:\Windows\Fonts\meiryo.ttc",
    r"C:\Windows\Fonts\msgothic.ttc",
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
];

/// 1 辺の上限（巨大な拡大率でメモリを使い切らないため）。
const MAX_SIDE: u32 = 4096;

pub fn load_font(bytes: Vec<u8>, index: u32) -> Option<FontVec> {
    FontVec::try_from_vec_and_index(bytes, index).ok()
}

/// `text` を `px` ピクセルの高さで描き、色 `rgb` のストレートアルファ画像を返します。
/// 描くものが無ければ（空文字・空白のみ）`None`。
pub fn rasterize(font: &FontVec, text: &str, px: f32, rgb: [u8; 3]) -> Option<RgbaFrame> {
    let scaled = font.as_scaled(PxScale::from(px.clamp(1.0, MAX_SIDE as f32)));
    let line_height = scaled.height() + scaled.line_gap();

    let mut glyphs: Vec<OutlinedGlyph> = Vec::new();
    let mut width = 0.0f32;
    let mut lines = 0u32;
    for (row, line) in text.lines().enumerate() {
        lines += 1;
        let baseline = scaled.ascent() + row as f32 * line_height;
        let mut x = 0.0f32;
        let mut prev = None;
        for ch in line.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scaled.scale(), point(x, baseline));
            x += scaled.h_advance(id);
            prev = Some(id);
            if let Some(outlined) = font.outline_glyph(glyph) {
                glyphs.push(outlined);
            }
        }
        width = width.max(x);
    }
    if glyphs.is_empty() || lines == 0 {
        return None;
    }
    let w = (width.ceil() as u32 + 2).min(MAX_SIDE);
    let h = ((lines as f32 * line_height).ceil() as u32 + 2).min(MAX_SIDE);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for p in pixels.chunks_exact_mut(4) {
        p[..3].copy_from_slice(&rgb);
    }
    for g in &glyphs {
        let b = g.px_bounds();
        g.draw(|gx, gy, cov| {
            let (x, y) = (b.min.x as i32 + gx as i32 + 1, b.min.y as i32 + gy as i32 + 1);
            if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                let a = &mut pixels[((y as u32 * w + x as u32) * 4 + 3) as usize];
                *a = (*a as f32 + cov * 255.0).min(255.0) as u8;
            }
        });
    }
    Some(RgbaFrame { width: w, height: h, pixels })
}
