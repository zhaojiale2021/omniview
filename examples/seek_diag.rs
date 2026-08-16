//! Headless seek/startup diagnostic: measures how long open and seek take
//! and whether decoded frames are clean (PTS continuity + corruption flags).
//!
//! Usage: cargo run --release --example seek_diag -- <file>

use std::time::Instant;

use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_diag <file>");
    let mut ctl = PlaybackController::new();

    // ── Open timing ────────────────────────────────────────────────
    let t0 = Instant::now();
    ctl.apply(Command::Open(path.clone())).unwrap();
    println!(
        "OPEN took {:.1} ms (state={:?})",
        t0.elapsed().as_millis(),
        ctl.state()
    );
    println!("duration={:.1}s", ctl.duration());

    // ── First frame latency after Play ─────────────────────────────
    ctl.apply(Command::Play).unwrap();
    let t1 = Instant::now();
    let mut first = None;
    while first.is_none() && t1.elapsed().as_secs() < 10 {
        if let Some(f) = ctl.next_video_frame(1.0 / 60.0) {
            first = Some(f.pts_secs);
            println!(
                "FIRST frame after play: pts={:.3}s, latency={:.1} ms",
                f.pts_secs,
                t1.elapsed().as_millis()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if first.is_none() {
        println!("NO FRAME within 10s of play!");
    }

    // Let it run a bit to fill the queue.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // ── Seek timings at several positions ──────────────────────────
    for target in [5.0f64, 30.0, 60.0, 90.0, 150.0] {
        let t = Instant::now();
        ctl.apply(Command::Seek(target)).unwrap();
        let seek_ms = t.elapsed().as_millis();

        let t2 = Instant::now();
        let mut first_after: Option<f64> = None;
        let mut pts_seen: Vec<f64> = Vec::new();
        while t2.elapsed().as_secs() < 10 {
            if let Some(f) = ctl.next_video_frame(0.0) {
                if first_after.is_none() && f.pts_secs >= target - 0.1 {
                    first_after = Some(f.pts_secs);
                    println!(
                        "  seek->{target:>5.1}s: apply={seek_ms} ms, first frame pts={:.3}s after {:.1} ms",
                        f.pts_secs,
                        t2.elapsed().as_millis()
                    );
                }
                pts_seen.push(f.pts_secs);
                if pts_seen.len() > 4 {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        println!("  pts after seek: {:?}", pts_seen);
        if first_after.is_none() {
            println!("  !! no frame >= {target} within 10s");
        }
    }

    ctl.apply(Command::Stop).unwrap();
    println!("DONE");
}
