//! # au2-render
//!
//! OpenAvi のヘッドレス GPU 合成エンジン。`Scene` とフレーム番号から RGBA 画像を作ります。
//! プレビューと動画書き出しが同じコードを使うことで、見た目と出力結果の食い違いを防ぎます。
//!
//! 設計は `docs/RENDER_DESIGN.md` を参照してください。
//!
//! ```no_run
//! use au2_render::{Gpu, NoMedia, Renderer};
//! # fn demo(scene: &au2_core::Scene) -> Result<(), au2_render::RenderError> {
//! let mut renderer = Renderer::new(Gpu::headless()?);
//! let frame = renderer.render(scene, 0, &mut NoMedia)?;
//! assert_eq!(frame.pixels.len(), (frame.width * frame.height * 4) as usize);
//! # Ok(())
//! # }
//! ```

mod eval;
mod gpu;
mod media;
mod renderer;
mod text;

pub use au2_media::RgbaFrame;
pub use eval::{eval_draw_params, Controls, DrawParams, GroupTransform, Homography, View};
pub use gpu::Gpu;
pub use media::{MediaSource, NoMedia};
pub use renderer::{ObjectBounds, Renderer, FORMAT};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("利用可能な GPU アダプタが見つかりません")]
    NoAdapter,
    #[error("GPU デバイスを取得できません: {0}")]
    RequestDevice(wgpu::RequestDeviceError),
    #[error("シーンのサイズ {width}x{height} は GPU の上限（1 辺 {max}px）を超えています")]
    SceneTooLarge { width: u32, height: u32, max: u32 },
    #[error("フレームの読み戻しに失敗しました: {0}")]
    Readback(String),
}
