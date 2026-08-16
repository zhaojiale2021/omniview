use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::Context;

/// Thumbnail buckets are one second wide; the preview image shows the
/// nearest decoded frame for the hovered position.
const THUMB_STEP: f64 = 1.0;
/// How long to wait before re-requesting the same thumbnail bucket if the
/// background decoder hasn't returned it yet.
const THUMB_RETRY: Duration = Duration::from_millis(800);
/// Keep at most this many decoded preview textures.  A 160x90 RGBA frame
/// is ~57 KB of GPU texture, so 128 entries are only a few MB.
const THUMB_CACHE_MAX: usize = 128;

/// Install a system CJK font as a fallback so Chinese UI text ("缓冲中…",
/// "续播") renders instead of tofu boxes.  egui's default fonts have no
/// CJK glyphs, so we load a single-face TTF/OTF from the usual system
/// locations.  Windows fonts are also reachable from WSL via /mnt/c.
/// Note: .ttc collections are skipped — ab_glyph (egui's font backend)
/// cannot parse them.  Returns the path that was loaded.
pub fn install_cjk_font(ctx: &egui::Context) -> Option<std::path::PathBuf> {
    const CANDIDATES: &[&str] = &[
        // Windows (native build)
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\Deng.ttf",
        // WSL (Windows fonts are mounted under /mnt/c)
        "/mnt/c/Windows/Fonts/msyh.ttf",
        "/mnt/c/Windows/Fonts/simhei.ttf",
        "/mnt/c/Windows/Fonts/Deng.ttf",
        // Linux
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.otf",
        "/usr/share/fonts/opentype/source-han-sans/SourceHanSansSC-Regular.otf",
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // A real CJK font is megabytes; anything smaller is not it.
        if bytes.len() < 500_000 {
            continue;
        }
        // Skip font collections ('ttcf') and anything that is not a
        // single-face sfnt container — egui would panic later on bad data.
        let magic = &bytes[..4];
        if magic == b"ttcf" || magic == b"wOFF" || magic == b"wOF2" {
            continue;
        }
        if magic != [0x00, 0x01, 0x00, 0x00]
            && magic != b"OTTO"
            && magic != b"true"
            && magic != b"typ1"
        {
            continue;
        }
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
        // Push to the END of each family so Latin text keeps the default
        // fonts and only missing (CJK) glyphs fall through to this one.
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .push("cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return Some(std::path::PathBuf::from(path));
    }
    tracing::warn!("no system CJK font found; Chinese UI text will render as boxes");
    None
}

/// Player style: blue accent, slider trailing fill, slightly beefier
/// slider rail and consistent widget rounding.
fn apply_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let accent = egui::Color32::from_rgb(66, 146, 255);
    style.visuals.selection.bg_fill = accent;
    style.visuals.hyperlink_color = accent;
    // NOTE: egui 0.29 only offers TRAILING slider fill (colors the part
    // after the handle) — the opposite of the player convention where the
    // played portion is filled.  Keep it off; the handle itself takes the
    // accent color.
    style.visuals.slider_trailing_fill = false;
    style.visuals.widgets.inactive.rounding = egui::Rounding::same(4.0);
    style.visuals.widgets.hovered.rounding = egui::Rounding::same(4.0);
    style.visuals.widgets.active.rounding = egui::Rounding::same(4.0);
    style.spacing.slider_rail_height = 6.0;
    ctx.set_style(style);
}

/// One-shot diagnostic: log which UI glyphs the current font stack can
/// render (runs right after font installation).
pub fn audit_glyphs(ctx: &egui::Context) {
    for s in [
        "缓冲中…",
        "续播",
        "🎬",
        "📂",
        "⏸",
        "▶",
        "🔇",
        "🔉",
        "🔊",
        "⛶",
        "×",
        "360°",
    ] {
        let ok = ctx
            .fonts(|f| f.has_glyphs(&egui::FontId::new(14.0, egui::FontFamily::Proportional), s));
        tracing::info!("glyph {s:?} renderable: {ok}");
    }
}

