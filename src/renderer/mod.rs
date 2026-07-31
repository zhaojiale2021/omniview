use std::sync::Arc;
use wgpu::{PresentMode, TextureUsages};
use wgpu::util::DeviceExt;
use winit::window::Window;

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

#[allow(dead_code)]
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
    pub quad: Quad,
    pub quad_pipeline: wgpu::RenderPipeline,
    pub is_360: bool,
    pub camera_buffer: wgpu::Buffer,
    pub camera_bind_group: wgpu::BindGroup,
    pub texture_sampler: wgpu::Sampler,
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    pub video_texture: Option<wgpu::Texture>,
    pub video_texture_view: Option<wgpu::TextureView>,
    pub video_bind_group: Option<wgpu::BindGroup>,
    pub placeholder_bind_group: wgpu::BindGroup,
    pub camera: OrbitCamera,
    pub egui_state: egui_winit::State,
    pub egui_renderer: egui_wgpu::Renderer,
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

        // Placeholder 1x1 white texture
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
            wgpu::ImageCopyTexture {
                texture: &placeholder,
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

        Self {
            surface, device, queue, config, size: (size.width, size.height),
            sphere, render_pipeline, quad, quad_pipeline, is_360: false,
            camera_buffer, camera_bind_group,
            texture_sampler, texture_bind_group_layout: texture_bgl,
            video_texture: None, video_texture_view: None, video_bind_group: None,
            placeholder_bind_group, camera,
            egui_state, egui_renderer,
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
        let aspect = self.size.0 as f32 / self.size.1.max(1) as f32;
        let vp = self.camera.view_proj_matrix(aspect);
        self.update_camera(&vp);
    }

    /// Upload a video frame into the GPU texture.  `write_texture`
    /// enqueues a single staging→texture copy through wgpu's internal
    /// ring — no per-frame allocations, no extra hop.
    pub fn upload_video_frame(&mut self, rgba_data: &[u8], width: u32, height: u32) {
        let bytes_needed = (width as usize) * (height as usize) * 4;
        if bytes_needed == 0 { return; }

        // Ensure the video texture exists and is the correct size
        let needs_new = match &self.video_texture {
            Some(t) => t.width() != width || t.height() != height,
            None => true,
        };
        if needs_new {
            let tex_size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
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

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: self.video_texture.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba_data[..bytes_needed],
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
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
        frame: Option<(std::sync::Arc<Vec<u8>>, u32, u32)>,
    ) -> Result<(), wgpu::SurfaceError> {
        self.update_camera_uniform();
        let output = match self.surface.get_current_texture() {
            Ok(o) => o,
            // Surface lost/outdated — skip this frame entirely.  The
            // frame is NOT uploaded, so nothing accumulates.
            Err(e) => return Err(e),
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("Encoder") });

        if let Some((data, width, height)) = frame {
            self.upload_video_frame(&data, width, height);
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

        self.queue.submit(extra_cbs.into_iter().chain(std::iter::once(encoder.finish())));
        output.present();
        Ok(())
    }

    pub fn egui_ctx(&self) -> egui::Context {
        self.egui_state.egui_ctx().clone()
    }
}
