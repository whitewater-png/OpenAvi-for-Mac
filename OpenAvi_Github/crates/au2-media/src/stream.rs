//! 常駐 ffmpeg による動画の連続デコード。
//!
//! フレームごとにプロセスを起動すると 1 枚 100ms 前後かかり再生が成り立たない。ここでは 1 本の動画につき
//! ffmpeg を 1 プロセス常駐させ、標準出力から RGBA フレームを順に読む。再生（昇順アクセス）では
//! 次のフレームを読むだけで済み、少しの先送りは読み捨てで追いつく。後戻りや大きなシークだけ再起動する。

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::RgbaFrame;

/// シーク位置を目的のフレームより手前にずらす秒数。`-noaccurate_seek` は目的の直前のキーフレームに着地するとは
/// 限らず（MPEG-TS やキーフレームの時刻が DTS で決まる MOV では、直後のキーフレームに着地して手前のフレームを失う）、
/// 少し手前から読み始めれば、fps フィルタが動画の先頭から番号を振るので、着地点によらず結果が同じになる。
/// 代償は、シークのたびにこの秒数ぶんのデコードが増えること（キーフレーム間隔がこれより長い TS は未対応）。
const SEEK_MARGIN_SECS: f64 = 2.0;

/// これ以内の先送りは、再起動せずフレームを読み捨てて追いつく。再起動は少なくとも `SEEK_MARGIN_SECS` ぶんをデコードし直すので、
/// それより短い先送りは読み捨てのほうが必ず安い（高 fps ほど枚数が増える）。
pub(crate) fn skip_limit(fps: f64) -> u64 {
    (((SEEK_MARGIN_SECS + 0.5) * fps).ceil() as u64).max(45)
}

pub(crate) struct VideoStream {
    child: Child,
    stdout: ChildStdout,
    width: u32,
    height: u32,
    /// 次に読み出されるフレームの番号（動画のネイティブフレーム）
    next_index: u64,
    last: Option<(u64, RgbaFrame)>,
    ended: bool,
    /// ffmpeg が異常終了した（強制終了・クラッシュなど）。本当の終端とは区別して、記録せず作り直す
    broken: bool,
    skip_limit: u64,
}

impl VideoStream {
    /// `start_index` 番目（ネイティブ fps 基準）のフレームから連続デコードを開始します。
    ///
    /// `stream_start` は映像ストリームの開始時刻[秒]。
    ///
    /// 起動方法は、動画が固定・可変フレームレートのどちらでも同じ（どちらか分からない動画があるため）:
    /// - 通常の（正確な）シークはシーク位置より前のフレームを捨てる。静止区間では「そのとき画面に出ているフレーム」まで
    ///   捨ててしまい、以降のコマ番号が後ろの内容にずれる。そこで `-noaccurate_seek -copyts` でキーフレームから
    ///   全フレームを出させ、
    /// - fps フィルタで動画の先頭からの番号を振り直し、`trim` で目的の番号まで読み捨てる。
    /// - fps フィルタの前に `settb=AVTB`: 入力のタイムベースがフレーム間隔そのもの（AVI など）だと、fps フィルタが
    ///   `start_time` を入力のタイムベースに丸め、半フレームのずらしが 1 フレームになってしまう。
    /// - fps フィルタの枠は、各フレームの表示時刻が枠の真ん中に来るよう半フレームずらす（`start_time` を半フレーム手前）。
    ///   枠の境目にぴったり合わせると、表示時刻が僅かに早いだけで 1 つ前の枠に入り、重複と欠落が起きる
    ///   （撮影時刻のゆらぎがある録画、MKV/WebM のミリ秒精度の時刻など）。
    pub(crate) fn spawn(
        ffmpeg: &Path,
        path: &Path,
        start_index: u64,
        fps: f64,
        (width, height): (u32, u32),
        stream_start: f64,
    ) -> Option<Self> {
        let mut cmd = Command::new(ffmpeg);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let fps_safe = fps.max(1.0);
        let slot_start = stream_start - 0.5 / fps_safe;
        let mut child = cmd
            // -max_error_rate 1: デコードエラーの割合で終了コードを変えさせない（既定は 2/3 を超えると 69 で終了し、
            // 読めたフレームがあっても「異常終了」に見えてしまう）
            .args(["-v", "error", "-max_error_rate", "1", "-noaccurate_seek", "-copyts"])
            // 先頭付近は -ss を付けない。B フレーム付きの AVI では `-ss 0` が先頭ではなく後ろのキーフレームに着地する
            .args(seek_arg(start_index as f64 / fps_safe - SEEK_MARGIN_SECS))
            .arg("-i")
            .arg(path)
            // round=down: 枠の中の最後のフレームを採る（切り捨てによる番号計算と一致）
            .args(["-an", "-sn", "-vf"])
            .arg(format!("settb=AVTB,fps={fps}:round=down:start_time={slot_start},trim=start_frame={start_index},scale={width}:{height}"))
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        Some(Self { child, stdout, width, height, next_index: start_index, last: None, ended: false, broken: false, skip_limit: skip_limit(fps) })
    }

