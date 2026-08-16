// Windows: build as a GUI app so launching the exe doesn't open a
// console window spamming logs (logs still go to player.log).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod media;
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
    // Default filter: our own info logs, but keep wgpu/egui internals at
    // warn so the log file isn't flooded with per-frame maintain noise.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,omniview=info,wgpu_core=warn,wgpu_hal=warn,egui_wgpu=warn".into()
    });
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
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }

    // Panics should always leave a trace next to the executable even on
    // Windows where a console window is not available.  The same file is
    // used by the tracing subscriber below.
    let panic_log = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let mut line = format!("PANIC: {msg}");
        if let Some(loc) = info.location() {
            line.push_str(&format!(" ({}:{})", loc.file(), loc.line()));
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
        {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        eprintln!("{line}");
    }));

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
