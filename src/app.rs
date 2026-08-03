use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalPosition,
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use crate::media::playback::PlaybackController;
use crate::media::types::{Command, PlaybackState, VideoFrame};
use crate::renderer::Renderer;
use crate::ui::PlayerUI;

/// In fullscreen, hide the bars after this long without mouse activity.
const UI_HIDE_DELAY: Duration = Duration::from_secs(3);
/// Seek step for the arrow keys (seconds).
const SEEK_STEP: f64 = 5.0;
/// Volume step for the up/down keys.
const VOLUME_STEP: f32 = 0.1;
/// How often to persist the resume-position state.
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(5);

pub struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    ctl: PlaybackController,
    ui: Option<PlayerUI>,
    dragging: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    /// Any window event arrived since the last `about_to_wait` —
    /// forces a render even when paused (UI interaction must be drawn).
    input_seen: bool,
    /// Time of the last user input — drives fullscreen auto-hide.
    last_input: Instant,
    /// File to open once the window is ready.
    pending_file: Option<String>,

    // ── Resume position (remembered across sessions) ────────────
    state: HashMap<String, f64>,
    state_path: PathBuf,
    last_state_save: Instant,

    // ── Stutter diagnostics ─────────────────────────────────────
    /// Last `about_to_wait` time; a large gap while playing means the main
    /// thread was blocked (the classic "stutters every few seconds").
    last_render: Option<Instant>,
    /// Throttles diagnostic log spam.
    last_diag_log: Instant,
    /// Audio ring-underflow count seen at the last check.
    last_underruns: u64,

    // ── Screenshots ────────────────────────────────────────────
    shot_dir: PathBuf,

    // ── One-shot error logging ─────────────────────────────────
    error_logged: bool,
}

impl App {
    pub fn new(initial_file: Option<String>) -> Self {
        Self {
            window: None,
            renderer: None,
            ctl: PlaybackController::new(),
            ui: None,
            dragging: false,
            last_cursor: None,
            input_seen: false,
            last_input: Instant::now(),
            pending_file: initial_file,
            state: HashMap::new(),
            state_path: PathBuf::from("player_state.json"),
            last_state_save: Instant::now(),
            last_render: None,
            last_diag_log: Instant::now(),
            last_underruns: 0,
            shot_dir: PathBuf::from("."),
            error_logged: false,
        }
    }

    fn open_file(&mut self, path: &str) {
        if let Err(e) = self.ctl.apply(Command::Open(path.into())) {
            tracing::error!("Open failed: {e}");
            return;
        }
        // Auto-play to preserve the old UX (opening a file started playback).
        // Playback always starts at 0; the saved position is available via
        // the resume button in the transport bar.
        let _ = self.ctl.apply(Command::Play);
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(w) = &self.window {
            if w.fullscreen().is_some() {
                w.set_fullscreen(None);
            } else {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
        }
        self.last_input = Instant::now();
    }

    fn screenshot(&mut self) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = self.shot_dir.join(format!("screenshot_{ts}.png"));
        if let Some(r) = &mut self.renderer {
            if r.save_frame_png(path.to_str().unwrap()) {
                tracing::info!("Screenshot saved: {}", path.display());
            } else {
                tracing::warn!("No video frame to capture yet");
            }
        }
    }

    fn load_state(&mut self) {
        if let Ok(s) = std::fs::read_to_string(&self.state_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, f64>>(&s) {
                self.state = map;
                tracing::info!("Loaded {} resume positions", self.state.len());
            }
        }
    }

