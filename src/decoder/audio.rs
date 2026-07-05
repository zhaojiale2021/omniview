use std::collections::VecDeque;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    StreamConfig,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;

struct AudioShared {
    buffer: Mutex<VecDeque<f32>>,
    volume: Mutex<f32>,
    stopped: AtomicBool,
    eof: AtomicBool,
    speed: Mutex<f64>,
}

pub struct AudioDecoder {
    shared: Arc<AudioShared>,
    _stream: cpal::Stream,
    _thread: Option<thread::JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
}

impl AudioDecoder {
    pub fn open(path: &str, speed: f64) -> Result<Self, String> {
        let shared = Arc::new(AudioShared {
            buffer: Mutex::new(VecDeque::with_capacity((SAMPLE_RATE as usize) * 2)),
            volume: Mutex::new(0.8),
            stopped: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            speed: Mutex::new(speed),
        });

        // --- Create cpal stream first ---
        let host = cpal::default_host();
        let device = host.default_output_device()
            .ok_or("No audio output device")?;

        let config = StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };

        let shared_cb = shared.clone();
        let err_fn = |err| tracing::error!("cpal: {err}");

        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let volume = *shared_cb.volume.lock().unwrap();
                let mut buf = shared_cb.buffer.lock().unwrap();
                for sample in data.iter_mut() {
                    *sample = buf.pop_front().unwrap_or(0.0) * volume;
                }
            },
            err_fn,
            None,
        ).map_err(|e| format!("Stream error: {e}"))?;

        stream.play().map_err(|e| format!("Play error: {e}"))?;

        // --- Spawn ffmpeg reader thread ---
        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let child_for_thread = child_arc.clone();
        let shared_reader = shared.clone();
        let path_owned = path.to_string();

        let thread_handle = thread::spawn(move || {
            run_ffmpeg_reader(&path_owned, speed, &shared_reader, &child_for_thread);
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

    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn run_ffmpeg_reader(
    path: &str,
    speed: f64,
    shared: &Arc<AudioShared>,
    child_holder: &Arc<Mutex<Option<Child>>>,
) {
    // Build args: -re for real-time, optional atempo for speed
    let mut args: Vec<String> = vec![
        "-v".into(), "quiet".into(),
        "-re".into(),
    ];

    if (speed - 1.0).abs() > 0.01 {
        args.push("-af".into());
        args.push(format!("atempo={speed}"));
    }

    args.push("-i".into());
    args.push(path.into());
    args.extend_from_slice(&[
        "-f".into(), "f32le".into(),
        "-acodec".into(), "pcm_f32le".into(),
        "-ac".into(), CHANNELS.to_string(),
        "-ar".into(), SAMPLE_RATE.to_string(),
        "-".into(),
    ]);

    let mut child = match Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("ffmpeg audio spawn: {e}");
            shared.eof.store(true, Ordering::Relaxed);
            return;
        }
    };

    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            shared.eof.store(true, Ordering::Relaxed);
            return;
        }
    };

    *child_holder.lock().unwrap() = Some(child);

    let mut reader = std::io::BufReader::with_capacity(65536, stdout);
    let mut byte_buf = [0u8; 4];
    let chunk_size = 512; // push 512 samples at a time for smooth buffering
    let mut samples = Vec::with_capacity(chunk_size);

    loop {
        if shared.stopped.load(Ordering::Relaxed) {
            break;
        }

        samples.clear();
        for _ in 0..chunk_size {
            if let Err(e) = reader.read_exact(&mut byte_buf) {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    shared.eof.store(true, Ordering::Relaxed);
                } else {
                    tracing::error!("Audio read: {e}");
                }
                // Push remaining samples and exit
                if !samples.is_empty() {
                    shared.buffer.lock().unwrap().extend(samples.drain(..));
                }
                // Let the buffer drain naturally
                thread::sleep(Duration::from_millis(500));
                shared.eof.store(true, Ordering::Relaxed);
                return;
            }
            samples.push(f32::from_le_bytes(byte_buf));
        }

        // Push to ring buffer; if full, wait briefly
        {
            let mut buf = shared.buffer.lock().unwrap();
            while buf.len() > (SAMPLE_RATE as usize) * 4 {
                // Buffer has >4 seconds of audio; wait for cpal to drain
                drop(buf);
                if shared.stopped.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
                buf = shared.buffer.lock().unwrap();
            }
            buf.extend(samples.drain(..));
        }
    }

    // Cleanup
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
