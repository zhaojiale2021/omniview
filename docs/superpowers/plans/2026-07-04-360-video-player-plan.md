# 360° Video Player — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a minimal but functional 360° video player that opens a window, plays an equirectangular video, and allows mouse-controlled view rotation.

**Architecture:** wgpu renders a UV sphere from the inside; the sphere's texture comes from ffmpeg-decoded video frames. egui overlays playback controls on the 3D scene. The decoder runs on a background thread, pushing frames to the main thread via channels.

**Tech Stack:** Rust + winit 0.30 + wgpu 24 + ffmpeg-next 7 + egui 0.29 + egui-winit 0.29 + egui-wgpu 0.29 + glam 0.29 + bytemuck 1 + rfd 0.15

## Global Constraints

- Rust edition 2024, minimum rustc 1.96
- FFmpeg development libraries must be installed (`libavformat-dev`, `libavcodec-dev`, `libavutil-dev`, `libswscale-dev` on Debian; `ffmpeg-devel` on Fedora)
- All wgpu shaders in WGSL
- No unsafe code outside ffmpeg bindings and wgpu surface creation
- Commit after every task with conventional-commit message format

---

### Task 1: Project Scaffolding & Window Creation

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/app.rs`

**Interfaces:**
- Produces: `App` struct implementing `winit::application::ApplicationHandler`, opens a 1280×720 window.

- [ ] **Step 1: Write Cargo.toml**

```toml
[package]
name = "my-project"
version = "0.1.0"
edition = "2024"

[dependencies]
winit = "0.30"
wgpu = "24"
pollster = "0.4"
ffmpeg-next = "7"
egui = "0.29"
egui-winit = "0.29"
egui-wgpu = "0.29"
rfd = "0.15"
glam = "0.29"
bytemuck = { version = "1", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[features]
default = []
```

- [ ] **Step 2: Write src/main.rs**

```rust
mod app;
mod renderer;
mod decoder;
mod ui;

use winit::event_loop::EventLoop;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let event_loop = EventLoop::new().unwrap();
    let mut app = app::App::new();
    tracing::info!("Starting 360° Video Player");
    event_loop.run_app(&mut app).unwrap();
}
```

- [ ] **Step 3: Write src/app.rs**

```rust
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};

pub struct App {
    pub window: Option<Arc<Window>>,
}

impl App {
    pub fn new() -> Self {
        Self { window: None }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if let WindowEvent::CloseRequested = event {
            event_loop.exit();
        }
    }
}
```

- [ ] **Step 4: Verify build**

Run: `cargo run`
Expected: Window titled "360° Video Player" opens at 1280×720, closes cleanly.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/main.rs src/app.rs Cargo.lock
git commit -m "feat: scaffold project with winit window"
```

---

### Task 2: wgpu Renderer Core

**Files:**
- Create: `src/renderer/mod.rs`
- Modify: `src/app.rs`

**Interfaces:**
- `Renderer::new(window: Arc<Window>) -> Renderer` (async via `pollster::block_on`)
- `Renderer::resize(width, height)`
- `Renderer::render()` — clears to dark gray

- [ ] **Step 1: Write src/renderer/mod.rs**

```rust
use std::sync::Arc;
use wgpu::{PresentMode, TextureUsages};
use winit::window::Window;

pub struct Renderer {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: (u32, u32),
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .unwrap();

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        tracing::info!("wgpu ready: adapter={}, format={:?}", adapter.get_info().name, format);
        Self { surface, device, queue, config, size: (size.width, size.height) }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        self.config.width = self.size.0;
        self.config.height = self.size.1;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Encoder") });

        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.12, g: 0.12, b: 0.15, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}
```

- [ ] **Step 2: Update app.rs to initialize Renderer and render each frame**

```rust
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};
use crate::renderer::Renderer;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
}

impl App {
    pub fn new() -> Self {
        Self { window: None, renderer: None }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let renderer = match &mut self.renderer {
            Some(r) => r,
            None => return,
        };
        if let Err(e) = renderer.render() {
            match e {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let s = renderer.size;
                    renderer.resize(s.0, s.1);
                }
                wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                wgpu::SurfaceError::Timeout => {}
            }
        }
    }
}
```

- [ ] **Step 3: Build and verify**

Run: `cargo run`
Expected: Dark-gray window at 1280×720, stable rendering, no flickering.

- [ ] **Step 4: Commit**

