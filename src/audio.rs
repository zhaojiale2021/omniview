//! Audio player using ffmpeg PCM pipe + cpal output.
//!
//! Architecture:
//!   ffmpeg -re -i <file> -f f32le -ac 2 -ar 48000 pipe:stdout
//!     → reader thread fills a ring buffer (VecDeque<f32>)
//!     → cpal output callback drains the ring buffer
//!
//! The audio clock (samples_played) is the master clock for A/V sync.
//! Seek and speed changes are handled by restarting the ffmpeg process.

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Fallback sample rate when the device reports no native config.
const DEFAULT_SAMPLE_RATE: u32 = 48000;
const DEFAULT_CHANNELS: u32 = 2;

// ── Shared state between cpal callback and reader thread ──────────────
struct Shared {
    buffer: Mutex<VecDeque<f32>>,
    volume: Mutex<f32>,
    paused: AtomicBool,
    /// Signals the reader thread to stop; also set on EOF.
    stopped: AtomicBool,
    /// Total f32 samples delivered to the output device (monotonic).
    /// Clock = samples_played / SAMPLES_PER_SEC
    samples_played: AtomicU64,
}

pub struct AudioPlayer {
    shared: Arc<Shared>,
    _stream: cpal::Stream,
    _reader: Option<thread::JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
    sample_rate: u32,
    channels: u16,
    samples_per_sec: u64,
}

impl AudioPlayer {
    // ── Construction ──────────────────────────────────────────────────

    /// Open a video file for audio playback.
    ///
    /// * `path`   – file path passed to ffmpeg `-i`
    /// * `speed`  – 1.0 = normal; uses `atempo` filter for ≠1
    /// * `seek`   – start position in seconds (0 = beginning)
    pub fn open(path: &str, speed: f64, seek: f64) -> Result<Self, String> {
        let host = cpal::default_host();
        let dev = host
            .default_output_device()
            .ok_or("No audio output device")?;
        let dev_name = dev.name().unwrap_or_else(|_| "?".into());

        // ── Choose the stream format from the DEVICE's native config ──
        // WASAPI in shared mode is happiest when we match the device's
        // own sample rate / channel count; forcing 48 kHz on a device
        // that natively runs at another rate can leave the event-driven
        // callback never firing.
        let (sample_rate, channels) = match dev.default_output_config() {
            Ok(cfg) => {
                tracing::info!(
                    "Audio output device: {dev_name} — native {} Hz, {} ch, {:?}",
                    cfg.sample_rate().0,
                    cfg.channels(),
                    cfg.sample_format(),
                );
                (cfg.sample_rate().0, cfg.channels())
            }
            Err(e) => {
                tracing::warn!("No native config for {dev_name} ({e}); using 48 kHz/2ch");
                (DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS as u16)
            }
        };

        let samples_per_sec = (sample_rate as u64) * (channels as u64);

        let shared = Arc::new(Shared {
            buffer: Mutex::new(VecDeque::new()),
            volume: Mutex::new(0.8),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            samples_played: AtomicU64::new((seek * samples_per_sec as f64) as u64),
        });

        // ── cpal output stream ────────────────────────────────────
        // `BufferSize::Default` lets WASAPI pick a device-aligned buffer
        // size.  `Fixed(256)` can stall the callback on some devices.
        let cfg = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let sh = shared.clone();
        let stream = dev
            .build_output_stream(
                &cfg,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    // NEVER panic in the audio callback: a panic here
                    // (e.g. a poisoned lock from another thread) kills
                    // the cpal stream and silences audio permanently.
                    let n = data.len() as u64;
                    sh.samples_played.fetch_add(n, Ordering::Relaxed);
                    if sh.paused.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let vol = sh.volume.lock().map(|v| *v).unwrap_or(1.0);
                    match sh.buffer.lock() {
                        Ok(mut buf) => {
                            for sample in data.iter_mut() {
                                *sample = buf.pop_front().unwrap_or(0.0) * vol;
                            }
                        }
                        Err(_) => data.fill(0.0),
                    }
                },
                |e| tracing::error!("cpal: {e}"),
                None,
            )
            .map_err(|e| format!("cpal stream: {e}"))?;

        stream.play().map_err(|e| format!("cpal play: {e}"))?;

        // ── ffmpeg reader thread ──────────────────────────────────
        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let sh2 = shared.clone();
        let ct = child_arc.clone();
        let p = path.to_string();
        let (rrate, rch) = (sample_rate, channels);

        let reader = thread::spawn(move || {
            run_reader(&p, speed, seek, rrate, rch, sh2, ct);
        });

