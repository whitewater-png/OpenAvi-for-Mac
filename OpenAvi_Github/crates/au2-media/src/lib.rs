//! # au2-media
//!
//! OpenAvi の画像・動画デコードおよびフレームキャッシュモジュール。
//!
//! - **画像ファイル**: `.png`, `.jpg`, `.jpeg`, `.bmp`, `.webp`, `.gif` を `image` クレートでネイティブデコード。
//! - **アニメーション GIF**: 全フレームを展開し、タイムラインのフレーム位置に応じてアニメーション再生。
//! - **動画ファイル**: `.mp4`, `.avi`, `.mov`, `.mkv`, `.webm`, `.wmv` 等を `ffmpeg` パイプライン（PATH・Homebrew 等の標準パス・WinGet 領域から自動検出）で RGBA フレームへデコードし、メモリキャッシュします。

mod stream;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use image::AnimationDecoder;

/// RGBA8 ピクセルバッファ。
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// ファイル拡張子からメディア種別を判別します。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

impl MediaKind {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "bmp" | "webp" => Some(Self::Image),
            "gif" | "mp4" | "avi" | "mov" | "mkv" | "webm" | "wmv" | "m4v" => Some(Self::Video),
            "wav" | "mp3" | "ogg" | "flac" | "aac" | "m4a" => Some(Self::Audio),
            _ => None,
        }
    }
}

/// 静止画またはアニメーション GIF のキャッシュエントリ。
enum CachedAsset {
    Static(RgbaFrame),
    AnimatedGif(Vec<RgbaFrame>),
    VideoInfo {
        width: u32,
        height: u32,
        /// 動画のネイティブフレームレート（時刻 → フレーム番号の変換に使う）
        fps: f64,
        /// 常駐デコーダ（再生・コマ送りの連続アクセス用）
        stream: Option<stream::VideoStream>,
        frames: HashMap<u32, RgbaFrame>,
    },
    Failed,
}

/// メディアデコーダ＆キャッシュ。
pub struct MediaCache {
    assets: HashMap<PathBuf, CachedAsset>,
    ffmpeg_bin: Option<PathBuf>,
    ffprobe_bin: Option<PathBuf>,
}

impl Default for MediaCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaCache {
    pub fn new() -> Self {
        let (ffmpeg_bin, ffprobe_bin) = locate_ffmpeg_tools();
        if let Some(p) = &ffmpeg_bin {
            log::info!("FFmpeg 検出: {}", p.display());
        }
        Self {
            assets: HashMap::new(),
            ffmpeg_bin,
            ffprobe_bin,
        }
    }

    pub fn has_ffmpeg(&self) -> bool {
        self.ffmpeg_bin.is_some()
    }

    /// 静止画または動画の指定フレーム（秒単位およびフレーム番号）の [`RgbaFrame`] を取得します。
    pub fn get_frame(&mut self, path: &Path, frame_index: u32, time_sec: f64) -> Option<&RgbaFrame> {
        if !path.exists() {
            return None;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();

        if !self.assets.contains_key(path) {
            let asset = match ext.as_str() {
                "gif" => load_gif_or_static(path),
                "png" | "jpg" | "jpeg" | "bmp" | "webp" => load_static_image(path),
                _ => self.init_video_asset(path),
            };
            self.assets.insert(path.to_path_buf(), asset);
        }

        // 動画の場合、要求フレームが未キャッシュならデコード
        let missing = matches!(
            self.assets.get(path),
            Some(CachedAsset::VideoInfo { frames, .. }) if !frames.contains_key(&frame_index)
        );
        if missing {
            if let Some(ffmpeg) = self.ffmpeg_bin.clone() {
                if let Some(CachedAsset::VideoInfo { width, height, fps, stream, frames }) = self.assets.get_mut(path) {
                    if let Some(decoded) = decode_video_frame(&ffmpeg, path, time_sec, *fps, (*width, *height), stream) {
                        // メモリ上限保護（最大 180 フレーム保持）
                        if frames.len() >= 180 {
                            frames.clear();
                        }
                        frames.insert(frame_index, decoded);
                    }
                }
            }
        }

        match self.assets.get(path)? {
            CachedAsset::Static(frame) => Some(frame),
            CachedAsset::AnimatedGif(frames) if !frames.is_empty() => {
                Some(&frames[(frame_index as usize) % frames.len()])
            }
            CachedAsset::VideoInfo { frames, .. } => frames.get(&frame_index),
            _ => None,
        }
    }

    fn init_video_asset(&self, path: &Path) -> CachedAsset {
        let Some(ffmpeg) = &self.ffmpeg_bin else {
            return CachedAsset::Failed;
        };
        let (width, height, fps) = self
            .ffprobe_bin
            .as_ref()
            .and_then(|probe| probe_video(probe, path))
            .unwrap_or((640, 360, 30.0));

        let mut frames = HashMap::new();
        if let Some(first) = decode_video_frame_ffmpeg(ffmpeg, path, 0.0, width, height) {
            frames.insert(0, first);
            CachedAsset::VideoInfo { width, height, fps, stream: None, frames }
        } else {
            CachedAsset::Failed
        }
    }
}

fn load_static_image(path: &Path) -> CachedAsset {
    match image::open(path) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            CachedAsset::Static(RgbaFrame {
                width: rgba.width(),
                height: rgba.height(),
                pixels: rgba.into_raw(),
            })
        }
        Err(e) => {
            log::warn!("画像の読み込み失敗 ({}): {e}", path.display());
            CachedAsset::Failed
        }
    }
}

