use std::sync::mpsc;
use std::sync::Arc;
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{DeviceEvent, ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};
#[cfg(feature = "mpv")]
use crate::decoder::media::AudioPlayer;
use crate::decoder::video::{DecodedFrame, DecoderCommand, VideoDecoder};
use crate::renderer::Renderer;
use crate::ui::PlayerUI;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub decoder: Option<VideoDecoder>,
    #[cfg(feature = "mpv")]
    pub audio: Option<AudioPlayer>,
    pub command_tx: Option<mpsc::Sender<DecoderCommand>>,
    pub dragging: bool,
    pub ui: Option<PlayerUI>,
    pub current_file: Option<String>,
    pub playback_speed: f64,
    last_cursor: Option<PhysicalPosition<f64>>,
    // ~30 completed video frames; video syncs to audio clock
    frame_count: u64,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None, renderer: None, decoder: None,
            #[cfg(feature = "mpv")] audio: None,
            command_tx: None, dragging: false, ui: None,
            current_file: None, playback_speed: 1.0, last_cursor: None,
            frame_count: 0,
        }
    }

    pub fn open_file(&mut self, path: &str) {
        self.current_file = Some(path.to_string());
        self.frame_count = 0;

        // Stop old
        if let Some(d) = self.decoder.take() { d.stop(); }
        #[cfg(feature = "mpv")]
        drop(self.audio.take());
        self.command_tx.take();

        // Start mpv for audio + clock
        #[cfg(feature = "mpv")]
        match AudioPlayer::open(path) {
            Ok(a) => self.audio = Some(a),
            Err(e) => tracing::warn!("mpv: {e}"),
        }

        // Start ffmpeg for video frames
        match VideoDecoder::open(path, self.playback_speed) {
            Ok((dec, tx)) => {
                self.decoder = Some(dec);
                self.command_tx = Some(tx);
            }
            Err(e) => tracing::error!("Video: {e}"),
        }

        tracing::info!("Loaded: {path}");
    }

    pub fn set_speed(&mut self, speed: f64) {
        if (self.playback_speed - speed).abs() < 0.01 { return; }
        self.playback_speed = speed;

        // Update mpv speed
        #[cfg(feature = "mpv")]
        if let Some(ref a) = self.audio {
            a.set_speed(speed);
        }

        // Restart video decoder with new speed
        if let (Some(ref path), Some(d)) = (self.current_file.clone(), self.decoder.take()) {
            d.stop();
            self.command_tx.take();
            let cur = self.ui.as_ref().map(|u| u.position).unwrap_or(0.0);
            match VideoDecoder::open(path, speed) {
                Ok((dec, tx)) => {
                    if cur > 0.01 { let _ = tx.send(DecoderCommand::Seek(cur)); }
                    self.decoder = Some(dec);
                    self.command_tx = Some(tx);
                }
                Err(e) => tracing::error!("Speed: {e}"),
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("360° Video Player")
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        let ui = PlayerUI::new(&renderer.egui_ctx());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.ui = Some(ui);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if matches!(&event, WindowEvent::CloseRequested) { event_loop.exit(); return; }

        if let WindowEvent::Resized(s) = &event {
            if let Some(r) = &mut self.renderer { r.resize(s.width, s.height); r.update_camera_uniform(); }
        }

        // MouseInput BEFORE egui
        if let WindowEvent::MouseInput { state, button: MouseButton::Left, .. } = &event {
            self.dragging = *state == ElementState::Pressed;
            if !self.dragging { self.last_cursor = None; }
        }

        // CursorMoved BEFORE egui
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

        let consumed = if let (Some(w), Some(r)) = (&self.window, &mut self.renderer) {
            r.egui_state.on_window_event(w, &event).consumed
        } else { false };

        if consumed { return; }

        match event {
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(r) = &mut self.renderer {
                    let s = match delta { MouseScrollDelta::LineDelta(_, y) => y, MouseScrollDelta::PixelDelta(p) => p.y as f32 / 10.0 };
                    r.camera.handle_scroll(s); r.update_camera_uniform();
                }
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { physical_key: PhysicalKey::Code(kc), state: ElementState::Pressed, .. }, ..
            } => match kc {
                KeyCode::KeyO => {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Video", &["mp4","webm","mkv","avi","mov","m4v"]).pick_file() {
                        self.open_file(&p.to_string_lossy());
                    }
                }
                KeyCode::Space => {
                    #[cfg(feature = "mpv")]
                    if let Some(ref a) = self.audio {
                        a.set_paused(!a.is_paused());
                        // Also pause video
                        if let Some(tx) = &self.command_tx {
                            let cmd = if a.is_paused() { DecoderCommand::Pause } else { DecoderCommand::Resume };
                            let _ = tx.send(cmd);
                        }
                    }
                }
                KeyCode::Escape => event_loop.exit(),
                _ => {}
            },
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, _event: DeviceEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // --- Frame sync: mpv clock → video frames ---
        #[cfg(feature = "mpv")]
        let clock = self.audio.as_ref().map(|a| a.clock()).unwrap_or(
            self.ui.as_ref().map(|u| u.position).unwrap_or(0.0)
        );
        #[cfg(not(feature = "mpv"))]
        let clock = self.ui.as_ref().map(|u| u.position).unwrap_or(0.0);

        if let Some(ref decoder) = self.decoder {
            // Drain all frames, find the one closest to the clock
            let mut best: Option<DecodedFrame> = None;
            let mut best_diff = f64::MAX;
            while let Ok(f) = decoder.frame_rx.try_recv() {
                let pts = f.pts_secs;
                let diff = (pts - clock).abs();
                if diff < best_diff {
                    best = Some(f);
                    best_diff = diff;
                }
                // If we've gone past the clock, stop draining
                if pts > clock + 0.1 { break; }
            }
            if let Some(f) = best {
                if let Some(r) = &mut self.renderer {
                    r.update_video_texture(&f.data, f.width, f.height);
                }
                if let Some(ref mut ui) = self.ui { ui.position = f.pts_secs; }
            }
        }

        // --- UI state ---
        #[cfg(feature = "mpv")]
        let paused = self.audio.as_ref().map(|a| a.is_paused()).unwrap_or(true);
        #[cfg(not(feature = "mpv"))]
        let paused = self.decoder.as_ref().map(|d| d.paused.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(true);
        let dur = self.decoder.as_ref().map(|d| d.duration_secs).unwrap_or(0.0);

        let mut open_action = false;
        let mut seek_action: Option<f64> = None;
        let mut pause_action: Option<bool> = None;
        let mut speed_action: Option<f64> = None;

        if let (Some(w), Some(r), Some(ui)) = (&self.window, &mut self.renderer, &mut self.ui) {
            ui.playing = !paused;
            ui.duration = dur;
            ui.speed = self.playback_speed;
            r.is_360 = ui.is_360;
            #[cfg(feature = "mpv")]
            if let Some(ref a) = self.audio { a.set_volume(ui.volume); }

            let raw = r.egui_state.take_egui_input(w);
            r.egui_state.egui_ctx().begin_pass(raw);
            let out = ui.update();
            r.egui_state.handle_platform_output(w, out.platform_output.clone());
            let prims = r.egui_state.egui_ctx().tessellate(out.shapes.clone(), out.pixels_per_point);

            open_action = ui.open_file_clicked; ui.open_file_clicked = false;
            seek_action = ui.seek_to.take();
            if ui.speed_changed { ui.speed_changed = false; speed_action = Some(ui.speed); }
            if ui.playing != !paused { pause_action = Some(!ui.playing); }

            if let Err(e) = r.render(&prims, &out.textures_delta, out.pixels_per_point) {
                match e {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => { let s = r.size; r.resize(s.0, s.1); }
                    wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                    wgpu::SurfaceError::Timeout => {}
                }
            }
        } else if let Some(r) = &mut self.renderer {
            if let Err(e) = r.render(&[], &egui::TexturesDelta::default(), 1.0) {
                match e {
                    wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => { let s = r.size; r.resize(s.0, s.1); }
                    wgpu::SurfaceError::OutOfMemory => tracing::error!("OOM"),
                    wgpu::SurfaceError::Timeout => {}
                }
            }
        }

        // --- Apply actions ---
        if open_action {
            if let Some(p) = rfd::FileDialog::new().add_filter("Video", &["mp4","webm","mkv","avi","mov","m4v"]).pick_file() {
                self.open_file(&p.to_string_lossy());
            }
        }
        if let Some(pos) = seek_action {
            #[cfg(feature = "mpv")]
            if let Some(ref a) = self.audio { a.seek(pos); }
            if let Some(tx) = &self.command_tx { let _ = tx.send(DecoderCommand::Seek(pos)); }
        }
        if let Some(do_pause) = pause_action {
            #[cfg(feature = "mpv")]
            if let Some(ref a) = self.audio { a.set_paused(do_pause); }
            if let Some(tx) = &self.command_tx {
                let cmd = if do_pause { DecoderCommand::Pause } else { DecoderCommand::Resume };
                let _ = tx.send(cmd);
            }
        }
        if let Some(spd) = speed_action { self.set_speed(spd); }
    }
}
