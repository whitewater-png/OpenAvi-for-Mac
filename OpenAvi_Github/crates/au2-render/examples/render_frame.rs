//! `.exo` の 1 フレームをウィンドウ無しで PNG に描き出します。
//!
//! ```bash
//! cargo run --release -p au2-render --example render_frame -- samples/sample.exo 30 out.png
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

    let bytes = std::fs::read(&input).expect("入力ファイルを読み込めません");
    let doc = au2_exo::parse_exo_bytes(&bytes).expect(".exo を解析できません");
    for w in &doc.warnings {
        eprintln!("警告: {w}");
    }

    let mut renderer = Renderer::new(Gpu::headless().expect("GPU を初期化できません"));
    match renderer.load_system_font() {
        Some(path) => eprintln!("フォント: {path}"),
        None => eprintln!("日本語フォントが見つからないため、テキストは描画されません"),
    }
    eprintln!("GPU: {}", renderer.gpu().adapter_info.name);
    let mut media = MediaCache::new();
    let started = std::time::Instant::now();
    let rendered = renderer.render(&doc.scene, frame, &mut media).expect("描画に失敗しました");
    eprintln!("{}x{} を {:.1} ms で描画しました", rendered.width, rendered.height, started.elapsed().as_secs_f64() * 1e3);

    let output = PathBuf::from(output);
    image::save_buffer(&output, &rendered.pixels, rendered.width, rendered.height, image::ColorType::Rgba8)
        .expect("PNG を書き出せません");
    eprintln!("書き出しました: {}", output.display());
}
