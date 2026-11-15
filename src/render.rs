//! Offscreen wgpu renderer used by the GUI, `render`, `turntable`, and MCP.
//!
//! Skinning happens on the CPU in [`crate::scene::Scene::posed_mesh`] before
//! upload. The shader only shades, which keeps Metal, DX12, Vulkan, and
//! lavapipe/WARP on the same path.

use std::collections::HashSet;
use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use image::RgbaImage;

use crate::error::{MvError, Result};
use crate::scene::{Aabb, Mesh, Scene};

const CLEAR: wgpu::Color = wgpu::Color {
    r: 0.16,
    g: 0.17,
    b: 0.20,
    a: 1.0,
};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
    color: [f32; 4],
}

/// Position plus a packed normal. Used when the full 48-byte vertex would
/// overflow the device buffer limit. UVs and vertex colors are not needed
/// for an untextured shaded mesh.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct CompactVertex {
    pos: [f32; 3],
    normal: [i8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    view_proj: [[f32; 4]; 4],
    model: [[f32; 4]; 4],
    normal_mat: [[f32; 4]; 4],
    base_color: [f32; 4],
    misc: [f32; 4],
    extra: [f32; 4],
}

const UNIFORM_STRIDE: u64 = 256;

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
    pipeline: wgpu::RenderPipeline,
    pipeline_cull: wgpu::RenderPipeline,
    pipeline_blend: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,
    line_overlay: wgpu::RenderPipeline,
    compact: wgpu::RenderPipeline,
    compact_cull: wgpu::RenderPipeline,
    compact_blend: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    white: wgpu::Texture,
    /// Device max, and the budget uploads actually use (tests can lower it).
    max_buffer_size: u64,
    vertex_budget: u64,
}

#[derive(Clone, Debug)]
pub struct Shot {
    pub view: String,
    pub yaw: f32,
    pub pitch: f32,
    pub width: u32,
    pub height: u32,
    pub mode: String,
    pub animation: Option<String>,
    pub time: f32,
    pub adapter: String,
}

pub struct FrameRequest<'a> {
    pub scene: &'a Scene,
    pub width: u32,
    pub height: u32,
    pub yaw: f32,
    pub pitch: f32,
    pub mode: RenderMode,
    pub anim: Option<usize>,
    pub time: f32,
    pub uv_set: usize,
    pub bone: Option<usize>,
    pub show_skeleton: bool,
    pub show_grid: bool,
    /// When false, the grid and skeleton are drawn and the meshes are not.
    /// Used to tell a blank model from the background.
    pub show_surface: bool,
    /// 1.0 frames the bounds. Smaller moves the camera closer.
    pub distance_scale: f32,
    pub target_offset: [f32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    Shaded,
    Wireframe,
    Normals,
    UvChecker,
    Weights,
}

impl RenderMode {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text.to_ascii_lowercase().as_str() {
            "shaded" | "shade" => Self::Shaded,
            "wire" | "wireframe" => Self::Wireframe,
            "normal" | "normals" => Self::Normals,
            "uv" | "uv-checker" | "uvchecker" => Self::UvChecker,
            "weights" | "weight" | "skin" => Self::Weights,
            other => {
                return Err(MvError::invalid(format!(
                "unknown mode '{other}' (expected shaded, wireframe, normals, uv-checker, weights)"
            )))
            }
        })
    }

    fn shader_mode(self) -> f32 {
        match self {
            Self::Shaded => 0.0,
            Self::Wireframe => 1.0,
            Self::Normals => 2.0,
            Self::UvChecker => 3.0,
            Self::Weights => 4.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shaded => "shaded",
            Self::Wireframe => "wireframe",
            Self::Normals => "normals",
            Self::UvChecker => "uv-checker",
            Self::Weights => "weights",
        }
    }
}

pub fn view_angles(view: &str) -> Result<(f32, f32)> {
    Ok(match view.to_ascii_lowercase().replace('_', "-").as_str() {
        "front" => (0.0, 0.0),
        "back" => (180.0, 0.0),
        "left" => (-90.0, 0.0),
        "right" => (90.0, 0.0),
        "top" => (0.0, 89.0),
        "three-quarter" | "threequarter" | "3q" => (35.0, 25.0),
        other => {
            return Err(MvError::invalid(format!(
                "unknown view '{other}' (expected front, back, left, right, top, three-quarter)"
            )))
        }
    })
}

pub fn frame_distance(bounds: Aabb) -> f32 {
    let fov = 40f32.to_radians();
    bounds.radius().max(1e-9) / (fov * 0.5).tan() * 1.4
}

/// Camera distance used to frame `bounds`. Scales from sub-millimetre parts
/// up to kilometres; it is not clamped to a fixed 5 cm.
pub fn view_distance(bounds: Aabb, distance_scale: f32) -> f32 {
    let radius = bounds.radius().max(1e-9);
    (frame_distance(bounds) * distance_scale.max(0.05)).max(radius * 1.05)
}

impl Renderer {
    pub fn new() -> Result<Self> {
        let software = std::env::var("MODELVIEW_SOFTWARE")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        if software {
            return Self::with_fallback(true);
        }
        match Self::with_fallback(false) {
            Ok(renderer) => Ok(renderer),
            Err(first) => Self::with_fallback(true)
                .map_err(|second| MvError::render(format!("{first}; software adapter: {second}"))),
        }
    }

