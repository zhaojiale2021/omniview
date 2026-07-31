#[cfg(feature = "mpv")]
mod mpv_player {
    use libmpv::Mpv;

    pub struct Player {
        mpv: Mpv,
    }

    impl Player {
        pub fn open(path: &str) -> Result<Self, String> {
            let mpv = Mpv::new().map_err(|e| format!("mpv: {e}"))?;
            mpv.set_property("vo", "null").map_err(|e| format!("mpv: {e}"))?;
            mpv.command("loadfile", &[path, "replace"])
                .map_err(|e| format!("mpv load: {e}"))?;
            mpv.set_property("pause", false).ok();
            std::thread::sleep(std::time::Duration::from_millis(200));
            let dur: f64 = mpv.get_property("duration").unwrap_or(0.0);
            tracing::info!("mpv ready, dur={dur:.1}s");
            Ok(Self { mpv })
        }
        pub fn set_paused(&self, p: bool) { let _ = self.mpv.set_property("pause", p); }
        pub fn is_paused(&self) -> bool { self.mpv.get_property::<bool>("pause").unwrap_or(false) }
        pub fn seek(&self, s: f64) { let _ = self.mpv.command("seek", &[&format!("{s}"), "absolute"]); }
        pub fn set_speed(&self, s: f64) { let _ = self.mpv.set_property("speed", s); }
        pub fn set_volume(&self, v: f32) { let _ = self.mpv.set_property("volume", (v * 100.0).clamp(0.0, 100.0) as i64); }
        pub fn clock(&self) -> f64 { self.mpv.get_property::<f64>("time-pos").unwrap_or(0.0) }
    }
}

#[cfg(all(feature = "audio-fallback", not(feature = "mpv")))]
mod cpal_player {
    use std::collections::VecDeque;
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
    use std::thread;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    struct Shared {
        buffer: Mutex<VecDeque<f32>>,
        volume: Mutex<f32>,
        paused: AtomicBool,
        stopped: AtomicBool,
        samples_played: AtomicU64,
    }

    pub struct Player {
        shared: Arc<Shared>,
        _stream: cpal::Stream,
        _thread: Option<thread::JoinHandle<()>>,
        child: Arc<Mutex<Option<Child>>>,
    }

    impl Player {
        pub fn open(path: &str) -> Result<Self, String> {
            let shared = Arc::new(Shared {
                buffer: Mutex::new(VecDeque::new()),
                volume: Mutex::new(0.8),
                paused: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                samples_played: AtomicU64::new(0),
            });

            let host = cpal::default_host();
            let dev = host.default_output_device().ok_or("No audio device")?;
            let cfg = cpal::StreamConfig { channels: 2, sample_rate: cpal::SampleRate(48000), buffer_size: cpal::BufferSize::Default };
            let sh = shared.clone();
            let stream = dev.build_output_stream(&cfg,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let n = data.len() as u64;
                    if sh.paused.load(Ordering::Relaxed) { for s in data.iter_mut() { *s = 0.0; } sh.samples_played.fetch_add(n, Ordering::Relaxed); return; }
                    let vol = *sh.volume.lock().unwrap();
                    let mut buf = sh.buffer.lock().unwrap();
                    for s in data.iter_mut() { *s = buf.pop_front().unwrap_or(0.0) * vol; }
                    sh.samples_played.fetch_add(n, Ordering::Relaxed);
                }, |e| tracing::error!("cpal: {e}"), None,
            ).map_err(|e| format!("cpal: {e}"))?;
            stream.play().map_err(|e| format!("cpal play: {e}"))?;

            let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
            let ct = child_arc.clone();
            let sr = shared.clone();
            let p = path.to_string();
            let th = thread::spawn(move || {
                let mut args: Vec<String> = vec!["-v".into(),"quiet".into(),"-re".into(),"-i".into(),p.clone()];
                args.extend_from_slice(&["-f".into(),"f32le".into(),"-acodec".into(),"pcm_f32le".into(),"-ac".into(),"2".into(),"-ar".into(),"48000".into(),"-".into()]);
                let mut child = match Command::new("ffmpeg").args(&args).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() { Ok(c) => c, Err(_) => return };
                let stdout = match child.stdout.take() { Some(s) => s, None => return };
                *ct.lock().unwrap() = Some(child);
                let mut r = std::io::BufReader::with_capacity(65536, stdout);
                let mut bb = [0u8;4];
                loop {
                    if sr.stopped.load(Ordering::Relaxed) { break; }
                    let mut chunk = Vec::with_capacity(512);
                    for _ in 0..512 {
                        match r.read_exact(&mut bb) {
                            Ok(()) => chunk.push(f32::from_le_bytes(bb)),
                            Err(_) => { sr.stopped.store(true, Ordering::Relaxed); return; }
                        }
                    }
                    let mut buf = sr.buffer.lock().unwrap();
                    while buf.len() > 48000*4 && !sr.stopped.load(Ordering::Relaxed) { drop(buf); thread::sleep(std::time::Duration::from_millis(10)); buf = sr.buffer.lock().unwrap(); }
                    buf.extend(chunk);
                }
                if let Some(mut c) = ct.lock().unwrap().take() { let _ = c.kill(); let _ = c.wait(); }
            });

            Ok(Self { shared, _stream: stream, _thread: Some(th), child: child_arc })
        }
        pub fn set_paused(&self, p: bool) { self.shared.paused.store(p, Ordering::Relaxed); }
        pub fn is_paused(&self) -> bool { self.shared.paused.load(Ordering::Relaxed) }
        pub fn seek(&self, _s: f64) {} // seek not implemented for fallback
        pub fn set_speed(&self, _s: f64) {} // speed not implemented for fallback
        pub fn set_volume(&self, v: f32) { *self.shared.volume.lock().unwrap() = v.clamp(0.0, 1.0); }
        pub fn clock(&self) -> f64 {
            let total = self.shared.samples_played.load(Ordering::Relaxed) as f64;
            total / (48000.0 * 2.0)
        }
    }
    impl Drop for Player { fn drop(&mut self) { self.shared.stopped.store(true, Ordering::Relaxed); if let Some(mut c) = self.child.lock().unwrap().take() { let _ = c.kill(); let _ = c.wait(); } } }
}

// Re-export the right player
#[cfg(feature = "mpv")]
pub use mpv_player::Player as AudioPlayer;
#[cfg(all(feature = "audio-fallback", not(feature = "mpv")))]
pub use cpal_player::Player as AudioPlayer;
