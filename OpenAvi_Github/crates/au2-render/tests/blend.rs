//! 合成モード（標準描画の `blend`）と「上のオブジェクトでクリッピング」の GPU テスト。GPU が無ければスキップします。
//!
//! 期待値は docs/RENDER_DESIGN.md「合成モード・クリッピング」の式を手計算（倍精度）して丸めたもの。
//! 下地 D = (64, 128, 192)、上 S = (200, 100, 30)。

use std::path::PathBuf;

use au2_core::{EffectKind, ParamValue, Scene, SceneSettings, TimelineObject, Track};
use au2_render::{BlendMode, Gpu, MediaSource, NoMedia, Renderer, RgbaFrame};

const D: &str = "4080c0";
const S: &str = "c8641e";
const D_RGB: [u8; 4] = [64, 128, 192, 255];

/// (合成モード, 不透明な S を重ねた結果, 透明度 50 の S を重ねた結果)
const EXPECTED: [(u32, [u8; 3], [u8; 3]); 13] = [
    (0, [200, 100, 30], [132, 114, 111]),  // 通常
    (1, [255, 228, 222], [160, 178, 207]), // 加算: min(S + D, 1)
    (2, [0, 28, 162], [32, 78, 177]),      // 減算: max(D - S, 0)
    (3, [50, 50, 23], [57, 89, 107]),      // 乗算: S * D
    (4, [214, 178, 199], [139, 153, 196]), // スクリーン: 1 - (1 - S)(1 - D)
    (5, [100, 101, 144], [82, 114, 168]),  // オーバーレイ: D < 0.5 ? 2SD : 1 - 2(1 - S)(1 - D)（D の G = 128/255 は明側）
    (6, [200, 128, 192], [132, 128, 192]), // 比較(明): max
    (7, [64, 100, 30], [64, 114, 111]),    // 比較(暗): min
    (8, [70, 134, 198], [67, 131, 195]),   // 輝度: Y = S、CbCr = D（BT.601）
    (9, [194, 94, 24], [129, 111, 108]),   // 色差: Y = D、CbCr = S（BT.601）
    (10, [9, 0, 0], [36, 64, 96]),         // 陰影: max(S + D - 1, 0)
    (11, [209, 73, 0], [136, 100, 96]),    // 明暗: clamp(D + 2S - 1)
    (12, [136, 28, 162], [100, 78, 177]),  // 差分: |S - D|
];

fn renderer() -> Option<Renderer> {
    match Gpu::headless() {
        Ok(gpu) => Some(Renderer::new(gpu)),
        Err(e) => {
            eprintln!("GPU が無いためスキップします: {e}");
            None
        }
    }
}

fn scene(width: u32, height: u32) -> Scene {
    Scene::new("test", SceneSettings { width, height, ..Default::default() })
}

fn px(f: &RgbaFrame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    [f.pixels[i], f.pixels[i + 1], f.pixels[i + 2], f.pixels[i + 3]]
}

fn near(actual: [u8; 4], expected: [u8; 4], tol: u8) -> bool {
    actual.iter().zip(expected.iter()).all(|(a, e)| a.abs_diff(*e) <= tol)
}

fn assert_near(actual: [u8; 4], expected: [u8; 4], tol: u8, what: &str) {
    assert!(near(actual, expected, tol), "{what}: 期待 {expected:?} 実際 {actual:?}");
}

fn set_track(obj: &mut TimelineObject, effect: &str, key: &str, value: f64) {
    let eff = obj.effects.iter_mut().find(|e| e.name == effect).expect("effect");
    *eff.param_mut(key).expect("param") = ParamValue::Track(Track::constant(value, 1));
}

fn set_blend(obj: &mut TimelineObject, blend: &str) {
    let draw = obj.effects.iter_mut().find(|e| e.kind == EffectKind::StandardDraw).expect("標準描画");
    *draw.param_mut("blend").expect("blend") = ParamValue::Text(blend.into());
}

/// 図形。サイズ 100（new_shape の既定 140 は計算しにくいので揃える）
fn shape(ty: u32, color: &str) -> TimelineObject {
    let mut obj = TimelineObject::new_shape(0, 9, ty, color);
    set_track(&mut obj, "図形", "サイズ", 100.0);
    obj
}