    fn with_fallback(software: bool) -> Result<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: software,
            apply_limit_buckets: false,
        }))
        .map_err(|err| MvError::render(format!("no GPU adapter: {err}")))?;
        let info = adapter.get_info();
        let adapter_name = format!("{:?} {} ({:?})", info.backend, info.name, info.device_type);
        // Ask for the adapter's real limits. The defaults cap a buffer at
        // 256 MiB, which a 20-million-vertex mesh exceeds even on a 5090.
        let limits = adapter.limits();
        let max_buffer_size = limits.max_buffer_size;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("modelview"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|err| MvError::render(format!("device request failed: {err}")))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("modelview"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("frame"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: NonZeroU64::new(std::mem::size_of::<Uniforms>() as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("frame"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 24,
                    shader_location: 2,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 32,
                    shader_location: 3,
                },
            ],
        };
        let pipeline = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            vertex_layout.clone(),
            wgpu::PrimitiveTopology::TriangleList,
            None,
            wgpu::DepthBiasState::default(),
            Some(true),
            Some(wgpu::CompareFunction::Less),
            false,
            "vs_main",
        );
        let pipeline_cull = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            vertex_layout.clone(),
            wgpu::PrimitiveTopology::TriangleList,
            Some(wgpu::Face::Back),
            wgpu::DepthBiasState::default(),
            Some(true),
            Some(wgpu::CompareFunction::Less),
            false,
            "vs_main",
        );
        let pipeline_blend = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            vertex_layout.clone(),
            wgpu::PrimitiveTopology::TriangleList,
            None,
            wgpu::DepthBiasState::default(),
            Some(false),
            Some(wgpu::CompareFunction::Less),
            true,
            "vs_main",
        );
        let line_pipeline = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            vertex_layout.clone(),
            wgpu::PrimitiveTopology::LineList,
            None,
            wgpu::DepthBiasState::default(),
            Some(true),
            Some(wgpu::CompareFunction::Less),
            false,
            "vs_main",
        );
        let line_overlay = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            vertex_layout,
            wgpu::PrimitiveTopology::LineList,
            None,
            wgpu::DepthBiasState::default(),
            Some(false),
            Some(wgpu::CompareFunction::Always),
            false,
            "vs_main",
        );
        let compact_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<CompactVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Snorm8x4,
                    offset: 12,
                    shader_location: 1,
                },
            ],
        };
        let compact = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            compact_layout.clone(),
            wgpu::PrimitiveTopology::TriangleList,
            None,
            wgpu::DepthBiasState::default(),
            Some(true),
            Some(wgpu::CompareFunction::Less),
            false,
            "vs_compact",
        );
        let compact_cull = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            compact_layout.clone(),
            wgpu::PrimitiveTopology::TriangleList,
            Some(wgpu::Face::Back),
            wgpu::DepthBiasState::default(),
            Some(true),
            Some(wgpu::CompareFunction::Less),
            false,
            "vs_compact",
        );
        let compact_blend = make_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            compact_layout,
            wgpu::PrimitiveTopology::TriangleList,
            None,
            wgpu::DepthBiasState::default(),
            Some(false),
            Some(wgpu::CompareFunction::Less),
            true,
            "vs_compact",
        );
        let white_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("white"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &white_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            ..Default::default()
        });
        Ok(Self {
            device,
            queue,
            adapter_name,
            pipeline,
            pipeline_cull,
            pipeline_blend,
            line_pipeline,
            line_overlay,
            compact,
            compact_cull,
            compact_blend,
            layout,
            sampler,
            white: white_tex,
            max_buffer_size,
            vertex_budget: max_buffer_size,
        })
    }

    fn white_view(&self) -> wgpu::TextureView {
        self.white
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    pub fn render_rgba(&mut self, frame: &FrameRequest<'_>) -> Result<RgbaImage> {
        let width = frame.width.max(1);
        let height = frame.height.max(1);
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("color"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

        let prepared = prepare(frame);
        let view_proj = camera_matrix(
            frame.scene.bounds,
            frame.yaw,
            frame.pitch,
            width,
            height,
            frame.distance_scale,
            frame.target_offset,
        );
        let mut uniforms = Vec::new();
        let mut draws: Vec<Draw> = Vec::new();
        for draw in &prepared.draws {
            let offset = (uniforms.len() as u64) * UNIFORM_STRIDE;
            uniforms.push(pack_uniform(view_proj, draw));
            draws.push(Draw {
                offset: offset as u32,
                vertex_start: draw.vertex_start,
                vertex_count: draw.vertex_count,
                lines: draw.lines,
                overlay: draw.overlay,
                texture: draw.texture,
                cull: draw.cull,
                blend: draw.blend,
                misc: draw.misc,
            });
        }
        if uniforms.is_empty() {
            uniforms.push(Uniforms {
                view_proj: view_proj.to_cols_array_2d(),
                model: Mat4::IDENTITY.to_cols_array_2d(),
                normal_mat: Mat4::IDENTITY.to_cols_array_2d(),
                base_color: [1.0, 1.0, 1.0, 1.0],
                misc: [0.0, 0.5, 0.0, 0.0],
                extra: [0.0; 4],
            });
        }
        let mut uniform_bytes = Vec::with_capacity(uniforms.len() * UNIFORM_STRIDE as usize);
        for uniform in &uniforms {
            let bytes = bytemuck::bytes_of(uniform);
            uniform_bytes.extend_from_slice(bytes);
            uniform_bytes.resize(
                uniform_bytes.len() + (UNIFORM_STRIDE as usize - bytes.len()),
                0,
            );
        }
        let uniform_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: uniform_bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&uniform_buffer, 0, &uniform_bytes);

        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let chunks = self.upload_vertices(&prepared.vertices, &draws)?;

        let mut texture_views = Vec::new();
        for texture in &frame.scene.textures {
            texture_views.push(self.upload_texture(texture));
        }

        let white_view = self.white_view();

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for chunk in &chunks {
                pass.set_vertex_buffer(0, chunk.buffer.slice(..));
                for draw in &chunk.draws {
                    let view = draw
                        .texture
                        .and_then(|i| texture_views.get(i))
                        .unwrap_or(&white_view);
                    let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("draw"),
                        layout: &self.layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &uniform_buffer,
                                    offset: 0,
                                    size: NonZeroU64::new(std::mem::size_of::<Uniforms>() as u64),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                        ],
                    });
                    pass.set_pipeline(self.pipeline_for(draw, chunk.compact));
                    pass.set_bind_group(0, &bind, &[draw.offset]);
                    pass.draw(
                        draw.vertex_start..draw.vertex_start + draw.vertex_count,
                        0..1,
                    );
                }
            }
        }

        let padded = align_up(width * 4, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let read_size = padded as u64 * height as u64;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: read_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &color,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|err| MvError::render(format!("GPU poll failed: {err}")))?;
        if let Some(err) = pollster::block_on(scope.pop()) {
            return Err(MvError::new(
                "DEVICE_LIMIT",
                format!("GPU rejected the frame: {err}"),
            ));
        }
        rx.recv()
            .map_err(|err| MvError::render(err.to_string()))?
            .map_err(|err| MvError::render(format!("map failed: {err}")))?;
        let mapped = slice
            .get_mapped_range()
            .map_err(|err| MvError::render(format!("map range failed: {err}")))?;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            let src = (y * padded) as usize;
            let dst = (y * width * 4) as usize;
            pixels[dst..dst + (width * 4) as usize]
                .copy_from_slice(&mapped[src..src + (width * 4) as usize]);
        }
        drop(mapped);
        readback.unmap();
        RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(|| MvError::render("could not pack the rendered image"))
    }

    /// Tests use this to force vertex buffers to split. The value is clamped
    /// to the device's real `max_buffer_size`.
    pub fn set_vertex_budget(&mut self, bytes: u64) {
        let bytes = bytes.max(std::mem::size_of::<GpuVertex>() as u64 * 3);
        self.vertex_budget = bytes.min(self.max_buffer_size);
    }

    fn pipeline_for(&self, draw: &Draw, compact: bool) -> &wgpu::RenderPipeline {
        if draw.lines {
            if draw.overlay {
                &self.line_overlay
            } else {
                &self.line_pipeline
            }
        } else if compact {
            if draw.blend {
                &self.compact_blend
            } else if draw.cull {
                &self.compact_cull
            } else {
                &self.compact
            }
        } else if draw.blend {
            &self.pipeline_blend
        } else if draw.cull {
            &self.pipeline_cull
        } else {
            &self.pipeline
        }
    }

    fn upload_vertices(&self, vertices: &[GpuVertex], draws: &[Draw]) -> Result<Vec<VertexChunk>> {
        if draws.is_empty() || vertices.is_empty() {
            return Ok(Vec::new());
        }
        let full_bytes = vertices.len() as u64 * std::mem::size_of::<GpuVertex>() as u64;
        let compact_ok = full_bytes > self.vertex_budget && !draws_need_full_vertices(draws);
        let mut pieces = Vec::new();
        for draw in draws {
            let compact = compact_ok && !draw.lines;
            let align = if draw.lines { 2 } else { 3 };
            let max_verts = self.max_vertices(compact)?;
            for (start, count) in
                split_range(draw.vertex_start, draw.vertex_count, max_verts, align)?
            {
                let mut piece = draw.clone();
                piece.vertex_start = start;
                piece.vertex_count = count;
                pieces.push((piece, compact));
            }
        }
        let mut chunks = Vec::new();
        let mut group: Vec<(Draw, bool)> = Vec::new();
        let mut group_verts = 0u32;
        let mut group_compact = pieces.first().map(|p| p.1).unwrap_or(false);
        for (piece, compact) in pieces {
            let max_verts = self.max_vertices(compact)?;
            let switch = compact != group_compact || group_verts + piece.vertex_count > max_verts;
            if switch && !group.is_empty() {
                chunks.push(self.flush_chunk(vertices, &group, group_compact)?);
                group.clear();
                group_verts = 0;
            }
            group_compact = compact;
            group_verts += piece.vertex_count;
            group.push((piece, compact));
        }
        if !group.is_empty() {
            chunks.push(self.flush_chunk(vertices, &group, group_compact)?);
        }
        Ok(chunks)
    }

    fn max_vertices(&self, compact: bool) -> Result<u32> {
        let stride = if compact {
            std::mem::size_of::<CompactVertex>()
        } else {
            std::mem::size_of::<GpuVertex>()
        } as u64;
        let budget = self.vertex_budget.min(self.max_buffer_size);
        let n = (budget / stride) as u32;
        if n < 3 {
            return Err(MvError::new(
                "DEVICE_LIMIT",
                format!("GPU buffer limit {budget} bytes is too small for a triangle"),
            ));
        }
        Ok(n)
    }

    fn flush_chunk(
        &self,
        vertices: &[GpuVertex],
        group: &[(Draw, bool)],
        compact: bool,
    ) -> Result<VertexChunk> {
        let mut local_draws = Vec::with_capacity(group.len());
        let mut local_start = 0u32;
        let bytes = if compact {
            let mut packed = Vec::new();
            for (draw, _) in group {
                let src = draw.vertex_start as usize;
                let count = draw.vertex_count as usize;
                for v in &vertices[src..src + count] {
                    packed.extend_from_slice(bytemuck::bytes_of(&CompactVertex {
                        pos: v.pos,
                        normal: pack_normal(v.normal),
                    }));
                }
                let mut local = draw.clone();
                local.vertex_start = local_start;
                local_draws.push(local);
                local_start += draw.vertex_count;
            }
            packed
        } else {
            let mut packed = Vec::new();
            for (draw, _) in group {
                let src = draw.vertex_start as usize;
                let count = draw.vertex_count as usize;
                packed.extend_from_slice(bytemuck::cast_slice(&vertices[src..src + count]));
                let mut local = draw.clone();
                local.vertex_start = local_start;
                local_draws.push(local);
                local_start += draw.vertex_count;
            }
            packed
        };
        if bytes.len() as u64 > self.max_buffer_size {
            return Err(MvError::new(
                "DEVICE_LIMIT",
                format!(
                    "vertex chunk is {} bytes and the device allows {}",
                    bytes.len(),
                    self.max_buffer_size
                ),
            ));
        }
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(if compact {
                "vertices-compact"
            } else {
                "vertices"
            }),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, &bytes);
        Ok(VertexChunk {
            buffer,
            compact,
            draws: local_draws,
        })
    }

    fn upload_texture(&self, texture: &crate::scene::Texture) -> wgpu::TextureView {
        if texture.status != "ok"
            || texture.rgba.len() < 4
            || texture.width == 0
            || texture.height == 0
        {
            return self.white_view();
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&texture.name),
            size: wgpu::Extent3d {
                width: texture.width,
                height: texture.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &texture.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(texture.width * 4),
                rows_per_image: Some(texture.height),
            },
            wgpu::Extent3d {
                width: texture.width,
                height: texture.height,
                depth_or_array_layers: 1,
            },
        );
        tex.create_view(&wgpu::TextureViewDescriptor::default())
    }
}