    /// ffmpeg が異常終了したか。そのストリームは捨てて作り直す。
    pub(crate) fn is_broken(&self) -> bool {
        self.broken
    }

    /// 1 枚でもフレームを返したか（コピーを作らずに調べる）。
    pub(crate) fn has_last(&self) -> bool {
        self.last.is_some()
    }

    /// ffmpeg のプロセス ID（テスト用）。
    #[cfg(test)]
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 終端まで読み切っている場合の、動画のフレーム数（= 最初に存在しないフレーム番号）。
    pub(crate) fn end_index(&self) -> Option<u64> {
        self.ended.then_some(self.next_index)
    }

    /// 直近に返した（= 終端まで読み切った場合は最後の）フレーム。
    pub(crate) fn last_clone(&self) -> Option<RgbaFrame> {
        self.last.as_ref().map(|(_, f)| f.clone())
    }

    /// `index` 番目のフレームを返します。後戻り・大きな先送りで再起動が必要なら `None`。
    /// 動画の終端を越えた要求には最後のフレームを返し続けます。
    pub(crate) fn frame_at(&mut self, index: u64) -> Option<RgbaFrame> {
        if self.broken {
            return None;
        }
        if let Some((last_index, frame)) = &self.last {
            if *last_index == index {
                return Some(frame.clone());
            }
        }
        // 後戻りは常に再起動（終端後でも、最後のフレームを返してはいけない）
        if index < self.next_index {
            return None;
        }
        // 終端に達している場合、それより先の要求は最後のフレームを保持表示する
        if self.ended {
            return self.last.as_ref().map(|(_, f)| f.clone());
        }
        if index - self.next_index > self.skip_limit {
            return None;
        }
        let len = self.width as usize * self.height as usize * 4;
        // 読み捨てたフレームのうち最後のものを `held` に取っておく（途中で終端に当たったら、これが動画の本当の最後のフレーム）。
        // バッファは 2 本を入れ替えて使い回す（4K では 1 枚 33MB。読み捨てるたびに確保しない）
        let mut buf = vec![0u8; len];
        let mut held: Vec<u8> = Vec::new();
        let mut has_held = false;
        loop {
            if self.stdout.read_exact(&mut buf).is_err() {
                // 読めなくなった理由を見分ける: 正常終了なら本当の終端。強制終了・クラッシュなら終端ではない
                if !self.child.wait().is_ok_and(|status| status.success()) {
                    self.broken = true;
                    return None;
                }
                self.ended = true;
                if has_held {
                    let frame = RgbaFrame { width: self.width, height: self.height, pixels: held };
                    self.last = Some((self.next_index - 1, frame.clone()));
                    return Some(frame);
                }
                return self.last.as_ref().map(|(_, f)| f.clone());
            }
            let current = self.next_index;
            self.next_index += 1;
            if current == index {
                let frame = RgbaFrame { width: self.width, height: self.height, pixels: buf };
                self.last = Some((current, frame.clone()));
                return Some(frame);
            }
            std::mem::swap(&mut buf, &mut held);
            has_held = true;
            buf.resize(len, 0);
        }
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 動画の最後のフレームを返します。取得できなければ `None`。
///
/// コンテナの終端から遡ってデコードする。音声のほうが長いと映像の終端はコンテナの終端より前にあるため、
/// 1 秒分で映像が 1 枚も出なければ、遡る範囲を広げて再試行する（短い動画なら先頭から全体を読む）。
pub(crate) fn read_last_frame(ffmpeg: &Path, path: &Path, width: u32, height: u32) -> Option<RgbaFrame> {
    [1, 10, 60].into_iter().find_map(|back_secs| read_last_frame_within(ffmpeg, path, width, height, back_secs))
}

fn read_last_frame_within(ffmpeg: &Path, path: &Path, width: u32, height: u32, back_secs: u32) -> Option<RgbaFrame> {
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = cmd
        .args(["-v", "error", "-max_error_rate", "1", "-sseof", &format!("-{back_secs}"), "-i"])
        .arg(path)
        .args(["-an", "-sn", "-vf", &format!("scale={width}:{height}"), "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let len = width as usize * height as usize * 4;
    let mut last: Option<Vec<u8>> = None;
    loop {
        let mut pixels = vec![0u8; len];
        if stdout.read_exact(&mut pixels).is_err() {
            break;
        }
        last = Some(pixels);
    }
    let _ = child.kill();
    let _ = child.wait();
    last.map(|pixels| RgbaFrame { width, height, pixels })
}

/// `-ss` を付ける最小の秒数。これより手前（= 先頭付近）では付けず、先頭から読む。
/// B フレーム付きの AVI では、`-ss` が 0〜約 0.13 秒だと先頭ではなく後ろのキーフレームに着地してしまうため。
/// 代償は、先頭付近のシークで最大この秒数ぶん余計にデコードすること。
const MIN_SEEK_SECS: f64 = 0.5;

/// `-ss` の引数。先頭付近（`MIN_SEEK_SECS` 未満）なら付けない。
pub(crate) fn seek_arg(seconds: f64) -> Vec<String> {
    if seconds >= MIN_SEEK_SECS {
        vec!["-ss".into(), format!("{seconds:.4}")]
    } else {
        Vec::new()
    }
}
