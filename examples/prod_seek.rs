//! Production-path seek quality: drive PlaybackController through seeks and
//! fingerprint every delivered frame to detect bad/corrupt frames.

use std::time::{Duration, Instant};

use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;

fn main() {
    let path = std::env::args().nth(1).expect("usage: prod_seek <file>");
    let mut ctl = PlaybackController::new();
    ctl.apply(Command::Open(path)).unwrap();
    ctl.apply(Command::Play).unwrap();

    // Let it run 1s
    let t0 = Instant::now();
    let mut first_pts = None;
    while t0.elapsed() < Duration::from_secs(1) {
        if let Some(f) = ctl.next_video_frame(0.0)
            && first_pts.is_none()
        {
            first_pts = Some(f.pts_secs);
            println!("start: first pts={:.3}s", f.pts_secs);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    for target in [2.0f64, 5.0, 10.0, 30.0, 60.0, 90.0, 150.0] {
        let ts = Instant::now();
        ctl.apply(Command::Seek(target)).unwrap();
        let seek_ms = ts.elapsed().as_millis();

        // watch the first 30 frames delivered after seek
        let mut got = 0u32;
        let mut first: Option<f64> = None;
        let mut before_target = 0u32;
        let mut jumps = 0u32;
        let mut last: Option<f64> = None;
        let deadline = Instant::now() + Duration::from_secs(6);
        while got < 30 && Instant::now() < deadline {
            if let Some(f) = ctl.next_video_frame(0.0) {
                got += 1;
                if first.is_none() {
                    first = Some(f.pts_secs);
                    println!(
                        "seek {target:>5.1}s: apply={seek_ms} ms -> first frame pts={:.3}s ({} after seek)",
                        f.pts_secs,
                        ts.elapsed().as_millis()
                    );
                }
                if f.pts_secs < target - 0.05 {
                    before_target += 1;
                }
                if let Some(l) = last
                    && (f.pts_secs - l).abs() > 0.05
                {
                    jumps += 1;
                }
                last = Some(f.pts_secs);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!(
            "  -> {got} frames; before_target={before_target} jumps(>50ms)={jumps} last_pts={:?}",
            last
        );
    }
    ctl.apply(Command::Stop).unwrap();
}
