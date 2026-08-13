use egui::Context;

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
        let Ok(bytes) = std::fs::read(path) else { continue };
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
        if magic != &[0x00, 0x01, 0x00, 0x00]
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
        "缓冲中…", "续播", "🎬", "📂", "⏸", "▶", "🔇", "🔉", "🔊", "⛶", "×", "360°",
    ] {
        let ok = ctx.fonts(|f| {
            f.has_glyphs(&egui::FontId::new(14.0, egui::FontFamily::Proportional), s)
        });
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
    file_name: String,
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
            audited: false,
            file_name: String::new(),
            drag_start_pos: 0.0,
            muted: false,
            last_volume: 0.8,
        }
    }

    /// Set the media file name shown in the top bar.
    pub fn set_file_name(&mut self, name: String) {
        self.file_name = name;
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

        // Invisible full-screen click layer: double-click toggles
        // fullscreen (skipped in 360 mode where drag rotates the camera).
        if !self.is_360 {
            let bg_id = egui::Id::new("video_bg_click");
            let resp = egui::Area::new(bg_id)
                .order(egui::Order::Background)
                .show(&self.ctx, |ui| {
                    ui.allocate_rect(self.ctx.screen_rect(), egui::Sense::click())
                })
                .inner;
            if resp.double_clicked() {
                self.fullscreen_clicked = true;
            }
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
                                for s in [0.5f64, 1.0, 1.5, 2.0] {
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

                        // ── Open file (icon) ─────────────────────
                        let open = Self::icon_button(ui, "📂", 14.0, "Open file…");
                        if open.clicked() {
                            self.open_file_clicked = true;
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
                let base_frac =
                    ((self.position / dur).clamp(0.0, 1.0)) as f32;
                let mut target_frac = base_frac;

                if seek_response.drag_started() {
                    self.seeking = true;
                    self.drag_start_pos = self.position; // pre-drag position
                }
                if self.seeking {
                    if let Some(p) = seek_response.interact_pointer_pos() {
                        target_frac = ((p.x - bar_rect.left())
                            / bar_rect.width().max(1.0))
                        .clamp(0.0, 1.0);
                        // Live-update the time display during the drag.
                        self.position = target_frac as f64 * dur;
                    }
                }
                if seek_response.drag_stopped() {
                    self.seeking = false;
                    // Compare against the pre-drag position.
                    if (self.position - self.drag_start_pos).abs() > 0.2 {
                        self.seek_to = Some(self.position);
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
                        egui::Stroke::new(1.5, egui::Color32::WHITE),
                    );
                }

                // ── Seek time bubble (hover + drag) ───────────────
                // Follows the pointer above the bar with the target time.
                let show_preview = self.seeking || seek_response.hovered();
                if show_preview {
                    let pointer = seek_response
                        .hover_pos()
                        .or_else(|| ctx.pointer_latest_pos());
                    if let Some(p) = pointer {
                        // Value at the pointer: map x across the rail.
                        let rect = seek_response.rect;
                        let frac =
                            ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                        let t = if self.seeking {
                            self.position
                        } else {
                            frac as f64 * dur
                        };
                        let bubble_pos = egui::pos2(p.x, rect.top() - 46.0);
                        egui::Area::new(egui::Id::new("seek_preview"))
                            .fixed_pos(bubble_pos)
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
                                    });
                            });
                    }
                }

                ui.add_space(3.0);

                // ── Transport row ────────────────────────────────
                ui.horizontal(|ui| {
                    // Left transport cluster
                    let pp_icon = if self.playing { "⏸" } else { "▶" };
                    let pp = Self::icon_button(ui, pp_icon, 16.0, if self.playing { "Pause (Space)" } else { "Play (Space)" });
                    if pp.clicked() {
                        self.playing = !self.playing;
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
                        let vol = ui.add(
                            egui::Slider::new(&mut self.volume, 0.0..=1.0)
                                .show_value(false)
                                .text(""),
                        ).on_hover_text(format!("音量 {}%", (self.volume * 100.0).round() as i32));
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
                                ui.label(
                                    egui::RichText::new("缓冲中…")
                                        .size(16.0)
                                        .strong(),
                                );
                            });
                        });
                });
        }

        ctx.end_pass()
    }
}
