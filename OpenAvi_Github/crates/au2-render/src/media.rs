//! 画像・動画フレームの供給元。描画エンジンはデコード方法を知らず、このトレイト越しに受け取ります。

use std::path::Path;

use au2_media::{MediaCache, MediaKind, RgbaFrame};

pub trait MediaSource {
    /// `path` の `media_frame` 番目（動画は `time_sec` 秒）のフレームを返します。無ければ `None`。
    fn frame(&mut self, path: &str, media_frame: u32, time_sec: f64) -> Option<&RgbaFrame>;

    /// ファイルを開いておく（フレームは要求しない）。以降の `duration_secs` / `native_size` が答えられるようにするため。
    fn prepare(&mut self, _path: &str) {}

    /// 動画の長さ[秒]（ループ再生に使う）。分からなければ `None`。`frame()` を呼んだ後に答えられればよい。
    fn duration_secs(&mut self, _path: &str) -> Option<f64> {
        None
    }

    /// 描画上の大きさの基準になる、メディア本来のサイズ。`frame()` が縮小したフレームを返す供給元だけが答える
    /// （既定は `None` = 返したフレームのサイズをそのまま使う）。
    fn native_size(&mut self, _path: &str) -> Option<(u32, u32)> {
        None
    }
}

/// メディアを一切持たない供給元（図形だけのシーンやテスト用）。
pub struct NoMedia;

impl MediaSource for NoMedia {
    fn frame(&mut self, _path: &str, _media_frame: u32, _time_sec: f64) -> Option<&RgbaFrame> {
        None
    }
}

impl MediaSource for MediaCache {
    fn frame(&mut self, path: &str, media_frame: u32, time_sec: f64) -> Option<&RgbaFrame> {
        let path = Path::new(path);
        // 静止画は常にフレーム 0 に共通化（アニメーション GIF は MediaCache 側で巡回する）
        let index = match MediaKind::from_path(path) {
            Some(MediaKind::Image) => 0,
            _ => media_frame,
        };
        self.get_frame(path, index, time_sec)
    }

    fn native_size(&mut self, path: &str) -> Option<(u32, u32)> {
        MediaCache::native_size(self, Path::new(path))
    }

    fn prepare(&mut self, path: &str) {
        MediaCache::prepare(self, Path::new(path));
    }

    fn duration_secs(&mut self, path: &str) -> Option<f64> {
        MediaCache::duration_secs(self, Path::new(path))
    }
}
