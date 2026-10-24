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
    #[error("書き出しは完了しましたが、出力先を置き換えられませんでした（他のアプリが開いている可能性があります。既存のファイルは変更されている場合があります）。完成したファイルは {} に残してあります: {source}", part.display())]
    Finalize { part: PathBuf, source: std::io::Error },
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

    // 音声: 30 秒ずつミックスして生の f32 PCM を一時ファイルへ書く（長尺でもメモリを使い切らない）。無音なら付けない
    // 音声の下ごしらえ中もキャンセルできるよう、進捗コールバック（0 / 全体）を節ごとに呼ぶ
    let audio = if opts.audio { write_audio(scene, first, last, resolve, &mut || progress(0, total))? } else { None };

    // 出力は `.part` に書き、成功したときだけ本来の名前へ移す（失敗・キャンセルで既存ファイルを壊さない）
    let part = part_path(&opts.output);
    let result = run_ffmpeg(
        &ffmpeg,
        scene,
        opts,
        &part,
        renderer,
        media,
        progress,
        (first, total, width, height),
        audio.as_ref().map(|t| t.0.as_path()),
    );
    // 完成した `.part` を本来の名前へ移す。Windows では、出力先を別のプロセス（再生中のプレーヤーや、取り込み済みの
    // 動画を掴んでいるデコーダ）が開いていると置き換えられない。その場合も、苦労して書き出した結果は捨てない
    let result = result.and_then(|()| match std::fs::rename(&part, &opts.output) {
        Ok(()) => Ok(()),
        Err(rename_error) => match std::fs::copy(&part, &opts.output) {
            Ok(_) => {
                let _ = std::fs::remove_file(&part);
                Ok(())
            }
            Err(_) => Err(ExportError::Finalize { part: part.clone(), source: rename_error }),
        },
    });
    // 失敗・キャンセルでは中途半端な `.part` を消す。ただし、完成していたのに移せなかった場合だけは残す
    if result.is_err() && !matches!(result, Err(ExportError::Finalize { .. })) {
        let _ = std::fs::remove_file(&part);
    }
    result
}

/// 削除を保証する一時ファイル。
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 途中で抜けても（エラー・パニック）ffmpeg を残さない。
struct KillOnDrop(Option<std::process::Child>);

impl KillOnDrop {
    /// 今すぐ止める（キャンセル・エラー時に、ffmpeg が溜まった入力を処理し終えるのを待たない）。
    fn kill_now(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
        }
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let mut child = self.0.take().expect("wait は 1 回だけ");
        child.wait()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn part_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".part");
    output.with_file_name(name)
}

