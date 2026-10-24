//! 描画エンジンのテスト。GPU が無い環境（CI の Linux など）ではスキップします。
//!
//! - 画素プローブ: 既知の位置の色を直接検証（GPU 差に強い）
//! - ゴールデンイメージ: `tests/golden/*.png` と比較。`UPDATE_GOLDEN=1 cargo test -p au2-render` で更新

use std::path::PathBuf;

use au2_core::{Effect, Scene, SceneSettings, TimelineObject, Track};
use au2_render::{Gpu, MediaSource, NoMedia, Renderer, RgbaFrame};

fn renderer() -> Option<Renderer> {
    match Gpu::headless() {
        Ok(gpu) => {
            eprintln!("GPU: {} ({:?})", gpu.adapter_info.name, gpu.adapter_info.backend);
            Some(Renderer::new(gpu))
        }
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

fn assert_near(actual: [u8; 4], expected: [u8; 4], tol: u8, what: &str) {
    for c in 0..4 {
        assert!(
            actual[c].abs_diff(expected[c]) <= tol,
            "{what}: 期待 {expected:?} 実際 {actual:?}（チャンネル {c}）"
        );
    }
}

fn set_track(obj: &mut TimelineObject, effect: &str, key: &str, value: f64) {
    let eff = obj.effects.iter_mut().find(|e| e.name == effect).expect("effect");
    *eff.param_mut(key).expect("param") = au2_core::ParamValue::Track(Track::constant(value, 1));
}

/// new_shape の既定サイズは 140。テストでは計算しやすいよう 100 に揃える。
fn shape(start: u32, end: u32, ty: u32, color: &str) -> TimelineObject {
    let mut obj = TimelineObject::new_shape(start, end, ty, color);
    set_track(&mut obj, "図形", "サイズ", 100.0);
    obj
}

#[test]
fn empty_scene_is_opaque_black() {
    let Some(mut r) = renderer() else { return };
    let f = r.render(&scene(64, 48), 0, &mut NoMedia).unwrap();
    assert_eq!((f.width, f.height), (64, 48));
    assert!(f.pixels.chunks(4).all(|p| p == [0, 0, 0, 255]));
}

#[test]
fn non_multiple_of_64_width_reads_back_correctly() {
    // 1 行 4*w バイトが 256 の倍数にならない幅でも行がずれないこと
    let Some(mut r) = renderer() else { return };
    let mut s = scene(70, 20);
    s.insert_object(0, shape(0, 9, 0, "336699")); // 背景
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_eq!(f.pixels.len(), 70 * 20 * 4);
    for (x, y) in [(0, 0), (69, 0), (0, 19), (69, 19), (35, 10)] {
        assert_near(px(&f, x, y), [0x33, 0x66, 0x99, 255], 1, "背景");
    }
}

#[test]
fn circle_is_filled_inside_and_black_outside() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(200, 100);
    s.insert_object(0, shape(0, 9, 1, "ff0000")); // 直径 100 の円
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 100, 50), [255, 0, 0, 255], 1, "中心");
    assert_near(px(&f, 100 + 45, 50), [255, 0, 0, 255], 1, "半径内");
    assert_near(px(&f, 100 + 55, 50), [0, 0, 0, 255], 1, "半径外");
    assert_near(px(&f, 5, 5), [0, 0, 0, 255], 1, "隅");
    // 縁の画素はアンチエイリアスで中間値になる
    let row: Vec<u8> = (140..160).map(|x| px(&f, x, 50)[0]).collect();
    assert!(row.iter().any(|&v| v > 0 && v < 255), "縁にアンチエイリアスの中間値がある: {row:?}");
}

