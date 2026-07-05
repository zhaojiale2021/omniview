use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;

struct AudioShared {
    buffer: Mutex<VecDeque<f32>>,
    volume: Mutex<f32>,
    paused: AtomicBool,
    stopped: AtomicBool,
}

pub struct AudioDecoder {
    shared: Arc<AudioShared>,
    _stream: cpal::Stream,
    _thread: Option<thread::JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
}

impl AudioDecoder {
    pub fn open(path: &str, start_secs: f64) -> Result<Self, String> {
        let shared = Arc::new(AudioShared {
            buffer: Mutex::new(VecDeque::with_capacity((SAMPLE_RATE as usize) * 2)),
            volume: Mutex::new(0.8),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });

        // --- cpal output stream ---
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("No audio output device")?;

        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let shared_cb = shared.clone();
        let err_fn = |err| tracing::error!("cpal: {err}");

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if shared_cb.paused.load(Ordering::Relaxed) {
                        for s in data.iter_mut() { *s = 0.0; }
                        return;
                    }
                    let volume = *shared_cb.volume.lock().unwrap();
                    let mut buf = shared_cb.buffer.lock().unwrap();
                    for sample in data.iter_mut() {
                        *sample = buf.pop_front().unwrap_or(0.0) * volume;
                    }
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("Stream: {e}"))?;

        stream.play().map_err(|e| format!("Play: {e}"))?;

        // --- ffmpeg reader thread ---
        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let child_for_thread = child_arc.clone();
        let shared_reader = shared.clone();
        let path_owned = path.to_string();

        let thread_handle = thread::spawn(move || {
            read_audio(&path_owned, start_secs, &shared_reader, &child_for_thread);
        });

        Ok(Self {
            shared,
            _stream: stream,
            _thread: Some(thread_handle),
            child: child_arc,
        })
    }

    pub fn set_volume(&self, v: f32) {
        *self.shared.volume.lock().unwrap() = v.clamp(0.0, 1.0);
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn build_audio_ffmpeg_args(path: &str, start_secs: f64) -> Vec<String> {
    let mut args: Vec<String> = vec!["-v".into(), "quiet".into()];

    if start_secs > 0.01 {
        args.push("-ss".into());
        args.push(format!("{start_secs}"));
    }

    args.push("-re".into());
    args.push("-i".into());
    args.push(path.into());
    args.extend_from_slice(&[
        "-f".into(), "f32le".into(),
        "-acodec".into(), "pcm_f32le".into(),
        "-ac".into(), CHANNELS.to_string(),
        "-ar".into(), SAMPLE_RATE.to_string(),
        "-".into(),
    ]);
    args
}

fn read_audio(
    path: &str,
    start_secs: f64,
    shared: &Arc<AudioShared>,
    child_holder: &Arc<Mutex<Option<Child>>>,
) {
    let args = build_audio_ffmpeg_args(path, start_secs);

    let mut child = match Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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

    *child_holder.lock().unwrap() = Some(child);

    let mut reader = std::io::BufReader::with_capacity(65536, stdout);
    let mut byte_buf = [0u8; 4];
    let chunk_size: usize = 512;

    loop {
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }

        let mut samples = Vec::with_capacity(chunk_size);
        for _ in 0..chunk_size {
            match reader.read_exact(&mut byte_buf) {
                Ok(()) => samples.push(f32::from_le_bytes(byte_buf)),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Drain buffer naturally
                    if !samples.is_empty() {
                        shared.buffer.lock().unwrap().extend(samples.drain(..));
                    }
                    // Wait for buffer to drain
                    while shared.buffer.lock().unwrap().len() > 1024
                        && !shared.stopped.load(Ordering::Relaxed)
                    {
                        thread::sleep(std::time::Duration::from_millis(100));
                    }
                    shared.stopped.store(true, Ordering::Relaxed);
                    return;
                }
                Err(e) => {
                    tracing::error!("Audio read: {e}");
                    return;
                }
            }
        }

        let mut buf = shared.buffer.lock().unwrap();
        // Backpressure: if buffer has >4s of audio, wait
        while buf.len() > (SAMPLE_RATE as usize) * 4 && !shared.stopped.load(Ordering::Relaxed)
        {
            drop(buf);
            thread::sleep(std::time::Duration::from_millis(10));
            if shared.stopped.load(Ordering::Relaxed) {
                return;
            }
            buf = shared.buffer.lock().unwrap();
        }
        buf.extend(samples);
    }

    if let Some(mut c) = child_holder.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        self.stop();
    }
}
