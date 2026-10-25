
// スプライトの頂点シェーダ（位置・回転・拡大・3D/カメラ）。所有: 拡張描画/カメラの担当。
//
// d.ext（Rust 側 `eval::Homography::to_ext`）:
//   ext[0] = (m00, m01, m02, 1 = 射影変換を使う) / ext[1] = (m10, m11, m12, 0) / ext[2] = (m20, m21, m22, near) / ext[3] = 予備
//   物体ローカル座標 (x, y, 1) → 画面の同次座標 (X·w, Y·w, w)。w = 目からの奥行き / 焦点距離。
//   ext[0].w が 0 のときは center / angle による 2D 配置。
//
// フラグメントへ渡す local は常に「物体ローカルのピクセル座標」（拡大率は掛かった後、3D 回転・カメラ・縦横比の前）。
// 位置の w に奥行きが入るので、local（とそこから作る UV）は透視補正付きで補間される。

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, 1.0),
    );
    // アンチエイリアスのために 1px（ローカル座標で）外側へ広げる。被覆はローカル座標の SDF で決まるので、
    // 射影で縮んでも広がっても縁が欠けることはない
    let grown = d.half + vec2<f32>(1.0, 1.0);
    let local = corners[i] * grown;
    var out: VsOut;
    out.local = local;
    let half_canvas = d.canvas * 0.5;
    if d.ext[0].w > 0.5 {
        let q = vec3<f32>(local, 1.0);
        let h = vec3<f32>(dot(d.ext[0].xyz, q), dot(d.ext[1].xyz, q), dot(d.ext[2].xyz, q));
        // クリップ座標の z = w - near: 0 <= z <= w の範囲外（カメラの手前の打ち切り面より手前・後ろ）は GPU が切り取る
        out.pos = vec4<f32>(h.x / half_canvas.x, -h.y / half_canvas.y, h.z - d.ext[2].w, h.z);
        return out;
    }
    let s = sin(d.angle);
    let c = cos(d.angle);
    // y 下向き座標での正の回転 = 画面上で時計回り（AviUtl の回転方向）
    let rotated = vec2<f32>(local.x * c - local.y * s, local.x * s + local.y * c);
    let p = d.center + rotated;
    out.pos = vec4<f32>(p.x / half_canvas.x, -p.y / half_canvas.y, 0.0, 1.0);
    return out;
}
