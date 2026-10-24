//! 時間制御とシーンの入れ子の、時間の計算（描画・音声で共有する純粋関数）。
//!
//! - **時間制御**: 下のレイヤー（`layers` の添字が大きい側。グループ制御と同じ向き）のオブジェクトが見る時刻を置き換える。
//!   制御下のレイヤーは「置き換え後のフレーム」に存在するオブジェクトを、そのフレームの相対位置で評価する
//!   （トラック・動画の再生位置がすべてずれる）。
//! - **シーン / シーン(音声)**: 別シーンの何フレーム目を出すか。
//!
//! 実 AviUtl の式が分からない部分は近似（`docs/RENDER_DESIGN.md` の未決事項を参照）。

use crate::effect::{Effect, EffectKind};
use crate::object::TimelineObject;
use crate::project::{Scene, SceneSettings};
use crate::time::Frame;
use crate::track::{Interpolation, Keyframe, Track};

/// 浮動小数の誤差で 1 フレーム手前に落ちないための余裕。
const EPS: f64 = 1e-6;

impl TimelineObject {
    /// 時間制御オブジェクトを生成します。既定は「位置」0 → 100% の直線移動（= 時間をそのまま通す）。
    pub fn new_time_control(start: Frame, end: Frame) -> Self {
        let mut obj = Self::new(start, end);
        let last = obj.length() - 1;
        let mut src = Effect::new("時間制御");
        src.push_track("位置", linear_track(0.0, 100.0, last));
        src.push_track("繰り返し", Track::constant(1.0, 0));
        src.push_track("コマ落ち", Track::constant(1.0, 0));
        src.push_text("range", "0");
        obj.effects.push(src);
        obj
    }

    /// シーンオブジェクト（`scene` 番目のシーンを表示する）を生成します。`scene` は `Project::scenes` の添字（0 = Root）。
    pub fn new_scene(start: Frame, end: Frame, scene: usize) -> Self {
        let mut obj = Self::new(start, end);
        let mut src = Effect::new("シーン");
        src.push_track("再生位置", Track::constant(1.0, 0));
        src.push_track("再生速度", Track::constant(100.0, 1));
        src.push_track("ループ再生", Track::constant(0.0, 0));
        src.push_text("scene", scene.to_string());
        obj.effects.push(src);
        let draw = TimelineObject::new_shape(start, end, 1, "ffffff").effects.pop().expect("標準描画");
        obj.effects.push(draw);
        obj
    }

    /// シーン(音声)オブジェクトを生成します。
    pub fn new_scene_audio(start: Frame, end: Frame, scene: usize) -> Self {
        let mut obj = Self::new(start, end);
        obj.audio = true;
        obj.extra.clear();
        let mut src = Effect::new("シーン(音声)");
        src.push_track("再生位置", Track::constant(1.0, 0));
        src.push_track("再生速度", Track::constant(100.0, 1));
        src.push_track("ループ再生", Track::constant(0.0, 0));
        src.push_text("scene", scene.to_string());
        obj.effects.push(src);
        let play = TimelineObject::new_audio(start, end, "").effects.pop().expect("標準再生");
        obj.effects.push(play);
        obj
    }

    /// 直前オブジェクトを生成します。
    pub fn new_previous_object(start: Frame, end: Frame) -> Self {
        let mut obj = Self::new(start, end);
        obj.effects.push(Effect::new("直前オブジェクト"));
        let draw = TimelineObject::new_shape(start, end, 1, "ffffff").effects.pop().expect("標準描画");
        obj.effects.push(draw);
        obj
    }
}

fn linear_track(from: f64, to: f64, last: Frame) -> Track {
    if last == 0 {
        return Track::constant(from, 1);
    }
    Track {
        keyframes: vec![Keyframe { frame: 0, value: from }, Keyframe { frame: last, value: to }],
        interpolation: Some(Interpolation::linear()),
        precision: 1,
    }
}

