use std::sync::Arc;
use wgpu::{PresentMode, TextureUsages};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::media::types::VideoFrame;

pub mod sphere;
pub mod camera;
pub mod quad;
use camera::OrbitCamera;
use quad::{Quad, QuadVertex};
use sphere::{Sphere, Vertex};

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
}

pub struct Renderer {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    pub size: (u32, u32),
    pub sphere: Sphere,
    pub render_pipeline: wgpu::RenderPipeline,
    pub quad: Quad,
    pub quad_pipeline: wgpu::RenderPipeline,
    pub is_360: bool,
    pub camera_buffer: wgpu::Buffer,
    pub camera_bind_group: wgpu::BindGroup,
    pub texture_sampler: wgpu::Sampler,
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    pub y_texture: Option<wgpu::Texture>,
    pub y_texture_view: Option<wgpu::TextureView>,
    pub uv_texture: Option<wgpu::Texture>,
    pub uv_texture_view: Option<wgpu::TextureView>,
    pub video_bind_group: Option<wgpu::BindGroup>,
    pub placeholder_bind_group: wgpu::BindGroup,
    pub camera: OrbitCamera,
    /// Uploaded stride (bytes per row) of the current Y plane.
    y_stride: u32,
    /// Uploaded stride (bytes per row) of the current UV plane.
    uv_stride: u32,
    /// When the last frame was presented — vsync-phase estimate.
    last_present: Option<std::time::Instant>,
    /// Estimated vsync period in seconds (EWMA of present intervals).
    vsync_period: f64,
    pub egui_state: egui_winit::State,
    pub egui_renderer: egui_wgpu::Renderer,

    // ── Debug self-capture (env CAPTURE_PNG=<path>) ─────────────
    // Reads the backbuffer back to CPU every ~20 frames and writes a
    // PPM at <path>_<frame>.ppm, so the UI can be verified without a
    // screen grabber.  No-op when the env var is unset.
    capture_path: Option<String>,
    capture_staging: Option<wgpu::Buffer>,
    capture_counter: u32,
    /// Buffer used for on-demand screenshot readbacks.
    png_staging: Option<wgpu::Buffer>,
}

