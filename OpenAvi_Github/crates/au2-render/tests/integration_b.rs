//! 段階 B の機能どうしの組み合わせ（合成モード・クリッピング・3D・テキスト・シーン）の GPU テスト。
//! 各機能は個別のテストファイルで検証済み。ここでは「同じシーンに同居しても互いを壊さない」ことを確かめる。
//! GPU が無ければスキップ。ゴールデン画像はフォント非依存（テキストを描かない構成）で作る。

use std::path::PathBuf;

use au2_core::{Effect, ParamValue, Project, Scene, SceneSettings, TimelineObject, Track};
use au2_render::{Gpu, NoMedia, Renderer, RgbaFrame};

const W: u32 = 480;
const H: u32 = 270;

fn renderer() -> Option<Renderer> {
    match Gpu::headless() {
        Ok(gpu) => Some(Renderer::new(gpu)),
        Err(e) => {
            eprintln!("GPU が無いためスキップします: {e}");
            None
        }
    }
}

fn px(f: &RgbaFrame, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * f.width + x) * 4) as usize;
    [f.pixels[i], f.pixels[i + 1], f.pixels[i + 2], f.pixels[i + 3]]
}

fn near(actual: [u8; 4], expected: [u8; 3], tol: u8, what: &str) {
    for c in 0..3 {
        assert!(actual[c].abs_diff(expected[c]) <= tol, "{what}: 期待 {expected:?} 実際 {actual:?}");
    }
}

fn set(obj: &mut TimelineObject, effect: &str, key: &str, value: f64) {
    let e = obj.effects.iter_mut().find(|e| e.name == effect).unwrap_or_else(|| panic!("{effect}"));
    *e.param_mut(key).unwrap_or_else(|| panic!("{key}")) = ParamValue::Track(Track::constant(value, 1));
}

fn shape(ty: u32, color: &str, size: f64, x: f64, y: f64) -> TimelineObject {
    let mut o = TimelineObject::new_shape(0, 9, ty, color);
    set(&mut o, "図形", "サイズ", size);
    set(&mut o, "標準描画", "X", x);
    set(&mut o, "標準描画", "Y", y);
    o
}

fn with_blend(mut o: TimelineObject, mode: &str) -> TimelineObject {
    let d = o.effects.iter_mut().find(|e| e.name == "標準描画").unwrap();
    *d.param_mut("blend").unwrap() = ParamValue::Text(mode.into());
    o
}

fn extended(mut obj: TimelineObject, params: &[(&str, f64)]) -> TimelineObject {
    obj.effects.retain(|e| e.name != "標準描画");
    let mut d = Effect::new("拡張描画");
    for (key, default) in [
        ("X", 0.0),
        ("Y", 0.0),
        ("Z", 0.0),
        ("拡大率", 100.0),
        ("透明度", 0.0),
        ("縦横比", 0.0),
        ("X軸回転", 0.0),
        ("Y軸回転", 0.0),
        ("Z軸回転", 0.0),
        ("中心X", 0.0),
        ("中心Y", 0.0),
        ("中心Z", 0.0),
        ("裏面を表示しない", 0.0),
    ] {
        let v = params.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(default);
        d.push_track(key, Track::constant(v, 2));
    }
    d.push_text("blend", "0");
    obj.effects.push(d);
    obj
}

fn text_object(body: &str) -> TimelineObject {
    let mut t = TimelineObject::new_text(0, 9, body);
    set(&mut t, "テキスト", "サイズ", 48.0);
    let src = &mut t.effects[0];
    *src.param_mut("type").unwrap() = ParamValue::Text("1".into()); // 影付き
    *src.param_mut("align").unwrap() = ParamValue::Text("4".into());
    *src.param_mut("color").unwrap() = ParamValue::Text("ffffff".into());
    *src.param_mut("color2").unwrap() = ParamValue::Text("ff0000".into());
    t
}

