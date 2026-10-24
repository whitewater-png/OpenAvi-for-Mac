//! 合成モード（標準描画・拡張描画の `blend`）と「上のオブジェクトでクリッピング」の判定。
//!
//! 式そのものは `shaders/sprite_fragment.wgsl` にあります。描画パスの組み立ては `renderer.rs` を参照。
//! 設計と近似の一覧は `docs/RENDER_DESIGN.md`「合成モード・クリッピング」。

use au2_core::{EffectKind, Frame, ObjectCategory, Scene, TimelineObject};

/// AviUtl（拡張編集 0.92）の合成モード。値は `.exo` の `blend=` の番号と同じ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u32)]
pub enum BlendMode {
    #[default]
    Normal = 0,
    Add = 1,
    Subtract = 2,
    Multiply = 3,
    Screen = 4,
    Overlay = 5,
    Lighten = 6,
    Darken = 7,
    Luminance = 8,
    ColorDifference = 9,
    Shadow = 10,
    LightDark = 11,
    Difference = 12,
}

impl BlendMode {
    /// 番号順（`.exo` の `blend=` の値順）の全モード。
    pub const ALL: [BlendMode; 13] = [
        Self::Normal,
        Self::Add,
        Self::Subtract,
        Self::Multiply,
        Self::Screen,
        Self::Overlay,
        Self::Lighten,
        Self::Darken,
        Self::Luminance,
        Self::ColorDifference,
        Self::Shadow,
        Self::LightDark,
        Self::Difference,
    ];

    /// `.exo` の番号（`"3"`）または AviUtl の表示名（`"乗算"`、`.aup2` の表記）から。不明なら `None`。
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if let Ok(n) = raw.parse::<u32>() {
            return Self::ALL.get(n as usize).copied();
        }
        Self::ALL.iter().copied().find(|m| m.name() == raw).or(match raw {
            "差" => Some(Self::Difference),
            _ => None,
        })
    }

    /// AviUtl の表示名。
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "通常",
            Self::Add => "加算",
            Self::Subtract => "減算",
            Self::Multiply => "乗算",
            Self::Screen => "スクリーン",
            Self::Overlay => "オーバーレイ",
            Self::Lighten => "比較(明)",
            Self::Darken => "比較(暗)",
            Self::Luminance => "輝度",
            Self::ColorDifference => "色差",
            Self::Shadow => "陰影",
            Self::LightDark => "明暗",
            Self::Difference => "差分",
        }
    }

    /// `.exo` の `blend=` の番号（シェーダに渡す値と同じ）。
    pub fn code(self) -> u32 {
        self as u32
    }

    /// オブジェクトの合成モード。標準描画（無ければ拡張描画）の `blend` を読む。未指定・不明は通常。
    pub fn of_object(obj: &TimelineObject) -> Self {
        [EffectKind::StandardDraw, EffectKind::ExtendedDraw]
            .iter()
            .filter_map(|k| obj.effect(k))
            .find_map(|e| match e.text("blend") {
                Some(raw) => Self::parse(raw),
                // 数値トラックとして読まれた場合（`.exo` 以外の経路）も受け付ける
                None => e.track("blend").map(|t| t.evaluate(0)).and_then(|v| Self::parse(&format!("{}", v.round() as i64))),
            })
            .unwrap_or_default()
    }
}

/// `obj`（レイヤー `li`）が「上のオブジェクトでクリッピング」するか。オブジェクトの `clipping=1` か、
/// レイヤー設定の「上のオブジェクトでクリッピング」（`Layer::clip_above`）のどちらか。
pub(crate) fn clips_to_upper(scene: &Scene, li: usize, obj: &TimelineObject) -> bool {
    obj.clipping || scene.layers.get(li).is_some_and(|l| l.clip_above)
}

/// レイヤー `li` のオブジェクトが、すぐ下の行（レイヤー `li + 1`）のオブジェクトのクリッピング元になるか。
/// その場合、このオブジェクトは自分の順番では描かれず、マスクとしてだけ使われる（docs/RENDER_DESIGN.md）。
pub(crate) fn is_clip_source(scene: &Scene, li: usize, frame: Frame) -> bool {
    let Some(layer) = scene.layers.get(li + 1).filter(|l| l.visible) else { return false };
    layer.objects.iter().filter(|o| o.contains(frame)).any(|o| {
        o.category() == ObjectCategory::Media
            && o.source().is_some_and(|s| s.enabled)
            && clips_to_upper(scene, li + 1, o)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use au2_core::Track;

    #[test]
    fn parses_codes_and_names() {
        for (i, m) in BlendMode::ALL.iter().enumerate() {
            assert_eq!(m.code(), i as u32);
            assert_eq!(BlendMode::parse(&i.to_string()), Some(*m));
            assert_eq!(BlendMode::parse(m.name()), Some(*m));
        }
        assert_eq!(BlendMode::parse(" 3 "), Some(BlendMode::Multiply));
        assert_eq!(BlendMode::parse("差"), Some(BlendMode::Difference));
        assert_eq!(BlendMode::parse("13"), None);
        assert_eq!(BlendMode::parse("unknown"), None);
    }

    #[test]
    fn reads_the_standard_draw_blend_text() {
        let mut obj = TimelineObject::new_shape(0, 9, 1, "ffffff");
        assert_eq!(BlendMode::of_object(&obj), BlendMode::Normal);
        let draw = obj.effects.iter_mut().find(|e| e.kind == EffectKind::StandardDraw).unwrap();
        *draw.param_mut("blend").unwrap() = au2_core::ParamValue::Text("4".into());
        assert_eq!(BlendMode::of_object(&obj), BlendMode::Screen);
        let draw = obj.effects.iter_mut().find(|e| e.kind == EffectKind::StandardDraw).unwrap();
        *draw.param_mut("blend").unwrap() = au2_core::ParamValue::Track(Track::constant(12.0, 0));
        assert_eq!(BlendMode::of_object(&obj), BlendMode::Difference);
    }

    #[test]
    fn clip_source_is_the_object_directly_above_a_clipping_object() {
        let mut obj = TimelineObject::new_shape(0, 9, 1, "ffffff");
        obj.clipping = true;
        let mut scene = Scene::new("t", Default::default());
        scene.insert_object(1, obj);
        scene.insert_object(0, TimelineObject::new_shape(0, 9, 1, "ffffff"));
        assert!(is_clip_source(&scene, 0, 0));
        assert!(!is_clip_source(&scene, 0, 10), "範囲外のフレーム");
        assert!(!is_clip_source(&scene, 1, 0), "下にクリッピングするオブジェクトが無い");
        scene.layers[1].visible = false;
        assert!(!is_clip_source(&scene, 0, 0), "非表示レイヤーのオブジェクトはクリッピングしない");
    }
}
