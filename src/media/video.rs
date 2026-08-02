//! In-process video decoder using FFmpeg libraries (ffmpeg-next).
//!
//! Migrated from the legacy `src/decoder/video.rs` (which stays as
//! reference until Task 8 removes it).  Differences:
//!   - bounded frame queue capacity 3 (was 2)
//!   - emits `media::types::VideoFrame` instead of the legacy `DecodedFrame`
//!   - no `ReadyInfo`/`poll_ready` — probing moves to the demux (Task 4);
//!     the thread logs readiness internally
//!   - no self-heal/diagnostics state (that was the old player's job)
//!
//! The decode thread feeds a bounded channel; the render loop picks one
//! frame per vsync, so decode is paced by consumption.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::thread;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};

use crate::media::types::VideoFrame;

#[derive(Debug, Clone)]
pub enum DecoderCmd { Pause, Resume, Stop }

pub struct VideoDecoder {
    frame_rx: mpsc::Receiver<VideoFrame>,
    speed_secs: Arc<AtomicU64>,
}

impl VideoDecoder {
    /// Spawn the decoder thread.  Returns immediately — the thread opens
    /// the file, probes the stream, and starts decoding on its own.
    pub fn open(path: &str, start_pos: f64) -> (Self, mpsc::Sender<DecoderCmd>) {
        // Bounded channel — capacity 3 is the backpressure that stops
        // the decoder from running ahead of the display.
        let (frame_tx, frame_rx) = mpsc::sync_channel::<VideoFrame>(3);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let paused = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        // Speed is read from this atomic each frame to pace the decode.
        let speed_secs = Arc::new(AtomicU64::new(1.0f64.to_bits()));

        let st = stopped.clone();
        let pa = paused.clone();
        let spd = speed_secs.clone();
        let p = path.to_string();
        let ct = cmd_tx.clone();

        thread::spawn(move || {
            decode_loop(&p, start_pos, frame_tx, cmd_rx, st, pa, spd);
        });

        (Self { frame_rx, speed_secs }, ct)
    }

    /// Spawn the decoder thread reading packets from a channel (driven by
    /// the demuxer) instead of pulling from the file directly.  The file is
    /// opened only to read the video stream's codec parameters (time_base,
    /// fps, width, height) — no seek is performed here; the demux already
    /// seeked to `start_pos`.
    pub fn from_packets(
        path: &str,
        pkt_rx: mpsc::Receiver<ffmpeg::codec::packet::Packet>,
        start_pos: f64,
    ) -> (Self, mpsc::Sender<DecoderCmd>) {
        let (frame_tx, frame_rx) = mpsc::sync_channel::<VideoFrame>(3);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let paused = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let speed_secs = Arc::new(AtomicU64::new(1.0f64.to_bits()));

        let st = stopped.clone();
        let pa = paused.clone();
        let spd = speed_secs.clone();
        let p = path.to_string();
        let ct = cmd_tx.clone();

        thread::spawn(move || {
            decode_packets_loop(&p, start_pos, pkt_rx, frame_tx, cmd_rx, st, pa, spd);
        });

        (Self { frame_rx, speed_secs }, ct)
    }

    /// Update the pacing speed (1.0 = normal).  Frames are produced at
    /// content-rate × speed so they are not generated faster than needed.
    pub fn set_speed(&self, speed: f64) {
        self.speed_secs
            .store(speed.to_bits(), Ordering::Relaxed);
    }

    /// Non-blocking poll for the next decoded frame.
    pub fn recv(&self) -> Option<VideoFrame> {
        self.frame_rx.try_recv().ok()
    }
}

// ── Decode loop (runs in background thread) ────────────────────────

