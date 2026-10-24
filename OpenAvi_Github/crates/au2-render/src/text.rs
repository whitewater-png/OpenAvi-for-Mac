//! テキストのラスタライズ（CPU）。結果は RGBA 画像にして、通常のテクスチャスプライトとして描きます。
//!
//! AviUtl のテキストオブジェクトの装飾（影・縁取り）・文字間隔・太字/斜体・等幅・揃え・表示速度・フォント名解決に対応。
//! 実 AviUtl の見た目が不明な箇所は近似（docs/RENDER_DESIGN.md の未決事項）。

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use au2_media::RgbaFrame;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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

/// 文字装飾（AviUtl の「文字装飾」`type` 0..=4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoration {
    /// 標準文字
    Plain,
    /// 影付き文字
    Shadow,
    /// 影付き文字(薄)
    ShadowLight,
    /// 縁取り文字
    Outline,
    /// 縁取り文字(細)
    OutlineThin,
}

impl Decoration {
    pub fn from_type(n: u32) -> Self {
        match n {
            1 => Self::Shadow,
            2 => Self::ShadowLight,
            3 => Self::Outline,
            4 => Self::OutlineThin,
            _ => Self::Plain,
        }
    }
}

/// ラスタライズの指定。長さはすべて出力ピクセル単位（拡大率反映済み）。
#[derive(Debug, Clone)]
pub struct TextStyle {
    pub px: f32,
    pub color: [u8; 3],
    /// 影・縁取りの色（`color2`）
    pub color2: [u8; 3],
    pub bold: bool,
    pub italic: bool,
    pub decoration: Decoration,
    /// 文字間隔（字ごとに足す幅）
    pub spacing_x: f32,
    /// 行間（行送りに足す幅）
    pub spacing_y: f32,
    pub monospace: bool,
    /// AviUtl の揃え 0..=8: 0/1/2 = 左/中/右（上）, 3/4/5 = 左/中/右（中）, 6/7/8 = 左/中/右（下）
    pub align: u32,
    /// 表示する文字数（改行を除く先頭から）。`None` = すべて
    pub visible_chars: Option<usize>,
}

impl TextStyle {
    pub fn new(px: f32, color: [u8; 3]) -> Self {
        Self {
            px,
            color,
            color2: [0, 0, 0],
            bold: false,
            italic: false,
            decoration: Decoration::Plain,
            spacing_x: 0.0,
            spacing_y: 0.0,
            monospace: false,
            align: 4,
            visible_chars: None,
        }
    }
}

/// ラスタライズ結果。`anchor` は画像の中心から見た「オブジェクトの位置に合わせる点」（y 下向き、ピクセル）。
#[derive(Debug, Clone)]
pub struct TextBitmap {
    pub frame: RgbaFrame,
    pub anchor: [f32; 2],
}

/// 改行を除いた文字数（表示速度の上限判定に使う）。
pub fn char_count(text: &str) -> usize {
    text.lines().map(|l| l.chars().count()).sum()
}

/// 表示速度（文字/秒）と相対フレームから、表示する文字数を求める。速度 0 以下は「すべて表示」（`None`）。
pub fn visible_chars_at(speed: f64, rel: u32, fps: f64) -> Option<usize> {
    if speed <= 0.0 {
        return None;
    }
    Some((speed * rel as f64 / fps.max(1.0)).floor() as usize)
}

/// 斜体のせん断（上端を右へ倒す）の傾き（約 12 度）。
const ITALIC_SLOPE: f32 = 0.21;

struct PlacedGlyph {
    ch: char,
    x: f32,
    line: usize,
}

fn decoration_radius(deco: Decoration, px: f32) -> (i32, i32) {
    // (縁取り半径, 影オフセット)。近似: 縁取りは字の高さの約 1/20（細は 1/40）、影は 1/20。
    match deco {
        Decoration::Plain => (0, 0),
        Decoration::Shadow | Decoration::ShadowLight => (0, (px / 20.0).round().max(1.0) as i32),
        Decoration::Outline => ((px / 20.0).round().max(1.0) as i32, 0),
        Decoration::OutlineThin => ((px / 40.0).round().max(1.0) as i32, 0),
    }
}