fn load_gif_or_static(path: &Path) -> CachedAsset {
    if let Ok(file) = std::fs::File::open(path) {
        let reader = std::io::BufReader::new(file);
        if let Ok(decoder) = image::codecs::gif::GifDecoder::new(reader) {
            if let Ok(frames) = decoder.into_frames().collect_frames() {
                let list: Vec<RgbaFrame> = frames
                    .into_iter()
                    .map(|f| {
                        let buf = f.into_buffer();
                        RgbaFrame {
                            width: buf.width(),
                            height: buf.height(),
                            pixels: buf.into_raw(),
                        }
                    })
                    .collect();
                if list.len() == 1 {
                    return CachedAsset::Static(list.into_iter().next().unwrap());
                } else if !list.is_empty() {
                    return CachedAsset::AnimatedGif(list);
                }
            }
        }
    }
    load_static_image(path)
}

fn locate_ffmpeg_tools() -> (Option<PathBuf>, Option<PathBuf>) {
    // 1. PATH から検索
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
    {
        return (Some(PathBuf::from("ffmpeg")), Some(PathBuf::from("ffprobe")));
    }

    // 2. macOS / Linux の標準インストール先を検索
    //    Finder から .app として起動すると PATH にシェル設定 (Homebrew 等) が反映されないため
    #[cfg(unix)]
    {
        const DIRS: [&str; 4] = ["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin", "/usr/bin"];
        let runs = |bin: &Path| {
            Command::new(bin)
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let find = |name: &str| DIRS.iter().map(|d| Path::new(d).join(name)).find(|p| runs(p));
        if let Some(bin) = find("ffmpeg") {
            return (Some(bin), find("ffprobe"));
        }
    }

    // 3. Windows WinGet パッケージ領域を検索
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let pkg_dir = PathBuf::from(local_app_data).join("Microsoft\\WinGet\\Packages");
        if let Ok(entries) = std::fs::read_dir(pkg_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name.contains("ffmpeg") {
                    if let Some(bin_dir) = find_file_recursive(&entry.path(), "ffmpeg.exe", 4) {
                        let probe = bin_dir.parent().unwrap().join("ffprobe.exe");
                        return (Some(bin_dir), probe.exists().then_some(probe));
                    }
                }
            }
        }
    }

    (None, None)
}