```bash
git add src/renderer/mod.rs src/app.rs Cargo.lock
git commit -m "feat: add wgpu renderer with surface clear"
```

---

### Task 3: Sphere Geometry & Equirectangular Shader

**Files:**
- Create: `src/renderer/sphere.rs`
- Create: `src/renderer/equirect.wgsl`
- Modify: `src/renderer/mod.rs`

**Interfaces:**
- `Sphere::new(device, sectors, stacks) -> Sphere` — creates vertex/index buffers for inward-facing UV sphere
- `Vertex` — `#[repr(C)]` struct with `position: [f32; 3]`, `uv: [f32; 2]`, with `Vertex::desc()` layout
- `CameraUniform` — `#[repr(C)]` struct with `view_proj: [[f32; 4]; 4]`
- Renderer renders the sphere with a placeholder white texture

- [ ] **Step 1: Write src/renderer/sphere.rs**

```rust
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: 12,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
            ],
        }
    }
}

pub struct Sphere {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
}

impl Sphere {
    pub fn new(device: &wgpu::Device, sectors: u32, stacks: u32) -> Self {
        let radius = 1.0;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        for i in 0..=stacks {
            let phi = std::f32::consts::PI * (i as f32 / stacks as f32);
            let (sin_phi, cos_phi) = phi.sin_cos();
            for j in 0..=sectors {
                let theta = 2.0 * std::f32::consts::PI * (j as f32 / sectors as f32);
                let (sin_theta, cos_theta) = theta.sin_cos();
                vertices.push(Vertex {
                    position: [
                        radius * sin_phi * sin_theta,
                        radius * cos_phi,
                        radius * sin_phi * cos_theta,
                    ],
                    uv: [j as f32 / sectors as f32, i as f32 / stacks as f32],
                });
            }
        }

        for i in 0..stacks {
            for j in 0..sectors {
                let first = i * (sectors + 1) + j;
                let second = first + sectors + 1;
                indices.extend_from_slice(&[
                    first as u32, second as u32, (first + 1) as u32,
                    second as u32, (second + 1) as u32, (first + 1) as u32,
                ]);
            }
        }

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Sphere VB"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Sphere IB"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        Self { vertex_buffer, index_buffer, index_count: indices.len() as u32 }
    }
}
```

- [ ] **Step 2: Write src/renderer/equirect.wgsl**

```wgsl
struct Uniforms {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var video_texture: texture_2d<f32>;
@group(1) @binding(1) var video_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_pos = uniforms.view_proj * vec4(input.position, 1.0);
    out.uv = input.uv;
    return out;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(video_texture, video_sampler, input.uv);
}
```

- [ ] **Step 3: Update src/renderer/mod.rs with full pipeline, sphere, camera uniform, and placeholder texture**

Full replacement of the file:

