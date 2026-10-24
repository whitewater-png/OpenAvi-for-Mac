//! シーンオブジェクト（別シーンをテクスチャに描いてスプライトにする）・フレームバッファ・直前オブジェクト。
//!
//! 子シーンは、親の `targets` を退避して入れ子の深さごとの作業用フレームバッファへ同じ `render_scene` で描き、
//! 結果をオブジェクトごとのテクスチャへ写してから親のスプライトとして使う（キューへの投入順で依存関係を保つ）。

use std::sync::Arc;

use au2_core::{scene_time, Frame, Project, Scene, TimelineObject};
use au2_media::RgbaFrame;

use super::{Renderer, SpriteUniform, Targets, TexEntry, FORMAT, SHAPE_BOX};
use crate::eval::DrawParams;
use crate::media::MediaSource;
use crate::RenderError;

/// シーンの入れ子の上限（自分自身を含む循環は深さに関係なく描かない）。
pub(super) const MAX_SCENE_DEPTH: usize = 8;
/// シーンオブジェクトの描画結果を残す数（超えたら全消去する簡易方式）。
const SCENE_TEXTURE_LIMIT: usize = 32;

/// 1 回の `render_scene` の文脈。
pub(super) struct SceneCtx<'a> {
    /// シーンオブジェクトが参照できるシーン（`Project::scenes`）。空ならシーンオブジェクトは描かない
    pub scenes: &'a [Scene],
    /// 描画中のシーンの添字（外側から）。循環参照の検出に使う
    pub stack: Vec<usize>,
    /// フレームの背景（ルートは不透明な黒、子シーンは透明）
    pub background: wgpu::Color,
}

impl<'a> SceneCtx<'a> {
    pub fn root(scenes: &'a [Scene], index: Option<usize>) -> Self {
        Self { scenes, stack: index.into_iter().collect(), background: wgpu::Color::BLACK }
    }
}

/// 直前に描いたスプライト（直前オブジェクト用）。
pub(super) struct PrevSprite {
    pub uniform: SpriteUniform,
    pub tex: Option<Arc<TexEntry>>,
    /// そのスプライトを描いたときの拡大率
    pub zoom: f32,
}

impl PrevSprite {
    /// 直前のオブジェクトの絵を、直前オブジェクト自身の標準描画（位置・拡大率・回転・透明度）で描き直す。
    /// 近似: 拡大率は直前のオブジェクトの拡大率を外してから掛け直す（フィルタ効果はまだ物体単位にかからないので、絵は本体そのもの）。
    pub fn redraw(&self, p: DrawParams) -> (SpriteUniform, Option<Arc<TexEntry>>) {
        let scale = if self.zoom > 0.0 { p.zoom / self.zoom } else { p.zoom };
        let half = [self.uniform.half[0] * scale, self.uniform.half[1] * scale];
        let uniform = SpriteUniform { center: [p.x, p.y], angle: p.angle, alpha: p.alpha, half, ..self.uniform };
        (uniform, self.tex.clone())
    }
}

/// テクスチャをそのまま貼るスプライト。
fn textured(canvas: [f32; 2], p: DrawParams, half: [f32; 2]) -> SpriteUniform {
    SpriteUniform {
        canvas,
        center: [p.x, p.y],
        half,
        angle: p.angle,
        alpha: p.alpha,
        color: [1.0, 1.0, 1.0, 1.0],
        mode: 0,
        shape: SHAPE_BOX,
        sides: 0,
        _pad: 0,
        ext: [0.0; 16],
        blend: [0; 4],
    }
}

/// フレームバッファオブジェクトのスプライト: 写し取った画面全体を、標準描画で置く。
pub(super) fn framebuffer_sprite(
    canvas: [f32; 2],
    p: DrawParams,
    tex: Arc<TexEntry>,
) -> (SpriteUniform, Option<Arc<TexEntry>>) {
    (textured(canvas, p, [canvas[0] * p.zoom / 2.0, canvas[1] * p.zoom / 2.0]), Some(tex))
}

