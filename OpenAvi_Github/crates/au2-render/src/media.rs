//! 画像・動画フレームの供給元。描画エンジンはデコード方法を知らず、このトレイト越しに受け取ります。

use std::path::Path;

use au2_media::{MediaCache, MediaKind, RgbaFrame};

pub trait MediaSource {
    /// `path` の `media_frame` 番目（動画は `time_sec` 秒）のフレームを返します。無ければ `None`。
    fn frame(&mut self, path: &str, media_frame: u32, time_sec: f64) -> Option<&RgbaFrame>;
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
}
