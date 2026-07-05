use std::sync::atomic::Ordering;
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
#[cfg(feature = "audio")]
use crate::decoder::audio::AudioDecoder;
use crate::decoder::video::{DecodedFrame, DecoderCommand, VideoDecoder};
use crate::renderer::Renderer;
use crate::ui::PlayerUI;

pub struct App {
    pub window: Option<Arc<Window>>,
    pub renderer: Option<Renderer>,
    pub decoder: Option<VideoDecoder>,
    #[cfg(feature = "audio")]
    pub audio: Option<AudioDecoder>,
    pub command_tx: Option<mpsc::Sender<DecoderCommand>>,
    pub dragging: bool,
    pub ui: Option<PlayerUI>,
    pub current_file: Option<String>,
    pub playback_speed: f64,
    last_cursor: Option<PhysicalPosition<f64>>,
}

impl App {
    pub fn new() -> Self {
        Self {
            window: None, renderer: None, decoder: None,
            command_tx: None, dragging: false, ui: None,
            current_file: None, playback_speed: 1.0,
            last_cursor: None,
            #[cfg(feature = "audio")] audio: None,
        }
    }

    pub fn open_file(&mut self, path: &str) {
        self.current_file = Some(path.to_string());
        self.start_playback(path, 0.0);
    }

    fn start_playback(&mut self, path: &str, start_secs: f64) {
        if let Some(d) = self.decoder.take() { d.stop(); }
        #[cfg(feature = "audio")] drop(self.audio.take());
        self.command_tx.take();

        let (decoder, cmd_tx) = match VideoDecoder::open(path, self.playback_speed) {
            Ok(v) => v,
            Err(e) => { tracing::error!("Open: {e}"); return; }
        };
        if start_secs > 0.01 {
            let _ = cmd_tx.send(DecoderCommand::Seek(start_secs));
        }
        self.decoder = Some(decoder);
        self.command_tx = Some(cmd_tx);

        #[cfg(feature = "audio")]
        match AudioDecoder::open(path, self.playback_speed, start_secs) {
            Ok(audio) => { self.audio = Some(audio); }
            Err(e) => tracing::warn!("Audio: {e}"),
        }

        tracing::info!("Playing: {path} speed={} start={start_secs}", self.playback_speed);
    }

    pub fn set_speed(&mut self, speed: f64) {
        if (self.playback_speed - speed).abs() < 0.01 { return; }
        self.playback_speed = speed;
        let cur_pos = self.ui.as_ref().map(|u| u.position).unwrap_or(0.0);

        // Restart video with new speed
        if let Some(d) = self.decoder.take() {
            d.stop();
            self.command_tx.take();
            if let Some(ref path) = self.current_file {
                match VideoDecoder::open(path, speed) {
                    Ok((dec, tx)) => {
                        let _ = tx.send(DecoderCommand::Seek(cur_pos));
                        self.decoder = Some(dec);
                        self.command_tx = Some(tx);
                    }
                    Err(e) => tracing::error!("Video speed: {e}"),
                }
            }
        }

        // Restart audio with new speed
        #[cfg(feature = "audio")]
        {
            drop(self.audio.take());
            if let Some(ref path) = self.current_file {
                match AudioDecoder::open(path, speed, cur_pos) {
                    Ok(a) => { self.audio = Some(a); }
                    Err(e) => tracing::warn!("Audio speed: {e}"),
                }
            }
        }
    }