/// `Op::Snapshot` の記録: 現在の合成結果を `dst` へ写し、`clear` なら背景で塗り直す。
pub(super) fn record_snapshot(
    encoder: &mut wgpu::CommandEncoder,
    targets: &Targets,
    dst: &wgpu::Texture,
    clear: bool,
    background: wgpu::Color,
) {
    copy_whole(encoder, &targets.main, dst, targets.width, targets.height);
    if clear {
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("framebuffer clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &targets.main_view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(background), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        }));
    }
}

fn copy_whole(encoder: &mut wgpu::CommandEncoder, src: &wgpu::Texture, dst: &wgpu::Texture, width: u32, height: u32) {
    let at = |texture| wgpu::ImageCopyTexture {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d::ZERO,
        aspect: wgpu::TextureAspect::All,
    };
    encoder.copy_texture_to_texture(at(src), at(dst), wgpu::Extent3d { width, height, depth_or_array_layers: 1 });
}

impl Renderer {
    /// `project.scenes[scene_index]` の `frame` 番目を描画し、結果をフレームバッファに残します（読み戻しなし）。
    /// [`Renderer::render_to_framebuffer`] と違い、シーンオブジェクトが同じプロジェクトの他のシーンを表示できます。
    pub fn render_project_to_framebuffer(
        &mut self,
        project: &Project,
        scene_index: usize,
        frame: Frame,
        media: &mut dyn MediaSource,
    ) -> Result<(), RenderError> {
        let scene = project.scenes.get(scene_index).ok_or(RenderError::NoSuchScene(scene_index))?;
        self.render_scene(scene, frame, media, &SceneCtx::root(&project.scenes, Some(scene_index)))
    }

    /// `project.scenes[scene_index]` の `frame` 番目を描画して RGBA で返します（読み戻しまで同期）。
    pub fn render_project(
        &mut self,
        project: &Project,
        scene_index: usize,
        frame: Frame,
        media: &mut dyn MediaSource,
    ) -> Result<RgbaFrame, RenderError> {
        self.render_project_to_framebuffer(project, scene_index, frame, media)?;
        self.read_framebuffer()
    }

    /// 書き込み先にもサンプル元にもなる空のテクスチャ（子シーンの結果・フレームバッファの写し）。
    fn blank_texture_entry(&self, width: u32, height: u32, label: &str) -> TexEntry {
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.texture_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        TexEntry { size: (width, height), _texture: texture, bind_group }
    }

    /// フレームバッファオブジェクトの写し先（サイズごとに 1 枚を使い回す。コマンドの順序で前後の使用と分かれる）。
    pub(super) fn snapshot_texture(&mut self, width: u32, height: u32) -> Arc<TexEntry> {
        if let Some(t) = self.snapshots.get(&(width, height)) {
            return t.clone();
        }
        let entry = Arc::new(self.blank_texture_entry(width, height, "framebuffer snapshot"));
        self.snapshots.insert((width, height), entry.clone());
        entry
    }

    /// シーンオブジェクト: 参照先のシーンを子として描き、その画像をスプライトにする。
    /// 参照先が無い・循環している（描画中のシーンを参照する）・入れ子が深すぎる場合は描かない。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn scene_sprite(
        &mut self,
        obj: &TimelineObject,
        rel: Frame,
        fps: f64,
        canvas: [f32; 2],
        p: DrawParams,
        media: &mut dyn MediaSource,
        ctx: &SceneCtx<'_>,
    ) -> Option<(SpriteUniform, Option<Arc<TexEntry>>)> {
        let src = obj.source()?;
        let index = scene_time::scene_index(src, ctx.scenes)?;
        if ctx.stack.contains(&index) || ctx.stack.len() >= MAX_SCENE_DEPTH {
            return None;
        }
        let child = &ctx.scenes[index];
        let child_frame = scene_time::scene_object_frame(src, rel, fps, &child.settings);
        let mut child_ctx = SceneCtx { scenes: ctx.scenes, stack: ctx.stack.clone(), background: wgpu::Color::TRANSPARENT };
        child_ctx.stack.push(index);

        // 親の状態を退避し、深さごとの作業用フレームバッファに子シーンを描く
        let depth = ctx.stack.len();
        let parent_targets = self.targets.take();
        let parent_bounds = std::mem::take(&mut self.bounds);
        let parent_generation = self.generation;
        self.targets = self.nested_targets.remove(&depth);
        let drawn = self.render_scene(child, child_frame, media, &child_ctx).is_ok();
        let entry = if drawn { self.take_scene_result(obj) } else { None };
        if let Some(t) = self.targets.take() {
            self.nested_targets.insert(depth, t);
        }
        self.targets = parent_targets;
        self.bounds = parent_bounds;
        self.generation = parent_generation;

        let entry = entry?;
        let (w, h) = entry.size;
        Some((textured(canvas, p, [w as f32 * p.zoom / 2.0, h as f32 * p.zoom / 2.0]), Some(entry)))
    }

    /// 直前に描いた子シーン（`self.targets`）を、そのオブジェクト専用のテクスチャへ写す。
    fn take_scene_result(&mut self, obj: &TimelineObject) -> Option<Arc<TexEntry>> {
        let (width, height) = self.targets.as_ref().map(|t| (t.width, t.height))?;
        let entry = match self.scene_textures.get(&obj.id).filter(|e| e.size == (width, height)) {
            Some(e) => e.clone(),
            None => {
                if self.scene_textures.len() >= SCENE_TEXTURE_LIMIT {
                    self.scene_textures.clear();
                }
                let e = Arc::new(self.blank_texture_entry(width, height, "scene object"));
                self.scene_textures.insert(obj.id, e.clone());
                e
            }
        };
        let targets = self.targets.as_ref()?;
        let mut encoder =
            self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scene object copy") });
        copy_whole(&mut encoder, &targets.main, &entry._texture, width, height);
        self.gpu.queue.submit([encoder.finish()]);
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(x: f32, zoom: f32) -> DrawParams {
        DrawParams { x, y: -3.0, zoom, angle: 0.5, alpha: 0.25 }
    }

    #[test]
    fn previous_object_keeps_the_image_and_takes_its_own_placement() {
        let prev = PrevSprite {
            uniform: SpriteUniform { color: [1.0, 0.0, 0.0, 1.0], mode: 1, shape: 3, sides: 5, ..textured([10.0, 10.0], params(7.0, 2.0), [20.0, 10.0]) },
            tex: None,
            zoom: 2.0,
        };
        let (u, tex) = prev.redraw(params(-4.0, 0.5));
        assert!(tex.is_none());
        assert_eq!(u.center, [-4.0, -3.0]);
        assert_eq!(u.half, [5.0, 2.5], "直前の拡大率 2 を外して 0.5 を掛ける");
        assert_eq!((u.mode, u.shape, u.sides, u.color), (1, 3, 5, [1.0, 0.0, 0.0, 1.0]), "絵（図形・色）は直前のもの");
        assert_eq!((u.angle, u.alpha), (0.5, 0.25));
    }
}