/// 子シーン（120x120、黄色一色）と、それを置いた親シーン。
fn project(with_text: bool) -> Project {
    let mut child = Scene::new("child", SceneSettings { width: 120, height: 120, length: 10, ..Default::default() });
    child.insert_object(0, TimelineObject::new_shape(0, 9, 0, "ffeb3b"));

    let mut root = Scene::new("root", SceneSettings { width: W, height: H, length: 10, ..Default::default() });
    root.insert_object(0, TimelineObject::new_shape(0, 9, 0, "1a237e")); // 背景
    root.insert_object(1, shape(1, "e94560", 160.0, -120.0, -20.0)); // 赤い円
    root.insert_object(2, with_blend(shape(2, "00c853", 160.0, -60.0, -20.0), "1")); // 緑の四角（加算）
    if with_text {
        root.insert_object(3, extended(text_object("OpenAvi"), &[("X", 110.0), ("Y", -60.0), ("Y軸回転", 35.0)]));
    }
    root.insert_object(4, shape(6, "ffeb3b", 100.0, 110.0, 80.0)); // クリッピング元の星（上のレイヤー）
    let mut clipped = shape(2, "ff4081", 160.0, 110.0, 80.0);
    clipped.clipping = true;
    root.insert_object(5, clipped); // 星の形にだけ見える四角
    let mut scene_obj = TimelineObject::new_scene(0, 9, 1); // 子シーン（Project.scenes[1]）
    set(&mut scene_obj, "標準描画", "X", -190.0);
    set(&mut scene_obj, "標準描画", "Y", 90.0);
    set(&mut scene_obj, "標準描画", "拡大率", 50.0);
    root.insert_object(6, scene_obj);

    let mut p = Project::new("integration", root);
    p.scenes.push(child);
    p
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.png"))
}

fn assert_golden(name: &str, frame: &RgbaFrame) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        image::save_buffer(&path, &frame.pixels, frame.width, frame.height, image::ColorType::Rgba8).unwrap();
        return;
    }
    let expected = image::open(&path)
        .unwrap_or_else(|e| panic!("{name}: ゴールデンイメージを開けません（UPDATE_GOLDEN=1 で作成）: {e}"))
        .to_rgba8();
    assert_eq!((expected.width(), expected.height()), (frame.width, frame.height));
    let bad = expected
        .as_raw()
        .chunks(4)
        .zip(frame.pixels.chunks(4))
        .filter(|(a, b)| a.iter().zip(b.iter()).any(|(x, y)| x.abs_diff(*y) > 3))
        .count();
    assert!(bad <= (frame.width * frame.height) as usize / 1000, "{name}: 許容を超える画素が {bad} 個");
}

#[test]
fn features_coexist_in_one_scene() {
    let Some(mut r) = renderer() else { return };
    let f = r.render_project(&project(false), 0, 0, &mut NoMedia).unwrap();
    // 背景
    near(px(&f, 5, 5), [26, 35, 126], 2, "背景");
    // 円と、加算された緑の四角の重なり: (233,69,96) + (0,200,83) → (233,255,179)
    near(px(&f, 150, 115), [233, 255, 179], 3, "円と加算の四角の重なり");
    // 円だけの部分
    near(px(&f, 60, 115), [233, 69, 96], 3, "円");
    // 四角だけの部分（背景に加算）
    near(px(&f, 230, 115), [26, 235, 209], 3, "背景への加算");
    // クリッピング: 星の中心はマゼンタ、星の外（四角の内側）は背景のまま
    near(px(&f, 350, 215), [255, 64, 129], 3, "星の形に切り抜かれた四角");
    near(px(&f, 280, 185), [26, 35, 126], 2, "星の外は四角が見えない");
    // 子シーンを置いたシーンオブジェクト（拡大率 50% → 60x60）
    near(px(&f, 50, 225), [255, 235, 59], 3, "子シーン");
    near(px(&f, 50 + 40, 225), [26, 35, 126], 2, "子シーンの外");
    assert_golden("integration_b", &f);
}

#[test]
fn text_with_decoration_and_3d_lands_where_the_object_is() {
    let Some(mut r) = renderer() else { return };
    if r.load_system_font().is_none() {
        eprintln!("日本語フォントが無いためスキップします");
        return;
    }
    let with = r.render_project(&project(true), 0, 0, &mut NoMedia).unwrap();
    let without = r.render_project(&project(false), 0, 0, &mut NoMedia).unwrap();
    // テキストの右上領域（X=110, Y=-60 → 画素 (350, 75) 付近）にだけ差が出る。他の場所（円・四角・クリッピング）は同じ
    let mut diff_in_text = 0;
    let mut diff_elsewhere = 0;
    for y in 0..H {
        for x in 0..W {
            if px(&with, x, y) != px(&without, x, y) {
                if (200..W).contains(&x) && y < 140 {
                    diff_in_text += 1;
                } else {
                    diff_elsewhere += 1;
                }
            }
        }
    }
    assert!(diff_in_text > 300, "テキストが描かれていない: {diff_in_text}");
    assert_eq!(diff_elsewhere, 0, "テキストの追加が他の場所の描画を変えた");
    // 影付き（color2 = 赤）の画素が出ている
    let red = with.pixels.chunks(4).filter(|p| p[0] > 200 && p[1] < 60 && p[2] < 60).count();
    assert!(red > 20, "影の色が出ていない: {red}");
}
