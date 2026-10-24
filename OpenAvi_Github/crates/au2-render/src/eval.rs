//! オブジェクトの「このフレームでの描画パラメータ」の評価。
//!
//! 標準描画（X / Y / 拡大率 / 回転 / 透明度）に、フェードと組み込みアニメーション効果を反映します。
//! プレビューと書き出しで同じ結果になるよう、評価はここに一本化します。

use std::f32::consts::TAU;

use au2_core::{EffectKind, Frame, TimelineObject};

/// 1 オブジェクト・1 フレーム分の描画パラメータ。座標はシーン中心基準のピクセル（+y 下）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawParams {
    pub x: f32,
    pub y: f32,
    /// 1.0 = 等倍
    pub zoom: f32,
    /// ラジアン（正 = 時計回り）
    pub angle: f32,
    /// 不透明度 0.0〜1.0
    pub alpha: f32,
}

/// `rel` はオブジェクト先頭からの相対フレーム。
pub fn eval_draw_params(obj: &TimelineObject, rel: Frame, fps: f64) -> DrawParams {
    let draw = obj.effect(&EffectKind::StandardDraw);
    let ev = |key: &str, default: f64| draw.map(|d| d.eval(key, rel, default)).unwrap_or(default);
    let mut p = DrawParams {
        x: ev("X", 0.0) as f32,
        y: ev("Y", 0.0) as f32,
        zoom: (ev("拡大率", 100.0) / 100.0) as f32,
        angle: (ev("回転", 0.0) as f32).to_radians(),
        alpha: (1.0 - ev("透明度", 0.0) / 100.0).clamp(0.0, 1.0) as f32,
    };

    let fps = fps.max(1.0);
    let time_sec = rel as f64 / fps;
    for eff in obj.effects.iter().filter(|e| e.enabled) {
        if eff.name == "フェード" {
            let fade_in = eff.eval("イン", rel, 0.0) * fps;
            let fade_out = eff.eval("アウト", rel, 0.0) * fps;
            let remain = (obj.length() - 1 - rel) as f64;
            if fade_in > 0.0 {
                p.alpha *= (rel as f64 / fade_in).min(1.0) as f32;
            }
            if fade_out > 0.0 {
                p.alpha *= (remain / fade_out).min(1.0) as f32;
            }
        } else if eff.name == "アニメーション効果" {
            let script = eff.text("name").unwrap_or_default();
            let name = script.split('@').next().unwrap_or(script);
            let t = time_sec as f32;
            match name {
                "揺れ" | "振動" => {
                    let ax = eff.eval("X振幅", rel, 25.0) as f32;
                    let ay = eff.eval("Y振幅", rel, 15.0) as f32;
                    let period = eff.eval("周期", rel, 0.4).max(0.01) as f32;
                    let phase = (t / period) * TAU;
                    p.x += phase.sin() * ax;
                    p.y += (phase * 1.3).cos() * ay;
                }
                "点滅" => {
                    let period = eff.eval("周期", rel, 0.5).max(0.01) as f32;
                    let min_alpha = (eff.eval("最小透明度", rel, 60.0) / 100.0).clamp(0.0, 1.0) as f32;
                    let w = (((t / period) * TAU).sin() + 1.0) * 0.5;
                    p.alpha *= 1.0 - min_alpha * w;
                }
                "回転" => {
                    let deg_per_sec = eff.eval("角速度", rel, 90.0) as f32;
                    p.angle += (t * deg_per_sec).to_radians();
                }
                _ => {}
            }
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use au2_core::{Effect, Track};

    #[test]
    fn defaults_are_identity() {
        let obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        let p = eval_draw_params(&obj, 10, 30.0);
        assert_eq!(p, DrawParams { x: 0.0, y: 0.0, zoom: 1.0, angle: 0.0, alpha: 1.0 });
    }

    #[test]
    fn fade_in_ramps_alpha() {
        let mut obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        let mut fade = Effect::new("フェード");
        fade.push_track("イン", Track::constant(1.0, 1)); // 1 秒 = 30 フレーム
        fade.push_track("アウト", Track::constant(0.0, 1));
        obj.effects.push(fade);
        assert!((eval_draw_params(&obj, 15, 30.0).alpha - 0.5).abs() < 1e-6);
        assert!((eval_draw_params(&obj, 60, 30.0).alpha - 1.0).abs() < 1e-6);
    }

    #[test]
    fn rotation_effect_advances_with_time() {
        let mut obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        let mut anim = Effect::new("アニメーション効果");
        anim.push_text("name", "回転@openavi_effects");
        anim.push_track("角速度", Track::constant(90.0, 1));
        obj.effects.push(anim);
        let p = eval_draw_params(&obj, 30, 30.0); // 1 秒後 = 90 度
        assert!((p.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }
}
