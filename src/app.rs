use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, KeyEvent, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use crate::player::Player;
use crate::renderer::Renderer;
use crate::ui::PlayerUI;

pub struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    player: Player,
    ui: Option<PlayerUI>,
    dragging: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    /// Any window event arrived since the last `about_to_wait` —
    /// forces a render even when paused (UI interaction must be drawn).
    input_seen: bool,
    /// File to open once the window is ready.
    pending_file: Option<String>,
}

impl App {
    pub fn new(initial_file: Option<String>) -> Self {
        Self {
            window: None,
            renderer: None,
            player: Player::new(),
            ui: None,
            dragging: false,
            last_cursor: None,
            input_seen: false,
            pending_file: initial_file,
        }
    }

    fn open_file(&mut self, path: &str) {
        if let Err(e) = self.player.open(path) {
            tracing::error!("Open failed: {e}");
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Media Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        let ui = PlayerUI::new(&renderer.egui_ctx());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.ui = Some(ui);

        // Open file from command line if provided
        if let Some(ref path) = self.pending_file.take() {
            self.open_file(path);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.input_seen = true;

        // ── Lifecycle ──────────────────────────────────────────
        if matches!(&event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }

        // ── Resize ─────────────────────────────────────────────
        if let WindowEvent::Resized(s) = &event {
            if let Some(r) = &mut self.renderer {
                r.resize(s.width, s.height);
                r.update_camera_uniform();
            }
        }

        // ── Mouse input BEFORE egui (so drag works even over UI) ──
        if let WindowEvent::MouseInput { state, button: MouseButton::Left, .. } = &event {
            self.dragging = *state == ElementState::Pressed;
            if !self.dragging {
                self.last_cursor = None;
            }
        }

        // ── Cursor movement BEFORE egui ────────────────────────
        if let WindowEvent::CursorMoved { position, .. } = &event {
            if self.dragging {
                if let Some(r) = &mut self.renderer {
                    if r.is_360 {
                        let dx = position.x - self.last_cursor.map(|p| p.x).unwrap_or(position.x);
                        let dy = position.y - self.last_cursor.map(|p| p.y).unwrap_or(position.y);
                        r.camera.handle_mouse(dx, dy, r.size.0 as f64);
                        r.update_camera_uniform();
                    }
                }
            }
            self.last_cursor = Some(*position);
        }

        // ── egui input ─────────────────────────────────────────
        let consumed = if let (Some(w), Some(r)) = (&self.window, &mut self.renderer) {
            r.egui_state.on_window_event(w, &event).consumed
        } else {
            false
        };

        if consumed {
            return;
        }

        // ── Keyboard shortcuts ─────────────────────────────────
        match event {
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key: PhysicalKey::Code(kc), state: ElementState::Pressed, .. },
                ..
            } => match kc {
                KeyCode::KeyO => {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                        .pick_file()
                    {
                        self.open_file(&p.to_string_lossy());
                    }
                }
                KeyCode::Space => {
                    self.player.play_pause();
                }
                KeyCode::Escape => event_loop.exit(),
                _ => {}
            },
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, _event: winit::event::DeviceEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // ── Receive latest video frame ──────────────────────────
        // The upload happens inside render() (after surface acquire),
        // so a failing surface can't leak staging buffers.
        let mut frame_data: Option<(std::sync::Arc<Vec<u8>>, u32, u32)> = None;
        let frame_uploaded = if let Some(frame) = self.player.try_recv_frame() {
            frame_data = Some((frame.data, frame.width, frame.height));
            true
        } else {
            false
        };

        // ── Sync UI state from player (skip fields driven by UI) ──
        let seeking = self.ui.as_ref().map(|u| u.seeking).unwrap_or(false);
        if let Some(ref mut ui) = self.ui {
            if !seeking {
                ui.position = self.player.clock();
            }
            ui.duration = self.player.duration();
            ui.playing = self.player.is_playing();
            ui.speed = self.player.speed();
        }

        let paused = self.player.is_paused();

        // ── Gather UI actions ────────────────────────────────────
        let mut open_action = false;
        let mut seek_action: Option<f64> = None;
        let mut pause_action = false;
        let mut speed_action: Option<f64> = None;
        let mut volume_action: Option<f32> = None;

        let renderer = &mut self.renderer;
        if let (Some(w), Some(r), Some(ui)) = (&self.window, renderer, &mut self.ui) {
            r.is_360 = ui.is_360;

            let raw = r.egui_state.take_egui_input(w);
            r.egui_state.egui_ctx().begin_pass(raw);
            let out = ui.update();
            r.egui_state.handle_platform_output(w, out.platform_output.clone());
            let prims = r.egui_state
                .egui_ctx()
                .tessellate(out.shapes.clone(), out.pixels_per_point);

            open_action = ui.open_file_clicked;
            ui.open_file_clicked = false;
            seek_action = ui.seek_to.take();
            if ui.speed_changed {
                ui.speed_changed = false;
                speed_action = Some(ui.speed);
            }
            if ui.volume_changed {
                ui.volume_changed = false;
                volume_action = Some(ui.volume);
            }
            if ui.playing == paused {
                pause_action = true;
            }

            // ── Render (unless paused and nothing changed) ──────
            // Playback advancing, UI interaction, egui needing a
            // repaint, or a fresh frame all keep the loop at Poll; a
            // static paused screen drops to Wait and renders on
            // demand (near-zero CPU).
            let interactive = self.input_seen
                || self.dragging
                || ui.seeking
                || r.egui_state.egui_ctx().has_requested_repaint();
            let at_end = self.player.duration() > 0.0
                && self.player.clock() >= self.player.duration() - 0.05;
            if (!paused && !at_end) || interactive || frame_uploaded {
                _event_loop.set_control_flow(ControlFlow::Poll);
                if let Err(e) = r.render(&prims, &out.textures_delta, out.pixels_per_point, frame_data.take()) {
                    match e {
                        wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                            let s = r.size;
                            r.resize(s.0, s.1);
                        }
                        wgpu::SurfaceError::OutOfMemory => tracing::error!("GPU OOM"),
                        wgpu::SurfaceError::Timeout => {}
                    }
                }
            } else {
                _event_loop.set_control_flow(ControlFlow::Wait);
            }
        } else if let Some(r) = &mut self.renderer {
            if let Err(e) = r.render(&[], &egui::TexturesDelta::default(), 1.0, None) {
                match e {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                        let s = r.size;
                        r.resize(s.0, s.1);
                    }
                    wgpu::SurfaceError::OutOfMemory => {}
                    wgpu::SurfaceError::Timeout => {}
                }
            }
        }

        self.input_seen = false;

        // ── Apply actions ────────────────────────────────────────
        if open_action {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                .pick_file()
            {
                self.open_file(&p.to_string_lossy());
            }
        }
        if let Some(pos) = seek_action {
            self.player.seek(pos);
        }
        if pause_action {
            self.player.play_pause();
        }
        if let Some(spd) = speed_action {
            self.player.set_speed(spd);
        }
        if let Some(vol) = volume_action {
            self.player.set_volume(vol);
        }
    }
}