/// 画面全体を塗る背景
fn background(color: &str) -> TimelineObject {
    shape(0, color)
}

/// 単色の画像（ファイル名 `.0`）
struct Solid(&'static str, RgbaFrame);

impl Solid {
    fn new(name: &'static str, w: u32, h: u32, rgba: [u8; 4]) -> Self {
        Solid(name, RgbaFrame { width: w, height: h, pixels: rgba.repeat((w * h) as usize) })
    }
}

impl MediaSource for Solid {
    fn frame(&mut self, path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        (path == self.0).then_some(&self.1)
    }
}

// ---- 合成モード ----

#[test]
fn every_blend_mode_matches_hand_computed_values() {
    let Some(mut r) = renderer() else { return };
    let mut failures = Vec::new();
    for (mode, full, half) in EXPECTED {
        for (transparency, expected) in [(0.0, full), (50.0, half)] {
            let mut s = scene(32, 32);
            s.insert_object(0, background(D));
            let mut top = shape(2, S); // 100x100 の四角で画面全体を覆う
            set_blend(&mut top, &mode.to_string());
            set_track(&mut top, "標準描画", "透明度", transparency);
            s.insert_object(1, top);
            let f = r.render(&s, 0, &mut NoMedia).unwrap();
            let expected = [expected[0], expected[1], expected[2], 255];
            for (x, y) in [(16, 16), (1, 1), (30, 30)] {
                let actual = px(&f, x, y);
                if !near(actual, expected, 1) {
                    failures.push(format!(
                        "{}（{mode}）透明度 {transparency} ({x},{y}): 期待 {expected:?} 実際 {actual:?}",
                        BlendMode::ALL[mode as usize].name()
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "合成モードの結果が違う:\n{}", failures.join("\n"));
}

#[test]
fn blend_mode_outside_the_object_leaves_the_background() {
    // 合成モードのスプライトはクワッドの外・アンチエイリアスの外側で下地を変えない
    let Some(mut r) = renderer() else { return };
    for mode in 1..=12 {
        let mut s = scene(100, 100);
        s.insert_object(0, background(D));
        let mut top = shape(1, S); // 直径 100 の円 → 隅は円の外
        set_track(&mut top, "標準描画", "拡大率", 50.0);
        set_blend(&mut top, &mode.to_string());
        s.insert_object(1, top);
        let f = r.render(&s, 0, &mut NoMedia).unwrap();
        assert_near(px(&f, 2, 2), D_RGB, 0, &format!("モード {mode} の隅"));
        assert_near(px(&f, 50 + 22, 50 + 22), D_RGB, 0, &format!("モード {mode} の円の外（クワッド内）"));
    }
}

#[test]
fn blend_mode_by_name_applies_to_images() {
    // `.aup2` 由来の表示名でも受け付け、画像（テクスチャ）スプライトにも効く
    let Some(mut r) = renderer() else { return };
    let mut s = scene(64, 64);
    s.insert_object(0, background(D));
    let mut image = TimelineObject::new_image(0, 9, "solid.png");
    set_blend(&mut image, "乗算");
    s.insert_object(1, image);
    let f = r.render(&s, 0, &mut Solid::new("solid.png", 20, 20, [200, 100, 30, 255])).unwrap();
    assert_near(px(&f, 32, 32), [50, 50, 23, 255], 1, "画像の乗算");
    assert_near(px(&f, 2, 2), D_RGB, 0, "画像の外");

    // 画像自身のアルファも効果の強さになる（半透明の画素で乗算 → 下地と半々）
    *s.layers[1].objects[0].effects[0].param_mut("file").unwrap() = ParamValue::Text("half.png".into());
    let f = r.render(&s, 0, &mut Solid::new("half.png", 20, 20, [200, 100, 30, 128])).unwrap();
    assert_near(px(&f, 32, 32), [57, 89, 107, 255], 2, "半透明画像の乗算");
}

#[test]
fn consecutive_blend_objects_each_read_the_composite_below() {
    // 合成モードのオブジェクトが続いても、2 つ目は 1 つ目まで合成した結果を下地として読む
    let Some(mut r) = renderer() else { return };
    let mut s = scene(32, 32);
    s.insert_object(0, background(D));
    for layer in 1..=2 {
        let mut sub = shape(2, S);
        set_blend(&mut sub, "2"); // 減算
        s.insert_object(layer, sub);
    }
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    // D - S - S = (64-200-200, 128-100-100, 192-30-30) → (0, 0, 132)
    assert_near(px(&f, 16, 16), [0, 0, 132, 255], 1, "減算を 2 回");

    // 間に通常合成を挟んでも、その結果を読む: D → 通常で (10,20,250)、→ 差分 S → |S - (10,20,250)|
    let mut s = scene(32, 32);
    s.insert_object(0, background(D));
    let mut diff = shape(2, S);
    set_blend(&mut diff, "12");
    s.insert_object(1, diff);
    s.insert_object(2, shape(2, "0a14fa"));
    let mut diff = shape(2, S);
    set_blend(&mut diff, "12");
    s.insert_object(3, diff);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 16, 16), [190, 80, 220, 255], 1, "通常を挟んだ差分");
}

#[test]
fn exo_blend_and_clipping_are_read_and_rendered() {
    let Some(mut r) = renderer() else { return };
    let section = |n: u32, layer: u32, extra: &str, ty: u32, size: u32, color: &str, blend: u32| {
        format!(
            "[{n}]\r\nstart=1\r\nend=10\r\nlayer={layer}\r\noverlay=1\r\n{extra}camera=0\r\n\
             [{n}.0]\r\n_name=図形\r\nサイズ={size}\r\n縦横比=0.0\r\nライン幅=4000\r\ntype={ty}\r\ncolor={color}\r\nname=\r\n\
             [{n}.1]\r\n_name=標準描画\r\nX=0.0\r\nY=0.0\r\nZ=0.0\r\n拡大率=100.00\r\n透明度=0.0\r\n回転=0.00\r\nblend={blend}\r\n"
        )
    };
    let exo = format!(
        "[exedit]\r\nwidth=100\r\nheight=100\r\nrate=30\r\nscale=1\r\nlength=10\r\naudio_rate=44100\r\naudio_ch=2\r\n{}{}{}",
        section(0, 1, "", 0, 100, D, 0),
        // レイヤー 2: 直径 50 の円（クリッピング元。自身は描かれない）
        section(1, 2, "", 1, 50, "ffffff", 0),
        // レイヤー 3: 画面を覆う四角を乗算、上のオブジェクトでクリッピング
        section(2, 3, "clipping=1\r\n", 2, 100, S, 3),
    );
    let doc = au2_exo::parse_exo_str(&exo).unwrap();
    let obj = &doc.scene.layers[2].objects[0];
    assert!(obj.clipping);
    assert_eq!(BlendMode::of_object(obj), BlendMode::Multiply);
    let f = r.render(&doc.scene, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 50, 50), [50, 50, 23, 255], 1, "円の内側は乗算");
    assert_near(px(&f, 50 + 35, 50), D_RGB, 0, "円の外は下地のまま（白い円も描かれない）");
}

// ---- クリッピング ----

/// レイヤー 1: 白い円（直径 100、中心）、レイヤー 2: 画面を覆う赤い四角
fn clip_scene() -> Scene {
    let mut s = scene(200, 100);
    s.insert_object(0, shape(1, "ffffff"));
    let mut red = shape(2, "ff0000");
    set_track(&mut red, "標準描画", "拡大率", 200.0);
    s.insert_object(1, red);
    s
}

fn assert_clipped_to_circle(f: &RgbaFrame) {
    assert_near(px(f, 100, 50), [255, 0, 0, 255], 0, "円の中心");
    assert_near(px(f, 100 + 45, 50), [255, 0, 0, 255], 0, "円の内側");
    assert_near(px(f, 100 + 60, 50), [0, 0, 0, 255], 0, "円の外（四角の内側）");
    assert_near(px(f, 3, 3), [0, 0, 0, 255], 0, "隅");
    // クリッピング元の白い円は描かれない: どこにも緑・青の成分が無い。縁はマスクのアンチエイリアスで中間の赤
    assert!(f.pixels.chunks(4).all(|p| p[1] == 0 && p[2] == 0), "クリッピング元が描かれている");
    let row: Vec<u8> = (140..160).map(|x| px(f, x, 50)[0]).collect();
    assert!(row.iter().any(|&v| v > 0 && v < 255), "縁がアンチエイリアスされていない: {row:?}");
}

#[test]
fn clipping_cuts_the_object_to_the_upper_shape() {
    let Some(mut r) = renderer() else { return };
    let mut s = clip_scene();
    // クリッピング無し: 四角が全体を覆う
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 160, 50), [255, 0, 0, 255], 0, "クリッピング無し");
    s.layers[1].objects[0].clipping = true;
    assert_clipped_to_circle(&r.render(&s, 0, &mut NoMedia).unwrap());
}

#[test]
fn layer_clip_above_setting_also_clips() {
    let Some(mut r) = renderer() else { return };
    let mut s = clip_scene();
    s.layers[1].clip_above = true;
    assert_clipped_to_circle(&r.render(&s, 0, &mut NoMedia).unwrap());
}

#[test]
fn clipping_without_an_object_directly_above_draws_nothing() {
    let Some(mut r) = renderer() else { return };
    // 上のレイヤーのオブジェクトが別の時間にしか無い
    let mut s = scene(64, 64);
    s.insert_object(0, TimelineObject::new_shape(20, 29, 1, "ffffff"));
    let mut red = shape(2, "ff0000");
    red.clipping = true;
    s.insert_object(1, red.clone());
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert!(f.pixels.chunks(4).all(|p| p == [0, 0, 0, 255]), "上が空なのに描かれた");

    // 2 つ上のレイヤーにしか無い場合も、すぐ上ではないので描かない
    let mut s = scene(64, 64);
    s.insert_object(0, shape(1, "ffffff"));
    s.insert_object(2, red.clone());
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 32, 32), [255, 255, 255, 255], 0, "2 つ上の円はクリッピング元にならず普通に描かれる");
    assert!(f.pixels.chunks(4).all(|p| p[0] == p[1]), "赤い四角が描かれた");

    // 先頭レイヤー（上が無い）
    let mut s = scene(64, 64);
    s.insert_object(0, red);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert!(f.pixels.chunks(4).all(|p| p == [0, 0, 0, 255]), "先頭レイヤーなのに描かれた");
}

