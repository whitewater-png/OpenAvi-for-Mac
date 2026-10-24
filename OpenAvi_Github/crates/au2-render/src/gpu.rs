//! wgpu のデバイス管理。ウィンドウ無し（ヘッドレス）でも、eframe の既存デバイスを借りても使えます。

use std::sync::Arc;

use crate::RenderError;

/// GPU デバイス一式。
#[derive(Clone)]
pub struct Gpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub adapter_info: wgpu::AdapterInfo,
    /// テクスチャに別フォーマット（sRGB 違い）のビューを作れるか。GL バックエンドなどでは不可。
    pub view_formats: bool,
}

impl Gpu {
    /// ウィンドウ無しでアダプタとデバイスを取得します（書き出し・テスト用）。
    /// 高性能 GPU を優先し、無ければソフトウェアレンダラ（フォールバック）を試します。
    pub fn headless() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = [false, true]
            .into_iter()
            .find_map(|fallback| {
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: fallback,
                    compatible_surface: None,
                }))
            })
            .ok_or(RenderError::NoAdapter)?;
        let adapter_info = adapter.get_info();
        let view_formats = supports_view_formats(&adapter);
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("au2-render"),
                required_features: wgpu::Features::empty(),
                // アダプタの上限をそのまま使う（既定の 8192 だと、対応 GPU でも 8K 超のシーンを書き出せない）
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
            },
            None,
        ))
        .map_err(RenderError::RequestDevice)?;
        Ok(Self { device: Arc::new(device), queue: Arc::new(queue), adapter_info, view_formats })
    }

    /// 既存のデバイスを借ります（プレビューで eframe の `RenderState` を共有する用）。
    pub fn from_parts(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>, adapter: &wgpu::Adapter) -> Self {
        Self { device, queue, adapter_info: adapter.get_info(), view_formats: supports_view_formats(adapter) }
    }
}

fn supports_view_formats(adapter: &wgpu::Adapter) -> bool {
    adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS)
}
