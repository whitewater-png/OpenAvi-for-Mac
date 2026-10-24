//! プレビュー画面。
//!
//! - 図形・テキスト・画像ファイル・動画ファイル・フィルタオブジェクトを合成描画します。
//! - 画像ファイル（`.png`, `.jpg`, `.bmp`, `.webp`, `.gif`）および動画ファイル（`.mp4`, `.avi`, `.mov`, `.mkv` 等）は
//!   [`au2_media::MediaCache`] を通じてデコードされ、GPU テクスチャとして回転・拡大率・透明度付きで描画されます。

use eframe::egui::{self, emath::Rot2, pos2, vec2, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke};

use crate::state::EditorState;

pub fn ui(ui: &mut egui::Ui, st: &mut EditorState) {
    // 下部のシークバー＆再生コントロール領域の高さ（約 54px）を残して上部にプレビューキャンバスを配置
    let control_height = 54.0;
    let avail = ui.available_size();
    let preview_size = vec2(avail.x.max(64.0), (avail.y - control_height).max(64.0));

    let (resp, painter) = ui.allocate_painter(preview_size, Sense::click());
    let area = resp.rect;
    painter.rect_filled(area, 0.0, Color32::from_gray(24));

    let s = &st.scene.settings;
    let (sw, sh) = (s.width.max(1) as f32, s.height.max(1) as f32);
    let scale = (area.width() / sw).min(area.height() / sh).max(0.0001);
    let canvas = Rect::from_center_size(area.center(), vec2(sw * scale, sh * scale));
    painter.rect_filled(canvas, 0.0, Color32::BLACK);
    let painter = painter.with_clip_rect(canvas.intersect(area));

    let frame = st.frame;

    // 描画は au2-render（GPU）に任せ、結果のテクスチャをキャンバスいっぱいに表示する
    match st.render_preview() {
        Some(tex) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(tex, canvas, uv, Color32::WHITE);
        }
        None => {
            painter.text(
                canvas.center(),
                Align2::CENTER_CENTER,
                st.preview.as_ref().and_then(|p| p.last_error.clone()).unwrap_or_else(|| "GPU プレビューを初期化できませんでした".into()),
                FontId::proportional(14.0),
                Color32::LIGHT_GRAY,
            );
        }
    }

    // 選択中オブジェクトの枠（回転に追従する）
    if let Some(b) = st.selected.and_then(|id| st.preview.as_ref()?.renderer.object_bounds(id)) {
        let center = canvas.center() + vec2(b.center[0], b.center[1]) * scale;
        let rot = Rot2::from_angle(b.angle);
        let (hx, hy) = (b.half[0] * scale + 2.0, b.half[1] * scale + 2.0);
        let corners: Vec<Pos2> = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)]
            .into_iter()
            .map(|(sx, sy)| center + rot * vec2(sx * hx, sy * hy))
            .collect();
        painter.add(Shape::closed_line(corners, Stroke::new(1.0_f32, Color32::from_rgb(255, 200, 0))));
    }

    painter.text(
        canvas.left_top() + vec2(6.0, 4.0),
        Align2::LEFT_TOP,
        format!("{} / frame {}", st.scene.settings.frame_rate.timecode(frame), frame + 1),
        FontId::monospace(12.0),
        Color32::from_white_alpha(160),
    );

    // 3. 動画プレビュー下部にシークバーと再生コントロールボタンを配置
    ui.add_space(4.0);
    bottom_transport_bar(ui, st);
}

fn bottom_transport_bar(ui: &mut egui::Ui, st: &mut EditorState) {
    let last = st.last_frame();
    let mut f = st.frame;

    // 1段目：プレビュー幅いっぱいのシークバー
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(80.0);
        if ui
            .add(egui::Slider::new(&mut f, 0..=last).show_value(false))
            .on_hover_text("シークバー（ドラッグでフレーム移動）")
            .changed()
        {
            st.set_frame(f);
        }
    });

    // 2段目：再生・コマ送りボタンと現在タイムコード / フレーム番号
    ui.horizontal(|ui| {
        if ui.button("⏮").on_hover_text("先頭フレームへ (Home)").clicked() {
            st.set_frame(0);
        }
        if ui.button("◀").on_hover_text("1 フレーム戻る (←)").clicked() {
            st.step(-1);
        }
        let play_label = if st.playing { "⏸ 一時停止" } else { "▶ 再生" };
        if ui
            .button(play_label)
            .on_hover_text("再生 / 一時停止 (Space)")
            .clicked()
        {
            st.toggle_play();
        }
        if ui.button("▶|").on_hover_text("1 フレーム進む (→)").clicked() {
            st.step(1);
        }
        if ui.button("⏭").on_hover_text("最終フレームへ (End)").clicked() {
            st.set_frame(last);
        }

        ui.separator();
        let tc = st.scene.settings.frame_rate.timecode(st.frame);
        ui.monospace(format!("{tc}  ({} / {} F)", st.frame + 1, last + 1));
    });
}