/// 範囲の音声をミックスして一時ファイル（f32le・48kHz・ステレオ）に書きます。全区間が無音なら `None`。
fn write_audio(
    scene: &Scene,
    first: Frame,
    last: Frame,
    resolve: &dyn Fn(&str) -> PathBuf,
    keep_going: &mut dyn FnMut() -> bool,
) -> Result<Option<TempFile>, ExportError> {
    let fps = scene.settings.frame_rate.as_f64();
    let chunk_frames = ((30.0 * fps) as Frame).max(1);
    // 同じプロセス内で複数の書き出しが並んでも（テストなど）ぶつからないよう連番を付ける
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("openavi-export-{}-{n}.f32", std::process::id()));
    let file = TempFile(path);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&file.0)?);
    let mut cache = AudioCache::new();
    let mut any_sound = false;
    let mut start = first;
    while start <= last {
        if !keep_going() {
            return Err(ExportError::Cancelled);
        }
        let end = (start + chunk_frames).min(last + 1);
        let pcm = au2_audio::mix(scene, &mut cache, start, end, resolve);
        any_sound |= pcm.iter().any(|&v| v != 0.0);
        for v in &pcm {
            out.write_all(&v.to_le_bytes())?;
        }
        start = end;
    }
    out.flush()?;
    Ok(any_sound.then_some(file))
}
#[allow(clippy::too_many_arguments)]
fn run_ffmpeg(
    ffmpeg: &Path,
    scene: &Scene,
    opts: &ExportOptions,
    part: &Path,
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
    // RGB → YUV は BT.709（HD の標準）で変換し、プレーヤーが同じ行列で戻せるようタグも付ける
    cmd.args(["-vf", "pad=ceil(iw/2)*2:ceil(ih/2)*2,scale=out_color_matrix=bt709:out_range=tv,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv"]);
    cmd.args(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]);
    match opts.codec {
        Codec::H264 => cmd.args(["-c:v", "libx264", "-preset", "medium"]),
        Codec::H265 => cmd.args(["-c:v", "libx265", "-preset", "medium", "-tag:v", "hvc1"]),
    };
    cmd.args(["-crf", &opts.crf.to_string(), "-pix_fmt", "yuv420p"]);
    if audio_path.is_some() {
        cmd.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    cmd.args(["-f", "mp4", "-movflags", "+faststart"]).arg(part);
    let child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()?;
    let mut child = KillOnDrop(Some(child));
    let mut stdin = child.0.as_mut().and_then(|c| c.stdin.take()).expect("stdin は piped");

    // ffmpeg のエラー出力は別スレッドで読み集める（パイプが詰まって止まるのを防ぐ）
    let mut stderr = child.0.as_mut().and_then(|c| c.stderr.take()).expect("stderr は piped");
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
    if outcome.is_err() {
        child.kill_now();
    }
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
        let _wav_cleanup = TempFile(wav.clone());
        scene.insert_object(1, TimelineObject::new_audio(0, 29, wav.to_string_lossy()));

        let out = std::env::temp_dir().join(format!("au2-export-test-{}.mp4", std::process::id()));
        let _out_cleanup = TempFile(out.clone()); // アサーションが失敗しても一時ファイルを残さない
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
        // 色空間のタグ（HD の標準 BT.709）が付いている
        let probe = Command::new(ffmpeg.with_file_name("ffprobe"))
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .arg("stream=color_space,color_primaries,color_transfer,color_range")
            .args(["-of", "default=nw=1"])
            .arg(&out)
            .output();
        if let Ok(p) = probe {
            let tags = String::from_utf8_lossy(&p.stdout).into_owned();
            for expected in ["color_space=bt709", "color_primaries=bt709", "color_transfer=bt709", "color_range=tv"] {
                assert!(tags.contains(expected), "{expected} が無い: {tags}");
            }
        }
        let decoded = Command::new(&ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        assert_eq!(decoded.stdout.len(), 64 * 36 * 4 * 30, "30 フレーム出ている");
        // 純赤の輝度: BT.709 なら Y≈63、BT.601 なら Y≈81（行列が本当に切り替わっていることの確認）
        let yuv = Command::new(&ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&out)
            .args(["-an", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
            .output()
            .unwrap();
        let y = yuv.stdout[0] as i32;
        assert!((y - 63).abs() <= 3, "BT.709 の赤の Y≈63 のはずが {y}");
        let (r, g, b) = (decoded.stdout[0], decoded.stdout[1], decoded.stdout[2]);
        assert!(r > 200 && g < 60 && b < 60, "赤いまま: {r},{g},{b}");

    }

    #[test]
    fn cancel_stops_and_removes_output() {
        let (Ok(gpu), Some(_)) = (Gpu::headless(), au2_media::locate_ffmpeg()) else { return };
        let mut renderer = Renderer::new(gpu);
        let mut scene = Scene::new("t", SceneSettings { width: 32, height: 32, length: 30, ..Default::default() });
        scene.insert_object(0, TimelineObject::new_shape(0, 29, 0, "00ff00"));
        let out = std::env::temp_dir().join(format!("au2-export-cancel-{}.mp4", std::process::id()));
        std::fs::write(&out, b"previous export").unwrap(); // 既存ファイルはキャンセルしても壊れない
        let _out_cleanup = TempFile(out.clone());
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
        assert_eq!(std::fs::read(&out).unwrap(), b"previous export", "既存ファイルが残っている");
        assert!(!part_path(&out).exists(), ".part が残っていない");
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

    #[test]
    fn a_finished_export_is_kept_as_part_when_the_destination_cannot_be_replaced() {
        let (Ok(gpu), Some(_)) = (Gpu::headless(), au2_media::locate_ffmpeg()) else { return };
        let mut renderer = Renderer::new(gpu);
        let mut scene = Scene::new("t", SceneSettings { width: 32, height: 32, length: 5, ..Default::default() });
        scene.insert_object(0, TimelineObject::new_shape(0, 4, 0, "0000ff"));
        // 出力先が（中身のある）ディレクトリで、置き換えられない状況を作る
        let dir = std::env::temp_dir().join(format!("au2-export-finalize-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("out.mp4")).unwrap();
        std::fs::write(dir.join("out.mp4").join("keep"), b"x").unwrap();
        let _cleanup = TempDir(dir.clone());
        let out = dir.join("out.mp4");
        let mut progress = |_: u32, _: u32| true;
        let r = export_video(&scene, &ExportOptions::new(&out), &mut renderer, &mut NoMedia, &|s| PathBuf::from(s), &mut progress);
        match r {
            Err(ExportError::Finalize { part, .. }) => {
                assert!(part.exists(), "完成した書き出しは捨てずに残す: {}", part.display());
                assert!(std::fs::metadata(&part).unwrap().len() > 100);
            }
            other => panic!("Finalize エラーのはず: {other:?}"),
        }
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
