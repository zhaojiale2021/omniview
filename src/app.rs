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

use serde::{Deserialize, Serialize};

use crate::media::playback::PlaybackController;
use crate::media::subtitle::SubtitleFile;
use crate::media::thumb::{THUMB_MAX_H, THUMB_MAX_W, ThumbnailService};
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
/// How long an arrow key must be held before continuous seek repeats.
const SEEK_REPEAT_INTERVAL: Duration = Duration::from_millis(250);
/// Window/saved-state version key.
const STATE_MAX_RESUME: usize = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistMode {
    Normal,
    RepeatAll,
    RepeatOne,
    Shuffle,
}

impl PlaylistMode {
    fn next(self) -> Self {
        match self {
            Self::Normal => Self::RepeatAll,
            Self::RepeatAll => Self::RepeatOne,
            Self::RepeatOne => Self::Shuffle,
            Self::Shuffle => Self::Normal,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Normal => "顺序",
            Self::RepeatAll => "列表循环",
            Self::RepeatOne => "单曲循环",
            Self::Shuffle => "随机",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PlayerState {
    resume: HashMap<String, f64>,
    volume: f32,
    speed: f64,
    is_360: bool,
    window: Option<WindowState>,
}

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
    /// While fullscreen transition is in flight, keep polling and
    /// re-syncing the surface size every frame so the old window image
    /// never lingers as a fullscreen "window print".
    fullscreen_sync_until: Option<Instant>,
    /// Desired fullscreen state after the latest toggle; if the platform
    /// hasn't reached it yet, `sync_fullscreen_state` retries the request.
    fullscreen_pending: Option<bool>,
    /// When the fullscreen request was last (re)issued.
    last_fullscreen_retry: Option<Instant>,
    /// File to open once the window is ready.
    pending_file: Option<String>,
    /// Whether the OS cursor is currently visible.
    cursor_visible: bool,

    // ── Resume position + player settings (remembered across sessions) ──
    state: PlayerState,
    state_path: PathBuf,
    last_state_save: Instant,

    // ── Playlist ────────────────────────────────────────────────
    playlist: Vec<String>,
    playlist_index: usize,
    playlist_mode: PlaylistMode,
    /// Guards the automatic "play next" transition so Ended is handled once.
    end_handled: bool,

    // ── Held-arrow continuous seek ─────────────────────────────
    arrow_left_held: bool,
    arrow_right_held: bool,
    last_seek_repeat: Option<Instant>,

    // ── Subtitle overlay ───────────────────────────────────────
    subtitles: Option<SubtitleFile>,

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

    // ── Seek-preview thumbnails ────────────────────────────────
    thumb_service: ThumbnailService,

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
            fullscreen_sync_until: None,
            fullscreen_pending: None,
            last_fullscreen_retry: None,
            pending_file: initial_file,
            cursor_visible: true,
            state: PlayerState::default(),
            state_path: PathBuf::from("player_state.json"),
            last_state_save: Instant::now(),
            playlist: Vec::new(),
            playlist_index: 0,
            playlist_mode: PlaylistMode::Normal,
            end_handled: true,
            arrow_left_held: false,
            arrow_right_held: false,
            last_seek_repeat: None,
            subtitles: None,
            last_render: None,
            last_diag_log: Instant::now(),
            last_underruns: 0,
            shot_dir: PathBuf::from("."),
            thumb_service: ThumbnailService::new(),
            error_logged: false,
        }
    }

    fn open_file(&mut self, path: &str) {
        if let Err(e) = self.ctl.apply(Command::Open(path.into())) {
            tracing::error!("Open failed: {e}");
            return;
        }
        self.add_to_playlist(path);
        // Auto-play to preserve the old UX (opening a file started playback).
        // Playback always starts at 0; the saved position is available via
        // the resume button in the transport bar.
        let _ = self.ctl.apply(Command::Play);
        // Thumbnails belong to the previous file; drop them immediately.
        if let Some(ui) = &mut self.ui {
            ui.clear_thumbnails();
        }
        self.end_handled = false;
        self.load_subtitle_for(path);
    }