#[test]
fn upper_object_alpha_scales_the_clipped_object() {
    let Some(mut r) = renderer() else { return };
    let mut s = clip_scene();
    set_track(&mut s.layers[0].objects[0], "標準描画", "透明度", 50.0);
    s.layers[1].objects[0].clipping = true;
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 100, 50), [128, 0, 0, 255], 1, "半透明の円で切り抜く → 赤が半分");
    assert_near(px(&f, 160, 50), [0, 0, 0, 255], 0, "円の外");
}

#[test]
fn clipped_object_keeps_its_blend_mode_and_background_survives() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(200, 100);
    s.insert_object(0, background("0000ff"));
    s.insert_object(1, shape(1, "ffffff")); // クリッピング元（描かれない）
    let mut green = shape(2, "00ff00");
    set_track(&mut green, "標準描画", "拡大率", 200.0);
    set_blend(&mut green, "1"); // 加算
    green.clipping = true;
    s.insert_object(2, green);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 100, 50), [0, 255, 255, 255], 0, "円の内側: 青 + 緑");
    assert_near(px(&f, 160, 50), [0, 0, 255, 255], 0, "円の外: 背景の青のまま");
}

// ---- ゴールデンイメージ ----

/// 縦横のグラデーション（R: 左→右で減少、G: 上→下で増加、B: 左→右で増加）
struct Backdrop(RgbaFrame);