fn decode_loop(
    path: &str,
    start_pos: f64,
    frame_tx: SyncSender<VideoFrame>,
    command_rx: mpsc::Receiver<DecoderCmd>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    speed_secs: Arc<AtomicU64>,
) {
    if let Err(e) = ffmpeg::init() {
        tracing::error!("ffmpeg init: {e}");
        return;
    }

    let mut input = match format::input(&path) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("Open input failed: {e}");
            return;
        }
    };

    let stream = match input.streams().best(media::Type::Video) {
        Some(s) => s,
        None => {
            tracing::error!("No video stream");
            return;
        }
    };
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let rate = stream.rate();
    let fps = if rate.denominator() > 0 {
        rate.numerator() as f64 / rate.denominator() as f64
    } else {
        30.0
    };
    let duration_secs = stream.duration() as f64
        * time_base.numerator() as f64
        / time_base.denominator() as f64;

    let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Open decoder failed: {e}");
            return;
        }
    };
    let mut decoder = match ctx.decoder().video() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Open video decoder failed: {e}");
            return;
        }
    };
    let (width, height) = (decoder.width(), decoder.height());

    // Seek to the keyframe at or before the requested start position
    // (BACKWARD), then discard decoded frames until pts >= start_pos.
    // Frames decoded before the target are dropped below.
    if start_pos > 0.01 {
        let ts = (start_pos * 1_000_000.0) as i64; // AV_TIME_BASE microseconds
        let rc = unsafe {
            ffmpeg::ffi::av_seek_frame(
                input.as_mut_ptr(),
                -1,
                ts,
                ffmpeg::ffi::AVSEEK_FLAG_BACKWARD as i32,
            )
        };
        if rc < 0 {
            tracing::warn!("Seek to {start_pos:.1}s failed (rc {rc})");
        }
    }

    let mut scaler = match software::scaling::Context::get(
        decoder.format(),
        width,
        height,
        format::Pixel::RGBA,
        width,
        height,
        software::scaling::Flags::BILINEAR,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("swscale init failed: {e}");
            return;
        }
    };

    tracing::info!("Video: {width}x{height} @ {fps:.2}fps, {duration_secs:.1}s");

    // Reusable output frame for the RGB conversion.
    let mut rgb = frame::Video::empty();

    // Pace the decode to the content rate × current speed (like
    // ffmpeg's -readrate), so frames are produced at playback speed
    // rather than flooding ahead of the display clock.  The speed is
    // read from the shared atomic so future speed changes are live.
    let mut last_speed = f64::from_bits(speed_secs.load(Ordering::Relaxed)).max(0.1);
    let mut next_frame_at = std::time::Instant::now();

    let mut packets = input.packets();
    'outer: loop {
        // Process commands.  A disconnected command channel (all senders
        // dropped) is an implicit stop: the caller can't send Stop any
        // more, and dropping `frame_rx` alone is not detected while
        // paused (no frames are sent then), so the thread would leak.
        loop {
            match command_rx.try_recv() {
                Ok(cmd) => match cmd {
                    DecoderCmd::Stop => {
                        stopped.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                    DecoderCmd::Pause => paused.store(true, Ordering::Relaxed),
                    DecoderCmd::Resume => {
                        paused.store(false, Ordering::Relaxed);
                        // Restart the pacing clock on resume: after a
                        // long pause the old schedule is far in the
                        // past, which would make the decoder flood
                        // frames to catch up.
                        next_frame_at = std::time::Instant::now();
                    }
                },
                Err(mpsc::TryRecvError::Disconnected) => {
                    stopped.store(true, Ordering::Relaxed);
                    break 'outer;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }

        // TRUE pause: don't pull the next packet — the demux position is
        // frozen exactly here, and resume continues from this packet.
        if paused.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }

        let (stream, packet) = match packets.next() {
            Some(p) => p,
            None => break, // EOF
        };
        if stream.index() != stream_index {
            continue;
        }

        if let Err(e) = decoder.send_packet(&packet) {
            // Eof is expected at the end of the stream.
            if matches!(e, ffmpeg::Error::Eof) {
                break;
            }
            tracing::debug!("send_packet: {e}");
            continue;
        }

        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            // A pause/stop may have arrived while frames were buffered.
            // Disconnect (all senders dropped) is an implicit stop, as
            // in the outer command-drain loop.
            loop {
                match command_rx.try_recv() {
                    Ok(cmd) => match cmd {
                        DecoderCmd::Stop => {
                            stopped.store(true, Ordering::Relaxed);
                            break 'outer;
                        }
                        DecoderCmd::Pause => paused.store(true, Ordering::Relaxed),
                        DecoderCmd::Resume => paused.store(false, Ordering::Relaxed),
                    },
                    Err(mpsc::TryRecvError::Disconnected) => {
                        stopped.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if stopped.load(Ordering::Relaxed) {
                break 'outer;
            }
            if paused.load(Ordering::Relaxed) {
                // Stop mid-frame; resume continues (buffered frames are
                // drained by the next receive call).
                continue 'outer;
            }

            if let Err(e) = scaler.run(&decoded, &mut rgb) {
                tracing::debug!("scale: {e}");
                continue;
            }

            // `best_effort_timestamp` (stream time base) is the reliable
            // presentation time; the codec's own `pts`/time_base can be
            // unset, which would silently make every pts 0.
            let pts_secs = decoded
                .timestamp()
                .or(decoded.pts())
                .map(|p| p as f64 * time_base.numerator() as f64 / time_base.denominator() as f64)
                .unwrap_or(0.0);

            // Discard frames decoded before the seek target (the
            // BACKWARD seek lands on an earlier keyframe).
            if start_pos > 0.01 && pts_secs < start_pos {
                continue;
            }

            // Pace to content rate × current speed.
            let sp = f64::from_bits(speed_secs.load(Ordering::Relaxed)).max(0.1);
            if (sp - last_speed).abs() > 0.01 {
                last_speed = sp;
                next_frame_at = std::time::Instant::now();
            }
            let frame_period = 1.0 / (fps * sp);
            let now = std::time::Instant::now();
            if now < next_frame_at {
                thread::sleep(next_frame_at - now);
            }
            next_frame_at += std::time::Duration::from_secs_f64(frame_period);

            let data = Arc::new(rgb.data(0).to_vec());
            let frame_out = VideoFrame {
                data,
                width,
                height,
                pts_secs,
            };
            if frame_tx.send(frame_out).is_err() {
                // Receiver dropped — shut down.
                stopped.store(true, Ordering::Relaxed);
                break 'outer;
            }
        }
    }

    let _ = decoder.send_eof();
    tracing::debug!("Video decoder thread finished");
}

// ── Decode-from-packet-channel loop (driven by Demux) ─────────────

fn decode_packets_loop(
    path: &str,
    start_pos: f64,
    pkt_rx: mpsc::Receiver<ffmpeg::codec::packet::Packet>,
    frame_tx: SyncSender<VideoFrame>,
    command_rx: mpsc::Receiver<DecoderCmd>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    speed_secs: Arc<AtomicU64>,
) {
    if let Err(e) = ffmpeg::init() {
        tracing::error!("ffmpeg init: {e}");
        return;
    }

    // Open the file ONLY to read codec parameters — the demux already
    // seeked to start_pos and is routing packets through the channel.
    let input = match format::input(&path) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("Open input failed: {e}");
            return;
        }
    };

    let stream = match input.streams().best(media::Type::Video) {
        Some(s) => s,
        None => {
            tracing::error!("No video stream");
            return;
        }
    };
    let time_base = stream.time_base();
    let rate = stream.rate();
    let fps = if rate.denominator() > 0 {
        rate.numerator() as f64 / rate.denominator() as f64
    } else {
        30.0
    };

    let ctx = match ffmpeg::codec::context::Context::from_parameters(stream.parameters()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Open decoder failed: {e}");
            return;
        }
    };
    let mut decoder = match ctx.decoder().video() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Open video decoder failed: {e}");
            return;
        }
    };
    let (width, height) = (decoder.width(), decoder.height());

    // The input context was only needed for stream params — drop it so
    // the file descriptor is not held for the thread's lifetime.
    drop(input);

    let mut scaler = match software::scaling::Context::get(
        decoder.format(),
        width,
        height,
        format::Pixel::RGBA,
        width,
        height,
        software::scaling::Flags::BILINEAR,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("swscale init failed: {e}");
            return;
        }
    };

    // NO av_seek_frame here — the demux already seeked BACKWARD.
    // Frames decoded before the target pts (from the earlier keyframe)
    // are discarded below.

    let mut rgb = frame::Video::empty();

    let mut last_speed = f64::from_bits(speed_secs.load(Ordering::Relaxed)).max(0.1);
    let mut next_frame_at = std::time::Instant::now();

    'outer: loop {
        // Drain commands (non-blocking).
        loop {
            match command_rx.try_recv() {
                Ok(cmd) => match cmd {
                    DecoderCmd::Stop => {
                        stopped.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                    DecoderCmd::Pause => paused.store(true, Ordering::Relaxed),
                    DecoderCmd::Resume => {
                        paused.store(false, Ordering::Relaxed);
                        next_frame_at = std::time::Instant::now();
                    }
                },
                Err(mpsc::TryRecvError::Disconnected) => {
                    stopped.store(true, Ordering::Relaxed);
                    break 'outer;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }

        // TRUE pause: don't pull the next packet.
        if paused.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }

        // Read the next packet from the demux channel.
        let packet = match pkt_rx.recv() {
            Ok(p) => p,
            Err(_) => break, // channel closed (demux stopped / EOF)
        };

        if let Err(e) = decoder.send_packet(&packet) {
            if matches!(e, ffmpeg::Error::Eof) {
                break;
            }
            tracing::debug!("send_packet: {e}");
            continue;
        }

        let mut decoded = frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            // Drain commands mid-frame (like the file-based decode loop).
            loop {
                match command_rx.try_recv() {
                    Ok(cmd) => match cmd {
                        DecoderCmd::Stop => {
                            stopped.store(true, Ordering::Relaxed);
                            break 'outer;
                        }
                        DecoderCmd::Pause => paused.store(true, Ordering::Relaxed),
                        DecoderCmd::Resume => paused.store(false, Ordering::Relaxed),
                    },
                    Err(mpsc::TryRecvError::Disconnected) => {
                        stopped.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
            if stopped.load(Ordering::Relaxed) {
                break 'outer;
            }
            if paused.load(Ordering::Relaxed) {
                continue 'outer;
            }

            if let Err(e) = scaler.run(&decoded, &mut rgb) {
                tracing::debug!("scale: {e}");
                continue;
            }

            let pts_secs = decoded
                .timestamp()
                .or(decoded.pts())
                .map(|p| p as f64 * time_base.numerator() as f64 / time_base.denominator() as f64)
                .unwrap_or(0.0);

            // Discard frames decoded before the seek target.
            if start_pos > 0.01 && pts_secs < start_pos {
                continue;
            }

            // Pace to content rate × current speed.
            let sp = f64::from_bits(speed_secs.load(Ordering::Relaxed)).max(0.1);
            if (sp - last_speed).abs() > 0.01 {
                last_speed = sp;
                next_frame_at = std::time::Instant::now();
            }
            let frame_period = 1.0 / (fps * sp);
            let now = std::time::Instant::now();
            if now < next_frame_at {
                thread::sleep(next_frame_at - now);
            }
            next_frame_at += std::time::Duration::from_secs_f64(frame_period);

            let data = Arc::new(rgb.data(0).to_vec());
            let frame_out = VideoFrame {
                data,
                width,
                height,
                pts_secs,
            };
            if frame_tx.send(frame_out).is_err() {
                stopped.store(true, Ordering::Relaxed);
                break 'outer;
            }
        }
    }

    let _ = decoder.send_eof();
    tracing::debug!("Video decoder thread (packets) finished");
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_frames_in_order() {
        let (dec, _cmd) = VideoDecoder::open("/tmp/test_v.mp4", 0.0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut f = None;
        while f.is_none() && std::time::Instant::now() < deadline {
            f = dec.recv();
            if f.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let f = f.expect("decoder should produce a frame within 5s");
        assert_eq!(f.data.len(), (f.width * f.height * 4) as usize);
        assert!(f.pts_secs >= 0.0);

        // A second frame must arrive with a non-decreasing PTS — the
        // `in_order` in the test name should mean something.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut f2 = None;
        while f2.is_none() && std::time::Instant::now() < deadline {
            f2 = dec.recv();
            if f2.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let f2 = f2.expect("decoder should produce a second frame within 5s");
        assert!(
            f2.pts_secs >= f.pts_secs,
            "PTS should be non-decreasing: {} then {}",
            f.pts_secs,
            f2.pts_secs
        );
    }
}