// TextureView is not Clone in some wgpu versions. I used .clone() above.
// If it fails, I'll store the white texture and recreate the view.

#[derive(Clone)]
struct Draw {
    offset: u32,
    vertex_start: u32,
    vertex_count: u32,
    lines: bool,
    overlay: bool,
    texture: Option<usize>,
    cull: bool,
    blend: bool,
    misc: [f32; 4],
}

struct CpuDraw {
    vertex_start: u32,
    vertex_count: u32,
    lines: bool,
    overlay: bool,
    texture: Option<usize>,
    cull: bool,
    blend: bool,
    model: Mat4,
    normal_mat: Mat4,
    base_color: [f32; 4],
    misc: [f32; 4],
    extra: [f32; 4],
}

struct Prepared {
    vertices: Vec<GpuVertex>,
    draws: Vec<CpuDraw>,
}

struct VertexChunk {
    buffer: wgpu::Buffer,
    compact: bool,
    draws: Vec<Draw>,
}

fn pack_normal(normal: [f32; 3]) -> [i8; 4] {
    let pack = |c: f32| (c.clamp(-1.0, 1.0) * 127.0).round() as i8;
    [pack(normal[0]), pack(normal[1]), pack(normal[2]), 0]
}

fn draws_need_full_vertices(draws: &[Draw]) -> bool {
    draws.iter().any(|draw| {
        if draw.lines {
            return false;
        }
        let mode = draw.misc[2];
        // UV checker and weight colors need the full vertex. Textures need UVs.
        draw.misc[3] > 0.5 || (mode > 2.5 && mode < 4.5)
    })
}

