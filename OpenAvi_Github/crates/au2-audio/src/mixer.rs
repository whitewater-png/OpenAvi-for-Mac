//! 音声オブジェクトのデコードとミキシング。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use au2_core::scene_time::{scene_index, ScenePlayback};
use au2_core::{Effect, EffectKind, Frame, Project, Scene, TimelineObject};

use crate::{CHANNELS, SAMPLE_RATE};

/// デコード済み音声（48kHz ステレオ f32）のキャッシュ。
#[derive(Default)]
pub struct AudioCache {
    ffmpeg: Option<PathBuf>,
    clips: HashMap<PathBuf, Option<Arc<Vec<f32>>>>,
}

impl AudioCache {
    pub fn new() -> Self {
        Self { ffmpeg: au2_media::locate_ffmpeg(), clips: HashMap::new() }
    }

    /// デコード済みデータを直接登録します（テスト用・外部で用意した音声用）。
    pub fn insert(&mut self, path: impl Into<PathBuf>, samples: Vec<f32>) {
        self.clips.insert(path.into(), Some(Arc::new(samples)));
    }

    /// 音声ファイルをデコードして返します。失敗は 1 度だけ試し、以後は `None` を返します。
    pub fn get(&mut self, path: &Path) -> Option<Arc<Vec<f32>>> {
        if !self.clips.contains_key(path) {
            let decoded = self.decode(path).map(Arc::new);
            if decoded.is_none() {
                log::warn!("音声をデコードできません: {}", path.display());
            }
            self.clips.insert(path.to_path_buf(), decoded);
        }
        self.clips.get(path)?.clone()
    }