/// `text` を描き、ストレートアルファの画像と位置合わせ点を返します。描くものが無ければ（空文字・空白のみ・表示 0 文字）`None`。
pub fn rasterize(font: &FontVec, text: &str, style: &TextStyle) -> Option<TextBitmap> {
    let px = style.px.clamp(1.0, MAX_SIDE as f32);
    let scaled = font.as_scaled(PxScale::from(px));
    let line_height = scaled.height() + scaled.line_gap();
    let pitch = (line_height + style.spacing_y).max(1.0);
    let ascent = scaled.ascent();
    let bold_extra = if style.bold { (px * 0.04).max(1.0) } else { 0.0 };

    // 配置（行ごとの幅も求める）
    let mut placed: Vec<PlacedGlyph> = Vec::new();
    let mut line_widths: Vec<f32> = Vec::new();
    for (row, line) in text.lines().enumerate() {
        let mut x = 0.0f32;
        let mut prev = None;
        let mut n = 0;
        for ch in line.chars() {
            let id = font.glyph_id(ch);
            let adv = scaled.h_advance(id);
            if style.monospace {
                // 等幅: 全角は px、半角（ASCII）は px/2 の升に入れて中央寄せ
                let cell = if ch.is_ascii() { px * 0.5 } else { px };
                placed.push(PlacedGlyph { ch, x: x + (cell - adv) * 0.5, line: row });
                x += cell;
            } else {
                if let Some(p) = prev {
                    x += scaled.kern(p, id);
                }
                placed.push(PlacedGlyph { ch, x, line: row });
                x += adv + bold_extra;
            }
            prev = Some(id);
            n += 1;
            x += style.spacing_x;
        }
        // 末尾の字間は幅に含めない
        if n > 0 {
            x -= style.spacing_x;
        }
        line_widths.push(x.max(0.0));
    }
    let lines = line_widths.len();
    if lines == 0 {
        return None;
    }
    let block_w = line_widths.iter().cloned().fold(0.0f32, f32::max);
    let block_h = (lines - 1) as f32 * pitch + line_height;

    let align = style.align.min(8);
    let (hf, vf) = ((align % 3) as f32 * 0.5, (align / 3) as f32 * 0.5);

    let (outline_r, shadow_d) = decoration_radius(style.decoration, px);
    let margin = (outline_r + shadow_d + 1) as f32;
    let pad_l = if style.italic { ((-scaled.descent()).max(0.0) * ITALIC_SLOPE).ceil() } else { 0.0 };
    let pad_r = (if style.italic { (ascent * ITALIC_SLOPE).ceil() } else { 0.0 }) + bold_extra.ceil();
    let (ox, oy) = (margin + pad_l, margin);
    let w = ((block_w + 2.0 * margin + pad_l + pad_r).ceil() as u32).clamp(1, MAX_SIDE);
    let h = ((block_h + 2.0 * margin).ceil() as u32).clamp(1, MAX_SIDE);

    // 文字本体のカバレッジ
    let mut fill = vec![0.0f32; (w * h) as usize];
    let mut drawn = false;
    let passes = if style.bold { bold_extra.ceil() as i32 + 1 } else { 1 };
    for (idx, g) in placed.iter().enumerate() {
        let visible = style.visible_chars.is_none_or(|n| idx < n);
        if !visible {
            continue;
        }
        let baseline = oy + ascent + g.line as f32 * pitch;
        // 行の揃え（ブロック内での左・中・右）
        let lx = ox + (block_w - line_widths[g.line]) * hf + g.x;
        for pass in 0..passes {
            let off = if passes > 1 { bold_extra * pass as f32 / (passes - 1) as f32 } else { 0.0 };
            let glyph = g.ch;
            let id = font.glyph_id(glyph).with_scale_and_position(scaled.scale(), point(lx + off, baseline));
            let Some(outlined) = font.outline_glyph(id) else { continue };
            let b = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                if cov <= 0.0 {
                    return;
                }
                let y = b.min.y as i32 + gy as i32;
                if y < 0 || y as u32 >= h {
                    return;
                }
                let mut xf = b.min.x + gx as f32;
                if style.italic {
                    xf += (baseline - (y as f32 + 0.5)) * ITALIC_SLOPE;
                }
                let x0 = xf.floor();
                let frac = xf - x0;
                for (dx, wgt) in [(0i32, 1.0 - frac), (1, frac)] {
                    let x = x0 as i32 + dx;
                    if wgt > 0.0 && x >= 0 && (x as u32) < w {
                        let c = &mut fill[(y as u32 * w + x as u32) as usize];
                        *c = c.max(cov * wgt);
                    }
                }
                drawn = true;
            });
        }
    }
    if !drawn {
        return None;
    }

    // 下の層から重ねる: 影 → 縁取り → 本体
    let mut out = vec![0.0f32; (w * h * 4) as usize]; // 非プリマルチプライ合成用に premultiplied で持つ
    let mut over = |mask: &[f32], dx: i32, dy: i32, rgb: [u8; 3], scale: f32| {
        for y in 0..h as i32 {
            let sy = y - dy;
            if sy < 0 || sy >= h as i32 {
                continue;
            }
            for x in 0..w as i32 {
                let sx = x - dx;
                if sx < 0 || sx >= w as i32 {
                    continue;
                }
                let a = mask[(sy as u32 * w + sx as u32) as usize] * scale;
                if a <= 0.0 {
                    continue;
                }
                let o = ((y as u32 * w + x as u32) * 4) as usize;
                for c in 0..3 {
                    out[o + c] = rgb[c] as f32 * a + out[o + c] * (1.0 - a);
                }
                out[o + 3] = a + out[o + 3] * (1.0 - a);
            }
        }
    };
    match style.decoration {
        Decoration::Plain => {}
        Decoration::Shadow => over(&fill, shadow_d, shadow_d, style.color2, 1.0),
        Decoration::ShadowLight => over(&fill, shadow_d, shadow_d, style.color2, 0.5),
        Decoration::Outline | Decoration::OutlineThin => {
            let dil = dilate(&fill, w as usize, h as usize, outline_r);
            over(&dil, 0, 0, style.color2, 1.0);
        }
    }
    over(&fill, 0, 0, style.color, 1.0);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for (o, p) in out.chunks_exact(4).zip(pixels.chunks_exact_mut(4)) {
        let a = o[3];
        if a > 0.0 {
            for c in 0..3 {
                p[c] = (o[c] / a).round().clamp(0.0, 255.0) as u8;
            }
            p[3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        } else {
            p[..3].copy_from_slice(&style.color);
        }
    }

    // 位置合わせ点: ブロックの左/中/右 × 上/中/下（画像中心基準）
    let anchor = [ox + block_w * hf - w as f32 / 2.0, oy + block_h * vf - h as f32 / 2.0];
    Some(TextBitmap { frame: RgbaFrame { width: w, height: h, pixels }, anchor })
}

