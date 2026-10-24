// フィルタオブジェクト「色調補正」: 下位レイヤーの合成結果全体にかかる全画面パス。
//
// ⚠ 各項目の式は試作用の近似。実 AviUtl の参照画像で確定させる（docs/RENDER_DESIGN.md「未決事項」）。

struct Params {
    // x: 明るさ(倍率) y: コントラスト(倍率) z: 色相(ラジアン) w: 輝度(倍率)
    a: vec4<f32>,
    // x: 彩度(倍率)
    b: vec4<f32>,
}

@group(0) @binding(0) var<uniform> p: Params;
@group(1) @binding(0) var src: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // 画面全体を覆う 1 枚の三角形
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pos[i], 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) frag: vec4<f32>) -> @location(0) vec4<f32> {
    let px = textureLoad(src, vec2<i32>(frag.xy), 0);
    var c = px.rgb * p.a.x;
    c = (c - vec3<f32>(0.5)) * p.a.y + vec3<f32>(0.5);

    // 色相・輝度・彩度が既定値なら YIQ 変換（係数の丸め誤差が出る）を通さない
    if p.a.z == 0.0 && p.a.w == 1.0 && p.b.x == 1.0 {
        return vec4<f32>(clamp(c, vec3<f32>(0.0), vec3<f32>(1.0)), px.a);
    }

    // YIQ で色相回転・輝度・彩度を処理
    let y = dot(c, vec3<f32>(0.299, 0.587, 0.114));
    let i = dot(c, vec3<f32>(0.596, -0.274, -0.322));
    let q = dot(c, vec3<f32>(0.211, -0.523, 0.312));
    let cs = cos(p.a.z);
    let sn = sin(p.a.z);
    let i2 = (i * cs - q * sn) * p.b.x;
    let q2 = (i * sn + q * cs) * p.b.x;
    let y2 = y * p.a.w;
    let rgb = vec3<f32>(
        y2 + 0.956 * i2 + 0.621 * q2,
        y2 - 0.272 * i2 - 0.647 * q2,
        y2 - 1.106 * i2 + 1.703 * q2,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), px.a);
}
