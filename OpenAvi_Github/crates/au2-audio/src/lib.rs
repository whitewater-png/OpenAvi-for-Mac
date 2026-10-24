//! # au2-audio
//!
//! OpenAvi の音声: デコード（ffmpeg）・ミキシング・再生（cpal）。
//!
//! すべてサンプルレート [`SAMPLE_RATE`] のインターリーブ f32 ステレオで扱います。
//! ミキサーは書き出し（au2-export）とライブ再生で共有します。

mod mixer;
#[cfg(feature = "output")]
mod output;

pub use mixer::{mix, mix_project, AudioCache};
#[cfg(feature = "output")]
pub use output::AudioPlayer;

/// 内部の標準サンプルレート。
pub const SAMPLE_RATE: u32 = 48_000;
/// チャンネル数（ステレオ）。
pub const CHANNELS: usize = 2;