#[test]
fn position_zoom_and_alpha_are_applied() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(200, 100);
    let mut obj = shape(0, 9, 2, "00ff00"); // 四角形 100x100
    set_track(&mut obj, "標準描画", "X", 50.0);
    set_track(&mut obj, "標準描画", "拡大率", 50.0); // 50x50 に縮小
    set_track(&mut obj, "標準描画", "透明度", 50.0);
    s.insert_object(0, obj);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    // 中心は (150, 50)、辺は 50px → x 125..175, y 25..75。半透明の緑 on 黒 = 128
    assert_near(px(&f, 150, 50), [0, 128, 0, 255], 2, "中心");
    assert_near(px(&f, 130, 30), [0, 128, 0, 255], 2, "内側");
    assert_near(px(&f, 120, 50), [0, 0, 0, 255], 1, "左外");
    assert_near(px(&f, 150, 20), [0, 0, 0, 255], 1, "上外");
}

#[test]
fn rotation_turns_square_into_diamond() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(200, 200);
    let mut obj = shape(0, 9, 2, "ffffff"); // 100x100
    set_track(&mut obj, "標準描画", "回転", 45.0);
    s.insert_object(0, obj);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    // 45 度回転した正方形の頂点は中心から 100/√2 ≈ 70.7px。軸上では 65px まで白、隅は黒
    assert_near(px(&f, 100 + 65, 100), [255, 255, 255, 255], 1, "頂点方向");
    assert_near(px(&f, 100 + 45, 100 + 45), [0, 0, 0, 255], 1, "元の角の位置");
}

#[test]
fn layers_composite_in_order() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(100, 100);
    s.insert_object(0, shape(0, 9, 0, "0000ff")); // 背面: 青
    let mut square = shape(0, 9, 2, "ffff00"); // 前面: 黄色の四角（50x50 に縮小）
    set_track(&mut square, "標準描画", "拡大率", 50.0);
    s.insert_object(1, square);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 50, 50), [255, 255, 0, 255], 1, "前面");
    assert_near(px(&f, 2, 2), [0, 0, 255, 255], 1, "背面");

    // 非表示レイヤーは描かれない
    s.layers[1].visible = false;
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 50, 50), [0, 0, 255, 255], 1, "非表示");
}

#[test]
fn objects_outside_their_frame_range_are_skipped() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(40, 40);
    s.insert_object(0, shape(10, 19, 0, "ffffff"));
    assert_near(px(&r.render(&s, 5, &mut NoMedia).unwrap(), 20, 20), [0, 0, 0, 255], 0, "範囲前");
    assert_near(px(&r.render(&s, 10, &mut NoMedia).unwrap(), 20, 20), [255, 255, 255, 255], 0, "範囲内");
    assert_near(px(&r.render(&s, 20, &mut NoMedia).unwrap(), 20, 20), [0, 0, 0, 255], 0, "範囲後");
}

#[test]
fn color_correction_filter_affects_only_layers_below() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(100, 50);
    // レイヤー1: 左に置いた白い四角（フィルタの下）、レイヤー2: 色調補正、レイヤー3: 右に置いた白い四角（フィルタの上）
    let mut below = shape(0, 9, 2, "ffffff");
    set_track(&mut below, "標準描画", "X", -25.0);
    set_track(&mut below, "標準描画", "拡大率", 40.0);
    let mut above = shape(0, 9, 2, "ffffff");
    set_track(&mut above, "標準描画", "X", 25.0);
    set_track(&mut above, "標準描画", "拡大率", 40.0);
    let mut cc = TimelineObject::new_color_correction(0, 9);
    cc.effects[0] = {
        let mut e = Effect::new("色調補正");
        e.push_track("明るさ", Track::constant(50.0, 1));
        e
    };
    s.insert_object(0, below);
    s.insert_object(1, cc);
    s.insert_object(2, above);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 25, 25), [128, 128, 128, 255], 2, "フィルタの下（半分の明るさ）");
    assert_near(px(&f, 75, 25), [255, 255, 255, 255], 1, "フィルタの上（影響なし）");
}

#[test]
fn neutral_color_correction_is_identity() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(32, 32);
    s.insert_object(0, shape(0, 9, 0, "3a7bd5"));
    let plain = r.render(&s, 0, &mut NoMedia).unwrap();
    s.insert_object(1, TimelineObject::new_color_correction(0, 9));
    let corrected = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_eq!(plain.pixels, corrected.pixels);
}