/// 時間制御オブジェクト `ctrl` の下で、絶対フレーム `frame` が置き換わる先の絶対フレーム。
/// `frame` が `ctrl` の範囲外なら `frame` のまま。
///
/// 近似（実 AviUtl の式は未確認）:
/// 1. コマ落ち `k`: 制御オブジェクト内の相対位置を `k` フレーム単位に切り捨てる（`k` フレームずつ同じ絵）
/// 2. 位置 `p`[%]: 制御オブジェクトの長さに対する割合。`t = p / 100 × (長さ - 1)`。
///    既定の 0 → 100 の直線移動で恒等、100 → 0 で逆再生。トラックが無ければ恒等
/// 3. 繰り返し `n`（2 以上）: `t` を `長さ / n` で巻き戻す（先頭の 1/n 区間を、等速のまま n 回繰り返す）
/// 4. 結果が負なら制御オブジェクトの先頭に揃える
pub fn time_control_frame(ctrl: &TimelineObject, frame: Frame) -> Frame {
    let Some(src) = ctrl.source() else { return frame };
    if !ctrl.contains(frame) {
        return frame;
    }
    let len = ctrl.length();
    let last = (len - 1) as f64;
    let rel = frame - ctrl.frame_start;
    let step = src.eval("コマ落ち", rel, 1.0).round().max(1.0) as Frame;
    let rel = rel - rel % step;
    let pos = match src.track("位置") {
        Some(track) => track.evaluate(rel) / 100.0 * last,
        None => rel as f64,
    };
    let repeat = src.eval("繰り返し", rel, 1.0).round().max(1.0);
    let pos = if repeat > 1.0 { pos.rem_euclid(len as f64 / repeat) } else { pos };
    let t = (pos + EPS).floor().max(0.0);
    (ctrl.frame_start as f64 + t).min(Frame::MAX as f64) as Frame
}

/// 時間制御が及ぶ下のレイヤー数（0 = 下のレイヤーすべて）。`range` 設定（グループ制御と同じ）か「対象レイヤー数」。
pub fn control_range(src: &Effect) -> usize {
    src.text("range")
        .and_then(|t| t.trim().parse::<f64>().ok())
        .or_else(|| src.track("対象レイヤー数").map(|t| t.start_value()))
        .map_or(0, |v| v.max(0.0) as usize)
}

/// 各レイヤーが見るフレーム（時間制御を適用した後の絶対フレーム）。長さは `scene.layers.len()`。
///
/// レイヤー 1 から順に見て、表示中のレイヤーにある有効な時間制御が、その下のレイヤー（範囲内）の時刻を置き換える。
/// 時間制御そのものも、自分のレイヤーが見る（上の時間制御で置き換え済みの）フレームで評価するので、入れ子は合成される。
/// 範囲が重なる場合は、下にある（後から現れた）時間制御が優先される。
pub fn layer_frames(scene: &Scene, frame: Frame) -> Vec<Frame> {
    // (この添字まで有効, 置き換え後のフレーム)
    let mut remaps: Vec<(usize, Frame)> = Vec::new();
    let mut out = Vec::with_capacity(scene.layers.len());
    for (li, layer) in scene.layers.iter().enumerate() {
        remaps.retain(|(until, _)| li <= *until);
        let f = remaps.last().map_or(frame, |(_, f)| *f);
        out.push(f);
        if !layer.visible {
            continue;
        }
        let ctrl = layer.objects.iter().find(|o| {
            o.contains(f) && o.source().is_some_and(|s| s.enabled && s.kind == EffectKind::TimeControl)
        });
        if let Some(ctrl) = ctrl {
            let range = control_range(ctrl.source().expect("source を確認済み"));
            let until = if range == 0 { usize::MAX } else { li.saturating_add(range) };
            remaps.push((until, time_control_frame(ctrl, f)));
        }
    }
    out
}

