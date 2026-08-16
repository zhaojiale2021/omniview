//! Final check: after PRODUCTION Demux::open seek, are the first packets
//! keyframes? Does the decoder emit unflagged garbage frames?

use ffmpeg_next as ffmpeg;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_pkt <file>");
    ffmpeg::init().unwrap();
    for pos in [1.0f64, 5.0, 30.0, 90.0, 150.0] {
        let mut d = omniview::media::demux::Demux::open(&path, pos, None);
        let _info = loop {
            if let Some(r) = d.poll_ready() {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let v_rx = d.take_channels().unwrap();
        let mut keys = Vec::new();
        for _ in 0..6 {
            match v_rx.recv_timeout(std::time::Duration::from_millis(300)) {
                Ok(p) => keys.push((p.dts(), p.is_key())),
                Err(_) => break,
            }
        }
        println!("seek {pos:>5.1}s: first video pkts (dts, key) = {keys:?}");
        d.stop();
    }
}