    fn open_files(&mut self, paths: Vec<String>) {
        if paths.is_empty() {
            return;
        }
        self.playlist = paths;
        self.playlist_index = 0;
        self.end_handled = true;
        self.open_file(&self.playlist[0].clone());
    }

    fn add_to_playlist(&mut self, path: &str) {
        if self.playlist.is_empty() {
            self.playlist = vec![path.to_string()];
            self.playlist_index = 0;
        } else if self.playlist.iter().position(|p| p == path).is_none() {
            self.playlist.push(path.to_string());
            self.playlist_index = self.playlist.len() - 1;
        }
    }

    fn play_next(&mut self) {
        if self.playlist.is_empty() || self.playlist_index + 1 >= self.playlist.len() {
            return;
        }
        self.playlist_index += 1;
        let next = self.playlist[self.playlist_index].clone();
        self.end_handled = true;
        self.open_file(&next);
    }

    fn play_prev(&mut self) {
        if self.playlist.is_empty() || self.playlist_index == 0 {
            return;
        }
        self.playlist_index -= 1;
        let prev = self.playlist[self.playlist_index].clone();
        self.end_handled = true;
        self.open_file(&prev);
    }

    fn cycle_playlist_mode(&mut self) {
        self.playlist_mode = self.playlist_mode.next();
        tracing::info!("playlist mode: {}", self.playlist_mode.label());
    }

