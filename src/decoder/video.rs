use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::thread;

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub pts_secs: f64,
}

#[derive(Debug, Clone)]
pub enum DecoderCommand {
    Pause,
    Resume,
    Seek(f64),
    Stop,
}

#[allow(dead_code)]
pub struct VideoDecoder {
    pub frame_rx: mpsc::Receiver<DecodedFrame>,
    command_tx: mpsc::Sender<DecoderCommand>,
    thread_handle: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
    pub width: u32,
    pub height: u32,
    pub duration_secs: f64,
    pub fps: f64,
}

/// Query video metadata using ffprobe.
fn probe_metadata(path: &str) -> Result<(u32, u32, f64, f64), String> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            path,
        ])
        .output()
        .map_err(|e| format!("Failed to run ffprobe: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffprobe failed: {stderr}"));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("ffprobe JSON parse: {e}"))?;

    let streams = json["streams"]
        .as_array()
        .ok_or("No streams found")?;

    let video_stream = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or("No video stream")?;

    let width = video_stream["width"].as_u64().unwrap_or(0) as u32;
    let height = video_stream["height"].as_u64().unwrap_or(0) as u32;

    if width == 0 || height == 0 {
        return Err("Invalid video dimensions".into());
    }

    let duration_secs = json["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    // FPS from avg_frame_rate or r_frame_rate
    let fps = video_stream["avg_frame_rate"]
        .as_str()
        .or_else(|| video_stream["r_frame_rate"].as_str())
        .and_then(|s| {
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() == 2 {
                let num: f64 = parts[0].parse().ok()?;
                let den: f64 = parts[1].parse().ok()?;
                if den > 0.0 {
                    Some(num / den)
                } else {
                    None
                }
            } else {
                parts[0].parse::<f64>().ok()
            }
        })
        .unwrap_or(30.0);

    Ok((width, height, duration_secs, fps))
}