fn split_range(start: u32, count: u32, max_verts: u32, align: u32) -> Result<Vec<(u32, u32)>> {
    let max_verts = (max_verts / align) * align;
    if max_verts == 0 {
        return Err(MvError::new(
            "DEVICE_LIMIT",
            "GPU buffer limit is too small to hold one primitive",
        ));
    }
    let mut out = Vec::new();
    let mut left = count;
    let mut at = start;
    while left > 0 {
        let mut n = left.min(max_verts);
        if n < left {
            n = (n / align) * align;
        }
        if n == 0 {
            return Err(MvError::new(
                "DEVICE_LIMIT",
                "GPU buffer limit is too small to hold one primitive",
            ));
        }
        out.push((at, n));
        at += n;
        left -= n;
    }
    Ok(out)
}

fn prepare(frame: &FrameRequest<'_>) -> Prepared {
    let worlds = frame.scene.world_matrices(frame.anim, frame.time);
    let mut vertices = Vec::new();
    let mut draws = Vec::new();
    let mut skinned_done = HashSet::new();

    if frame.show_grid {
        push_grid(&mut vertices, &mut draws, frame.scene.bounds);
    }

    if frame.show_surface {
        for (ni, node) in frame.scene.nodes.iter().enumerate() {
            for &mi in &node.meshes {
                let mesh = &frame.scene.meshes[mi];
                let skinned = mesh.skin.is_some() && !mesh.joints.is_empty();
                if skinned && !skinned_done.insert(mi) {
                    continue;
                }
                let (positions, normals) = if skinned {
                    frame.scene.posed_mesh(mi, &worlds)
                } else {
                    (mesh.positions.clone(), mesh.normals.clone())
                };
                let model = if skinned {
                    Mat4::IDENTITY
                } else {
                    worlds.get(ni).copied().unwrap_or(Mat4::IDENTITY)
                };
                let start = vertices.len() as u32;
                let material = mesh.material.and_then(|i| frame.scene.materials.get(i));
                let base_color = material
                    .map(|m| m.base_color)
                    .unwrap_or([0.72, 0.74, 0.78, 1.0]);
                let texture = material.and_then(|m| m.base_color_texture).filter(|&i| {
                    frame
                        .scene
                        .textures
                        .get(i)
                        .map(|t| t.status == "ok")
                        .unwrap_or(false)
                });
                let uv_set = mesh.uvs.get(frame.uv_set).or_else(|| mesh.uvs.first());
                let corners: Vec<u32> = if mesh.indices.len() >= 3 {
                    mesh.indices.clone()
                } else {
                    (0..positions.len() as u32).collect()
                };
                for tri in corners.chunks_exact(3) {
                    let mut tri = [tri[0], tri[1], tri[2]];
                    // Keep the geometric winding on the same side as the stored
                    // normals. A negative node scale or an exporter that flips
                    // only one of them makes back-face culling punch holes.
                    if let (Some(a), Some(b), Some(c)) = (
                        positions.get(tri[0] as usize),
                        positions.get(tri[1] as usize),
                        positions.get(tri[2] as usize),
                    ) {
                        let geo = (Vec3::from(*b) - Vec3::from(*a))
                            .cross(Vec3::from(*c) - Vec3::from(*a));
                        let stored = [tri[0], tri[1], tri[2]]
                            .iter()
                            .filter_map(|i| normals.get(*i as usize).copied())
                            .map(Vec3::from)
                            .fold(Vec3::ZERO, |acc, n| acc + n);
                        let mirrored = model.determinant() < 0.0;
                        if (geo.dot(stored) < 0.0) != mirrored {
                            tri.swap(1, 2);
                        }
                    }
                    for idx in tri {
                        let i = idx as usize;
                        let uv = uv_set
                            .and_then(|s| s.coords.get(i))
                            .copied()
                            .unwrap_or([0.0, 0.0]);
                        let slot = frame
                            .bone
                            .and_then(|node| joint_slot(frame.scene, mesh, node));
                        let color = if frame.mode == RenderMode::Weights {
                            heat(weight_of(mesh, i, slot))
                        } else {
                            [1.0, 1.0, 1.0, 1.0]
                        };
                        let mut normal = normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
                        if Vec3::from(normal).length_squared() < 1e-8 {
                            normal = [0.0, 1.0, 0.0];
                        }
                        vertices.push(GpuVertex {
                            pos: positions.get(i).copied().unwrap_or([0.0, 0.0, 0.0]),
                            normal,
                            uv,
                            color,
                        });
                    }
                }
                let count = vertices.len() as u32 - start;
                let alpha_mode = material.map(|m| m.alpha_mode.as_str()).unwrap_or("opaque");
                let blend = alpha_mode == "blend"
                    || material.map(|m| m.base_color[3] < 0.99).unwrap_or(false);
                let opaque = !blend && alpha_mode != "mask";
                let cull = opaque && !material.map(|m| m.double_sided).unwrap_or(false);
                let extra = [
                    match alpha_mode {
                        "mask" => 1.0,
                        "blend" => 2.0,
                        _ if blend => 2.0,
                        _ => 0.0,
                    },
                    material.map(|m| m.alpha_cutoff).unwrap_or(0.5),
                    if material
                        .map(|m| m.shading_model.eq_ignore_ascii_case("unlit"))
                        .unwrap_or(false)
                    {
                        1.0
                    } else {
                        0.0
                    },
                    0.0,
                ];
                if count >= 3 {
                    draws.push(CpuDraw {
                        vertex_start: start,
                        vertex_count: count,
                        lines: false,
                        overlay: false,
                        cull,
                        texture: if frame.mode == RenderMode::Shaded {
                            texture
                        } else {
                            None
                        },
                        model,
                        normal_mat: normal_matrix(model),
                        base_color,
                        misc: [
                            material.map(|m| m.metallic).unwrap_or(0.0),
                            material.map(|m| m.roughness).unwrap_or(0.55),
                            frame.mode.shader_mode(),
                            if texture.is_some() && frame.mode == RenderMode::Shaded {
                                1.0
                            } else {
                                0.0
                            },
                        ],
                        extra,
                        blend,
                    });
                }
                if frame.mode == RenderMode::Wireframe && count >= 3 {
                    let line_start = vertices.len() as u32;
                    push_edges(&mut vertices, &positions, &mesh.indices);
                    let line_count = vertices.len() as u32 - line_start;
                    if line_count >= 2 {
                        draws.push(CpuDraw {
                            vertex_start: line_start,
                            vertex_count: line_count,
                            lines: true,
                            overlay: true,
                            cull: false,
                            texture: None,
                            model,
                            normal_mat: Mat4::IDENTITY,
                            base_color: [0.92, 0.94, 0.96, 1.0],
                            misc: [0.0, 0.0, 5.0, 0.0],
                            extra: [0.0; 4],
                            blend: false,
                        });
                    }
                }
            }
        }
    }

    if frame.show_skeleton {
        let line_start = vertices.len() as u32;
        for (i, node) in frame.scene.nodes.iter().enumerate() {
            if !node.is_bone {
                continue;
            }
            let Some(parent) = node.parent else { continue };
            let a = worlds.get(parent).map(translation_of).unwrap_or(Vec3::ZERO);
            let b = worlds.get(i).map(translation_of).unwrap_or(Vec3::ZERO);
            push_line(&mut vertices, a, b, [0.95, 0.55, 0.20, 1.0]);
        }
        let line_count = vertices.len() as u32 - line_start;
        if line_count >= 2 {
            draws.push(CpuDraw {
                vertex_start: line_start,
                vertex_count: line_count,
                lines: true,
                overlay: true,
                cull: false,
                texture: None,
                model: Mat4::IDENTITY,
                normal_mat: Mat4::IDENTITY,
                base_color: [0.95, 0.55, 0.20, 1.0],
                misc: [0.0, 0.0, 5.0, 0.0],
                extra: [0.0; 4],
                blend: false,
            });
        }
    }
    draws.sort_by_key(|d| d.blend);
    Prepared { vertices, draws }
}