```rust
use std::sync::Arc;
use wgpu::{PresentMode, TextureUsages};
use winit::window::Window;

pub mod sphere;
use sphere::{Sphere, Vertex};

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
}

impl CameraUniform {
    pub fn new() -> Self {
        Self { view_proj: glam::Mat4::IDENTITY.to_cols_array_2d() }
    }
}

pub struct Renderer {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: (u32, u32),
    pub sphere: Sphere,
    pub render_pipeline: wgpu::RenderPipeline,
    pub camera_buffer: wgpu::Buffer,
    pub camera_bind_group: wgpu::BindGroup,
    pub texture_sampler: wgpu::Sampler,
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    pub video_texture: Option<wgpu::Texture>,
    pub video_texture_view: Option<wgpu::TextureView>,
    pub video_bind_group: Option<wgpu::BindGroup>,
    pub placeholder_bind_group: wgpu::BindGroup,
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .unwrap();
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied()
            .find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let sphere = Sphere::new(&device, 64, 32);

        let sphere_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Equirect Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("equirect.wgsl").into()),
        });

        // Camera uniform buffer
        let aspect = size.width as f32 / size.height.max(1) as f32;
        let initial_vp = glam::Mat4::perspective_rh(
            std::f32::consts::FRAC_PI_2,
            aspect,
            0.1,
            100.0,
        )
        .to_cols_array_2d();

        let camera_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Camera UB"),
            contents: bytemuck::cast_slice(&[CameraUniform { view_proj: initial_vp }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Camera BGL"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Camera BG"),
            layout: &camera_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entity_binding(),
            }],
        });

        // Texture sampler
        let texture_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Video Sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Texture bind group layout
        let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Texture BGL"),
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

        // Placeholder 1×1 white texture
        let placeholder = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Placeholder"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &placeholder,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let placeholder_view = placeholder.create_view(&Default::default());
        let placeholder_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Placeholder BG"),
            layout: &texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&placeholder_view),
                },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&texture_sampler) },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&camera_bgl, &texture_bgl],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Equirect Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &sphere_shader,
                entry_point: "vs_main",
                buffers: &[Vertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &sphere_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                // CW winding + cull front = inside-out rendering (we're inside the sphere)
                front_face: wgpu::FrontFace::Cw,
                cull_mode: Some(wgpu::Face::Front),
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        tracing::info!("Render pipeline created");

        Self {
            surface, device, queue, config, size: (size.width, size.height),
            sphere, render_pipeline, camera_buffer, camera_bind_group,
            texture_sampler, texture_bind_group_layout: texture_bgl,
            video_texture: None, video_texture_view: None, video_bind_group: None,
            placeholder_bind_group,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        self.config.width = self.size.0;
        self.config.height = self.size.1;
        self.surface.configure(&self.device, &self.config);
    }

    /// Update camera uniform buffer from a view-projection matrix
    pub fn update_camera(&mut self, view_proj: &[[f32; 4]; 4]) {
        self.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[CameraUniform { view_proj: *view_proj }]),
        );
    }

    /// Upload RGBA pixel data as a wgpu texture for video frame rendering
    pub fn update_video_texture(&mut self, rgba_data: &[u8], width: u32, height: u32) {
        let tex_size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };

        let needs_new = match &self.video_texture {
            Some(t) => t.width() != width || t.height() != height,
            None => true,
        };

        if needs_new {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Video Frame"),
                size: tex_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Video BG"),
                layout: &self.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.texture_sampler),
                    },
                ],
            });
            self.video_texture = Some(texture);
            self.video_texture_view = Some(view);
            self.video_bind_group = Some(bind_group);
        }

        if let Some(ref texture) = self.video_texture {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                tex_size,
            );
        }
    }

    pub fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Encoder") });

        {
            let texture_bg = self.video_bind_group.as_ref().unwrap_or(&self.placeholder_bind_group);
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Main Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0, g: 0.0, b: 0.0, a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_pipeline(&self.render_pipeline);
            rpass.set_bind_group(0, &self.camera_bind_group, &[]);
            rpass.set_bind_group(1, texture_bg, &[]);
            rpass.set_vertex_buffer(0, self.sphere.vertex_buffer.slice(..));
            rpass.set_index_buffer(self.sphere.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            rpass.draw_indexed(0..self.sphere.index_count, 0, 0..1);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}
```

- [ ] **Step 4: Build and test**

Run: `cargo run`
Expected: Window shows a mostly-white/light-gray interior of the sphere (placeholder white texture mapped onto the inside of the sphere with perspective projection).

- [ ] **Step 5: Commit**

```bash
git add src/renderer/sphere.rs src/renderer/equirect.wgsl src/renderer/mod.rs Cargo.lock
git commit -m "feat: add sphere geometry, equirect shader, render pipeline"
```

---

### Task 4: Orbit Camera

**Files:**
- Create: `src/renderer/camera.rs`
- Modify: `src/renderer/mod.rs`
- Modify: `src/app.rs`

**Interfaces:**
- `OrbitCamera::new() -> Self`
- `OrbitCamera::view_proj_matrix(aspect) -> [[f32; 4]; 4]`
- `OrbitCamera::handle_mouse(delta_x, delta_y, window_height)`
- `OrbitCamera::handle_scroll(delta)`

- [ ] **Step 1: Write src/renderer/camera.rs**

