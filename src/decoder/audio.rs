use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig,
};

const PCM_SAMPLE_RATE: u32 = 48000;
const PCM_CHANNELS: u16 = 2;
const CHUNK_SIZE: usize = 2048; // samples per chunk

/// Audio decoder + player. Spawns an ffmpeg process that outputs raw f32 PCM
/// to stdout, reads it in a background thread, and feeds it to cpal.
pub struct AudioDecoder {
    stopped: Arc<AtomicBool>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

impl AudioDecoder {
    pub fn open(path: &str) -> Result<Self, String> {
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = stopped.clone();
        let path = path.to_string();

        let thread_handle = Some(thread::spawn(move || {
            Self::play_loop(&path, stopped_clone);
        }));

        Ok(Self { stopped, thread_handle })
    }

    fn play_loop(path: &str, stopped: Arc<AtomicBool>) {
        // Get default audio output device
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                tracing::warn!("No audio output device found");
                return;
            }
        };

        let config = StreamConfig {
            channels: PCM_CHANNELS,
            sample_rate: cpal::SampleRate(PCM_SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        // Channel: raw PCM float chunks from reader → cpal callback
        let (chunk_tx, chunk_rx) = mpsc::sync_channel::<Vec<f32>>(128);

        // Shared flag to signal EOF to the audio callback
        let eof = Arc::new(AtomicBool::new(false));
        let eof_cb = eof.clone();

        let err_fn = |err| tracing::error!("Audio stream error: {err}");

        let stream = match device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                // Fill output buffer from the chunk channel
                let mut chunk_buf: Option<Vec<f32>> = None;
                for sample in data.chunks_mut(PCM_CHANNELS as usize) {
                    // Get a new chunk if we've exhausted the current one
                    if chunk_buf.as_ref().map(|b| b.is_empty()).unwrap_or(true) {
                        chunk_buf = match chunk_rx.try_recv() {
                            Ok(chunk) => Some(chunk),
                            Err(mpsc::TryRecvError::Empty) => {
                                if eof_cb.load(Ordering::Relaxed) {
                                    return; // EOF, leave remaining samples as silence
                                }
                                // No data yet — write silence and retry
                                for s in sample.iter_mut() {
                                    *s = 0.0;
                                }
                                continue;
                            }
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        };
                    }
                    if let Some(ref mut buf) = chunk_buf {
                        for s in sample.iter_mut() {
                            *s = buf.drain(..1).next().unwrap_or(0.0);
                        }
                    }
                }
            },
            err_fn,
            None,
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to build output stream: {e}");
                return;
            }
        };

        if let Err(e) = stream.play() {
            tracing::error!("Failed to start stream: {e}");
            return;
        }

        // Spawn ffmpeg to decode audio
        let mut child = match Command::new("ffmpeg")
            .args([
                "-v", "quiet",
                "-i", path,
                "-f", "f32le",
                "-acodec", "pcm_f32le",
                "-ac", &PCM_CHANNELS.to_string(),
                "-ar", &PCM_SAMPLE_RATE.to_string(),
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn ffmpeg audio: {e}");
                return;
            }
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => return,
        };

        let mut reader = std::io::BufReader::with_capacity(65536, stdout);
        // Read CHUNK_SIZE floats at a time (4 bytes each)
        let mut byte_buf = vec![0u8; CHUNK_SIZE * 4];
        let mut chunk = Vec::with_capacity(CHUNK_SIZE);

        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }

            chunk.clear();
            // Read enough bytes for one chunk
            for _ in 0..CHUNK_SIZE {
                if reader.read_exact(&mut byte_buf[..4]).is_err() {
                    eof.store(true, Ordering::Relaxed);
                    break;
                }
                chunk.push(f32::from_le_bytes([
                    byte_buf[0], byte_buf[1], byte_buf[2], byte_buf[3],
                ]));
            }

            if chunk.is_empty() {
                eof.store(true, Ordering::Relaxed);
                break;
            }

            if chunk_tx.send(chunk.clone()).is_err() {
                break; // receiver dropped (stream closed)
            }
        }

        eof.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        tracing::info!("Audio playback finished");
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}
