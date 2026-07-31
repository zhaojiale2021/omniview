//! Unified media player combining audio + video playback.
//!
//! Architecture:
//! - Audio: ffmpeg -re + cpal → master clock (samples actually played)
//! - Video: ffmpeg (paced with -readrate) → bounded channel (cap 2)
//!   → main thread takes ONE frame per vsync → displays the newest
//!   frame at or before the clock
//! - Clock: audio clock when audio is present, otherwise a smooth
//!   Instant-based clock.
//!
//! Backpressure comes from the bounded decode channel: the decoder
//! blocks when the display is behind, so it never decodes more than a
//! frame or two ahead — bounded memory, no CPU overproduction.

use std::sync::mpsc;
use std::time::Instant;

use crate::audio::AudioPlayer;
use crate::decoder::video::{DecodedFrame, DecoderCommand, VideoDecoder};

pub struct Player {
    audio: Option<AudioPlayer>,
    video: Option<VideoDecoder>,
    video_cmd: Option<mpsc::Sender<DecoderCommand>>,
    /// One frame ahead of the clock, held between vsyncs.
    pending: Option<DecodedFrame>,
    file_path: Option<String>,
    speed: f64,
    duration_secs: f64,
    volume: f32,
    has_audio: bool,

    // Instant-based clock (used when there is no audio).
    video_start: Option<Instant>,
    video_start_pos: f64,
    video_paused_pos: f64,

    /// PTS of last displayed frame (skip duplicate GPU uploads).
    last_pts: f64,

    // ── Diagnostics ─────────────────────────────────────────────
    first_frame_logged: bool,
    /// Last time a frame was actually displayed (not just polled).
    last_display: Instant,
    no_frame_warned: bool,
    /// Frames received from the decoder channel (displayed or dropped).
    frames_received: u64,
    /// PTS of the most recent frame taken from the channel.
    last_recv_pts: f64,
    last_diag: Instant,
}

impl Player {
    pub fn new() -> Self {
        Self {
            audio: None, video: None, video_cmd: None,
            pending: None,
            file_path: None, speed: 1.0, duration_secs: 0.0, volume: 0.8,
            has_audio: false,
            video_start: None, video_start_pos: 0.0, video_paused_pos: 0.0,
            last_pts: -1.0,
            first_frame_logged: false,
            last_display: Instant::now(),
            no_frame_warned: false,
            frames_received: 0,
            last_recv_pts: -1.0,
            last_diag: Instant::now(),
        }
    }

    // ── Open / Close ─────────────────────────────────────────────

    pub fn open(&mut self, path: &str) -> Result<(), String> {
        self.close();

        // Video decoder spawns instantly; probe + decode run on its
        // own thread, so open() doesn't block the UI.
        let (video, cmd_tx) = VideoDecoder::open(path, self.speed, 0.0);
        self.file_path = Some(path.to_string());

        match AudioPlayer::open(path, self.speed, 0.0) {
            Ok(audio) => {
                audio.set_volume(self.volume);
                self.audio = Some(audio);
                self.has_audio = true;
            }
            Err(e) => {
                tracing::warn!("Audio unavailable (video-only): {e}");
                self.audio = None;
                self.has_audio = false;
            }
        }

        self.video = Some(video);
        self.video_cmd = Some(cmd_tx);
        self.pending = None;
        self.video_start = Some(Instant::now());
        self.video_start_pos = 0.0;
        self.last_pts = -1.0;

        tracing::info!("Player opened: {path} (audio={})", self.has_audio);
        Ok(())
    }

    pub fn close(&mut self) {
        drop(self.video.take());
        drop(self.audio.take());
        self.video_cmd = None;
        self.pending = None;
        self.file_path = None;
        self.duration_secs = 0.0;
        self.has_audio = false;
        self.video_start = None;
    }

    // ── Controls ──────────────────────────────────────────────────

