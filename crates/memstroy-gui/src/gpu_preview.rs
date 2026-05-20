//! GPU-accelerated preview compositor using wgpu.
//!
//! Renders the scene at a given time `t` into an RGBA pixel buffer
//! that can be uploaded to an egui texture for live scrubbing.
//!
//! Architecture:
//! - `PreviewCompositor` owns the wgpu device + queue (headless)
//! - `render_frame(scene, t)` → `Vec<u8>` (RGBA pixels)
//! - Layers are composited via alpha blending on a render target
//! - Chroma key is implemented as a fragment shader
//! - Text rendering uses pre-rasterized glyphs (placeholder boxes for now)
//!
//! This avoids the FFmpeg round-trip for preview, enabling real-time
//! scrubbing at 30+ fps even on integrated GPUs.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use memstroy_core::*;
use tracing::{info, warn};

/// Output of a single preview frame render.
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA8 pixel data, row-major, top-to-bottom.
    pub pixels: Vec<u8>,
}

/// GPU-backed scene compositor for live preview.
pub struct PreviewCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Chroma key shader pipeline.
    chroma_pipeline: wgpu::RenderPipeline,
    /// Simple blit/composite pipeline (alpha blending).
    blit_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for texture + sampler.
    tex_bind_group_layout: wgpu::BindGroupLayout,
    /// Output resolution.
    width: u32,
    height: u32,
}

impl PreviewCompositor {
    /// Create a new compositor. This initialises a headless wgpu device.
    /// Returns `Err` if no suitable GPU adapter is found.
    pub async fn new(width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| anyhow!("No suitable GPU adapter found"))?;

        info!(adapter = ?adapter.get_info().name, "GPU adapter");

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("memstroy-preview"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await
            .context("request wgpu device")?;

        // Create shader modules
        let shader_src = include_str!("shaders/preview.wgsl");
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("preview_shader"),
            source: wgpu::ShaderSource::Wgsl(shader_src.into()),
        });

        // Bind group layout: texture + sampler
        let tex_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tex_bgl"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("preview_pl"),
            bind_group_layouts: &[&tex_bind_group_layout],
            push_constant_ranges: &[],
        });

        let target_format = wgpu::TextureFormat::Rgba8Unorm;

        // Blit pipeline (simple textured quad with alpha)
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_blit",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Chroma key pipeline
        let chroma_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("chroma"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_chromakey",
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            device,
            queue,
            chroma_pipeline,
            blit_pipeline,
            tex_bind_group_layout,
            width,
            height,
        })
    }

    /// Render one frame of the scene at time `t`.
    /// Returns RGBA8 pixel buffer of size width * height * 4.
    pub fn render_frame(&self, scene: &Scene, t: f32, assets_root: &Path) -> Result<PreviewFrame> {
        let w = self.width;
        let h = self.height;

        // Create render target texture
        let target_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview_target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target_tex.create_view(&Default::default());

        // Clear to background color
        let [r, g, b] = scene.output.background_color;
        let clear_color = wgpu::Color {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
            a: 1.0,
        };

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("preview_enc"),
        });

        // Clear pass
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
        }

        // TODO: For each background active at time t, render it
        // TODO: For each actor active at time t, decode frame + chroma key + composite
        // TODO: For each overlay active at time t, render text/image

        // Read back pixels
        let bytes_per_row = (w * 4 + 255) & !255; // align to 256
        let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bytes_per_row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &output_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        self.queue.submit(std::iter::once(encoder.finish()));

        // Map buffer and read pixels
        let buffer_slice = output_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .map_err(|_| anyhow!("buffer map channel closed"))?
            .map_err(|e| anyhow!("buffer map failed: {:?}", e))?;

        let mapped = buffer_slice.get_mapped_range();

        // Copy to output, removing padding
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            let src_offset = (row * bytes_per_row) as usize;
            let dst_offset = (row * w * 4) as usize;
            let row_bytes = (w * 4) as usize;
            pixels[dst_offset..dst_offset + row_bytes]
                .copy_from_slice(&mapped[src_offset..src_offset + row_bytes]);
        }

        drop(mapped);
        output_buffer.unmap();

        Ok(PreviewFrame { width: w, height: h, pixels })
    }

    /// Upload a `PreviewFrame` to an egui `ColorImage`.
    pub fn frame_to_color_image(frame: &PreviewFrame) -> egui::ColorImage {
        let pixels: Vec<egui::Color32> = frame
            .pixels
            .chunks_exact(4)
            .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
            .collect();
        egui::ColorImage {
            size: [frame.width as usize, frame.height as usize],
            pixels,
        }
    }

    /// Resize the output.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }
}
