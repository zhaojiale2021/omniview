//! Single demuxer thread that probes streams and routes audio/video packets
//! to per-stream bounded channels.
//!
//! The demuxer opens the file, probes for the best video and audio streams,
//! reports metadata via `ready_tx`, then enters a loop reading packets from
//! ffmpeg and routing them by stream index.  Commands (Stop) are drained
//! via `try_recv` each iteration so they are never blocked by a full packet
//! channel.  Reaching end-of-file is reported via a one-shot `eof_tx`
//! signal so the controller can transition to `PlaybackState::Ended`.
//!
//! When a packet channel is full (e.g. paused decoder), the demux **holds**
//! the current packet, keeps draining commands, sleeps briefly, and retries
//! the send — it does NOT exit.  The thread only exits on EOF, a `Stop`
//! command, or a disconnected receiver.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, media};

// ── Public types ────────────────────────────────────────────────────

/// Stream metadata gathered during the probe phase.
///
/// `width`/`height`/`fps`/`has_audio` are informational metadata: consumed
/// by tests and available for future UI indicators; playback itself only
/// needs `has_video` and `duration`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DemuxInfo {
    pub has_video: bool,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration: f64,
    pub has_audio: bool,
}

/// Commands sent to the demux thread.
pub enum DemuxCmd {
    Stop,
}

/// A demuxer that spawns a background thread to read and route packets.
///
/// The struct holds the *receiving* ends of the packet channels; the thread
/// owns the two `SyncSender`s.  Call `take_channels()` to claim the receivers
/// and hand them off to decoders.
pub struct Demux {
    ready_rx: mpsc::Receiver<Result<DemuxInfo, String>>,
    video_pkt_rx: Option<mpsc::Receiver<ffmpeg::Packet>>,
    audio_pkt_rx: Option<mpsc::Receiver<ffmpeg::Packet>>,
    cmd_tx: mpsc::Sender<DemuxCmd>,
    eof_rx: mpsc::Receiver<()>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl Demux {
    /// Spawn the demux thread.  Returns immediately — the thread probes
    /// streams first and reports via `poll_ready()`.
    pub fn open(path: &str, start_pos: f64) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        // Bounded channels (cap 64) so a paused decoder doesn't cause
        // unbounded memory growth; the demux thread uses send_timeout
        // to avoid blocking forever.
        let (video_pkt_tx, video_pkt_rx) = mpsc::sync_channel::<ffmpeg::Packet>(64);
        let (audio_pkt_tx, audio_pkt_rx) = mpsc::sync_channel::<ffmpeg::Packet>(64);
        let (cmd_tx, cmd_rx) = mpsc::channel::<DemuxCmd>();
        let (eof_tx, eof_rx) = mpsc::channel::<()>();

        let p = path.to_string();
        let handle = thread::spawn(move || {
            demux_loop(
                &p,
                start_pos,
                ready_tx,
                video_pkt_tx,
                audio_pkt_tx,
                cmd_rx,
                eof_tx,
            );
        });

        Demux {
            ready_rx,
            video_pkt_rx: Some(video_pkt_rx),
            audio_pkt_rx: Some(audio_pkt_rx),
            cmd_tx,
            eof_rx,
            _thread: Some(handle),
        }
    }

    /// Non-blocking poll for the probe result.
    pub fn poll_ready(&self) -> Option<Result<DemuxInfo, String>> {
        self.ready_rx.try_recv().ok()
    }

    /// Take the packet-channel receivers.  Returns `None` if already taken.
    pub fn take_channels(
        &mut self,
    ) -> Option<(mpsc::Receiver<ffmpeg::Packet>, mpsc::Receiver<ffmpeg::Packet>)> {
        match (self.video_pkt_rx.take(), self.audio_pkt_rx.take()) {
            (Some(v), Some(a)) => Some((v, a)),
            _ => None,
        }
    }

    /// Ask the demux thread to stop.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(DemuxCmd::Stop);
    }

    /// Whether the demuxer has reached the end of the file (one-shot).
    pub fn poll_eof(&self) -> bool {
        matches!(self.eof_rx.try_recv(), Ok(()))
    }
}

