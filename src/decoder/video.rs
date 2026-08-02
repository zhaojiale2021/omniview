//! In-process video decoder using FFmpeg libraries (ffmpeg-next).
//!
//! Unlike the old ffmpeg-CLI subprocess + OS-pipe approach, decoding
//! happens inside this process.  A pause simply stops pulling packets
//! from the demuxer (the decode position is frozen), and a resume
//! continues from exactly where it paused — instant, position-preserved,
//! and no pipe to stall on Windows.  This is how native players work.
//!
//! The decoder thread feeds a bounded channel (capacity 2); the main
//! thread picks one frame per vsync, so decode is paced by consumption.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::thread;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};

// ── Public types ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_secs: f64,
}

#[derive(Debug, Clone)]
pub enum DecoderCommand {
    Pause,
    Resume,
    Stop,
}

/// Sent once, after the streams are probed, before decoding starts.
#[derive(Debug, Clone)]
#[allow(dead_code)] // width/height/fps are part of the public API
pub struct ReadyInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_secs: f64,
}

pub struct VideoDecoder {
    pub frame_rx: mpsc::Receiver<DecodedFrame>,
    ready_rx: mpsc::Receiver<Result<ReadyInfo, String>>,
    command_tx: mpsc::Sender<DecoderCommand>,
    _thread: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    /// Playback speed (f64 bit-cast).  Changed dynamically so speed
    /// changes don't require restarting the decoder.
    speed_secs: Arc<AtomicU64>,
}

impl VideoDecoder {
    /// Spawn the decoder thread.  Returns immediately — the thread probes
    /// the streams first and reports via `poll_ready()`.
    pub fn open(
        path: &str,
        speed: f64,
        start_pos: f64,
    ) -> (Self, mpsc::Sender<DecoderCommand>) {
        // Bounded channel — capacity 2 is the backpressure that stops
        // the decoder from running ahead of the display.
        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(2);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (command_tx, command_rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let speed_secs = Arc::new(AtomicU64::new(speed.to_bits()));

        let st = stopped.clone();
        let pa = paused.clone();
        let spd = speed_secs.clone();
        let p = path.to_string();
        let cmd_tx = command_tx.clone();

        let handle = thread::spawn(move || {
            decode_loop(&p, start_pos, frame_tx, ready_tx, command_rx, st, pa, spd);
        });

        (
            Self {
                frame_rx,
                ready_rx,
                command_tx,
                _thread: Some(handle),
                stopped,
                paused,
                speed_secs,
            },
            cmd_tx,
        )
    }

    /// Non-blocking poll for the metadata result.
    pub fn poll_ready(&self) -> Option<Result<ReadyInfo, String>> {
        self.ready_rx.try_recv().ok()
    }

    /// Change playback speed live (no restart — the decode thread reads
    /// this each frame to pace itself).
    pub fn set_speed(&self, speed: f64) {
        self.speed_secs.store(speed.to_bits(), Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        let _ = self.command_tx.send(DecoderCommand::Stop);
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── Decode loop (runs in background thread) ────────────────────────

fn decode_loop(
    path: &str,
    start_pos: f64,
    frame_tx: SyncSender<DecodedFrame>,
    ready_tx: mpsc::Sender<Result<ReadyInfo, String>>,
    command_rx: mpsc::Receiver<DecoderCommand>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    speed_secs: Arc<AtomicU64>,
) {
    if let Err(e) = ffmpeg::init() {
        tracing::error!("ffmpeg init: {e}");
        let _ = ready_tx.send(Err(format!("ffmpeg init: {e}")));
        return;
    }

    let mut input = match format::input(&path) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("Open input failed: {e}");
            let _ = ready_tx.send(Err(format!("Open input failed: {e}")));
            return;
        }
    };

    let stream = match input.streams().best(media::Type::Video) {
        Some(s) => s,
        None => {
            let msg = "No video stream".to_string();
            let _ = ready_tx.send(Err(msg.clone()));
            tracing::error!("{msg}");
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
            let msg = format!("Open decoder failed: {e}");
            let _ = ready_tx.send(Err(msg.clone()));
            tracing::error!("{msg}");
            return;
        }
    };
    let mut decoder = match ctx.decoder().video() {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("Open video decoder failed: {e}");
            let _ = ready_tx.send(Err(msg.clone()));
            tracing::error!("{msg}");
            return;
        }
    };
    let (width, height) = (decoder.width(), decoder.height());

    // Seek to the keyframe at or before the requested start position
    // (BACKWARD), then discard decoded frames until pts >= start_pos.
    // Frames decoded before the target are dropped in the main loop.
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
            let msg = format!("swscale init failed: {e}");
            let _ = ready_tx.send(Err(msg.clone()));
            tracing::error!("{msg}");
            return;
        }
    };

    tracing::info!("Video: {width}x{height} @ {fps:.2}fps, {duration_secs:.1}s");
    let _ = ready_tx.send(Ok(ReadyInfo {
        width,
        height,
        fps,
        duration_secs,
    }));

    // Reusable output frame for the RGB conversion.
    let mut rgb = frame::Video::empty();

    // Pace the decode to the content rate × current speed (like
    // ffmpeg's -readrate), so frames are produced at playback speed
    // rather than flooding ahead of the display clock.  The speed is
    // read from the shared atomic so speed changes are live (no
    // decoder restart).
    let mut last_speed = f64::from_bits(speed_secs.load(Ordering::Relaxed)).max(0.1);
    let mut next_frame_at = std::time::Instant::now();

    let mut packets = input.packets();
    'outer: loop {
        // Process commands.
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                DecoderCommand::Stop => {
                    stopped.store(true, Ordering::Relaxed);
                    break 'outer;
                }
                DecoderCommand::Pause => paused.store(true, Ordering::Relaxed),
                DecoderCommand::Resume => {
                    paused.store(false, Ordering::Relaxed);
                    // Restart the pacing clock on resume: after a long
                    // pause the old schedule is far in the past, which
                    // would make the decoder flood frames to catch up.
                    next_frame_at = std::time::Instant::now();
                }
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
            while let Ok(cmd) = command_rx.try_recv() {
                match cmd {
                    DecoderCommand::Stop => {
                        stopped.store(true, Ordering::Relaxed);
                        break 'outer;
                    }
                    DecoderCommand::Pause => paused.store(true, Ordering::Relaxed),
                    DecoderCommand::Resume => paused.store(false, Ordering::Relaxed),
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
            let frame_out = DecodedFrame {
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
    tracing::debug!("Video decoder thread finished");
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_frames() {
        let (dec, _cmd) = VideoDecoder::open("/tmp/test_360.mp4", 1.0, 0.0);
        let _ = dec.poll_ready().unwrap_or_else(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(r) = dec.poll_ready() {
                    return r;
                }
                if std::time::Instant::now() > deadline {
                    panic!("probe timeout");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let frame = dec.frame_rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(frame.data.len(), frame.width as usize * frame.height as usize * 4);
        dec.stop();
    }
}
