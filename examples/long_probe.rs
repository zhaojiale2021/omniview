use std::time::Instant;
use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;

/// 60s continuous playback (no seeks, no UI) — does the media pipeline
/// itself degrade late in the file, or is the window/UI the trigger?
fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let path = std::env::args().nth(1).expect("usage: long_probe <file>");
    let mut ctl = PlaybackController::new();
    ctl.apply(Command::Open(path.into())).unwrap();
    ctl.apply(Command::Play).unwrap();
    let dl = Instant::now() + std::time::Duration::from_secs(10);
    while matches!(
        ctl.state(),
        omniview::media::types::PlaybackState::Loading
    ) && Instant::now() < dl
    {
        ctl.poll_pending();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    println!("state={:?} pos={:.3}", ctl.state(), ctl.position());

    let t0 = Instant::now();
    let mut last_frame = t0;
    let mut n = 0u64;
    let mut gaps = Vec::new();
    let mut last_report = t0;
    while t0.elapsed().as_secs_f64() < 60.0 {
        if ctl.next_video_frame(1.0 / 60.0).is_some() {
            let now = Instant::now();
            let gap = now.duration_since(last_frame).as_millis() as u64;
            if gap > 80 {
                gaps.push(gap);
            }
            last_frame = now;
            n += 1;
        }
        if last_report.elapsed().as_secs() >= 5 {
            println!(
                "[{}s] pos={:.2} frames={} gaps>80ms={:?} underruns={} buffered={}",
                t0.elapsed().as_secs(),
                ctl.position(),
                n,
                gaps,
                ctl.audio_underruns(),
                ctl.buffered_frames()
            );
            gaps.clear();
            last_report = Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    println!("DONE pos={:.2} frames={} underruns={}", ctl.position(), n, ctl.audio_underruns());
    ctl.apply(Command::Stop).unwrap();
}