/// テスト用のメモリ上メディア: 水平グラデーション（左 = 赤、右 = 青）
struct Gradient(RgbaFrame);

impl Gradient {
    fn new(w: u32, h: u32) -> Self {
        let mut pixels = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                let t = x as f32 / (w - 1) as f32;
                pixels.extend_from_slice(&[((1.0 - t) * 255.0) as u8, 0, (t * 255.0) as u8, 255]);
            }
        }
        Gradient(RgbaFrame { width: w, height: h, pixels })
    }
}

impl MediaSource for Gradient {
    fn frame(&mut self, path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        (path == "gradient.png").then_some(&self.0)
    }
}

#[test]
fn image_is_drawn_at_native_size_and_missing_media_shows_placeholder() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(100, 100);
    s.insert_object(0, TimelineObject::new_image(0, 9, "gradient.png"));
    let f = r.render(&s, 0, &mut Gradient::new(40, 20)).unwrap();
    // 40x20 が中央（x 30..70, y 40..60）に等倍で置かれる
    let left = px(&f, 31, 50);
    let right = px(&f, 68, 50);
    assert!(left[0] > 200 && left[2] < 50, "左端は赤: {left:?}");
    assert!(right[2] > 200 && right[0] < 50, "右端は青: {right:?}");
    assert_near(px(&f, 10, 10), [0, 0, 0, 255], 0, "画像の外");
    assert_near(px(&f, 50, 30), [0, 0, 0, 255], 0, "画像の上");

    // 供給元が見つけられないファイルはプレースホルダ（画面いっぱいの青灰色）
    let mut s = scene(100, 100);
    s.insert_object(0, TimelineObject::new_image(0, 9, "missing.png"));
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 50, 50), [60, 70, 90, 255], 1, "プレースホルダ");
}

// ---- ゴールデンイメージ ----

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.png"))
}

/// 1 チャンネル ±3 の誤差を許し、超過する画素が全体の 0.1% 以内なら一致とみなす。
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
fn golden_composition() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(320, 180);
    s.insert_object(0, shape(0, 9, 0, "1a237e")); // 背景
    let mut circle = shape(0, 9, 1, "e94560");
    set_track(&mut circle, "標準描画", "X", -90.0);
    set_track(&mut circle, "標準描画", "拡大率", 80.0);
    s.insert_object(1, circle);
    let mut square = shape(0, 9, 2, "0f9b8e");
    set_track(&mut square, "標準描画", "拡大率", 60.0);
    set_track(&mut square, "標準描画", "回転", 20.0);
    s.insert_object(2, square);
    let mut star = shape(0, 9, 6, "ffd54a");
    set_track(&mut star, "標準描画", "X", 95.0);
    set_track(&mut star, "標準描画", "拡大率", 90.0);
    s.insert_object(3, star);
    let mut pentagon = shape(0, 9, 5, "ffffff");
    pentagon.effects[0] = {
        let mut e = Effect::new("図形");
        e.push_track("サイズ", Track::constant(60.0, 0));
        e.push_text("type", "5");
        e.push_text("color", "ffffff");
        e
    };
    set_track(&mut pentagon, "標準描画", "Y", 60.0);
    set_track(&mut pentagon, "標準描画", "透明度", 30.0);
    s.insert_object(4, pentagon);
    let mut cc = TimelineObject::new_color_correction(0, 9);
    cc.effects[0] = {
        let mut e = Effect::new("色調補正");
        e.push_track("彩度", Track::constant(60.0, 1));
        e.push_track("色相", Track::constant(0.0, 1));
        e
    };
    s.insert_object(5, cc);
    let f = r.render(&s, 0, &mut Gradient::new(1, 1)).unwrap();
    assert_golden("composition", &f);
}

#[test]
fn golden_sample_exo() {
    // samples/sample.exo をそのまま描く（フォントを設定しないのでテキストは描かれず、動画は見つからないためプレースホルダになる）
    let Some(mut r) = renderer() else { return };
    let bytes = std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples/sample.exo")).unwrap();
    let doc = au2_exo::parse_exo_bytes(&bytes).unwrap();
    let f = r.render(&doc.scene, 30, &mut NoMedia).unwrap();
    assert_golden("sample_exo_f30", &f);
}

