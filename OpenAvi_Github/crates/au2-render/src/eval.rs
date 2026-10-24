//! オブジェクトの「このフレームでの描画パラメータ」の評価。
//!
//! 標準描画 / 拡張描画（X / Y / Z / 拡大率 / 透明度 / 縦横比 / XYZ 軸回転 / 中心 / 裏面を表示しない）に、
//! フェードと組み込みアニメーション効果、グループ制御・カメラ制御を反映します。
//! プレビューと書き出しで同じ結果になるよう、評価はここに一本化します。
//!
//! ## 3D の座標系と射影
//!
//! - ワールド座標: シーン中心が原点、+x 右、+y 下、+z 奥（AviUtl と同じ）。単位はシーンのピクセル。
//! - 回転は 3 軸とも右手系の標準の回転行列（各軸の正の向きへ見たとき時計回り。Z 軸回転 = 画面上で時計回り）。
//!   合成順は `Rx · Ry · Rz`（物体には Z 軸回転 → Y 軸回転 → X 軸回転の順に掛かる）。**近似**（実 AviUtl の順序は未確認）。
//! - 射影: カメラ制御が無いときも、目の位置 z = -1024、z = 0 の面が等倍になる透視投影をかける
//!   （AviUtl の慣例。Z = 1024 で半分の大きさに見える）。カメラ制御があれば、その位置・目標・傾き・視野角で置き換える。
//! - 1 つのスプライト（平面のクワッド）の射影は、物体ローカル座標 → 画面座標の 3x3 の射影変換（[`Homography`]）で表せる。
//!   これを `SpriteUniform::ext` に詰めて頂点シェーダへ渡す（ローカル座標 `local` は透視補正付きで補間される）。

use std::f32::consts::TAU;

use au2_core::{EffectKind, Frame, ObjectCategory, TimelineObject};

/// 3 成分ベクトル。
pub type Vec3 = [f32; 3];
/// 3x3 行列（行優先: `m[行][列]`、列ベクトルに左から掛ける）。
pub type Mat3 = [[f32; 3]; 3];

/// 透視の基準距離（px）。カメラ制御が無いときの目の位置は z = -1024、焦点距離も 1024（z = 0 の面が等倍）。
pub const PERSPECTIVE_DISTANCE: f32 = 1024.0;
/// カメラの手前の打ち切り面（目からの距離 px）。これより手前・後ろの部分は描かない。
pub const NEAR_DISTANCE: f32 = 1.0;

const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    m
}

fn mat_vec(a: &Mat3, v: Vec3) -> Vec3 {
    [0, 1, 2].map(|r| a[r][0] * v[0] + a[r][1] * v[1] + a[r][2] * v[2])
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn length(a: Vec3) -> f32 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

fn rot_x(a: f32) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn rot_y(a: f32) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

fn rot_z(a: f32) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// X / Y / Z 軸回転（ラジアン）の合成。物体には Z → Y → X の順に掛かる（`Rx · Ry · Rz`）。
pub fn rotation_xyz(rx: f32, ry: f32, rz: f32) -> Mat3 {
    mat_mul(&rot_x(rx), &mat_mul(&rot_y(ry), &rot_z(rz)))
}

/// グループ制御が子に掛ける変換: `p ↦ offset + scale · rot · p`。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupTransform {
    pub offset: Vec3,
    pub rot: Mat3,
    pub scale: f32,
}

impl GroupTransform {
    pub const IDENTITY: Self = Self { offset: [0.0; 3], rot: IDENTITY, scale: 1.0 };

    pub fn apply_point(&self, p: Vec3) -> Vec3 {
        add(self.offset, scale(mat_vec(&self.rot, p), self.scale))
    }

    /// `self ∘ inner`（先に `inner`、次に `self`）。
    pub fn compose(&self, inner: &GroupTransform) -> GroupTransform {
        GroupTransform {
            offset: self.apply_point(inner.offset),
            rot: mat_mul(&self.rot, &inner.rot),
            scale: self.scale * inner.scale,
        }
    }
}

impl Default for GroupTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// 視点（カメラ）: ワールド → カメラ座標（右・下・奥）→ 透視投影。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub eye: Vec3,
    /// 行 = カメラの右・下・奥の単位ベクトル（ワールド座標）
    pub basis: Mat3,
    /// 焦点距離（px）。カメラから `focal` だけ奥の面が等倍で映る
    pub focal: f32,
}

