use egui::Context;

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
    /// Position saved at drag start, used to detect actual changes.
    drag_start_pos: f64,
    /// Mute state; clicking the volume icon toggles it.
    pub muted: bool,
    /// Volume remembered before muting.
    last_volume: f32,
}

impl PlayerUI {
    pub fn new(ctx: &Context) -> Self {
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
            file_name: String::new(),
            drag_start_pos: 0.0,
            muted: false,
            last_volume: 0.8,
        }
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
                // ── Seek bar (full width strip) ──────────────────
                let dur = self.duration.max(1.0);
                let mut slider_val = self.position;
                // Slider always allocates `spacing.slider_width`, so
                // widen it to the panel width here.
                ui.spacing_mut().slider_width = ui.available_width().max(1.0);
                let seek_response = ui.add(
                    egui::Slider::new(&mut slider_val, 0.0..=dur)
                        .text("")
                        .show_value(false)
                        .custom_formatter(|n, _| {
                            let m = (n as u64) / 60;
                            let s = (n as u64) % 60;
                            format!("{m}:{s:02}")
                        })
                        .custom_parser(|s| {
                            let parts: Vec<&str> = s.split(':').collect();
                            if parts.len() == 2 {
                                Some(
                                    parts[0].parse::<f64>().unwrap_or(0.0) * 60.0
                                        + parts[1].parse::<f64>().unwrap_or(0.0),
                                )
                            } else {
                                None
                            }
                        }),
                );

                if seek_response.drag_started() {
                    self.seeking = true;
                    self.drag_start_pos = self.position; // save pre-drag position
                }
                // Update position in real-time during drag for time display
                if self.seeking {
                    self.position = slider_val;
                }
                // Only trigger seek when user releases the slider
                if seek_response.drag_stopped() {
                    self.seeking = false;
                    // Compare against position before drag, not during-drag value
                    if (slider_val - self.drag_start_pos).abs() > 0.2 {
                        self.seek_to = Some(slider_val);
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
                        );
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
        if self.buffering {
            egui::Area::new(egui::Id::new("buffering_hint"))
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -40.0))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("缓冲中…").size(18.0).strong());
                    });
                });
        }

        ctx.end_pass()
    }
}
