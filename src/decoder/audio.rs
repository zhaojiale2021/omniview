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

        // cpal stream
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("No audio output device")?;
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
                if shared_cb.paused.load(Ordering::Relaxed) {
                    for s in data.iter_mut() { *s = 0.0; }
                    return;
                }
                let vol = *shared_cb.volume.lock().unwrap();
                let mut buf = shared_cb.buffer.lock().unwrap();
                for s in data.iter_mut() {
                    *s = buf.pop_front().unwrap_or(0.0) * vol;
                }
            },
            err_fn,
            None,
        ).map_err(|e| format!("cpal stream: {e}"))?;

        stream.play().map_err(|e| format!("cpal play: {e}"))?;

        // Reader thread
        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let child_t = child_arc.clone();
        let shared_r = shared.clone();
        let p = path.to_string();

        let th = thread::spawn(move || {
            let mut args: Vec<String> = vec![
                "-v".into(), "quiet".into(), "-re".into(),
            ];
            if start_secs > 0.01 {
                args.push("-ss".into());
                args.push(format!("{start_secs}"));
            }
            args.push("-i".into());
            args.push(p.into());
            args.extend_from_slice(&[
                "-f".into(), "f32le".into(),
                "-acodec".into(), "pcm_f32le".into(),
                "-ac".into(), CHANNELS.to_string(),
                "-ar".into(), SAMPLE_RATE.to_string(),
                "-".into(),
            ]);

            let mut child = match Command::new("ffmpeg")
                .args(&args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            {
                Ok(c) => c,
                Err(e) => { tracing::error!("ffmpeg audio: {e}"); return; }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => { tracing::error!("ffmpeg audio: no stdout"); return; }
            };
            *child_t.lock().unwrap() = Some(child);

            let mut rdr = std::io::BufReader::with_capacity(65536, stdout);
            let mut bb = [0u8; 4];
            loop {
                if shared_r.stopped.load(Ordering::Relaxed) { break; }
                let mut chunk = Vec::with_capacity(512);
                for _ in 0..512 {
                    match rdr.read_exact(&mut bb) {
                        Ok(()) => chunk.push(f32::from_le_bytes(bb)),
                        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            if !chunk.is_empty() {
                                shared_r.buffer.lock().unwrap().extend(chunk);
                            }
                            while shared_r.buffer.lock().unwrap().len() > 1024
                                && !shared_r.stopped.load(Ordering::Relaxed)
                            { thread::sleep(std::time::Duration::from_millis(100)); }
                            shared_r.stopped.store(true, Ordering::Relaxed);
                            return;
                        }
                        Err(e) => { tracing::error!("audio read: {e}"); return; }
                    }
                }
                let mut buf = shared_r.buffer.lock().unwrap();
                while buf.len() > (SAMPLE_RATE as usize) * 4
                    && !shared_r.stopped.load(Ordering::Relaxed)
                {
                    drop(buf);
                    thread::sleep(std::time::Duration::from_millis(10));
                    if shared_r.stopped.load(Ordering::Relaxed) { return; }
                    buf = shared_r.buffer.lock().unwrap();
                }
                buf.extend(chunk);
            }
            if let Some(mut c) = child_t.lock().unwrap().take() {
                let _ = c.kill(); let _ = c.wait();
            }
        });

        Ok(Self { shared, _stream: stream, _thread: Some(th), child: child_arc })
    }

    pub fn set_volume(&self, v: f32) {
        *self.shared.volume.lock().unwrap() = v.clamp(0.0, 1.0);
    }
    pub fn set_paused(&self, p: bool) {
        self.shared.paused.store(p, Ordering::Relaxed);
    }
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill(); let _ = c.wait();
        }
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) { self.stop(); }
}