    #[allow(unused_variables)]
    fn restart_audio(&mut self, start_secs: f64) {
        #[cfg(feature = "audio")]
        {
            drop(self.audio.take());
            if let Some(ref path) = self.current_file.clone() {
                match AudioDecoder::open(path, self.playback_speed, start_secs) {
                    Ok(audio) => { self.audio = Some(audio); }
                    Err(e) => tracing::warn!("Audio restart: {e}"),
                }
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
        if matches!(&event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }

        // Resize
        if let WindowEvent::Resized(s) = &event {
            if let Some(r) = &mut self.renderer {
                r.resize(s.width, s.height);
                r.update_camera_uniform();
            }
        }

        // Camera: track drag state BEFORE egui (egui would consume MouseInput)
        if let WindowEvent::MouseInput { state, button: MouseButton::Left, .. } = &event {
            self.dragging = *state == ElementState::Pressed;
            if !self.dragging {
                self.last_cursor = None;
            }
        }

        // Camera: update on CursorMoved BEFORE egui
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

        // Feed event to egui-winit
        let consumed = if let (Some(w), Some(r)) = (&self.window, &mut self.renderer) {
            r.egui_state.on_window_event(w, &event).consumed
        } else {
            false
        };

        if consumed {
            return;
        }

        match event {
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
                event: KeyEvent { physical_key: PhysicalKey::Code(kc), state: ElementState::Pressed, .. },
                ..
            } => {
                match kc {
                    KeyCode::KeyO => {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                            .pick_file()
                        {
                            self.open_file(&p.to_string_lossy());
                        }
                    }
                    KeyCode::Space => {
                        if let (Some(tx), Some(d)) = (&self.command_tx, &self.decoder) {
                            let paused = d.paused.load(Ordering::Relaxed);
                            let cmd = if paused { DecoderCommand::Resume } else { DecoderCommand::Pause };
                            let _ = tx.send(cmd);
                            #[cfg(feature = "audio")]
                            if let Some(ref a) = self.audio { a.set_paused(!paused); }
                        }
                    }
                    KeyCode::Escape => event_loop.exit(),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, _event: DeviceEvent) {
        // Unused - we use CursorMoved instead of DeviceEvent::MouseMotion
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // --- Drain decoder frames ---
        if let Some(ref decoder) = self.decoder {
            let mut latest: Option<DecodedFrame> = None;
            while let Ok(f) = decoder.frame_rx.try_recv() { latest = Some(f); }
            if let Some(f) = latest {
                if let Some(r) = &mut self.renderer {
                    r.update_video_texture(&f.data, f.width, f.height);
                }
                if let Some(ref mut ui) = self.ui { ui.position = f.pts_secs; }
            }
        }

        // --- Sync decoder -> UI ---
        let paused = self.decoder.as_ref().map(|d| d.paused.load(Ordering::Relaxed)).unwrap_or(true);
        let dur = self.decoder.as_ref().map(|d| d.duration_secs).unwrap_or(0.0);

        // --- Run egui + render ---
        let mut open_action = false;
        let mut seek_action: Option<f64> = None;
        let mut pause_action: Option<bool> = None; // true=pause, false=resume
        let mut speed_action: Option<f64> = None;

        if let (Some(w), Some(r), Some(ui)) = (&self.window, &mut self.renderer, &mut self.ui) {
            ui.playing = !paused;
            ui.duration = dur;
            ui.speed = self.playback_speed;
            r.is_360 = ui.is_360;

            #[cfg(feature = "audio")]
            if let Some(ref a) = self.audio { a.set_volume(ui.volume); }

            // egui frame
            let raw = r.egui_state.take_egui_input(w);
            r.egui_state.egui_ctx().begin_pass(raw);
            let output = ui.update();
            r.egui_state.handle_platform_output(w, output.platform_output.clone());
            let prims = r.egui_state.egui_ctx().tessellate(output.shapes.clone(), output.pixels_per_point);

            // Capture actions
            open_action = ui.open_file_clicked; ui.open_file_clicked = false;
            seek_action = ui.seek_to.take();
            if ui.speed_changed { ui.speed_changed = false; speed_action = Some(ui.speed); }
            let want_play = ui.playing;
            if want_play != !paused { pause_action = Some(!want_play); } // true = pause

            // Render
            if let Err(e) = r.render(&prims, &output.textures_delta, output.pixels_per_point) {
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
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                .pick_file()
            {
                self.open_file(&p.to_string_lossy());
            }
        }
        if let Some(pos) = seek_action {
            if let Some(tx) = &self.command_tx { let _ = tx.send(DecoderCommand::Seek(pos)); }
            self.restart_audio(pos);
        }
        if let Some(do_pause) = pause_action {
            if let Some(tx) = &self.command_tx {
                let cmd = if do_pause { DecoderCommand::Pause } else { DecoderCommand::Resume };
                let _ = tx.send(cmd);
            }
            #[cfg(feature = "audio")]
            if let Some(ref a) = self.audio { a.set_paused(do_pause); }
        }
        if let Some(spd) = speed_action {
            self.set_speed(spd);
        }
    }
}