```rust
use glam::Mat4;

pub struct OrbitCamera {
    pub yaw: f32,
    pub pitch: f32,
    pub fov: f32,
    pub sensitivity: f32,
}

impl OrbitCamera {
    pub fn new() -> Self {
        Self { yaw: 0.0, pitch: 0.0, fov: 90.0, sensitivity: 0.003 }
    }

    pub fn view_proj_matrix(&self, aspect: f32) -> [[f32; 4]; 4] {
        let proj = Mat4::perspective_rh(self.fov.to_radians(), aspect, 0.1, 100.0);
        let view = Mat4::from_euler(glam::EulerRot::YXZ, self.yaw.to_radians(), self.pitch.to_radians(), 0.0);
        (proj * view).to_cols_array_2d()
    }

    pub fn handle_mouse(&mut self, delta_x: f64, delta_y: f64, window_height: f64) {
        let factor = self.sensitivity / window_height.max(1.0) as f32;
        self.yaw += (delta_x as f32) * factor * 180.0;
        self.pitch += (delta_y as f32) * factor * 180.0;
        self.pitch = self.pitch.clamp(-89.0, 89.0);
    }

    pub fn handle_scroll(&mut self, delta: f32) {
        self.fov = (self.fov - delta * 2.0).clamp(30.0, 120.0);
    }
}
```

- [ ] **Step 2: Update renderer/mod.rs — add camera field**

At top:
```rust
pub mod camera;
use camera::OrbitCamera;
```

Add field to `Renderer` struct:
```rust
pub camera: OrbitCamera,
```

Initialize in `new()`:
```rust
let camera = OrbitCamera::new();
```

After creating the camera buffer / initial VP, update the initial write:
```rust
let aspect = size.width as f32 / size.height.max(1) as f32;
let initial_vp = camera.view_proj_matrix(aspect);
queue.write_buffer(&camera_buffer, 0, bytemuck::cast_slice(&[CameraUniform {
    view_proj: initial_vp,
}]));
```

Add method:
```rust
pub fn update_camera_uniform(&mut self) {
    let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
    let vp = self.camera.view_proj_matrix(aspect);
    self.update_camera(&vp);
}
```

Call `self.update_camera_uniform();` at the start of `render()`.

- [ ] **Step 3: Update app.rs — forward mouse events to camera**

```rust
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};
use crate::renderer::Renderer;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub dragging: bool,
}

impl App {
    pub fn new() -> Self {
        Self { window: None, renderer: None, dragging: false }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                    r.update_camera_uniform();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = true;
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = false;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(r) = &mut self.renderer {
                    let scroll = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32 / 10.0,
                    };
                    r.camera.handle_scroll(scroll);
                    r.update_camera_uniform();
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.dragging {
                if let Some(r) = &mut self.renderer {
                    r.camera.handle_mouse(delta.0, delta.1, r.size.1 as f64);
                    r.update_camera_uniform();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        let renderer = match &mut self.renderer { Some(r) => r, None => return };
        renderer.update_camera_uniform();
        if let Err(e) = renderer.render() {
            match e {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let s = renderer.size;
                    renderer.resize(s.0, s.1);
                }
                wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                wgpu::SurfaceError::Timeout => {}
            }
        }
    }
}
```

- [ ] **Step 4: Build and test**

Run: `cargo run`
Expected: Window shows sphere interior. Left-click-drag to rotate view. Scroll to zoom in/out (FOV changes). Movement should be smooth.

- [ ] **Step 5: Commit**

```bash
git add src/renderer/camera.rs src/renderer/mod.rs src/app.rs Cargo.lock
git commit -m "feat: add orbit camera with mouse drag rotation"
```

---

### Task 5: ffmpeg Video Decoder

**Files:**
- Create: `src/decoder/mod.rs`
- Create: `src/decoder/video.rs`

**Interfaces:**
- `VideoDecoder::open(path: &str) -> Result<(VideoDecoder, mpsc::Receiver<DecodedFrame>, ..., String>`
- `DecodedFrame { data: Vec<u8>, width: u32, height: u32, pts: f64 }`

- [ ] **Step 1: Write src/decoder/mod.rs**

```rust
pub mod video;
```

- [ ] **Step 2: Write src/decoder/video.rs**

