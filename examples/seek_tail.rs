use std::time::Instant;
use omniview::media::playback::PlaybackController;
use omniview::media::types::Command;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_tail <file>");
    let mut ctl = PlaybackController::new();
    ctl.apply(Command::Open(path)).unwrap();
    ctl.apply(Command::Play).unwrap();
    // let it settle
    std::thread::sleep(std::time::Duration::from_millis(800));
    for target in [5.0f64, 60.0, 120.0, 150.0, 170.0, 178.0, 180.0, 182.0] {
        ctl.apply(Command::Seek(target)).unwrap();
        let dl = Instant::now() + std::time::Duration::from_secs(8);
        let mut first: Option<f64> = None;
        while first.is_none() && Instant::now() < dl {
            if let Some(f) = ctl.next_video_frame(0.0)
                && f.pts_secs >= target - 0.1
            {
                first = Some(f.pts_secs);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        println!("seek->{target:>6.1}s: first frame pts={:?}", first);
    }
    ctl.apply(Command::Stop).unwrap();
}