/// 半径 `r`（整数ピクセル）の円盤でカバレッジを膨らませる（最大値フィルタ）。
fn dilate(src: &[f32], w: usize, h: usize, r: i32) -> Vec<f32> {
    if r <= 0 {
        return src.to_vec();
    }
    let mut out = vec![0.0f32; w * h];
    // 行方向の最大値 hk（窓の半幅 k）を k を増やしながら更新し、半幅 k に当たる dy の行へ反映する
    let mut hk = src.to_vec();
    let half_width = |dy: i32| (((r * r - dy * dy) as f32).sqrt().floor()) as i32;
    for k in 0..=r {
        if k > 0 {
            for y in 0..h {
                let row = y * w;
                for x in 0..w {
                    let mut m = hk[row + x];
                    if x as i32 - k >= 0 {
                        m = m.max(src[row + x - k as usize]);
                    }
                    if x + (k as usize) < w {
                        m = m.max(src[row + x + k as usize]);
                    }
                    hk[row + x] = m;
                }
            }
        }
        for dy in -r..=r {
            if half_width(dy) != k {
                continue;
            }
            for y in 0..h as i32 {
                let sy = y + dy;
                if sy < 0 || sy >= h as i32 {
                    continue;
                }
                let (d, s) = (y as usize * w, sy as usize * w);
                for x in 0..w {
                    out[d + x] = out[d + x].max(hk[s + x]);
                }
            }
        }
    }
    out
}

