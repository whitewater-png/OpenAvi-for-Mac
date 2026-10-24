//! 合成エンジン本体。

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use au2_core::{EffectKind, Frame, ObjectCategory, Scene, TimelineObject};
use au2_media::RgbaFrame;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::eval::{eval_draw_params, DrawParams};
use crate::gpu::Gpu;
use crate::media::MediaSource;
use crate::RenderError;

/// フレームバッファのフォーマット。ガンマ空間のまま扱う（AviUtl と同じく sRGB 変換をしない）。
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// テクスチャキャッシュの上限枚数（超えたら全消去する簡易方式）。
const TEXTURE_CACHE_LIMIT: usize = 64;

/// テキストのテクスチャキャッシュキーの接頭辞（フォント変更時に無効化するため）。
const TEXT_KEY_PREFIX: &str = "\0text\0";

const SHAPE_CIRCLE: u32 = 0;
const SHAPE_BOX: u32 = 1;
const SHAPE_POLYGON: u32 = 2;
const SHAPE_STAR: u32 = 3;

/// 欠落メディアのプレースホルダ色（プレビューと同じ）。
const PLACEHOLDER_COLOR: [f32; 3] = [60.0 / 255.0, 70.0 / 255.0, 90.0 / 255.0];

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SpriteUniform {
    canvas: [f32; 2],
    center: [f32; 2],
    half: [f32; 2],
    angle: f32,
    alpha: f32,
    color: [f32; 4],
    /// 0: テクスチャ / 1: SDF 図形
    mode: u32,
    shape: u32,
    sides: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorCorrectionUniform {
    /// 明るさ・コントラスト（倍率）、色相（ラジアン）、輝度（倍率）
    a: [f32; 4],
    /// 彩度（倍率）
    b: [f32; 4],
}

struct TexEntry {
    size: (u32, u32),
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

enum Op {
    Sprite { offset: u32, tex: Option<Arc<TexEntry>> },
    ColorCorrection { offset: u32 },
}

struct Targets {
    width: u32,
    height: u32,
    main: wgpu::Texture,
    main_view: wgpu::TextureView,
    /// フィルタパスが読む用のコピー先（ping-pong 相手）
    scratch: wgpu::Texture,
    scratch_bind_group: wgpu::BindGroup,
    readback: wgpu::Buffer,
    padded_row: u32,
}

pub struct Renderer {
    gpu: Gpu,
    sprite_pipeline: wgpu::RenderPipeline,
    sprite_uniform_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    dummy_texture: Arc<TexEntry>,
    filter_pipeline: wgpu::RenderPipeline,
    filter_uniform_layout: wgpu::BindGroupLayout,
    filter_src_layout: wgpu::BindGroupLayout,
    font: Option<ab_glyph::FontVec>,
    targets: Option<Targets>,
    textures: HashMap<(String, u32), Arc<TexEntry>>,
}

fn uniform_entry(visibility: wgpu::ShaderStages, size: usize) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: true,
            min_binding_size: NonZeroU64::new(size as u64),
        },
        count: None,
    }
}

fn align_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