    pub fn play_pause(&mut self) -> bool {
        let cur = if self.has_audio {
            self.audio.as_ref().map(|a| a.is_paused()).unwrap_or(true)
        } else {
            self.video.as_ref()
                .map(|v| v.paused.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(true)
        };
        let new = !cur;

        // Freeze / advance the Instant display clock.
        if new {
            self.video_paused_pos = self.clock();
            self.video_start = None;
        } else {
            self.video_start = Some(Instant::now());
            self.video_start_pos = self.video_paused_pos;
        }

        if self.has_audio {
            if let Some(ref a) = self.audio { a.set_paused(new); }
        }
        if let Some(ref tx) = self.video_cmd {
            let c = if new { DecoderCommand::Pause } else { DecoderCommand::Resume };
            let _ = tx.send(c);
        }
        new
    }

    fn restart_at(&mut self, pos: f64) {
        let path = match self.file_path.clone() { Some(p) => p, _ => return };
        let pos = pos.clamp(0.0, self.duration_secs);
        self.pending = None;
        self.last_pts = -1.0;
        if let Some(ref mut a) = self.audio { a.restart(&path, self.speed, pos); a.set_volume(self.volume); }
        if let Some(old) = self.video.take() { old.stop(); }
        self.video_cmd = None;
        let (d, t) = VideoDecoder::open(&path, self.speed, pos);
        self.video = Some(d);
        self.video_cmd = Some(t);
        self.video_start = Some(Instant::now());
        self.video_start_pos = pos;
    }

    pub fn seek(&mut self, pos: f64) {
        tracing::info!("Seek → {pos:.1}s");
        self.restart_at(pos);
    }

    pub fn set_speed(&mut self, speed: f64) {
        if (self.speed - speed).abs() < 0.01 { return; }
        self.speed = speed;
        let pos = self.clock();
        tracing::info!("Speed → {speed}× at {pos:.1}s");
        self.restart_at(pos);
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(ref a) = self.audio { a.set_volume(self.volume); }
    }

    // ── Queries ───────────────────────────────────────────────────

    /// Playback position in seconds.
    ///
    /// Always a smooth Instant-based clock.  It is NOT derived from the
    /// audio `samples_played` counter: on some systems (e.g. cpal/WASAPI)
    /// the audio callback can stall, which would freeze the audio clock
    /// and therefore freeze video selection entirely.  Audio and video
    /// are restarted at the same position on open/seek/speed changes,
    /// so the Instant clock stays in A/V sync in practice.
    pub fn clock(&self) -> f64 {
        match self.video_start {
            Some(start) => (self.video_start_pos + start.elapsed().as_secs_f64() * self.speed)
                .min(self.duration_secs.max(0.0)),
            None => self.video_paused_pos,
        }
    }

    pub fn is_playing(&self) -> bool {
        if self.has_audio {
            self.audio.as_ref().map(|a| !a.is_paused()).unwrap_or(false)
        } else {
            self.video.as_ref()
                .map(|v| !v.paused.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
        }
    }

    pub fn is_paused(&self) -> bool { !self.is_playing() }
    pub fn duration(&self) -> f64 { self.duration_secs }
    pub fn speed(&self) -> f64 { self.speed }

    /// Poll for decode metadata (arrives ~200ms after open, from the
    /// decoder thread).  Must be called each vsync while waiting.
    fn poll_ready(&mut self) {
        if let Some(ref video) = self.video {
            if let Some(res) = video.poll_ready() {
                match res {
                    Ok(info) => {
                        self.duration_secs = info.duration_secs;
                        tracing::info!("Decode ready: {:.1}s", info.duration_secs);
                    }
                    Err(e) => tracing::error!("Video open failed: {e}"),
                }
            }
        }
    }

    /// Select the frame to display this vsync.
    ///
    /// Policy:
    /// - take at most ONE frame from the channel (bounded pipe ⇒ the
    ///   decoder is paced by consumption)
    /// - display the newest frame whose PTS is at or before the clock
    /// - a frame ahead of the clock is held in `pending` for the next
    ///   vsync (dropping an older one is fine — it was superseded)
    /// - returns None if the display frame is unchanged (skip the
    ///   duplicate GPU upload)
    pub fn try_recv_frame(&mut self) -> Option<DecodedFrame> {
        self.poll_ready();
        let clock = self.clock();
        let mut chosen: Option<DecodedFrame> = None;

        if let Some(f) = self.pending.take() {
            if f.pts_secs <= clock {
                chosen = Some(f);
            } else {
                self.pending = Some(f);
            }
        }

        if let (Some(video), None) = (&self.video, &chosen) {
            if let Ok(f) = video.frame_rx.try_recv() {
                self.frames_received += 1;
                self.last_recv_pts = f.pts_secs;
                if f.pts_secs <= clock {
                    chosen = Some(f);
                } else {
                    self.pending = Some(f);
                }
            }
        }

        // Periodic diagnostic (1s): shows whether frames arrive at all
        // and how the clock tracks their PTS.  `a_played` only advances
        // while the cpal callback fires; `a_buf` is the ring-buffer fill.
        if self.last_diag.elapsed().as_secs() >= 1 {
            let (a_played, a_buf) = self
                .audio
                .as_ref()
                .map(|a| a.diagnostics())
                .unwrap_or((0, 0));
            tracing::info!(
                "DIAG clock={clock:.2}s received={} last_pts={:.3}s pending={} audio:played={} buf={}",
                self.frames_received,
                self.last_recv_pts,
                self.pending.is_some(),
                a_played,
                a_buf,
            );
            self.last_diag = Instant::now();
        }

        let frame = match chosen {
            Some(f) => f,
            None => {
                // Diagnostic: no frame has been displayable for a while.
                let since = self.last_display.elapsed();
                if self.video.is_some()
                    && !self.is_paused()
                    && since.as_secs() > 2
                    && !self.no_frame_warned
                {
                    tracing::warn!(
                        "No video frame displayed for {:.1}s — clock={:.2}s duration={:.1}s \
                         has_audio={} (check ffmpeg/ffprobe on PATH; frame-pts vs clock mismatch?)",
                        since.as_secs_f64(),
                        self.clock(),
                        self.duration_secs,
                        self.has_audio,
                    );
                    self.no_frame_warned = true;
                }
                return None;
            }
        };
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            tracing::info!(
                "First video frame: pts={:.3}s clock={:.3}s has_audio={}",
                frame.pts_secs,
                clock,
                self.has_audio,
            );
        }
        self.last_display = Instant::now();
        self.no_frame_warned = false;
        if (frame.pts_secs - self.last_pts).abs() < 0.001 {
            return None;
        }
        self.last_pts = frame.pts_secs;
        Some(frame)
    }
}

impl Drop for Player {
    fn drop(&mut self) { self.close(); }
}