impl Backdrop {
    fn new(w: u32, h: u32) -> Self {
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let t = x as f32 / (w - 1) as f32;
                let u = y as f32 / (h - 1) as f32;
                pixels.extend_from_slice(&[((1.0 - t) * 255.0) as u8, (u * 255.0) as u8, (t * 255.0) as u8, 255]);
            }
        }
        Backdrop(RgbaFrame { width: w, height: h, pixels })
    }
}

impl MediaSource for Backdrop {
    fn frame(&mut self, path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        (path == "backdrop.png").then_some(&self.0)
    }
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.png"))
}

/// tests/render.rs と同じ基準: 1 チャンネル ±3 を超える画素が全体の 0.1% 以内なら一致
fn assert_golden(name: &str, frame: &RgbaFrame) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        image::save_buffer(&path, &frame.pixels, frame.width, frame.height, image::ColorType::Rgba8).unwrap();
        eprintln!("ゴールデンイメージを書き出しました: {}", path.display());
        return;
    }
    let expected = image::open(&path)
        .unwrap_or_else(|e| panic!("{name}: ゴールデンイメージを開けません（UPDATE_GOLDEN=1 で作成）: {e}"))
        .to_rgba8();
    assert_eq!((expected.width(), expected.height()), (frame.width, frame.height), "{name}: サイズ不一致");
    let bad = expected
        .as_raw()
        .chunks(4)
        .zip(frame.pixels.chunks(4))
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 3))
        .count();
    let allowed = (frame.width * frame.height) as usize / 1000;
    assert!(bad <= allowed, "{name}: 許容を超える画素が {bad} 個（許容 {allowed}）");
}

