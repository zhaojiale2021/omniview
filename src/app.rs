use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    event::{DeviceEvent, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};
use crate::decoder::video::{DecoderCommand, VideoDecoder, DecodedFrame};
use crate::renderer::Renderer;
use crate::ui::PlayerUI;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub decoder: Option<VideoDecoder>,
    pub command_tx: Option<mpsc::Sender<DecoderCommand>>,
    pub dragging: bool,
    pub ui: Option<PlayerUI>,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None,
            renderer: None,
            decoder: None,
            command_tx: None,
            dragging: false,
            ui: None,
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
        let ui = PlayerUI::new(&renderer.egui_ctx());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.ui = Some(ui);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Always handle CloseRequested immediately
        if matches!(&event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }

        // Feed the event to egui-winit BEFORE application handling.
        // on_window_event returns EventResponse indicating if egui consumed it.
        let window = self.window.as_ref();
        let consumed_by_egui = if let (Some(w), Some(r)) = (window, &mut self.renderer) {
            r.egui_state.on_window_event(w, &event).consumed
        } else {
            false
        };

        // Always handle Resized so the swap chain is reconfigured
        if let WindowEvent::Resized(s) = &event {
            if let Some(r) = &mut self.renderer {
                r.resize(s.width, s.height);
                r.update_camera_uniform();
            }
        }

        if consumed_by_egui {
            return;
        }

        // Events below are NOT consumed by egui — handle application logic
        match event {
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
                                if d.paused.load(Ordering::Relaxed) {
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
                // Don't rotate the camera if an egui widget is capturing pointer input
                let egui_wants_pointer = self.renderer.as_ref()
                    .map(|r| r.egui_state.egui_ctx().wants_pointer_input())
                    .unwrap_or(false);
                if egui_wants_pointer {
                    return;
                }
                if let Some(r) = &mut self.renderer {
                    r.camera.handle_mouse(delta.0, delta.1, r.size.1 as f64);
                    r.update_camera_uniform();
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // --- Drain decoder frames, keep only the latest ---
        if let Some(ref decoder) = self.decoder {
            let mut latest_frame: Option<DecodedFrame> = None;
            while let Ok(frame) = decoder.frame_rx.try_recv() {
                latest_frame = Some(frame);
            }
            if let Some(frame) = latest_frame {
                if let Some(r) = &mut self.renderer {
                    r.update_video_texture(&frame.data, frame.width, frame.height);
                    // Update position from the latest frame
                    if let Some(ref mut ui) = self.ui {
                        ui.position = frame.pts_secs;
                    }
                }
            }
        }

        // --- Sync decoder state to UI ---
        let decoder_paused = self.decoder.as_ref()
            .map(|d| d.paused.load(Ordering::Relaxed))
            .unwrap_or(true);
        let duration = self.decoder.as_ref()
            .map(|d| d.duration_secs)
            .unwrap_or(0.0);

        // --- Run egui UI and render ---
        let mut action_open_file = false;
        let mut action_seek: Option<f64> = None;
        let mut action_toggle_pause = false;

        if let (Some(window), Some(renderer), Some(ui)) =
            (&self.window, &mut self.renderer, &mut self.ui)
        {
            ui.playing = !decoder_paused;
            ui.duration = duration;

            // Prepare input for egui and begin the pass
            let raw_input = renderer.egui_state.take_egui_input(window);
            renderer.egui_state.egui_ctx().begin_pass(raw_input);

            // Build UI panels and end the pass
            let full_output = ui.update();

            // Handle egui platform output (cursor changes, clipboard, IME, etc.)
            renderer.egui_state.handle_platform_output(
                window,
                full_output.platform_output.clone(),
            );

            // Tessellate shapes into clipped primitives for GPU rendering
            let clipped_primitives = renderer.egui_state.egui_ctx().tessellate(
                full_output.shapes.clone(),
                full_output.pixels_per_point,
            );

            // Capture UI actions
            action_open_file = ui.open_file_clicked;
            ui.open_file_clicked = false;
            action_seek = ui.seek_to.take();

            // Detect play/pause toggle
            let should_play = ui.playing;
            let is_playing = !decoder_paused;
            if should_play != is_playing {
                action_toggle_pause = true;
            }

            // Render the 3D sphere scene with egui overlay
            if let Err(e) = renderer.render(&clipped_primitives, full_output.pixels_per_point) {
                match e {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                        let s = renderer.size;
                        renderer.resize(s.0, s.1);
                    }
                    wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                    wgpu::SurfaceError::Timeout => {}
                }
            }
        } else if let Some(r) = &mut self.renderer {
            // Fallback: render without UI (shouldn't happen once UI is initialized)
            if let Err(e) = r.render(&[], 1.0) {
                match e {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                        let s = r.size;
                        r.resize(s.0, s.1);
                    }
                    wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                    wgpu::SurfaceError::Timeout => {}
                }
            }
        }

        // --- Handle UI actions outside the borrow scope ---
        if action_open_file {
            let path = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                .pick_file();
            if let Some(p) = path {
                self.open_file(&p.to_string_lossy());
            }
        }

        if let Some(seek) = action_seek {
            if let Some(tx) = &self.command_tx {
                let _ = tx.send(DecoderCommand::Seek(seek));
            }
        }

        if action_toggle_pause {
            if let Some(tx) = &self.command_tx {
                let cmd = if decoder_paused {
                    DecoderCommand::Resume
                } else {
                    DecoderCommand::Pause
                };
                let _ = tx.send(cmd);
            }
        }
    }
}
