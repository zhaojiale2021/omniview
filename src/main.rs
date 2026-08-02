// Windows: build as a GUI app so launching the exe doesn't open a
// console window spamming logs (logs still go to player.log).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio;
mod decoder;
mod player;
mod renderer;
mod ui;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use winit::event_loop::{ControlFlow, EventLoop};

fn main() {
    // Log to both stderr and `player.log` next to the executable, so
    // the app is diagnosable on Windows even when launched by
    // double-click (no console attached).
    let log_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("player.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("player.log"));
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());
    match std::fs::File::create(&log_path) {
        Ok(f) => {
            tracing_subscriber::registry()
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::sync::Mutex::new(f))
                        .with_ansi(false),
                )
                .with(env_filter)
                .init();
        }
        Err(e) => {
            eprintln!("warning: cannot open player.log: {e}");
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .init();
        }
    }

    // Optional: open a file from command line
    let initial_file = std::env::args().nth(1);

    let event_loop = EventLoop::new().unwrap();
    // Bootstrap at Poll so the first frame renders; `about_to_wait`
    // then switches between Poll (playing/interacting) and Wait
    // (paused, static) to save CPU.
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = app::App::new(initial_file);
    tracing::info!("Starting media player");
    event_loop.run_app(&mut app).unwrap();
}