#[test]
fn text_is_drawn_when_a_font_is_available() {
    let Some(mut r) = renderer() else { return };
    if r.load_system_font().is_none() {
        eprintln!("日本語フォントが無いためスキップします");
        return;
    }
    let mut s = scene(200, 100);
    let mut text = TimelineObject::new_text(0, 9, "あA");
    set_track(&mut text, "テキスト", "サイズ", 60.0);
    s.insert_object(0, text);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    // 文字は白。中央付近に白っぽい画素があり、隅は黒のまま
    let lit = f.pixels.chunks(4).filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200).count();
    assert!(lit > 100, "白い画素が少なすぎる: {lit}");
    assert_near(px(&f, 2, 2), [0, 0, 0, 255], 0, "隅");
    assert_near(px(&f, 197, 97), [0, 0, 0, 255], 0, "隅");
    // 文字は中央に寄っている: 左右の端 20px には何も描かれない
    for y in 0..100 {
        for x in (0..20).chain(180..200) {
            assert_eq!(px(&f, x, y)[0], 0, "端に描画がある ({x},{y})");
        }
    }

    // 空文字は何も描かない
    let mut s = scene(50, 50);
    s.insert_object(0, TimelineObject::new_text(0, 9, ""));
    assert!(r.render(&s, 0, &mut NoMedia).unwrap().pixels.chunks(4).all(|p| p == [0, 0, 0, 255]));
}

#[test]
fn object_bounds_follow_position_zoom_and_rotation() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(200, 100);
    let mut obj = shape(0, 9, 2, "ffffff");
    set_track(&mut obj, "標準描画", "X", 30.0);
    set_track(&mut obj, "標準描画", "Y", -10.0);
    set_track(&mut obj, "標準描画", "拡大率", 50.0);
    set_track(&mut obj, "標準描画", "回転", 90.0);
    let id = obj.id;
    s.insert_object(0, obj);
    r.render_to_framebuffer(&s, 0, &mut NoMedia).unwrap();
    let b = r.object_bounds(id).expect("描いたオブジェクトの範囲");
    assert_eq!(b.center, [30.0, -10.0]);
    assert_eq!(b.half, [25.0, 25.0]);
    assert!((b.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    assert!(r.framebuffer_view().is_some());
    // 範囲外のフレームでは記録されない
    r.render_to_framebuffer(&s, 50, &mut NoMedia).unwrap();
    assert!(r.object_bounds(id).is_none());
}

#[test]
fn oversized_scene_is_an_error_not_a_panic() {
    let Some(mut r) = renderer() else { return };
    let max = r.gpu().device.limits().max_texture_dimension_2d;
    let s = scene(max + 1, 8);
    let err = r.render_to_framebuffer(&s, 0, &mut NoMedia).unwrap_err();
    assert!(matches!(err, au2_render::RenderError::SceneTooLarge { .. }), "{err}");
    // 上限以内なら通る（メモリ節約のため幅は 4096 まで）
    assert!(r.render_to_framebuffer(&scene(max.min(4096), 4), 0, &mut NoMedia).is_ok());
}

/// 任意サイズのメディアを返す供給元（原寸を超える巨大画像のテスト用）。
struct Huge(RgbaFrame);

impl MediaSource for Huge {
    fn frame(&mut self, _path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        Some(&self.0)
    }
}

#[test]
fn image_wider_than_the_gpu_limit_is_downscaled_instead_of_panicking() {
    let Some(mut r) = renderer() else { return };
    let max = r.gpu().device.limits().max_texture_dimension_2d;
    let (w, h) = (max + 8, 2u32);
    let pixels = [200u8, 100, 50, 255].repeat((w * h) as usize);
    let mut s = scene(64, 32);
    s.insert_object(0, TimelineObject::new_image(0, 9, "huge.png"));
    // 縮小してアップロードされても、描画上の大きさは元の幅（シーンより遥かに広い）。高さは 2px なので、
    // 画像が覆うのは中央の 2 行（y=15,16）だけ
    let f = r.render(&s, 0, &mut Huge(RgbaFrame { width: w, height: h, pixels })).unwrap();
    assert_near(px(&f, 32, 16), [200, 100, 50, 255], 2, "画像が覆う帯の色");
    assert_near(px(&f, 0, 15), [200, 100, 50, 255], 2, "左端まで届く（元の幅で描かれる）");
    assert_near(px(&f, 32, 5), [0, 0, 0, 255], 0, "帯の外は黒");
}

/// 縮小デコードされる動画を模した供給元: 実フレームは 4x4 だが、本来のサイズは 16x16 と答える。
struct Shrunk(RgbaFrame);

impl MediaSource for Shrunk {
    fn frame(&mut self, _path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        Some(&self.0)
    }
    fn native_size(&mut self, _path: &str) -> Option<(u32, u32)> {
        Some((16, 16))
    }
}

#[test]
fn sprite_size_follows_native_size_not_decoded_size() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(40, 40);
    s.insert_object(0, TimelineObject::new_video(0, 9, "v.mp4"));
    let frame = RgbaFrame { width: 4, height: 4, pixels: [255u8, 0, 0, 255].repeat(16) };
    let f = r.render(&s, 0, &mut Shrunk(frame)).unwrap();
    // 16x16 が中央（x,y 12..28）に置かれる
    assert_near(px(&f, 14, 20), [255, 0, 0, 255], 1, "16px 範囲の内側");
    assert_near(px(&f, 10, 20), [0, 0, 0, 255], 1, "16px 範囲の左外");
    assert_near(px(&f, 30, 20), [0, 0, 0, 255], 1, "右外");
}