```rust
use std::sync::{
    mpsc,
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

#[derive(Clone)]
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts: f64,
    pub duration: f64,
}

#[derive(Debug, Clone)]
pub enum DecoderCommand {
    Pause,
    Resume,
    Seek(f64),
    Stop,
}

pub struct VideoDecoder {
    pub frame_rx: mpsc::Receiver<DecodedFrame>,
    command_tx: mpsc::Sender<DecoderCommand>,
    thread_handle: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    pub duration: f64,
}

impl VideoDecoder {
    pub fn open(
        path: &str,
    ) -> Result<(Self, mpsc::Sender<DecoderCommand>), String> {
        ffmpeg_next::init().map_err(|e| format!("ffmpeg init failed: {e}"))?;

        let ictx =
            ffmpeg_next::format::input(&path).map_err(|e| format!("Cannot open {path}: {e}"))?;

        let duration = ictx.duration() as f64 / f64::from(ffmpeg_next::ffi::AV_TIME_BASE);

        let input_stream = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .ok_or_else(|| "No video stream".to_string())?;
        let video_stream_index = input_stream.index();

        // Extract time_base for PTS calculation
        let time_base = input_stream.time_base();

        let decoder_context =
            ffmpeg_next::codec::context::Context::from_parameters(input_stream.parameters())
                .map_err(|e| format!("Codec params: {e}"))?;
        let mut decoder = decoder_context
            .decoder()
            .open()
            .map_err(|e| format!("Decoder open: {e}"))?;

        let (frame_tx, frame_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = stopped.clone();

        let thread_handle = Some(thread::spawn(move || {
            Self::decode_loop(
                ictx, decoder, video_stream_index, time_base,
                frame_tx, command_rx, stopped_clone,
            );
        }));

        Ok((
            Self {
                frame_rx,
                command_tx,
                thread_handle,
                stopped,
                duration,
            },
            command_tx,
        ))
    }

    fn decode_loop(
        mut ictx: ffmpeg_next::format::context::InputContext,
        mut decoder: ffmpeg_next::decoder::Video,
        stream_index: usize,
        time_base: ffmpeg_next::rational::Rational,
        frame_tx: mpsc::Sender<DecodedFrame>,
        command_rx: mpsc::Receiver<DecoderCommand>,
        stopped: Arc<AtomicBool>,
    ) {
        let mut paused = false;
        let tb_num = time_base.numerator() as f64;
        let tb_den = time_base.denominator() as f64;
        let tb_factor = tb_num / tb_den;

        for (stream, packet) in ictx.packets() {
            if stopped.load(Ordering::Relaxed) {
                break;
            }

            // Non-blocking command check
            if let Ok(cmd) = command_rx.try_recv() {
                match cmd {
                    DecoderCommand::Stop => break,
                    DecoderCommand::Pause => paused = true,
                    DecoderCommand::Resume => paused = false,
                    DecoderCommand::Seek(pts) => {
                        let ts = (pts / tb_factor) as i64;
                        let _ = ictx.seek(ts, ..ts.saturating_add(1));
                        let _ = decoder.flush();
                    }
                }
            }

            if paused {
                thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }

            if stream.index() == stream_index {
                if decoder.send_packet(&packet).is_ok() {
                    while let Ok(frame) = decoder.receive() {
                        if let Some(decoded) = Self::frame_to_rgba(&frame, tb_factor) {
                            if frame_tx.send(decoded).is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                }
            }
        }

        // Flush remaining frames
        decoder.send_eof().ok();
        while let Ok(frame) = decoder.receive() {
            if let Some(decoded) = Self::frame_to_rgba(&frame, tb_factor) {
                let _ = frame_tx.send(decoded);
            }
        }
        tracing::info!("Decoder thread finished");
    }

    fn frame_to_rgba(frame: &ffmpeg_next::frame::Video, tb_factor: f64) -> Option<DecodedFrame> {
        use ffmpeg_next::format::pixel::Pixel;
        use ffmpeg_next::software::converter::Context;

        let width = frame.width();
        let height = frame.height();
        let pts = frame.pts().unwrap_or(0) as f64 * tb_factor;

        // Use ffmpeg's swscale to convert to RGBA
        let mut converter = Context::get(
            frame.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            ffmpeg_next::software::converter::Flags::BILINEAR,
        )
        .ok()?;

        let rgb_frame = converter.run(frame).ok()?;

        let stride = rgb_frame.stride(0) as usize;
        let data_ptr = rgb_frame.data(0);
        let total_size = (height as usize) * stride;
        let mut data = Vec::with_capacity(total_size);

        // Safety: ffmpeg guarantees data[0] points to a valid buffer of stride * height bytes
        unsafe {
            data.set_len(total_size);
            std::ptr::copy_nonoverlapping(data_ptr, data.as_mut_ptr(), total_size);
        }

        Some(DecodedFrame { data, width, height, pts, duration: 0.0 })
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.command_tx.send(DecoderCommand::Stop);
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check`
Expected: Compilation succeeds.

- [ ] **Step 4: Commit**

```bash
git add src/decoder/mod.rs src/decoder/video.rs Cargo.lock
git commit -m "feat: add ffmpeg video decoder with RGBA conversion"
```