// ── Demux loop (runs in background thread) ─────────────────────────

/// Send as many stashed packets as the channel accepts.
fn flush_stash(
    tx: &mpsc::SyncSender<ffmpeg::Packet>,
    stash: &mut std::collections::VecDeque<ffmpeg::Packet>,
    count: &mut u64,
) {
    while let Some(p) = stash.pop_front() {
        match tx.try_send(p) {
            Ok(()) => *count += 1,
            Err(mpsc::TrySendError::Full(p)) => {
                stash.push_front(p);
                break;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                stash.clear();
                break;
            }
        }
    }
}

fn demux_loop(
    path: &str,
    start_pos: f64,
    ready_tx: mpsc::Sender<Result<DemuxInfo, String>>,
    video_pkt_tx: mpsc::SyncSender<ffmpeg::Packet>,
    audio_pkt_tx: mpsc::SyncSender<ffmpeg::Packet>,
    cmd_rx: mpsc::Receiver<DemuxCmd>,
    eof_tx: mpsc::Sender<()>,
) {
    // Init ffmpeg (once per process is fine, but safe to call again).
    if let Err(e) = ffmpeg::init() {
        tracing::error!("ffmpeg init: {e}");
        let _ = ready_tx.send(Err(format!("ffmpeg init: {e}")));
        return;
    }

    // Open the input file.
    let mut input = match format::input(&path) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("Open input failed: {e}");
            let _ = ready_tx.send(Err(format!("Open input failed: {e}")));
            return;
        }
    };

    // ── Probe streams ──────────────────────────────────────────────

    let video_stream = input.streams().best(media::Type::Video);
    let audio_stream = input.streams().best(media::Type::Audio);

    let (has_video, width, height, fps, duration) = if let Some(ref vs) = video_stream {
        let time_base = vs.time_base();
        let rate = vs.rate();
        let fps_val = if rate.denominator() > 0 {
            rate.numerator() as f64 / rate.denominator() as f64
        } else {
            0.0
        };
        let dur = vs.duration() as f64
            * time_base.numerator() as f64
            / time_base.denominator() as f64;

        // Open decoder briefly to get width/height.
        let ctx = match ffmpeg::codec::context::Context::from_parameters(vs.parameters()) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Open decoder context failed: {e}");
                let _ = ready_tx.send(Err(msg.clone()));
                tracing::error!("{msg}");
                return;
            }
        };
        let decoder = match ctx.decoder().video() {
            Ok(d) => d,
            Err(e) => {
                let msg = format!("Open video decoder failed: {e}");
                let _ = ready_tx.send(Err(msg.clone()));
                tracing::error!("{msg}");
                return;
            }
        };
        (true, decoder.width(), decoder.height(), fps_val, dur)
    } else {
        (false, 0, 0, 0.0, 0.0)
    };

    let has_audio = audio_stream.is_some();

    // Extract stream indices *now* — `video_stream` / `audio_stream` are
    // dropped after this line, which releases the immutable borrow on
    // `input` before the mutable `input.as_mut_ptr()` calls below.
    let video_stream_index = video_stream.as_ref().map(|s| s.index());
    let audio_stream_index = audio_stream.as_ref().map(|s| s.index());

    let info = DemuxInfo {
        has_video,
        width,
        height,
        fps,
        duration,
        has_audio,
    };

    tracing::info!(
        "Demux: {}x{} @ {:.2}fps, {:.1}s, video={}, audio={}",
        width,
        height,
        fps,
        duration,
        has_video,
        has_audio
    );
    let _ = ready_tx.send(Ok(info));

    // ── Seek to start position ─────────────────────────────────────

    if start_pos > 0.01 {
        let ts = (start_pos * 1_000_000.0) as i64;
        let rc = unsafe {
            ffmpeg::ffi::av_seek_frame(
                input.as_mut_ptr(),
                -1,
                ts,
                ffmpeg::ffi::AVSEEK_FLAG_BACKWARD as i32,
            )
        };
        if rc < 0 {
            tracing::warn!("Demux seek to {start_pos:.1}s failed (rc {rc})");
        }
    }

    // ── Main packet-routing loop ───────────────────────────────────

    let mut reached_eof = false;
    let mut routed_video = 0u64;
    let mut routed_audio = 0u64;
    let mut read_errors = 0u64;
    let mut route_log = std::time::Instant::now();
    // Bounded per-stream read-ahead stashes.  When one stream's channel is
    // full (e.g. the audio ring backs up at speed != 1), the demux stashes
    // that stream's packets and KEEPS READING, so the other stream (video)
    // is never starved by audio backpressure.  Without this, the demux
    // holds a full audio packet and video packets behind it stop flowing —
    // video freezes at 2x while the audio ring drains.
    let mut video_stash: std::collections::VecDeque<ffmpeg::Packet> =
        std::collections::VecDeque::with_capacity(8);
    let mut audio_stash: std::collections::VecDeque<ffmpeg::Packet> =
        std::collections::VecDeque::with_capacity(8);

    'outer: loop {
        // (1) Drain commands BEFORE reading the next packet.
        loop {
            match cmd_rx.try_recv() {
                Ok(DemuxCmd::Stop) => break 'outer,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'outer,
            }
        }

        // (1b) Flush stashed packets whose channels have space again.
        flush_stash(&video_pkt_tx, &mut video_stash, &mut routed_video);
        flush_stash(&audio_pkt_tx, &mut audio_stash, &mut routed_audio);

        // (2) Read the next packet from the container.  The iterator API
        // spins silently on any non-EOF error, so read manually: EOF ends
        // the loop, other errors are logged and retried.
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => {
                reached_eof = true;
                break; // EOF
            }
            Err(e) => {
                read_errors += 1;
                tracing::warn!("demux read error (retrying): {e}");
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        let stream_index = packet.stream();

        // Select the target channel + stash for this packet.
        let target_video = Some(stream_index) == video_stream_index;
        let target_audio = Some(stream_index) == audio_stream_index;
        if !target_video && !target_audio {
            continue; // skip non-A/V streams
        }
        let (tx, stash, count): (
            &mpsc::SyncSender<ffmpeg::Packet>,
            &mut std::collections::VecDeque<ffmpeg::Packet>,
            &mut u64,
        ) = if target_video {
            (&video_pkt_tx, &mut video_stash, &mut routed_video)
        } else {
            (&audio_pkt_tx, &mut audio_stash, &mut routed_audio)
        };

        // (3) Send, or stash when the channel is full so the OTHER stream
        //     keeps flowing.  Only when the channel AND the stash are both
        //     full do we wait (draining commands so Stop/Seek stay
        //     responsive).  A full channel is normal during pause.
        let mut pkt = packet;
        loop {
            match tx.try_send(pkt) {
                Ok(()) => {
                    *count += 1;
                    break; // sent — go read next packet
                }
                Err(mpsc::TrySendError::Full(p)) => {
                    if stash.len() < 8 {
                        stash.push_back(p);
                        break; // keep reading — other stream flows
                    }
                    pkt = p; // channel + stash full: wait for space
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(DemuxCmd::Stop) => break 'outer,
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => break 'outer,
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break 'outer,
            }
        }
        if route_log.elapsed().as_secs() >= 5 {
            tracing::info!(
                "Demux routing: video={routed_video} audio={routed_audio} (read_errors={read_errors})"
            );
            route_log = std::time::Instant::now();
        }
    }

    if reached_eof {
        let _ = eof_tx.send(());
    }
    tracing::info!(
        "Demux thread finished: eof={reached_eof} video_pkts={routed_video} audio_pkts={routed_audio} read_errors={read_errors}"
    );
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_streams() {
        let d = Demux::open("/tmp/test_v.mp4", 0.0);
        let info = d
            .poll_ready()
            .unwrap_or_else(|| {
                let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    if let Some(r) = d.poll_ready() {
                        return r;
                    }
                    if std::time::Instant::now() > dl {
                        panic!("probe timeout");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
            .unwrap();
        assert!(info.width > 0 && info.height > 0 && info.duration > 0.0);
        assert_eq!(info.has_video, true);
    }
}
