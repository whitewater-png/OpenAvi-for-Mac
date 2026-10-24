//! 出力デバイスの動作確認: 440Hz の小さな音を 0.5 秒鳴らし、再生位置が進むことを確かめる。
//!
//! ```bash
//! cargo run -p au2-audio --example beep
//! ```

use std::sync::Arc;
use std::time::Duration;

use au2_audio::{AudioPlayer, CHANNELS, SAMPLE_RATE};

fn main() {
    let samples = (SAMPLE_RATE / 2) as usize;
    let mut buf = Vec::with_capacity(samples * CHANNELS);
    for i in 0..samples {
        let v = (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.05;
        buf.extend_from_slice(&[v, v]);
    }
    let mut player = AudioPlayer::new();
    player.play(Arc::new(buf)).expect("音声出力を開始できません");
    std::thread::sleep(Duration::from_millis(250));
    let mid = player.position_secs().unwrap_or(0.0);
    std::thread::sleep(Duration::from_millis(450));
    println!("250ms 後の再生位置: {mid:.3}s / 終了後に再生中か: {}", player.is_playing());
    assert!(mid > 0.1 && mid < 0.4, "再生位置が実時間どおりに進んでいない: {mid}");
    assert!(!player.is_playing(), "0.5 秒のバッファは終わっているはず");
}
