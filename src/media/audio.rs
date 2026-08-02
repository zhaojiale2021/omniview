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
use ffmpeg::codec::packet::Packet;
use ffmpeg::{channel_layout::ChannelLayout, codec, filter, format, frame, media, software, error};

// ── Commands ─────────────────────────────────────────────────────────

pub enum AudioCmd {
    Pause(bool),
    Speed(f64),
    Volume(f32),
    Stop,
}

// ── Shared state (cpal callback + decoder thread) ────────────────────

struct Shared {
    buffer: Mutex<VecDeque<f32>>,
    volume: Mutex<f32>,
    paused: AtomicBool,
    stopped: AtomicBool,
    samples_played: Arc<AtomicU64>,
}

// ── Public pipeline ──────────────────────────────────────────────────

pub struct AudioPipeline {
    pub samples_played: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub cmd_tx: mpsc::Sender<AudioCmd>,
    shared: Arc<Shared>,
    _stream: cpal::Stream,
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
        pkt_rx: mpsc::Receiver<Packet>,
        start_pos: f64,
    ) -> Result<Self, (String, mpsc::Receiver<Packet>)> {
        let dev_name = dev.name().unwrap_or_else(|_| "?".into());

        // Wrap so we can return it on error paths.
        let mut pkt_rx = Some(pkt_rx);

        // ── Shared state ──────────────────────────────────────────
        // samples_played is seeded after we determine sample_rate/channels.
        let samples_played = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared {
            buffer: Mutex::new(VecDeque::new()),
            volume: Mutex::new(0.8),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            samples_played: samples_played.clone(),
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
                        let n = data.len() as u64;
                        if sh.paused.load(Ordering::Relaxed) {
                            data.fill(0.0);
                            return; // do NOT advance samples_played — pause freezes the master clock
                        }
                        sh.samples_played.fetch_add(n, Ordering::Relaxed);
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
                    |e| tracing::error!("cpal audio: {e}"),
                    None,
                )
                .map_err(|e| format!("cpal stream: {e}"))
            };

        // Prefer 48 kHz / 2 ch; fall back to device native config.
        let (stream, sample_rate, channels) =
            match build_stream(48000u32, 2u16, shared.clone()) {
                Ok(s) => {
                    tracing::info!("Audio output: {dev_name} — 48 kHz / 2 ch");
                    (s, 48000u32, 2u16)
                }
                Err(e) => {
                    tracing::warn!("48 kHz/2ch failed ({e}); trying device native config");
                    let (r, c) = match dev.default_output_config() {
                        Ok(cfg) => (cfg.sample_rate().0, cfg.channels()),
                        Err(e2) => {
                            tracing::warn!("no native config ({e2}); using 48 kHz/2ch");
                            (48000u32, 2u16)
                        }
                    };
                    tracing::info!("Audio output: {dev_name} — {r} Hz / {c} ch (native)");
                    let s = match build_stream(r, c, shared.clone()) {
                        Ok(s) => s,
                        Err(e) => {
                            return Err((
                                format!("cpal stream: {e}"),
                                pkt_rx.take().unwrap(),
                            ));
                        }
                    };
                    (s, r, c)
                }
            };

        // Seed the audio master clock so position() starts at `start_pos`.
        // speed is 1.0 at construction time.
        let initial_offset =
            (start_pos * sample_rate as f64 * channels as f64) as u64;
        samples_played.store(initial_offset, Ordering::Relaxed);

        stream.play().map_err(|e| {
            let msg = format!("cpal play: {e}");
            (msg, pkt_rx.take().unwrap())
        })?;

        // ── Command channel ───────────────────────────────────────
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();

        // ── Ring buffer backpressure: 200ms cap ───────────────────
        let buf_cap = (sample_rate as usize) * (channels as usize) * 200 / 1000;
        let sh_sink = shared.clone();

        // Sink closure: push interleaved f32 into ring buffer.
        let sink = move |samples: Vec<f32>| {
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
                        buf.extend(&samples);
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

        let rx = pkt_rx.take().unwrap();
        let decoder_thread = thread::spawn(move || {
            if let Err(e) = decode_audio_packets(
                &path_owned,
                rx,
                cmd_rx,
                spawn_rate,
                spawn_channels,
                sink,
            ) {
                tracing::error!("Audio decoder: {e}");
            }
            // Signal the cpal callback to stop expecting data.
            sh_thread.stopped.store(true, Ordering::Relaxed);
        });

        Ok(AudioPipeline {
            samples_played,
            sample_rate,
            channels,
            cmd_tx,
            shared,
            _stream: stream,
            _decoder_thread: Some(decoder_thread),
        })
    }

    // ── Controls ──────────────────────────────────────────────────

    /// Monotonic sample count delivered to the output device.
    pub fn samples_played(&self) -> u64 {
        self.shared.samples_played.load(Ordering::Relaxed)
    }

    /// Pause / resume the decode thread and the cpal callback.
    pub fn set_paused(&self, p: bool) {
        self.shared.paused.store(p, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Pause(p));
    }

    /// Set playback speed (1.0 = normal).  Rebuilds the atempo filter graph.
    pub fn set_speed(&self, s: f64) {
        let _ = self.cmd_tx.send(AudioCmd::Speed(s));
    }

    /// Set output volume.  Applied in the cpal callback via shared state.
    pub fn set_volume(&self, v: f32) {
        let clamped = v.clamp(0.0, 1.0);
        *self.shared.volume.lock().unwrap() = clamped;
        let _ = self.cmd_tx.send(AudioCmd::Volume(clamped));
    }

    /// Stop playback: signal the decode thread and cpal callback.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(AudioCmd::Stop);
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
    pkt_rx: mpsc::Receiver<Packet>,
    cmd_rx: mpsc::Receiver<AudioCmd>,
    sample_rate: u32,
    channels: u16,
    mut sink: impl FnMut(Vec<f32>) + Send,
) -> Result<(), String> {
    ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;

    // Open the file just to read the audio stream's codec parameters.
    let input =
        format::input(path).map_err(|e| format!("open input for audio codec: {e}"))?;

    let audio_stream = input
        .streams()
        .best(media::Type::Audio)
        .ok_or_else(|| "no audio stream".to_string())?;

    let ctx = codec::context::Context::from_parameters(audio_stream.parameters())
        .map_err(|e| format!("audio codec context: {e}"))?;
    let mut decoder = ctx
        .decoder()
        .audio()
        .map_err(|e| format!("audio decoder: {e}"))?;
    decoder
        .set_parameters(audio_stream.parameters())
        .map_err(|e| format!("set audio decoder params: {e}"))?;

    // Swr resampler: decoder native format -> interleaved f32 @ target rate/channels.
    let output_layout = ChannelLayout::default(channels as i32);
    let mut resampler = software::resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.rate(),
        format::Sample::F32(format::sample::Type::Packed),
        output_layout,
        sample_rate,
    )
    .map_err(|e| format!("swr context: {e}"))?;

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

        {
            let mut out = graph.get("out").unwrap();
            out.set_sample_format(decoder.format());
            out.set_channel_layout(decoder.channel_layout());
            out.set_sample_rate(decoder.rate());
        }

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

    loop {
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
                                            tracing::warn!("atempo rebuild: {e}");
                                            atempo = None;
                                        }
                                    }
                                }
                            }
                            Ok(AudioCmd::Pause(true)) => {} // already paused — no-op
                            Ok(AudioCmd::Volume(_)) => {} // applied in cpal callback
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
                                tracing::warn!("atempo rebuild: {e}");
                                atempo = None;
                            }
                        }
                    }
                }
                Ok(AudioCmd::Volume(_)) => {} // applied in cpal callback
                Ok(AudioCmd::Pause(false)) => {} // already playing — no-op
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // (2) Get the next packet from the demuxer.
        let packet = match pkt_rx.recv() {
            Ok(p) => p,
            Err(_) => break, // EOF / channel closed
        };

        // (3) Send packet to decoder.
        decoder
            .send_packet(&packet)
            .map_err(|e| format!("send packet: {e}"))?;

        // (4) Receive decoded frames.
        while decoder.receive_frame(&mut decoded).is_ok() {
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
                resampler
                    .run(&f, &mut resampled)
                    .map_err(|e| format!("swr run: {e}"))?;

                // Extract interleaved f32 samples from the resampled frame.
                // data(0) is the interleaved f32 plane.
                let bytes = resampled.data(0);
                let samples: Vec<f32> = bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();

                if !samples.is_empty() {
                    sink(samples);
                }

                // Flush swr internal frames.
                loop {
                    match resampler.flush(&mut resampled) {
                        Ok(Some(_)) => {
                            let flush_bytes = resampled.data(0);
                            let flush_samples: Vec<f32> = flush_bytes
                                .chunks_exact(4)
                                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                                .collect();
                            if !flush_samples.is_empty() {
                                sink(flush_samples);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!("swr flush: {e}");
                            break;
                        }
                    }
                }
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
                        resampler
                            .run(&f, &mut resampled)
                            .map_err(|e| format!("swr run: {e}"))?;
                        let bytes = resampled.data(0);
                        let samples: Vec<f32> = bytes
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect();
                        if !samples.is_empty() {
                            sink(samples);
                        }
                    }
                    Err(ffmpeg::Error::Other { errno: error::EAGAIN }) => break,
                    Err(ffmpeg::Error::Eof) => break,
                    Err(_) => break,
                }
            }
        } else {
            resampler
                .run(&decoded, &mut resampled)
                .map_err(|e| format!("swr run: {e}"))?;
            let bytes = resampled.data(0);
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            if !samples.is_empty() {
                sink(samples);
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
                    resampler
                        .run(&f, &mut resampled)
                        .map_err(|e| format!("swr run: {e}"))?;
                    let bytes = resampled.data(0);
                    let samples: Vec<f32> = bytes
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    if !samples.is_empty() {
                        sink(samples);
                    }
                }
                Err(ffmpeg::Error::Other { errno: error::EAGAIN }) => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(_) => break,
            }
        }
    }

    // Flush swr internal frames.
    loop {
        match resampler.flush(&mut resampled) {
            Ok(Some(_)) => {
                let flush_bytes = resampled.data(0);
                let flush_samples: Vec<f32> = flush_bytes
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                if !flush_samples.is_empty() {
                    sink(flush_samples);
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

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::demux::Demux;

    #[test]
    fn decodes_audio_pcm() {
        // Open the test file via Demux to get the audio packet channel.
        let mut demux = Demux::open("/tmp/test_av.mp4", 0.0);

        // Wait for probe to complete.
        let info = loop {
            if let Some(r) = demux.poll_ready() {
                break r.unwrap();
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert!(info.has_audio, "test file must have an audio stream");

        // Take the channels; we only need audio.
        let (_video_rx, audio_rx) = demux.take_channels().unwrap();

        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCmd>();
        let collected: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = collected.clone();

        // Sink: just accumulate all decoded samples.
        let sink = move |samples: Vec<f32>| {
            if let Ok(mut v) = collected_clone.lock() {
                v.extend(&samples);
            }
        };

        // Spawn the decode thread.
        let handle = thread::spawn(move || {
            let _ = decode_audio_packets(
                "/tmp/test_av.mp4",
                audio_rx,
                cmd_rx,
                48000,
                2,
                sink,
            );
        });

        // Wait for at least some samples (deadline 5 s, poll every 20 ms).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for decoded audio samples");
            }
            let len = collected.lock().unwrap().len();
            if len > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        // Assert we got PCM data.
        let final_samples = collected.lock().unwrap().len();
        assert!(final_samples > 0, "decoded audio samples must be non-empty");

        // Send Stop so the decode thread exits cleanly.
        let _ = cmd_tx.send(AudioCmd::Stop);
        let _ = handle.join();
    }
}