    fn open_folder(&mut self, folder: &str) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            tracing::warn!("Cannot open folder: {folder}");
            return;
        };
        let exts = ["mp4", "webm", "mkv", "avi", "mov", "m4v"];
        let mut files = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.is_file()
                && path
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| exts.contains(&x.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
            {
                files.push(path.to_string_lossy().into_owned());
            }
        }
        files.sort();
        if files.is_empty() {
            tracing::warn!("No video files in folder: {folder}");
            return;
        }
        self.open_files(files);
    }

    /// Load an external `.srt` subtitle file with the same stem as the
    /// video (e.g. `movie.mp4` -> `movie.srt`).  Missing file is not an
    /// error: the player simply runs without subtitles.
    fn load_subtitle_for(&mut self, video_path: &str) {
        let path = PathBuf::from(video_path);
        let srt = path.with_extension("srt");
        let ass = path.with_extension("ass");
        self.subtitles = if let Ok(s) = std::fs::read_to_string(&srt) {
            tracing::info!("Loaded subtitle file: {}", srt.display());
            Some(SubtitleFile::parse(&s))
        } else if let Ok(s) = std::fs::read_to_string(&ass) {
            tracing::info!("Loaded subtitle file: {}", ass.display());
            Some(SubtitleFile::parse_ass(&s))
        } else {
            None
        };
    }

    /// While the left/right arrow key is held, keep seeking after a short
    /// repeat interval instead of only on the initial key press.
    fn maybe_repeat_seek(&mut self) {
        if !self.arrow_left_held && !self.arrow_right_held {
            self.last_seek_repeat = None;
            return;
        }
        if let Some(t) = self.last_seek_repeat
            && t.elapsed() < SEEK_REPEAT_INTERVAL
        {
            return;
        }
        self.last_seek_repeat = Some(Instant::now());
        let dur = self.ctl.duration();
        let pos = if self.arrow_left_held {
            (self.ctl.position() - SEEK_STEP).clamp(0.0, dur)
        } else {
            (self.ctl.position() + SEEK_STEP).clamp(0.0, dur)
        };
        let _ = self.ctl.apply(Command::Seek(pos));
    }

    fn toggle_fullscreen(&mut self) {
        if let Some(w) = &self.window {
            // If a previous toggle hasn't been applied by the compositor
            // yet, base the new target on the PENDING state, not on
            // `w.fullscreen()` (which may still report the old state).
            let want_fullscreen = match self.fullscreen_pending {
                Some(desired) => !desired,
                None => w.fullscreen().is_none(),
            };
            if want_fullscreen {
                // Prefer the window's current monitor explicitly; some
                // compositors treat Borderless(None) as a no-op.
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                    w.current_monitor(),
                )));
            } else {
                w.set_fullscreen(None);
            }
            // Remember the desired state so `sync_fullscreen_state` can
            // retry if the platform ignores/loses the first request, and
            // keep Polling for a short while until the OS applies it.
            self.fullscreen_pending = Some(want_fullscreen);
            self.last_fullscreen_retry = Some(Instant::now());
            w.request_redraw();
        }
        self.fullscreen_sync_until = Some(Instant::now() + Duration::from_millis(800));
        self.last_input = Instant::now();
    }

    /// Reconfigure the renderer to the window's actual current size.
    /// Called every frame; cheap when the size already matches.  This is
    /// what makes fullscreen transitions robust: even if the platform never
    /// sends a `Resized` event for `set_fullscreen`, the next frame still
    /// picks up the new size and clears/redraws at fullscreen dimensions.
    fn sync_renderer_size(&mut self) {
        if let (Some(w), Some(r)) = (&self.window, &mut self.renderer) {
            let s = w.inner_size();
            let new_size = (s.width.max(1), s.height.max(1));
            if r.size != new_size {
                r.resize(new_size.0, new_size.1);
                r.camera.dirty = true;
                r.update_camera_uniform();
            }
        }
    }

    /// If `set_fullscreen` was requested but the window hasn't reached the
    /// desired state yet (some WSLg/Wayland compositors apply the request
    /// late or drop it), re-issue the request a few times instead of
    /// leaving the window in a half-windowed "window print" state.
    fn sync_fullscreen_state(&mut self) {
        let Some(desired) = self.fullscreen_pending else {
            return;
        };
        let Some(w) = &self.window else {
            return;
        };
        let actual = w.fullscreen().is_some();
        if actual == desired {
            self.fullscreen_pending = None;
            self.last_fullscreen_retry = None;
            return;
        }
        let should_retry = self
            .last_fullscreen_retry
            .map(|t| t.elapsed() >= Duration::from_millis(250))
            .unwrap_or(true);
        if should_retry {
            tracing::debug!(
                "fullscreen state not reached (desired={desired}, actual={actual}); retrying"
            );
            if desired {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(
                    w.current_monitor(),
                )));
            } else {
                w.set_fullscreen(None);
            }
            self.last_fullscreen_retry = Some(Instant::now());
        }
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
        let Ok(s) = std::fs::read_to_string(&self.state_path) else {
            return;
        };
        if let Ok(state) = serde_json::from_str::<PlayerState>(&s) {
            self.state = state;
            tracing::info!(
                "Loaded player state ({} resume positions)",
                self.state.resume.len()
            );
        } else if let Ok(map) = serde_json::from_str::<HashMap<String, f64>>(&s) {
            // Backwards compatibility: the previous format was a plain map
            // of path -> position.
            self.state = PlayerState {
                resume: map,
                ..Default::default()
            };
            tracing::info!(
                "Loaded legacy player state ({} resume positions)",
                self.state.resume.len()
            );
        }
    }

    /// Persist playback position, volume, speed, 360 mode and window
    /// bounds so the next session can restore them.
    fn save_state(&mut self) {
        if let Some(path) = self.ctl.file_path().map(|s| s.to_string()) {
            let pos = self.ctl.position();
            let dur = self.ctl.duration();
            if pos > 5.0 && dur > 10.0 && pos < dur - 5.0 {
                self.state.resume.insert(path.clone(), pos);
                if self.state.resume.len() > STATE_MAX_RESUME {
                    let cur = self.state.resume.remove(&path);
                    self.state.resume.clear();
                    if let Some(v) = cur {
                        self.state.resume.insert(path, v);
                    }
                }
            }
        }
        self.state.volume = self.ctl.volume();
        self.state.speed = self.ctl.speed();
        self.state.is_360 = self.renderer.as_ref().map(|r| r.is_360).unwrap_or(false);
        if let Some(w) = &self.window
            && let Ok(pos) = w.outer_position()
        {
            let size = w.inner_size();
            self.state.window = Some(WindowState {
                x: pos.x,
                y: pos.y,
                width: size.width.max(1),
                height: size.height.max(1),
            });
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Paths live next to the executable.  Load state BEFORE creating
        // the window so saved size/position can be restored.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        self.state_path = exe_dir
            .as_ref()
            .map(|d| d.join("player_state.json"))
            .unwrap_or_else(|| PathBuf::from("player_state.json"));
        self.shot_dir = exe_dir.unwrap_or_else(|| PathBuf::from("."));
        self.load_state();

        // Window title from the command-line file.  Set at CREATION: a
        // runtime set_title is flaky on WSLg/X11 (the property change
        // occasionally kills the X connection and the event loop).
        let title = self
            .pending_file
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| format!("{} — Omniview", n.to_string_lossy()))
            .unwrap_or_else(|| "Omniview".to_string());
        let mut attrs = Window::default_attributes().with_title(title);
        if let Some(ws) = &self.state.window {
            attrs = attrs
                .with_inner_size(winit::dpi::PhysicalSize::new(
                    ws.width.max(1),
                    ws.height.max(1),
                ))
                .with_position(winit::dpi::Position::Physical(PhysicalPosition::new(
                    ws.x, ws.y,
                )));
        } else {
            attrs = attrs.with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        }
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        let ui = PlayerUI::new(&renderer.egui_ctx());
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.ui = Some(ui);

        // Restore saved player settings.
        let saved_volume = self.state.volume.clamp(0.0, 1.0);
        let saved_speed = if self.state.speed > 0.0 {
            self.state.speed
        } else {
            1.0
        };
        let _ = self.ctl.apply(Command::SetVolume(saved_volume));
        let _ = self.ctl.apply(Command::SetSpeed(saved_speed));
        if let Some(ref mut ui) = self.ui {
            ui.volume = saved_volume;
            ui.muted = saved_volume <= 0.001;
            ui.speed = saved_speed;
            ui.is_360 = self.state.is_360;
        }

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
        if let WindowEvent::Resized(s) = &event
            && let Some(r) = &mut self.renderer
        {
            r.resize(s.width, s.height);
            // Aspect ratio changed — force a uniform refresh.
            r.camera.dirty = true;
            r.update_camera_uniform();
        }

        // ── Mouse input BEFORE egui (so drag works even over UI) ──
        if let WindowEvent::MouseInput {
            state,
            button: MouseButton::Left,
            ..
        } = &event
        {
            self.dragging = *state == ElementState::Pressed;
            if !self.dragging {
                self.last_cursor = None;
            }
        }

        // ── Cursor movement BEFORE egui ────────────────────────
        if let WindowEvent::CursorMoved { position, .. } = &event {
            if self.dragging
                && let Some(r) = &mut self.renderer
                && r.is_360
            {
                let dx = position.x - self.last_cursor.map(|p| p.x).unwrap_or(position.x);
                let dy = position.y - self.last_cursor.map(|p| p.y).unwrap_or(position.y);
                r.camera.handle_mouse(dx, dy, r.size.0 as f64);
                r.update_camera_uniform();
            }
            self.last_cursor = Some(*position);
        }

        // ── Mouse wheel = zoom (360 mode) ──────────────────────
        if let WindowEvent::MouseWheel { delta, .. } = &event
            && let Some(r) = &mut self.renderer
            && r.is_360
        {
            let d = match delta {
                MouseScrollDelta::LineDelta(_, y) => *y,
                MouseScrollDelta::PixelDelta(p) => p.y as f32 / 50.0,
            };
            r.camera.handle_scroll(d);
            r.update_camera_uniform();
        }

        // ── Drag & drop a video file onto the window ───────────
        if let WindowEvent::DroppedFile(p) = &event
            && let Some(s) = p.to_str()
        {
            self.open_file(s);
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
        if let WindowEvent::KeyboardInput {
            event:
                KeyEvent {
                    physical_key: PhysicalKey::Code(kc),
                    state,
                    ..
                },
            ..
        } = event
        {
            let pressed = state == ElementState::Pressed;
            match kc {
                KeyCode::KeyO if pressed => {
                    if let Some(paths) = rfd::FileDialog::new()
                        .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                        .pick_files()
                    {
                        let files = paths
                            .iter()
                            .filter_map(|p| p.to_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>();
                        self.open_files(files);
                    }
                }
                KeyCode::Space if pressed => {
                    let _ = self.ctl.apply(Command::Toggle);
                }
                KeyCode::KeyF if pressed => {
                    self.toggle_fullscreen();
                }
                KeyCode::KeyR if pressed => {
                    if let Some(r) = &mut self.renderer {
                        r.camera.reset();
                        r.update_camera_uniform();
                    }
                }
                KeyCode::KeyS if pressed => {
                    self.screenshot();
                }
                KeyCode::KeyM if pressed => {
                    if let Some(ref mut ui) = self.ui {
                        ui.mute_clicked = true;
                    }
                }
                KeyCode::KeyN if pressed => {
                    let on = !self.ctl.night_mode();
                    let _ = self.ctl.apply(Command::SetNightMode(on));
                    if let Some(ref mut ui) = self.ui {
                        ui.night_mode = on;
                    }
                }
                KeyCode::KeyL if pressed => {
                    self.cycle_playlist_mode();
                }
                KeyCode::ArrowLeft => {
                    self.arrow_left_held = pressed;
                    if pressed {
                        self.last_seek_repeat = Some(Instant::now());
                        let pos = (self.ctl.position() - SEEK_STEP).clamp(0.0, self.ctl.duration());
                        let _ = self.ctl.apply(Command::Seek(pos));
                    } else {
                        self.last_seek_repeat = None;
                    }
                }
                KeyCode::ArrowRight => {
                    self.arrow_right_held = pressed;
                    if pressed {
                        self.last_seek_repeat = Some(Instant::now());
                        let pos = (self.ctl.position() + SEEK_STEP).clamp(0.0, self.ctl.duration());
                        let _ = self.ctl.apply(Command::Seek(pos));
                    } else {
                        self.last_seek_repeat = None;
                    }
                }
                KeyCode::ArrowUp if pressed => {
                    let v = (self.ctl.volume() + VOLUME_STEP).min(1.0);
                    let _ = self.ctl.apply(Command::SetVolume(v));
                    if let Some(ref mut ui) = self.ui {
                        ui.volume = v;
                        if v > 0.0 {
                            ui.muted = false;
                        }
                    }
                }
                KeyCode::ArrowDown if pressed => {
                    let v = (self.ctl.volume() - VOLUME_STEP).max(0.0);
                    let _ = self.ctl.apply(Command::SetVolume(v));
                    if let Some(ref mut ui) = self.ui {
                        ui.volume = v;
                        if v <= 0.001 {
                            ui.muted = true;
                        }
                    }
                }
                KeyCode::BracketLeft if pressed => {
                    self.play_prev();
                }
                KeyCode::BracketRight if pressed => {
                    self.play_next();
                }
                KeyCode::Escape if pressed => {
                    // Exit fullscreen first; a second Escape quits.
                    if self
                        .window
                        .as_ref()
                        .map(|w| w.fullscreen().is_some())
                        .unwrap_or(false)
                    {
                        self.toggle_fullscreen();
                    } else {
                        self.save_state();
                        event_loop.exit();
                    }
                }
                _ => {}
            }
        }
    }

    fn device_event(
        &mut self,
        _el: &ActiveEventLoop,
        _id: winit::event::DeviceId,
        _event: winit::event::DeviceEvent,
    ) {
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Re-sync the surface size before doing anything else.  This is
        // cheap when nothing changed and is the safety net for fullscreen
        // transitions whose `Resized` event may arrive late or not at all.
        self.sync_renderer_size();
        self.sync_fullscreen_state();
        self.maybe_repeat_seek();

        // ── Auto-advance playlist at end of file ───────────────
        if matches!(self.ctl.state(), PlaybackState::Ended) && !self.end_handled {
            self.end_handled = true;
            if self.playlist.is_empty() {
                // nothing to do
            } else {
                match self.playlist_mode {
                    PlaylistMode::Normal => {
                        if self.playlist_index + 1 < self.playlist.len() {
                            self.play_next();
                        }
                    }
                    PlaylistMode::RepeatAll => {
                        if self.playlist_index + 1 < self.playlist.len() {
                            self.play_next();
                        } else {
                            self.playlist_index = 0;
                            let first = self.playlist[0].clone();
                            self.end_handled = true;
                            self.open_file(&first);
                        }
                    }
                    PlaylistMode::RepeatOne => {
                        let cur = self.playlist[self.playlist_index].clone();
                        self.end_handled = true;
                        self.open_file(&cur);
                    }
                    PlaylistMode::Shuffle => {
                        if self.playlist.len() == 1 {
                            let cur = self.playlist[0].clone();
                            self.end_handled = true;
                            self.open_file(&cur);
                        } else {
                            let mut idx = self.playlist_index;
                            while idx == self.playlist_index {
                                let nanos = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.subsec_nanos() as usize)
                                    .unwrap_or(0);
                                idx = nanos % self.playlist.len();
                            }
                            self.playlist_index = idx;
                            let next = self.playlist[idx].clone();
                            self.end_handled = true;
                            self.open_file(&next);
                        }
                    }
                }
            }
        }

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
        let mut frame: Option<VideoFrame> =
            self.ctl.next_video_frame(vsync_ahead * self.ctl.speed());
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
        // the decoder cannot keep up with the media clock.  Exempt the
        // first seconds after open/seek while fresh decoders fill up.
        if !self.ctl.paused()
            && frame.is_none()
            && self.ctl.buffered_frames() == 0
            && !matches!(
                self.ctl.state(),
                PlaybackState::Ended
                    | PlaybackState::Error(_)
                    | PlaybackState::Loading
                    | PlaybackState::Seeking
            )
            && !self.ctl.startup_grace()
            && self.last_diag_log.elapsed() > Duration::from_secs(10)
        {
            tracing::warn!("video starvation: no frame and empty queue while playing");
            self.last_diag_log = Instant::now();
        }

        // ── Sync UI state from controller (skip fields driven by UI) ──
        let seeking = self.ui.as_ref().map(|u| u.seeking).unwrap_or(false);
        // Saved position for the current file, shown as the resume button.
        let resume_pos = self
            .ctl
            .file_path()
            .and_then(|p| self.state.resume.get(p).copied());
        // Treat Ended like paused for UI purposes: playback is over, the
        // transport shows stopped, and the control bars stay visible.
        let ended = matches!(self.ctl.state(), PlaybackState::Ended);

        // ── Thumbnail service results ─────────────────────────────
        // The service runs on a background thread; this is the only place
        // where its results enter egui (load_texture must happen on the
        // render thread).
        while let Some(result) = self.thumb_service.poll() {
            match result {
                Ok(thumb) => {
                    // A slow decode from the previously open file may
                    // arrive after the user has opened another one.
                    if self.ctl.file_path() == Some(thumb.path.as_str())
                        && let Some(ref mut ui) = self.ui
                    {
                        ui.store_thumbnail(thumb.pos, thumb.rgba, thumb.width, thumb.height);
                    }
                }
                Err(e) => tracing::debug!("thumbnail decode failed: {e}"),
            }
        }

        if let Some(ref mut ui) = self.ui {
            if !seeking {
                ui.position = self.ctl.position();
            }
            ui.duration = self.ctl.duration();
            ui.playing = !self.ctl.paused() && !ended;
            ui.speed = self.ctl.speed();
            ui.resume_available = resume_pos.is_some();
            ui.resume_position = resume_pos.unwrap_or(0.0);
            // Top-bar title: show the actual file name.
            ui.set_file_name(
                self.ctl
                    .file_path()
                    .and_then(|p| std::path::Path::new(p).file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            ui.set_file_path(self.ctl.file_path().unwrap_or("").to_string());
            ui.subtitle_text = self
                .subtitles
                .as_ref()
                .and_then(|subs| subs.for_time(self.ctl.position()))
                .map(|s| s.to_string());
            ui.audio_tracks = self.ctl.audio_tracks().to_vec();
            ui.video_tracks = self.ctl.video_tracks().to_vec();
            ui.audio_track = self.ctl.audio_track();
            ui.video_track = self.ctl.video_track();
            ui.night_mode = self.ctl.night_mode();
            ui.playlist_mode_label = self.playlist_mode.label().to_string();

            // Buffering: only right after open/seek (startup grace) while
            // the frame queue is still empty.  During steady playback the
            // queue legitimately drains to 0 between pops, so showing the
            // hint then would just flicker.
            ui.buffering = matches!(
                self.ctl.state(),
                PlaybackState::Loading | PlaybackState::Seeking
            ) || (self.ctl.startup_grace()
                && !self.ctl.paused()
                && !ended
                && self.ctl.buffered_frames() == 0);
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

        // ── Hide the cursor with the bars in idle fullscreen ──────
        let cursor_visible = !(fullscreen && !ui_visible && !paused);
        if self.cursor_visible != cursor_visible {
            if let Some(w) = &self.window {
                w.set_cursor_visible(cursor_visible);
            }
            self.cursor_visible = cursor_visible;
        }

        // ── Gather UI actions ────────────────────────────────────
        let mut open_action = false;
        let mut open_folder_action = false;
        let mut seek_action: Option<f64> = None;
        let mut pause_action = false;
        let mut speed_action: Option<f64> = None;
        let mut volume_action: Option<f32> = None;
        let mut fullscreen_action = false;
        let mut resume_action = false;
        let mut prev_action = false;
        let mut next_action = false;
        let mut night_mode_action: Option<bool> = None;
        let mut audio_track_action: Option<usize> = None;
        let mut video_track_action: Option<usize> = None;
        let mut thumb_request: Option<f64> = None;

        let renderer = &mut self.renderer;
        if let (Some(w), Some(r), Some(ui)) = (&self.window, renderer, &mut self.ui) {
            let mode_changed = r.is_360 != ui.is_360;
            r.is_360 = ui.is_360;
            if mode_changed {
                // The quad/sphere transform is uploaded lazily; force a
                // re-upload so the new mode renders with the right matrix.
                r.camera.dirty = true;
            }
            ui.is_fullscreen = fullscreen;
            ui.ui_visible = ui_visible;

            let raw = r.egui_state.take_egui_input(w);
            r.egui_state.egui_ctx().begin_pass(raw);
            let out = ui.update();
            r.egui_state
                .handle_platform_output(w, out.platform_output.clone());
            let prims = r
                .egui_state
                .egui_ctx()
                .tessellate(out.shapes.clone(), out.pixels_per_point);

            open_action = ui.open_file_clicked;
            ui.open_file_clicked = false;
            open_folder_action = ui.open_folder_clicked;
            ui.open_folder_clicked = false;
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
            thumb_request = ui.thumbnail_request.take();
            if ui.night_mode_changed {
                ui.night_mode_changed = false;
                night_mode_action = Some(ui.night_mode);
            }
            if ui.audio_track_changed {
                ui.audio_track_changed = false;
                if let Some(t) = ui.audio_track {
                    audio_track_action = Some(t);
                }
            }
            if ui.video_track_changed {
                ui.video_track_changed = false;
                if let Some(t) = ui.video_track {
                    video_track_action = Some(t);
                }
            }
            prev_action = ui.prev_clicked;
            ui.prev_clicked = false;
            next_action = ui.next_clicked;
            ui.next_clicked = false;
            if ui.playing == paused {
                pause_action = true;
            }

            // ── Render (unless paused and nothing changed) ──────
            // Playback advancing, UI interaction, egui needing a
            // repaint, or a fresh frame all keep the loop at Poll; a
            // static paused screen drops to Wait and renders on
            // demand (near-zero CPU).
            let fullscreen_syncing = self
                .fullscreen_sync_until
                .map(|t| t > Instant::now())
                .unwrap_or(false)
                || self.fullscreen_pending.is_some();
            if self
                .fullscreen_sync_until
                .is_none_or(|t| t <= Instant::now())
            {
                self.fullscreen_sync_until = None;
            }
            let interactive = self.input_seen
                || self.dragging
                || ui.seeking
                || fullscreen_syncing
                || r.egui_state.egui_ctx().has_requested_repaint();
            // Ended (like paused) drops the loop to Wait.  The old
            // position-based `at_end` heuristic is gone — the controller
            // now reports Ended itself when the demuxer reaches EOF.
            if !paused || interactive || frame_uploaded {
                _event_loop.set_control_flow(ControlFlow::Poll);
                if let Err(e) = r.render(
                    &prims,
                    &out.textures_delta,
                    out.pixels_per_point,
                    frame.take(),
                ) {
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
        } else if let Some(r) = &mut self.renderer
            && let Err(e) = r.render(&[], &egui::TexturesDelta::default(), 1.0, None)
        {
            match e {
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated => {
                    let s = r.size;
                    r.resize(s.0, s.1);
                }
                wgpu::SurfaceError::OutOfMemory => {}
                wgpu::SurfaceError::Timeout => {}
            }
        }

        self.input_seen = false;

        // ── Apply actions ────────────────────────────────────────
        if open_action
            && let Some(paths) = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "webm", "mkv", "avi", "mov", "m4v"])
                .pick_files()
        {
            let files = paths
                .iter()
                .filter_map(|p| p.to_str().map(|s| s.to_string()))
                .collect::<Vec<_>>();
            self.open_files(files);
        }
        if open_folder_action && let Some(folder) = rfd::FileDialog::new().pick_folder() {
            self.open_folder(&folder.to_string_lossy());
        }
        if fullscreen_action {
            self.toggle_fullscreen();
        }
        if resume_action
            && let Some(pos) = self
                .ctl
                .file_path()
                .and_then(|p| self.state.resume.get(p).copied())
        {
            let _ = self.ctl.apply(Command::Seek(pos));
            let _ = self.ctl.apply(Command::Play);
        }
        if prev_action {
            self.play_prev();
        }
        if next_action {
            self.play_next();
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
        if let Some(on) = night_mode_action {
            let _ = self.ctl.apply(Command::SetNightMode(on));
        }
        if let Some(t) = audio_track_action {
            let _ = self.ctl.apply(Command::SetAudioTrack(t));
        }
        if let Some(t) = video_track_action {
            let _ = self.ctl.apply(Command::SetVideoTrack(t));
        }
        if let Some(pos) = thumb_request
            && let Some(path) = self.ctl.file_path()
        {
            self.thumb_service
                .request(path.to_string(), pos, THUMB_MAX_W, THUMB_MAX_H);
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