// ---- フォント名の解決 ----

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '-' | '_' | '(' | ')' | '（' | '）' | '　'))
        .map(|c| match c {
            // 全角英数 → 半角
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            c => c,
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// よく使われる日本語・Windows のフォント名 → 探すファイル名（正規化済みの部分文字列、優先順）。
fn alias_needles(norm: &str) -> &'static [&'static str] {
    match norm {
        "msuigothic" | "msgothic" | "mspgothic" | "msゴシック" | "mspゴシック" => {
            &["msgothic", "ヒラギノ角ゴシックw3", "notosanscjk", "meiryo"]
        }
        "msmincho" | "mspmincho" | "ms明朝" | "msp明朝" => &["msmincho", "ヒラギノ明朝", "notoserifcjk", "yumin"],
        "meiryo" | "meiryoui" | "メイリオ" => &["meiryo", "ヒラギノ角ゴシックw3", "notosanscjk", "yugoth"],
        "yugothic" | "yugothicui" | "游ゴシック" => &["yugoth", "ヒラギノ角ゴシックw3", "notosanscjk", "meiryo"],
        "yumincho" | "游明朝" => &["yumin", "ヒラギノ明朝", "notoserifcjk", "msmincho"],
        n if n.starts_with("ヒラギノ角ゴ") || n.starts_with("hiraginokaku") || n == "hiraginosans" => {
            &["ヒラギノ角ゴシックw3", "hiraginosans", "notosanscjk"]
        }
        n if n.starts_with("ヒラギノ明朝") || n.starts_with("hiraginomin") => &["ヒラギノ明朝", "notoserifcjk"],
        n if n.starts_with("ヒラギノ丸ゴ") || n.starts_with("hiraginomaru") => &["ヒラギノ丸ゴ", "ヒラギノ角ゴシックw3"],
        _ => &[],
    }
}

fn font_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("/Library/Fonts"),
        PathBuf::from(r"C:\Windows\Fonts"),
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("Library/Fonts"));
        dirs.push(home.join(".local/share/fonts"));
        dirs.push(home.join(".fonts"));
    }
    dirs
}

fn scan(dir: &Path, depth: u32, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            if depth > 0 {
                scan(&path, depth - 1, out);
            }
        } else if path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| matches!(x.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc"))
        {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((normalize(stem), path));
            }
        }
    }
}

fn installed_fonts() -> &'static Vec<(String, PathBuf)> {
    static FONTS: OnceLock<Vec<(String, PathBuf)>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut v = Vec::new();
        for d in font_dirs() {
            scan(&d, 3, &mut v);
        }
        v.sort();
        v
    })
}

/// 正規化済みの名前 `needle` に合うファイルを探す。完全一致を優先し、次に「含む」のうち最も短い名前。
fn find_by_needle<'a>(fonts: &'a [(String, PathBuf)], needle: &str) -> Option<&'a PathBuf> {
    if needle.is_empty() {
        return None;
    }
    if let Some((_, p)) = fonts.iter().find(|(s, _)| s == needle) {
        return Some(p);
    }
    fonts.iter().filter(|(s, _)| s.contains(needle)).min_by_key(|(s, _)| s.chars().count()).map(|(_, p)| p)
}

/// `fonts` の中から `name` に合うファイルを選ぶ（大文字小文字・空白・全角半角を無視。別名表を使う）。
pub fn match_font_path<'a>(fonts: &'a [(String, PathBuf)], name: &str) -> Option<&'a PathBuf> {
    let norm = normalize(name);
    if norm.is_empty() {
        return None;
    }
    if let Some(p) = find_by_needle(fonts, &norm).filter(|_| norm.chars().count() >= 3) {
        return Some(p);
    }
    alias_needles(&norm).iter().find_map(|n| find_by_needle(fonts, &normalize(n)))
}

type FontCache = Mutex<HashMap<String, Option<Arc<FontVec>>>>;

