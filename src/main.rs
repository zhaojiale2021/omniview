mod app;
mod renderer;
mod decoder;
mod ui;

use winit::event_loop::EventLoop;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let event_loop = EventLoop::new().unwrap();
    let mut app = app::App::new();
    tracing::info!("Starting 360° Video Player");
    event_loop.run_app(&mut app).unwrap();
}