impl View {
    /// カメラ制御が無いときの視点（z = -1024 から +z を見る。z = 0 の面が等倍）。
    pub const DEFAULT: Self = Self { eye: [0.0, 0.0, -PERSPECTIVE_DISTANCE], basis: IDENTITY, focal: PERSPECTIVE_DISTANCE };

    /// `eye` から `target` を見るカメラ。`roll` は視線まわりの傾き（ラジアン、正 = カメラが時計回りに傾く
    /// ＝映像は反時計回りに回る）。`fov` は縦の視野角（ラジアン）で、0 以下なら既定の焦点距離 1024。
    pub fn look_at(eye: Vec3, target: Vec3, roll: f32, fov: f32, canvas_height: f32) -> Self {
        let dir = sub(target, eye);
        let len = length(dir);
        let fwd = if len > 1e-6 { scale(dir, 1.0 / len) } else { [0.0, 0.0, 1.0] };
        // 画面の「下」（+y）を基準に右を決める。真上・真下を向くときは +x を右にする
        let r = cross([0.0, 1.0, 0.0], fwd);
        let r_len = length(r);
        let right = if r_len > 1e-6 { scale(r, 1.0 / r_len) } else { [1.0, 0.0, 0.0] };
        let down = cross(fwd, right);
        let (s, c) = roll.sin_cos();
        let right2 = add(scale(right, c), scale(down, s));
        let down2 = sub(scale(down, c), scale(right, s));
        let focal = if fov > 0.0 {
            let half = (fov.min(179f32.to_radians())) * 0.5;
            (canvas_height.max(1.0) * 0.5) / half.tan()
        } else {
            PERSPECTIVE_DISTANCE
        };
        Self { eye, basis: [right2, down2, fwd], focal }
    }
}

impl Default for View {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// 1 オブジェクト・1 フレーム分の描画パラメータ。座標はシーン中心基準のピクセル（+y 下、+z 奥）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawParams {
    /// オブジェクト自身の座標（グループ制御を掛ける前）
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// 1.0 = 等倍。グループ制御の拡大率も掛かった値（ラスタライズの大きさにも使う）
    pub zoom: f32,
    /// Z 軸回転（標準描画の「回転」）。ラジアン（正 = 時計回り）
    pub angle: f32,
    /// 不透明度 0.0〜1.0
    pub alpha: f32,
    /// X 軸回転・Y 軸回転（ラジアン、拡張描画）
    pub rx: f32,
    pub ry: f32,
    /// 縦横比 -1.0〜1.0（拡張描画の「縦横比」/ 100。正 = 横が縮む、負 = 縦が縮む）
    pub aspect: f32,
    /// 回転の中心（拡張描画の中心X/Y/Z。拡大前のオブジェクトのピクセル。正の中心X で絵は左へずれる）
    pub center: Vec3,
    /// 裏面を表示しない
    pub cull_back: bool,
    /// 所属するグループ制御の変換（無ければ恒等）
    pub group: GroupTransform,
    /// 視点（カメラ制御の対象ならそのカメラ、それ以外は既定の視点）
    pub view: View,
}

impl Default for DrawParams {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            zoom: 1.0,
            angle: 0.0,
            alpha: 1.0,
            rx: 0.0,
            ry: 0.0,
            aspect: 0.0,
            center: [0.0; 3],
            cull_back: false,
            group: GroupTransform::IDENTITY,
            view: View::DEFAULT,
        }
    }
}

impl DrawParams {
    /// 物体ローカル座標（拡大率を掛けた後のピクセル。スプライト中心が原点）→ 画面座標 の射影変換。
    ///
    /// `world(q) = P + L · ((q.x, q.y, 0) - center · zoom)`、`L = group.rot · Rx·Ry·Rz · diag(縦横比)`、
    /// `P = group(x, y, z)`。それをカメラ座標へ移して透視投影する。
    pub fn homography(&self) -> Homography {
        let (sx, sy) = if self.aspect >= 0.0 { (1.0 - self.aspect.min(1.0), 1.0) } else { (1.0, 1.0 + self.aspect.max(-1.0)) };
        let own = rotation_xyz(self.rx, self.ry, self.angle);
        let l = mat_mul(&mat_mul(&self.group.rot, &own), &[[sx, 0.0, 0.0], [0.0, sy, 0.0], [0.0, 0.0, 1.0]]);
        let pos = self.group.apply_point([self.x, self.y, self.z]);
        let origin = sub(pos, mat_vec(&l, scale(self.center, self.zoom)));
        let v = &self.view;
        let m = mat_mul(&v.basis, &l);
        let t = mat_vec(&v.basis, sub(origin, v.eye));
        let f = v.focal;
        Homography {
            m: [[m[0][0], m[0][1], t[0]], [m[1][0], m[1][1], t[1]], [m[2][0] / f, m[2][1] / f, t[2] / f]],
            near: NEAR_DISTANCE / f,
        }
    }
}

