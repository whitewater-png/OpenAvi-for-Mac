//! 常駐 ffmpeg による動画の連続デコード。
//!
//! フレームごとにプロセスを起動すると 1 枚 100ms 前後かかり再生が成り立たない。ここでは 1 本の動画につき
//! ffmpeg を 1 プロセス常駐させ、標準出力から RGBA フレームを順に読む。再生（昇順アクセス）では
//! 次のフレームを読むだけで済み、少しの先送りは読み捨てで追いつく。後戻りや大きなシークだけ再起動する。

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::RgbaFrame;

/// これ以内の先送りは、再起動せずフレームを読み捨てて追いつく。
const SKIP_LIMIT: u64 = 45;

pub(crate) struct VideoStream {
    child: Child,
    stdout: ChildStdout,
    width: u32,
    height: u32,
    /// 次に読み出されるフレームの番号（動画のネイティブフレーム）
    next_index: u64,
    last: Option<(u64, RgbaFrame)>,
    ended: bool,
}

impl VideoStream {
    /// `start_index` 番目（ネイティブ fps 基準）のフレームから連続デコードを開始します。
    pub(crate) fn spawn(ffmpeg: &Path, path: &Path, start_index: u64, fps: f64, width: u32, height: u32) -> Option<Self> {
        let mut cmd = Command::new(ffmpeg);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        // フレームの表示時刻ちょうどを指定すると丸めで 1 枚先から始まることがあるため、半フレーム手前から始める
        let start_sec = ((start_index as f64 - 0.5) / fps.max(1.0)).max(0.0);
        let mut child = cmd
            .args(["-v", "error", "-ss", &format!("{start_sec:.4}"), "-i"])
            .arg(path)
            .args(["-an", "-sn", "-vf", &format!("scale={width}:{height}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        Some(Self { child, stdout, width, height, next_index: start_index, last: None, ended: false })
    }

    /// 直近に返したフレーム。
    pub(crate) fn last_frame(&self) -> Option<RgbaFrame> {
        self.last.as_ref().map(|(_, f)| f.clone())
    }

    /// `index` 番目のフレームを返します。後戻り・大きな先送りで再起動が必要なら `None`。
    /// 動画の終端を越えた要求には最後のフレームを返し続けます。
    pub(crate) fn frame_at(&mut self, index: u64) -> Option<RgbaFrame> {
        if let Some((last_index, frame)) = &self.last {
            if *last_index == index {
                return Some(frame.clone());
            }
        }
        if index < self.next_index || index - self.next_index > SKIP_LIMIT {
            return if self.ended { self.last.as_ref().map(|(_, f)| f.clone()) } else { None };
        }
        if self.ended {
            return self.last.as_ref().map(|(_, f)| f.clone());
        }
        let len = (self.width * self.height * 4) as usize;
        loop {
            let mut pixels = vec![0u8; len];
            if self.stdout.read_exact(&mut pixels).is_err() {
                self.ended = true;
                return self.last.as_ref().map(|(_, f)| f.clone());
            }
            let current = self.next_index;
            self.next_index += 1;
            if current == index {
                let frame = RgbaFrame { width: self.width, height: self.height, pixels };
                self.last = Some((current, frame.clone()));
                return Some(frame);
            }
        }
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