impl Renderer {
    pub async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .unwrap();
        let adapter_info = adapter.get_info();
        tracing::info!(
            "GPU adapter: {} | backend={:?} | type={:?}",
            adapter_info.name,
            adapter_info.backend,
            adapter_info.device_type
        );
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
        // Debug self-capture: needs COPY_SRC on the surface texture.
        let capture_path = std::env::var("CAPTURE_PNG").ok();
        let mut usage = TextureUsages::RENDER_ATTACHMENT;
        if capture_path.is_some() && caps.usages.contains(TextureUsages::COPY_SRC) {
            usage |= TextureUsages::COPY_SRC;
        }
        let config = wgpu::SurfaceConfiguration {
            usage,
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
        let quad = Quad::new(&device);

        let sphere_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Equirect Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("equirect.wgsl").into()),
        });

        // Camera
        let camera = OrbitCamera::new();
        let aspect = size.width as f32 / size.height.max(1) as f32;
        let initial_vp = camera.view_proj_matrix(aspect);

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
                resource: camera_buffer.as_entire_binding(),
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

        // Texture bind group layout: Y plane (R8), UV plane (Rg8), sampler.
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

        // Placeholder 1x1 textures (Y=255, Cb=128, Cr=128 → white).
        let placeholder_y = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Placeholder"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let placeholder_uv = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Placeholder UV"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &placeholder_y,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8, 255, 255, 255],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &placeholder_uv,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[128u8, 128],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(2),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let placeholder_y_view = placeholder_y.create_view(&Default::default());
        let placeholder_uv_view = placeholder_uv.create_view(&Default::default());
        let placeholder_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Placeholder BG"),
            layout: &texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&placeholder_y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&placeholder_uv_view),
                },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&texture_sampler) },
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
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Quad pipeline
        let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Quad Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
        });
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Quad Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &quad_shader,
                entry_point: "vs_main",
                buffers: &[QuadVertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &quad_shader,
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
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        tracing::info!("Render pipeline created");

        // egui
        let egui_ctx = egui::Context::default();
        let viewport_id = egui::ViewportId::ROOT;
        let egui_state = egui_winit::State::new(
            egui_ctx, viewport_id, window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(), None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(
            &device, config.format, None, 1, false,
        );

        let capture_staging = if capture_path.is_some() {
            Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Capture Staging"),
                size: (config.width as u64) * (config.height as u64) * 4,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }))
        } else {
            None
        };

        Self {
            surface, device, queue, config, size: (size.width, size.height),
            sphere, render_pipeline, quad, quad_pipeline, is_360: false,
            camera_buffer, camera_bind_group,
            texture_sampler, texture_bind_group_layout: texture_bgl,
            y_texture: None, y_texture_view: None,
            uv_texture: None, uv_texture_view: None,
            video_bind_group: None,
            placeholder_bind_group, camera,
            y_stride: 0, uv_stride: 0,
            last_present: None,
            vsync_period: 1.0 / 60.0,
            egui_state, egui_renderer,
            capture_path,
            capture_staging,
            capture_counter: 0,
            png_staging: None,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        self.config.width = self.size.0;
        self.config.height = self.size.1;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn update_camera(&mut self, view_proj: &[[f32; 4]; 4]) {
        self.queue.write_buffer(
            &self.camera_buffer, 0,
            bytemuck::cast_slice(&[CameraUniform { view_proj: *view_proj }]),
        );
    }

    pub fn update_camera_uniform(&mut self) {
        // Skip the per-frame `write_buffer` when nothing moved: the render
        // loop calls this every frame, but the uniform only changes when
        // the camera or the window aspect ratio changes.
        if !self.camera.dirty {
            return;
        }
        self.camera.dirty = false;
        let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
        let vp = self.camera.view_proj_matrix(aspect);
        self.update_camera(&vp);
    }

    /// Estimated seconds until the next vsync.  A texture uploaded now is
    /// presented at that vsync, so the render loop uses this as the
    /// media-time lookahead for frame selection (stable 2-vsync cadence for
    /// 30 fps content on 60 Hz displays).
    pub fn next_vsync_in(&self) -> f64 {
        let since = self
            .last_present
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        (self.vsync_period - since).max(0.0)
    }

    /// Upload an NV12 video frame into the GPU textures.  YUV→RGB is left
    /// to the fragment shader, so the CPU only copies planar data (about
    /// half the bytes of RGBA) and never runs an RGB conversion matrix.
    pub fn upload_video_frame(&mut self, frame: &VideoFrame) {
        let (width, height) = (frame.width, frame.height);
        if width == 0 || height == 0 {
            return;
        }
        let uv_w = width.div_ceil(2);
        let uv_h = height.div_ceil(2);

        // Ensure the video textures exist and are the correct size.
        let needs_new = match &self.y_texture {
            Some(t) => t.width() != width || t.height() != height,
            None => true,
        };
        if needs_new {
            let y_tex_size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
            let y_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Video Y Plane"),
                size: y_tex_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                // COPY_SRC lets screenshots read the frame back.
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let uv_tex_size = wgpu::Extent3d { width: uv_w, height: uv_h, depth_or_array_layers: 1 };
            let uv_texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Video UV Plane"),
                size: uv_tex_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rg8Unorm,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST | TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let y_view = y_texture.create_view(&Default::default());
            let uv_view = uv_texture.create_view(&Default::default());
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Video BG"),
                layout: &self.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.texture_sampler),
                    },
                ],
            });
            self.y_texture = Some(y_texture);
            self.y_texture_view = Some(y_view);
            self.uv_texture = Some(uv_texture);
            self.uv_texture_view = Some(uv_view);
            self.video_bind_group = Some(bind_group);
        }

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: self.y_texture.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.y,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(frame.y_stride),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: self.uv_texture.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &frame.uv,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(frame.uv_stride),
                rows_per_image: Some(uv_h),
            },
            wgpu::Extent3d { width: uv_w, height: uv_h, depth_or_array_layers: 1 },
        );
        self.y_stride = frame.y_stride;
        self.uv_stride = frame.uv_stride;
    }

    /// Render the video frame + egui overlay.
    ///
    /// `frame` is the freshly decoded frame to display, if any.  The
    /// texture upload happens only AFTER the surface is acquired, so a
    /// `write_texture` is always followed by a `submit` — a failing
    /// surface can't leak staging buffers into `pending_writes`.
    pub fn render(
        &mut self,
        clipped_primitives: &[egui::ClippedPrimitive],
        textures_delta: &egui::TexturesDelta,
        pixels_per_point: f32,
        frame: Option<VideoFrame>,
    ) -> Result<(), wgpu::SurfaceError> {
        self.update_camera_uniform();
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Encoder") });

        if let Some(frame) = frame {
            self.upload_video_frame(&frame);
        }

        // Upload egui textures
        for (id, image_delta) in &textures_delta.set {
            self.egui_renderer.update_texture(&self.device, &self.queue, *id, image_delta);
        }
        for id in &textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        // egui vertex/index buffers
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.0, self.size.1],
            pixels_per_point,
        };
        let extra_cbs = self.egui_renderer.update_buffers(
            &self.device, &self.queue, &mut encoder, clipped_primitives, &screen_descriptor,
        );

        // ── Main video pass ──────────────────────────────────────
        {
            let texture_bg = self.video_bind_group.as_ref().unwrap_or(&self.placeholder_bind_group);
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Main Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rpass.set_bind_group(0, &self.camera_bind_group, &[]);
            rpass.set_bind_group(1, texture_bg, &[]);
            if self.is_360 {
                rpass.set_pipeline(&self.render_pipeline);
                rpass.set_vertex_buffer(0, self.sphere.vertex_buffer.slice(..));
                rpass.set_index_buffer(self.sphere.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                rpass.draw_indexed(0..self.sphere.index_count, 0, 0..1);
            } else {
                rpass.set_pipeline(&self.quad_pipeline);
                rpass.set_vertex_buffer(0, self.quad.vertex_buffer.slice(..));
                rpass.set_index_buffer(self.quad.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                rpass.draw_indexed(0..self.quad.index_count, 0, 0..1);
            }
        }

        // ── egui overlay ─────────────────────────────────────────
        if !clipped_primitives.is_empty() {
            let rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Egui Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.egui_renderer.render(
                &mut rpass.forget_lifetime(),
                clipped_primitives,
                &screen_descriptor,
            );
        }

        // Debug self-capture: copy the backbuffer into a staging buffer
        // every ~20 frames, read it back after present, save a PPM.
        let capture_now = self.capture_path.is_some()
            && self.capture_staging.is_some()
            && {
                self.capture_counter += 1;
                self.capture_counter.is_multiple_of(20)
            };
        if capture_now
            && let (Some(staging), Some(_)) = (&self.capture_staging, &self.capture_path) {
                encoder.copy_texture_to_buffer(
                    wgpu::ImageCopyTexture {
                        texture: &output.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::ImageCopyBuffer {
                        buffer: staging,
                        layout: wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(4 * self.size.0),
                            rows_per_image: Some(self.size.1),
                        },
                    },
                    wgpu::Extent3d {
                        width: self.size.0,
                        height: self.size.1,
                        depth_or_array_layers: 1,
                    },
                );
            }

        self.queue.submit(extra_cbs.into_iter().chain(std::iter::once(encoder.finish())));
        output.present();

        // Track the present cadence to estimate the vsync period.
        // present() returns at (or just after) a vsync, so consecutive
        // intervals approximate the display refresh.
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_present {
            let dt = now.duration_since(prev).as_secs_f64();
            if dt > 0.004 && dt < 0.050 {
                self.vsync_period = self.vsync_period * 0.8 + dt * 0.2;
            }
        }
        self.last_present = Some(now);

        if capture_now
            && let (Some(staging), Some(path)) = (&self.capture_staging, &self.capture_path) {
                let slice = staging.slice(..);
                let (tx, rx) = std::sync::mpsc::channel();
                slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
                self.device.poll(wgpu::Maintain::Wait);
                if rx.recv().is_ok() {
                    let data = slice.get_mapped_range();
                    let (w, h) = (self.size.0, self.size.1);
                    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
                    for px in data.chunks_exact(4) {
                        ppm.extend_from_slice(&px[..3]);
                    }
                    let p = format!("{path}_{}", self.capture_counter);
                    let _ = std::fs::write(&p, ppm);
                    tracing::info!("Captured UI frame to {p}");
                }
                staging.unmap();
            }

        Ok(())
    }

    pub fn egui_ctx(&self) -> egui::Context {
        self.egui_state.egui_ctx().clone()
    }

    /// Save the current video frame (no UI) as a PNG.  Returns false if
    /// there is no frame yet.  Blocks briefly for the GPU readback.
    pub fn save_frame_png(&mut self, path: &str) -> bool {
        let (Some(y_tex), Some(uv_tex)) = (&self.y_texture, &self.uv_texture) else {
            return false;
        };
        let (w, h) = (y_tex.width(), y_tex.height());
        let uv_h = uv_tex.height();
        let y_stride = self.y_stride.max(w.div_ceil(256) * 256);
        let uv_stride = self.uv_stride.max((uv_tex.width() * 2).div_ceil(256) * 256);
        let y_size = y_stride as u64 * h as u64;
        let uv_size = uv_stride as u64 * uv_h as u64;
        let total = y_size + uv_size;

        if self.png_staging.as_ref().map(|b| b.size() != total).unwrap_or(true) {
            self.png_staging = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("PNG Staging"),
                size: total,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            }));
        }
        let staging = self.png_staging.as_ref().unwrap();

        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("PNG Encoder"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: y_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(y_stride),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: uv_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: staging,
                layout: wgpu::ImageDataLayout {
                    offset: y_size,
                    bytes_per_row: Some(uv_stride),
                    rows_per_image: Some(uv_h),
                },
            },
            wgpu::Extent3d { width: uv_tex.width(), height: uv_h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        self.device.poll(wgpu::Maintain::Wait);
        if rx.recv().is_err() {
            return false;
        }
        let data = slice.get_mapped_range();
        let y = &data[..y_size as usize];
        let uv = &data[y_size as usize..];
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for row in 0..h {
            for col in 0..w {
                let yv = y[row as usize * y_stride as usize + col as usize] as f32 / 255.0;
                let u = uv[(row / 2) as usize * uv_stride as usize + (col / 2) as usize * 2] as f32
                    / 255.0;
                let v = uv[(row / 2) as usize * uv_stride as usize + (col / 2) as usize * 2 + 1]
                    as f32
                    / 255.0;
                let y2 = (yv - 16.0 / 255.0) * (255.0 / 219.0);
                let u2 = (u - 128.0 / 255.0) * (255.0 / 224.0);
                let v2 = (v - 128.0 / 255.0) * (255.0 / 224.0);
                let r = (y2 + 1.5748 * v2).clamp(0.0, 1.0) * 255.0;
                let g = (y2 - 0.1873 * u2 - 0.4681 * v2).clamp(0.0, 1.0) * 255.0;
                let b = (y2 + 1.8556 * u2).clamp(0.0, 1.0) * 255.0;
                rgba.extend_from_slice(&[r as u8, g as u8, b as u8, 255]);
            }
        }
        let ok = image::save_buffer(path, &rgba, w, h, image::ColorType::Rgba8).is_ok();
        drop(data);
        staging.unmap();
        ok
    }
}
