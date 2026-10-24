//! cpal による再生。ミックス済みのバッファを指定位置から鳴らします。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{CHANNELS, SAMPLE_RATE};

/// 再生中の状態（コールバックと共有する）。
struct Shared {
    buffer: Arc<Vec<f32>>,
    /// 次に出力するフレーム（サンプル）位置
    position: AtomicUsize,
    finished: AtomicBool,
}

pub struct AudioPlayer {
    // Stream は Send でないので、保持するだけ（drop で停止）
    stream: Option<cpal::Stream>,
    shared: Option<Arc<Shared>>,
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self { stream: None, shared: None }
    }

    /// `buffer`（48kHz ステレオ）を先頭から再生します。出力デバイスが無い・開けない場合は `Err`。
    pub fn play(&mut self, buffer: Arc<Vec<f32>>) -> Result<(), String> {
        self.stop();
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("出力デバイスがありません")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let channels = supported.channels() as usize;
        let device_rate = supported.sample_rate().0;
        let config: cpal::StreamConfig = supported.config();

        let shared = Arc::new(Shared { buffer, position: AtomicUsize::new(0), finished: AtomicBool::new(false) });
        let cb_shared = shared.clone();
        // 出力デバイスのレートが 48kHz と違えば、最近傍で簡易変換する（品質より同期を優先）
        let step = SAMPLE_RATE as f64 / device_rate as f64;
        let mut frac = 0.0f64;
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let total = cb_shared.buffer.len() / CHANNELS;
                    let mut pos = cb_shared.position.load(Ordering::Relaxed);
                    for frame in data.chunks_mut(channels) {
                        if pos >= total {
                            frame.fill(0.0);
                            cb_shared.finished.store(true, Ordering::Relaxed);
                            continue;
                        }
                        let (l, r) = (cb_shared.buffer[pos * CHANNELS], cb_shared.buffer[pos * CHANNELS + 1]);
                        match channels {
                            1 => frame[0] = (l + r) * 0.5,
                            _ => {
                                frame[0] = l;
                                frame[1] = r;
                                frame[2..].fill(0.0);
                            }
                        }
                        frac += step;
                        let advance = frac.floor();
                        frac -= advance;
                        pos += advance as usize;
                    }
                    cb_shared.position.store(pos, Ordering::Relaxed);
                },
                |e| log::warn!("音声出力エラー: {e}"),
                None,
            )
            .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;
        self.stream = Some(stream);
        self.shared = Some(shared);
        Ok(())
    }

    pub fn stop(&mut self) {
        self.stream = None;
        self.shared = None;
    }

    pub fn is_playing(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| !s.finished.load(Ordering::Relaxed))
    }

    /// 再生済みの位置（秒）。
    pub fn position_secs(&self) -> Option<f64> {
        let s = self.shared.as_ref()?;
        Some(s.position.load(Ordering::Relaxed) as f64 / SAMPLE_RATE as f64)
    }
}
