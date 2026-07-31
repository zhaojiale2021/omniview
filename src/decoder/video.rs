//! Video frame decoder using ffmpeg CLI pipe.
//!
//! Architecture:
//!   ffmpeg (paced with `-readrate`, so it emits frames at exactly the
//!   playback rate) → raw RGBA pipe → decoder thread → bounded channel
//!   (capacity 2) → main thread picks one frame per vsync.
//!
//! The bounded channel is the backpressure: the decoder blocks when the
//! display is behind, so it never runs more than a frame or two ahead.
//! Combined with `-readrate {speed}`, decode work is bounded to exactly
//! what playback consumes — no CPU overproduction, no memory pile-up.
//!
//! Metadata probing (ffprobe) runs inside the decoder thread, so
//! `open()` never blocks the main thread — seeks don't freeze the UI.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::thread;

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

/// Sent once, after the ffprobe metadata step, before decoding starts.
#[derive(Debug, Clone)]
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
}

// ── Metadata probe (runs on the decoder thread) ────────────────────

fn probe_metadata(path: &str) -> Result<ReadyInfo, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;

    if !output.status.success() {
        return Err(format!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr)));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("json: {e}"))?;

    let streams = json["streams"].as_array().ok_or("No streams")?;
    let vs = streams.iter().find(|s| s["codec_type"] == "video").ok_or("No video stream")?;

    let width = vs["width"].as_u64().unwrap_or(0) as u32;
    let height = vs["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        return Err("Invalid dimensions".into());
    }

    let duration_secs = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let fps = vs["avg_frame_rate"]
        .as_str()
        .or_else(|| vs["r_frame_rate"].as_str())
        .and_then(|s| {
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() == 2 {
                let num: f64 = parts[0].parse().ok()?;
                let den: f64 = parts[1].parse().ok()?;
                if den > 0.0 { Some(num / den) } else { None }
            } else {
                parts[0].parse::<f64>().ok()
            }
        })
        .unwrap_or(30.0);

    Ok(ReadyInfo { width, height, fps, duration_secs })
}

// ── Decoder ─────────────────────────────────────────────────────────

impl VideoDecoder {
    /// Spawn the decoder thread.  Returns immediately — the thread does
    /// the ffprobe probe first and reports via `poll_ready()`.
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

        let st = stopped.clone();
        let pa = paused.clone();
        let p = path.to_string();
        let cmd_tx = command_tx.clone();

        let handle = thread::spawn(move || {
            decode_loop(&p, speed, start_pos, frame_tx, ready_tx, command_rx, st, pa);
        });

