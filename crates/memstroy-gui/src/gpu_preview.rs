//! GPU-accelerated preview compositor using wgpu.
#![allow(dead_code)]
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

use anyhow::{Context, Result, anyhow};
use memstroy_core::*;
use tracing::info;

/// Output of a single preview frame render.
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA8 pixel data, row-major, top-to-bottom.
    pub pixels: Vec<u8>,
}

/// Parameters for compositing a single actor layer.
pub struct ActorCompositeParams {
    /// Index of the actor in the scene.
    pub actor_idx: usize,
    /// Local time within the actor's source clip.
    pub local_time: f32,
    /// Chroma key parameters (if chroma keying is desired).
    pub chroma_key: Option<ChromaKeyParams>,
    /// Layout state (position, scale, rotation, opacity).
    pub layout: ActorState,
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

    /// Create a wgpu texture from RGBA pixel data.
    fn create_texture_from_rgba(&self, pixels: &[u8], img_w: u32, img_h: u32) -> wgpu::Texture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("actor_frame"),
            size: wgpu::Extent3d {
                width: img_w,
                height: img_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(img_w * 4),
                rows_per_image: Some(img_h),
            },
            wgpu::Extent3d {
                width: img_w,
                height: img_h,
                depth_or_array_layers: 1,
            },
        );

        texture
    }

    /// Create a bind group for a texture + sampler pair.
    fn create_tex_bind_group(&self, texture: &wgpu::Texture) -> wgpu::BindGroup {
        let view = texture.create_view(&Default::default());
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("actor_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("actor_bg"),
            layout: &self.tex_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        })
    }

    /// Load a JPEG frame from disk and return RGBA pixel data with dimensions.
    /// Uses the same pattern as FrameCache: frames are stored as JPEG files
    /// in a cache directory with naming like `frame_XXXXX.jpg`.
    fn load_frame_rgba(frame_path: &Path) -> Result<(Vec<u8>, u32, u32)> {
        let img = image::open(frame_path)
            .context("Failed to open frame image")?
            .to_rgba8();
        let (w, h) = img.dimensions();
        Ok((img.into_raw(), w, h))
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

        // For each actor active at time t, render it onto the target
        for actor in &scene.actors {
            if !actor.visible {
                continue;
            }
            let t_in = actor.t_in.unwrap_or(0.0);
            let t_out = actor.t_out.unwrap_or(scene.output.duration);
            if t < t_in || t > t_out {
                continue;
            }

            let local_t = t - t_in + actor.source_start;

            // Attempt to load the frame from the frame cache directory
            // Frame cache uses format: <cache_dir>/frame_XXXXX.jpg
            // The cache dir is typically at: <assets_root>/.frame_cache/<actor_idx>/
            // For robustness, we also try the source path with .frames/ suffix
            let cache_dir = assets_root
                .join(".frame_cache")
                .join(&actor.id);

            // Calculate frame index (assuming 30fps extraction)
            let frame_idx = (local_t * 30.0).round() as u32;
            let frame_path = cache_dir.join(format!("frame_{:05}.jpg", frame_idx));

            let (pixels, img_w, img_h) = match Self::load_frame_rgba(&frame_path) {
                Ok(data) => data,
                Err(_) => {
                    // Frame not available; skip this actor silently
                    continue;
                }
            };

            // Create a GPU texture from the frame pixel data
            let actor_texture = self.create_texture_from_rgba(&pixels, img_w, img_h);

            // Create bind group with texture + sampler
            let bind_group = self.create_tex_bind_group(&actor_texture);

            // Choose pipeline based on whether chroma key is active
            // (similarity > 0 indicates chroma key should be applied)
            let use_chroma = actor.chroma_key.similarity > 0.01;

            // TODO: push constants for transform
            // The actor's layout transform (position/scale/rotation) should be passed
            // as push constants or a uniform buffer to the vertex shader to properly
            // position the quad. Currently the fullscreen quad covers the entire
            // render target; a proper implementation would scale/translate the quad
            // based on actor.layout interpolated at time t.
            // For now, we render fullscreen and rely on the chroma key to composite.

            // Render pass for this actor layer (load existing target, blend on top)
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("actor_layer"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });

                if use_chroma {
                    pass.set_pipeline(&self.chroma_pipeline);
                } else {
                    pass.set_pipeline(&self.blit_pipeline);
                }

                pass.set_bind_group(0, &bind_group, &[]);
                // Draw fullscreen quad (4 vertices as triangle strip)
                pass.draw(0..4, 0..1);
            }
        }

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

    /// Composite multiple actors onto the render target in order.
    ///
    /// Each entry in `actors` specifies the actor index, local time,
    /// optional chroma key parameters, and layout state (position/scale).
    /// Actors are composited bottom-to-top (first in list = bottom layer).
    ///
    /// This method loads frames from the cache directory under `assets_root`,
    /// creates GPU textures, and renders each layer with alpha blending.
    pub fn composite_actors_to_texture(
        &self,
        actors: &[ActorCompositeParams],
        assets_root: &Path,
        scene: &Scene,
    ) -> Result<PreviewFrame> {
        let w = self.width;
        let h = self.height;

        // Create render target
        let target_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("composite_target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target_tex.create_view(&Default::default());

        let [r, g, b] = scene.output.background_color;
        let clear_color = wgpu::Color {
            r: r as f64 / 255.0,
            g: g as f64 / 255.0,
            b: b as f64 / 255.0,
            a: 1.0,
        };

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("composite_enc"),
        });

        // Clear pass
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite_clear"),
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

        // Composite each actor layer in order
        for params in actors {
            let actor_id = if params.actor_idx < scene.actors.len() {
                &scene.actors[params.actor_idx].id
            } else {
                continue;
            };

            let cache_dir = assets_root
                .join(".frame_cache")
                .join(actor_id);

            // Calculate frame index (assuming 30fps extraction)
            let frame_idx = (params.local_time * 30.0).round() as u32;
            let frame_path = cache_dir.join(format!("frame_{:05}.jpg", frame_idx));

            let (pixels, img_w, img_h) = match Self::load_frame_rgba(&frame_path) {
                Ok(data) => data,
                Err(_) => continue, // Skip unavailable frames
            };

            let actor_texture = self.create_texture_from_rgba(&pixels, img_w, img_h);
            let bind_group = self.create_tex_bind_group(&actor_texture);

            let use_chroma = params.chroma_key.is_some();

            // TODO: push constants for transform
            // params.layout contains position/scale/rotation that should be
            // passed as push constants to the vertex shader to transform the
            // quad appropriately. Without push constant support in the current
            // pipeline layout, actors render fullscreen and rely on alpha
            // blending for compositing.

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("composite_actor_layer"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    ..Default::default()
                });

                if use_chroma {
                    pass.set_pipeline(&self.chroma_pipeline);
                } else {
                    pass.set_pipeline(&self.blit_pipeline);
                }

                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
        }

        // Readback
        let bytes_per_row = (w * 4 + 255) & !255;
        let output_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("composite_readback"),
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
        let mut output_pixels = vec![0u8; (w * h * 4) as usize];
        for row in 0..h {
            let src_offset = (row * bytes_per_row) as usize;
            let dst_offset = (row * w * 4) as usize;
            let row_bytes = (w * 4) as usize;
            output_pixels[dst_offset..dst_offset + row_bytes]
                .copy_from_slice(&mapped[src_offset..src_offset + row_bytes]);
        }
        drop(mapped);
        output_buffer.unmap();

        Ok(PreviewFrame { width: w, height: h, pixels: output_pixels })
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
