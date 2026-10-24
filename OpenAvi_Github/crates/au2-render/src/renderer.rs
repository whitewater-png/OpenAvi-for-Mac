//! 合成エンジン本体。

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use au2_core::{EffectKind, Frame, ObjectCategory, ObjectId, Scene, TimelineObject};
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
/// 動画フレームのテクスチャを残す枚数。
const VIDEO_TEXTURE_KEEP: usize = 16;

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
    /// 拡張描画・3D・カメラ用の予約領域（頂点シェーダが使う。段階 B の「拡張描画/カメラ」担当が所有）
    ext: [f32; 16],
    /// 合成モード・クリッピング用の予約領域（フラグメントシェーダが使う。段階 B の「合成/クリッピング」担当が所有）
    blend: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ColorCorrectionUniform {
    /// 明るさ・コントラスト（倍率）、色相（ラジアン）、輝度（倍率）
    a: [f32; 4],
    /// 彩度（倍率）
    b: [f32; 4],
}

/// 直近の描画での、オブジェクトの描画範囲（回転した矩形）。選択枠などの UI 用。
/// 座標はシーン中心基準のピクセル（+y 下）、`half` は回転前の半サイズ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectBounds {
    pub center: [f32; 2],
    pub half: [f32; 2],
    /// ラジアン（正 = 時計回り）
    pub angle: f32,
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
    /// `view_formats` が使えない環境（GL など）だけ作る、sRGB フォーマットのコピー先。
    /// 対応環境では `main` の sRGB ビューをそのまま表示に使うので `None`（Metal ではフォーマット違いの
    /// テクスチャ間コピーが未定義動作になるため、この経路は使わない）。
    display: Option<wgpu::Texture>,
    display_view: wgpu::TextureView,
    /// フィルタパスが読む用のコピー先（ping-pong 相手）
    scratch: wgpu::Texture,
    scratch_bind_group: wgpu::BindGroup,
    /// 読み戻し用バッファ。プレビューは読み戻さないので、最初の `render()` まで作らない
    readback: Option<wgpu::Buffer>,
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
    bounds: HashMap<ObjectId, ObjectBounds>,
    generation: u64,
    targets: Option<Targets>,
    textures: HashMap<(String, u32), Arc<TexEntry>>,
    /// 動画フレームのキャッシュキー（古い順）
    video_keys: std::collections::VecDeque<(String, u32)>,
    /// テキスト画像ごとの位置合わせ点（揃え）。`textures` のテキストキーと対応する
    text_anchors: HashMap<(String, u32), [f32; 2]>,
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
            source: wgpu::ShaderSource::Wgsl(
                concat!(
                    include_str!("shaders/sprite_common.wgsl"),
                    include_str!("shaders/sprite_vertex.wgsl"),
                    include_str!("shaders/sprite_fragment.wgsl")
                )
                .into(),
            ),
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
            bounds: HashMap::new(),
            generation: 0,
            targets: None,
            textures: HashMap::new(),
            video_keys: std::collections::VecDeque::new(),
            text_anchors: HashMap::new(),
        }
    }

    /// テキスト描画に使うフォント（`.ttf` / `.otf` / `.ttc`）を設定します。
    pub fn set_font(&mut self, bytes: Vec<u8>, index: u32) -> bool {
        self.textures.retain(|(k, _), _| !k.starts_with(TEXT_KEY_PREFIX));
        self.text_anchors.clear();
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

    /// 直近の描画結果を egui などに表示するためのビュー（sRGB 解釈）。まだ描いていなければ `None`。
    pub fn framebuffer_view(&self) -> Option<&wgpu::TextureView> {
        self.targets.as_ref().map(|t| &t.display_view)
    }

    /// フレームバッファを作り直すたびに増える番号。表示側がテクスチャを再登録する目安。
    pub fn framebuffer_generation(&self) -> u64 {
        self.generation
    }

    /// 直近の描画でのオブジェクトの描画範囲。描かれなかったオブジェクトは `None`。
    pub fn object_bounds(&self, id: ObjectId) -> Option<ObjectBounds> {
        self.bounds.get(&id).copied()
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

    fn ensure_targets(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        let max = self.gpu.device.limits().max_texture_dimension_2d;
        if width > max || height > max {
            return Err(RenderError::SceneTooLarge { width, height, max });
        }
        if self.targets.as_ref().is_some_and(|t| t.width == width && t.height == height) {
            return Ok(());
        }
        let device = &self.gpu.device;
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let srgb = [FORMAT.add_srgb_suffix()];
        let make = |label: &str, format: wgpu::TextureFormat, usage: wgpu::TextureUsages, view_formats: &[wgpu::TextureFormat]| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats,
            })
        };
        let main = make(
            "framebuffer",
            FORMAT,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING,
            if self.gpu.view_formats { &srgb } else { &[] },
        );
        let scratch = make(
            "framebuffer scratch",
            FORMAT,
            wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            &[],
        );
        // 表示用の sRGB 解釈:
        //  - 対応環境: 同じテクスチャの sRGB ビュー（コピー不要）
        //  - 未対応環境（GL）: sRGB フォーマットの別テクスチャへコピーする。
        //    Metal ではフォーマット違いのテクスチャ間コピーが一時ビューの解放済み参照を生むため、使わない。
        let display = (!self.gpu.view_formats).then(|| {
            make(
                "framebuffer display",
                FORMAT.add_srgb_suffix(),
                wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                &[],
            )
        });
        let main_view = main.create_view(&wgpu::TextureViewDescriptor::default());
        let display_view = match &display {
            Some(texture) => texture.create_view(&wgpu::TextureViewDescriptor::default()),
            None => main.create_view(&wgpu::TextureViewDescriptor {
                format: Some(FORMAT.add_srgb_suffix()),
                ..Default::default()
            }),
        };
        let scratch_view = scratch.create_view(&wgpu::TextureViewDescriptor::default());
        let scratch_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scratch bind group"),
            layout: &self.filter_src_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&scratch_view) }],
        });
        let padded_row = align_up(width * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        self.generation += 1;
        self.targets = Some(Targets { width, height, main, main_view, display, display_view, scratch, scratch_bind_group, readback: None, padded_row });
        Ok(())
    }

    /// メディアフレームを GPU テクスチャにしてキャッシュします。
    /// `is_video` のフレームは古いものから 16 枚だけ残す（再生中に静止画・テキストのキャッシュを巻き込んで消さないため）。
    /// GPU の最大テクスチャサイズを超える画像は縮小してアップロードし、`size` には元のサイズを残す。
    fn media_texture(&mut self, key: (String, u32), frame: &RgbaFrame, is_video: bool) -> Option<Arc<TexEntry>> {
        if let Some(t) = self.textures.get(&key) {
            return Some(t.clone());
        }
        if frame.width == 0 || frame.height == 0 || frame.pixels.len() != (frame.width as usize) * (frame.height as usize) * 4 {
            return None;
        }
        if self.textures.len() >= TEXTURE_CACHE_LIMIT {
            self.textures.clear();
            self.video_keys.clear();
        }
        let max = self.gpu.device.limits().max_texture_dimension_2d;
        let fitted = frame.fit_within(max);
        let (uploaded, original) = match &fitted {
            Some(small) => (small, (frame.width, frame.height)),
            None => (frame, (frame.width, frame.height)),
        };
        let mut entry =
            Self::make_texture_entry(&self.gpu, &self.texture_layout, &self.sampler, uploaded.width, uploaded.height, &uploaded.pixels);
        entry.size = original;
        let entry = Arc::new(entry);
        if is_video {
            self.video_keys.push_back(key.clone());
            while self.video_keys.len() > VIDEO_TEXTURE_KEEP {
                if let Some(old) = self.video_keys.pop_front() {
                    self.textures.remove(&old);
                }
            }
        }
        self.textures.insert(key, entry.clone());
        Some(entry)
    }

    /// `frame` 番目のフレームを描画し、結果を [`Renderer::framebuffer_view`] のテクスチャに残します（読み戻しなし）。
    /// プレビューのように、画面へ直接表示する用途向けです。
    pub fn render_to_framebuffer(
        &mut self,
        scene: &Scene,
        frame: Frame,
        media: &mut dyn MediaSource,
    ) -> Result<(), RenderError> {
        let width = scene.settings.width.max(1);
        let height = scene.settings.height.max(1);
        self.ensure_targets(width, height)?;

        let limits = self.gpu.device.limits();
        let sprite_stride = align_up(std::mem::size_of::<SpriteUniform>() as u32, limits.min_uniform_buffer_offset_alignment);
        let filter_stride =
            align_up(std::mem::size_of::<ColorCorrectionUniform>() as u32, limits.min_uniform_buffer_offset_alignment);

        let mut sprite_bytes: Vec<u8> = Vec::new();
        let mut filter_bytes: Vec<u8> = Vec::new();
        let mut ops: Vec<Op> = Vec::new();
        let canvas = [width as f32, height as f32];
        let fps = scene.settings.frame_rate.as_f64();
        self.bounds.clear();

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
                        self.bounds.insert(
                            obj.id,
                            ObjectBounds { center: uniform.center, half: uniform.half, angle: uniform.angle },
                        );
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

        if let Some(display) = &targets.display {
            encoder.copy_texture_to_texture(
                wgpu::ImageCopyTexture {
                    texture: &targets.main,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyTexture {
                    texture: display,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        self.gpu.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// `frame` 番目のフレームを描画して RGBA で返します（読み戻しまで同期）。書き出し・テスト用。
    pub fn render(
        &mut self,
        scene: &Scene,
        frame: Frame,
        media: &mut dyn MediaSource,
    ) -> Result<RgbaFrame, RenderError> {
        self.render_to_framebuffer(scene, frame, media)?;
        let device = &self.gpu.device;
        {
            let targets = self.targets.as_mut().expect("render_to_framebuffer で確保済み");
            let size = targets.padded_row as u64 * targets.height as u64;
            targets.readback.get_or_insert_with(|| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("readback"),
                    size,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                })
            });
        }
        let targets = self.targets.as_ref().expect("確保済み");
        let readback = targets.readback.as_ref().expect("直前に作成済み");
        let (width, height) = (targets.width, targets.height);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &targets.main,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(targets.padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.gpu.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|e| RenderError::Readback(e.to_string()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        {
            let data = slice.get_mapped_range();
            for row in data.chunks(targets.padded_row as usize).take(height as usize) {
                pixels.extend_from_slice(&row[..width as usize * 4]);
            }
        }
        readback.unmap();
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
            ext: [0.0; 16],
            blend: [0; 4],
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
                    let is_video = src.kind == EffectKind::VideoFile;
                    // 「ループ再生」: 動画の長さを超えた位置は先頭へ巻き戻す（長さは最初のフレーム取得後に分かる）
                    let looping = is_video && src.eval("ループ再生", rel, 0.0) >= 0.5;
                    let mut sec = sec;
                    if looping {
                        // 長さは供給元が動画を開いた後に分かる。分かっていれば、巻き戻し前の時刻では何も要求しない
                        // （終端の先を要求すると、キャッシュが空のとき終端へシークして無駄に ffmpeg を起動してしまう）
                        let mut duration = media.duration_secs(file);
                        if duration.is_none() {
                            // まず動画を開かせて（フレームは要求しない）、長さが分かるか見る。
                            // MKV/WebM のように分からない場合だけ、実際の位置を要求して終端から学習させる
                            media.prepare(file);
                            duration = media.duration_secs(file);
                            if duration.is_none() {
                                let _ = media.frame(file, media_frame, sec);
                                duration = media.duration_secs(file);
                            }
                        }
                        if let Some(duration) = duration.filter(|d| *d > 0.0) {
                            sec = sec.rem_euclid(duration);
                        }
                    }
                    let frame = media.frame(file, media_frame, sec)?;
                    // 動画はプロジェクトのフレーム番号ではなく時刻（ミリ秒）で区別する。
                    // フレームレートを変えても、同じ番号が別の時刻を指して古いテクスチャを返すことがない
                    let key_frame = if is_video { (sec * 1000.0).round() as u32 } else { 0 };
                    let entry = self.media_texture((file.to_string(), key_frame), frame, is_video)?;
                    // デコード解像度が縮小されていても、描画上の大きさは動画本来のサイズで決める
                    let (w, h) = media.native_size(file).unwrap_or(entry.size);
                    Some((entry, w, h))
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
                use crate::text::{Decoration, TextStyle};
                let size = (src.eval("サイズ", rel, 34.0) as f32 * p.zoom).max(1.0);
                let color_hex = src.text("color").unwrap_or("ffffff");
                let color2_hex = src.text("color2").unwrap_or("000000");
                let to_u8 = |c: [f32; 3]| [(c[0] * 255.0).round() as u8, (c[1] * 255.0).round() as u8, (c[2] * 255.0).round() as u8];
                let rgb = to_u8(parse_hex_color(color_hex).unwrap_or([1.0, 1.0, 1.0]));
                let rgb2 = to_u8(parse_hex_color(color2_hex).unwrap_or([0.0, 0.0, 0.0]));
                let body = src.text("text").unwrap_or_default().replace("\r\n", "\n");
                let int = |k: &str| src.text(k).and_then(|t| t.trim().parse::<u32>().ok()).unwrap_or(0);
                let font_name = src.text("font").unwrap_or("").trim().to_string();
                // 表示速度（文字/秒）: 0 = すべて表示。表示する文字数は全文字数で頭打ち（キャッシュキーが増え続けないように）
                let total = crate::text::char_count(&body);
                let visible = crate::text::visible_chars_at(src.eval("表示速度", rel, 0.0), rel, fps).map(|n| n.min(total));
                let style = TextStyle {
                    color2: rgb2,
                    bold: src.eval("B", rel, 0.0) >= 0.5,
                    italic: src.eval("I", rel, 0.0) >= 0.5,
                    decoration: Decoration::from_type(int("type")),
                    spacing_x: src.text("spacing_x").and_then(|t| t.trim().parse::<f32>().ok()).unwrap_or(0.0) * p.zoom,
                    spacing_y: src.text("spacing_y").and_then(|t| t.trim().parse::<f32>().ok()).unwrap_or(0.0) * p.zoom,
                    monospace: int("monospace") != 0,
                    align: int("align").min(8),
                    visible_chars: visible,
                    ..TextStyle::new(size, rgb)
                };
                // 画像の内容が変わるものはすべてキーに含める（フォント名も: 同じ名前は同じフォントに解決される）
                let key = (
                    format!(
                        "{TEXT_KEY_PREFIX}{:.1}\0{color_hex}\0{color2_hex}\0{}{}{}{}{}\0{:.1}\0{:.1}\0{}\0{font_name}\0{body}",
                        style.px,
                        style.bold as u8,
                        style.italic as u8,
                        style.monospace as u8,
                        int("type"),
                        style.align,
                        style.spacing_x,
                        style.spacing_y,
                        visible.map_or(-1, |n| n as i64),
                    ),
                    0,
                );
                // 位置合わせ点はキャッシュ済みの画像から求め直せないので、画像と一緒に覚えておく
                let (entry, anchor) = match (self.textures.get(&key), self.text_anchors.get(&key)) {
                    (Some(t), Some(a)) => (t.clone(), *a),
                    _ => {
                        let named = if font_name.is_empty() { None } else { crate::text::resolve_font(&font_name) };
                        let font: &ab_glyph::FontVec = match named.as_deref() {
                            Some(f) => f,
                            None => self.font.as_ref()?,
                        };
                        let bitmap = crate::text::rasterize(font, &body, &style)?;
                        let entry = self.media_texture(key.clone(), &bitmap.frame, false)?;
                        if self.text_anchors.len() >= 256 {
                            self.text_anchors.clear();
                        }
                        self.text_anchors.insert(key, bitmap.anchor);
                        (entry, bitmap.anchor)
                    }
                };
                let (w, h) = entry.size;
                // 画像は拡大率をラスタライズ時に反映済みなので、スプライトは等倍で置く。
                // 揃えの位置合わせ点がオブジェクトの位置に来るよう、回転に合わせて中心をずらす
                let (s, c) = p.angle.sin_cos();
                let center = [p.x - (anchor[0] * c - anchor[1] * s), p.y - (anchor[0] * s + anchor[1] * c)];
                Some((SpriteUniform { mode: 0, center, half: [w as f32 / 2.0, h as f32 / 2.0], ..base }, Some(entry)))
            }
            // それ以外の種類は未対応（docs/RENDER_DESIGN.md 参照）
            _ => None,
        }
    }
}
