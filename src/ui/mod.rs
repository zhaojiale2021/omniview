use egui::Context;

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
    file_name: String,
    /// Position saved at drag start, used to detect actual changes.
    drag_start_pos: f64,
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
            file_name: String::new(),
            drag_start_pos: 0.0,
        }
    }

    pub fn update(&mut self) -> egui::FullOutput {
        let ctx = &self.ctx;
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
                    });
                });
            });

        // ── Bottom controls ─────────────────────────────────────
        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(200),
                inner_margin: egui::Margin::symmetric(8.0, 4.0),
                ..Default::default()
            })
            .min_height(48.0)
            .show(ctx, |ui| {
                // ── Seek bar (full width) ───────────────────────
                let dur = self.duration.max(1.0);
                let mut slider_val = self.position;

                let seek_response = ui.add(
                    egui::Slider::new(&mut slider_val, 0.0..=dur)
                        .text("")
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

                // ── Buttons row ─────────────────────────────────
                ui.horizontal(|ui| {
                    // Open
                    if ui
                        .add_sized([50.0, 24.0], egui::Button::new("📂 Open"))
                        .clicked()
                    {
                        self.open_file_clicked = true;
                    }

                    // Play / Pause
                    let pp_label = if self.playing { "⏸ Pause" } else { "▶ Play" };
                    if ui
                        .add_sized([60.0, 24.0], egui::Button::new(pp_label))
                        .clicked()
                    {
                        self.playing = !self.playing;
                    }

                    // Time display
                    ui.label(
                        egui::RichText::new(format!(
                            "{:01}:{:02}:{:02} / {:01}:{:02}:{:02}",
                            (self.position as u64) / 3600,
                            ((self.position as u64) % 3600) / 60,
                            (self.position as u64) % 60,
                            (self.duration as u64) / 3600,
                            ((self.duration as u64) % 3600) / 60,
                            (self.duration as u64) % 60,
                        ))
                        .size(12.0)
                        .color(if is_dark {
                            egui::Color32::LIGHT_GRAY
                        } else {
                            egui::Color32::DARK_GRAY
                        }),
                    );

                    ui.separator();

                    // Speed buttons
                    let speeds: [f64; 4] = [0.5, 1.0, 1.5, 2.0];
                    for &s in &speeds {
                        let label = if (s - 1.0).abs() < 0.01 {
                            "1×".to_string()
                        } else {
                            format!("{s}×")
                        };
                        let is_active = (self.speed - s).abs() < 0.01;
                        let fill = if is_active {
                            egui::Color32::from_rgb(60, 120, 200)
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        if ui
                            .add_sized(
                                [36.0, 20.0],
                                egui::Button::new(label).fill(fill).rounding(3.0),
                            )
                            .clicked()
                            && !is_active
                        {
                            self.speed = s;
                            self.speed_changed = true;
                        }
                    }

                    ui.separator();

                    // Volume
                    ui.label("🔊");
                    let vol_before = self.volume;
                    ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).text(""));
                    if (self.volume - vol_before).abs() > 0.001 {
                        self.volume_changed = true;
                    }
                });
            });

        ctx.end_pass()
    }
}