fn parse_hex_color(hex: &str) -> Option<[f32; 3]> {
    let v = u32::from_str_radix(hex.trim(), 16).ok()?;
    Some([((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0])
}

impl Renderer {
    pub fn new(gpu: Gpu) -> Self {
        let device = &gpu.device;

        // --- スプライト ---
        let sprite_uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite uniform"),
            entries: &[uniform_entry(
                wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                std::mem::size_of::<SpriteUniform>(),
            )],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sprite_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/sprite.wgsl").into()),
        });
        let sprite_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite pipeline layout"),
            bind_group_layouts: &[&sprite_uniform_layout, &texture_layout],
            push_constant_ranges: &[],
        });
        let over = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let sprite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite pipeline"),
            layout: Some(&sprite_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &sprite_shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &sprite_shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: Some(over),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sprite sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // --- 色調補正フィルタ ---
        let filter_uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("filter uniform"),
            entries: &[uniform_entry(wgpu::ShaderStages::FRAGMENT, std::mem::size_of::<ColorCorrectionUniform>())],
        });
        let filter_src_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("filter src"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let filter_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("color_correction.wgsl"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/color_correction.wgsl").into()),
        });
        let filter_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("filter pipeline layout"),
            bind_group_layouts: &[&filter_uniform_layout, &filter_src_layout],
            push_constant_ranges: &[],
        });
        let filter_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("color correction pipeline"),
            layout: Some(&filter_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &filter_shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &filter_shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let dummy_texture =
            Arc::new(Self::make_texture_entry(&gpu, &texture_layout, &sampler, 1, 1, &[255, 255, 255, 255]));
        Self {
            gpu,
            sprite_pipeline,
            sprite_uniform_layout,
            texture_layout,
            sampler,
            dummy_texture,
            filter_pipeline,
            filter_uniform_layout,
            filter_src_layout,
            font: None,
            targets: None,
            textures: HashMap::new(),
        }
    }

    /// テキスト描画に使うフォント（`.ttf` / `.otf` / `.ttc`）を設定します。
    pub fn set_font(&mut self, bytes: Vec<u8>, index: u32) -> bool {
        self.textures.retain(|(k, _), _| !k.starts_with(TEXT_KEY_PREFIX));
        self.font = crate::text::load_font(bytes, index);
        self.font.is_some()
    }

    /// OS に入っている日本語フォントを探して設定し、見つかったパスを返します。
    pub fn load_system_font(&mut self) -> Option<&'static str> {
        crate::text::SYSTEM_FONT_CANDIDATES.iter().copied().find(|path| {
            std::fs::read(path).is_ok_and(|bytes| self.set_font(bytes, 0))
        })
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    fn make_texture_entry(
        gpu: &Gpu,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> TexEntry {
        let texture = gpu.device.create_texture_with_data(
            &gpu.queue,
            &wgpu::TextureDescriptor {
                label: Some("media"),
                size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            rgba,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("media bind group"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        TexEntry { size: (width, height), _texture: texture, bind_group }
    }

    fn ensure_targets(&mut self, width: u32, height: u32) {
        if self.targets.as_ref().is_some_and(|t| t.width == width && t.height == height) {
            return;
        }
        let device = &self.gpu.device;
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let make = |label: &str, usage: wgpu::TextureUsages| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage,
                view_formats: &[],
            })
        };
        let main = make(
            "framebuffer",
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let scratch = make("framebuffer scratch", wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING);
        let main_view = main.create_view(&wgpu::TextureViewDescriptor::default());
        let scratch_view = scratch.create_view(&wgpu::TextureViewDescriptor::default());
        let scratch_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scratch bind group"),
            layout: &self.filter_src_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&scratch_view) }],
        });
        let padded_row = align_up(width * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: padded_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.targets = Some(Targets { width, height, main, main_view, scratch, scratch_bind_group, readback, padded_row });
    }

    /// メディアフレームを GPU テクスチャにしてキャッシュします。
    fn media_texture(&mut self, key: (String, u32), frame: &RgbaFrame) -> Option<Arc<TexEntry>> {
        if let Some(t) = self.textures.get(&key) {
            return Some(t.clone());
        }
        if frame.width == 0 || frame.height == 0 || frame.pixels.len() != (frame.width * frame.height * 4) as usize {
            return None;
        }
        if self.textures.len() >= TEXTURE_CACHE_LIMIT {
            self.textures.clear();
        }
        let entry = Arc::new(Self::make_texture_entry(
            &self.gpu,
            &self.texture_layout,
            &self.sampler,
            frame.width,
            frame.height,
            &frame.pixels,
        ));
        self.textures.insert(key, entry.clone());
        Some(entry)
    }

    /// `frame` 番目のフレームを描画して RGBA で返します（読み戻しまで同期）。
    pub fn render(
        &mut self,
        scene: &Scene,
        frame: Frame,
        media: &mut dyn MediaSource,
    ) -> Result<RgbaFrame, RenderError> {
        let width = scene.settings.width.max(1);
        let height = scene.settings.height.max(1);
        self.ensure_targets(width, height);

        let limits = self.gpu.device.limits();
        let sprite_stride = align_up(std::mem::size_of::<SpriteUniform>() as u32, limits.min_uniform_buffer_offset_alignment);
        let filter_stride =
            align_up(std::mem::size_of::<ColorCorrectionUniform>() as u32, limits.min_uniform_buffer_offset_alignment);

        let mut sprite_bytes: Vec<u8> = Vec::new();
        let mut filter_bytes: Vec<u8> = Vec::new();
        let mut ops: Vec<Op> = Vec::new();
        let canvas = [width as f32, height as f32];
        let fps = scene.settings.frame_rate.as_f64();

        for (li, obj) in scene.objects_at(frame) {
            if !scene.layers[li].visible {
                continue;
            }
            let Some(src) = obj.source().filter(|s| s.enabled) else { continue };
            let rel = frame - obj.frame_start;
            match obj.category() {
                ObjectCategory::Media => {
                    let params = eval_draw_params(obj, rel, fps);
                    if params.alpha <= 0.0 {
                        continue;
                    }
                    if let Some((uniform, tex)) = self.sprite_for(obj, rel, fps, canvas, params, media) {
                        let offset = sprite_bytes.len() as u32;
                        sprite_bytes.extend_from_slice(bytemuck::bytes_of(&uniform));
                        sprite_bytes.resize((offset + sprite_stride) as usize, 0);
                        ops.push(Op::Sprite { offset, tex });
                    }
                }
                ObjectCategory::Filter if src.name == "色調補正" => {
                    let ev = |keys: &[&str], default: f64| {
                        keys.iter().find_map(|k| src.track(k)).map(|t| t.evaluate(rel)).unwrap_or(default)
                    };
                    let uniform = ColorCorrectionUniform {
                        a: [
                            (ev(&["明るさ"], 100.0) / 100.0) as f32,
                            (ev(&["ｺﾝﾄﾗｽﾄ", "コントラスト"], 100.0) / 100.0) as f32,
                            (ev(&["色相"], 0.0) as f32).to_radians(),
                            (ev(&["輝度"], 100.0) / 100.0) as f32,
                        ],
                        b: [(ev(&["彩度"], 100.0) / 100.0) as f32, 0.0, 0.0, 0.0],
                    };
                    let offset = filter_bytes.len() as u32;
                    filter_bytes.extend_from_slice(bytemuck::bytes_of(&uniform));
                    filter_bytes.resize((offset + filter_stride) as usize, 0);
                    ops.push(Op::ColorCorrection { offset });
                }
                _ => {}
            }
        }

        // 空でも有効なバッファ／バインドグループを作れるよう最低 1 要素分を確保
        sprite_bytes.resize(sprite_bytes.len().max(sprite_stride as usize), 0);
        filter_bytes.resize(filter_bytes.len().max(filter_stride as usize), 0);
        let device = &self.gpu.device;
        let sprite_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sprite uniforms"),
            contents: &sprite_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let filter_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("filter uniforms"),
            contents: &filter_bytes,
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_uniform = |layout: &wgpu::BindGroupLayout, buffer: &wgpu::Buffer, size: usize| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("uniform bind group"),
                layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer,
                        offset: 0,
                        size: NonZeroU64::new(size as u64),
                    }),
                }],
            })
        };
        let sprite_bg = bind_uniform(&self.sprite_uniform_layout, &sprite_buffer, std::mem::size_of::<SpriteUniform>());
        let filter_bg =
            bind_uniform(&self.filter_uniform_layout, &filter_buffer, std::mem::size_of::<ColorCorrectionUniform>());

        let targets = self.targets.as_ref().expect("ensure_targets で確保済み");
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("render frame") });
        let color_attachment = |load: wgpu::LoadOp<wgpu::Color>| wgpu::RenderPassColorAttachment {
            view: &targets.main_view,
            resolve_target: None,
            ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
        };

        // 背景は不透明な黒
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(color_attachment(wgpu::LoadOp::Clear(wgpu::Color::BLACK)))],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        }));

        let mut i = 0;
        while i < ops.len() {
            match &ops[i] {
                Op::Sprite { .. } => {
                    // 連続するスプライトは 1 パスにまとめる
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("sprites"),
                        color_attachments: &[Some(color_attachment(wgpu::LoadOp::Load))],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    pass.set_pipeline(&self.sprite_pipeline);
                    while let Some(Op::Sprite { offset, tex }) = ops.get(i) {
                        let bind_group = &tex.as_ref().unwrap_or(&self.dummy_texture).bind_group;
                        pass.set_bind_group(0, &sprite_bg, &[*offset]);
                        pass.set_bind_group(1, bind_group, &[]);
                        pass.draw(0..4, 0..1);
                        i += 1;
                    }
                }
                Op::ColorCorrection { offset } => {
                    // 現在の合成結果を scratch にコピーし、それを読みながら main へ書き戻す
                    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
                    encoder.copy_texture_to_texture(
                        wgpu::ImageCopyTexture {
                            texture: &targets.main,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::ImageCopyTexture {
                            texture: &targets.scratch,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        size,
                    );
                    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("color correction"),
                        color_attachments: &[Some(color_attachment(wgpu::LoadOp::Load))],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    pass.set_pipeline(&self.filter_pipeline);
                    pass.set_bind_group(0, &filter_bg, &[*offset]);
                    pass.set_bind_group(1, &targets.scratch_bind_group, &[]);
                    pass.draw(0..3, 0..1);
                    i += 1;
                }
            }
        }

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &targets.main,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &targets.readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(targets.padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.gpu.queue.submit([encoder.finish()]);

        let slice = targets.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        {
            let data = slice.get_mapped_range();
            for row in data.chunks(targets.padded_row as usize).take(height as usize) {
                pixels.extend_from_slice(&row[..(width * 4) as usize]);
            }
        }
        targets.readback.unmap();
        Ok(RgbaFrame { width, height, pixels })
    }

    /// メディアオブジェクト 1 つ分のスプライトを組み立てます。描画対象外なら `None`。
    fn sprite_for(
        &mut self,
        obj: &TimelineObject,
        rel: Frame,
        fps: f64,
        canvas: [f32; 2],
        p: DrawParams,
        media: &mut dyn MediaSource,
    ) -> Option<(SpriteUniform, Option<Arc<TexEntry>>)> {
        let src = obj.source()?;
        let base = SpriteUniform {
            canvas,
            center: [p.x, p.y],
            half: [0.0, 0.0],
            angle: p.angle,
            alpha: p.alpha,
            color: [1.0, 1.0, 1.0, 1.0],
            mode: 1,
            shape: SHAPE_BOX,
            sides: 0,
            _pad: 0,
        };
        match src.kind {
            EffectKind::Shape => {
                let size = src.eval("サイズ", rel, 100.0) as f32 * p.zoom;
                let [r, g, b] = src.text("color").and_then(parse_hex_color).unwrap_or([1.0, 1.0, 1.0]);
                let ty: u32 = src.text("type").and_then(|t| t.trim().parse().ok()).unwrap_or(1);
                let mut u = SpriteUniform { color: [r, g, b, 1.0], half: [size / 2.0, size / 2.0], ..base };
                match ty {
                    // 背景: 画面全体を塗る（移動・回転は無視）
                    0 => {
                        u.center = [0.0, 0.0];
                        u.angle = 0.0;
                        u.half = [canvas[0] / 2.0, canvas[1] / 2.0];
                    }
                    1 => u.shape = SHAPE_CIRCLE,
                    2 => u.shape = SHAPE_BOX,
                    6 => u.shape = SHAPE_STAR,
                    n => {
                        u.shape = SHAPE_POLYGON;
                        u.sides = n.clamp(3, 12);
                    }
                }
                Some((u, None))
            }
            EffectKind::ImageFile | EffectKind::VideoFile => {
                let file = src.text("file").filter(|f| !f.trim().is_empty());
                let tex = file.and_then(|file| {
                    let start = (src.eval("再生位置", rel, 1.0) - 1.0).max(0.0);
                    let speed = (src.eval("再生速度", rel, 100.0) / 100.0).max(0.0);
                    let media_frame = (start + rel as f64 * speed).floor() as u32;
                    let sec = media_frame as f64 / fps.max(1.0);
                    let frame = media.frame(file, media_frame, sec)?;
                    let key_frame = if src.kind == EffectKind::ImageFile { 0 } else { media_frame };
                    let entry = self.media_texture((file.to_string(), key_frame), frame)?;
                    Some((entry, frame.width, frame.height))
                });
                match tex {
                    Some((entry, w, h)) => Some((
                        SpriteUniform { mode: 0, half: [w as f32 * p.zoom / 2.0, h as f32 * p.zoom / 2.0], ..base },
                        Some(entry),
                    )),
                    // ファイル未指定・読み込み失敗: プレビューと同じプレースホルダ
                    None => Some((
                        SpriteUniform {
                            color: [PLACEHOLDER_COLOR[0], PLACEHOLDER_COLOR[1], PLACEHOLDER_COLOR[2], 1.0],
                            half: [canvas[0] * p.zoom / 2.0, canvas[1] * p.zoom / 2.0],
                            ..base
                        },
                        None,
                    )),
                }
            }
            EffectKind::Text => {
                let size = (src.eval("サイズ", rel, 34.0) as f32 * p.zoom).max(1.0);
                let color_hex = src.text("color").unwrap_or("ffffff");
                let [cr, cg, cb] = parse_hex_color(color_hex).unwrap_or([1.0, 1.0, 1.0]);
                let body = src.text("text").unwrap_or_default().replace("\r\n", "\n");
                let key = (format!("{TEXT_KEY_PREFIX}{size:.1}\0{color_hex}\0{body}"), 0);
                let entry = match self.textures.get(&key) {
                    Some(t) => t.clone(),
                    None => {
                        let rgb = [(cr * 255.0).round() as u8, (cg * 255.0).round() as u8, (cb * 255.0).round() as u8];
                        let bitmap = crate::text::rasterize(self.font.as_ref()?, &body, size, rgb)?;
                        self.media_texture(key, &bitmap)?
                    }
                };
                let (w, h) = entry.size;
                // 画像は拡大率をラスタライズ時に反映済みなので、スプライトは等倍で置く
                Some((SpriteUniform { mode: 0, half: [w as f32 / 2.0, h as f32 / 2.0], ..base }, Some(entry)))
            }
            // それ以外の種類は未対応（docs/RENDER_DESIGN.md 参照）
            _ => None,
        }
    }
}