/// 要求された時刻を記録し、長さ 1 秒の動画のふりをする供給元。
struct Clip {
    requested: Vec<f64>,
    frame: RgbaFrame,
}

impl MediaSource for Clip {
    fn frame(&mut self, _path: &str, _media_frame: u32, time_sec: f64) -> Option<&RgbaFrame> {
        self.requested.push(time_sec);
        Some(&self.frame)
    }
    fn duration_secs(&mut self, _path: &str) -> Option<f64> {
        Some(1.0)
    }
}

#[test]
fn looping_video_wraps_its_time_at_the_clip_length() {
    let Some(mut r) = renderer() else { return };
    let frame = RgbaFrame { width: 2, height: 2, pixels: [255u8, 255, 255, 255].repeat(4) };
    for (looping, expected) in [(true, 0.5), (false, 1.5)] {
        let mut s = scene(8, 8); // 30fps
        let mut obj = TimelineObject::new_video(0, 99, "clip.mp4");
        set_track(&mut obj, "動画ファイル", "ループ再生", if looping { 1.0 } else { 0.0 });
        s.insert_object(0, obj);
        let mut clip = Clip { requested: Vec::new(), frame: frame.clone() };
        r.render_to_framebuffer(&s, 45, &mut clip).unwrap(); // 1.5 秒の位置
        if looping {
            // 長さが分かっているので、巻き戻し前の時刻（1 秒超）は一度も要求しない
            assert!(clip.requested.iter().all(|t| *t < 1.0), "巻き戻し前の時刻を要求している: {:?}", clip.requested);
        }
        let last = *clip.requested.last().unwrap();
        assert!((last - expected).abs() < 1e-6, "ループ {looping}: 要求された時刻 {last}（期待 {expected}）");
    }
}

/// 長さが分かるのは `prepare` の後だけ、という供給元（MKV を開く前のように、最初は長さが不明）。
struct OpensOnPrepare {
    prepared: bool,
    requested: Vec<f64>,
    frame: RgbaFrame,
}

impl MediaSource for OpensOnPrepare {
    fn frame(&mut self, _path: &str, _media_frame: u32, time_sec: f64) -> Option<&RgbaFrame> {
        self.requested.push(time_sec);
        Some(&self.frame)
    }
    fn prepare(&mut self, _path: &str) {
        self.prepared = true;
    }
    fn duration_secs(&mut self, _path: &str) -> Option<f64> {
        self.prepared.then_some(1.0)
    }
}