/// 物体ローカル座標 (x, y, 1) → 画面の同次座標 (X·w, Y·w, w)。`w` は目からの奥行き / 焦点距離（z = 0 の面で 1）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography {
    pub m: Mat3,
    /// これより小さい `w` の部分はカメラの手前（または後ろ）なので描かない
    pub near: f32,
}

impl Homography {
    /// 3D を使わない 2D の配置（`ext` を埋めないスプライト用。背景など）。
    pub fn flat(center: [f32; 2], angle: f32) -> Self {
        let (s, c) = angle.sin_cos();
        Self { m: [[c, -s, center[0]], [s, c, center[1]], [0.0, 0.0, 1.0]], near: 0.0 }
    }

    pub fn homogeneous(&self, q: [f32; 2]) -> Vec3 {
        mat_vec(&self.m, [q[0], q[1], 1.0])
    }

    /// ローカル座標を画面座標へ。カメラの手前の打ち切り面より手前なら `None`。
    pub fn project(&self, q: [f32; 2]) -> Option<[f32; 2]> {
        let h = self.homogeneous(q);
        (h[2] > self.near.max(1e-9)).then(|| [h[0] / h[2], h[1] / h[2]])
    }

    /// 裏面が見えているか（面の向きが反転している、または真横から見て面積が無い）。
    pub fn is_back_facing(&self) -> bool {
        let m = &self.m;
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        det <= 0.0
    }

    /// 半サイズ `half` のクワッドが丸ごとカメラの手前の打ち切り面より手前にあるか。
    pub fn is_behind_camera(&self, half: [f32; 2]) -> bool {
        quad_corners(half).iter().all(|q| self.project(*q).is_none())
    }

    /// `SpriteUniform::ext`（WGSL `Draw.ext`）の並び:
    /// `ext[0] = (m00, m01, m02, 1 = 射影変換を使う)`、`ext[1] = (m10, m11, m12, 0)`、`ext[2] = (m20, m21, m22, near)`、`ext[3]` は予備。
    pub fn to_ext(&self) -> [f32; 16] {
        let m = &self.m;
        [
            m[0][0], m[0][1], m[0][2], 1.0, //
            m[1][0], m[1][1], m[1][2], 0.0, //
            m[2][0], m[2][1], m[2][2], self.near, //
            0.0, 0.0, 0.0, 0.0,
        ]
    }

    /// [`Homography::to_ext`] の逆。射影変換を使わない（全て 0 の）`ext` なら `None`。
    pub fn from_ext(ext: &[f32; 16]) -> Option<Self> {
        (ext[3] > 0.5).then(|| Self {
            m: [[ext[0], ext[1], ext[2]], [ext[4], ext[5], ext[6]], [ext[8], ext[9], ext[10]]],
            near: ext[11],
        })
    }
}

/// 左上・右上・右下・左下（回転前の向き）。
fn quad_corners(half: [f32; 2]) -> [[f32; 2]; 4] {
    let [hx, hy] = half;
    [[-hx, -hy], [hx, -hy], [hx, hy], [-hx, hy]]
}

/// 直近の描画での、オブジェクトの描画範囲。選択枠などの UI 用。座標はシーン中心基準のピクセル（+y 下）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectBounds {
    /// 射影後の四隅（左上・右上・右下・左下。回転前の向きで数える）。3D 回転・カメラでは台形になる
    pub corners: [[f32; 2]; 4],
    /// 以下は四隅に当てはめた回転矩形（2D の配置なら厳密、3D では近似）。中心はスプライト中心の射影
    pub center: [f32; 2],
    /// 回転前の半サイズ
    pub half: [f32; 2],
    /// ラジアン（正 = 時計回り）
    pub angle: f32,
}