fn push_grid(vertices: &mut Vec<GpuVertex>, draws: &mut Vec<CpuDraw>, bounds: Aabb) {
    let start = vertices.len() as u32;
    let center = Vec3::from(bounds.center());
    let y = if bounds.is_valid() {
        bounds.min[1] - bounds.radius().max(1e-9) * 0.02
    } else {
        0.0
    };
    // The grid is sized to the model. A 1 m floor around a 2 mm part
    // becomes a stripe across the frame and hides the mesh.
    let extent = bounds.radius().max(1e-9) * 2.5;
    let step = nice_step(extent / 4.0);
    let n = ((extent / step).ceil() as i32).clamp(1, 24);
    for i in -n..=n {
        let t = i as f32 * step;
        push_line(
            vertices,
            Vec3::new(center.x - n as f32 * step, y, center.z + t),
            Vec3::new(center.x + n as f32 * step, y, center.z + t),
            [0.30, 0.32, 0.36, 1.0],
        );
        push_line(
            vertices,
            Vec3::new(center.x + t, y, center.z - n as f32 * step),
            Vec3::new(center.x + t, y, center.z + n as f32 * step),
            [0.30, 0.32, 0.36, 1.0],
        );
    }
    let axis = bounds.radius().max(1e-9) * 0.35;
    push_line(
        vertices,
        Vec3::ZERO,
        Vec3::X * axis,
        [0.85, 0.25, 0.22, 1.0],
    );
    push_line(
        vertices,
        Vec3::ZERO,
        Vec3::Y * axis,
        [0.30, 0.75, 0.32, 1.0],
    );
    push_line(
        vertices,
        Vec3::ZERO,
        Vec3::Z * axis,
        [0.25, 0.45, 0.90, 1.0],
    );
    let count = vertices.len() as u32 - start;
    if count >= 2 {
        draws.push(CpuDraw {
            vertex_start: start,
            vertex_count: count,
            lines: true,
            overlay: false,
            cull: false,
            texture: None,
            model: Mat4::IDENTITY,
            normal_mat: Mat4::IDENTITY,
            base_color: [0.45, 0.48, 0.52, 1.0],
            misc: [0.0, 0.0, 5.0, 0.0],
            extra: [0.0; 4],
            blend: false,
        });
    }
}