    /// Persist the current playback position so the next session can
    /// resume.  Skips positions near the start/end.
    fn save_state(&mut self) {
        let Some(path) = self.ctl.file_path().map(|s| s.to_string()) else {
            return;
        };
        let pos = self.ctl.position();
        let dur = self.ctl.duration();
        if pos > 5.0 && dur > 10.0 && pos < dur - 5.0 {
            self.state.insert(path.clone(), pos);
            if self.state.len() > 30 {
                let cur = self.state.remove(&path);
                self.state.clear();
                if let Some(v) = cur {
                    self.state.insert(path, v);
                }
            }
            if let Ok(json) = serde_json::to_string(&self.state) {
                // Write on a background thread: a synchronous file write on
                // the render thread every STATE_SAVE_INTERVAL causes a
                // visible periodic hitch ("plays a few seconds, stutters").
                let path = self.state_path.clone();
                std::thread::spawn(move || {
                    let _ = std::fs::write(&path, json);
                });
            }
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

        // Paths live next to the executable.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        self.state_path = exe_dir
            .as_ref()
            .map(|d| d.join("player_state.json"))
            .unwrap_or_else(|| PathBuf::from("player_state.json"));
        self.shot_dir = exe_dir.unwrap_or_else(|| PathBuf::from("."));
        self.load_state();

        // Open file from command line if provided
        if let Some(ref path) = self.pending_file.take() {
            self.open_file(path);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.input_seen = true;
        self.last_input = Instant::now();

        // ── Lifecycle ──────────────────────────────────────────
        if matches!(&event, WindowEvent::CloseRequested) {
            self.save_state();
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

        // ── Mouse wheel = zoom (360 mode) ──────────────────────
        if let WindowEvent::MouseWheel { delta, .. } = &event {
            if let Some(r) = &mut self.renderer {
                if r.is_360 {
                    let d = match delta {
                        MouseScrollDelta::LineDelta(_, y) => *y as f32,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32 / 50.0,
                    };
                    r.camera.handle_scroll(d);
                    r.update_camera_uniform();
                }
            }
        }

        // ── Drag & drop a video file onto the window ───────────
        if let WindowEvent::DroppedFile(p) = &event {
            if let Some(s) = p.to_str() {
                self.open_file(s);
            }
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
                    let _ = self.ctl.apply(Command::Toggle);
                }
                KeyCode::KeyF => {
                    self.toggle_fullscreen();
                }
                KeyCode::KeyR => {
                    if let Some(r) = &mut self.renderer {
                        r.camera.reset();
                        r.update_camera_uniform();
                    }
                }
                KeyCode::KeyS => {
                    self.screenshot();
                }
                KeyCode::KeyM => {
                    if let Some(ref mut ui) = self.ui {
                        ui.mute_clicked = true;
                    }
                }
                KeyCode::ArrowLeft => {
                    let pos = (self.ctl.position() - SEEK_STEP).clamp(0.0, self.ctl.duration());
                    let _ = self.ctl.apply(Command::Seek(pos));
                }
                KeyCode::ArrowRight => {
                    let pos = (self.ctl.position() + SEEK_STEP).clamp(0.0, self.ctl.duration());
                    let _ = self.ctl.apply(Command::Seek(pos));
                }
                KeyCode::ArrowUp => {
                    let v = (self.ctl.volume() + VOLUME_STEP).min(1.0);
                    let _ = self.ctl.apply(Command::SetVolume(v));
                    if let Some(ref mut ui) = self.ui {
                        ui.volume = v;
                        if v > 0.0 { ui.muted = false; }
                    }
                }
                KeyCode::ArrowDown => {
                    let v = (self.ctl.volume() - VOLUME_STEP).max(0.0);
                    let _ = self.ctl.apply(Command::SetVolume(v));
                    if let Some(ref mut ui) = self.ui {
                        ui.volume = v;
                        if v <= 0.001 { ui.muted = true; }
                    }
                }
                KeyCode::Escape => {
                    // Exit fullscreen first; a second Escape quits.
                    if self.window.as_ref().map(|w| w.fullscreen().is_some()).unwrap_or(false) {
                        self.toggle_fullscreen();
                    } else {
                        self.save_state();
                        event_loop.exit();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, _event: winit::event::DeviceEvent) {}

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // ── Receive latest video frame ──────────────────────────
        // The upload happens inside render() (after surface acquire),
        // so a failing surface can't leak staging buffers.  The lookahead
        // is the time until the next present (the texture swap takes
        // effect at that vsync), scaled by playback speed.
        let vsync_ahead = self
            .renderer
            .as_ref()
            .map(|r| r.next_vsync_in())
            .unwrap_or(1.0 / 60.0);
        let mut frame: Option<VideoFrame> = self
            .ctl
            .next_video_frame(vsync_ahead * self.ctl.speed());
        let frame_uploaded = frame.is_some();

        // ── Stutter diagnostics (periodic, throttled) ─────────────
        if let Some(t) = self.last_render {
            let gap = t.elapsed();
            if gap > Duration::from_millis(100)
                && self.last_diag_log.elapsed() > Duration::from_secs(10)
                && !self.ctl.paused()
            {
                tracing::warn!(
                    "render gap: {}ms | video_buffered={} | audio_underruns={}",
                    gap.as_millis(),
                    self.ctl.buffered_frames(),
                    self.ctl.audio_underruns()
                );
                self.last_diag_log = Instant::now();
            }
        }
        // Playing but no frame available and nothing buffered ahead means
        // the decoder cannot keep up with the media clock.
        if !self.ctl.paused()
            && frame.is_none()
            && self.ctl.buffered_frames() == 0
            && !matches!(
                self.ctl.state(),
                PlaybackState::Ended | PlaybackState::Error(_)
            )
            && self.last_diag_log.elapsed() > Duration::from_secs(10)
        {
            tracing::warn!(
                "video starvation: no frame and empty queue while playing"
            );
            self.last_diag_log = Instant::now();
        }

        // ── Sync UI state from controller (skip fields driven by UI) ──
        let seeking = self.ui.as_ref().map(|u| u.seeking).unwrap_or(false);
        // Saved position for the current file, shown as the resume button.
        let resume_pos = self
            .ctl
            .file_path()
            .and_then(|p| self.state.get(p).copied());
        // Treat Ended like paused for UI purposes: playback is over, the
        // transport shows stopped, and the control bars stay visible.
        let ended = matches!(self.ctl.state(), PlaybackState::Ended);
        if let Some(ref mut ui) = self.ui {
            if !seeking {
                ui.position = self.ctl.position();
            }
            ui.duration = self.ctl.duration();
            ui.playing = !self.ctl.paused() && !ended;
            ui.speed = self.ctl.speed();
            ui.resume_available = resume_pos.is_some();
            ui.resume_position = resume_pos.unwrap_or(0.0);
        }

        let paused = self.ctl.paused() || ended;

        // ── Error surfacing (one-shot) ────────────────────────────
        if let PlaybackState::Error(msg) = self.ctl.state() {
            if !self.error_logged {
                tracing::error!("Playback error: {msg}");
                self.error_logged = true;
            }
        } else {
            self.error_logged = false;
        }

        // ── Fullscreen auto-hide ─────────────────────────────────
        // Bars hide in fullscreen after UI_HIDE_DELAY without input
        // (unless paused or interacting); any mouse/key activity shows
        // them again immediately.
        let fullscreen = self
            .window
            .as_ref()
            .map(|w| w.fullscreen().is_some())
            .unwrap_or(false);
        let ui_visible = !fullscreen
            || paused
            || seeking
            || self.dragging
            || self.last_input.elapsed() < UI_HIDE_DELAY;

        // ── Gather UI actions ────────────────────────────────────
        let mut open_action = false;
        let mut seek_action: Option<f64> = None;
        let mut pause_action = false;
        let mut speed_action: Option<f64> = None;
        let mut volume_action: Option<f32> = None;
        let mut fullscreen_action = false;
        let mut resume_action = false;

        let renderer = &mut self.renderer;
        if let (Some(w), Some(r), Some(ui)) = (&self.window, renderer, &mut self.ui) {
            r.is_360 = ui.is_360;
            ui.is_fullscreen = fullscreen;
            ui.ui_visible = ui_visible;

            let raw = r.egui_state.take_egui_input(w);
            r.egui_state.egui_ctx().begin_pass(raw);
            let out = ui.update();
            r.egui_state.handle_platform_output(w, out.platform_output.clone());
            let prims = r.egui_state
                .egui_ctx()
                .tessellate(out.shapes.clone(), out.pixels_per_point);

            open_action = ui.open_file_clicked;
            ui.open_file_clicked = false;
            fullscreen_action = ui.fullscreen_clicked;
            ui.fullscreen_clicked = false;
            resume_action = ui.resume_clicked;
            ui.resume_clicked = false;
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
            // Ended (like paused) drops the loop to Wait.  The old
            // position-based `at_end` heuristic is gone — the controller
            // now reports Ended itself when the demuxer reaches EOF.
            if !paused || interactive || frame_uploaded {
                _event_loop.set_control_flow(ControlFlow::Poll);
                if let Err(e) = r.render(&prims, &out.textures_delta, out.pixels_per_point, frame.take()) {
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
        if fullscreen_action {
            self.toggle_fullscreen();
        }
        if resume_action {
            if let Some(pos) = self
                .ctl
                .file_path()
                .and_then(|p| self.state.get(p).copied())
            {
                let _ = self.ctl.apply(Command::Seek(pos));
                let _ = self.ctl.apply(Command::Play);
            }
        }
        if let Some(pos) = seek_action {
            let _ = self.ctl.apply(Command::Seek(pos));
        }
        if pause_action {
            let _ = self.ctl.apply(Command::Toggle);
        }
        if let Some(spd) = speed_action {
            let _ = self.ctl.apply(Command::SetSpeed(spd));
        }
        if let Some(vol) = volume_action {
            let _ = self.ctl.apply(Command::SetVolume(vol));
        }

        // ── Persist resume position + periodic diagnostics ──────
        if self.last_state_save.elapsed() >= STATE_SAVE_INTERVAL {
            self.last_state_save = Instant::now();
            self.save_state();

            // ── Audio underflow diagnostics ─────────────────────
            let u = self.ctl.audio_underruns();
            if u > self.last_underruns {
                tracing::warn!(
                    "audio underflows: {} total (+{} since last check)",
                    u,
                    u - self.last_underruns
                );
                self.last_underruns = u;
            }
        }

        self.last_render = Some(Instant::now());
    }
}
