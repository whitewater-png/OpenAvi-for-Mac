//! # au2-export
//!
//! シーンを動画ファイルに書き出します。映像は `au2-render` で 1 フレームずつ描いて ffmpeg の標準入力へ送り、
//! 音声は `au2-audio` でミックスして一時ファイル経由で渡します。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use au2_audio::{AudioCache, SAMPLE_RATE};
use au2_core::{Frame, Scene};
use au2_render::{MediaSource, RenderError, Renderer};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("ffmpeg が見つかりません（書き出しには ffmpeg が必要です）")]
    NoFfmpeg,
    #[error("書き出す範囲が空です")]
    EmptyRange,
    #[error("描画に失敗しました: {0}")]
    Render(#[from] RenderError),
    #[error("ffmpeg の起動・入出力に失敗しました: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg がエラーで終了しました（コード {0:?}）:\n{1}")]
    Encoder(Option<i32>, String),
    #[error("キャンセルされました")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 (libx264)
    H264,
    /// H.265 / HEVC (libx265)
    H265,
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub output: PathBuf,
    pub codec: Codec,
    /// 品質（CRF）。小さいほど高画質・大容量。H.264 は 18 前後が目安。
    pub crf: u8,
    /// 書き出す範囲（両端を含むフレーム番号）。`None` ならシーン全体。
    pub range: Option<(Frame, Frame)>,
    /// 音声を含めるか（音声オブジェクトが 1 つも無ければ自動的に無音トラックなしになる）
    pub audio: bool,
}

impl ExportOptions {
    pub fn new(output: impl Into<PathBuf>) -> Self {
        Self { output: output.into(), codec: Codec::H264, crf: 18, range: None, audio: true }
    }
}

/// 進捗コールバックの引数: (書き出し済みフレーム数, 全フレーム数)。`false` を返すとキャンセル。
pub type Progress<'a> = &'a mut dyn FnMut(u32, u32) -> bool;

/// 書き出す範囲（両端を含む）を解決します。
pub fn resolve_range(scene: &Scene, range: Option<(Frame, Frame)>) -> Option<(Frame, Frame)> {
    let last = scene.settings.length.max(scene.content_end()).max(1) - 1;
    let (a, b) = range.unwrap_or((0, last));
    (a <= b).then_some((a, b))
}