    fn decode(&self, path: &Path) -> Option<Vec<f32>> {
        let ffmpeg = self.ffmpeg.as_ref()?;
        let mut cmd = Command::new(ffmpeg);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let out = cmd
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-vn"])
            .args(mono_filter(ffmpeg, path))
            .args(["-f", "f32le", "-ac", "2", "-ar", &SAMPLE_RATE.to_string(), "-"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() || out.stdout.len() < 4 * CHANNELS {
            return None;
        }
        Some(out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
    }
}

/// モノラル音源には、両チャンネルへ等レベルで複製するフィルタを返します。
/// ffmpeg 既定のモノラル → ステレオ変換は -3dB になり、AviUtl より小さく鳴ってしまうため。
fn mono_filter(ffmpeg: &Path, path: &Path) -> Vec<&'static str> {
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let info = cmd
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default();
    let is_mono = info.lines().any(|l| l.contains("Audio:") && l.contains(", mono"));
    if is_mono {
        vec!["-af", "pan=stereo|FL=FC|FR=FC"]
    } else {
        Vec::new()
    }
}

/// フレーム → サンプル位置（`frame` 番目の先頭）。
pub fn frame_to_sample(frame: Frame, fps: f64) -> usize {
    (frame as f64 * SAMPLE_RATE as f64 / fps.max(1.0)).round() as usize
}

/// `start..end`（end は含まない）フレームの範囲をミックスして返します。
/// 長さは `frame_to_sample(end) - frame_to_sample(start)` サンプル × 2ch。
///
/// 対象は音声ファイルオブジェクト（音量・左右・再生位置・再生速度・ループ）。表示されているレイヤーのみ。
/// `resolve` はオブジェクトに書かれたファイル名を実際のパスへ解決する関数。
/// シーン(音声) オブジェクトは描かない（参照先のシーンが無い）。それも鳴らすには [`mix_project`] を使う。
pub fn mix(
    scene: &Scene,
    cache: &mut AudioCache,
    start: Frame,
    end: Frame,
    resolve: &dyn Fn(&str) -> PathBuf,
) -> Vec<f32> {
    clamp(mix_scene(scene, &[], &mut Vec::new(), cache, start, end, resolve))
}

/// `project.scenes[scene_index]` の `start..end` をミックスします。[`mix`] に加えて、シーン(音声) オブジェクトが
/// 同じプロジェクトの別シーンの音声を鳴らす（循環参照は鳴らさない）。シーンが無ければ無音。
pub fn mix_project(
    project: &Project,
    scene_index: usize,
    cache: &mut AudioCache,
    start: Frame,
    end: Frame,
    resolve: &dyn Fn(&str) -> PathBuf,
) -> Vec<f32> {
    let Some(scene) = project.scenes.get(scene_index) else {
        return Vec::new();
    };
    clamp(mix_scene(scene, &project.scenes, &mut vec![scene_index], cache, start, end, resolve))
}

/// シーン(音声) の入れ子の上限。
const MAX_SCENE_DEPTH: usize = 8;

fn clamp(mut out: Vec<f32>) -> Vec<f32> {
    for s in &mut out {
        *s = s.clamp(-1.0, 1.0);
    }
    out
}

/// クリップせずにミックスする（入れ子のシーンは親でまとめてクリップする）。`stack` は鳴らしている途中のシーンの添字。
fn mix_scene(
    scene: &Scene,
    scenes: &[Scene],
    stack: &mut Vec<usize>,
    cache: &mut AudioCache,
    start: Frame,
    end: Frame,
    resolve: &dyn Fn(&str) -> PathBuf,
) -> Vec<f32> {
    let fps = scene.settings.frame_rate.as_f64();
    let base = frame_to_sample(start, fps);
    let total = frame_to_sample(end.max(start), fps) - base;
    let mut out = vec![0.0f32; total * CHANNELS];

    for layer in scene.layers.iter().filter(|l| l.visible) {
        for obj in &layer.objects {
            if obj.frame_end < start || obj.frame_start >= end {
                continue;
            }
            let Some(src) = obj.source().filter(|s| s.enabled) else { continue };
            if src.kind == EffectKind::SceneAudio {
                mix_scene_audio(&mut out, base, total, obj, fps, scenes, stack, cache, resolve);
                continue;
            }
            if src.name != "音声ファイル" {
                continue;
            }
            let Some(file) = src.text("file").filter(|f| !f.trim().is_empty()) else { continue };
            let Some(clip) = cache.get(&resolve(file)) else { continue };
            mix_object(&mut out, base, total, obj, &clip, fps);
        }
    }
    out
}

/// 標準再生の音量・左右から、左右のゲイン。
fn gains(play: Option<&Effect>, rel: Frame) -> (f32, f32) {
    let volume = play.map_or(100.0, |p| p.eval("音量", rel, 100.0)) as f32 / 100.0;
    let pan = play.map_or(0.0, |p| p.eval("左右", rel, 0.0)).clamp(-100.0, 100.0) as f32 / 100.0;
    (volume * (1.0 - pan).min(1.0), volume * (1.0 + pan).min(1.0))
}

/// シーン(音声): 参照先のシーンを、必要な範囲だけ再帰的にミックスして足し込む。
///
/// 近似: 再生位置・再生速度・ループ再生はオブジェクト先頭の値で固定（動かしても追従しない）。速度変更は最近傍サンプル。
/// 子シーンの長さを越えたら、ループ再生なら先頭へ戻り、そうでなければ無音（音声ファイルと同じ）。
#[allow(clippy::too_many_arguments)]
fn mix_scene_audio(
    out: &mut [f32],
    base: usize,
    total: usize,
    obj: &TimelineObject,
    fps: f64,
    scenes: &[Scene],
    stack: &mut Vec<usize>,
    cache: &mut AudioCache,
    resolve: &dyn Fn(&str) -> PathBuf,
) {
    let Some(src) = obj.source() else { return };
    let Some(index) = scene_index(src, scenes) else { return };
    if stack.contains(&index) || stack.len() >= MAX_SCENE_DEPTH {
        return;
    }
    let child = &scenes[index];
    let child_fps = child.settings.frame_rate.as_f64().max(1.0);
    let pb = ScenePlayback::of(src, 0);
    let obj_start = frame_to_sample(obj.frame_start, fps);
    let lo = obj_start.max(base);
    let hi = frame_to_sample(obj.frame_end + 1, fps).min(base + total);
    if lo >= hi {
        return;
    }
    let start_samples = pb.start / child_fps * SAMPLE_RATE as f64;
    let pos = |s: usize| (start_samples + (s - obj_start) as f64 * pb.speed) as usize;
    let child_len = match child.settings.length {
        0 => usize::MAX,
        len => frame_to_sample(len, child_fps),
    };
    let looping = pb.looping && child_len != usize::MAX;
    // 子シーンでミックスする範囲（サンプル）
    let (c_lo, c_hi) = if looping { (0, child_len) } else { (pos(lo), (pos(hi - 1) + 1).min(child_len)) };
    if c_lo >= c_hi {
        return;
    }
    let to_frame = |sample: usize| (sample as f64 * child_fps / SAMPLE_RATE as f64) as Frame;
    let (f_lo, f_hi) = (to_frame(c_lo), to_frame(c_hi).saturating_add(2));
    stack.push(index);
    let buf = mix_scene(child, scenes, stack, cache, f_lo, f_hi, resolve);
    stack.pop();
    let buf_base = frame_to_sample(f_lo, child_fps);

    let play = obj.effects.iter().find(|e| e.name == "標準再生");
    let mut cached_frame = Frame::MAX;
    let (mut gain_l, mut gain_r) = (0.0f32, 0.0f32);
    for s in lo..hi {
        let rel = (((s - obj_start) as f64 * fps / SAMPLE_RATE as f64) as Frame).min(obj.length() - 1);
        if rel != cached_frame {
            cached_frame = rel;
            (gain_l, gain_r) = gains(play, rel);
        }
        let mut p = pos(s);
        if p >= child_len {
            if !looping {
                continue;
            }
            p %= child_len;
        }
        let Some(i) = p.checked_sub(buf_base).filter(|i| (i + 1) * CHANNELS <= buf.len()) else { continue };
        let o = (s - base) * CHANNELS;
        out[o] += buf[i * CHANNELS] * gain_l;
        out[o + 1] += buf[i * CHANNELS + 1] * gain_r;
    }
}

fn mix_object(out: &mut [f32], base: usize, total: usize, obj: &TimelineObject, clip: &[f32], fps: f64) {
    let Some(src) = obj.source() else { return };
    let play = obj.effects.iter().find(|e| e.name == "標準再生");
    let clip_frames = clip.len() / CHANNELS;
    if clip_frames == 0 {
        return;
    }
    let obj_start = frame_to_sample(obj.frame_start, fps);
    let obj_end = frame_to_sample(obj.frame_end + 1, fps);
    let looping = src.eval("ループ再生", 0, 0.0) >= 0.5;

    // 音量・左右・再生位置・速度はフレーム単位で評価する
    let lo = obj_start.max(base);
    let hi = obj_end.min(base + total);
    let mut cached_frame = Frame::MAX;
    let (mut gain_l, mut gain_r, mut start_sec, mut speed) = (0.0f32, 0.0f32, 0.0f64, 1.0f64);
    for s in lo..hi {
        let rel = (((s - obj_start) as f64 * fps / SAMPLE_RATE as f64) as Frame).min(obj.length() - 1);
        if rel != cached_frame {
            cached_frame = rel;
            (gain_l, gain_r) = gains(play, rel);
            start_sec = src.eval("再生位置", rel, 0.0);
            speed = (src.eval("再生速度", rel, 100.0) / 100.0).max(0.0);
        }
        let t = (s - obj_start) as f64 / SAMPLE_RATE as f64;
        let mut pos = ((start_sec + t * speed) * SAMPLE_RATE as f64) as usize;
        if pos >= clip_frames {
            if !looping {
                continue;
            }
            pos %= clip_frames;
        }
        let o = (s - base) * CHANNELS;
        out[o] += clip[pos * CHANNELS] * gain_l;
        out[o + 1] += clip[pos * CHANNELS + 1] * gain_r;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use au2_core::{Project, Scene, SceneSettings, Track};

    fn scene() -> Scene {
        // 30fps、48000Hz → 1 フレーム = 1600 サンプル
        Scene::new("t", SceneSettings::default())
    }

    fn set(obj: &mut TimelineObject, effect: &str, key: &str, value: f64) {
        let e = obj.effects.iter_mut().find(|e| e.name == effect).unwrap();
        *e.param_mut(key).unwrap() = au2_core::ParamValue::Track(Track::constant(value, 1));
    }

    /// 全サンプルが `v` の `secs` 秒分のステレオクリップ
    fn constant_clip(v: f32, secs: f64) -> Vec<f32> {
        vec![v; (secs * SAMPLE_RATE as f64) as usize * CHANNELS]
    }

    fn resolve(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn frame_to_sample_uses_scene_fps() {
        assert_eq!(frame_to_sample(30, 30.0), 48_000);
        assert_eq!(frame_to_sample(1, 30.0), 1_600);
    }

    #[test]
    fn volume_and_range_are_applied() {
        let mut cache = AudioCache::default();
        cache.insert("a.wav", constant_clip(0.5, 5.0));
        let mut s = scene();
        let mut obj = TimelineObject::new_audio(10, 19, "a.wav"); // フレーム 10..=19
        set(&mut obj, "標準再生", "音量", 50.0);
        s.insert_object(0, obj);
        let out = mix(&s, &mut cache, 0, 30, &resolve);
        assert_eq!(out.len(), 30 * 1600 * 2);
        assert_eq!(out[0], 0.0, "範囲前は無音");
        let at = |frame: usize| out[frame * 1600 * 2];
        assert!((at(10) - 0.25).abs() < 1e-6, "0.5 × 50%");
        assert!((at(19) - 0.25).abs() < 1e-6);
        assert_eq!(at(20), 0.0, "範囲後は無音");
    }

    #[test]
    fn pan_and_mixing_clamp() {
        let mut cache = AudioCache::default();
        cache.insert("a.wav", constant_clip(0.8, 2.0));
        let mut s = scene();
        let mut left = TimelineObject::new_audio(0, 9, "a.wav");
        set(&mut left, "標準再生", "左右", -100.0); // 完全に左
        s.insert_object(0, left);
        let out = mix(&s, &mut cache, 0, 10, &resolve);
        assert!((out[0] - 0.8).abs() < 1e-6 && out[1] == 0.0);

        // 同じ音を 2 つ重ねると 1.6 → 1.0 にクリップ
        s.insert_object(1, TimelineObject::new_audio(0, 9, "a.wav"));
        let out = mix(&s, &mut cache, 0, 10, &resolve);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn start_position_speed_and_loop() {
        // 前半 1 秒が 0.1、後半 1 秒が 0.9 のクリップ
        let mut clip = constant_clip(0.1, 1.0);
        clip.extend(constant_clip(0.9, 1.0));
        let mut cache = AudioCache::default();
        cache.insert("a.wav", clip);

        let mut s = scene();
        let mut obj = TimelineObject::new_audio(0, 29, "a.wav"); // 1 秒間
        set(&mut obj, "音声ファイル", "再生位置", 1.0); // 1 秒目から
        s.insert_object(0, obj);
        assert!((mix(&s, &mut cache, 0, 1, &resolve)[0] - 0.9).abs() < 1e-6);

        // 2 倍速: 0.5 秒時点の再生位置は 1.0 秒 → 後半の値
        let mut s = scene();
        let mut obj = TimelineObject::new_audio(0, 29, "a.wav");
        set(&mut obj, "音声ファイル", "再生速度", 200.0);
        s.insert_object(0, obj);
        let out = mix(&s, &mut cache, 0, 30, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(0.25) - 0.1).abs() < 1e-6);
        assert!((at(0.75) - 0.9).abs() < 1e-6);

        // 再生位置がクリップの終端を越えたら、ループなしは無音・ループありは先頭へ戻る
        let mut s = scene();
        let mut obj = TimelineObject::new_audio(0, 59, "a.wav"); // 2 秒間
        set(&mut obj, "音声ファイル", "再生位置", 1.5);
        s.insert_object(0, obj.clone());
        let out = mix(&s, &mut cache, 0, 60, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(0.1) - 0.9).abs() < 1e-6);
        assert_eq!(at(0.9), 0.0, "終端を越えた後は無音");

        set(&mut obj, "音声ファイル", "ループ再生", 1.0);
        let mut s = scene();
        s.insert_object(0, obj);
        let out = mix(&s, &mut cache, 0, 60, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(0.9) - 0.1).abs() < 1e-6, "ループで先頭の 0.1 に戻る");
    }

    #[test]
    fn hidden_layers_and_missing_files_are_silent() {
        let mut cache = AudioCache::default();
        cache.insert("a.wav", constant_clip(0.5, 1.0));
        let mut s = scene();
        s.insert_object(0, TimelineObject::new_audio(0, 9, "a.wav"));
        s.insert_object(1, TimelineObject::new_audio(0, 9, "missing-and-undecodable.wav"));
        s.layers[0].visible = false;
        assert!(mix(&s, &mut cache, 0, 10, &resolve).iter().all(|&v| v == 0.0));
    }

    /// Root（空）と、`clip` を 0..=len-1 フレームに置いた子シーン（長さ `len` フレーム）。
    fn project_with_child(clip: &str, len: Frame) -> Project {
        let mut child = Scene::new("Scene1", SceneSettings { length: len, ..Default::default() });
        child.insert_object(0, TimelineObject::new_audio(0, len - 1, clip));
        let mut p = Project::new("p", scene());
        p.scenes.push(child);
        p
    }

    #[test]
    fn scene_audio_plays_the_other_scene() {
        let mut cache = AudioCache::default();
        cache.insert("a.wav", constant_clip(0.5, 5.0));
        let mut p = project_with_child("a.wav", 30);
        p.scenes[1].layers[0].objects[0].frame_end = 59; // 子シーンの長さより長く置いた音も、長さで切れる
        let mut obj = TimelineObject::new_scene_audio(10, 49, 1);
        set(&mut obj, "標準再生", "音量", 50.0);
        p.scenes[0].insert_object(0, obj);
        let out = mix_project(&p, 0, &mut cache, 0, 50, &resolve);
        let at = |frame: usize| out[frame * 1600 * 2];
        assert_eq!(at(5), 0.0, "範囲前は無音");
        assert!((at(10) - 0.25).abs() < 1e-6, "子シーンの音 × 50%: {}", at(10));
        assert!((at(39) - 0.25).abs() < 1e-6, "子の最後のフレーム");
        assert_eq!(at(40), 0.0, "子シーンの長さ（30 フレーム）を越えたら無音");
        // Project を渡さない mix() では鳴らない
        assert!(mix(&p.scenes[0], &mut cache, 0, 50, &resolve).iter().all(|&v| v == 0.0));
        assert!(mix_project(&p, 9, &mut cache, 0, 50, &resolve).is_empty(), "存在しないシーン");
    }

    #[test]
    fn scene_audio_start_speed_and_loop() {
        // 子シーン: 前半 1 秒が 0.1、後半 1 秒が 0.9（60 フレーム）
        let mut clip = constant_clip(0.1, 1.0);
        clip.extend(constant_clip(0.9, 1.0));
        let mut cache = AudioCache::default();
        cache.insert("a.wav", clip);
        let base = project_with_child("a.wav", 60);

        let mut p = base.clone();
        let mut obj = TimelineObject::new_scene_audio(0, 59, 1);
        set(&mut obj, "シーン(音声)", "再生位置", 31.0); // 子の 30 フレーム目 = 1 秒目から
        p.scenes[0].insert_object(0, obj.clone());
        let out = mix_project(&p, 0, &mut cache, 0, 60, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(0.1) - 0.9).abs() < 1e-6, "{}", at(0.1));
        assert_eq!(at(1.5), 0.0, "子シーンの終端を越えたら無音");

        set(&mut obj, "シーン(音声)", "ループ再生", 1.0);
        let mut p = base.clone();
        p.scenes[0].insert_object(0, obj);
        let out = mix_project(&p, 0, &mut cache, 0, 60, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(1.5) - 0.1).abs() < 1e-6, "ループで子の先頭（0.1）に戻る: {}", at(1.5));

        let mut p = base;
        let mut obj = TimelineObject::new_scene_audio(0, 29, 1);
        set(&mut obj, "シーン(音声)", "再生速度", 200.0);
        p.scenes[0].insert_object(0, obj);
        let out = mix_project(&p, 0, &mut cache, 0, 30, &resolve);
        let at = |sec: f64| out[(sec * SAMPLE_RATE as f64) as usize * 2];
        assert!((at(0.25) - 0.1).abs() < 1e-6);
        assert!((at(0.75) - 0.9).abs() < 1e-6, "2 倍速: 0.75 秒は子の 1.5 秒: {}", at(0.75));
    }