#[test]
fn loop_opens_the_video_before_requesting_any_frame() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(8, 8);
    let mut obj = TimelineObject::new_video(0, 99, "clip.mp4");
    set_track(&mut obj, "動画ファイル", "ループ再生", 1.0);
    s.insert_object(0, obj);
    let frame = RgbaFrame { width: 2, height: 2, pixels: [255u8; 16].to_vec() };
    let mut media = OpensOnPrepare { prepared: false, requested: Vec::new(), frame };
    r.render_to_framebuffer(&s, 45, &mut media).unwrap(); // 1.5 秒
    assert!(media.prepared, "先に prepare される");
    // 長さが分かったので、巻き戻し前の時刻（1.5 秒）では一度も要求しない = 終端の先へのシークを起こさない
    assert_eq!(media.requested.len(), 1, "要求は 1 回だけ: {:?}", media.requested);
    assert!((media.requested[0] - 0.5).abs() < 1e-6, "要求された時刻 {}", media.requested[0]);
}

// ---- 時間制御・シーン・フレームバッファ・直前オブジェクト ----

fn set_param(obj: &mut TimelineObject, key: &str, track: Track) {
    *obj.effects[0].param_mut(key).expect("param") = au2_core::ParamValue::Track(track);
}

fn linear(from: f64, to: f64, last: u32) -> Track {
    Track {
        keyframes: vec![au2_core::Keyframe { frame: 0, value: from }, au2_core::Keyframe { frame: last, value: to }],
        interpolation: Some(au2_core::Interpolation::linear()),
        precision: 1,
    }
}

fn white_clip() -> Clip {
    Clip { requested: Vec::new(), frame: RgbaFrame { width: 2, height: 2, pixels: [255u8; 16].to_vec() } }
}

/// 時間制御（`ctrl_layer`）とレイヤー2 の動画を置いて `frame` を描き、動画に要求された時刻[秒]を返す。
fn requested_time_under(r: &mut Renderer, ctrl: TimelineObject, ctrl_layer: usize, frame: u32) -> f64 {
    let mut s = scene(8, 8); // 30fps
    s.insert_object(ctrl_layer, ctrl);
    s.insert_object(1, TimelineObject::new_video(0, 299, "clip.mp4"));
    let mut clip = white_clip();
    r.render_to_framebuffer(&s, frame, &mut clip).unwrap();
    assert_eq!(clip.requested.len(), 1, "要求は 1 回: {:?}", clip.requested);
    clip.requested[0]
}

#[test]
fn time_control_remaps_the_media_time_of_lower_layers() {
    let Some(mut r) = renderer() else { return };
    let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
    // 既定（位置 0 → 100%）は恒等
    let t = requested_time_under(&mut r, TimelineObject::new_time_control(0, 100), 0, 10);
    assert!(near(t, 10.0 / 30.0), "恒等: {t}");
    // 位置を 50% に固定 → 長さ 101 の 50% = 50 フレーム目
    let mut freeze = TimelineObject::new_time_control(0, 100);
    set_param(&mut freeze, "位置", Track::constant(50.0, 1));
    let t = requested_time_under(&mut r, freeze.clone(), 0, 10);
    assert!(near(t, 50.0 / 30.0), "静止: {t}");
    // 逆再生（位置 100 → 0%）: 10 フレーム目は 90 フレーム目の絵
    let mut reverse = TimelineObject::new_time_control(0, 100);
    set_param(&mut reverse, "位置", linear(100.0, 0.0, 100));
    let t = requested_time_under(&mut r, reverse, 0, 10);
    assert!(near(t, 90.0 / 30.0), "逆再生: {t}");
    // コマ落ち 4: 10 フレーム目は 8 フレーム目の絵
    let mut drop = TimelineObject::new_time_control(0, 100);
    set_param(&mut drop, "コマ落ち", Track::constant(4.0, 0));
    let t = requested_time_under(&mut r, drop, 0, 10);
    assert!(near(t, 8.0 / 30.0), "コマ落ち: {t}");
    // 動画より下のレイヤーにある時間制御は効かない
    let t = requested_time_under(&mut r, freeze, 2, 10);
    assert!(near(t, 10.0 / 30.0), "下の時間制御は無関係: {t}");
}

