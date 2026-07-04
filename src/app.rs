use std::sync::mpsc;
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};
use crate::decoder::video::{DecoderCommand, VideoDecoder};
use crate::renderer::Renderer;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub decoder: Option<VideoDecoder>,
    pub command_tx: Option<mpsc::Sender<DecoderCommand>>,
    pub dragging: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            decoder: None,
            command_tx: None,
            dragging: false,
        }
    }

    pub fn open_file(&mut self, path: &str) {
        // Clean up previous decoder
        if let Some(d) = self.decoder.take() {
            d.stop();
        }
        self.command_tx.take();

        match VideoDecoder::open(path) {
            Ok((decoder, cmd_tx)) => {
                self.decoder = Some(decoder);
                self.command_tx = Some(cmd_tx);
                tracing::info!("Loaded: {path}");

                // Update renderer with video dimensions
                if let Some(r) = &mut self.renderer {
                    // video loaded, renderer will show frames when they arrive
                }
            }
            Err(e) => {
                tracing::error!("Failed to open video: {e}");
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
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(key_code),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                match key_code {
                    KeyCode::KeyO => {
                        // Open file via rfd dialog
                        if self.window.is_some() {
                            let path = rfd::FileDialog::new()
                                .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                                .pick_file();
                            if let Some(p) = path {
                                self.open_file(&p.to_string_lossy());
                            }
                        }
                    }
                    KeyCode::Space => {
                        if let Some(tx) = &self.command_tx {
                            if let Some(ref d) = self.decoder {
                                if d.paused.load(std::sync::atomic::Ordering::Relaxed) {
                                    let _ = tx.send(DecoderCommand::Resume);
                                } else {
                                    let _ = tx.send(DecoderCommand::Pause);
                                }
                            }
                        }
                    }
                    KeyCode::Escape => event_loop.exit(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn device_event(
        &mut self,
        _el: &ActiveEventLoop,
        _id: winit::event::DeviceId,
        event: DeviceEvent,
    ) {
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
            let mut latest_frame: Option<crate::decoder::video::DecodedFrame> = None;
            while let Ok(frame) = decoder.frame_rx.try_recv() {
                latest_frame = Some(frame);
            }
            if let Some(frame) = latest_frame {
                if let Some(r) = &mut self.renderer {
                    r.update_video_texture(&frame.data, frame.width, frame.height);
                }
            }
        }

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