        (
            Self {
                frame_rx,
                ready_rx,
                command_tx,
                _thread: Some(handle),
                stopped,
                paused,
            },
            cmd_tx,
        )
    }

    /// Non-blocking poll for the metadata result.
    pub fn poll_ready(&self) -> Option<Result<ReadyInfo, String>> {
        self.ready_rx.try_recv().ok()
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
    speed: f64,
    start_pos: f64,
    frame_tx: SyncSender<DecodedFrame>,
    ready_tx: mpsc::Sender<Result<ReadyInfo, String>>,
    command_rx: mpsc::Receiver<DecoderCommand>,
    stopped: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) {
    // Probe first (ffprobe), report, then decode.
    let info = match probe_metadata(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!("Video probe failed: {e}");
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    if stopped.load(Ordering::Relaxed) {
        return;
    }
    tracing::info!(
        "Video: {}x{} @ {:.2}fps, {:.1}s",
        info.width, info.height, info.fps, info.duration_secs
    );
    let _ = ready_tx.send(Ok(info.clone()));

    let frame_size = (info.width as usize) * (info.height as usize) * 4;
    let fps = info.fps.max(0.1);

    // Pacer: `-readrate {speed}` makes ffmpeg emit frames at exactly
    // `speed × content rate`, so decode work matches playback exactly.
    let mut args: Vec<String> = vec!["-v".into(), "quiet".into()];

    if start_pos > 0.01 {
        args.push("-ss".into());
        args.push(format!("{start_pos}"));
    }
    args.push("-readrate".into());
    args.push(format!("{speed}"));

    args.push("-i".into());
    args.push(path.into());

    args.extend_from_slice(&[
        "-f".into(), "rawvideo".into(),
        "-pix_fmt".into(), "rgba".into(),
        "-vsync".into(), "0".into(),
        "-".into(),
    ]);

    let mut child = match Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // e.g. ffmpeg not found on PATH — the usual "no video" cause.
            tracing::error!("ffmpeg video spawn failed: {e}");
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return,
    };

    // Collect ffmpeg stderr for diagnostics (e.g. "Unrecognized option
    // 'readrate'" on old builds, or "No such file").  A thread avoids
    // the pipe filling up and deadlocking while we read stdout.
    let child_stderr = child.stderr.take();
    let stderr_handle = child_stderr.map(|mut stderr| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, stdout);
    // Two alternating buffers: never clone a whole frame, never
    // reallocate in the common case.
    let mut bufs = [vec![0u8; frame_size], Vec::new()];
    let mut b = 0usize;
    let mut frame_count: u64 = (start_pos * fps) as u64;

    loop {
        // Process commands
        while let Ok(cmd) = command_rx.try_recv() {
            match cmd {
                DecoderCommand::Stop => {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
                DecoderCommand::Pause => paused.store(true, Ordering::Relaxed),
                DecoderCommand::Resume => paused.store(false, Ordering::Relaxed),
            }
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }

        if paused.load(Ordering::Relaxed) {
            thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        // Ensure this slot has a buffer (it may have been moved out
        // on the previous send while still in flight).
        if bufs[b].len() != frame_size {
            bufs[b] = vec![0u8; frame_size];
        }

        match reader.read_exact(&mut bufs[b]) {
            Ok(()) => {
                frame_count += 1;
                // Move the buffer into the Arc (no copy); the slot
                // becomes empty and is re-filled above when needed.
                let frame = DecodedFrame {
                    data: Arc::new(std::mem::take(&mut bufs[b])),
                    width: info.width,
                    height: info.height,
                    pts_secs: frame_count as f64 / fps,
                };
                b ^= 1;
                // Blocking send = backpressure: never run more than 2
                // frames ahead of the display.
                if frame_tx.send(frame).is_err() {
                    stopped.store(true, Ordering::Relaxed);
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                tracing::error!("Video read error: {e}");
                stopped.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    // Report ffmpeg's stderr — catches missing files, unsupported
    // options (e.g. old ffmpeg without `-readrate`), etc.
    if let Some(h) = stderr_handle {
        if let Ok(err) = h.join() {
            let msg = String::from_utf8_lossy(&err);
            let msg = msg.trim();
            if !msg.is_empty() {
                tracing::warn!("ffmpeg video stderr: {msg}");
            }
        }
    }
    tracing::debug!("Video decoder thread finished");
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(path: &str) -> ReadyInfo {
        probe_metadata(path).unwrap()
    }

    #[test]
    fn test_probe_metadata() {
        let info = probe("/tmp/test_360.mp4");
        // Any sane 360 clip: width = 2×height, nonzero fps/duration.
        assert!(info.width > 0 && info.height > 0);
        assert!((info.width as f64 / info.height as f64 - 2.0).abs() < 0.01);
        assert!(info.duration_secs > 0.0);
        assert!(info.fps > 0.0);
    }

    #[test]
    fn test_decode_frames() {
        let (dec, _cmd) = VideoDecoder::open("/tmp/test_360.mp4", 1.0, 0.0);
        let ready = dec.poll_ready();
        // Ready arrives after the async probe (~200ms).
        let _ = ready.unwrap_or_else(|| {
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
        // First frame arrives quickly with -readrate pacing.
        let frame = dec.frame_rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(frame.data.len(), frame.width as usize * frame.height as usize * 4);
        dec.stop();
    }
}