impl ObjectBounds {
    /// 半サイズ `half` のクワッドを `h` で射影した範囲。一部でもカメラの手前の打ち切り面より手前なら `None`。
    pub fn project(h: &Homography, half: [f32; 2]) -> Option<Self> {
        let [tl, tr, br, bl] = quad_corners(half).map(|q| h.project(q));
        let corners = [tl?, tr?, br?, bl?];
        let center = h.project([0.0, 0.0])?;
        let dist = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
        let [tl, tr, br, bl] = corners;
        let top = [tr[0] - tl[0] + br[0] - bl[0], tr[1] - tl[1] + br[1] - bl[1]];
        Some(Self {
            corners,
            center,
            half: [(dist(tl, tr) + dist(bl, br)) / 4.0, (dist(tl, bl) + dist(tr, br)) / 4.0],
            angle: top[1].atan2(top[0]),
        })
    }
}

/// `rel` はオブジェクト先頭からの相対フレーム。グループ制御・カメラ制御は含まない（[`Controls::apply`] で掛ける）。
pub fn eval_draw_params(obj: &TimelineObject, rel: Frame, fps: f64) -> DrawParams {
    // 拡張描画があればそちら（標準描画と同時には付かない）。どちらでも同じ名前のパラメータを読む
    let draw = obj.effect(&EffectKind::ExtendedDraw).or_else(|| obj.effect(&EffectKind::StandardDraw));
    let ev = |keys: &[&str], default: f64| {
        draw.and_then(|d| keys.iter().find_map(|k| d.track(k))).map(|t| t.evaluate(rel)).unwrap_or(default)
    };
    let deg = |keys: &[&str]| (ev(keys, 0.0) as f32).to_radians();
    let mut p = DrawParams {
        x: ev(&["X"], 0.0) as f32,
        y: ev(&["Y"], 0.0) as f32,
        z: ev(&["Z"], 0.0) as f32,
        zoom: (ev(&["拡大率"], 100.0) / 100.0) as f32,
        angle: deg(&["Z軸回転", "回転"]),
        alpha: (1.0 - ev(&["透明度"], 0.0) / 100.0).clamp(0.0, 1.0) as f32,
        rx: deg(&["X軸回転"]),
        ry: deg(&["Y軸回転"]),
        aspect: (ev(&["縦横比"], 0.0) / 100.0).clamp(-1.0, 1.0) as f32,
        center: [ev(&["中心X"], 0.0) as f32, ev(&["中心Y"], 0.0) as f32, ev(&["中心Z"], 0.0) as f32],
        cull_back: ev(&["裏面を表示しない"], 0.0) >= 0.5,
        ..DrawParams::default()
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

/// 制御オブジェクトの `range`（対象レイヤー数）。0・未指定 = 下のレイヤー全て（**近似**）。
fn control_range(obj: &TimelineObject, rel: Frame) -> usize {
    let Some(src) = obj.source() else { return 0 };
    let v = src.text("range").and_then(|t| t.trim().parse::<f64>().ok()).or_else(|| src.track("range").map(|t| t.evaluate(rel)));
    v.map(|v| v.max(0.0) as usize).unwrap_or(0)
}

struct Scope<T> {
    layer: usize,
    /// 対象の最後のレイヤー（含む）
    last: usize,
    value: T,
}

impl<T> Scope<T> {
    fn new(layer: usize, range: usize, value: T) -> Self {
        let last = if range == 0 { usize::MAX } else { layer.saturating_add(range) };
        Self { layer, last, value }
    }

    fn covers(&self, layer: usize) -> bool {
        self.layer < layer && layer <= self.last
    }
}

/// 1 フレーム分の制御オブジェクト（グループ制御・カメラ制御）の状態。レイヤー順（上 → 下）に
/// [`Controls::observe`] で登録し、メディアオブジェクトには [`Controls::apply`] で掛ける。
///
/// - グループ制御: 自分より下の `range` レイヤー（0 = 下全て）のオブジェクトに、X/Y/Z・拡大率・回転を親として掛ける。
///   範囲が重なるときは最も近い（下の）グループ制御が効き、そのグループ制御自身も上のグループ制御の範囲内ならその子になる
///   （AviUtl の「上位グループ制御の影響を受ける」が有効な状態に相当）。
/// - カメラ制御: 自分より下のレイヤー（`range` があればその数、無ければ全て）のうち、**`camera_target`（`.exo` の `camera=1`、
///   「カメラ制御の対象」）が付いたオブジェクトだけ**をそのカメラで描く。それ以外は既定の視点（z = -1024 からの透視）のまま。
///   複数あるときは最も近い（下の）カメラ制御が効く。
#[derive(Default)]
pub struct Controls {
    canvas_height: f32,
    groups: Vec<Scope<GroupTransform>>,
    cameras: Vec<Scope<View>>,
}

impl Controls {
    pub fn new(canvas_height: f32) -> Self {
        Self { canvas_height, ..Default::default() }
    }

    fn group_for(&self, layer: usize) -> Option<&GroupTransform> {
        self.groups.iter().rev().find(|g| g.covers(layer)).map(|g| &g.value)
    }

    /// 制御オブジェクトを登録します（制御オブジェクト以外は無視）。`rel` はそのオブジェクト先頭からの相対フレーム。
    pub fn observe(&mut self, layer: usize, obj: &TimelineObject, rel: Frame) {
        if obj.category() != ObjectCategory::Control {
            return;
        }
        let Some(src) = obj.source().filter(|s| s.enabled) else { return };
        let ev = |keys: &[&str], default: f64| {
            keys.iter().find_map(|k| src.track(k)).map(|t| t.evaluate(rel) as f32).unwrap_or(default as f32)
        };
        match src.kind {
            EffectKind::GroupControl => {
                let own = GroupTransform {
                    offset: [ev(&["X"], 0.0), ev(&["Y"], 0.0), ev(&["Z"], 0.0)],
                    rot: rotation_xyz(
                        ev(&["X軸回転"], 0.0).to_radians(),
                        ev(&["Y軸回転"], 0.0).to_radians(),
                        ev(&["Z軸回転", "回転"], 0.0).to_radians(),
                    ),
                    scale: (ev(&["拡大率"], 100.0) / 100.0).max(0.0),
                };
                let value = match self.group_for(layer) {
                    Some(parent) => parent.compose(&own),
                    None => own,
                };
                self.groups.push(Scope::new(layer, control_range(obj, rel), value));
            }
            EffectKind::Camera => {
                let view = View::look_at(
                    [ev(&["X"], 0.0), ev(&["Y"], 0.0), ev(&["Z"], -PERSPECTIVE_DISTANCE as f64)],
                    [ev(&["目標X"], 0.0), ev(&["目標Y"], 0.0), ev(&["目標Z"], 0.0)],
                    ev(&["傾き"], 0.0).to_radians(),
                    ev(&["視野角"], 0.0).to_radians(),
                    self.canvas_height,
                );
                self.cameras.push(Scope::new(layer, control_range(obj, rel), view));
            }
            _ => {}
        }
    }

    /// `layer` にあるオブジェクト `obj` の描画パラメータに、効いているグループ制御・カメラ制御を掛けます。
    pub fn apply(&self, layer: usize, obj: &TimelineObject, mut p: DrawParams) -> DrawParams {
        if let Some(g) = self.group_for(layer) {
            p.group = *g;
            p.zoom *= g.scale;
        }
        if obj.camera_target {
            if let Some(c) = self.cameras.iter().rev().find(|c| c.covers(layer)) {
                p.view = c.value;
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au2_core::{Effect, Track};

    fn near(a: [f32; 2], b: [f32; 2], tol: f32) -> bool {
        (a[0] - b[0]).abs() <= tol && (a[1] - b[1]).abs() <= tol
    }

    /// 拡張描画を付けた図形（標準描画を置き換える）。
    fn extended(params: &[(&str, f64)]) -> TimelineObject {
        let mut obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        obj.effects.retain(|e| e.kind != EffectKind::StandardDraw);
        let mut d = Effect::new("拡張描画");
        for (k, v) in params {
            d.push_track(*k, Track::constant(*v, 2));
        }
        obj.effects.push(d);
        obj
    }

    #[test]
    fn defaults_are_identity() {
        let obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        let p = eval_draw_params(&obj, 10, 30.0);
        assert_eq!(p, DrawParams::default());
        assert_eq!((p.x, p.y, p.zoom, p.angle, p.alpha), (0.0, 0.0, 1.0, 0.0, 1.0));
        // 既定の視点では z = 0 の面は等倍: 2D の配置と同じ射影変換
        let h = p.homography();
        assert_eq!(h.m, Homography::flat([0.0, 0.0], 0.0).m);
        assert!(!h.is_back_facing());
    }

    #[test]
    fn standard_draw_matches_flat_2d_placement() {
        let mut obj = TimelineObject::new_shape(0, 99, 1, "ff0000");
        let d = obj.effects.iter_mut().find(|e| e.kind == EffectKind::StandardDraw).unwrap();
        *d.param_mut("X").unwrap() = au2_core::ParamValue::Track(Track::constant(30.0, 1));
        *d.param_mut("Y").unwrap() = au2_core::ParamValue::Track(Track::constant(-10.0, 1));
        *d.param_mut("回転").unwrap() = au2_core::ParamValue::Track(Track::constant(30.0, 2));
        let p = eval_draw_params(&obj, 0, 30.0);
        let h = p.homography();
        let flat = Homography::flat([30.0, -10.0], 30f32.to_radians());
        for q in [[0.0, 0.0], [50.0, 0.0], [-20.0, 40.0]] {
            assert!(near(h.project(q).unwrap(), flat.project(q).unwrap(), 1e-4), "{q:?}");
        }
    }

    #[test]
    fn extended_draw_parameters_are_read() {
        let obj = extended(&[
            ("X", 10.0),
            ("Y", 20.0),
            ("Z", 30.0),
            ("拡大率", 50.0),
            ("透明度", 25.0),
            ("縦横比", -40.0),
            ("X軸回転", 90.0),
            ("Y軸回転", 180.0),
            ("Z軸回転", 45.0),
            ("中心X", 1.0),
            ("中心Y", 2.0),
            ("中心Z", 3.0),
            ("裏面を表示しない", 1.0),
        ]);
        let p = eval_draw_params(&obj, 0, 30.0);
        assert_eq!((p.x, p.y, p.z, p.zoom), (10.0, 20.0, 30.0, 0.5));
        assert!((p.alpha - 0.75).abs() < 1e-6);
        assert!((p.aspect + 0.4).abs() < 1e-6);
        assert!((p.rx - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((p.ry - std::f32::consts::PI).abs() < 1e-6);
        assert!((p.angle - std::f32::consts::FRAC_PI_4).abs() < 1e-6);
        assert_eq!(p.center, [1.0, 2.0, 3.0]);
        assert!(p.cull_back);
    }

    #[test]
    fn z_translation_scales_by_perspective() {
        // Z = 1024: 目からの距離が 2 倍 → 半分の大きさ。位置も中心へ半分寄る
        let p = eval_draw_params(&extended(&[("X", 100.0), ("Z", 1024.0)]), 0, 30.0);
        let h = p.homography();
        assert!(near(h.project([0.0, 0.0]).unwrap(), [50.0, 0.0], 1e-3));
        assert!(near(h.project([50.0, 50.0]).unwrap(), [75.0, 25.0], 1e-3));
        // Z = -512: 2 倍
        let p = eval_draw_params(&extended(&[("Z", -512.0)]), 0, 30.0);
        assert!(near(p.homography().project([50.0, -50.0]).unwrap(), [100.0, -100.0], 1e-3));
    }

    #[test]
    fn y_rotation_projects_a_trapezoid_and_turns_edge_on_at_90() {
        // Y 軸回転 60°: 右端 (50, y, 0) → x' = 50cos60 = 25, z' = -50sin60 = -43.30（手前）→ 拡大 1024/980.70
        let h = eval_draw_params(&extended(&[("Y軸回転", 60.0)]), 0, 30.0).homography();
        let k_r = 1024.0 / (1024.0 - 43.30127);
        let k_l = 1024.0 / (1024.0 + 43.30127);
        assert!(near(h.project([50.0, -50.0]).unwrap(), [25.0 * k_r, -50.0 * k_r], 1e-2));
        assert!(near(h.project([-50.0, 50.0]).unwrap(), [-25.0 * k_l, 50.0 * k_l], 1e-2));
        assert!(!h.is_back_facing());
        // 90°: 真横から見るので幅 0（面積 0 = 裏面扱い）
        let h = eval_draw_params(&extended(&[("Y軸回転", 90.0)]), 0, 30.0).homography();
        assert!(h.project([50.0, 0.0]).unwrap()[0].abs() < 1e-3);
        assert!(h.project([-50.0, 0.0]).unwrap()[0].abs() < 1e-3);
    }

    #[test]
    fn back_face_is_detected_at_180_degrees() {
        for (key, deg, back) in [("Y軸回転", 180.0, true), ("X軸回転", 180.0, true), ("Y軸回転", 120.0, true), ("Y軸回転", 60.0, false), ("Z軸回転", 180.0, false)] {
            let h = eval_draw_params(&extended(&[(key, deg)]), 0, 30.0).homography();
            assert_eq!(h.is_back_facing(), back, "{key} {deg}");
        }
        // 裏から見ると左右が反転する
        let h = eval_draw_params(&extended(&[("Y軸回転", 180.0)]), 0, 30.0).homography();
        assert!(near(h.project([50.0, 10.0]).unwrap(), [-50.0, 10.0], 1e-3));
    }

    #[test]
    fn rotation_order_is_z_then_y_then_x() {
        // Z90 → X90: (50,0,0) → Rz → (0,50,0) → Rx → (0,0,50)。逆順なら (0,50,0) になる
        let r = rotation_xyz(90f32.to_radians(), 0.0, 90f32.to_radians());
        let v = mat_vec(&r, [50.0, 0.0, 0.0]);
        assert!(v[0].abs() < 1e-4 && v[1].abs() < 1e-4 && (v[2] - 50.0).abs() < 1e-4, "{v:?}");
    }

    #[test]
    fn center_offset_moves_the_pivot() {
        // 中心X = 50（拡大率 200% → 100px）: 回転の中心が絵の右寄りになり、絵は左へずれる
        let p = eval_draw_params(&extended(&[("中心X", 50.0), ("拡大率", 200.0)]), 0, 30.0);
        assert!(near(p.homography().project([0.0, 0.0]).unwrap(), [-100.0, 0.0], 1e-3));
        // その点を中心に 90° 回すと、絵の中心は真上へ
        let p = eval_draw_params(&extended(&[("中心X", 50.0), ("Z軸回転", 90.0)]), 0, 30.0);
        assert!(near(p.homography().project([0.0, 0.0]).unwrap(), [0.0, -50.0], 1e-3));
    }

    #[test]
    fn aspect_squeezes_one_axis() {
        let h = eval_draw_params(&extended(&[("縦横比", 50.0)]), 0, 30.0).homography();
        assert!(near(h.project([40.0, 40.0]).unwrap(), [20.0, 40.0], 1e-3));
        let h = eval_draw_params(&extended(&[("縦横比", -50.0)]), 0, 30.0).homography();
        assert!(near(h.project([40.0, 40.0]).unwrap(), [40.0, 20.0], 1e-3));
    }

    #[test]
    fn ext_block_round_trips_and_zero_means_flat() {
        let h = eval_draw_params(&extended(&[("Y軸回転", 30.0), ("Z", 100.0)]), 0, 30.0).homography();
        assert_eq!(Homography::from_ext(&h.to_ext()), Some(h));
        assert_eq!(Homography::from_ext(&[0.0; 16]), None);
    }

    #[test]
    fn behind_camera_is_detected() {
        let h = eval_draw_params(&extended(&[("Z", -2000.0)]), 0, 30.0).homography();
        assert!(h.is_behind_camera([50.0, 50.0]));
        assert!(h.project([0.0, 0.0]).is_none());
        let h = eval_draw_params(&extended(&[("Z", -1000.0)]), 0, 30.0).homography();
        assert!(!h.is_behind_camera([50.0, 50.0]));
    }

    #[test]
    fn bounds_are_the_projected_quad() {
        let h = eval_draw_params(&extended(&[("X", 30.0), ("Z軸回転", 90.0)]), 0, 30.0).homography();
        let b = ObjectBounds::project(&h, [20.0, 10.0]).unwrap();
        assert!(near(b.center, [30.0, 0.0], 1e-4));
        assert!(near(b.half, [20.0, 10.0], 1e-4));
        assert!((b.angle - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        // 左上 (-20,-10) を時計回りに 90° → (10, -20)
        assert!(near(b.corners[0], [40.0, -20.0], 1e-4));
    }

    fn group(params: &[(&str, f64)], range: &str) -> TimelineObject {
        let mut obj = TimelineObject::new_group_control(0, 99);
        let mut e = Effect::new("グループ制御");
        for (k, v) in params {
            e.push_track(*k, Track::constant(*v, 2));
        }
        e.push_text("range", range);
        obj.effects[0] = e;
        obj
    }

    fn camera(params: &[(&str, f64)]) -> TimelineObject {
        let mut obj = TimelineObject::new_camera_control(0, 99);
        let mut e = Effect::new("カメラ制御");
        for (k, v) in params {
            e.push_track(*k, Track::constant(*v, 2));
        }
        obj.effects[0] = e;
        obj
    }

    fn placed(c: &Controls, layer: usize, obj: &TimelineObject) -> [f32; 2] {
        c.apply(layer, obj, eval_draw_params(obj, 0, 30.0)).homography().project([0.0, 0.0]).unwrap()
    }

    #[test]
    fn group_control_transforms_only_its_range() {
        let mut c = Controls::new(100.0);
        c.observe(0, &group(&[("X", 100.0), ("拡大率", 50.0), ("回転", 90.0)], "1"), 0);
        let obj = extended(&[("X", 80.0)]);
        // 下 1 レイヤーだけ: (80,0) → 縮小 (40,0) → 時計回り 90° (0,40) → +X100
        assert!(near(placed(&c, 1, &obj), [100.0, 40.0], 1e-3));
        let p = c.apply(1, &obj, eval_draw_params(&obj, 0, 30.0));
        assert!((p.zoom - 0.5).abs() < 1e-6, "グループの拡大率は大きさにも掛かる");
        assert!(near(placed(&c, 2, &obj), [80.0, 0.0], 1e-3), "範囲外");
        assert!(near(placed(&c, 0, &obj), [80.0, 0.0], 1e-3), "自分のレイヤー");
    }

    #[test]
    fn nested_group_controls_compose() {
        let mut c = Controls::new(100.0);
        c.observe(0, &group(&[("X", 100.0)], "0"), 0); // 0 = 下全て
        c.observe(2, &group(&[("Y", 10.0), ("拡大率", 200.0)], "1"), 0);
        let obj = extended(&[("X", 5.0)]);
        assert!(near(placed(&c, 1, &obj), [105.0, 0.0], 1e-3));
        assert!(near(placed(&c, 3, &obj), [110.0, 10.0], 1e-3), "内側のグループ→外側のグループ");
        assert!(near(placed(&c, 9, &obj), [105.0, 0.0], 1e-3), "内側の範囲外は外側だけ");
    }

    #[test]
    fn camera_applies_only_to_camera_targets_below_it() {
        let mut c = Controls::new(100.0);
        // カメラを右へ 100 平行移動（目標も同じだけ）→ 対象は左へ 100 ずれる
        c.observe(1, &camera(&[("X", 100.0), ("Z", -1024.0), ("目標X", 100.0)]), 0);
        let mut obj = extended(&[]);
        assert!(near(placed(&c, 2, &obj), [0.0, 0.0], 1e-3), "camera_target でないものは動かない");
        obj.camera_target = true;
        assert!(near(placed(&c, 2, &obj), [-100.0, 0.0], 1e-3));
        assert!(near(placed(&c, 0, &obj), [0.0, 0.0], 1e-3), "カメラより上のレイヤー");
    }

    #[test]
    fn default_camera_equals_default_view_and_distance_zooms() {
        let v = View::look_at([0.0, 0.0, -1024.0], [0.0; 3], 0.0, 0.0, 100.0);
        for r in 0..3 {
            for k in 0..3 {
                assert!((v.basis[r][k] - View::DEFAULT.basis[r][k]).abs() < 1e-6);
            }
        }
        assert_eq!((v.eye, v.focal), (View::DEFAULT.eye, View::DEFAULT.focal));
        // 半分の距離から見ると 2 倍
        let p = DrawParams { view: View::look_at([0.0, 0.0, -512.0], [0.0; 3], 0.0, 0.0, 100.0), ..Default::default() };
        assert!(near(p.homography().project([10.0, 10.0]).unwrap(), [20.0, 20.0], 1e-3));
        // 視野角 90°・高さ 100: 焦点距離 50 → 距離 1024 の面は 50/1024 倍
        let p = DrawParams { view: View::look_at([0.0, 0.0, -1024.0], [0.0; 3], 0.0, 90f32.to_radians(), 100.0), ..Default::default() };
        assert!(near(p.homography().project([1024.0, 0.0]).unwrap(), [50.0, 0.0], 1e-2));
    }

    #[test]
    fn camera_roll_and_orbit() {
        // 傾き 90°: カメラが時計回りに傾く → 右にある物は画面の上へ
        let p = DrawParams { view: View::look_at([0.0, 0.0, -1024.0], [0.0; 3], 90f32.to_radians(), 0.0, 100.0), ..Default::default() };
        assert!(near(p.homography().project([40.0, 0.0]).unwrap(), [0.0, -40.0], 1e-3));
        // 右側（+x）から原点を見る: 面を真横から見るので幅が無い。奥（+z）にあった点が画面の右へ
        let p = DrawParams { view: View::look_at([1024.0, 0.0, 0.0], [0.0; 3], 0.0, 0.0, 100.0), ..Default::default() };
        assert!(p.homography().is_back_facing());
        let view = p.view;
        let v = mat_vec(&view.basis, sub([0.0, 0.0, 100.0], view.eye));
        assert!(v[0] > 99.0 && v[2] > 1023.0, "{v:?}");
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