/// 時間制御を適用したうえで、描画順（レイヤー1 → 100）に (レイヤー番号, オブジェクト, そのレイヤーが見る絶対フレーム) を列挙します。
/// 時間制御が無ければ [`Scene::objects_at`] と同じ並び。
pub fn objects_at_remapped(scene: &Scene, frame: Frame) -> impl Iterator<Item = (usize, &TimelineObject, Frame)> {
    let frames = layer_frames(scene, frame);
    scene.layers.iter().enumerate().flat_map(move |(li, layer)| {
        let f = frames[li];
        layer.objects.iter().filter(move |o| o.contains(f)).map(move |o| (li, o, f))
    })
}

/// シーン / シーン(音声) オブジェクトが参照するシーンの添字。`scene` 設定は `Project::scenes` の添字（0 = Root、1.. = Scene1..）。
/// 数値でなければシーン名として探す。
pub fn scene_index(src: &Effect, scenes: &[Scene]) -> Option<usize> {
    let index = match src.text("scene") {
        Some(text) => match text.trim().parse::<usize>() {
            Ok(i) => Some(i),
            Err(_) => scenes.iter().position(|s| s.name == text.trim()),
        },
        None => src.track("scene").map(|t| t.start_value().max(0.0) as usize),
    }?;
    (index < scenes.len()).then_some(index)
}

/// シーンオブジェクトの再生設定（その相対フレームでの値）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScenePlayback {
    /// 子シーンの開始フレーム（0 始まり。`再生位置` は 1 始まり）
    pub start: f64,
    /// 倍率（1.0 = 等速）。負にはしない
    pub speed: f64,
    pub looping: bool,
}

impl ScenePlayback {
    pub fn of(src: &Effect, rel: Frame) -> Self {
        Self {
            start: (src.eval("再生位置", rel, 1.0) - 1.0).max(0.0),
            speed: (src.eval("再生速度", rel, 100.0) / 100.0).max(0.0),
            looping: src.eval("ループ再生", rel, 0.0) >= 0.5,
        }
    }
}

