//! In-process video decoder using FFmpeg libraries (ffmpeg-next).
//!
//! The decoder consumes packets from the demuxer's packet channel and
//! emits RGBA `VideoFrame`s into a bounded frame queue (capacity 3), so the
//! render loop picks one frame per vsync and decode is paced by consumption.
//!
//! Codec parameters (time_base, fps, width, height) are read by briefly
//! opening the file inside the decode thread; the demux owns all seeking
//! and packet routing.

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
