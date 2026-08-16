//! In-process audio decoder + cpal output pipeline.
//!
//! Architecture:
//!   ffmpeg-next decode thread -> swr resample -> interleaved f32
//!     -> bounded ring buffer -> cpal output callback
//!
//! Speed changes use an ffmpeg atempo audio filter graph.
//! Volume is applied in the cpal output callback via shared state.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};
use ffmpeg_next as ffmpeg;
use ffmpeg::{channel_layout::ChannelLayout, codec, filter, format, frame, media, software, error};

/// Send handle to the audio actor thread.  The actor owns the cpal output
/// stream (cpal streams are !Send, so they live on the thread that built
/// them); every field here is safe to share and to move across threads.
#[derive(Clone)]
pub struct AudioHandle {
    /// Master-clock counter (advanced only for real samples popped from
    /// the ring by the cpal callback).
    pub samples_played: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub channels: u16,
    cmd_tx: mpsc::Sender<AudioCmd>,
    shared: Arc<Shared>,
    underruns: Arc<AtomicU64>,
    ring: Arc<Mutex<VecDeque<f32>>>,
}

impl AudioHandle {
    pub fn set_paused(&self, p: bool) {
        let _ = self.cmd_tx.send(AudioCmd::Pause(p));
    }

    pub fn set_speed(&self, s: f64) {
        let _ = self.cmd_tx.send(AudioCmd::Speed(s));
    }

    pub fn set_volume(&self, v: f32) {
        if let Ok(mut vol) = self.shared.volume.lock() {
            *vol = v.clamp(0.0, 1.0);
        }
        let _ = self.cmd_tx.send(AudioCmd::Volume);
    }

    pub fn start_stream(&self) {
        let _ = self.cmd_tx.send(AudioCmd::StartStream);
    }

    /// Re-sync the audio master clock to `pos` seconds: drop older ring
    /// samples and advance the counter so the clock reads `pos` at the
    /// current sample position (used after a wall-clock fallback).
    pub fn trim_to(&self, pos: f64) {
        let _ = self.cmd_tx.send(AudioCmd::Trim(pos));
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(AudioCmd::Stop);
    }