fn find_file_recursive(dir: &Path, target_name: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(target_name)) {
            return Some(path);
        } else if path.is_dir() {
            if let Some(found) = find_file_recursive(&path, target_name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// 動画の (幅, 高さ, フレームレート)。幅・高さはプレビュー用に縮小後の値。
fn probe_video(ffprobe: &Path, video_path: &Path) -> Option<(u32, u32, f64)> {
    let mut cmd = Command::new(ffprobe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "csv=s=x:p=0",
        ])
        .arg(video_path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    let mut parts = line.split('x');
    let width: u32 = parts.next()?.parse().ok()?;
    let height: u32 = parts.next()?.parse().ok()?;
    let fps = parts
        .next()
        .and_then(|r| {
            let (n, d) = r.split_once('/')?;
            Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?)
        })
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(30.0);
    // プレビュー負荷軽減のため長辺 960px 以下にスケーリング
    if width > 960 {
        let scaled_h = (height as f64 * 960.0 / width as f64).round() as u32;
        Some((960, (scaled_h.max(2) / 2) * 2, fps))
    } else {
        Some((width.max(2), height.max(2), fps))
    }
}

/// 常駐デコーダでフレームを取得します。後戻りや大きなシークでは再起動し、起動できなければ 1 枚だけ個別にデコードします。
fn decode_video_frame(
    ffmpeg: &Path,
    path: &Path,
    time_sec: f64,
    fps: f64,
    (width, height): (u32, u32),
    stream: &mut Option<stream::VideoStream>,
) -> Option<RgbaFrame> {
    let index = (time_sec.max(0.0) * fps).round() as u64;
    if let Some(frame) = stream.as_mut().and_then(|s| s.frame_at(index)) {
        return Some(frame);
    }
    // 終端を越えるシークでは新しいストリームが 1 枚も出さないので、直前に出していた最後のフレームを保持表示する
    let held = stream.as_ref().and_then(|s| s.last_frame());
    *stream = stream::VideoStream::spawn(ffmpeg, path, index, fps, width, height);
    match stream.as_mut().and_then(|s| s.frame_at(index)) {
        Some(frame) => Some(frame),
        None => {
            *stream = None;
            held.or_else(|| decode_video_frame_ffmpeg(ffmpeg, path, time_sec, width, height))
        }
    }
}

fn decode_video_frame_ffmpeg(
    ffmpeg: &Path,
    video_path: &Path,
    time_sec: f64,
    width: u32,
    height: u32,
) -> Option<RgbaFrame> {
    let expected_len = (width * height * 4) as usize;
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .args(["-ss", &format!("{time_sec:.3}"), "-i"])
        .arg(video_path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={width}:{height}"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-loglevel",
            "error",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut buf = Vec::with_capacity(expected_len);
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_end(&mut buf);
    }
    let _ = child.wait();

    if buf.len() >= expected_len {
        buf.truncate(expected_len);
        Some(RgbaFrame { width, height, pixels: buf })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_detection() {
        assert_eq!(MediaKind::from_path(Path::new("a.png")), Some(MediaKind::Image));
        assert_eq!(MediaKind::from_path(Path::new("b.MP4")), Some(MediaKind::Video));
        assert_eq!(MediaKind::from_path(Path::new("c.wav")), Some(MediaKind::Audio));
    }

    #[test]
    fn decode_sample_image_and_video() {
        let mut cache = MediaCache::new();
        let img_path = Path::new("../../samples/sample_image.png");
        let img_frame = cache.get_frame(img_path, 0, 0.0).expect("decode sample_image.png");
        assert_eq!((img_frame.width, img_frame.height), (240, 160));
        assert_eq!(img_frame.pixels.len(), 240 * 160 * 4);

        if cache.has_ffmpeg() {
            let vid_path = Path::new("../../samples/sample_video.mp4");
            let vid_frame = cache.get_frame(vid_path, 15, 0.5).expect("decode sample_video.mp4");
            assert_eq!((vid_frame.width, vid_frame.height), (320, 180));
            assert_eq!(vid_frame.pixels.len(), 320 * 180 * 4);
        }
    }

    #[test]
    fn streaming_decode_matches_single_frame_decode() {
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = Path::new("../../samples/sample_video.mp4");
        let fps = 30.0;
        // 前進（連続・読み捨てを含む）→ 後戻り → 終端超過の順にアクセス
        let order: Vec<u32> = [0, 1, 2, 3, 10, 11, 5, 40, 41, 59, 200].to_vec();
        for &i in &order {
            let t = i as f64 / fps;
            let streamed = cache.get_frame(path, i, t).unwrap_or_else(|| panic!("frame {i}")).clone();
            assert_eq!((streamed.width, streamed.height), (320, 180));
            let reference = decode_video_frame_ffmpeg(&ffmpeg, path, (t.min(59.0 / fps) - 0.5 / fps).max(0.0), 320, 180).unwrap();
            let max_diff = streamed.pixels.iter().zip(&reference.pixels).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(max_diff <= 2, "frame {i}: 単独デコードと {max_diff} ずれている");
        }
    }
}