        Ok(Self {
            shared,
            _stream: stream,
            _reader: Some(reader),
            child: child_arc,
            sample_rate,
            channels,
            samples_per_sec,
        })
    }

    // ── Controls ──────────────────────────────────────────────────────

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    /// 0.0 – 1.0
    pub fn set_volume(&self, v: f32) {
        *self.shared.volume.lock().unwrap() = v.clamp(0.0, 1.0);
    }

    /// Audio health probe for the DIAG log: (samples_played, buffered
    /// samples).  samples_played only advances when the cpal output
    /// callback fires — a frozen value means the stream isn't running.
    pub fn diagnostics(&self) -> (u64, usize) {
        let played = self.shared.samples_played.load(Ordering::Relaxed);
        let buffered = self.shared.buffer.lock().map(|b| b.len()).unwrap_or(0);
        (played, buffered)
    }

    /// Restart the ffmpeg reader (for seek / speed changes).
    /// Kills the old ffmpeg, clears the buffer, resets clock, spawns a new reader.
    pub fn restart(&mut self, path: &str, speed: f64, seek: f64) {
        // Signal old reader to stop, then kill ffmpeg.  Killing the
        // process closes the pipe, so the old reader thread exits on
        // its next read — no need to sleep on the main thread.
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }

        // Reset state for new reader
        self.shared.samples_played
            .store((seek * self.samples_per_sec as f64) as u64, Ordering::Relaxed);
        self.shared.buffer.lock().unwrap().clear();
        self.shared.stopped.store(false, Ordering::Relaxed);

        // Spawn new reader
        let sh = self.shared.clone();
        let ct = self.child.clone();
        let p = path.to_string();
        let (rrate, rch) = (self.sample_rate, self.channels);
        self._reader = Some(thread::spawn(move || {
            run_reader(&p, speed, seek, rrate, rch, sh, ct);
        }));
    }

}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

// ── ffmpeg reader loop ──────────────────────────────────────────────

fn run_reader(
    path: &str,
    speed: f64,
    seek: f64,
    sample_rate: u32,
    channels: u16,
    shared: Arc<Shared>,
    child_arc: Arc<Mutex<Option<Child>>>,
) {
    let mut args: Vec<String> = vec!["-v".into(), "quiet".into()];

    // Real-time pacing for normal speed
    if (speed - 1.0).abs() < 0.01 {
        args.push("-re".into());
    }

    if seek > 0.01 {
        args.push("-ss".into());
        args.push(format!("{seek}"));
    }

    args.push("-i".into());
    args.push(path.into());

    // Audio speed filter
    if (speed - 1.0).abs() >= 0.01 {
        args.push("-filter:a".into());
        args.push(format!("atempo={speed}"));
    }

    args.extend_from_slice(&[
        "-f".into(), "f32le".into(),
        "-acodec".into(), "pcm_f32le".into(),
        "-ac".into(), channels.to_string(),
        "-ar".into(), sample_rate.to_string(),
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
            tracing::error!("ffmpeg audio spawn: {e}");
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return,
    };

    // Collect ffmpeg audio stderr for diagnostics (hidden decode errors,
    // missing audio stream, etc.).
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    *child_arc.lock().unwrap() = Some(child);

    let mut reader = std::io::BufReader::with_capacity(65536, stdout);
    let mut buf_bytes = [0u8; 4]; // one f32

    // Read samples in chunks of 512 (= ~5.3ms at 48kHz stereo)
    loop {
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }

        let mut chunk: Vec<f32> = Vec::with_capacity(512);
        for _ in 0..512 {
            match reader.read_exact(&mut buf_bytes) {
                Ok(()) => chunk.push(f32::from_le_bytes(buf_bytes)),
                Err(_) => {
                    // EOF — stream ended naturally
                    tracing::info!("Audio stream ended");
                    shared.stopped.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }

        if chunk.is_empty() {
            break;
        }

        // Backpressure: sleep if buffer is too full (2 seconds max)
        let buf_cap = (sample_rate as usize) * (channels as usize) * 2;
        loop {
            let buf = shared.buffer.lock().unwrap();
            if buf.len() <= buf_cap || shared.stopped.load(Ordering::Relaxed) {
                break;
            }
            drop(buf);
            thread::sleep(std::time::Duration::from_millis(10));
        }

        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }

        shared.buffer.lock().unwrap().extend(&chunk);
    }

    // Cleanup: kill ffmpeg if we're the ones stopping
    if let Some(mut c) = child_arc.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }

    // Report ffmpeg's stderr — catches decode errors, missing audio
    // streams, unsupported codecs, etc.
    if let Some(h) = stderr_handle {
        if let Ok(err) = h.join() {
            let msg = String::from_utf8_lossy(&err);
            let msg = msg.trim();
            if !msg.is_empty() {
                tracing::warn!("ffmpeg audio stderr: {msg}");
            }
        }
    }
}