/// Player controls laid out like Windows Media Player:
/// - top bar: title · open · speed · 360° toggle
/// - bottom transport bar (single row): play/pause · seek bar ·
///   time · volume(mute) · fullscreen
/// - fullscreen auto-hides the bars after a few idle seconds while
///   playing; any input brings them back.
pub struct PlayerUI {
    pub ctx: Context,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub seek_to: Option<f64>,
    pub open_file_clicked: bool,
    pub open_folder_clicked: bool,
    pub is_360: bool,
    pub speed: f64,
    pub speed_changed: bool,
    pub volume_changed: bool,
    pub seeking: bool,
    /// True while the window is fullscreen.
    pub is_fullscreen: bool,
    /// True when the bars should be drawn (false = hidden, e.g. an
    /// idle fullscreen video).
    pub ui_visible: bool,
    /// Set by the UI when the user clicks the fullscreen toggle.
    pub fullscreen_clicked: bool,
    /// Set (by the UI or the M key) to toggle mute on the next update.
    pub mute_clicked: bool,
    /// Whether a saved resume position exists for the current file.
    pub resume_available: bool,
    /// Saved position (seconds) for the current file.
    pub resume_position: f64,
    /// Set when the user clicks the resume button.
    pub resume_clicked: bool,
    /// True while playing with an empty frame queue (buffering after
    /// open/seek, or a transient stall).  Shown as a hint over the video.
    pub buffering: bool,
    /// Current subtitle text to draw over the video.
    pub subtitle_text: Option<String>,
    /// Set when the user clicks the previous/next playlist buttons.
    pub prev_clicked: bool,
    pub next_clicked: bool,
    /// Audio/video track selection (stream indices).
    pub audio_tracks: Vec<usize>,
    pub video_tracks: Vec<usize>,
    pub audio_track: Option<usize>,
    pub video_track: Option<usize>,
    pub audio_track_changed: bool,
    pub video_track_changed: bool,
    /// Night-mode limiter toggle.
    pub night_mode: bool,
    pub night_mode_changed: bool,
    /// Playlist mode label shown in the transport bar.
    pub playlist_mode_label: String,
    file_name: String,
    /// Full path of the current media file, used for thumbnail requests.
    file_path: String,
    /// Set during `update` when the seek preview needs a thumbnail at the
    /// given position.  The app consumes it and sends a background request.
    pub thumbnail_request: Option<f64>,
    /// Decoded seek-preview images, keyed by one-second bucket.
    thumb_cache: HashMap<u64, egui::TextureHandle>,
    /// Last thumbnail request sent, for retry throttling.
    thumb_last_req: Option<(String, u64, Instant)>,
    /// Glyph audit has run once (fonts only exist after the first pass).
    audited: bool,
    /// Position saved at drag start, used to detect actual changes.
    drag_start_pos: f64,
    /// Mute state; clicking the volume icon toggles it.
    pub muted: bool,
    /// Volume remembered before muting.
    last_volume: f32,
}

impl PlayerUI {
    pub fn new(ctx: &Context) -> Self {
        if let Some(path) = install_cjk_font(ctx) {
            tracing::info!("CJK font loaded from {}", path.display());
        }
        apply_style(ctx);

        Self {
            ctx: ctx.clone(),
            playing: false,
            position: 0.0,
            duration: 0.0,
            volume: 0.8,
            seek_to: None,
            open_file_clicked: false,
            open_folder_clicked: false,
            is_360: false,
            speed: 1.0,
            speed_changed: false,
            volume_changed: false,
            seeking: false,
            buffering: false,
            is_fullscreen: false,
            ui_visible: true,
            fullscreen_clicked: false,
            mute_clicked: false,
            resume_available: false,
            resume_position: 0.0,
            resume_clicked: false,
            subtitle_text: None,
            prev_clicked: false,
            next_clicked: false,
            audio_tracks: Vec::new(),
            video_tracks: Vec::new(),
            audio_track: None,
            video_track: None,
            audio_track_changed: false,
            video_track_changed: false,
            night_mode: false,
            night_mode_changed: false,
            playlist_mode_label: "顺序".to_string(),
            audited: false,
            file_name: String::new(),
            file_path: String::new(),
            thumbnail_request: None,
            thumb_cache: HashMap::new(),
            thumb_last_req: None,
            drag_start_pos: 0.0,
            muted: false,
            last_volume: 0.8,
        }
    }