fn push_edges(vertices: &mut Vec<GpuVertex>, positions: &[[f32; 3]], indices: &[u32]) {
    let mut seen = HashSet::new();
    for tri in indices.chunks_exact(3) {
        for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
            let key = if a < b { (a, b) } else { (b, a) };
            if !seen.insert(key) {
                continue;
            }
            let Some(pa) = positions.get(a as usize).copied() else {
                continue;
            };
            let Some(pb) = positions.get(b as usize).copied() else {
                continue;
            };
            push_line(
                vertices,
                Vec3::from(pa),
                Vec3::from(pb),
                [0.92, 0.94, 0.96, 1.0],
            );
        }
    }
}

fn push_line(vertices: &mut Vec<GpuVertex>, a: Vec3, b: Vec3, color: [f32; 4]) {
    for p in [a, b] {
        vertices.push(GpuVertex {
            pos: p.to_array(),
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            color,
        });
    }
}

fn weight_of(mesh: &Mesh, vertex: usize, bone: Option<usize>) -> f32 {
    let Some(bone) = bone else { return 0.0 };
    let Some(skin_index) = mesh.skin else {
        return 0.0;
    };
    let _ = skin_index;
    let joints = mesh.joints.get(vertex).copied().unwrap_or([0; 4]);
    let weights = mesh.weights.get(vertex).copied().unwrap_or([0.0; 4]);
    // Joint indices are slots into the skin, not node indices. The caller
    // passes a node index; the slot is resolved before this function via
    // the color already... we resolve here by comparing against the mesh
    // skin stored on the scene. This function only sees the mesh, so the
    // caller passes the slot through `bone` after lookup. See `prepare`.
    let mut w = 0.0;
    for k in 0..4 {
        if joints[k] as usize == bone {
            w += weights[k];
        }
    }
    w
}