/// フォント名（`font` パラメータ）から、インストール済みのフォントを読み込む。見つからなければ `None`（呼び出し側が既定フォントにフォールバック）。
/// 結果（見つからなかった場合も）はプロセス内でキャッシュされる。
pub fn resolve_font(name: &str) -> Option<Arc<FontVec>> {
    static CACHE: OnceLock<FontCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = normalize(name);
    if let Some(hit) = cache.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let font = match_font_path(installed_fonts(), name)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| load_font(bytes, 0))
        .map(Arc::new);
    if let Ok(mut c) = cache.lock() {
        c.insert(key, font.clone());
    }
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_font() -> Option<FontVec> {
        SYSTEM_FONT_CANDIDATES.iter().find_map(|p| std::fs::read(p).ok().and_then(|b| load_font(b, 0))).or_else(|| {
            installed_fonts().iter().find_map(|(_, p)| std::fs::read(p).ok().and_then(|b| load_font(b, 0)))
        })
    }

    macro_rules! font_or_skip {
        () => {
            match test_font() {
                Some(f) => f,
                None => {
                    eprintln!("フォントが無いためスキップ");
                    return;
                }
            }
        };
    }

    fn ink(b: &TextBitmap) -> usize {
        b.frame.pixels.chunks(4).filter(|p| p[3] > 128).count()
    }

    fn bbox(b: &TextBitmap) -> (u32, u32, u32, u32) {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..b.frame.height {
            for x in 0..b.frame.width {
                if b.frame.pixels[((y * b.frame.width + x) * 4 + 3) as usize] > 128 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x0, y0, x1, y1)
    }

    fn style() -> TextStyle {
        TextStyle::new(40.0, [255, 255, 255])
    }

    #[test]
    fn spacing_grows_bitmap() {
        let f = font_or_skip!();
        let base = rasterize(&f, "ABC", &style()).unwrap();
        let wide = rasterize(&f, "ABC", &TextStyle { spacing_x: 10.0, ..style() }).unwrap();
        // 3 文字 → 字間 2 か所 × 10px
        assert!(wide.frame.width >= base.frame.width + 19, "{} vs {}", wide.frame.width, base.frame.width);
        let tall = rasterize(&f, "A\nB", &style()).unwrap();
        let tall2 = rasterize(&f, "A\nB", &TextStyle { spacing_y: 12.0, ..style() }).unwrap();
        assert!(tall2.frame.height >= tall.frame.height + 11);
        // 縦の間隔は幅を変えない
        assert_eq!(tall2.frame.width, tall.frame.width);
    }

    #[test]
    fn outline_and_shadow_use_color2() {
        let f = font_or_skip!();
        let red = [255, 0, 0];
        let plain = rasterize(&f, "O", &style()).unwrap();
        for deco in [Decoration::Shadow, Decoration::ShadowLight, Decoration::Outline, Decoration::OutlineThin] {
            let b = rasterize(&f, "O", &TextStyle { color2: red, decoration: deco, ..style() }).unwrap();
            let reds = b.frame.pixels.chunks(4).filter(|p| p[3] > 100 && p[0] > 200 && p[1] < 60 && p[2] < 60).count();
            assert!(reds > 20, "{deco:?}: color2 の画素が無い ({reds})");
            let whites = b.frame.pixels.chunks(4).filter(|p| p[3] > 250 && p[0] > 250 && p[1] > 250).count();
            assert!(whites > 20, "{deco:?}: 本体が残っていない");
            assert!(b.frame.width > plain.frame.width, "{deco:?}: 余白が増えない");
        }
        // 標準文字には color2 は出ない
        let p = rasterize(&f, "O", &TextStyle { color2: red, ..style() }).unwrap();
        assert!(p.frame.pixels.chunks(4).all(|q| q[3] == 0 || q[1] > 200));
        // 縁取り(細) は縁取りより細い（color2 の画素が少ない）
        let count_red = |d| {
            let b = rasterize(&f, "O", &TextStyle { color2: red, decoration: d, ..TextStyle::new(80.0, [255, 255, 255]) }).unwrap();
            b.frame.pixels.chunks(4).filter(|p| p[3] > 100 && p[0] > 200 && p[1] < 60).count()
        };
        assert!(count_red(Decoration::OutlineThin) < count_red(Decoration::Outline));
        // 薄い影は濃い影より透ける
        let max_a_red = |d| {
            let b = rasterize(&f, "O", &TextStyle { color2: red, decoration: d, ..style() }).unwrap();
            b.frame.pixels.chunks(4).filter(|p| p[1] < 60 && p[0] > 200).map(|p| p[3]).max().unwrap()
        };
        assert!(max_a_red(Decoration::ShadowLight) < max_a_red(Decoration::Shadow) - 50);
    }

    #[test]
    fn shadow_is_offset_down_right() {
        let f = font_or_skip!();
        let b = rasterize(&f, "I", &TextStyle { color2: [255, 0, 0], decoration: Decoration::Shadow, ..style() }).unwrap();
        let w = b.frame.width;
        let cx = |pred: &dyn Fn(&[u8]) -> bool| {
            let (mut sum, mut n) = (0.0, 0.0);
            for (i, p) in b.frame.pixels.chunks(4).enumerate() {
                if pred(p) {
                    sum += (i as u32 % w) as f32;
                    n += 1.0;
                }
            }
            sum / n
        };
        let red = cx(&|p| p[3] > 100 && p[0] > 200 && p[1] < 60);
        let white = cx(&|p| p[3] > 250 && p[1] > 250);
        assert!(red > white, "影が右にずれていない: {red} {white}");
    }

    #[test]
    fn bold_adds_ink_and_italic_shears() {
        let f = font_or_skip!();
        let n = rasterize(&f, "I", &style()).unwrap();
        let b = rasterize(&f, "I", &TextStyle { bold: true, ..style() }).unwrap();
        assert!(ink(&b) > ink(&n), "太字で画素が増えない");
        // 斜体: 上端の重心が下端の重心より右にある
        let it = rasterize(&f, "I", &TextStyle { italic: true, ..style() }).unwrap();
        let (_, y0, _, y1) = bbox(&it);
        let center_x = |it: &TextBitmap, ya: u32, yb: u32| {
            let (mut s, mut c) = (0.0, 0.0);
            for y in ya..=yb {
                for x in 0..it.frame.width {
                    let a = it.frame.pixels[((y * it.frame.width + x) * 4 + 3) as usize] as f32;
                    s += a * x as f32;
                    c += a;
                }
            }
            s / c
        };
        let q = (y1 - y0) / 4;
        let top = center_x(&it, y0, y0 + q);
        let bottom = center_x(&it, y1 - q, y1);
        assert!(top > bottom + 3.0, "斜体で傾いていない: {top} {bottom}");
        let top_n = center_x(&n, bbox(&n).1, bbox(&n).1 + q);
        let bottom_n = center_x(&n, bbox(&n).3 - q, bbox(&n).3);
        assert!((top_n - bottom_n).abs() < 1.0, "標準が傾いている");
    }

    #[test]
    fn align_moves_anchor() {
        let f = font_or_skip!();
        let at = |a| rasterize(&f, "ABCD\nAB", &TextStyle { align: a, ..style() }).unwrap();
        let (l, c, r) = (at(3), at(4), at(5));
        let w = c.frame.width as f32;
        assert!(l.anchor[0] < c.anchor[0] && c.anchor[0] < r.anchor[0]);
        // 左=ブロック左端、右=ブロック右端
        assert!(r.anchor[0] - l.anchor[0] > w * 0.5, "左右の位置合わせ点がブロック幅ほど離れていない");
        assert!((c.anchor[0] - (l.anchor[0] + r.anchor[0]) / 2.0).abs() < 0.01);
        let (t, m, b) = (at(1), at(4), at(7));
        assert!(t.anchor[1] < m.anchor[1] && m.anchor[1] < b.anchor[1]);
        // 上下は対称（中央が画像の中心）
        assert!(m.anchor[1].abs() < 0.6 && c.anchor[0].abs() < 0.6);
        assert!((t.anchor[1] + b.anchor[1]).abs() < 0.6);
        // 行の揃え: 右揃えでは短い 2 行目が右に寄る（下半分の重心が左揃えより右）
        let lower_x = |bm: &TextBitmap| {
            let (mut s, mut n) = (0.0, 0.0);
            for y in bm.frame.height / 2 + 4..bm.frame.height {
                for x in 0..bm.frame.width {
                    if bm.frame.pixels[((y * bm.frame.width + x) * 4 + 3) as usize] > 128 {
                        s += x as f32;
                        n += 1.0;
                    }
                }
            }
            s / n
        };
        assert!(lower_x(&r) > lower_x(&l) + 10.0);
    }

    #[test]
    fn typewriter_reveals_n_chars() {
        let f = font_or_skip!();
        assert!(rasterize(&f, "ABC", &TextStyle { visible_chars: Some(0), ..style() }).is_none());
        let full = rasterize(&f, "ABC", &style()).unwrap();
        let mut last = 0;
        for n in 1..=3 {
            let b = rasterize(&f, "ABC", &TextStyle { visible_chars: Some(n), ..style() }).unwrap();
            // 配置はそのまま（画像の大きさ・位置合わせ点は不変）
            assert_eq!((b.frame.width, b.frame.height), (full.frame.width, full.frame.height));
            assert_eq!(b.anchor, full.anchor);
            let i = ink(&b);
            assert!(i > last, "{n} 文字目で増えない");
            last = i;
        }
        assert_eq!(last, ink(&full));
        // 1 文字だけなら、右端の列には何も無い
        let one = rasterize(&f, "ABC", &TextStyle { visible_chars: Some(1), ..style() }).unwrap();
        assert!(bbox(&one).2 < bbox(&full).2 - 10);
        assert_eq!(visible_chars_at(0.0, 5, 30.0), None);
        assert_eq!(visible_chars_at(10.0, 0, 30.0), Some(0));
        assert_eq!(visible_chars_at(10.0, 30, 30.0), Some(10));
        assert_eq!(visible_chars_at(10.0, 44, 30.0), Some(14));
        assert_eq!(char_count("ab\ncd"), 4);
    }

    #[test]
    fn monospace_uses_equal_cells() {
        let f = font_or_skip!();
        let prop = rasterize(&f, "iiii", &style()).unwrap();
        let mono = rasterize(&f, "iiii", &TextStyle { monospace: true, ..style() }).unwrap();
        // 半角 4 文字 = 4 × px/2 = 80
        assert!(mono.frame.width >= 80 && mono.frame.width <= 90, "{}", mono.frame.width);
        assert_ne!(prop.frame.width, mono.frame.width);
        let full = rasterize(&f, "あい", &TextStyle { monospace: true, ..style() }).unwrap();
        assert!(full.frame.width >= 80 && full.frame.width <= 90, "{}", full.frame.width);
    }

    #[test]
    fn size_cap_and_empty() {
        let f = font_or_skip!();
        assert!(rasterize(&f, "", &style()).is_none());
        assert!(rasterize(&f, "  ", &style()).is_none());
        let big = rasterize(&f, &"W".repeat(200), &TextStyle::new(400.0, [255; 3])).unwrap();
        assert!(big.frame.width <= MAX_SIDE && big.frame.height <= MAX_SIDE);
    }

    #[test]
    fn font_names_resolve() {
        let fonts: Vec<(String, PathBuf)> = ["msgothic", "meiryo", "meiryob", "ヒラギノ角ゴシックw3", "arial", "yugothm"]
            .iter()
            .map(|s| (s.to_string(), PathBuf::from(format!("/fonts/{s}.ttc"))))
            .collect();
        let find = |n: &str| match_font_path(&fonts, n).map(|p| p.file_stem().unwrap().to_str().unwrap().to_string());
        assert_eq!(find("Arial").as_deref(), Some("arial"));
        assert_eq!(find("ARIAL").as_deref(), Some("arial"));
        assert_eq!(find("MS UI Gothic").as_deref(), Some("msgothic"));
        assert_eq!(find("ＭＳ ゴシック").as_deref(), Some("msgothic"));
        assert_eq!(find("Meiryo").as_deref(), Some("meiryo"));
        assert_eq!(find("メイリオ").as_deref(), Some("meiryo"));
        assert_eq!(find("Yu Gothic").as_deref(), Some("yugothm"));
        assert_eq!(find("ヒラギノ角ゴ ProN").as_deref(), Some("ヒラギノ角ゴシックw3"));
        assert_eq!(find("No Such Font"), None);
        assert_eq!(find(""), None);
        // 別名表が無い MS 明朝は、無ければ None（呼び出し側が既定フォントへ）
        assert_eq!(find("MS Mincho"), None);
    }

    #[test]
    fn resolve_font_falls_back_to_none_for_unknown() {
        assert!(resolve_font("Definitely Not A Font 12345").is_none());
    }
}
