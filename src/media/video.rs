//! In-process video decoder using FFmpeg libraries (ffmpeg-next).
//!
//! The decoder consumes packets from the demuxer's packet channel and
//! emits NV12 `VideoFrame`s into a bounded frame queue.  Decoding runs at
//! full speed; the queue is the jitter buffer and the render loop picks the
//! frame that matches the media clock.  YUV→RGB conversion happens on the
//! GPU (shader), so the CPU never touches a full RGB frame.
//!
//! Codec parameters (time_base, fps, width, height) are read by briefly
//! opening the file inside the decode thread; the demux owns all seeking
//! and packet routing.

use std::collections::VecDeque;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};

use crate::media::types::VideoFrame;

/// Bounded jitter buffer with peek semantics: the renderer pops only the
/// frames whose PTS has been reached by the media clock and leaves the rest
/// queued, so decode-ahead never causes frames to be discarded.
const FRAME_QUEUE_CAP: usize = 48;

/// A reusable NV12 plane allocation (one Y or UV plane), shared via `Arc`.
type PlaneBuffer = Arc<Vec<u8>>;

pub struct VideoQueue {
    frames: Mutex<VecDeque<VideoFrame>>,
    space: Condvar,
}

impl Default for VideoQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoQueue {
    pub fn new() -> Self {
        Self {
            frames: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
        }
    }

    /// Push a decoded frame, blocking while the queue is full (backpressure
    /// paces decode to consumption).  Returns immediately if `stopped`.
    pub fn push(&self, frame: VideoFrame, stopped: &AtomicBool) {
        let mut q = self.frames.lock().unwrap();
        while q.len() >= FRAME_QUEUE_CAP && !stopped.load(Ordering::Relaxed) {
            q = self.space.wait(q).unwrap();
        }
        if stopped.load(Ordering::Relaxed) {
            return;
        }
        q.push_back(frame);
        self.space.notify_one();
    }

    /// Number of frames currently buffered ahead of the clock.
    pub fn len(&self) -> usize {
        self.frames.lock().unwrap().len()
    }

    /// True when no frames are buffered ahead of the clock.
    #[allow(dead_code)] // used by library consumers; binary never calls it directly
    pub fn is_empty(&self) -> bool {
        self.frames.lock().unwrap().is_empty()
    }

    /// Pop every frame whose PTS is at/before `clock` and return the newest
    /// one popped (frames in between are skipped, which is correct when the
    /// clock is ahead, e.g. after a speed change or resume).  Also returns
    /// how many frames remain buffered ahead of the clock.
    pub fn drain_upto(&self, clock: f64) -> (Option<VideoFrame>, usize) {
        let mut q = self.frames.lock().unwrap();
        let mut last = None;
        while let Some(f) = q.front() {
            if f.pts_secs <= clock {
                last = q.pop_front();
            } else {
                break;
            }
        }
        if !q.is_empty() {
            self.space.notify_one();
        }
        (last, q.len())
    }

    /// Wake a decoder thread blocked waiting for queue space.  Used on
    /// teardown: the decoder cannot drain its command channel while blocked
    /// in `push`, so the controller pokes the condvar after sending Stop.
    pub fn wake(&self) {
        self.space.notify_all();
    }
}

#[derive(Debug, Clone)]
pub enum DecoderCmd {
    Pause,
    Resume,
    Stop,
}

pub struct VideoDecoder {
    queue: Arc<VideoQueue>,
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
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let paused = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(VideoQueue::new());

        let st = stopped.clone();
        let pa = paused.clone();
        let p = path.to_string();
        let ct = cmd_tx.clone();
        let q = queue.clone();

        thread::spawn(move || {
            decode_packets_loop(&p, start_pos, pkt_rx, q, cmd_rx, st, pa);
        });

        (Self { queue }, ct)
    }

    /// Pop the frames the media clock has reached.
    pub fn drain_upto(&self, clock: f64) -> (Option<VideoFrame>, usize) {
        self.queue.drain_upto(clock)
    }

    /// Number of decoded frames waiting ahead of the clock (diagnostics).
    pub fn buffered(&self) -> usize {
        self.queue.len()
    }

    /// Wake the decoder thread if it is blocked waiting for queue space.
    pub fn interrupt(&self) {
        self.queue.wake();
    }
}

// ── Decode-from-packet-channel loop (driven by Demux) ─────────────