    #[test]
    fn recursive_scene_audio_terminates_and_plays_once() {
        let mut cache = AudioCache::default();
        cache.insert("a.wav", constant_clip(0.25, 2.0));
        let mut p = project_with_child("a.wav", 30);
        // Root → 自分自身、Scene1 → Scene2 → Scene1 の循環
        p.scenes[0].insert_object(0, TimelineObject::new_scene_audio(0, 29, 0));
        p.scenes[0].insert_object(1, TimelineObject::new_scene_audio(0, 29, 1));
        p.scenes[1].insert_object(1, TimelineObject::new_scene_audio(0, 29, 2));
        let mut scene2 = Scene::new("Scene2", SceneSettings { length: 30, ..Default::default() });
        scene2.insert_object(0, TimelineObject::new_scene_audio(0, 29, 1));
        p.scenes.push(scene2);
        let out = mix_project(&p, 0, &mut cache, 0, 30, &resolve);
        assert!((out[1600 * 2] - 0.25).abs() < 1e-6, "Scene1 の音が 1 回だけ: {}", out[1600 * 2]);
    }

    #[test]
    fn decodes_a_real_file_through_ffmpeg() {
        let mut cache = AudioCache::new();
        if cache.ffmpeg.is_none() {
            return;
        }
        // 0.5 秒・8bit モノラル 8kHz の WAV を手で作り、ffmpeg 経由で 48kHz ステレオに変換できること
        let mut wav = Vec::new();
        let data = vec![255u8; 4000];
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let path = std::env::temp_dir().join(format!("au2-audio-test-{}.wav", std::process::id()));
        std::fs::write(&path, &wav).unwrap();
        let clip = cache.get(&path).expect("デコードできる");
        std::fs::remove_file(&path).ok();
        let secs = clip.len() as f64 / CHANNELS as f64 / SAMPLE_RATE as f64;
        assert!((secs - 0.5).abs() < 0.02, "長さ {secs}");
        assert!(clip[clip.len() / 2] > 0.9, "最大振幅は約 1.0: {}", clip[clip.len() / 2]);
    }
}
