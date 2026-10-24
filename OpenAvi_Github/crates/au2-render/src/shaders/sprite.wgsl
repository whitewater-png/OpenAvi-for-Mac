// スプライト描画: 1 オブジェクト = 1 クワッド。
// mode 0: テクスチャ（画像・動画フレーム） / mode 1: SDF 図形（解析的アンチエイリアス）
//
// 座標系: シーン中心が原点、+x 右、+y 下、単位はシーンのピクセル。

const PI: f32 = 3.14159265358979;
const TAU: f32 = 6.28318530717959;

struct Draw {
    canvas: vec2<f32>,
    center: vec2<f32>,
    half: vec2<f32>,
    angle: f32,
    alpha: f32,
    color: vec4<f32>,
    mode: u32,
    shape: u32,
    sides: u32,
    _pad: u32,
}

@group(0) @binding(0) var<uniform> d: Draw;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, 1.0),
    );
    // アンチエイリアスのために 1px 外側へ広げる
    let ext = d.half + vec2<f32>(1.0, 1.0);
    let local = corners[i] * ext;
    let s = sin(d.angle);
    let c = cos(d.angle);
    // y 下向き座標での正の回転 = 画面上で時計回り（AviUtl の回転方向）
    let rotated = vec2<f32>(local.x * c - local.y * s, local.x * s + local.y * c);
    let p = d.center + rotated;
    var out: VsOut;
    out.pos = vec4<f32>(p.x / (d.canvas.x * 0.5), -p.y / (d.canvas.y * 0.5), 0.0, 1.0);
    out.local = local;
    return out;
}

fn sd_box(p: vec2<f32>, h: vec2<f32>) -> f32 {
    let q = abs(p) - h;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
}

// 正多角形（頂点が真上）。内側は厳密、外側は近似（アンチエイリアスには十分）
fn sd_polygon(p: vec2<f32>, r: f32, n: u32) -> f32 {
    let apothem = r * cos(PI / f32(n));
    var sd = -1.0e9;
    for (var k = 0u; k < n; k = k + 1u) {
        let a = -PI * 0.5 + TAU * (f32(k) + 0.5) / f32(n);
        sd = max(sd, dot(p, vec2<f32>(cos(a), sin(a))) - apothem);
    }
    return sd;
}

fn seg_dist(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

// 5 つ星（外側頂点が真上、内側半径 0.4 倍）。凹形なので辺への距離＋交差回数で符号を決める
fn sd_star(p: vec2<f32>, r: f32) -> f32 {
    var dist = 1.0e9;
    var inside = false;
    for (var i = 0u; i < 10u; i = i + 1u) {
        let j = (i + 1u) % 10u;
        let ri = select(r * 0.4, r, i % 2u == 0u);
        let rj = select(r * 0.4, r, j % 2u == 0u);
        let ai = -PI * 0.5 + TAU * f32(i) / 10.0;
        let aj = -PI * 0.5 + TAU * f32(j) / 10.0;
        let a = vec2<f32>(cos(ai), sin(ai)) * ri;
        let b = vec2<f32>(cos(aj), sin(aj)) * rj;
        dist = min(dist, seg_dist(p, a, b));
        if ((a.y > p.y) != (b.y > p.y)) && (p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x) {
            inside = !inside;
        }
    }
    return select(dist, -dist, inside);
}

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
