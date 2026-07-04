use egui::Context;

pub struct PlayerUI {
    pub ctx: Context,
    pub playing: bool,
    pub position: f64,
    pub duration: f64,
    pub volume: f32,
    pub seek_to: Option<f64>,
    pub file_name: String,
    pub open_file_clicked: bool,
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
            file_name: String::new(),
            open_file_clicked: false,
        }
    }

    /// Run egui UI panels and return FullOutput.
    /// The caller must have called `ctx.begin_pass()` first
    /// (via `egui_winit::State::take_egui_input` + `Context::begin_pass`).
    pub fn update(&mut self) -> egui::FullOutput {
        let ctx = &self.ctx;
        let seek_to = &mut self.seek_to;
        let playing = &mut self.playing;
        let volume = &mut self.volume;
        let open_file = &mut self.open_file_clicked;
        let pos = self.position;
        let dur = self.duration;

        egui::TopBottomPanel::bottom("controls")
            .frame(egui::Frame {
                fill: egui::Color32::from_black_alpha(180),
                ..Default::default()
            })
            .min_height(40.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Open file button
                    if ui.button("\u{1F4C1}").clicked() {
                        *open_file = true;
                    }

                    // Play/Pause button
                    let icon = if *playing { "\u{23F8}" } else { "\u{25B6}" };
                    if ui.button(icon).clicked() {
                        *playing = !*playing;
                    }

                    // Position slider
                    let dur_secs = dur.max(1.0);
                    let mut slider = pos;
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
                    if (slider - pos).abs() > 0.1 {
                        *seek_to = Some(slider);
                    }

                    // Time display
                    ui.label(format!(
                        "{:02}:{:02} / {:02}:{:02}",
                        (pos as u64) / 60,
                        (pos as u64) % 60,
                        (dur as u64) / 60,
                        (dur as u64) % 60,
                    ));

                    // Volume slider
                    ui.add(egui::Slider::new(volume, 0.0..=1.0).text("Vol"));
                });
            });

        ctx.end_pass()
    }
}