fn decode_packets_loop(
    path: &str,
    start_pos: f64,
    pkt_rx: mpsc::Receiver<ffmpeg::codec::packet::Packet>,
    queue: Arc<VideoQueue>,
    command_rx: mpsc::Receiver<DecoderCmd>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
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
        // NV12 keeps the CPU on the planar side (no RGB matrix); the GPU
        // shader converts to RGB.  Half the upload bytes of RGBA.
        format::Pixel::NV12,
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

    let mut nv12 = frame::Video::empty();
    let mut first_packet = true;
    let mut frames_sent = 0u64;

    // Frame pool: reusable plane-buffer slots.  The queue holds up to
    // FRAME_QUEUE_CAP frames plus one held by the renderer, so a slot's
    // Arc is private again by the time we cycle back to it — `Arc::make_mut`
    // reuses the allocation and we never allocate/free ~3 MB per frame
    // (large-block allocator churn causes periodic hitches).
    // 52 slots > queue cap 48 + one frame in the renderer + one in flight.
    let mut frame_pool: Vec<(PlaneBuffer, PlaneBuffer)> = (0..52)
        .map(|_| (Arc::new(Vec::new()), Arc::new(Vec::new())))
        .collect();
    let mut next_slot = 0usize;

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
            Ok(p) => {
                if first_packet {
                    first_packet = false;
                    tracing::info!("Video: first packet received");
                }
                p
            }
            Err(_) => {
                tracing::warn!("Video: packet channel closed (demux stopped or EOF)");
                break;
            }
        };
        if let Err(e) = decoder.send_packet(&packet) {
            if matches!(e, ffmpeg::Error::Eof) {
                break;
            }
            tracing::debug!("send_packet: {e}");
            continue;
        }

        let mut decoded = frame::Video::empty();
        let mut corrupt_dropped = 0u64;
        let mut corrupt_log = std::time::Instant::now();
        while decoder.receive_frame(&mut decoded).is_ok() {
            // Skip frames the decoder flagged as corrupt (missing/broken
            // references).  Showing them produces green/blocky artifacts;
            // dropping them just skips a frame.  Some encodes (B-frame
            // heavy streams) flag these intermittently.
            if decoded
                .flags()
                .contains(ffmpeg::util::frame::flag::Flags::CORRUPT)
            {
                corrupt_dropped += 1;
                if corrupt_log.elapsed().as_secs() >= 5 {
                    tracing::warn!("Video: dropped {corrupt_dropped} corrupt frames in last 5s");
                    corrupt_dropped = 0;
                    corrupt_log = std::time::Instant::now();
                }
                continue;
            }
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

            if let Err(e) = scaler.run(&decoded, &mut nv12) {
                tracing::debug!("scale: {e}");
                continue;
            }

            // Container pts first: best_effort_timestamp can drift
            // after a seek+stall on VFR streams (observed ~0.57s label
            // offset), while the packet pts is the ground truth.
            let pts_secs = decoded
                .pts()
                .or_else(|| decoded.timestamp())
                .map(|p| p as f64 * time_base.numerator() as f64 / time_base.denominator() as f64)
                .unwrap_or(0.0);

            // Discard frames decoded before the seek target.
            if start_pos > 0.01 && pts_secs < start_pos {
                continue;
            }

            let slot = next_slot;
            next_slot = (next_slot + 1) % frame_pool.len();
            let (y_arc, uv_arc) = &mut frame_pool[slot];
            let y_buf = Arc::make_mut(y_arc);
            y_buf.clear();
            let y_stride = pad_plane_into(
                nv12.data(0),
                nv12.stride(0),
                nv12.height(),
                nv12.width() as usize,
                y_buf,
            );
            let uv_buf = Arc::make_mut(uv_arc);
            uv_buf.clear();
            let uv_width = nv12.width().div_ceil(2) as usize;
            let uv_height = nv12.height().div_ceil(2);
            let uv_stride = pad_plane_into(
                nv12.data(1),
                nv12.stride(1),
                uv_height,
                uv_width * 2,
                uv_buf,
            );
            frames_sent += 1;
            let y_checksum = crate::media::types::fnv1a(y_buf);
            let frame_out = VideoFrame {
                y: y_arc.clone(),
                uv: uv_arc.clone(),
                width: nv12.width(),
                height: nv12.height(),
                y_stride,
                uv_stride,
                pts_secs,
                y_checksum,
            };
            queue.push(frame_out, &stopped);
        }
    }

    let _ = decoder.send_eof();
    tracing::info!("Video: decode loop finished (frames_sent={frames_sent})");
}

/// Copy a decoded plane row-by-row into `out` with rows aligned to 256
/// bytes — the alignment `wgpu::Queue::write_texture` requires for
/// `bytes_per_row`.  The output buffer comes from the decoder's frame pool,
/// so steady-state playback performs no per-frame allocations.
fn pad_plane_into(
    src: &[u8],
    src_stride: usize,
    height: u32,
    row_bytes: usize,
    out: &mut Vec<u8>,
) -> u32 {
    debug_assert!(
        row_bytes <= src_stride,
        "row_bytes {row_bytes} > src_stride {src_stride}"
    );
    let padded = row_bytes.div_ceil(256) * 256;
    let need = padded * height as usize;
    if out.len() < need {
        out.resize(need, 0);
    }
    for r in 0..height as usize {
        out[r * padded..r * padded + row_bytes]
            .copy_from_slice(&src[r * src_stride..r * src_stride + row_bytes]);
    }
    padded as u32
}