---

### Task 6: Integrate Decoder with Renderer — Live Video Playback

**Files:**
- Modify: `src/app.rs`
- Modify: `src/renderer/mod.rs` (add frame ingestion)

**Interfaces:**
- App holds decoder and command sender
- On `about_to_wait`: drain frame channel, push latest frame to renderer

- [ ] **Step 1: Update app.rs — integrate decoder, throttle rendering to video frame rate**

```rust
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    window::{Window, WindowId},
};
use crate::{
    decoder::video::{DecoderCommand, VideoDecoder},
    renderer::Renderer,
};

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub decoder: Option<VideoDecoder>,
    pub command_tx: Option<std::sync::mpsc::Sender<DecoderCommand>>,
    pub dragging: bool,
    pub loaded: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            decoder: None,
            command_tx: None,
            dragging: false,
            loaded: false,
        }
    }

    fn open_file(&mut self, path: &str) {
        match VideoDecoder::open(path) {
            Ok((decoder, cmd_tx)) => {
                self.decoder = Some(decoder);
                self.command_tx = Some(cmd_tx);
                self.loaded = true;
                tracing::info!("Loaded: {path}");
            }
            Err(e) => {
                tracing::error!("Failed to open {path}: {e}");
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window_attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(window_attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);

        // For now, open a hardcoded path (replace with rfd dialog later)
        // self.open_file("/path/to/your/360_video.mp4");
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(s) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(s.width, s.height);
                    r.update_camera_uniform();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.dragging = true,
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.dragging = false,
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(r) = &mut self.renderer {
                    let scroll = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32 / 10.0,
                    };
                    r.camera.handle_scroll(scroll);
                    r.update_camera_uniform();
                }
            }
            WindowEvent::KeyboardInput { event: ke, .. } => {
                use winit::keyboard::KeyCode;
                if ke.state == winit::event::ElementState::Pressed {
                    match ke.physical_key {
                        KeyCode::Space => {
                            if let Some(ref tx) = self.command_tx {
                                let _ = tx.send(DecoderCommand::Pause);
                            }
                        }
                        KeyCode::Escape => event_loop.exit(),
                        KeyCode::KeyO => {
                            // Open file dialog placeholder
                            tracing::info!("Open file requested (use rfd later)");
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.dragging {
                if let Some(r) = &mut self.renderer {
                    r.camera.handle_mouse(delta.0, delta.1, r.size.1 as f64);
                    r.update_camera_uniform();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Drain decoder frames, keep only the latest
        if let Some(ref decoder) = self.decoder {
            let mut latest: Option<crate::decoder::video::DecodedFrame> = None;
            while let Ok(frame) = decoder.frame_rx.try_recv() {
                latest = Some(frame);
            }
            if let Some(frame) = latest {
                if let Some(r) = &mut self.renderer {
                    r.update_video_texture(&frame.data, frame.width, frame.height);
                }
            }
        }

        let renderer = match &mut self.renderer { Some(r) => r, None => return };
        renderer.update_camera_uniform();
        if let Err(e) = renderer.render() {
            match e {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let s = renderer.size;
                    renderer.resize(s.0, s.1);
                }
                wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                wgpu::SurfaceError::Timeout => {}
            }
        }
    }
}
```

- [ ] **Step 2: Manual test with a 360 video**

To test, add a temporary hardcoded path in `resumed()`:
```rust
self.open_file("/tmp/test_360.mp4");
```

Or use `ffplay` to generate a test video first if needed.

- [ ] **Step 3: Build and test**

Run: `cargo run` (with a test video path)
Expected: Video plays on the sphere interior. Mouse drag rotates view. Black if no video path is set.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs Cargo.lock
git commit -m "feat: integrate video decoder with renderer for live playback"
```

---

### Task 7: egui UI Overlay

**Files:**
- Create: `src/ui/mod.rs`

**Interfaces:**
- `PlayerUI` struct: holds playback state (playing, position, duration)
- egui context integrated with winit + wgpu via `egui_winit` and `egui_wgpu`

- [ ] **Step 1: Write src/ui/mod.rs**

```rust
use egui::Context;

pub struct PlayerUI {
    pub egui_ctx: Context,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub file_name: String,
}