fn heat(w: f32) -> [f32; 4] {
    let w = w.clamp(0.0, 1.0);
    let r = (w * 1.6).clamp(0.0, 1.0);
    let g = if w < 0.5 {
        w * 1.6
    } else {
        (1.0 - (w - 0.5) * 1.4).clamp(0.0, 1.0)
    };
    let b = (1.0 - w * 1.7).clamp(0.0, 1.0);
    [r, g, b, 1.0]
}

fn translation_of(m: &Mat4) -> Vec3 {
    m.w_axis.truncate()
}

fn normal_matrix(model: Mat4) -> Mat4 {
    model.inverse().transpose()
}

fn nice_step(value: f32) -> f32 {
    let value = value.max(1e-12);
    let exp = value.log10().floor();
    let base = 10f32.powf(exp);
    let m = value / base;
    let nice = if m < 1.5 {
        1.0
    } else if m < 3.5 {
        2.0
    } else if m < 7.5 {
        5.0
    } else {
        10.0
    };
    nice * base
}

fn camera_matrix(
    bounds: Aabb,
    yaw_deg: f32,
    pitch_deg: f32,
    width: u32,
    height: u32,
    distance_scale: f32,
    target_offset: [f32; 3],
) -> Mat4 {
    let target = Vec3::from(bounds.center()) + Vec3::from(target_offset);
    let distance = view_distance(bounds, distance_scale);
    let (near, far) = clip_planes(bounds, distance_scale);
    let yaw = yaw_deg.to_radians();
    let pitch = pitch_deg.clamp(-89.0, 89.0).to_radians();
    let eye = target
        + Vec3::new(
            distance * pitch.cos() * yaw.sin(),
            distance * pitch.sin(),
            distance * pitch.cos() * yaw.cos(),
        );
    let view = glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y);
    let aspect = width as f32 / height.max(1) as f32;
    // WebGPU's clip space is Y-up and Z in [0, 1], which is glam's DirectX convention.
    glam::camera::rh::proj::directx::perspective(40f32.to_radians(), aspect, near, far) * view
}

/// How many pixels differ from a render of the same frame with the meshes hidden.
/// A speck on the clear colour, or the grid by itself, does not count.
pub fn model_pixel_count(image: &RgbaImage, background: &RgbaImage) -> u32 {
    if image.dimensions() != background.dimensions() {
        return 0;
    }
    image
        .pixels()
        .zip(background.pixels())
        .filter(|(a, b)| {
            let d = (a[0] as i16 - b[0] as i16).unsigned_abs() as u32
                + (a[1] as i16 - b[1] as i16).unsigned_abs() as u32
                + (a[2] as i16 - b[2] as i16).unsigned_abs() as u32;
            d > 18
        })
        .count() as u32
}

pub fn render_misses_the_model(image: &RgbaImage, background: &RgbaImage) -> bool {
    let different = model_pixel_count(image, background);
    let min_pixels = (image.width().saturating_mul(image.height()) / 400).max(24);
    different < min_pixels
}

pub fn without_surface<'a>(frame: &FrameRequest<'a>) -> FrameRequest<'a> {
    FrameRequest {
        scene: frame.scene,
        width: frame.width,
        height: frame.height,
        yaw: frame.yaw,
        pitch: frame.pitch,
        mode: frame.mode,
        anim: frame.anim,
        time: frame.time,
        uv_set: frame.uv_set,
        bone: frame.bone,
        show_skeleton: frame.show_skeleton,
        show_grid: frame.show_grid,
        show_surface: false,
        distance_scale: frame.distance_scale,
        target_offset: frame.target_offset,
    }
}

/// A render that is one solid color drew neither the mesh nor the grid.
pub fn image_is_empty(image: &RgbaImage) -> bool {
    if image.width() == 0 || image.height() == 0 {
        return true;
    }
    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    for pixel in image.pixels() {
        for c in 0..3 {
            min[c] = min[c].min(pixel[c]);
            max[c] = max[c].max(pixel[c]);
        }
    }
    max[0] - min[0] < 4 && max[1] - min[1] < 4 && max[2] - min[2] < 4
}

