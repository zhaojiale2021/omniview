use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;
use omniview::renderer::Renderer;

struct Harness {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    ctl: PlaybackController,
    file: String,
    phase: u32,
    phase_start: Instant,
    frames: u32,
    last_render: Option<Instant>,
    max_gap_ms: u64,
}

impl Harness {
    fn new(file: String) -> Self {
        Self {
            window: None,
            renderer: None,
            ctl: PlaybackController::new(),
            file,
            phase: 0,
            phase_start: Instant::now(),
            frames: 0,
            last_render: None,
            max_gap_ms: 0,
        }
    }
}

impl ApplicationHandler for Harness {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Render Seek Test")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 540.0));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let renderer = pollster::block_on(Renderer::new(window.clone()));
        self.window = Some(window);
        self.renderer = Some(renderer);

        let f = self.file.clone();
        self.ctl.apply(Command::Open(f)).unwrap();
        self.ctl.apply(Command::Play).unwrap();
        println!("PHASE 0: open+play at t=0");
        self.phase_start = Instant::now();
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::CloseRequested = event {
            _el.exit();
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        let elapsed = self.phase_start.elapsed();
        match self.phase {
            0 if elapsed > Duration::from_millis(2500) => {
                self.ctl.apply(Command::Seek(30.0)).unwrap();
                println!("PHASE 1: seek->30s");
                self.phase = 1;
                self.phase_start = Instant::now();
            }
            1 if elapsed > Duration::from_millis(2000) => {
                self.ctl.apply(Command::Seek(90.0)).unwrap();
                println!("PHASE 2: seek->90s");
                self.phase = 2;
                self.phase_start = Instant::now();
            }
            2 if elapsed > Duration::from_millis(2000) => {
                self.ctl.apply(Command::Seek(150.0)).unwrap();
                println!("PHASE 3: seek->150s");
                self.phase = 3;
                self.phase_start = Instant::now();
            }
            3 if elapsed > Duration::from_millis(2500) => {
                println!("MAX RENDER GAP: {} ms", self.max_gap_ms);
                _el.exit();
                return;
            }
            _ => {}
        }

        let (Some(r), Some(_w)) = (&mut self.renderer, &self.window) else {
            return;
        };
        let lookahead = r.next_vsync_in();
        let frame = self.ctl.next_video_frame(lookahead * self.ctl.speed());

        if let Some(t) = self.last_render {
            let gap = t.elapsed().as_millis() as u64;
            if gap > self.max_gap_ms {
                self.max_gap_ms = gap;
                if gap > 80 {
                    println!(
                        "  render gap {} ms @ phase {} (pos {:.1}s)",
                        gap,
                        self.phase,
                        self.ctl.position()
                    );
                }
            }
        }
        self.last_render = Some(Instant::now());

        let prims = Vec::new();
        let td = egui::TexturesDelta::default();
        let _ = r.render(&prims, &td, 1.0, frame);
        self.frames += 1;
        _el.set_control_flow(ControlFlow::Poll);
    }
}

fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let file = std::env::args().nth(1).expect("usage: render_seek <file>");
    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = Harness::new(file);
    event_loop.run_app(&mut app).unwrap();
    println!("DONE frames={}", app.frames);
}
