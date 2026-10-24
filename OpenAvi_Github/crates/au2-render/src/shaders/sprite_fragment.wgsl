// スプライトのフラグメントシェーダ（テクスチャ/図形の色、合成モード、クリッピング）。所有: 合成/クリッピングの担当。
//
// エントリポイント:
//   fs      … 通常合成・クリッピング無し（高速経路。グループ 2 を使わない）
//   fs_ext  … 合成モード ≠ 通常、またはクリッピングあり。グループ 2 の dst / clip_mask を読む
//   fs_mask … クリッピング元（上のオブジェクト）の不透明度をマスク（R8）へ書く
//
// d.blend.x: 合成モード（.exo の blend= の番号。0 = 通常）
// d.blend.y: 1 = clip_mask の値を不透明度に掛ける（上のオブジェクトでクリッピング）
//
// 合成の定義（ガンマ空間のまま。docs/RENDER_DESIGN.md「合成モード・クリッピング」）:
//   B  = blend_rgb(mode, Cs, Cd)            … 下地 Cd・上 Cs（ストレートアルファ）の各チャンネルの式
//   B' = mix(Cs, B, Ad)                     … 下地が透明（Ad < 1）な所は通常合成に寄せる（W3C compositing と同じ）
//   出力 = (B', As) をハードウェアの「over」で合成 → 結果 = mix(Cd, B', As)。つまり上の不透明度が効果の強さになる

@group(2) @binding(0) var dst_tex: texture_2d<f32>;
@group(2) @binding(1) var clip_mask: texture_2d<f32>;

fn sprite_color(in: VsOut) -> vec4<f32> {
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

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return sprite_color(in);
}

@fragment
fn fs_mask(in: VsOut) -> @location(0) vec4<f32> {
    let a = sprite_color(in).a;
    return vec4<f32>(a, a, a, a);
}

// BT.601（フルレンジ）の YCbCr。AviUtl 内部の YC48 と同じ係数
fn to_ycbcr(c: vec3<f32>) -> vec3<f32> {
    let y = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    return vec3<f32>(y, (c.b - y) * 0.564334, (c.r - y) * 0.713267);
}

fn from_ycbcr(v: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        v.x + 1.402 * v.z,
        v.x - 0.344136 * v.y - 0.714136 * v.z,
        v.x + 1.772 * v.y,
    );
}

// s = 上（このオブジェクト）、b = 下地。結果は 0〜1 に収める
fn blend_rgb(mode: u32, s: vec3<f32>, b: vec3<f32>) -> vec3<f32> {
    let one = vec3<f32>(1.0);
    var r = s;
    switch mode {
        // 加算
        case 1u: { r = s + b; }
        // 減算: 下地から引く
        case 2u: { r = b - s; }
        // 乗算
        case 3u: { r = s * b; }
        // スクリーン
        case 4u: { r = one - (one - s) * (one - b); }
        // オーバーレイ: 下地が暗い所は乗算、明るい所はスクリーン（下地 0.5 が境目）
        case 5u: { r = select(one - 2.0 * (one - s) * (one - b), 2.0 * s * b, b < vec3<f32>(0.5)); }
        // 比較(明)
        case 6u: { r = max(s, b); }
        // 比較(暗)
        case 7u: { r = min(s, b); }
        // 輝度: 上の輝度 Y と、下地の色差 CbCr ⚠ 近似
        case 8u: { r = from_ycbcr(vec3<f32>(to_ycbcr(s).x, to_ycbcr(b).yz)); }
        // 色差: 下地の輝度 Y と、上の色差 CbCr ⚠ 近似
        case 9u: { r = from_ycbcr(vec3<f32>(to_ycbcr(b).x, to_ycbcr(s).yz)); }
        // 陰影: 焼き込みリニア ⚠ 近似
        case 10u: { r = s + b - one; }
        // 明暗: リニアライト ⚠ 近似
        case 11u: { r = b + 2.0 * s - one; }
        // 差分
        case 12u: { r = abs(s - b); }
        default: {}
    }
    return clamp(r, vec3<f32>(0.0), one);
}

@fragment
fn fs_ext(in: VsOut) -> @location(0) vec4<f32> {
    var c = sprite_color(in);
    let pixel = vec2<i32>(in.pos.xy);
    if d.blend.y != 0u {
        c.a = c.a * textureLoad(clip_mask, pixel, 0).r;
    }
    if d.blend.x != 0u {
        let dst = textureLoad(dst_tex, pixel, 0);
        c = vec4<f32>(mix(c.rgb, blend_rgb(d.blend.x, c.rgb, dst.rgb), dst.a), c.a);
    }
    return c;
}
