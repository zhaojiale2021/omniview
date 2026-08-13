use std::time::Instant;
use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;

/// Headless frame-flow probe: open, play, then seek several times and
/// measure (a) wall gaps between delivered frames and (b) clock continuity.
/// No window / GPU / capture overhead — isolates the media pipeline.
fn main() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let path = std::env::args().nth(1).expect("usage: flow_probe <file>");
    let mut ctl = PlaybackController::new();

    let settle = |ctl: &mut PlaybackController| {
        let dl = Instant::now() + std::time::Duration::from_secs(10);
        while matches!(ctl.state(), omniview::media::types::PlaybackState::Loading
            | omniview::media::types::PlaybackState::Seeking)
            && Instant::now() < dl
        {
            ctl.poll_pending();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };

    ctl.apply(Command::Open(path.into())).unwrap();
    ctl.apply(Command::Play).unwrap();
    settle(&mut ctl);
    println!("state after open: {:?} pos={:.3}", ctl.state(), ctl.position());

    let run = |ctl: &mut PlaybackController, label: &str, secs: f64| {
        let t0 = Instant::now();
        let mut last_frame = t0;
        let last_pos = ctl.position();
        let mut gaps: Vec<u64> = Vec::new();
        let mut n = 0u64;
        let mut last_report = t0;
        println!("[trace] {label} has_audio={}", ctl.has_audio());
        while t0.elapsed().as_secs_f64() < secs {
            let t = Instant::now();
            if ctl.next_video_frame(1.0 / 60.0).is_some() {
                let gap = t.duration_since(last_frame).as_millis() as u64;
                if gap > 80 {
                    gaps.push(gap);
                }
                last_frame = t;
                n += 1;
            } else if t.duration_since(last_report).as_millis() > 100 {
                println!(
                    "[trace] no frame for {}ms pos={:.3} buffered={}",
                    t.duration_since(last_frame).as_millis(),
                    ctl.position(),
                    ctl.buffered_frames()
                );
                last_report = t;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let pos = ctl.position();
        println!(
            "{label}: {n} frames in {secs:.1}s, gaps>80ms: {:?}, clock {last_pos:.3} -> {pos:.3}",
            gaps
        );
    };

    run(&mut ctl, "after-open 4s", 4.0);

    for target in [30.0f64, 90.0, 150.0] {
        ctl.apply(Command::Seek(target)).unwrap();
        settle(&mut ctl);
        println!("seek->{target}: state {:?} pos {:.3}", ctl.state(), ctl.position());
        run(&mut ctl, &format!("after-seek-{target} 4s"), 4.0);
    }

    ctl.apply(Command::Stop).unwrap();
}