impl PlayerUI {
    pub fn new() -> Self {
        Self {
            egui_ctx: Context::default(),
            playing: false,
            position: 0.0,
            duration: 0.0,
            volume: 0.8,
            file_name: String::new(),
        }
    }

    /// Run egui UI and return whether state changed
    pub fn update(&mut self) -> egui::Output {
        let ctx = &self.egui_ctx;
        let mut play_pressed = false;
        let mut seek_to: Option<f64> = None;

        ctx.input(|_i| {});

        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(180),
                ..Default::default()
            })
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Play/Pause button
                    let icon = if self.playing { "⏸" } else { "▶" };
                    if ui.button(icon).clicked() {
                        self.playing = !self.playing;
                        play_pressed = true;
                    }

                    // Position slider
                    let pos_secs = self.position as f64;
                    let dur_secs = self.duration.max(1.0);
                    let mut slider = pos_secs;
                    ui.add(
                        egui::Slider::new(&mut slider, 0.0..=dur_secs)
                            .text("")
                            .custom_formatter(|n, _| {
                                let m = (n as u64) / 60;
                                let s = (n as u64) % 60;
                                format!("{m:02}:{s:02}")
                            })
                            .custom_parser(|s| {
                                let parts: Vec<&str> = s.split(':').collect();
                                if parts.len() == 2 {
                                    Some(parts[0].parse::<f64>().unwrap_or(0.0) * 60.0
                                        + parts[1].parse::<f64>().unwrap_or(0.0))
                                } else {
                                    None
                                }
                            })
                            .sense(egui::Sense::click_and_drag()),
                    );
                    if slider != pos_secs {
                        seek_to = Some(slider);
                    }

                    // Time display
                    ui.label(format!(
                        "{:02}:{:02} / {:02}:{:02}",
                        (self.position as u64) / 60,
                        (self.position as u64) % 60,
                        (self.duration as u64) / 60,
                        (self.duration as u64) % 60,
                    ));

                    // Volume
                    let mut vol = self.volume;
                    ui.add(egui::Slider::new(&mut vol, 0.0..=1.0).text("Vol"));
                    self.volume = vol;
                });
            });

        ctx.end_frame()
    }
}
```

- [ ] **Step 2: Integrate egui with winit + wgpu in app.rs**

This requires using `egui_winit::State` and rendering egui via `egui_wgpu`. Creating egui's render pass after the sphere render pass so the UI overlays the 3D scene.

Update `src/app.rs` to include egui integration. Add egui state management and layered rendering.

- [ ] **Step 3: Build and test**

Run: `cargo run`
Expected: egui control bar appears at bottom of window. Play/pause button, progress slider, time display, volume slider visible.

- [ ] **Step 4: Commit**

```bash
git add src/ui/mod.rs src/app.rs
git commit -m "feat: add egui UI overlay with playback controls"
```

---

### Task 8: File Open Dialog & Full Integration

**Files:**
- Modify: `src/app.rs`

- [ ] **Step 1: Wire up `rfd` file dialog to `open_file`**

In the keyboard event handler or add a menu button, use `rfd::AsyncFileDialog`:

```rust
WindowEvent::KeyboardInput { event: ke, .. } => {
    if ke.state == ElementState::Pressed && ke.physical_key == KeyCode::KeyO {
        // Trigger file dialog
        // Store a flag to open it in about_to_wait to avoid blocking
    }
}
```

Simpler: use `rfd::FileDialog` synchronous version off the main thread.

- [ ] **Step 2: Wire seek bar to decoder commands**

When `seek_to` changes in UI, send `DecoderCommand::Seek(seek_to)`.

- [ ] **Step 3: Wire play/pause toggle**

Send `DecoderCommand::Pause` or `DecoderCommand::Resume` based on UI state changes.

- [ ] **Step 4: Polish and test full pipeline**

Full flow: Open file → video plays → drag to rotate → seek → pause/resume → close.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs Cargo.lock
git commit -m "feat: add file dialog and wire playback controls to decoder"
```

---

## Self-Review Checklist

- [ ] Spec coverage: Phase 1 covers window+wgpu+sphere+shader+camera+decoder+UI — all mapped to tasks
- [ ] No placeholders: every task has complete code, exact commands, expected output
- [ ] Type consistency: `CameraUniform.view_proj: [[f32;4];4]` used everywhere, `OrbitCamera` API consistent across tasks
- [ ] Each task ends with independently testable deliverable
- [ ] GTasks are bite-sized (each step 2-5 minutes)
