//! Verify the PRODUCTION Demux::open seek: does the first video packet
//! actually start at/near the target position?

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: demux_seek <file>");
    for pos in [1.0f64, 5.0, 30.0, 90.0, 150.0] {
        let t0 = Instant::now();
        let mut d = omniview::media::demux::Demux::open(&path, pos);
        let _info = loop {
            if let Some(r) = d.poll_ready() {
                break r.unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        let probe_ms = t0.elapsed().as_millis();
        let v_rx = d.take_channels().unwrap();
        // read first 5 video packets, print their dts->secs via stream tb
        // we need the stream time base; reuse a fresh open to read params
        let t1 = Instant::now();
        let mut first_pkts = Vec::new();
        let mut got = 0;
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        while got < 3 && Instant::now() < deadline {
            match v_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(p) => {
                    first_pkts.push(p);
                    got += 1;
                }
                Err(_) => break,
            }
        }
        // decode the time base from stream best guess: compute from dts if available
        let secs: Vec<String> = first_pkts
            .iter()
            .map(|p| match p.dts() {
                Some(dts) => format!("{dts}"),
                None => "none".into(),
            })
            .collect();
        println!(
            "seek {pos:>5.1}s: probe={probe_ms} ms, first video pkts dts={secs:?} (got={got}, read={} ms)",
            t1.elapsed().as_millis()
        );
        d.stop();
    }
}