#[test]
fn time_control_shows_objects_that_exist_at_the_remapped_time() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(20, 20);
    let mut ctrl = TimelineObject::new_time_control(0, 100);
    set_param(&mut ctrl, "位置", Track::constant(55.0, 1)); // 55 フレーム目
    s.insert_object(0, ctrl);
    s.insert_object(1, shape(50, 60, 0, "ff0000"));
    assert_near(px(&r.render(&s, 0, &mut NoMedia).unwrap(), 10, 10), [255, 0, 0, 255], 0, "置き換え後の時刻のオブジェクト");
    s.layers[0].visible = false;
    assert_near(px(&r.render(&s, 0, &mut NoMedia).unwrap(), 10, 10), [0, 0, 0, 255], 0, "時間制御が無ければ範囲外");
}

/// Root (100x100、青背景) と、40x20 の子シーン（中央に 10x10 の赤い四角、周りは透明）。
fn project_with_child() -> au2_core::Project {
    let mut root = scene(100, 100);
    root.insert_object(0, shape(0, 9, 0, "0000ff"));
    let mut child = Scene::new("Scene1", SceneSettings { width: 40, height: 20, length: 30, ..Default::default() });
    let mut square = shape(0, 29, 2, "ff0000");
    set_track(&mut square, "標準描画", "拡大率", 10.0);
    child.insert_object(0, square);
    let mut project = au2_core::Project::new("p", root);
    project.scenes.push(child);
    project
}

#[test]
fn scene_object_draws_the_other_scene_at_its_position() {
    let Some(mut r) = renderer() else { return };
    let mut project = project_with_child();
    let mut obj = TimelineObject::new_scene(0, 9, 1);
    set_track(&mut obj, "標準描画", "X", 20.0);
    set_track(&mut obj, "標準描画", "拡大率", 200.0); // 80x40、中の四角は 20x20
    project.scenes[0].insert_object(1, obj);
    let f = r.render_project(&project, 0, 0, &mut NoMedia).unwrap();
    // 子シーンの中心は (70, 50)。赤い四角は x 60..80, y 40..60
    assert_near(px(&f, 70, 50), [255, 0, 0, 255], 1, "子シーンの中身");
    assert_near(px(&f, 62, 42), [255, 0, 0, 255], 1, "四角の内側（拡大率 200%）");
    assert_near(px(&f, 40, 50), [0, 0, 255, 255], 1, "子シーンの透明な部分から親の背景が見える");
    assert_near(px(&f, 57, 50), [0, 0, 255, 255], 1, "四角の左外");
    assert_near(px(&f, 20, 20), [0, 0, 255, 255], 1, "子シーンの外");

    // 子シーンの時刻: 四角を 10 フレーム目以降だけにすると、0 フレーム目には無く、再生位置 11 なら見える
    project.scenes[1].layers[0].objects[0].frame_start = 10;
    let f = r.render_project(&project, 0, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 70, 50), [0, 0, 255, 255], 1, "子シーンの 0 フレーム目には四角が無い");
    let obj = &mut project.scenes[0].layers[1].objects[0];
    set_param(obj, "再生位置", Track::constant(11.0, 0));
    let f = r.render_project(&project, 0, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 70, 50), [255, 0, 0, 255], 1, "再生位置 11 = 子の 10 フレーム目");

    // Project を渡さない render() ではシーンオブジェクトは描かれない
    let f = r.render(&project.scenes[0], 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 70, 50), [0, 0, 255, 255], 1, "参照先が無い");
    assert!(r.render_project(&project, 5, 0, &mut NoMedia).is_err(), "存在しないシーン");
}

