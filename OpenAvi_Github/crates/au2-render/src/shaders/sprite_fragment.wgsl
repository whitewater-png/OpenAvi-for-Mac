// スプライトのフラグメントシェーダ（テクスチャ/図形の色、合成モード）。所有: 合成/クリッピングの担当。

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let l = in.local;
    if d.mode == 0u {
        let cov = clamp(0.5 - sd_box(l, d.half), 0.0, 1.0);
        let uv = l / (2.0 * d.half) + vec2<f32>(0.5, 0.5);
        let t = textureSampleLevel(tex, samp, uv, 0.0);
        return vec4<f32>(t.rgb, t.a * cov * d.alpha);
    }
    var sd = 0.0;
    switch d.shape {
        case 0u: { sd = length(l) - d.half.x; }
        case 1u: { sd = sd_box(l, d.half); }
        case 2u: { sd = sd_polygon(l, d.half.x, d.sides); }
        default: { sd = sd_star(l, d.half.x); }
    }
    let cov = clamp(0.5 - sd, 0.0, 1.0);
    return vec4<f32>(d.color.rgb, cov * d.alpha);
}