#[test]
fn golden_blend_modes() {
    // グラデーションの上に、同じ色の円を 12 種の合成モードで 6x2 に並べる（左上から 1 加算 … 右下 12 差分）。
    // 下段中央: 星でクリッピングした四角（加算）、左右: 通常合成の円・半透明の乗算の円
    let Some(mut r) = renderer() else { return };
    let mut s = scene(320, 180);
    s.insert_object(0, TimelineObject::new_image(0, 9, "backdrop.png"));
    let mut layer = 1;
    for mode in 1..=12u32 {
        let k = (mode - 1) % 6;
        let row = (mode - 1) / 6;
        let mut c = shape(1, "d09040");
        set_track(&mut c, "標準描画", "X", -125.0 + 50.0 * k as f64);
        set_track(&mut c, "標準描画", "Y", -60.0 + 55.0 * row as f64);
        set_track(&mut c, "標準描画", "拡大率", 44.0);
        set_blend(&mut c, &mode.to_string());
        s.insert_object(layer, c);
        layer += 1;
    }
    let mut normal = shape(1, "d09040");
    set_track(&mut normal, "標準描画", "X", -110.0);
    set_track(&mut normal, "標準描画", "Y", 60.0);
    set_track(&mut normal, "標準描画", "拡大率", 44.0);
    s.insert_object(layer, normal);
    layer += 1;
    let mut faint = shape(1, "d09040");
    set_track(&mut faint, "標準描画", "X", 110.0);
    set_track(&mut faint, "標準描画", "Y", 60.0);
    set_track(&mut faint, "標準描画", "拡大率", 44.0);
    set_track(&mut faint, "標準描画", "透明度", 50.0);
    set_blend(&mut faint, "3");
    s.insert_object(layer, faint);
    layer += 1;
    let mut star = shape(6, "ffffff");
    set_track(&mut star, "標準描画", "Y", 58.0);
    set_track(&mut star, "標準描画", "拡大率", 64.0);
    s.insert_object(layer, star);
    layer += 1;
    let mut clipped = shape(2, "d09040");
    set_track(&mut clipped, "標準描画", "Y", 58.0);
    set_track(&mut clipped, "標準描画", "拡大率", 48.0); // 星より小さく、星と四角の両方の縁が見える
    set_track(&mut clipped, "標準描画", "回転", 30.0);
    set_blend(&mut clipped, "1");
    clipped.clipping = true;
    s.insert_object(layer, clipped);
    let f = r.render(&s, 0, &mut Backdrop::new(320, 180)).unwrap();
    assert_golden("blend_modes", &f);
}