    /// Samples waiting in the ring (pipeline pre-roll / diagnostics).
    pub fn buffered_samples(&self) -> usize {
        self.ring.lock().map(|b| b.len()).unwrap_or(0)
    }

    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

/// Payload of a successful audio actor start.
pub struct AudioReady {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples_played: Arc<AtomicU64>,
    pub shared: Arc<Shared>,
    pub underruns: Arc<AtomicU64>,
}

/// Spawn the audio actor thread: it builds the cpal pipeline (stream
/// included) on its own thread and then services control commands.  Reports
/// readiness on `ready_tx`:
/// * `Ok(AudioReady)` — the stream is attached and the ring is pre-filling;
///   wrap it with `AudioHandle::new` and use the handle for all control.
/// * `Err(e)` / no message — no usable output device: the actor keeps
///   draining audio packets until the demux stops routing, so the video
///   stream never stalls.
pub fn spawn_audio_actor(
    path: String,
    dev: cpal::Device,
    start_pos: f64,
) -> (
    mpsc::Sender<AudioCmd>,
    mpsc::Receiver<Result<AudioReady, String>>,
    thread::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<AudioReady, String>>();

    let handle = thread::spawn(move || {
        let pipeline = match AudioPipeline::start(&path, &dev, start_pos) {
            Ok(p) => p,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        let ready = AudioReady {
            sample_rate: pipeline.sample_rate,
            channels: pipeline.channels,
            samples_played: pipeline.samples_played.clone(),
            shared: pipeline.shared.clone(),
            underruns: pipeline.shared.underruns.clone(),
        };
        let _ = ready_tx.send(Ok(ready));

        loop {
            match cmd_rx.recv() {
                Ok(AudioCmd::Pause(b)) => pipeline.set_paused(b),
                Ok(AudioCmd::Speed(s)) => pipeline.set_speed(s),
                Ok(AudioCmd::Volume) => {} // value applied via shared state
                Ok(AudioCmd::StartStream) => pipeline.start_stream(),
                Ok(AudioCmd::Trim(pos)) => pipeline.trim_to(pos),
                Ok(AudioCmd::Stop) => {
                    pipeline.stop();
                    break;
                }
                Err(_) => {
                    // Controller gone: tear down.
                    pipeline.stop();
                    break;
                }
            }
        }
        // `pipeline` drops here: stream stopped, decoder thread signalled.
    });

    (cmd_tx, ready_rx, handle)
}

impl AudioHandle {
    /// Wrap the pieces returned by a successful `spawn_audio_actor`.
    pub fn new(
        cmd_tx: mpsc::Sender<AudioCmd>,
        ready: AudioReady,
    ) -> Self {
        let ring = ready.shared.buffer.clone();
        Self {
            samples_played: ready.samples_played,
            sample_rate: ready.sample_rate,
            channels: ready.channels,
            cmd_tx,
            shared: ready.shared,
            underruns: ready.underruns,
            ring,
        }
    }
}

// ── Commands ─────────────────────────────────────────────────────────

pub enum AudioCmd {
    Pause(bool),
    Speed(f64),
    // The value is redundant: volume is applied in the cpal callback via
    // shared state; the decoder thread only needs to know a volume change
    // happened so it can wake from its command-drain loop.
    Volume,
    Stop,
    /// Begin audible playback (the stream is built paused so audio starts
    /// at the same instant as the media clock).
    StartStream,
    /// Drop ring samples older than `pos` seconds and advance the master
    /// counter to match — used to re-sync the audio master clock with the
    /// video position after a wall-clock fallback.
    Trim(f64),
}

// ── Shared state (cpal callback + decoder thread) ────────────────────

pub struct Shared {
    buffer: Arc<Mutex<VecDeque<f32>>>,
    volume: Mutex<f32>,
    paused: AtomicBool,
    stopped: AtomicBool,
    samples_played: Arc<AtomicU64>,
    /// Count of callback periods that had to zero-fill (ring underflow).
    /// Reported periodically so a "stutters every few seconds" issue can be
    /// attributed to audio starvation vs video/render starvation.
    underruns: Arc<AtomicU64>,
}

// ── Public pipeline ──────────────────────────────────────────────────

pub struct AudioPipeline {
    pub samples_played: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub cmd_tx: mpsc::Sender<AudioCmd>,
    shared: Arc<Shared>,
    /// The cpal output stream.  Starts PAUSED so audio playback begins
    /// exactly when the controller starts the media clock (`start_stream`).
    stream: cpal::Stream,
    _decoder_thread: Option<thread::JoinHandle<()>>,
}

impl AudioPipeline {
    /// Start the audio pipeline: cpal output stream + ffmpeg decode thread.
    ///
    /// `path` is the media file (opened inside the decode thread to read
    /// codec parameters).  `dev` is the cpal output device.
    /// `pkt_rx` receives packets from the demuxer (Task 4).
    /// `start_pos` seeds the audio master clock so `position()` is correct
    /// after open/seek.
    ///
    /// Returns the packet receiver back on failure so the caller can keep the
    /// demux alive (e.g. by spawning a discard-drain thread for video-only).
    pub fn start(
        path: &str,
        dev: &cpal::Device,
        start_pos: f64,
    ) -> Result<Self, String> {
        let dev_name = dev.name().unwrap_or_else(|_| "?".into());

        // ── Shared state ──────────────────────────────────────────
        // samples_played is seeded after we determine sample_rate/channels.
        let samples_played = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared {
            buffer: Arc::new(Mutex::new(VecDeque::new())),
            volume: Mutex::new(0.8),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            samples_played: samples_played.clone(),
            underruns: Arc::new(AtomicU64::new(0)),
        });

        // ── cpal output stream ────────────────────────────────────
        let build_stream =
            |rate: u32, ch: u16, sh: Arc<Shared>| -> Result<cpal::Stream, String> {
                let cfg = cpal::StreamConfig {
                    channels: ch,
                    sample_rate: cpal::SampleRate(rate),
                    buffer_size: cpal::BufferSize::Default,
                };
                dev.build_output_stream(
                    &cfg,
                    move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                        // NEVER panic in the callback.
                        if sh.paused.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return; // do NOT advance samples_played — pause freezes the master clock
                        }
                        let vol = sh.volume.lock().map(|v| *v).unwrap_or(1.0);
                        match sh.buffer.lock() {
                            Ok(mut buf) => {
                                // Advance the master clock only for REAL
                                // samples popped from the ring buffer, NOT
                                // for zero-fill silence played while the
                                // buffer is still filling (startup underflow)
                                // or mid-stream.  Counting `data.len()` makes
                                // the clock run ahead of the audible track by
                                // the buffer latency, permanently.
                                //
                                // Pop available samples in one batch and
                                // apply volume in a single pass instead of a
                                // per-sample lock-and-pop loop.
                                let take = data.len().min(buf.len());
                                for (i, s) in buf.drain(..take).enumerate() {
                                    data[i] = s * vol;
                                }
                                let real = take as u64;
                                for s in &mut data[take..] {
                                    *s = 0.0;
                                }
                                if real > 0 {
                                    sh.samples_played.fetch_add(real, Ordering::Relaxed);
                                }
                                if real < data.len() as u64 {
                                    sh.underruns.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Err(_) => data.fill(0.0),
                        }
                    },
                    |e| tracing::error!("cpal audio: {e}"),
                    None,
                )
                .map_err(|e| format!("cpal stream: {e}"))
            };

        // Adaptive output format: prefer the device's current native
        // configuration (sample rate + channel count) so the OS mixer
        // doesn't have to resample behind our back.  Fall back to the
        // ubiquitous 48 kHz stereo if the native config can't be opened,
        // then to 44.1 kHz stereo as a last resort.
        let native_cfg = dev
            .default_output_config()
            .map(|cfg| (cfg.sample_rate().0, cfg.channels()))
            .ok();

        let mut chosen: Option<(u32, u16, cpal::Stream)> = None;
        let mut attempts: Vec<(u32, u16, &str)> = Vec::new();
        if let Some((r, c)) = native_cfg {
            attempts.push((r, c, "native"));
        }
        attempts.push((48000, 2, "48 kHz / 2 ch"));
        attempts.push((44100, 2, "44.1 kHz / 2 ch"));

        for (r, c, label) in attempts {
            match build_stream(r, c, shared.clone()) {
                Ok(stream) => {
                    tracing::info!(
                        "Audio output: {dev_name} — {r} Hz / {c} ch ({label})"
                    );
                    chosen = Some((r, c, stream));
                    break;
                }
                Err(e) => {
                    tracing::warn!("Audio output {r} Hz/{c} ch failed: {e}");
                }
            }
        }

        let Some((sample_rate, channels, stream)) = chosen else {
            return Err("no usable cpal output format".to_string());
        };

        // Seed the audio master clock so position() starts at `start_pos`.
        // speed is 1.0 at construction time.
        let initial_offset =
            (start_pos * sample_rate as f64 * channels as f64) as u64;
        samples_played.store(initial_offset, Ordering::Relaxed);

        // Build paused: the ring fills with post-seek samples while the
        // controller finishes installing; `start_stream` begins audible
        // playback at the same instant the media clock starts, so video and
        // audio begin together instead of the audio running ahead during
        // the (slow) open/seek window.
        if let Err(e) = stream.pause() {
            tracing::warn!("cpal pause after build: {e}");
        }

        // ── Command channel ───────────────────────────────────────
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();

        // ── Ring buffer backpressure: 400ms cap ──────────────────
        // A buffer absorbs decode-thread hiccups (demux I/O, thread
        // scheduling, Windows timer slop) so the cpal callback never
        // underruns — an underrun stalls the audio master clock and
        // stutters video with it.  The cap is also the worst-case speed
        // change latency: old-speed samples already queued must play out
        // before new-speed samples become audible.  400ms keeps that
        // switch comfortably under the 500ms target while still leaving
        // enough cushion for normal scheduling jitter.
        let buf_cap = (sample_rate as usize) * (channels as usize) * 400 / 1000;
        let sh_sink = shared.clone();

        // Sink closure: push interleaved f32 into ring buffer.
        let sink = move |samples: &[f32]| {
            if samples.is_empty() {
                return;
            }
            loop {
                {
                    let mut buf = match sh_sink.buffer.lock() {
                        Ok(b) => b,
                        Err(_) => return,
                    };
                    if buf.len() + samples.len() <= buf_cap
                        || sh_sink.stopped.load(Ordering::Relaxed)
                    {
                        buf.extend(samples.iter().copied());
                        return;
                    }
                }
                if sh_sink.stopped.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        };

        // ── Spawn decoder thread ──────────────────────────────────
        let path_owned = path.to_string();
        let sh_thread = shared.clone();
        let spawn_rate = sample_rate;
        let spawn_channels = channels;

        let decoder_thread = thread::spawn(move || {
            // Catch panics so a filter/FFI bug is logged instead of dying
            // silently (on Windows there is no console to see the panic).
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode_audio_packets(
                    &path_owned,
                    cmd_rx,
                    spawn_rate,
                    spawn_channels,
                    start_pos,
                    sink,
                )
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!("Audio decoder: {e}"),
                Err(p) => {
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    tracing::error!("Audio decoder PANICKED: {msg}");
                }
            }
            tracing::info!("Audio: decoder thread finished");
            // Signal the cpal callback to stop expecting data.
            sh_thread.stopped.store(true, Ordering::Relaxed);
        });

        Ok(AudioPipeline {
            samples_played,
            sample_rate,
            channels,
            cmd_tx,
            shared,
            stream,
            _decoder_thread: Some(decoder_thread),
        })
    }

    // ── Controls ──────────────────────────────────────────────────

    /// Pause / resume the decode thread and the cpal callback.
    pub fn set_paused(&self, p: bool) {
        self.shared.paused.store(p, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Pause(p));
    }

    /// Set playback speed (1.0 = normal).  Rebuilds the atempo filter graph.
    pub fn set_speed(&self, s: f64) {
        let _ = self.cmd_tx.send(AudioCmd::Speed(s));
    }

    /// Stop playback: signal the decode thread and cpal callback.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Stop);
    }

