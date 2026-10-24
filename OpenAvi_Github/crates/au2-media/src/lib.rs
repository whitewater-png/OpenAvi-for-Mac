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
use std::time::{Duration, Instant};
use std::process::{Command, Stdio};

use image::AnimationDecoder;

/// RGBA8 ピクセルバッファ。
#[derive(Debug, Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbaFrame {
    /// どちらかの辺が `max` を超えるなら、縦横比を保って収まるよう縮小した画像を返す（最近傍）。収まっていれば `None`。
    pub fn fit_within(&self, max: u32) -> Option<RgbaFrame> {
        let max = max.max(1);
        if self.width <= max && self.height <= max {
            return None;
        }
        let k = max as f64 / self.width.max(self.height) as f64;
        let w = ((self.width as f64 * k).floor() as u32).clamp(1, max);
        let h = ((self.height as f64 * k).floor() as u32).clamp(1, max);
        let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
        for y in 0..h {
            let sy = ((y as u64 * self.height as u64) / h as u64) as usize;
            for x in 0..w {
                let sx = ((x as u64 * self.width as u64) / w as u64) as usize;
                let i = (sy * self.width as usize + sx) * 4;
                pixels.extend_from_slice(&self.pixels[i..i + 4]);
            }
        }
        Some(RgbaFrame { width: w, height: h, pixels })
    }
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
// 動画は状態が多いぶん大きいが、HashMap の値として 1 本につき 1 つしか持たないので、Box 化の手間に見合わない
#[allow(clippy::large_enum_variant)]
enum CachedAsset {
    Static(RgbaFrame),
    AnimatedGif(Vec<RgbaFrame>),
    VideoInfo {
        /// デコード後（縮小後）のサイズ
        width: u32,
        height: u32,
        /// 動画本来のサイズ（描画上の大きさはこちらで決める）
        native: (u32, u32),
        /// 最後にアクセスした時刻（使われなくなった常駐デコーダを止める判断に使う）
        last_used: Instant,
        /// 総フレーム数（ffprobe の長さから算出）。終端を越える要求を最後のフレームに丸める
        frame_count: Option<u64>,
        /// 動画の最後のフレーム（終端を越えるシークに使う。必要になるまで取得しない）
        tail: Option<Option<RgbaFrame>>,
        /// これ以降のフレーム番号は映像の終端を越えている（新しいストリームが 1 枚も出なかった最小の番号）。
        /// 長さが分からない動画（MKV/WebM は映像ストリームの長さを持たない）でも、毎フレーム ffmpeg を起動しないための印
        past_end: Option<u64>,
        /// ffprobe でフレーム数を調べようとしたか（失敗しても、毎フレーム全体を走査し直さない）
        count_probed: bool,
        /// デコードに失敗した動画は、この時刻まで再試行しない
        retry_after: Option<Instant>,
        /// 映像ストリームの開始時刻[秒]（ストリーム起動時の fps フィルタの枠を決める）
        stream_start: f64,
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
    /// 動画をデコードする最大の幅。プレビューは軽さを優先して小さく、書き出しは原寸にする。
    max_decode_width: u32,
    /// この時間使われなかった常駐デコーダは、数が多いときに止める
    stream_idle: Duration,
    /// デコード失敗後に再試行を待つか。書き出しでは待たない（待つとその間のフレームがプレースホルダになるため）
    retry_backoff: bool,
    /// 常駐デコーダを起動した回数（テスト・診断用）
    stream_spawns: u64,
}

/// 常駐 ffmpeg デコーダがこの本数を超えたら、使われていないものから止める。
/// 画面に同時に出ている動画は止めない（止めると毎フレーム起動し直して固まる）。
const MAX_LIVE_STREAMS: usize = 4;
/// 1 本の動画でキャッシュするデコード済みフレームの上限（枚数・バイト数のどちらでも）。
const MAX_CACHED_FRAMES: usize = 180;
const MAX_CACHED_BYTES: usize = 256 * 1024 * 1024;
/// デコードに失敗した動画を再試行するまでの待ち時間。
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(1);

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
            max_decode_width: 960,
            stream_idle: Duration::from_secs(3),
            retry_backoff: true,
            stream_spawns: 0,
        }
    }

    /// デコード失敗後の再試行待ち（既定: 待つ）を切り替えます。
    pub fn set_retry_backoff(&mut self, enabled: bool) {
        self.retry_backoff = enabled;
    }

    /// 動画をデコードする最大の幅を変えます（以後に読み込む動画に効く）。書き出しでは `u32::MAX` で原寸。
    pub fn set_max_decode_width(&mut self, width: u32) {
        self.max_decode_width = width.max(2);
    }

    /// 動画（映像）の長さ[秒]。まだ読み込んでいない・長さが分からない場合は `None`。
    pub fn duration_secs(&self, path: &Path) -> Option<f64> {
        match self.assets.get(path)? {
            CachedAsset::VideoInfo { frame_count: Some(count), past_end, fps, .. } => {
                // コンテナの長さが映像より長いと過大になるので、実際に終端が確認できた位置があればそちらを優先する
                let frames = past_end.map_or(*count, |end| (*count).min(end));
                Some(frames as f64 / *fps)
            }
            _ => None,
        }
    }

    /// 動画本来のサイズ。デコード済みフレームが縮小されていても、描画上の大きさはこれで決める。
    pub fn native_size(&self, path: &Path) -> Option<(u32, u32)> {
        match self.assets.get(path)? {
            CachedAsset::VideoInfo { native, .. } => Some(*native),
            _ => None,
        }
    }

    pub fn has_ffmpeg(&self) -> bool {
        self.ffmpeg_bin.is_some()
    }

    /// 動画の長さ・サイズを調べる ffprobe があるか。
    pub fn has_ffprobe(&self) -> bool {
        self.ffprobe_bin.is_some()
    }

    /// 静止画または動画の指定フレーム（秒単位およびフレーム番号）の [`RgbaFrame`] を取得します。
    pub fn get_frame(&mut self, path: &Path, frame_index: u32, time_sec: f64) -> Option<&RgbaFrame> {
        if !path.exists() {
            return None;
        }
        self.ensure_asset(path);

        // 動画のフレームは「ネイティブのフレーム番号」でキャッシュする。呼び出し側のフレーム番号（プロジェクトの
        // フレーム）で持つと、プロジェクトのフレームレートが変わったとき同じ番号が別の時刻を指してしまう。
        let key = match self.assets.get(path) {
            Some(CachedAsset::VideoInfo { fps, frame_count, .. }) => native_index(time_sec, *fps, *frame_count) as u32,
            _ => frame_index,
        };
        let now = Instant::now();
        let backoff = self.retry_backoff;
        let missing = match self.assets.get_mut(path) {
            Some(CachedAsset::VideoInfo { frames, last_used, retry_after, .. }) => {
                *last_used = now;
                // 直前にデコードに失敗した動画は、少し待ってから再試行する（毎フレーム ffmpeg を起動し続けないため）
                !frames.contains_key(&key) && (!backoff || retry_after.is_none_or(|t| now >= t))
            }
            _ => false,
        };
        if missing {
            if let Some(ffmpeg) = self.ffmpeg_bin.clone() {
                if let Some(CachedAsset::VideoInfo {
                    width, height, fps, stream, frames, frame_count, tail, past_end, count_probed, retry_after, stream_start, ..
                }) = self.assets.get_mut(path)
                {
                    let target = DecodeTarget { time_sec, fps: *fps, size: (*width, *height), stream_start: *stream_start };
                    let mut state = DecodeState {
                        stream,
                        tail,
                        past_end,
                        frame_count,
                        count_probed,
                        ffprobe: self.ffprobe_bin.as_deref(),
                        spawns: &mut self.stream_spawns,
                    };
                    let decoded = match decode_video_frame(&ffmpeg, path, target, &mut state) {
                        Decoded::Frame(f) => Some(f),
                        // 終端越え: 最後のフレーム（取得できていれば）を表示する。失敗ではない
                        Decoded::PastEnd(last) => last,
                        Decoded::Failed => {
                            *retry_after = backoff.then(|| now + RETRY_AFTER_FAILURE);
                            None
                        }
                    };
                    if let Some(decoded) = decoded {
                        // メモリ上限保護（枚数とバイト数。原寸の 4K ではすぐ数 GB になるため）
                        let frame_bytes = decoded.pixels.len();
                        if frames.len() >= MAX_CACHED_FRAMES || (frames.len() + 1) * frame_bytes > MAX_CACHED_BYTES {
                            frames.clear();
                        }
                        frames.insert(key, decoded);
                    }
                }
            }
            self.limit_live_streams(path, now);
        }

        match self.assets.get(path)? {
            CachedAsset::Static(frame) => Some(frame),
            CachedAsset::AnimatedGif(frames) if !frames.is_empty() => {
                Some(&frames[(frame_index as usize) % frames.len()])
            }
            CachedAsset::VideoInfo { frames, .. } => frames.get(&key),
            _ => None,
        }
    }

    /// ファイルを開いて（画像は読み込み、動画は先頭フレームのデコードと長さの調査をして）キャッシュに載せます。
    /// フレームを要求せずに、動画の長さ・本来のサイズだけ知りたいときに使う。すでに開いていれば何もしない。
    pub fn prepare(&mut self, path: &Path) {
        if path.exists() {
            self.ensure_asset(path);
        }
    }

    fn ensure_asset(&mut self, path: &Path) {
        if self.assets.contains_key(path) {
            return;
        }
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
        let asset = match ext.as_str() {
            "gif" => load_gif_or_static(path),
            "png" | "jpg" | "jpeg" | "bmp" | "webp" => load_static_image(path),
            _ => self.init_video_asset(path),
        };
        self.assets.insert(path.to_path_buf(), asset);
    }

    /// 常駐デコーダが上限を超えたら、`keep` 以外で最も古く使われたものを止める。
    fn limit_live_streams(&mut self, keep: &Path, now: Instant) {
        loop {
            let live: Vec<(PathBuf, Instant)> = self
                .assets
                .iter()
                .filter_map(|(p, a)| match a {
                    CachedAsset::VideoInfo { stream: Some(_), last_used, .. } => Some((p.clone(), *last_used)),
                    _ => None,
                })
                .collect();
            if live.len() <= MAX_LIVE_STREAMS {
                return;
            }
            // 画面に出ている（最近使われた）動画は止めない。止める候補は idle を過ぎたものだけ
            let idle = self.stream_idle;
            let candidate = live
                .into_iter()
                .filter(|(p, used)| p.as_path() != keep && now.duration_since(*used) >= idle)
                .min_by_key(|(_, used)| *used);
            let Some((oldest, _)) = candidate else { return };
            if let Some(CachedAsset::VideoInfo { stream, .. }) = self.assets.get_mut(&oldest) {
                *stream = None;
            }
        }
    }

    fn init_video_asset(&self, path: &Path) -> CachedAsset {
        let Some(ffmpeg) = &self.ffmpeg_bin else {
            return CachedAsset::Failed;
        };
        let VideoProbe { width: native_w, height: native_h, fps, duration, start_time } = self
            .ffprobe_bin
            .as_ref()
            .and_then(|probe| probe_video(probe, path))
            .unwrap_or(VideoProbe { width: 640, height: 360, fps: 30.0, duration: None, start_time: 0.0 });
        let (width, height) = scaled_size((native_w, native_h), self.max_decode_width);
        // MKV/WebM は映像ストリームの長さを持たず、ffprobe の r_frame_rate が平均レートと同じになる。
        // 可変フレームレートの録画（静止区間が多い）だと、そのレートは実際より大幅に低く、動く場面がコマ落ちしてしまう。
        // 最初の数百パケットの時刻の間隔から本来のレートを推定し、申告レートと大きく違うときだけそちらを採る
        let fps = match (duration, self.ffprobe_bin.as_deref()) {
            (None, Some(probe)) => probe_packet_times(probe, path, 500)
                .and_then(|times| estimate_fps(&times))
                .filter(|estimate| (estimate / fps - 1.0).abs() > 0.1)
                .unwrap_or(fps),
            _ => fps,
        };

        let mut frames = HashMap::new();
        if let Some(first) = decode_video_frame_ffmpeg(ffmpeg, path, 0.0, width, height) {
            frames.insert(0, first);
            CachedAsset::VideoInfo {
                width,
                height,
                native: (native_w, native_h),
                last_used: Instant::now(),
                // NTSC（29.97 など）では d*fps が 59.99999 のように僅かに欠けるので、少し足してから切り捨てる
                frame_count: duration.map(|d| ((d * fps + 0.01).floor() as u64).max(1)),
                tail: None,
                past_end: None,
                count_probed: false,
                retry_after: None,
                stream_start: start_time,
                fps,
                stream: None,
                frames,
            }
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

/// ffmpeg の実行ファイルを探します（音声デコード・書き出しなど他クレートからも使う）。
pub fn locate_ffmpeg() -> Option<PathBuf> {
    locate_ffmpeg_tools().0
}

fn locate_ffmpeg_tools() -> (Option<PathBuf>, Option<PathBuf>) {
    // 1. PATH から検索
    let mut probe = Command::new("ffmpeg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        probe.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    if probe.arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok() {
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

/// 動画の (幅, 高さ, フレームレート, 長さ[秒])。幅・高さは動画本来の値。
fn probe_video(ffprobe: &Path, video_path: &Path) -> Option<VideoProbe> {
    let mut cmd = Command::new(ffprobe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        // 長さはコンテナ（format）ではなく映像ストリームのものを使う。音声のほうが長い動画で終端を誤らないため
        .arg("stream=width,height,r_frame_rate,avg_frame_rate,duration,start_time")
        .args(["-of", "default=nw=1"])
        .arg(video_path)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| text.lines().find_map(|l| l.trim().strip_prefix(key)?.strip_prefix('=')).map(str::trim);
    let width: u32 = field("width")?.parse().ok()?;
    let height: u32 = field("height")?.parse().ok()?;
    let rate = |key: &str| {
        let (n, d) = field(key)?.split_once('/')?;
        Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?).filter(|f| f.is_finite() && *f > 0.0 && *f <= 240.0)
    };
    let fps = choose_fps(rate("r_frame_rate"), rate("avg_frame_rate"));
    let duration = field("duration").and_then(|d| d.parse::<f64>().ok()).filter(|d| d.is_finite() && *d > 0.0);
    let start_time = field("start_time").and_then(|d| d.parse::<f64>().ok()).filter(|d| d.is_finite()).unwrap_or(0.0);
    Some(VideoProbe { width: width.max(2), height: height.max(2), fps, duration, start_time })
}

/// ffprobe が報告する 2 つのレート（`r_frame_rate` = 全フレームの時刻を表せる最小のレート、`avg_frame_rate` = 実際の平均）から、
/// 時刻 → フレーム番号の変換に使うレートを決める。
/// - ちょうど 2 倍（インターレースのフィールドレート。59.94 と 29.97 など）: 平均（フレームレート）を採る。
/// - それ以外: `r_frame_rate`。可変フレームレートの動画（画面録画など）は平均が実レートより大幅に低く、
///   平均に合わせると動く場面がコマ落ちする。
fn choose_fps(r: Option<f64>, avg: Option<f64>) -> f64 {
    match (r, avg) {
        (Some(r), Some(avg)) if (r / avg - 2.0).abs() < 0.01 => avg,
        (Some(rate), _) | (None, Some(rate)) => rate,
        (None, None) => 30.0,
    }
}

struct VideoProbe {
    width: u32,
    height: u32,
    fps: f64,
    duration: Option<f64>,
    /// 映像ストリームの開始時刻[秒]（VFR のシークで元の時刻の座標系を保つのに使う）
    start_time: f64,
}

/// 先頭 `limit` 個の映像パケットの表示時刻[秒]（昇順）。
fn probe_packet_times(ffprobe: &Path, video_path: &Path, limit: usize) -> Option<Vec<f64>> {
    let mut cmd = Command::new(ffprobe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg("-read_intervals")
        .arg(format!("%+#{limit}"))
        .arg(video_path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let mut times: Vec<f64> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok()).collect();
    times.sort_by(f64::total_cmp);
    (times.len() >= 4).then_some(times)
}

/// パケットの表示時刻から、本来のフレームレートを推定します。
/// 静止区間（大きな間隔）や撮影時刻のゆらぎに影響されないよう、間隔の小さい側の中央値を使い、一般的なレートに丸める。
fn estimate_fps(times: &[f64]) -> Option<f64> {
    let mut deltas: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 1e-4).collect();
    if deltas.len() < 3 {
        return None;
    }
    deltas.sort_by(f64::total_cmp);
    let p10 = deltas[deltas.len() / 10];
    // 最小側の 2 倍以内の間隔だけ（= 動いている区間のフレーム間隔）の中央値
    let kept: Vec<f64> = deltas.iter().copied().filter(|d| *d <= 2.0 * p10).collect();
    let interval = kept[kept.len() / 2];
    let raw = 1.0 / interval;
    const COMMON: [f64; 9] = [23.976, 24.0, 25.0, 29.97, 30.0, 48.0, 50.0, 59.94, 60.0];
    let snapped = COMMON.into_iter().min_by(|a, b| (a - raw).abs().total_cmp(&(b - raw).abs()))?;
    let rate = if (snapped / raw - 1.0).abs() < 0.04 { snapped } else { raw };
    (1.0..=240.0).contains(&rate).then_some(rate)
}

/// 映像パケットの表示時刻から、映像のフレーム数を調べます（デコードはしないので速い）。
/// ストリームの長さを持たない形式（MKV/WebM）や、コンテナより映像が短い動画の終端を正確に知るために使う。
fn probe_frame_count(ffprobe: &Path, video_path: &Path, fps: f64) -> Option<u64> {
    let mut cmd = Command::new(ffprobe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(video_path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let times: Vec<f64> =
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok()).collect();
    let min = times.iter().copied().fold(f64::INFINITY, f64::min);
    let max = times.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min.is_finite() && max.is_finite()).then(|| ((max - min) * fps).round() as u64 + 1)
}

/// 幅が `max_width` を超えるなら、縦横比を保って縮小したサイズ（高さは偶数）。
fn scaled_size((width, height): (u32, u32), max_width: u32) -> (u32, u32) {
    if width > max_width {
        let scaled_h = (height as f64 * max_width as f64 / width as f64).round() as u32;
        (max_width, (scaled_h.max(2) / 2) * 2)
    } else {
        (width, height)
    }
}

/// 常駐デコーダでフレームを取得します。後戻りや大きなシークでは再起動し、起動できなければ 1 枚だけ個別にデコードします。
struct DecodeTarget {
    time_sec: f64,
    fps: f64,
    size: (u32, u32),
    stream_start: f64,
}

/// 動画 1 本ぶんのデコード状態（常駐デコーダ・終端の情報・起動回数）。
struct DecodeState<'a> {
    stream: &'a mut Option<stream::VideoStream>,
    /// 最後のフレーム。`None` = まだ調べていない、`Some(None)` = 調べたが取得できなかった
    tail: &'a mut Option<Option<RgbaFrame>>,
    past_end: &'a mut Option<u64>,
    /// 映像のフレーム数（分かっている場合）。MKV/WebM などで不明なときは、終端越えを最初に検出したときに調べる
    frame_count: &'a mut Option<u64>,
    count_probed: &'a mut bool,
    ffprobe: Option<&'a Path>,
    spawns: &'a mut u64,
}

/// デコードの結果。終端越えは失敗ではない（最後のフレームを表示し続ける状況）。
enum Decoded {
    Frame(RgbaFrame),
    /// 映像の終端を越えている。最後のフレームが取れていれば添える
    PastEnd(Option<RgbaFrame>),
    Failed,
}

/// 時刻 → ネイティブのフレーム番号。表示中のフレームは「その時刻に表示されている」もの（切り捨て）。
/// 微小な浮動小数点誤差で 1 つ手前に落ちないよう、わずかに足す。映像ストリームの長さが分かっていれば、
/// 終端を越える要求は最後のフレームに丸める。
fn native_index(time_sec: f64, fps: f64, frame_count: Option<u64>) -> u64 {
    let index = (time_sec.max(0.0) * fps + 1e-6).floor() as u64;
    frame_count.map_or(index, |count| index.min(count.saturating_sub(1)))
}

fn decode_video_frame(ffmpeg: &Path, path: &Path, target: DecodeTarget, state: &mut DecodeState) -> Decoded {
    let index = native_index(target.time_sec, target.fps, *state.frame_count);
    decode_at(ffmpeg, path, index, &target, state, true)
}

/// `index` 番目のフレームをデコードする。`may_probe` は、終端越えでフレーム数を調べて最後のフレームを取り直してよいか。
fn decode_at(ffmpeg: &Path, path: &Path, index: u64, target: &DecodeTarget, state: &mut DecodeState, may_probe: bool) -> Decoded {
    let DecodeTarget { fps, size: (width, height), stream_start, .. } = *target;
    let DecodeState { stream, tail, past_end, frame_count, count_probed, ffprobe, spawns } = state;

    // すでに終端越えと分かっている番号は、ffmpeg を起動せずに最後のフレーム（取得済みなら）を返す
    if past_end.is_some_and(|end| index >= end) {
        return Decoded::PastEnd(tail.clone().flatten());
    }
    let found = stream.as_mut().and_then(|s| s.frame_at(index));
    remember_end(stream, tail, past_end, frame_count);
    if let Some(frame) = found {
        return Decoded::Frame(frame);
    }

    **spawns += 1;
    **stream = stream::VideoStream::spawn(ffmpeg, path, index, fps, (width, height), stream_start);
    let Some(new_stream) = stream.as_mut() else {
        // プロセスを起動できなかった（一時的な資源不足など）。終端とは限らないので終端扱いにはしない
        **spawns += 1;
        return single_frame(ffmpeg, path, index, fps, (width, height));
    };
    let found = new_stream.frame_at(index);
    remember_end(stream, tail, past_end, frame_count);
    if let Some(frame) = found {
        return Decoded::Frame(frame);
    }
    // ffmpeg が異常終了していたなら、映像の終端ではなく失敗として扱う
    let died = stream.as_ref().is_some_and(|s| s.is_broken());
    **stream = None;
    if died {
        **spawns += 1;
        return single_frame(ffmpeg, path, index, fps, (width, height));
    }

    // 起動できたのに 1 枚も出さない = 映像の終端を越えている（音声のほうが長い動画など、長さが当てにならない
    // 場合も含む）。デコード失敗ではなく「最後のフレームを表示し続ける」状況として扱い、以後は起動しない。
    if index > 0 {
        **past_end = Some(past_end.map_or(index, |end| end.min(index)));
        // まず安い方法（パケットの表示時刻を数える）で正確なフレーム数を調べ、最後のフレームを普通にデコードする。
        // 調べられないときだけ、終端から遡ってデコードする高い方法に頼る
        if may_probe && frame_count.is_none() && !**count_probed {
            **count_probed = true;
            **frame_count = ffprobe.and_then(|probe| probe_frame_count(probe, path, fps));
        }
        if may_probe {
            if let Some(count) = **frame_count {
                if count > 0 && count - 1 < index {
                    **past_end = Some(count);
                    return match decode_at(ffmpeg, path, count - 1, target, state, false) {
                        Decoded::Frame(f) => {
                            *state.tail = Some(Some(f.clone()));
                            Decoded::PastEnd(Some(f))
                        }
                        other => other,
                    };
                }
            }
        }
        if tail.is_none() {
            **spawns += 1;
            **tail = Some(stream::read_last_frame(ffmpeg, path, width, height));
        }
        return Decoded::PastEnd(tail.clone().flatten());
    }
    **spawns += 1;
    single_frame(ffmpeg, path, index, fps, (width, height))
}

/// 常駐デコーダを使わず、1 枚だけ個別にデコードする。
fn single_frame(ffmpeg: &Path, path: &Path, index: u64, fps: f64, (width, height): (u32, u32)) -> Decoded {
    // フレームの表示時刻ちょうどでは丸めで 1 枚先になることがあるので、半フレーム手前を指定する
    let time_sec = ((index as f64 - 0.5) / fps.max(1.0)).max(0.0);
    match decode_video_frame_ffmpeg(ffmpeg, path, time_sec, width, height) {
        Some(frame) => Decoded::Frame(frame),
        None => Decoded::Failed,
    }
}

/// ストリームが終端まで読み切っていたら、動画の長さ（フレーム数）と最後のフレームを覚える。
fn remember_end(
    stream: &Option<stream::VideoStream>,
    tail: &mut Option<Option<RgbaFrame>>,
    past_end: &mut Option<u64>,
    frame_count: &mut Option<u64>,
) {
    let Some(s) = stream else { return };
    let Some(end) = s.end_index() else { return };
    // 1 枚も読めていないストリーム（終端の先から始めた、または読めなかった）の終点は、動画の終端の証拠にならない。
    // 空の結果から終端を決めてはいけない（先頭から始めた場合に「動画全体が終端の先」になってしまう）。
    // 終端越えの判定は、呼び出し側が別途（index > 0 のときだけ）行う
    if !s.has_last() {
        return;
    }
    *past_end = Some(past_end.map_or(end, |e| e.min(end)));
    // 順に再生して終端まで読み切った場合、フレーム数が正確に分かる（MKV/WebM は長さを持たないので、ここで学習する）
    if frame_count.is_none() {
        *frame_count = Some(end);
    }
    // 読めた最後の 1 枚が動画の最後のフレーム
    if tail.as_ref().is_none_or(|t| t.is_none()) {
        if let Some(last) = s.last_clone() {
            *tail = Some(Some(last));
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
    let expected_len = width as usize * height as usize * 4;
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .stdin(Stdio::null())
        // 先頭付近は、入力側のシーク（`-i` の前の `-ss`）ではなく出力側のシーク（`-i` の後ろ）にする。入力側だと、B フレーム付きの
        // AVI で後ろのキーフレームに着地してしまう。出力側は先頭からデコードして目的の位置まで読み捨てるので正確（最大 0.5 秒分）
        .args(stream::seek_arg(time_sec))
        .arg("-i")
        .arg(video_path)
        .args(if time_sec > 0.0 && stream::seek_arg(time_sec).is_empty() {
            vec!["-ss".to_string(), format!("{time_sec:.3}")]
        } else {
            Vec::new()
        })
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
        let order: Vec<u32> = [0, 1, 2, 3, 10, 11, 5, 40, 41, 200, 201, 59, 20, 21].to_vec();
        for &i in &order {
            let t = i as f64 / fps;
            let streamed = cache.get_frame(path, i, t).unwrap_or_else(|| panic!("frame {i}")).clone();
            assert_eq!((streamed.width, streamed.height), (320, 180));
            let reference = decode_video_frame_ffmpeg(&ffmpeg, path, (t.min(59.0 / fps) - 0.5 / fps).max(0.0), 320, 180).unwrap();
            let max_diff = streamed.pixels.iter().zip(&reference.pixels).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(max_diff <= 2, "frame {i}: 単独デコードと {max_diff} ずれている");
        }
    }

    #[test]
    fn decode_size_follows_max_width_but_native_size_is_kept() {
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        let path = Path::new("../../samples/sample_video.mp4"); // 320x180
        cache.set_max_decode_width(160);
        let f = cache.get_frame(path, 0, 0.0).unwrap();
        assert_eq!((f.width, f.height), (160, 90), "縮小してデコード");
        assert_eq!(cache.native_size(path), Some((320, 180)), "描画上の大きさは本来のサイズ");
    }

    #[test]
    fn scaled_size_keeps_aspect_and_even_height() {
        assert_eq!(scaled_size((1920, 1080), 960), (960, 540));
        assert_eq!(scaled_size((1000, 333), 500), (500, 166));
        assert_eq!(scaled_size((640, 360), 960), (640, 360), "小さい動画はそのまま");
    }

    #[test]
    fn live_streams_are_capped() {
        let dir = std::env::temp_dir().join(format!("au2-media-streams-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す // アサーションが失敗しても一時ファイルを残さない
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        cache.stream_idle = Duration::ZERO; // 直ちに「使われていない」とみなして上限の働きを見る
        // 同じ動画を別名（コピー）で 6 本開いても、常駐 ffmpeg は上限までしか残らない
        let src = Path::new("../../samples/sample_video.mp4");
        for i in 0..6 {
            let p = dir.join(format!("v{i}.mp4"));
            std::fs::copy(src, &p).unwrap();
            // フレーム 5 を要求（初期化時の 0 番ではなく、ストリームを起動させる）
            assert!(cache.get_frame(&p, 5, 5.0 / 30.0).is_some());
        }
        let live = cache
            .assets
            .values()
            .filter(|a| matches!(a, CachedAsset::VideoInfo { stream: Some(_), .. }))
            .count();
        assert!(live <= MAX_LIVE_STREAMS, "常駐 {live} 本");
    }

    #[test]
    fn many_visible_videos_do_not_restart_their_decoders_every_frame() {
        let dir = std::env::temp_dir().join(format!("au2-media-thrash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す // アサーションが失敗しても一時ファイルを残さない
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        // 5 本（上限 4 を超える）を同時に再生しても、各動画の ffmpeg は 1 回ずつしか起動しない
        let src = Path::new("../../samples/sample_video.mp4");
        let paths: Vec<PathBuf> = (0..5)
            .map(|i| {
                let p = dir.join(format!("v{i}.mp4"));
                std::fs::copy(src, &p).unwrap();
                p
            })
            .collect();
        for frame in 5..15u32 {
            for p in &paths {
                assert!(cache.get_frame(p, frame, frame as f64 / 30.0).is_some());
            }
        }
        assert_eq!(cache.stream_spawns, 5, "動画 1 本につき起動は 1 回だけ");
    }

    #[test]
    fn undecodable_video_fails_fast_without_respawning() {
        let dir = std::env::temp_dir().join(format!("au2-media-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す // アサーションが失敗しても一時ファイルを残さない
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        let p = dir.join("broken.mp4");
        std::fs::write(&p, b"this is not a video").unwrap();
        for i in 0..10 {
            assert!(cache.get_frame(&p, i, i as f64 / 30.0).is_none());
        }
        assert!(matches!(cache.assets.get(&p), Some(CachedAsset::Failed)));
        assert!(cache.stream_spawns <= 3, "起動の繰り返しに上限がある: {}", cache.stream_spawns);
    }

    #[test]
    fn fit_within_shrinks_only_oversized_frames() {
        let big = RgbaFrame { width: 100, height: 40, pixels: vec![7; 100 * 40 * 4] };
        assert!(big.fit_within(100).is_none());
        let small = big.fit_within(50).unwrap();
        assert_eq!((small.width, small.height), (50, 20));
        assert_eq!(small.pixels.len(), 50 * 20 * 4);
        let thin = RgbaFrame { width: 9000, height: 2, pixels: vec![0; 9000 * 2 * 4] }.fit_within(8192).unwrap();
        assert_eq!((thin.width, thin.height), (8192, 1), "高さは 1 以上に保つ");
    }

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 映像は 2 秒（60 フレーム）だが音声が 4 秒ある動画を作る。コンテナの長さは 4 秒になる。
    fn video_with_longer_audio(ffmpeg: &Path, dir: &Path) -> PathBuf {
        let out = dir.join("long-audio.mp4");
        let status = Command::new(ffmpeg)
            .args(["-y", "-v", "error", "-i", "../../samples/sample_video.mp4", "-f", "lavfi", "-i", "sine=d=4"])
            .args(["-map", "0:v", "-map", "1:a", "-c:v", "copy", "-c:a", "aac"])
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success());
        out
    }

    fn max_diff(a: &RgbaFrame, b: &RgbaFrame) -> u8 {
        a.pixels.iter().zip(&b.pixels).map(|(x, y)| x.abs_diff(*y)).max().unwrap()
    }

    #[test]
    fn seeking_past_the_video_end_shows_its_last_frame_even_when_audio_is_longer() {
        let dir = std::env::temp_dir().join(format!("au2-media-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す // アサーションが失敗しても一時ファイルを残さない
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = video_with_longer_audio(&ffmpeg, &dir);
        let src = Path::new("../../samples/sample_video.mp4");
        let last = decode_video_frame_ffmpeg(&ffmpeg, src, 59.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();

        // (a) 映像ストリームの長さが分かる場合: 終端を越えた要求は最後のフレームに丸められる
        cache.get_frame(&path, 10, 10.0 / 30.0).unwrap();
        for i in [70u32, 80, 95, 110] {
            let f = cache.get_frame(&path, i, i as f64 / 30.0).expect("終端越えでも表示できる").clone();
            assert!(max_diff(&f, &last) <= 2, "frame {i} は最後のフレーム");
        }
        let again = cache.get_frame(&path, 20, 20.0 / 30.0).expect("その後も通常のフレームを返せる").clone();
        assert!(max_diff(&again, &last) > 20, "frame 20 は最後のフレームとは別物");
        assert!(!matches!(cache.assets.get(&path), Some(CachedAsset::Failed)), "失敗扱いにならない");

        // (b) 長さが分からない場合（ffprobe が無い・長さ不明）でも、新しいストリームが空なら実際の最後のフレームを取りに行く
        let mut cache = MediaCache::new();
        cache.get_frame(&path, 10, 10.0 / 30.0).unwrap();
        if let Some(CachedAsset::VideoInfo { frame_count, .. }) = cache.assets.get_mut(&path) {
            *frame_count = None;
        }
        for i in [70u32, 80, 95, 110] {
            let f = cache.get_frame(&path, i, i as f64 / 30.0).expect("長さ不明でも終端越えを表示できる").clone();
            assert!(max_diff(&f, &last) <= 2, "frame {i} は最後のフレーム（長さ不明）");
        }
        assert!(cache.get_frame(&path, 20, 20.0 / 30.0).is_some(), "終端越えの後も通常再生に戻れる");
        assert!(!matches!(cache.assets.get(&path), Some(CachedAsset::Failed)), "デコード失敗扱いにならない");
    }

    /// 映像ストリームが長さを持たない Matroska（ffprobe の stream duration が N/A）に remux する。
    fn remux_to_mkv(ffmpeg: &Path, dir: &Path, extra_audio_secs: Option<u32>) -> PathBuf {
        let out = dir.join("video.mkv");
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-y", "-v", "error", "-i", "../../samples/sample_video.mp4"]);
        match extra_audio_secs {
            Some(secs) => {
                cmd.args(["-f", "lavfi", "-i", &format!("sine=d={secs}")]).args(["-map", "0:v", "-map", "1:a", "-c:a", "aac"]);
            }
            None => {
                cmd.args(["-map", "0:v"]);
            }
        }
        assert!(cmd.args(["-c:v", "copy"]).arg(&out).status().unwrap().success());
        out
    }

    #[test]
    fn mkv_without_stream_length_still_holds_the_real_last_frame() {
        let dir = std::env::temp_dir().join(format!("au2-media-mkv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = remux_to_mkv(&ffmpeg, &dir, None);
        let src = Path::new("../../samples/sample_video.mp4");
        let last = decode_video_frame_ffmpeg(&ffmpeg, src, 59.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();

        cache.get_frame(&path, 20, 20.0 / 30.0).unwrap();
        // 読み捨てながら終端をまたぐ（20 → 64）。間にある古いフレームではなく、本当の最後のフレームが出る
        let f64_ = cache.get_frame(&path, 64, 64.0 / 30.0).unwrap().clone();
        assert!(max_diff(&f64_, &last) <= 2, "frame 64 は最後のフレーム");
        // 以降、終端を越えた番号を順に要求しても、そのたびに ffmpeg を起動しない
        let spawns_before = cache.stream_spawns;
        for i in 65..80u32 {
            let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap().clone();
            assert!(max_diff(&f, &last) <= 2, "frame {i}");
        }
        assert!(cache.stream_spawns - spawns_before <= 1, "起動は多くても 1 回: {}", cache.stream_spawns - spawns_before);
    }

    #[test]
    fn mkv_whose_audio_is_far_longer_does_not_break_the_video() {
        let dir = std::env::temp_dir().join(format!("au2-media-mkvlong-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = remux_to_mkv(&ffmpeg, &dir, Some(700)); // 映像 2 秒に対して音声 700 秒
        assert!(cache.get_frame(&path, 20, 20.0 / 30.0).is_some());
        // 終端の最後のフレームを取得できなくても（遡る範囲が足りない）、壊れた動画として諦めない
        for i in 100..110u32 {
            let _ = cache.get_frame(&path, i, i as f64 / 30.0);
        }
        assert!(!matches!(cache.assets.get(&path), Some(CachedAsset::Failed)), "動画全体を失敗扱いにしない");
        assert!(cache.get_frame(&path, 30, 30.0 / 30.0).is_some(), "範囲内のフレームは引き続き表示できる");
        assert!(cache.stream_spawns <= 6, "終端越えのたびに起動し直さない: {}", cache.stream_spawns);
    }

    #[test]
    fn frame_index_rounds_down_in_mixed_rate_projects() {
        // 30fps の動画を 60fps のプロジェクトで表示: プロジェクトのフレーム 0,1,2,3 は動画の 0,0,1,1
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = Path::new("../../samples/sample_video.mp4");
        let at = |cache: &mut MediaCache, project_frame: u32| {
            cache.get_frame(path, project_frame, project_frame as f64 / 60.0).unwrap().clone()
        };
        let f: Vec<RgbaFrame> = (0..4).map(|i| at(&mut cache, i)).collect();
        assert!(max_diff(&f[0], &f[1]) <= 2, "0 と 1 は同じ動画フレーム");
        assert!(max_diff(&f[2], &f[3]) <= 2, "2 と 3 は同じ動画フレーム");
        let native1 = decode_video_frame_ffmpeg(&ffmpeg, path, 1.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();
        assert!(max_diff(&f[2], &native1) <= 2, "プロジェクトのフレーム 2 = 動画のフレーム 1");
    }

    #[test]
    fn past_end_requests_right_after_open_do_not_poison_the_video() {
        // 開いた直後（ストリームを一度も使っていない状態）に終端越えを要求しても、範囲内のフレームは壊れない
        let dir = std::env::temp_dir().join(format!("au2-media-poison-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = remux_to_mkv(&ffmpeg, &dir, Some(700));
        for i in 1500..1504u32 {
            let _ = cache.get_frame(&path, i, i as f64 / 30.0);
        }
        assert!(!matches!(cache.assets.get(&path), Some(CachedAsset::Failed)), "失敗扱いにならない");
        assert!(cache.get_frame(&path, 30, 1.0).is_some(), "範囲内のフレームは表示できる");

        // フレーム数を調べる手段（ffprobe）が無くても、終端越えの要求を繰り返して壊れた動画扱いにならない
        let mut cache = MediaCache::new();
        let path = remux_to_mkv(&ffmpeg, &dir, Some(700));
        cache.get_frame(&path, 5, 5.0 / 30.0).unwrap();
        cache.ffprobe_bin = None;
        for i in 1500..1510u32 {
            let _ = cache.get_frame(&path, i, i as f64 / 30.0);
        }
        assert!(!matches!(cache.assets.get(&path), Some(CachedAsset::Failed)), "終端越えを失敗と数えない");
        assert!(cache.get_frame(&path, 30, 1.0).is_some(), "範囲内のフレームは表示できる");
    }

    #[test]
    fn changing_the_project_frame_rate_does_not_reuse_frames_of_another_time() {
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = Path::new("../../samples/sample_video.mp4");
        // 同じ「フレーム番号 40」でも、30fps なら 1.333 秒、60fps なら 0.667 秒
        let a = cache.get_frame(path, 40, 40.0 / 30.0).unwrap().clone();
        let b = cache.get_frame(path, 40, 40.0 / 60.0).unwrap().clone();
        let expected = decode_video_frame_ffmpeg(&ffmpeg, path, 20.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();
        assert!(max_diff(&b, &expected) <= 2, "60fps の 40 番目は動画の 20 番目");
        assert!(max_diff(&a, &b) > 20, "別の時刻のフレームは別物");
    }

    #[test]
    fn ntsc_video_keeps_its_last_frame() {
        let dir = std::env::temp_dir().join(format!("au2-media-ntsc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = dir.join("ntsc.mp4");
        // 30000/1001 fps で 60 フレーム（長さ 2.002 秒）。画面の内容は 1 フレームごとに変わるテストパターン
        let made = Command::new(&ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=r=30000/1001:s=64x36:d=2.002"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return; // libx264 の無い ffmpeg
        }
        let fps = 30000.0 / 1001.0;
        let last = cache.get_frame(&path, 59, 59.0 / fps).unwrap().clone();
        let before = cache.get_frame(&path, 58, 58.0 / fps).unwrap().clone();
        assert!(max_diff(&last, &before) > 0, "最後の 2 枚は別のフレーム（最後のフレームが切り落とされていない）");
        if let Some(CachedAsset::VideoInfo { frame_count, .. }) = cache.assets.get(&path) {
            assert_eq!(*frame_count, Some(60));
        }
    }

    #[test]
    fn stepping_back_beyond_a_known_end_does_not_spawn_ffmpeg() {
        let dir = std::env::temp_dir().join(format!("au2-media-stepback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = remux_to_mkv(&ffmpeg, &dir, None); // 長さを持たない MKV
        cache.get_frame(&path, 59, 59.0 / 30.0).unwrap();
        cache.get_frame(&path, 70, 70.0 / 30.0).unwrap(); // 終端を読み切る
        let spawns = cache.stream_spawns;
        for i in (60..70u32).rev() {
            assert!(cache.get_frame(&path, i, i as f64 / 30.0).is_some(), "frame {i}");
        }
        assert_eq!(cache.stream_spawns, spawns, "終端が分かった後は、手前に戻っても（終端越えの間は）起動しない");
    }

    #[test]
    fn first_past_end_request_teaches_the_exact_length_of_a_lengthless_mkv() {
        let dir = std::env::temp_dir().join(format!("au2-media-learn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone()); // cache より先に宣言する（後に捨てられる）= 常駐 ffmpeg が掴むファイルを先に閉じてから消す
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        if cache.ffprobe_bin.is_none() {
            return;
        }
        let path = remux_to_mkv(&ffmpeg, &dir, None);
        let src = Path::new("../../samples/sample_video.mp4");
        let last = decode_video_frame_ffmpeg(&ffmpeg, src, 59.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();

        // 開いた直後にいきなり終端のはるか先を要求 → 正確なフレーム数（60）を調べ、最後のフレームを返す
        let f = cache.get_frame(&path, 1500, 1500.0 / 30.0).unwrap().clone();
        assert!(max_diff(&f, &last) <= 2);
        assert!(matches!(cache.assets.get(&path), Some(CachedAsset::VideoInfo { frame_count: Some(60), .. })));
        // そこから 1 コマずつ手前へ戻っても（まだ終端の先）、ffmpeg を起動しない
        let spawns = cache.stream_spawns;
        for i in (1480..1500u32).rev() {
            let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap().clone();
            assert!(max_diff(&f, &last) <= 2, "frame {i}");
        }
        // 最後のフレーム（番号 59）を 1 回だけデコードするための起動は許す。20 回の要求のたびに起動しないこと
        assert!(cache.stream_spawns <= spawns + 1, "起動は多くても 1 回: {} -> {}", spawns, cache.stream_spawns);
    }

    /// `rate` fps・`frames` 枚の、1 枚ごとに内容が変わるテスト動画を `ext` で作る。作れなければ `None`（libx264 が無い等）。
    fn testsrc_video(ffmpeg: &Path, dir: &Path, name: &str, rate: &str, frames: u32) -> Option<PathBuf> {
        let out = dir.join(name);
        let ok = Command::new(ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc=r={rate}:s=64x36"))
            .args(["-frames:v", &frames.to_string(), "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&out)
            .status()
            .is_ok_and(|s| s.success());
        ok.then_some(out)
    }

    /// 動画を先頭から順に全フレームデコードする（正解）。
    fn decode_all(ffmpeg: &Path, path: &Path) -> Vec<Vec<u8>> {
        let out = Command::new(ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        out.stdout.chunks_exact(64 * 36 * 4).map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn every_container_returns_the_right_frame_at_every_rate() {
        // MKV/WebM はタイムスタンプがミリ秒単位。24/29.97/30 fps で、途中からのストリーム開始でも
        // コマの重複・欠落が起きないこと（再生・書き出しがガタつかないこと）
        let probe = MediaCache::new();
        let Some(ffmpeg) = probe.ffmpeg_bin.clone() else { return };
        let dir = std::env::temp_dir().join(format!("au2-media-rates-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        for (rate, fps) in [("24", 24.0), ("30000/1001", 30000.0 / 1001.0), ("30", 30.0)] {
            let Some(mp4) = testsrc_video(&ffmpeg, &dir, "src.mp4", rate, 90) else { return };
            // avi: MJPEG に再エンコード（タイムベースがフレーム間隔そのもの）。ts-mp4: タイムスケール = フレームレートの MP4
            for ext in ["mp4", "mkv", "webm", "avi", "ts-mp4"] {
                let path = dir.join(format!("v-{}.{}", rate.replace('/', "_"), if ext == "ts-mp4" { "mp4" } else { ext }));
                let path = if ext == "ts-mp4" { path.with_file_name(format!("ts-{}", path.file_name().unwrap().to_string_lossy())) } else { path };
                let codec: &[&str] = match ext {
                    "webm" => &["-c:v", "libvpx", "-b:v", "2M"],
                    "avi" => &["-c:v", "mjpeg", "-q:v", "2"],
                    "ts-mp4" if rate.contains('/') => continue, // 29.97 は整数のタイムスケールで表せない
                    "ts-mp4" => &["-c:v", "copy", "-video_track_timescale", rate],
                    _ => &["-c:v", "copy"],
                };
                let remux = Command::new(&ffmpeg).args(["-y", "-v", "error", "-i"]).arg(&mp4).args(codec).arg(&path).status();
                if !remux.is_ok_and(|s| s.success()) {
                    continue; // libvpx が無い ffmpeg
                }
                let truth = decode_all(&ffmpeg, &path);
                assert!(truth.len() >= 85, "{ext} {rate}: 正解の枚数 {}", truth.len());
                // ストリームを 10 コマ目から始め、連続・読み捨て（+3）・また連続の順にたどる
                let mut cache = MediaCache::new();
                for i in [10u32, 11, 12, 13, 14, 15, 18, 19, 20, 40, 41, 42, 77, 78] {
                    let f = cache.get_frame(&path, i, i as f64 / fps).unwrap_or_else(|| panic!("{ext} {rate} frame {i}"));
                    let same = f.pixels == truth[i as usize];
                    assert!(same, "{ext} {rate}: フレーム {i} が正解のフレームと違う");
                }
            }
        }
    }

    #[test]
    fn playing_a_lengthless_mkv_to_its_end_teaches_its_duration() {
        let dir = std::env::temp_dir().join(format!("au2-media-playend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = remux_to_mkv(&ffmpeg, &dir, None);
        cache.get_frame(&path, 0, 0.0).unwrap();
        assert!(cache.duration_secs(&path).is_none(), "MKV は最初は長さが分からない");
        for i in 0..70u32 {
            // 順に再生して終端（60 フレーム）の先まで進める
            assert!(cache.get_frame(&path, i, i as f64 / 30.0).is_some(), "frame {i}");
        }
        let d = cache.duration_secs(&path).expect("終端まで再生すると長さが分かる");
        assert!((d - 2.0).abs() < 0.05, "長さ {d}");
    }

    #[test]
    fn failed_decodes_back_off_unless_disabled() {
        let dir = std::env::temp_dir().join(format!("au2-media-backoff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        let path = Path::new("../../samples/sample_video.mp4");
        cache.get_frame(path, 0, 0.0).unwrap();
        // ffmpeg が起動できなくなった状況（資源不足など）を作る
        cache.ffmpeg_bin = Some(dir.join("no-such-ffmpeg"));
        assert!(cache.get_frame(path, 20, 20.0 / 30.0).is_none());
        // 時間に頼らず「待ち時間の途中」を固定する（負荷の高い環境でも、1 秒以内に呼ばれたことに依存しない）
        if let Some(CachedAsset::VideoInfo { retry_after, .. }) = cache.assets.get_mut(path) {
            *retry_after = Some(Instant::now() + Duration::from_secs(3600));
        }
        let spawns = cache.stream_spawns;
        for i in 21..30u32 {
            assert!(cache.get_frame(path, i, i as f64 / 30.0).is_none());
        }
        assert_eq!(cache.stream_spawns, spawns, "待ち時間の間は再試行しない");

        // 書き出しでは待たない（待つと、その間のフレームがプレースホルダになる）
        cache.set_retry_backoff(false);
        let _ = cache.get_frame(path, 40, 40.0 / 30.0);
        let _ = cache.get_frame(path, 41, 41.0 / 30.0);
        assert!(cache.stream_spawns > spawns, "待たずに再試行する");
    }

    #[cfg(unix)]
    #[test]
    fn a_killed_ffmpeg_is_not_mistaken_for_the_end_of_the_video() {
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let path = Path::new("../../samples/sample_video.mp4");
        cache.get_frame(path, 5, 5.0 / 30.0).unwrap();
        let pid = match cache.assets.get(path) {
            Some(CachedAsset::VideoInfo { stream: Some(s), .. }) => s.pid(),
            _ => panic!("常駐デコーダが動いているはず"),
        };
        assert!(Command::new("kill").arg("-9").arg(pid.to_string()).status().unwrap().success());
        std::thread::sleep(Duration::from_millis(200));
        // 殺された後でも、動画の長さ・終端を誤って記録せず、次のフレームは作り直して正しく返す
        let f = cache.get_frame(path, 30, 1.0).expect("作り直して取得できる").clone();
        let truth = decode_video_frame_ffmpeg(&ffmpeg, path, 30.0 / 30.0 - 0.5 / 30.0, 320, 180).unwrap();
        assert!(max_diff(&f, &truth) <= 2, "正しいフレーム");
        match cache.assets.get(path) {
            Some(CachedAsset::VideoInfo { past_end, frame_count, .. }) => {
                assert_eq!(*past_end, None, "強制終了を終端と記録しない");
                assert_eq!(*frame_count, Some(60));
            }
            _ => panic!("動画のまま"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_stream_at_the_first_frame_does_not_mark_the_whole_video_as_past_the_end() {
        let mut cache = MediaCache::new();
        if !cache.has_ffmpeg() {
            return;
        }
        if !Path::new("/usr/bin/true").exists() {
            return; // 差し込む偽の ffmpeg が無い環境では、起動に失敗する別の経路になってしまい、検証にならない
        }
        let path = Path::new("../../samples/sample_video.mp4");
        cache.get_frame(path, 0, 0.0).unwrap();
        if let Some(CachedAsset::VideoInfo { frames, .. }) = cache.assets.get_mut(path) {
            frames.clear(); // 先頭フレームのキャッシュを失った状態
        }
        // 何も出力せずに正常終了する ffmpeg（/usr/bin/true）を差し込み、「空のストリーム」を再現する
        cache.ffmpeg_bin = Some(PathBuf::from("/usr/bin/true"));
        let _ = cache.get_frame(path, 0, 0.0);
        match cache.assets.get(path) {
            Some(CachedAsset::VideoInfo { past_end, .. }) => assert_eq!(*past_end, None, "動画全体を終端の先にしない"),
            _ => panic!("動画のまま"),
        }
        assert!(cache.duration_secs(path).is_some_and(|d| d > 1.0), "長さが壊れない");
    }

    #[test]
    fn a_file_with_mostly_undecodable_frames_still_holds_its_last_good_frame() {
        // ffmpeg は、デコードエラーの割合が既定の 2/3 を超えると、フレームを出し終えたあとでも終了コード 69 を返す。
        // それを「異常終了」と見なすと、終端の記録も保持表示もできなくなる（-max_error_rate 1 で防いでいる）
        let dir = std::env::temp_dir().join(format!("au2-media-broken-avi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let mut cache = MediaCache::new();
        let Some(ffmpeg) = cache.ffmpeg_bin.clone() else { return };
        let avi = dir.join("v.avi");
        let made = Command::new(&ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=r=30:s=64x36", "-frames:v", "60", "-c:v", "mjpeg"])
            .arg(&avi)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        // 先頭 10 フレームだけ正常で、以降の各フレームの中身を壊す（AVI の MJPEG: "00dc" チャンクの本体を 0xFF で埋める）
        let mut bytes = std::fs::read(&avi).unwrap();
        let mut chunks = Vec::new();
        let mut at = bytes.windows(4).position(|w| w == b"movi").expect("movi リスト") + 4;
        while let Some(rel) = bytes[at..].windows(4).position(|w| w == b"00dc") {
            let tag = at + rel;
            let size = u32::from_le_bytes(bytes[tag + 4..tag + 8].try_into().unwrap()) as usize;
            chunks.push((tag + 8, size));
            at = tag + 8 + size;
            if at + 8 > bytes.len() || chunks.len() == 60 {
                break;
            }
        }
        assert!(chunks.len() >= 60, "チャンクを見つけられた: {}", chunks.len());
        for &(offset, size) in &chunks[10..] {
            bytes[offset + 2..offset + size - 2].fill(0xFF);
        }
        std::fs::write(&avi, bytes).unwrap();
        cache.get_frame(&avi, 0, 0.0).expect("先頭は正常");
        // 順に再生する。読めるのは 10 枚だけで、その先は壊れた区間（最後に読めたフレームを返し続け、プレースホルダにならない）
        for i in 0..40u32 {
            assert!(cache.get_frame(&avi, i, i as f64 / 30.0).is_some(), "frame {i} を表示できる");
        }
        // 読み切った時点で「10 枚目で終わり」と学習できている（異常終了と誤認していない）。
        // 誤認すると、以後も毎フレーム ffmpeg を起動し直すことになる
        match cache.assets.get(&avi) {
            Some(CachedAsset::VideoInfo { past_end, .. }) => assert!(past_end.is_some_and(|e| (9..=11).contains(&e)), "終端を学習する: {past_end:?}"),
            _ => panic!("動画のまま"),
        }
        assert!(cache.stream_spawns <= 3, "起動し直し続けない: {}", cache.stream_spawns);
        assert!(!matches!(cache.assets.get(&avi), Some(CachedAsset::Failed)));
    }

    #[test]
    fn choose_fps_handles_interlaced_vfr_and_missing_rates() {
        assert_eq!(choose_fps(Some(30.0), Some(30.0)), 30.0);
        assert_eq!(choose_fps(Some(25.0), Some(24.99)), 25.0);
        assert_eq!(choose_fps(Some(60000.0 / 1001.0), Some(30000.0 / 1001.0)), 30000.0 / 1001.0, "ちょうど 2 倍はインターレース");
        assert_eq!(choose_fps(Some(50.0), Some(25.0)), 25.0);
        assert_eq!(choose_fps(Some(60.0), Some(31.0)), 60.0, "2 倍に近いだけの VFR（画面録画）は最大レートを採る");
        assert_eq!(choose_fps(Some(30.0), Some(12.0)), 30.0, "静止区間のある VFR も最大レート");
        assert_eq!(choose_fps(Some(24.0), None), 24.0);
        assert_eq!(choose_fps(None, Some(24.0)), 24.0);
        assert_eq!(choose_fps(None, None), 30.0);
    }

    /// 画面録画のような可変フレームレートの動画を作る。0〜0.97 秒に 30 枚、4.0 秒まで何も無く、4.0〜4.97 秒にまた 30 枚。
    /// `jitter` を付けると、各フレームの表示時刻を ±3ms ゆらがせる（実際のキャプチャに近い）。
    fn gap_video(ffmpeg: &Path, path: &Path, jitter: bool) -> bool {
        let pts = if jitter {
            "setpts='(N/30+if(gte(N,30),3,0)+0.003*sin(N*12.9898))/TB'"
        } else {
            "setpts='if(gte(N,30),PTS+3/TB,PTS)'"
        };
        Command::new(ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=r=30:s=64x36", "-frames:v", "60", "-vf", pts])
            .args(["-fps_mode", "vfr", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(path)
            .status()
            .is_ok_and(|s| s.success())
    }

    /// ソースの全フレームを、固定レート化せずにそのままの順で取り出す。
    fn decode_source_frames(ffmpeg: &Path, path: &Path) -> Vec<Vec<u8>> {
        Command::new(ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-an", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap()
            .stdout
            .chunks_exact(64 * 36 * 4)
            .map(<[u8]>::to_vec)
            .collect()
    }

    #[test]
    fn variable_frame_rate_video_keeps_its_frame_numbers_even_when_started_mid_gap() {
        // 正解（30fps の時刻 i/30 に表示されているフレーム）: i<30 はソースの i 枚目、30..120 は静止区間で 29 枚目のまま、
        // 120 以降はソースの (30 + i-120) 枚目
        let dir = std::env::temp_dir().join(format!("au2-media-vfr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let probe = MediaCache::new();
        let Some(ffmpeg) = probe.ffmpeg_bin.clone() else { return };
        let mut variants = Vec::new();
        for jitter in [false, true] {
            let mp4 = dir.join(format!("gap-{jitter}.mp4"));
            if !gap_video(&ffmpeg, &mp4, jitter) {
                return; // この ffmpeg では作れない
            }
            // MKV はミリ秒精度の時刻で、ストリームの長さを持たない（VFR と判定できない形式）。同じ中身を詰め替えて確かめる
            let mkv = dir.join(format!("gap-{jitter}.mkv"));
            assert!(Command::new(&ffmpeg).args(["-y", "-v", "error", "-i"]).arg(&mp4).args(["-c", "copy"]).arg(&mkv).status().unwrap().success());
            variants.push(mp4);
            variants.push(mkv);
        }
        for path in variants {
            let source = decode_source_frames(&ffmpeg, &path);
            assert_eq!(source.len(), 60);
            let expected = |i: usize| -> &Vec<u8> {
                match i {
                    0..=29 => &source[i],
                    30..=119 => &source[29],
                    _ => &source[30 + (i - 120)],
                }
            };
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // 先頭から順に（静止区間の前の動く区間を含む）
            let mut cache = MediaCache::new();
            for i in 0..30u32 {
                let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap();
                assert!(f.pixels == *expected(i as usize), "{name}: 先頭から順に {i} 枚目が違う");
            }
            // 静止区間のまんなか（60 枚目 = 2.0 秒）から始めて、静止区間を抜け 4.0 秒（120 枚目）以降まで順に進める
            let mut cache = MediaCache::new();
            cache.get_frame(&path, 0, 0.0).unwrap();
            for i in (60..150u32).chain([10, 11, 12]) {
                let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap_or_else(|| panic!("{name}: frame {i}"));
                assert!(f.pixels == *expected(i as usize), "{name}: 静止区間から始めて {i} 枚目が違う");
            }
            // 途中から始めたストリームが、動画を早く終わったと誤認していない（学習した長さは正しい）
            match cache.assets.get(&path) {
                Some(CachedAsset::VideoInfo { past_end, .. }) => assert!(past_end.is_none_or(|e| e >= 150), "{name}: past_end {past_end:?}"),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn estimate_fps_ignores_static_gaps_and_timestamp_jitter() {
        // 30fps（ミリ秒に丸められた時刻）で 30 枚、3 秒の静止、また 30 枚
        let mut t = Vec::new();
        for i in 0..30 {
            t.push((i as f64 / 30.0 * 1000.0).round() / 1000.0);
        }
        for i in 0..30 {
            t.push(4.0 + (i as f64 / 30.0 * 1000.0).round() / 1000.0);
        }
        assert_eq!(estimate_fps(&t), Some(30.0));
        // ±3ms のゆらぎ
        let jittered: Vec<f64> = (0..60).map(|i| i as f64 / 30.0 + 0.003 * ((i as f64) * 12.9898).sin()).collect();
        assert_eq!(estimate_fps(&jittered), Some(30.0));
        // 23.976 / 29.97 は近い値に丸める
        let ntsc: Vec<f64> = (0..40).map(|i| i as f64 * 1001.0 / 30000.0).collect();
        assert_eq!(estimate_fps(&ntsc), Some(29.97));
        assert_eq!(estimate_fps(&[0.0, 1.0]), None, "パケットが少なすぎる");
    }

    #[test]
    fn starting_a_stream_in_the_middle_of_a_gop_does_not_show_the_next_keyframe() {
        // MPEG-TS は、シーク先が非キーフレームだと直後のキーフレームに着地する。手前から読み始めれば番号がずれない
        let dir = std::env::temp_dir().join(format!("au2-media-gop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let probe = MediaCache::new();
        let Some(ffmpeg) = probe.ffmpeg_bin.clone() else { return };
        let path = dir.join("gop.ts");
        // フレーム番号を画面に埋め込む代わりに、1 枚ごとに内容が変わるテストパターンを 150 枚（キーフレーム間隔 12）
        let made = Command::new(&ffmpeg)
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=r=30:s=64x36", "-frames:v", "150"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "12", "-bf", "0"])
            .arg(&path)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let truth = decode_all(&ffmpeg, &path);
        assert!(truth.len() >= 148, "正解の枚数 {}", truth.len());
        for start in [100u32, 101, 107, 113, 130] {
            let mut cache = MediaCache::new();
            for i in start..start + 4 {
                let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap_or_else(|| panic!("TS: {start} から始めて {i} 枚目を取得できない"));
                // testsrc は 180 枚周期なので、フレームの内容そのもので区別できる（150 枚以内）
                assert!(f.pixels == truth[i as usize], "TS: {start} から始めて {i} 枚目が違う");
            }
        }
    }

    #[test]
    fn avi_with_b_frames_is_not_broken_near_the_start() {
        // B フレーム付きの AVI（H.264 / Xvid）では `-ss 0` が先頭ではなく後ろのキーフレームに着地する。
        // 先頭付近は -ss を付けない。（この種の AVI は、開始時刻に起因する最大 2 フレームのずれは残る = ROADMAP 参照）
        let dir = std::env::temp_dir().join(format!("au2-media-bavi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _cleanup = RemoveOnDrop(dir.clone());
        let probe = MediaCache::new();
        let Some(ffmpeg) = probe.ffmpeg_bin.clone() else { return };
        for (name, codec) in [("h264.avi", ["-c:v", "libx264"]), ("xvid.avi", ["-c:v", "mpeg4"])] {
            let path = dir.join(name);
            let extra: &[&str] = if name == "xvid.avi" { &["-bf", "2", "-vtag", "XVID"] } else { &["-bf", "2"] };
            // キーフレーム間隔は既定（250 = 8 秒）。小さい（正の）-ss が次のキーフレームに着地すると、60 枚目あたりが
            // 8 秒先の絵になる
            let made = Command::new(&ffmpeg)
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=r=30:s=64x36", "-frames:v", "300"])
                .args(codec)
                .args(extra)
                .arg(&path)
                .status();
            if !made.is_ok_and(|s| s.success()) {
                continue;
            }
            let truth = decode_all(&ffmpeg, &path);
            if truth.len() < 280 {
                continue;
            }
            for i in [1u32, 30, 61, 63, 90] {
                // 毎回、新しいキャッシュ（= ストリームの起動からやり直し）。シーク位置が先頭付近のときの -ss の効き方を確かめる
                let mut cache = MediaCache::new();
                let f = cache.get_frame(&path, i, i as f64 / 30.0).unwrap_or_else(|| panic!("{name} frame {i}"));
                // 先頭付近でも、固まったり壊れたりせず、実在するフレーム（± 数コマ以内）が出る
                let near = (i.saturating_sub(3)..=i + 3).any(|k| truth.get(k as usize).is_some_and(|t| *t == f.pixels));
                assert!(near, "{name}: {i} 枚目が、近くの正しいフレームのどれとも一致しない");
            }
        }
    }

    #[test]
    fn seek_arg_is_omitted_at_the_start_and_skip_limit_scales_with_fps() {
        assert_eq!(stream::skip_limit(10.0), 45, "低 fps でも最低 45 枚");
        assert_eq!(stream::skip_limit(30.0), 75);
        assert_eq!(stream::skip_limit(60.0), 150, "再起動は 2 秒ぶんデコードし直すので、先送り許容も 2.5 秒ぶんに");
        assert_eq!(stream::seek_arg(0.0), Vec::<String>::new(), "先頭は -ss なし");
        assert_eq!(stream::seek_arg(-1.0), Vec::<String>::new());
        assert_eq!(stream::seek_arg(0.1), Vec::<String>::new(), "先頭付近（0.5 秒未満）も -ss なし。B フレーム付き AVI が後ろのキーフレームに着地するため");
        assert!(!stream::seek_arg(0.5).is_empty());
        assert_eq!(stream::seek_arg(1.5), vec!["-ss".to_string(), "1.5000".to_string()]);
    }
}
