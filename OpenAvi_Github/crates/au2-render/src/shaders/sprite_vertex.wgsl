// スプライトの頂点シェーダ（位置・回転・拡大・3D/カメラ）。所有: 拡張描画/カメラの担当。

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