/// `scene` を `opts.output` に書き出します。
///
/// `resolve` は音声ファイル名を実パスへ解決する関数（プレビューと同じ規則を渡す）。
pub fn export_video(
    scene: &Scene,
    opts: &ExportOptions,
    renderer: &mut Renderer,
    media: &mut dyn MediaSource,
    resolve: &dyn Fn(&str) -> PathBuf,
    progress: Progress,
) -> Result<(), ExportError> {
    let ffmpeg = au2_media::locate_ffmpeg().ok_or(ExportError::NoFfmpeg)?;
    let (first, last) = resolve_range(scene, opts.range).ok_or(ExportError::EmptyRange)?;
    let total = last - first + 1;
    let (width, height) = (scene.settings.width.max(1), scene.settings.height.max(1));

    // 音声: 範囲全体をミックスして生の f32 PCM として一時ファイルへ（無音なら付けない）
    let audio_path = if opts.audio {
        let mut cache = AudioCache::new();
        let pcm = au2_audio::mix(scene, &mut cache, first, last + 1, resolve);
        if pcm.iter().any(|&v| v != 0.0) {
            let path = std::env::temp_dir().join(format!("openavi-export-{}.f32", std::process::id()));
            let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write(&path, bytes)?;
            Some(path)
        } else {
            None
        }
    } else {
        None
    };

    let result = run_ffmpeg(&ffmpeg, scene, opts, renderer, media, progress, (first, total, width, height), audio_path.as_deref());
    if let Some(p) = &audio_path {
        let _ = std::fs::remove_file(p);
    }
    if matches!(result, Err(ExportError::Cancelled) | Err(ExportError::Encoder(..))) {
        // 中途半端なファイルは残さない
        let _ = std::fs::remove_file(&opts.output);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg(
    ffmpeg: &Path,
    scene: &Scene,
    opts: &ExportOptions,
    renderer: &mut Renderer,
    media: &mut dyn MediaSource,
    progress: Progress,
    (first, total, width, height): (Frame, u32, u32, u32),
    audio_path: Option<&Path>,
) -> Result<(), ExportError> {
    let fps = scene.settings.frame_rate;
    let rate = format!("{}/{}", fps.num, fps.den);
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.args(["-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{width}x{height}")])
        .args(["-r", &rate, "-i", "-"]);
    if let Some(a) = audio_path {
        cmd.args(["-f", "f32le", "-ar", &SAMPLE_RATE.to_string(), "-ac", "2", "-i"]).arg(a);
    }
    // yuv420p は幅・高さが偶数である必要がある
    cmd.args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2"]);
    match opts.codec {
        Codec::H264 => cmd.args(["-c:v", "libx264", "-preset", "medium"]),
        Codec::H265 => cmd.args(["-c:v", "libx265", "-preset", "medium", "-tag:v", "hvc1"]),
    };
    cmd.args(["-crf", &opts.crf.to_string(), "-pix_fmt", "yuv420p"]);
    if audio_path.is_some() {
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-shortest"]);
    }
    cmd.args(["-movflags", "+faststart"]).arg(&opts.output);
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let mut stdin = child.stdin.take().expect("stdin は piped");

    // ffmpeg のエラー出力は別スレッドで読み集める（パイプが詰まって止まるのを防ぐ）
    let mut stderr = child.stderr.take().expect("stderr は piped");
    let err_thread = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });

    let mut outcome: Result<(), ExportError> = Ok(());
    for i in 0..total {
        if !progress(i, total) {
            outcome = Err(ExportError::Cancelled);
            break;
        }
        let frame = match renderer.render(scene, first + i, media) {
            Ok(f) => f,
            Err(e) => {
                outcome = Err(e.into());
                break;
            }
        };
        if let Err(e) = stdin.write_all(&frame.pixels) {
            // ffmpeg が先に終了した（エラー内容は終了後に取得する）
            log::warn!("ffmpeg への書き込みに失敗: {e}");
            break;
        }
    }
    drop(stdin);
    let status = child.wait()?;
    let stderr_text = err_thread.join().unwrap_or_default();
    match outcome {
        Err(e) => Err(e),
        Ok(()) if !status.success() => Err(ExportError::Encoder(status.code(), stderr_text)),
        Ok(()) => {
            progress(total, total);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au2_core::{SceneSettings, TimelineObject};
    use au2_render::{Gpu, NoMedia};

    #[test]
    fn range_is_resolved_against_scene_length() {
        let s = Scene::new("t", SceneSettings { length: 100, ..Default::default() });
        assert_eq!(resolve_range(&s, None), Some((0, 99)));
        assert_eq!(resolve_range(&s, Some((10, 20))), Some((10, 20)));
        assert_eq!(resolve_range(&s, Some((20, 10))), None);
    }

    /// 実際に書き出して ffprobe 相当の確認をする。GPU か ffmpeg が無ければスキップ。
    #[test]
    fn exports_a_playable_mp4_with_audio() {
        let (Ok(gpu), Some(_)) = (Gpu::headless(), au2_media::locate_ffmpeg()) else {
            eprintln!("GPU か ffmpeg が無いためスキップ");
            return;
        };
        let mut renderer = Renderer::new(gpu);
        let mut scene = Scene::new("t", SceneSettings { width: 64, height: 36, length: 30, ..Default::default() });
        scene.insert_object(0, TimelineObject::new_shape(0, 29, 0, "ff0000"));
        // 音声は手作りの WAV（0.5 秒 8bit 無音以外）を使う
        let wav = std::env::temp_dir().join(format!("au2-export-test-{}.wav", std::process::id()));
        std::fs::write(&wav, tiny_wav()).unwrap();
        scene.insert_object(1, TimelineObject::new_audio(0, 29, wav.to_string_lossy()));

        let out = std::env::temp_dir().join(format!("au2-export-test-{}.mp4", std::process::id()));
        let mut seen = Vec::new();
        let mut progress = |done: u32, total: u32| {
            seen.push((done, total));
            true
        };
        let opts = ExportOptions::new(&out);
        export_video(&scene, &opts, &mut renderer, &mut NoMedia, &|s| PathBuf::from(s), &mut progress).unwrap();
        assert!(std::fs::metadata(&out).unwrap().len() > 500, "ファイルが小さすぎる");
        assert_eq!(seen.first(), Some(&(0, 30)));
        assert_eq!(seen.last(), Some(&(30, 30)));

        // ffmpeg 自身で読み戻し: 映像フレーム数と音声ストリームの有無
        let ffmpeg = au2_media::locate_ffmpeg().unwrap();
        let info = Command::new(&ffmpeg).args(["-hide_banner", "-i"]).arg(&out).output().unwrap();
        let text = String::from_utf8_lossy(&info.stderr).into_owned();
        assert!(text.contains("Video: h264"), "{text}");
        assert!(text.contains("64x36"), "{text}");
        assert!(text.contains("Audio: aac"), "{text}");
        let decoded = Command::new(&ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        assert_eq!(decoded.stdout.len(), 64 * 36 * 4 * 30, "30 フレーム出ている");
        let (r, g, b) = (decoded.stdout[0], decoded.stdout[1], decoded.stdout[2]);
        assert!(r > 200 && g < 60 && b < 60, "赤いまま: {r},{g},{b}");

        std::fs::remove_file(&wav).ok();
        std::fs::remove_file(&out).ok();
    }

    #[test]
    fn cancel_stops_and_removes_output() {
        let (Ok(gpu), Some(_)) = (Gpu::headless(), au2_media::locate_ffmpeg()) else { return };
        let mut renderer = Renderer::new(gpu);
        let mut scene = Scene::new("t", SceneSettings { width: 32, height: 32, length: 30, ..Default::default() });
        scene.insert_object(0, TimelineObject::new_shape(0, 29, 0, "00ff00"));
        let out = std::env::temp_dir().join(format!("au2-export-cancel-{}.mp4", std::process::id()));
        let mut progress = |done: u32, _| done < 5;
        let r = export_video(
            &scene,
            &ExportOptions::new(&out),
            &mut renderer,
            &mut NoMedia,
            &|s| PathBuf::from(s),
            &mut progress,
        );
        assert!(matches!(r, Err(ExportError::Cancelled)), "{r:?}");
        assert!(!out.exists());
    }

    fn tiny_wav() -> Vec<u8> {
        let data: Vec<u8> = (0..4000).map(|i| if (i / 20) % 2 == 0 { 220 } else { 36 }).collect(); // 矩形波
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }
}