impl VideoDecoder {
    pub fn open(path: &str, speed: f64) -> Result<(Self, mpsc::Sender<DecoderCommand>), String> {
        let (width, height, duration_secs, fps) = probe_metadata(path)?;
        tracing::info!(
            "Video: {}x{} @ {:.2}fps, duration={:.1}s",
            width,
            height,
            fps,
            duration_secs
        );

        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(4); // bounded: ~130ms buffer at 30fps
        let (command_tx, command_rx) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));

        let stopped_clone = stopped.clone();
        let paused_clone = paused.clone();
        let path = path.to_string();
        let cmd_tx = command_tx.clone();

        let thread_handle = Some(thread::spawn(move || {
            Self::decode_loop(
                &path, width, height, fps, speed,
                frame_tx, command_rx, stopped_clone, paused_clone,
            );
        }));

        Ok((
            Self {
                frame_rx, command_tx, thread_handle,
                stopped, paused,
                width, height, duration_secs, fps,
            },
            cmd_tx,
        ))
    }

    fn decode_loop(
        path: &str,
        width: u32,
        height: u32,
        fps: f64,
        speed: f64,
        frame_tx: SyncSender<DecodedFrame>,
        command_rx: mpsc::Receiver<DecoderCommand>,
        stopped: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
    ) {
        let frame_size = (width as usize) * (height as usize) * 4;
        let mut seek_to: Option<f64> = None;
        let mut frame_count: u64 = 0;

        loop {
            if stopped.load(Ordering::Relaxed) {
                break;
            }

            // Build ffmpeg args
            let mut args: Vec<String> = vec![
                "-v".into(), "quiet".into(),
            ];

            // Speed control: for 1x use -re; for others use setpts
            let speed_filter: Option<String> = if (speed - 1.0).abs() < 0.01 {
                args.push("-re".into());
                None
            } else {
                // setpts=PTS/SPEED means SPEEDx playback
                let p = speed.recip();
                Some(format!("setpts={p:.4}*PTS"))
            };

            if let Some(seek_pts) = seek_to.take() {
                args.push("-ss".into());
                args.push(format!("{seek_pts}"));
                frame_count = (seek_pts * fps) as u64;
            }

            args.push("-i".into());
            args.push(path.into());

            if let Some(ref sf) = speed_filter {
                args.push("-filter:v".into());
                args.push(sf.clone());
            }

            args.extend_from_slice(&[
                "-f".into(), "rawvideo".into(),
                "-pix_fmt".into(), "rgba".into(),
                "-vsync".into(), "0".into(),
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
                    tracing::error!("Failed to spawn ffmpeg: {e}");
                    break;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => break,
            };

            let mut reader = std::io::BufReader::with_capacity(1024 * 1024, stdout); // 1MB buffer
            let mut frame_buf = vec![0u8; frame_size];

            loop {
                // Check commands
                if let Ok(cmd) = command_rx.try_recv() {
                    match cmd {
                        DecoderCommand::Stop => {
                            stopped.store(true, Ordering::Relaxed);
                            break;
                        }
                        DecoderCommand::Pause => {
                            paused.store(true, Ordering::Relaxed);
                        }
                        DecoderCommand::Resume => {
                            paused.store(false, Ordering::Relaxed);
                        }
                        DecoderCommand::Seek(pts) => {
                            seek_to = Some(pts.max(0.0));
                            paused.store(false, Ordering::Relaxed);
                            break; // kill this process, restart with -ss
                        }
                    }
                }

                if stopped.load(Ordering::Relaxed) {
                    break;
                }

                if paused.load(Ordering::Relaxed) {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }

                // Read one frame from pipe
                match reader.read_exact(&mut frame_buf) {
                    Ok(()) => {
                        frame_count += 1;
                        let pts_secs = frame_count as f64 / fps;
                        let frame = DecodedFrame {
                            data: frame_buf.clone(),
                            width,
                            height,
                            pts_secs,
                        };
                        if frame_tx.send(frame).is_err() {
                            stopped.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        // Stream ended (end of file or seek killed the process)
                        if seek_to.is_some() {
                            break; // restart with seek
                        }
                        // EOF - video finished, send some empty frames to signal end
                        tracing::info!("Video stream ended");
                        thread::sleep(std::time::Duration::from_millis(100));
                        // Don't exit, wait for seek/stop commands
                        loop {
                            if stopped.load(Ordering::Relaxed) {
                                break;
                            }
                            if let Ok(cmd) = command_rx.try_recv() {
                                match cmd {
                                    DecoderCommand::Stop => {
                                        stopped.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                    DecoderCommand::Seek(pts) => {
                                        seek_to = Some(pts);
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            thread::sleep(std::time::Duration::from_millis(50));
                        }
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Read error: {e}");
                        break;
                    }
                }
            }

            // Kill the ffmpeg process
            let _ = child.kill();
            let _ = child.wait();

            if stopped.load(Ordering::Relaxed) {
                break;
            }

            // If seek was requested, the inner loop already broke,
            // and seek_to is set. The outer loop restarts ffmpeg with -ss.
            if !stopped.load(Ordering::Relaxed) && seek_to.is_some() {
                continue;
            }

            // No seek pending and not stopped = stream ended naturally
            // Wait for commands
            loop {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(cmd) = command_rx.recv() {
                    match cmd {
                        DecoderCommand::Stop => {
                            stopped.store(true, Ordering::Relaxed);
                            break;
                        }
                        DecoderCommand::Seek(pts) => {
                            seek_to = Some(pts);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        tracing::info!("Decoder thread finished");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_metadata() {
        let result = probe_metadata("/tmp/test_360.mp4");
        assert!(result.is_ok(), "probe_metadata failed: {:?}", result.err());
        let (w, h, dur, fps) = result.unwrap();
        assert_eq!(w, 3840, "width should be 3840");
        assert_eq!(h, 1920, "height should be 1920");
        assert!(dur > 0.0, "duration should be positive");
        assert!(fps > 0.0, "fps should be positive");
        println!("probe_metadata: {}x{} @ {:.1}fps, {:.1}s", w, h, fps, dur);
    }

    #[test]
    fn test_decode_single_frame() {
        let (decoder, _cmd) = VideoDecoder::open("/tmp/test_360.mp4", 1.0).unwrap();
        // Receive first frame
        let frame = decoder.frame_rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(frame.is_ok(), "Should receive a frame: {:?}", frame.err());
        let frame = frame.unwrap();
        assert_eq!(frame.width, 3840);
        assert_eq!(frame.height, 1920);
        assert_eq!(frame.data.len(), 3840 * 1920 * 4 as usize);
        println!("decode_single_frame: {}x{} RGBA, pts={:.2}s", frame.width, frame.height, frame.pts_secs);
        decoder.stop();
    }
}