/// シーンオブジェクトの相対フレーム `rel` で表示する、子シーンのフレーム。
///
/// 動画ファイルと同じく `再生位置 + rel × 再生速度`（親と子のフレームレートが違えば秒で換算）。
/// 子シーンの長さを越えたら、ループ再生なら先頭へ巻き戻し、そうでなければ最後のフレームで止める（近似。動画ファイルの終端と同じ扱い）。
pub fn scene_object_frame(src: &Effect, rel: Frame, parent_fps: f64, child: &SceneSettings) -> Frame {
    let pb = ScenePlayback::of(src, rel);
    let ratio = child.frame_rate.as_f64().max(1.0) / parent_fps.max(1.0);
    let f = (pb.start + rel as f64 * pb.speed * ratio + EPS).floor().max(0.0) as u64;
    let len = child.length as u64;
    let f = match (len, pb.looping) {
        (0, _) => f,
        (len, true) => f % len,
        (len, false) => f.min(len - 1),
    };
    f.min(Frame::MAX as u64) as Frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::MAX_LAYERS;

    fn set(obj: &mut TimelineObject, key: &str, track: Track) {
        *obj.effects[0].param_mut(key).unwrap() = crate::ParamValue::Track(track);
    }

    #[test]
    fn huge_range_does_not_overflow() {
        let mut scene = Scene::new("s", crate::SceneSettings { width: 10, height: 10, length: 10, ..Default::default() });
        let mut ctrl = TimelineObject::new_time_control(0, 9);
        *ctrl.effects[0].param_mut("range").unwrap() = crate::ParamValue::Text("1e30".into());
        scene.insert_object(1, ctrl);
        scene.insert_object(2, TimelineObject::new_shape(0, 9, 0, "ffffff"));
        assert_eq!(layer_frames(&scene, 3).len(), scene.layers.len());
    }

    #[test]
    fn default_time_control_is_identity() {
        let ctrl = TimelineObject::new_time_control(10, 40);
        for f in [10, 11, 25, 39, 40] {
            assert_eq!(time_control_frame(&ctrl, f), f);
        }
        assert_eq!(time_control_frame(&ctrl, 5), 5, "範囲外はそのまま");
        assert_eq!(time_control_frame(&ctrl, 41), 41);
    }

    #[test]
    fn reversed_position_plays_backwards() {
        let mut ctrl = TimelineObject::new_time_control(10, 40); // 長さ 31
        set(&mut ctrl, "位置", linear_track(100.0, 0.0, 30));
        assert_eq!(time_control_frame(&ctrl, 10), 40);
        assert_eq!(time_control_frame(&ctrl, 11), 39);
        assert_eq!(time_control_frame(&ctrl, 40), 10);
    }

    #[test]
    fn constant_position_freezes_and_scales_with_length() {
        let mut ctrl = TimelineObject::new_time_control(0, 100); // 長さ 101 → 50% = 50 フレーム目
        set(&mut ctrl, "位置", Track::constant(50.0, 1));
        assert_eq!(time_control_frame(&ctrl, 0), 50);
        assert_eq!(time_control_frame(&ctrl, 90), 50);
        // 負の位置は先頭に揃える
        set(&mut ctrl, "位置", Track::constant(-20.0, 1));
        assert_eq!(time_control_frame(&ctrl, 30), 0);
    }

    #[test]
    fn frame_drop_holds_each_frame() {
        let mut ctrl = TimelineObject::new_time_control(0, 30);
        set(&mut ctrl, "コマ落ち", Track::constant(3.0, 0));
        let got: Vec<Frame> = (0..8).map(|f| time_control_frame(&ctrl, f)).collect();
        assert_eq!(got, [0, 0, 0, 3, 3, 3, 6, 6]);
    }

    #[test]
    fn repeat_loops_the_first_part() {
        let mut ctrl = TimelineObject::new_time_control(100, 129); // 長さ 30、3 回 → 10 フレームずつ
        set(&mut ctrl, "繰り返し", Track::constant(3.0, 0));
        assert_eq!(time_control_frame(&ctrl, 100), 100);
        assert_eq!(time_control_frame(&ctrl, 109), 109);
        assert_eq!(time_control_frame(&ctrl, 110), 100);
        assert_eq!(time_control_frame(&ctrl, 125), 105);
    }

    fn scene_with(objects: Vec<(usize, TimelineObject)>) -> Scene {
        let mut s = Scene::new("t", SceneSettings::default());
        for (li, o) in objects {
            s.insert_object(li, o);
        }
        s
    }

    #[test]
    fn layer_frames_apply_only_below_the_control_and_within_range() {
        let mut ctrl = TimelineObject::new_time_control(0, 99);
        set(&mut ctrl, "位置", Track::constant(10.0, 1)); // 常に 9.9 → 9 フレーム目
        *ctrl.effects[0].param_mut("range").unwrap() = crate::ParamValue::Text("2".into());
        let s = scene_with(vec![(3, ctrl)]);
        let f = layer_frames(&s, 50);
        assert_eq!(f.len(), MAX_LAYERS);
        assert_eq!(&f[..7], &[50, 50, 50, 50, 9, 9, 50], "制御の上・自分自身は元の時刻、下の 2 レイヤーだけ置き換え");

        // range=0 は下のレイヤーすべて
        let mut s = s;
        *s.layers[3].objects[0].effects[0].param_mut("range").unwrap() = crate::ParamValue::Text("0".into());
        assert!(layer_frames(&s, 50)[4..].iter().all(|&f| f == 9));
        // 範囲外の時刻・非表示レイヤー・無効化された時間制御は効かない
        assert!(layer_frames(&s, 120).iter().all(|&f| f == 120));
        s.layers[3].visible = false;
        assert!(layer_frames(&s, 50).iter().all(|&f| f == 50));
        s.layers[3].visible = true;
        s.layers[3].objects[0].effects[0].enabled = false;
        assert!(layer_frames(&s, 50).iter().all(|&f| f == 50));
    }

    #[test]
    fn nested_time_controls_compose() {
        // レイヤー1: 逆再生（0..=99）、レイヤー3: 10 フレームずらす（範囲 0..=99 の位置を +10%）
        let mut reverse = TimelineObject::new_time_control(0, 99);
        set(&mut reverse, "位置", linear_track(100.0, 0.0, 99));
        let mut shift = TimelineObject::new_time_control(0, 99);
        // 位置 p% → p/100*99。p = 0 + rel → 恒等に近いので、固定 50%（49 フレーム目）で止める
        set(&mut shift, "位置", Track::constant(50.0, 1));
        let s = scene_with(vec![(0, reverse), (2, shift)]);
        let f = layer_frames(&s, 10);
        assert_eq!(f[0], 10);
        assert_eq!(f[1], 89, "逆再生の下");
        assert_eq!(f[2], 89, "2 つ目の時間制御は逆再生後の時刻で評価される");
        assert_eq!(f[3], 49, "下の時間制御が優先");
    }

    #[test]
    fn remapped_iteration_finds_objects_at_the_remapped_time() {
        let mut ctrl = TimelineObject::new_time_control(0, 99);
        set(&mut ctrl, "位置", Track::constant(80.0, 1)); // 79 フレーム目
        let late = TimelineObject::new_shape(70, 99, 1, "ffffff");
        let late_id = late.id;
        let s = scene_with(vec![(0, ctrl), (1, late)]);
        // 元の時刻 5 には layer1 のオブジェクトは無いが、置き換え後の 79 にはある
        let hits: Vec<_> = objects_at_remapped(&s, 5).map(|(li, o, f)| (li, o.id, f)).collect();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1], (1, late_id, 79));
        // 時間制御が無いシーンでは objects_at と同じ
        let plain = scene_with(vec![(0, TimelineObject::new_shape(0, 9, 1, "ffffff"))]);
        assert_eq!(objects_at_remapped(&plain, 3).count(), plain.objects_at(3).count());
    }

    #[test]
    fn scene_index_by_number_or_name() {
        let scenes = vec![Scene::default(), Scene::new("Scene1", SceneSettings::default())];
        let obj = TimelineObject::new_scene(0, 9, 1);
        assert_eq!(scene_index(obj.source().unwrap(), &scenes), Some(1));
        let mut e = Effect::new("シーン");
        e.push_text("scene", "Scene1");
        assert_eq!(scene_index(&e, &scenes), Some(1));
        let mut e = Effect::new("シーン");
        e.push_text("scene", "7");
        assert_eq!(scene_index(&e, &scenes), None, "存在しない番号");
    }

    #[test]
    fn scene_object_frame_follows_start_speed_and_loop() {
        let child = SceneSettings { length: 20, ..Default::default() };
        let mut obj = TimelineObject::new_scene(0, 99, 1);
        assert_eq!(scene_object_frame(obj.source().unwrap(), 7, 30.0, &child), 7);
        assert_eq!(scene_object_frame(obj.source().unwrap(), 50, 30.0, &child), 19, "終端で止まる");
        set(&mut obj, "再生位置", Track::constant(5.0, 0));
        set(&mut obj, "再生速度", Track::constant(200.0, 1));
        assert_eq!(scene_object_frame(obj.source().unwrap(), 3, 30.0, &child), 4 + 6);
        set(&mut obj, "ループ再生", Track::constant(1.0, 0));
        assert_eq!(scene_object_frame(obj.source().unwrap(), 10, 30.0, &child), (4 + 20) % 20);
        // 子シーンが 60fps なら、親の 1 フレームは子の 2 フレーム
        let fast = SceneSettings { length: 0, frame_rate: crate::Rational::new(60, 1), ..Default::default() };
        let obj = TimelineObject::new_scene(0, 99, 1);
        assert_eq!(scene_object_frame(obj.source().unwrap(), 10, 30.0, &fast), 20);
    }
}
