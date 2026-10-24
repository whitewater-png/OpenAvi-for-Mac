//! `.exo`（または動画ファイル 1 本）の指定フレームをウィンドウ無しで PNG に描き出します。
//!
//! ```bash
//! cargo run --release -p au2-render --example render_frame -- samples/sample.exo 30 out.png
//! # 連続 60 フレームの平均描画時間を測る（最後のフレームを保存）
//! cargo run --release -p au2-render --example render_frame -- samples/sample.exo 0 out.png 60
//! ```

use std::path::PathBuf;

use au2_media::MediaCache;
use au2_render::{Gpu, Renderer};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(frame), Some(output)) = (args.next(), args.next(), args.next()) else {
        eprintln!("使い方: render_frame <入力.exo> <フレーム番号> <出力.png>");
        std::process::exit(2);
    };
    let frame: u32 = frame.parse().expect("フレーム番号は整数で指定してください");
    let count: u32 = args.next().map_or(1, |c| c.parse().expect("枚数は整数で指定してください")).max(1);

    // 動画ファイルを渡された場合は、その動画 1 本だけを置いた 1280x720 のシーンを作る（デコード速度の計測用）
    let is_video = au2_media::MediaKind::from_path(std::path::Path::new(&input)) == Some(au2_media::MediaKind::Video);
    let scene = if is_video {
        let mut scene = au2_core::Scene::new("video", au2_core::SceneSettings { width: 1280, height: 720, ..Default::default() });
        scene.insert_object(0, au2_core::TimelineObject::new_video(0, 9999, input.clone()));
        scene
    } else {
        let bytes = std::fs::read(&input).expect("入力ファイルを読み込めません");
        let doc = au2_exo::parse_exo_bytes(&bytes).expect(".exo を解析できません");
        for w in &doc.warnings {
            eprintln!("警告: {w}");
        }
        doc.scene
    };

    let mut renderer = Renderer::new(Gpu::headless().expect("GPU を初期化できません"));
    match renderer.load_system_font() {
        Some(path) => eprintln!("フォント: {path}"),
        None => eprintln!("日本語フォントが見つからないため、テキストは描画されません"),
    }
    eprintln!("GPU: {}", renderer.gpu().adapter_info.name);
    let mut media = MediaCache::new();
    let mut rendered = None;
    let started = std::time::Instant::now();
    for i in 0..count {
        rendered = Some(renderer.render(&scene, frame + i, &mut media).expect("描画に失敗しました"));
    }
    let rendered = rendered.expect("count >= 1");
    let total = started.elapsed().as_secs_f64();
    eprintln!(
        "{}x{} を {count} 枚、合計 {:.0} ms（平均 {:.1} ms / 枚 = {:.1} fps）",
        rendered.width,
        rendered.height,
        total * 1e3,
        total * 1e3 / count as f64,
        count as f64 / total
    );

    let output = PathBuf::from(output);
    image::save_buffer(&output, &rendered.pixels, rendered.width, rendered.height, image::ColorType::Rgba8)
        .expect("PNG を書き出せません");
    eprintln!("書き出しました: {}", output.display());
}