    /// Set the media file name shown in the top bar.
    pub fn set_file_name(&mut self, name: String) {
        self.file_name = name;
    }

    /// Set the full path of the current media file.  When it changes, the
    /// thumbnail cache is invalidated because previews belong to the old
    /// file.
    pub fn set_file_path(&mut self, path: String) {
        if self.file_path != path {
            self.file_path = path;
            self.clear_thumbnails();
        }
    }

    /// Drop all decoded seek-preview textures (e.g. when opening a new file).
    pub fn clear_thumbnails(&mut self) {
        self.thumb_cache.clear();
        self.thumb_last_req = None;
        self.thumbnail_request = None;
    }

    /// Store a decoded thumbnail from the background service as an egui
    /// texture.  Called by the app on the render thread before `update`.
    pub fn store_thumbnail(&mut self, pos: f64, rgba: Vec<u8>, width: u32, height: u32) {
        let bucket = (pos.max(0.0) / THUMB_STEP).floor() as u64;
        let image =
            egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba);
        let name = format!("thumb_{bucket}");
        if self.thumb_cache.len() >= THUMB_CACHE_MAX && !self.thumb_cache.contains_key(&bucket) {
            self.thumb_cache.clear();
        }
        let handle = self
            .ctx
            .load_texture(name, image, egui::TextureOptions::LINEAR);
        self.thumb_cache.insert(bucket, handle);
    }

    fn fmt_time(secs: f64) -> String {
        let s = secs.max(0.0) as u64;
        format!("{:01}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    }

    /// A small icon button sized for the transport bar.
    fn icon_button(ui: &mut egui::Ui, icon: &str, size: f32, hint: &str) -> egui::Response {
        ui.add_sized(
            [34.0, 26.0],
            egui::Button::new(egui::RichText::new(icon).size(size)).rounding(3.0),
        )
        .on_hover_text(hint)
    }

    fn toggle_mute(&mut self) {
        if self.muted || self.volume <= 0.001 {
            self.muted = false;
            self.volume = self.last_volume.max(0.05);
        } else {
            self.muted = true;
            self.last_volume = self.volume;
            self.volume = 0.0;
        }
        self.volume_changed = true;
    }

    pub fn update(&mut self) -> egui::FullOutput {
        // One-shot glyph audit: fonts only exist after the first egui
        // pass, so this cannot run from `new`.
        if !self.audited {
            self.audited = true;
            audit_glyphs(&self.ctx);
        }

        // Process a pending mute toggle before borrowing ctx.
        if self.mute_clicked {
            self.mute_clicked = false;
            self.toggle_mute();
        }

        let ctx = &self.ctx;

        // Auto-hidden (idle fullscreen) — draw nothing but still close
        // the egui pass so the app can render the video layer.
        if !self.ui_visible {
            return ctx.end_pass();
        }

        let is_dark = ctx.style().visuals.dark_mode;

        // ── Top bar ────────────────────────────────────────────
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(180),
                inner_margin: egui::Margin::symmetric(8.0, 4.0),
                ..Default::default()
            })
            .min_height(28.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let title = if self.file_name.is_empty() {
                        "Media Player".to_string()
                    } else {
                        format!("🎬 {}", self.file_name)
                    };
                    ui.label(egui::RichText::new(&title).size(13.0));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // ── Audio/video track combos ──────────────
                        if self.audio_tracks.len() > 1 {
                            let label = self
                                .audio_track
                                .map(|t| format!("音轨 {t}"))
                                .unwrap_or_else(|| "音轨: 默认".to_string());
                            egui::ComboBox::from_id_salt("audio_track")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(self.audio_track.is_none(), "默认")
                                        .clicked()
                                    {
                                        self.audio_track = None;
                                        self.audio_track_changed = true;
                                    }
                                    for t in &self.audio_tracks {
                                        if ui
                                            .selectable_label(
                                                self.audio_track == Some(*t),
                                                format!("音轨 {t}"),
                                            )
                                            .clicked()
                                        {
                                            self.audio_track = Some(*t);
                                            self.audio_track_changed = true;
                                        }
                                    }
                                });
                        }
                        if self.video_tracks.len() > 1 {
                            let label = self
                                .video_track
                                .map(|t| format!("视频轨 {t}"))
                                .unwrap_or_else(|| "视频轨: 默认".to_string());
                            egui::ComboBox::from_id_salt("video_track")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(self.video_track.is_none(), "默认")
                                        .clicked()
                                    {
                                        self.video_track = None;
                                        self.video_track_changed = true;
                                    }
                                    for t in &self.video_tracks {
                                        if ui
                                            .selectable_label(
                                                self.video_track == Some(*t),
                                                format!("视频轨 {t}"),
                                            )
                                            .clicked()
                                        {
                                            self.video_track = Some(*t);
                                            self.video_track_changed = true;
                                        }
                                    }
                                });
                        }

                        // ── 360° toggle ───────────────────────────
                        let (label, color) = if self.is_360 {
                            ("360° ON", egui::Color32::from_rgb(80, 200, 120))
                        } else {
                            ("360° OFF", egui::Color32::from_rgb(140, 140, 140))
                        };
                        if ui
                            .add_sized(
                                [70.0, 22.0],
                                egui::Button::new(egui::RichText::new(label).size(12.0))
                                    .fill(color)
                                    .rounding(3.0),
                            )
                            .clicked()
                        {
                            self.is_360 = !self.is_360;
                        }

                        // ── Speed (combo) ─────────────────────────
                        let speed_label = if (self.speed - 1.0).abs() < 0.01 {
                            "1×".to_string()
                        } else {
                            format!("{}×", self.speed)
                        };
                        egui::ComboBox::from_id_salt("speed")
                            .selected_text(speed_label)
                            .show_ui(ui, |ui| {
                                for s in
                                    [0.25f64, 0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0]
                                {
                                    let label = if (s - 1.0).abs() < 0.01 {
                                        "1×".to_string()
                                    } else {
                                        format!("{s}×")
                                    };
                                    if ui
                                        .selectable_label((self.speed - s).abs() < 0.01, label)
                                        .clicked()
                                    {
                                        self.speed = s;
                                        self.speed_changed = true;
                                    }
                                }
                            });

                        // ── Open file / folder (icons) ──────────
                        let open = Self::icon_button(ui, "📂", 14.0, "Open file(s)…");
                        if open.clicked() {
                            self.open_file_clicked = true;
                        }
                        let open_folder = Self::icon_button(ui, "📁", 14.0, "Open folder…");
                        if open_folder.clicked() {
                            self.open_folder_clicked = true;
                        }
                    });
                });
            });

        // ── Bottom controls (Windows Media Player layout) ─────────
        // WMP has the seek bar as a thin full-width strip ABOVE the
        // transport row: play/pause on the left, time/volume/fullscreen
        // on the right.
        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(200),
                inner_margin: egui::Margin::symmetric(8.0, 5.0),
                ..Default::default()
            })
            .show(ctx, |ui| {
                // ── Seek bar (custom, full width strip) ─────────────
                // Hand-painted instead of egui::Slider: egui 0.29 only
                // supports TRAILING fill (colors the part after the
                // handle) while a media player should fill the PLAYED
                // portion.  Click or drag anywhere on the bar to seek.
                let dur = self.duration.max(1.0);
                let (bar_rect, seek_response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width().max(1.0), 18.0),
                    egui::Sense::click_and_drag(),
                );

                let accent = ui.visuals().selection.bg_fill;
                let base_frac = ((self.position / dur).clamp(0.0, 1.0)) as f32;
                let mut target_frac = base_frac;

                if seek_response.drag_started() {
                    self.seeking = true;
                    self.drag_start_pos = self.position; // pre-drag position
                }
                if self.seeking
                    && let Some(p) = seek_response.interact_pointer_pos()
                {
                    target_frac =
                        ((p.x - bar_rect.left()) / bar_rect.width().max(1.0)).clamp(0.0, 1.0);
                    // Live-update the time display during the drag.
                    self.position = target_frac as f64 * dur;
                }
                if seek_response.drag_stopped() {
                    self.seeking = false;
                    // Compare against the pre-drag position.
                    if (self.position - self.drag_start_pos).abs() > 0.2 {
                        self.seek_to = Some(self.position);
                    }
                }

                // Plain click (press/release without dragging) seeks
                // immediately to the clicked position.
                if seek_response.clicked()
                    && let Some(p) = seek_response.interact_pointer_pos()
                {
                    let frac =
                        ((p.x - bar_rect.left()) / bar_rect.width().max(1.0)).clamp(0.0, 1.0);
                    let t = frac as f64 * dur;
                    if (t - self.position).abs() > 0.2 {
                        self.seek_to = Some(t);
                    }
                }

                // ── Paint: rail, played fill, handle ──────────────
                let rail_h = 6.0;
                let rail = egui::Rect::from_center_size(
                    bar_rect.center(),
                    egui::vec2(bar_rect.width(), rail_h),
                );
                let rounding = egui::Rounding::same(rail_h / 2.0);
                ui.painter()
                    .rect_filled(rail, rounding, ui.visuals().widgets.inactive.bg_fill);
                let played_w = (bar_rect.width() * target_frac).max(rail_h / 2.0);
                let played = egui::Rect::from_min_max(
                    rail.min,
                    egui::pos2(rail.min.x + played_w, rail.max.y),
                );
                ui.painter().rect_filled(played, rounding, accent);
                let hovered = seek_response.hovered() || self.seeking;
                let handle_r = if hovered { 7.0 } else { 5.0 };
                let cx = rail.min.x + bar_rect.width() * target_frac;
                let handle_c = egui::pos2(cx, bar_rect.center().y);
                ui.painter().circle_filled(handle_c, handle_r, accent);
                if hovered {
                    ui.painter().circle_stroke(
                        handle_c,
                        handle_r + 2.0,
                        egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                    );
                }

                // ── Seek time bubble + thumbnail preview ────────
                // Follows the pointer above the bar with the target time and
                // (once the background decoder has caught up) a frame from
                // that part of the video.
                let show_preview = self.seeking || seek_response.hovered();
                if show_preview {
                    let pointer = seek_response
                        .hover_pos()
                        .or_else(|| ctx.pointer_latest_pos());
                    if let Some(p) = pointer {
                        // Value at the pointer: map x across the rail.
                        let rect = seek_response.rect;
                        let frac = ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                        let t = if self.seeking {
                            self.position
                        } else {
                            frac as f64 * dur
                        }
                        .clamp(0.0, dur);
                        let bucket = (t / THUMB_STEP).floor() as u64;

                        // Ask the app for a thumbnail if this bucket hasn't
                        // been decoded yet.  Retry stale requests after a
                        // short delay (the service coalesces anyway, so
                        // rapid mouse movement only decodes the last one).
                        if !self.file_path.is_empty() && !self.thumb_cache.contains_key(&bucket) {
                            let should_request = match &self.thumb_last_req {
                                Some((path, b, at)) => {
                                    path != &self.file_path
                                        || *b != bucket
                                        || at.elapsed() >= THUMB_RETRY
                                }
                                None => true,
                            };
                            if should_request {
                                self.thumb_last_req =
                                    Some((self.file_path.clone(), bucket, Instant::now()));
                                self.thumbnail_request = Some(t);
                            }
                        }

                        let thumb = self.thumb_cache.get(&bucket).cloned();

                        // Anchor the popup ABOVE the pointer with its bottom
                        // edge just clear of the seek bar.  If it overlaps
                        // the bar, the Area steals hover from the seek bar,
                        // the preview disappears, hover returns, and the
                        // thumbnail flashes on/off at every frame.
                        let popup_half_w = 88.0;
                        let screen = ctx.screen_rect();
                        let bubble_x =
                            p.x.max(screen.left() + popup_half_w + 4.0)
                                .min(screen.right() - popup_half_w - 4.0);
                        let bubble_pos = egui::pos2(bubble_x, rect.top() - 6.0);
                        let thumb_size = egui::vec2(160.0, 90.0);
                        egui::Area::new(egui::Id::new("seek_preview"))
                            .fixed_pos(bubble_pos)
                            .pivot(egui::Align2::CENTER_BOTTOM)
                            .order(egui::Order::Foreground)
                            .show(ctx, |ui| {
                                egui::Frame::popup(ui.style())
                                    .fill(egui::Color32::from_black_alpha(210))
                                    .rounding(egui::Rounding::same(4.0))
                                    .inner_margin(egui::Margin::symmetric(8.0, 4.0))
                                    .show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(Self::fmt_time(t))
                                                .family(egui::FontFamily::Monospace)
                                                .size(12.0),
                                        );
                                        ui.add_space(4.0);
                                        // Always allocate the same thumbnail
                                        // rect so the popup size never
                                        // changes when the image arrives.
                                        let (thumb_rect, _) = ui
                                            .allocate_exact_size(thumb_size, egui::Sense::hover());
                                        if let Some(tex) = &thumb {
                                            ui.put(
                                                thumb_rect,
                                                egui::Image::new(tex)
                                                    .fit_to_exact_size(thumb_size)
                                                    .rounding(egui::Rounding::same(3.0)),
                                            );
                                        } else {
                                            ui.painter().rect_filled(
                                                thumb_rect,
                                                egui::Rounding::same(3.0),
                                                egui::Color32::from_black_alpha(60),
                                            );
                                        }
                                    });
                            });
                    }
                }

                ui.add_space(3.0);

                // ── Transport row ────────────────────────────────
                ui.horizontal(|ui| {
                    // Left transport cluster
                    let prev = Self::icon_button(ui, "|<", 12.0, "Previous in playlist ([)");
                    if prev.clicked() {
                        self.prev_clicked = true;
                    }
                    let pp_icon = if self.playing { "⏸" } else { "▶" };
                    let pp = Self::icon_button(
                        ui,
                        pp_icon,
                        16.0,
                        if self.playing {
                            "Pause (Space)"
                        } else {
                            "Play (Space)"
                        },
                    );
                    if pp.clicked() {
                        self.playing = !self.playing;
                    }
                    let next = Self::icon_button(ui, ">|", 12.0, "Next in playlist (])");
                    if next.clicked() {
                        self.next_clicked = true;
                    }

                    // Resume from the saved position — shown only when a
                    // position was remembered for this file.  Opening a
                    // video always starts at 0; resuming is explicit.
                    if self.resume_available {
                        let label = format!("续播 {}", Self::fmt_time(self.resume_position));
                        let resume = ui
                            .add_sized(
                                [100.0, 26.0],
                                egui::Button::new(egui::RichText::new(label).size(12.0))
                                    .rounding(3.0),
                            )
                            .on_hover_text("Resume from last position");
                        if resume.clicked() {
                            self.resume_clicked = true;
                        }
                    }

                    // Right cluster (time · volume · fullscreen)
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Night mode limiter toggle
                        let night_label = "🌙";
                        let night_color = if self.night_mode {
                            Some(egui::Color32::from_rgb(80, 200, 120))
                        } else {
                            None
                        };
                        let mut night_btn =
                            egui::Button::new(egui::RichText::new(night_label).size(14.0))
                                .rounding(3.0);
                        if let Some(c) = night_color {
                            night_btn = night_btn.fill(c);
                        }
                        let night = ui
                            .add_sized([30.0, 24.0], night_btn)
                            .on_hover_text("Night mode (N)");
                        if night.clicked() {
                            self.night_mode = !self.night_mode;
                            self.night_mode_changed = true;
                        }

                        // Fullscreen
                        let fs_hint = if self.is_fullscreen {
                            "Exit fullscreen (F)"
                        } else {
                            "Fullscreen (F)"
                        };
                        let fs = Self::icon_button(ui, "⛶", 14.0, fs_hint);
                        if fs.clicked() {
                            self.fullscreen_clicked = true;
                        }

                        // Volume slider
                        ui.spacing_mut().slider_width = 72.0;
                        let vol = ui
                            .add(
                                egui::Slider::new(&mut self.volume, 0.0..=1.0)
                                    .show_value(false)
                                    .text(""),
                            )
                            .on_hover_text(format!(
                                "音量 {}%",
                                (self.volume * 100.0).round() as i32
                            ));
                        if vol.drag_started() || vol.changed() {
                            if self.volume > 0.0 {
                                self.muted = false;
                            }
                            self.volume_changed = true;
                        }

                        // Volume icon = mute toggle
                        let vol_icon = if self.muted || self.volume <= 0.001 {
                            "🔇"
                        } else if self.volume < 0.5 {
                            "🔉"
                        } else {
                            "🔊"
                        };
                        let vicon = ui
                            .add_sized(
                                [26.0, 24.0],
                                egui::Button::new(egui::RichText::new(vol_icon).size(14.0))
                                    .frame(false),
                            )
                            .on_hover_text("Mute (M)");
                        if vicon.clicked() {
                            self.mute_clicked = true;
                        }

                        // Playlist mode label
                        ui.label(
                            egui::RichText::new(&self.playlist_mode_label)
                                .size(11.0)
                                .color(if is_dark {
                                    egui::Color32::LIGHT_GRAY
                                } else {
                                    egui::Color32::DARK_GRAY
                                }),
                        );

                        // Time display
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                Self::fmt_time(self.position),
                                Self::fmt_time(self.duration),
                            ))
                            .family(egui::FontFamily::Monospace)
                            .size(12.0)
                            .color(if is_dark {
                                egui::Color32::LIGHT_GRAY
                            } else {
                                egui::Color32::DARK_GRAY
                            }),
                        );
                    });
                });
            });

        // ── Subtitle overlay ───────────────────────────────────
        // Plain text above the bottom controls (external .srt).
        if let Some(text) = &self.subtitle_text
            && !text.is_empty()
        {
            egui::Area::new(egui::Id::new("subtitle_overlay"))
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -96.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_black_alpha(110))
                        .rounding(egui::Rounding::same(4.0))
                        .inner_margin(egui::Margin::symmetric(14.0, 6.0))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(text.clone())
                                    .size(22.0)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                        });
                });
        }

        // ── Buffering hint: centered over the video ────────────
        // Drawn on a dark rounded panel so it stays readable over any
        // video content.
        if self.buffering {
            egui::Area::new(egui::Id::new("buffering_hint"))
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -40.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_black_alpha(180))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(20.0, 12.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.add_space(10.0);
                                ui.label(egui::RichText::new("缓冲中…").size(16.0).strong());
                            });
                        });
                });
        }

        ctx.end_pass()
    }
}
