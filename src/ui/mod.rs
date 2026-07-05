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
        }
    }

    pub fn update(&mut self) -> egui::FullOutput {
        let ctx = &self.ctx;

        // Top bar with 360 toggle
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(160),
                ..Default::default()
            })
            .min_height(28.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("360° Video Player");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if self.is_360 { "360°: ON" } else { "360°: OFF" };
                        if ui.button(label).clicked() {
                            self.is_360 = !self.is_360;
                        }
                    });
                });
            });

        // Bottom controls
        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(200),
                ..Default::default()
            })
            .min_height(44.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() {
                        self.open_file_clicked = true;
                    }

                    let label = if self.playing { "Pause" } else { "Play" };
                    if ui.button(label).clicked() {
                        self.playing = !self.playing;
                    }

                    let dur_secs = self.duration.max(1.0);
                    let mut slider = self.position;
                    ui.add(
                        egui::Slider::new(&mut slider, 0.0..=dur_secs)
                            .text("")
                            .custom_formatter(|n, _| {
                                let m = (n as u64) / 60;
                                let s = (n as u64) % 60;
                                format!("{m:02}:{s:02}")
                            })
                            .custom_parser(|s| {
                                let parts: Vec<&str> = s.split(':').collect();
                                if parts.len() == 2 {
                                    Some(parts[0].parse::<f64>().unwrap_or(0.0) * 60.0
                                        + parts[1].parse::<f64>().unwrap_or(0.0))
                                } else {
                                    None
                                }
                            }),
                    );
                    if (slider - self.position).abs() > 0.3 {
                        self.seek_to = Some(slider);
                    }

                    ui.label(format!(
                        "{:02}:{:02} / {:02}:{:02}",
                        (self.position as u64) / 60,
                        (self.position as u64) % 60,
                        (self.duration as u64) / 60,
                        (self.duration as u64) % 60,
                    ));

                    ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0).text("V"));
                });
            });

        ctx.end_pass()
    }
}