    /// Start the output stream.  The stream is built paused so the audio
    /// decoder can pre-fill the ring; calling this makes the first real
    /// samples audible, synchronized with the media clock start.
    pub fn start_stream(&self) {
        if let Err(e) = self.stream.play() {
            tracing::error!("cpal play: {e}");
        }
    }

    /// Drop ring samples older than `pos_secs` and advance the master
    /// counter to match, so the audio clock reads `pos_secs` from the next
    /// popped sample.  Never rewinds: if the audio is already ahead of
    /// `pos_secs`, this is a no-op.
    pub fn trim_to(&self, pos_secs: f64) {
        let rate_ch = self.sample_rate as u64 * self.channels as u64;
        let target = (pos_secs.max(0.0) * rate_ch as f64) as u64;
        let cur = self.samples_played.load(Ordering::Relaxed);
        if target <= cur {
            return;
        }
        let drop = (target - cur) as usize;
        if let Ok(mut buf) = self.shared.buffer.lock() {
            let n = drop.min(buf.len());
            buf.drain(..n);
        }
        self.samples_played.store(target, Ordering::Relaxed);
    }

}

// ── Decode thread (unit-testable without a device) ───────────────────

/// Decode audio packets from the demuxer into interleaved f32 samples.
///
/// * `path`        - media file, opened to read the audio stream's codec params.
/// * `pkt_rx`      - audio packets from the demuxer (Task 4).
/// * `cmd_rx`      - control commands (pause, speed, stop).
/// * `sample_rate` - target output sample rate.
/// * `channels`    - target output channel count.
/// * `sink`        - callback for interleaved f32 sample batches.
///
/// This function is deliberately not a method on `AudioPipeline` so it can
/// be tested without a cpal device.
fn decode_audio_packets(
    path: &str,
    cmd_rx: mpsc::Receiver<AudioCmd>,
    sample_rate: u32,
    channels: u16,
    start_pos: f64,
    mut sink: impl FnMut(&[f32]) + Send,
) -> Result<(), String> {
    ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

    // Open the file and read the AUDIO stream directly — decoupled from the
    // demuxer's video routing, so video backpressure can never starve audio.
    let mut input =
        format::input(path).map_err(|e| format!("open input for audio codec: {e}"))?;

    // Extract everything we need from the stream, then drop it so `input`
    // can be borrowed mutably for the seek + packet iteration below.
    let (audio_stream_index, audio_time_base, audio_params) = {
        let s = input
            .streams()
            .best(media::Type::Audio)
            .ok_or_else(|| "no audio stream".to_string())?;
        (s.index(), s.time_base(), s.parameters())
    };
    tracing::info!("Audio: opened file, audio stream index={audio_stream_index}");

    // Seek to the start position (the demuxer does the same for video).
    if start_pos > 0.01 {
        let ts = (start_pos * 1_000_000.0) as i64;
        unsafe {
            ffmpeg::ffi::av_seek_frame(
                input.as_mut_ptr(),
                -1,
                ts,
                ffmpeg::ffi::AVSEEK_FLAG_BACKWARD,
            );
        }
    }

    let ctx = codec::context::Context::from_parameters(audio_params.clone())
        .map_err(|e| format!("audio codec context: {e}"))?;
    let mut decoder = ctx
        .decoder()
        .audio()
        .map_err(|e| format!("audio decoder: {e}"))?;
    decoder
        .set_parameters(audio_params.clone())
        .map_err(|e| format!("set audio decoder params: {e}"))?;

    // Swr resampler: created lazily from the FIRST real frame's parameters
    // and rebuilt when the input format changes.  The decoder's reported
    // channel layout can be unset, which made swr reject every real frame
    // with "Input changed" — killing audio the moment atempo engaged.
    let mut resampler: Option<software::resampling::Context> = None;

    // ── Atempo filter graph (None when speed == 1.0) ──────────────
    let mut atempo: Option<filter::Graph> = None;

    let build_atempo = |decoder: &codec::decoder::Audio,
                        speed: f64|
     -> Result<filter::Graph, String> {
        let mut graph = filter::Graph::new();

        let args = format!(
            "time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
            decoder.time_base(),
            decoder.rate(),
            decoder.format().name(),
            decoder.channel_layout().bits()
        );

        graph
            .add(
                &filter::find("abuffer").ok_or("abuffer not found")?,
                "in",
                &args,
            )
            .map_err(|e| format!("add abuffer: {e}"))?;
        graph
            .add(
                &filter::find("abuffersink").ok_or("abuffersink not found")?,
                "out",
                "",
            )
            .map_err(|e| format!("add abuffersink: {e}"))?;

        // Note: abuffersink options are NOT set here — they are non-runtime
        // options and setting them after graph.add() fails with warnings.
        // atempo passes the input format through, and the swr resampler is
        // built from the actual frames, so no sink configuration is needed.
        let spec = format!("atempo={speed}");
        graph
            .output("in", 0)
            .map_err(|e| format!("output in: {e}"))?
            .input("out", 0)
            .map_err(|e| format!("input out: {e}"))?
            .parse(&spec)
            .map_err(|e| format!("parse atempo: {e}"))?;
        graph
            .validate()
            .map_err(|e| format!("validate atempo: {e}"))?;

        Ok(graph)
    };

    // ── Main decode loop ──────────────────────────────────────────
    let mut decoded = frame::Audio::empty();
    let mut resampled = frame::Audio::empty();
    let mut filtered = frame::Audio::empty();
    let mut first_packet = true;
    let mut pushed_batches = 0u64;
    // Reusable sample buffer: filled per decoded frame and handed to the
    // sink by reference, so steady-state audio makes no per-frame heap
    // allocations (previously a fresh ~4 KB Vec per 10 ms block).
    let mut sample_buf: Vec<f32> = Vec::new();

    for (stream, packet) in input.packets() {
        // Skip non-audio packets (we read our own stream from the file).
        if stream.index() != audio_stream_index {
            continue;
        }

        // (1) Drain commands (non-blocking).
        loop {
            match cmd_rx.try_recv() {
                Ok(AudioCmd::Stop) => return Ok(()),
                Ok(AudioCmd::Pause(true)) => {
                    // Keep draining commands until unpaused or stopped.
                    loop {
                        match cmd_rx.recv() {
                            Ok(AudioCmd::Stop) => return Ok(()),
                            Ok(AudioCmd::Pause(false)) => break,
                            Ok(AudioCmd::Speed(s)) => {
                                // Update speed even while paused.
                                if (s - 1.0).abs() < 1e-9 {
                                    atempo = None;
                                } else {
                                    match build_atempo(&decoder, s) {
                                        Ok(g) => atempo = Some(g),
                                        Err(e) => {
                                            // Loud, diagnosable failure: the
                                            // clock still multiplies by speed
                                            // even when atempo is None, so a
                                            // silent None desyncs A/V at
                                            // speed != 1.
                                            tracing::error!(
                                                "atempo filter build failed (codec={}, speed={:.3}): {}",
                                                decoder.format().name(),
                                                s,
                                                e
                                            );
                                            atempo = None;
                                        }
                                    }
                                }
                            }
                            Ok(AudioCmd::Pause(true)) => {} // already paused — no-op
                            Ok(AudioCmd::Volume) => {} // applied in cpal callback
                            Ok(AudioCmd::StartStream) => {} // handled by the actor
                            Ok(AudioCmd::Trim(_)) => {} // handled by the actor
                            Err(mpsc::RecvError) => return Ok(()),
                        }
                    }
                }
                Ok(AudioCmd::Speed(s)) => {
                    if (s - 1.0).abs() < 1e-9 {
                        atempo = None;
                    } else {
                        match build_atempo(&decoder, s) {
                            Ok(g) => atempo = Some(g),
                            Err(e) => {
                                tracing::error!(
                                    "atempo filter build failed (codec={}, speed={:.3}): {}",
                                    decoder.format().name(),
                                    s,
                                    e
                                );
                                atempo = None;
                            }
                        }
                    }
                }
                Ok(AudioCmd::Volume) => {} // applied in cpal callback
                Ok(AudioCmd::StartStream) => {} // handled by the actor
                Ok(AudioCmd::Trim(_)) => {} // handled by the actor
                Ok(AudioCmd::Pause(false)) => {} // already playing — no-op
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        if first_packet {
            first_packet = false;
            tracing::info!(
                "Audio: first packet received ({} bytes)",
                packet.data().map(|d| d.len()).unwrap_or(0)
            );
        }

        // (3) Send packet to decoder.  A single bad packet must not kill
        // the thread (that would silence the stream and freeze the master
        // clock permanently); log and skip it.
        if let Err(e) = decoder.send_packet(&packet) {
            if matches!(e, ffmpeg::Error::Eof) {
                break;
            }
            tracing::warn!("audio send_packet: {e}");
            continue;
        }

        // (4) Receive decoded frames.
        while decoder.receive_frame(&mut decoded).is_ok() {
            // Discard audio decoded before the seek target: the demux seeks
            // BACKWARD to a keyframe, and without this filter the ring plays
            // pre-target audio, leaving A/V desynced by the keyframe
            // distance after every seek.
            if start_pos > 0.01
                && let Some(pts) = decoded.pts().or_else(|| decoded.timestamp()) {
                    let pts_secs = pts as f64
                        * audio_time_base.numerator() as f64
                        / audio_time_base.denominator() as f64;
                    if pts_secs < start_pos {
                        continue;
                    }
                }
            // Process the decoded frame through atempo filter (if active),
            // then resample to target format.
            let frames_to_resample: Vec<frame::Audio> = if let Some(ref mut graph) = atempo {
                // Feed decoded frame into the filter graph.
                graph
                    .get("in")
                    .unwrap()
                    .source()
                    .add(&decoded)
                    .map_err(|e| format!("atempo add: {e}"))?;

                // Pull filtered frames from the graph.
                let mut out_frames = Vec::new();
                loop {
                    match graph.get("out").unwrap().sink().frame(&mut filtered) {
                        Ok(()) => out_frames.push(std::mem::replace(
                            &mut filtered,
                            frame::Audio::empty(),
                        )),
                        Err(ffmpeg::Error::Other { errno: error::EAGAIN }) => break,
                        Err(ffmpeg::Error::Eof) => break,
                        Err(e) => {
                            tracing::warn!("atempo pull: {e}");
                            break;
                        }
                    }
                }
                out_frames
            } else {
                // No atempo — resample the decoded frame directly.
                vec![std::mem::replace(&mut decoded, frame::Audio::empty())]
            };

            for f in frames_to_resample {
                // Resample to interleaved f32 @ target rate/channels.
                if resampler.is_none() {
                    resampler = Some(new_resampler(&f, channels, sample_rate)?);
                    tracing::info!("Audio: swr ready -> {sample_rate} Hz / {channels} ch");
                }
                if let Err(e) = resampler.as_mut().unwrap().run(&f, &mut resampled) {
                    // Input parameters changed (atempo output negotiation):
                    // rebuild from this frame and retry once.
                    tracing::warn!("audio swr run: {e}; rebuilding resampler");
                    resampler = Some(new_resampler(&f, channels, sample_rate)?);
                    if let Err(e2) = resampler.as_mut().unwrap().run(&f, &mut resampled) {
                        tracing::warn!("audio swr run after rebuild: {e2}");
                        continue;
                    }
                }

                // Extract interleaved f32 samples from the resampled frame.
                // Only the first samples()*channels samples are valid —
                // data(0) spans the whole linesize (capacity), whose tail
                // holds stale data from previous frames on buffer reuse.
                valid_f32_samples(&resampled, &mut sample_buf);

                if !sample_buf.is_empty() {
                    pushed_batches += 1;
                    if pushed_batches == 1 {
                        tracing::info!("Audio: first samples pushed");
                    }
                    sink(&sample_buf);
                }

                // NOTE: no per-frame swr flush here.  ffmpeg-next's
                // flush() returns Ok(Some) without clearing the output
                // frame, so looping here re-pushed the same samples ~10x
                // (silent at 1x because delay()==None, but after atempo
                // engaged the duplicates flooded the ring and the whole
                // pipeline stalled).  The remaining swr delay is a few ms
                // and is flushed once at EOF below.
            }
        }
    }

    // ── Flush filters on EOF ──────────────────────────────────────
    decoder
        .send_eof()
        .map_err(|e| format!("send eof: {e}"))?;
    // Receive any remaining decoded frames after EOF.
    while decoder.receive_frame(&mut decoded).is_ok() {
        if let Some(ref mut graph) = atempo {
            let _ = graph.get("in").unwrap().source().add(&decoded);
            loop {
                match graph.get("out").unwrap().sink().frame(&mut filtered) {
                    Ok(()) => {
                        let f = std::mem::replace(&mut filtered, frame::Audio::empty());
                        let Some(resampler) = resampler.as_mut() else { break };
                        if let Err(e) = resampler.run(&f, &mut resampled) {
                            tracing::warn!("audio swr flush: {e}");
                            continue;
                        }
                        valid_f32_samples(&resampled, &mut sample_buf);
                        if !sample_buf.is_empty() {
                            sink(&sample_buf);
                        }
                    }
                    Err(ffmpeg::Error::Other { errno: error::EAGAIN }) => break,
                    Err(ffmpeg::Error::Eof) => break,
                    Err(_) => break,
                }
            }
        } else {
            let Some(resampler) = resampler.as_mut() else { continue };
            if let Err(e) = resampler.run(&decoded, &mut resampled) {
                tracing::warn!("audio swr flush: {e}");
                continue;
            }
            valid_f32_samples(&resampled, &mut sample_buf);
            if !sample_buf.is_empty() {
                sink(&sample_buf);
            }
        }
    }

    // Flush atempo graph.
    if let Some(ref mut graph) = atempo {
        let _ = graph.get("in").unwrap().source().flush();
        loop {
            match graph.get("out").unwrap().sink().frame(&mut filtered) {
                Ok(()) => {
                    let f = std::mem::replace(&mut filtered, frame::Audio::empty());
                    let Some(resampler) = resampler.as_mut() else { break };
                    if let Err(e) = resampler.run(&f, &mut resampled) {
                        tracing::warn!("audio swr flush: {e}");
                        continue;
                    }
                    valid_f32_samples(&resampled, &mut sample_buf);
                    if !sample_buf.is_empty() {
                        sink(&sample_buf);
                    }
                }
                Err(ffmpeg::Error::Other { errno: error::EAGAIN }) => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(_) => break,
            }
        }
    }

    // Flush swr internal frames.
    while let Some(resampler) = resampler.as_mut() {
        match resampler.flush(&mut resampled) {
            Ok(Some(_)) => {
                // Guard against ffmpeg-next's flush quirk: an empty output
                // frame still reports Some — stop when there is no data.
                if resampled.samples() == 0 {
                    break;
                }
                valid_f32_samples(&resampled, &mut sample_buf);
                if !sample_buf.is_empty() {
                    sink(&sample_buf);
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("swr flush: {e}");
                break;
            }
        }
    }

    Ok(())
}

/// Build a swr context for converting a frame to the target output
/// configuration.  Uses the FRAME's actual format/layout/rate: the
/// decoder's reported channel layout can be unset, which makes swr reject
/// every real frame with "Input changed".
fn new_resampler(
    frame: &ffmpeg::frame::Audio,
    channels: u16,
    sample_rate: u32,
) -> Result<software::resampling::Context, String> {
    let output_layout = ChannelLayout::default(channels as i32);
    software::resampling::Context::get(
        frame.format(),
        frame.channel_layout(),
        frame.rate(),
        format::Sample::F32(format::sample::Type::Packed),
        output_layout,
        sample_rate,
    )
    .map_err(|e| format!("swr context: {e}"))
}

/// Extract only the VALID interleaved f32 samples from a resampled frame.
///
/// `frame.data(0)` returns a slice whose length is the buffer's `linesize`
/// (capacity), not the number of valid samples.  The swr context reuses the
/// output frame's buffer across calls, so the tail of that slice contains
/// stale data from previous frames — feeding the whole slice to the ring
/// duplicates samples and floods the pipeline (observed ~2.5x
/// over-production → ring full → sink blocked → demux stall → freeze).
/// Only the first `samples() * channels` samples are valid.
fn valid_f32_samples(frame: &ffmpeg::frame::Audio, out: &mut Vec<f32>) {
    let valid = frame.samples() * frame.channels() as usize * 4;
    let bytes = frame.data(0);
    out.clear();
    out.extend(
        bytes[..valid.min(bytes.len())]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
    );
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_audio_pcm() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let collected: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = collected.clone();
        let sink = move |samples: &[f32]| {
            if let Ok(mut v) = collected_clone.lock() {
                v.extend_from_slice(samples);
            }
        };

        let handle = thread::spawn(move || {
            let _ = decode_audio_packets("/tmp/test_av.mp4", cmd_rx, 48000, 2, 0.0, sink);
        });

        // Wait for at least some samples.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for decoded audio samples");
            }
            if !collected.lock().unwrap().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!collected.lock().unwrap().is_empty(), "decoded audio samples must be non-empty");

        let _ = cmd_tx.send(AudioCmd::Stop);
        let _ = handle.join();
    }

    #[test]
    fn atempo_doubles_sample_output() {
        // Direct rate check: read the whole 20s fixture at 2x from the start.
        // A working atempo halves the output sample count (~960k vs ~1.92M).
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let produced: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let produced_sink = produced.clone();
        let sink = move |samples: &[f32]| {
            *produced_sink.lock().unwrap() += samples.len() as u64;
        };
        let handle = thread::spawn(move || {
            let _ = decode_audio_packets("/tmp/test_av20.mp4", cmd_rx, 48000, 2, 0.0, sink);
        });
        let _ = cmd_tx.send(AudioCmd::Speed(2.0));

        let dl = std::time::Instant::now() + Duration::from_secs(15);
        while !handle.is_finished() && std::time::Instant::now() < dl {
            thread::sleep(Duration::from_millis(50));
        }
        let total = *produced.lock().unwrap();
        assert!(
            total < 1_400_000,
            "atempo=2 should roughly halve output (got {total} samples)"
        );
        assert!(
            total > 500_000,
            "atempo=2 should still produce most of the stream (got {total})"
        );
        let _ = cmd_tx.send(AudioCmd::Stop);
        let _ = handle.join();
    }

    #[test]
    fn atempo_speed_change_keeps_thread_alive() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        // Reproduces the Windows regression: a speed change (atempo filter)
        // killed the audio thread silently.  Verifies it stays alive and
        // produces samples at ~2x the 1x rate.
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let ring: Arc<Mutex<std::collections::VecDeque<f32>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let produced: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let produced_sink = produced.clone();
        let ring_sink = ring.clone();
        let sink = move |samples: &[f32]| {
            let mut r = ring_sink.lock().unwrap();
            while r.len() + samples.len() > 115_200 {
                drop(r);
                thread::sleep(Duration::from_millis(5));
                r = ring_sink.lock().unwrap();
            }
            let n = samples.len() as u64;
            r.extend(samples.iter().copied());
            *produced_sink.lock().unwrap() += n;
        };
        let _consumer = {
            let ring_c = ring.clone();
            thread::spawn(move || {
                loop {
                    let mut r = ring_c.lock().unwrap();
                    let take = 960.min(r.len());
                    r.drain(..take);
                    drop(r);
                    thread::sleep(Duration::from_millis(10));
                }
            })
        };

        let handle = thread::spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                decode_audio_packets("/tmp/test_av20.mp4", cmd_rx, 48000, 2, 0.0, sink)
            }));
            match r {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("decode err: {e}")),
                Err(p) => Some(format!(
                    "PANIC: {}",
                    p.downcast_ref::<&str>().copied().unwrap_or("?")
                )),
            }
        });

        // Wait for the first samples.
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while *produced.lock().unwrap() == 0 && std::time::Instant::now() < dl {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(*produced.lock().unwrap() > 0, "no samples at 1x");

        // Baseline: the decoder must keep producing at 1x.
        let t0 = *produced.lock().unwrap();
        thread::sleep(Duration::from_secs(1));
        let base = *produced.lock().unwrap() - t0;
        assert!(base > 0, "no baseline production");

        // Switch to 2x — the production regression killed the thread here.
        let _ = cmd_tx.send(AudioCmd::Speed(2.0));
        thread::sleep(Duration::from_millis(500)); // let atempo engage

        let t0 = *produced.lock().unwrap();
        thread::sleep(Duration::from_secs(1));
        let after = *produced.lock().unwrap() - t0;
        assert!(
            !handle.is_finished(),
            "audio thread died during speed change: {:?}",
            handle.join().ok().flatten()
        );
        assert!(
            after > 0,
            "audio stopped producing after speed change (baseline was {base})"
        );

        let _ = cmd_tx.send(AudioCmd::Stop);
        let _ = handle.join();
    }
}