/// Near and far planes fitted to the framed bounds. A fixed 10_000:1 range
/// turns a 2cm mesh (or a large one with a very small near plane) into
/// depth-buffer noise.
pub fn clip_planes(bounds: Aabb, distance_scale: f32) -> (f32, f32) {
    let radius = bounds.radius().max(1e-9);
    let distance = view_distance(bounds, distance_scale);
    let closest = (distance - radius).max(radius * 1e-3);
    let near = (closest * 0.25).max(radius * 1e-4);
    let far = distance + radius * 2.5;
    (near, far.max(near * 4.0))
}

fn pack_uniform(view_proj: Mat4, draw: &CpuDraw) -> Uniforms {
    Uniforms {
        view_proj: view_proj.to_cols_array_2d(),
        model: draw.model.to_cols_array_2d(),
        normal_mat: draw.normal_mat.to_cols_array_2d(),
        base_color: draw.base_color,
        misc: draw.misc,
        extra: draw.extra,
    }
}

fn make_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    vertex_layout: wgpu::VertexBufferLayout<'_>,
    topology: wgpu::PrimitiveTopology,
    cull_mode: Option<wgpu::Face>,
    bias: wgpu::DepthBiasState,
    depth_write: Option<bool>,
    depth_compare: Option<wgpu::CompareFunction>,
    alpha_blend: bool,
    vertex_entry: &'static str,
) -> wgpu::RenderPipeline {
    let buffers = [Some(vertex_layout)];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("modelview"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vertex_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &buffers,
        },
        primitive: wgpu::PrimitiveState {
            topology,
            cull_mode,
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: depth_write,
            depth_compare,
            stencil: wgpu::StencilState::default(),
            bias,
        }),
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: Some(if alpha_blend {
                    wgpu::BlendState::ALPHA_BLENDING
                } else {
                    wgpu::BlendState::REPLACE
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn align_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut bytes);
    image
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|err| MvError::render(format!("png encode failed: {err}")))?;
    Ok(bytes)
}

/// Which skin-joint slot corresponds to a node, for the heatmap.
pub fn joint_slot(scene: &Scene, mesh: &Mesh, bone_node: usize) -> Option<usize> {
    let skin = mesh.skin.and_then(|i| scene.skins.get(i))?;
    skin.joints.iter().position(|j| *j == bone_node)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_planes_fit_tiny_and_large_bounds() {
        for half in [1e-6f32, 0.0012, 0.01, 1.5, 40.0, 1_000.0] {
            let bounds = Aabb {
                min: [-half, -half, -half],
                max: [half, half, half],
            };
            let (near, far) = clip_planes(bounds, 1.0);
            let distance = frame_distance(bounds);
            let radius = bounds.radius();
            assert!(
                near < distance - radius,
                "near {near} clips radius {radius}"
            );
            assert!(far > distance + radius, "far {far} clips the back");
            assert!(
                far / near < 40.0,
                "depth range {far}/{near} is too wide for half={half}"
            );
        }
    }

    #[test]
    fn a_solid_colour_image_is_empty() {
        let blank = RgbaImage::from_pixel(8, 8, image::Rgba([40, 42, 48, 255]));
        assert!(image_is_empty(&blank));
        let mut drawn = blank.clone();
        drawn.put_pixel(3, 3, image::Rgba([220, 180, 40, 255]));
        assert!(!image_is_empty(&drawn));
    }

    #[test]
    fn camera_distance_scales_from_millimetres_to_kilometres() {
        let tiny = Aabb {
            min: [-0.0012, -0.0012, -0.0012],
            max: [0.0012, 0.0012, 0.0012],
        };
        let distance = view_distance(tiny, 1.0);
        assert!(
            distance < 0.02,
            "a 2.4 mm part was framed from {distance} m"
        );
        assert!(distance > tiny.radius());
        let huge = Aabb {
            min: [-1_000.0, -1_000.0, -1_000.0],
            max: [1_000.0, 1_000.0, 1_000.0],
        };
        let far = view_distance(huge, 1.0);
        assert!(far > 1_000.0 && far < 20_000.0, "km framing was {far}");
        let (near, plane_far) = clip_planes(tiny, 1.0);
        assert!(plane_far / near < 40.0, "{plane_far}/{near}");
    }

    #[test]
    fn grid_step_stays_smaller_than_a_tiny_model() {
        let bounds = Aabb {
            min: [0.0, 0.0, 0.0],
            max: [0.0024, 0.0024, 0.0024],
        };
        let step = nice_step(bounds.radius() * 2.5 / 4.0);
        assert!(
            step < 0.0024,
            "grid step {step} is larger than a 2.4 mm model"
        );
    }

    #[test]
    fn a_blank_frame_matches_its_background() {
        let color = image::Rgba([40, 43, 51, 255]);
        let image = RgbaImage::from_pixel(64, 64, color);
        let background = image.clone();
        assert!(render_misses_the_model(&image, &background));
        let mut drawn = image.clone();
        for y in 20..44 {
            for x in 20..44 {
                drawn.put_pixel(x, y, image::Rgba([180, 170, 160, 255]));
            }
        }
        assert!(!render_misses_the_model(&drawn, &background));
    }
}