#[test]
fn recursive_scene_references_terminate() {
    let Some(mut r) = renderer() else { return };
    let mut project = project_with_child();
    // Root → 自分自身、Scene1 → Scene2 → Scene1 の循環
    project.scenes[0].insert_object(1, TimelineObject::new_scene(0, 9, 0));
    project.scenes[1].insert_object(1, TimelineObject::new_scene(0, 29, 2));
    let mut scene2 = Scene::new("Scene2", SceneSettings { width: 40, height: 20, length: 30, ..Default::default() });
    scene2.insert_object(0, TimelineObject::new_scene(0, 29, 1));
    project.scenes.push(scene2);
    project.scenes[0].insert_object(2, TimelineObject::new_scene(0, 9, 1));
    let f = r.render_project(&project, 0, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 50, 50), [255, 0, 0, 255], 1, "循環の外側は描かれる（Scene1 の四角）");
    assert_near(px(&f, 5, 5), [0, 0, 255, 255], 1, "背景");
}

#[test]
fn framebuffer_object_redraws_what_was_composited_so_far() {
    let Some(mut r) = renderer() else { return };
    let build = |clear: bool| {
        let mut s = scene(100, 100);
        s.insert_object(0, shape(0, 9, 0, "00ff00"));
        let mut dot = shape(0, 9, 2, "ff0000"); // 10x10 の赤を (10, 10) を中心に
        set_track(&mut dot, "標準描画", "X", -40.0);
        set_track(&mut dot, "標準描画", "Y", -40.0);
        set_track(&mut dot, "標準描画", "拡大率", 10.0);
        s.insert_object(1, dot);
        let mut fb = TimelineObject::new_framebuffer(0, 9);
        set_param(&mut fb, "フレームバッファをクリア", Track::constant(if clear { 1.0 } else { 0.0 }, 0));
        set_track(&mut fb, "標準描画", "拡大率", 50.0);
        s.insert_object(2, fb);
        s
    };
    // クリアなし: 元の絵の上に、半分に縮めた写しが重なる
    let f = r.render(&build(false), 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 10, 10), [255, 0, 0, 255], 1, "元の赤は残る");
    assert_near(px(&f, 30, 30), [255, 0, 0, 255], 1, "縮小した写しの赤（中心 + (-40, -40) × 0.5）");
    assert_near(px(&f, 5, 50), [0, 255, 0, 255], 1, "写しの外は元の背景");
    // クリアあり: 写しだけが残り、外は黒
    let f = r.render(&build(true), 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 10, 10), [0, 0, 0, 255], 1, "元の絵は消える");
    assert_near(px(&f, 30, 30), [255, 0, 0, 255], 1, "写しの赤");
    assert_near(px(&f, 50, 50), [0, 255, 0, 255], 1, "写しの緑");
    assert_near(px(&f, 5, 50), [0, 0, 0, 255], 1, "写しの外は黒");
}

#[test]
fn previous_object_redraws_the_previous_image_at_its_own_position() {
    let Some(mut r) = renderer() else { return };
    let mut s = scene(100, 50);
    let mut sq = shape(0, 9, 2, "ff00ff");
    set_track(&mut sq, "標準描画", "X", -25.0);
    set_track(&mut sq, "標準描画", "拡大率", 40.0); // 40x40
    s.insert_object(0, sq);
    let mut prev = TimelineObject::new_previous_object(0, 9);
    set_track(&mut prev, "標準描画", "X", 25.0);
    set_track(&mut prev, "標準描画", "拡大率", 20.0); // 20x20
    s.insert_object(1, prev);
    let f = r.render(&s, 0, &mut NoMedia).unwrap();
    assert_near(px(&f, 25, 25), [255, 0, 255, 255], 1, "元のオブジェクト");
    assert_near(px(&f, 75, 25), [255, 0, 255, 255], 1, "直前オブジェクトの位置に同じ絵");
    assert_near(px(&f, 75 + 8, 25), [255, 0, 255, 255], 1, "直前オブジェクト自身の拡大率（20x20）の内側");
    assert_near(px(&f, 75 + 12, 25), [0, 0, 0, 255], 1, "直前オブジェクト自身の拡大率（20x20）");
    assert_near(px(&f, 25 + 15, 25), [255, 0, 255, 255], 1, "元は 40x40 のまま");
}
