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
use std::time::{Duration, Instant};

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
    /// Synchronous play/pause state.  The decoder's own `paused` flag
    /// is updated asynchronously by its thread, so it must NOT be used
    /// as the source of truth (rapid Space presses would see stale
    /// state and fail to resume).
    playing: bool,

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
    /// Last time a frame was received from the decoder channel (used by
    /// the self-heal to detect a genuinely dead decoder).
    last_recv: Instant,
    no_frame_warned: bool,
    /// Frames received from the decoder channel (displayed or dropped).
    frames_received: u64,
    /// PTS of the most recent frame taken from the channel.
    last_recv_pts: f64,
    last_diag: Instant,

    // ── Self-healing decoder ───────────────────────────────────
    /// Restarts the decoder if frames stall (e.g. a stuck ffmpeg pipe
    /// after pause-resume on some Windows builds).
    heal_count: u32,
    last_heal: Instant,
    /// True from a resume until the first frame arrives — uses a short
    /// stall window so a stalled resume recovers quickly.
    resume_pending: bool,
}

impl Player {
    pub fn new() -> Self {
        Self {
            audio: None, video: None, video_cmd: None,
            pending: None,
            file_path: None, speed: 1.0, duration_secs: 0.0, volume: 0.8,
            has_audio: false,
            playing: false,
            video_start: None, video_start_pos: 0.0, video_paused_pos: 0.0,
            last_pts: -1.0,
            first_frame_logged: false,
            last_display: Instant::now(),
            last_recv: Instant::now(),
            no_frame_warned: false,
            frames_received: 0,
            last_recv_pts: -1.0,
            last_diag: Instant::now(),
            heal_count: 0,
            last_heal: Instant::now(),
            resume_pending: false,
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
        self.playing = true;
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
        self.playing = !self.playing;
        // Clock transition for pause/resume.  The in-process decoder
        // freezes its position on pause, so a resume simply continues
        // from where it was — instant, no position jump.
        if self.playing {
            self.video_start = Some(Instant::now());
            self.video_start_pos = self.video_paused_pos;
            // Reset the stall bookkeeping so the pause doesn't look like
            // a stalled decoder, and watch for a fast resume.
            self.last_display = Instant::now();
            self.no_frame_warned = false;
            self.resume_pending = true;
        } else {
            self.video_paused_pos = self.clock();
            self.video_start = None;
        }
        self.push_play_state();
        self.playing
    }

    /// Send the current `playing` state to the decoder and audio
    /// threads.  Does NOT touch the clock — callers set the clock first.
    fn push_play_state(&mut self) {
        if self.playing {
            if let Some(ref tx) = self.video_cmd {
                let _ = tx.send(DecoderCommand::Resume);
            }
            if self.has_audio {
                if let Some(ref a) = self.audio { a.set_paused(false); }
            }
        } else {
            if let Some(ref tx) = self.video_cmd {
                let _ = tx.send(DecoderCommand::Pause);
            }
            if self.has_audio {
                if let Some(ref a) = self.audio { a.set_paused(true); }
            }
        }
    }

    fn restart_at(&mut self, pos: f64) {
        let path = match self.file_path.clone() { Some(p) => p, _ => return };
        let pos = pos.clamp(0.0, self.duration_secs);
        if let Some(ref mut a) = self.audio { a.restart(&path, self.speed, pos); a.set_volume(self.volume); }
        self.restart_video_at(pos);
    }

    /// Reopen ONLY the video decoder at `pos` (audio untouched).  Used by
    /// the self-heal so a video recovery doesn't restart/spawn ffmpeg
    /// audio (no console window, no audio glitch).
    fn restart_video_at(&mut self, pos: f64) {
        let path = match self.file_path.clone() { Some(p) => p, _ => return };
        let pos = pos.clamp(0.0, self.duration_secs);
        self.pending = None;
        self.last_pts = -1.0;
        if let Some(old) = self.video.take() { old.stop(); }
        self.video_cmd = None;
        let (d, t) = VideoDecoder::open(&path, self.speed, pos);
        self.video = Some(d);
        self.video_cmd = Some(t);
        // The clock resumes from the target; if we're paused, stay paused.
        self.video_start = Some(Instant::now());
        self.video_start_pos = pos;
        if !self.playing {
            self.video_paused_pos = pos;
            self.video_start = None;
        }
        self.push_play_state();
        // A fresh decoder shouldn't count as a stalled one.
        self.last_display = Instant::now();
        self.last_recv = Instant::now();
        self.heal_count = 0;
    }

    /// Self-heal: restart the VIDEO decoder if we're playing but no
    /// frames have been RECEIVED for a while (a genuinely dead/stuck
    /// decoder).  A frame sitting in `pending` just ahead of the clock is
    /// NOT a stall — frames are still arriving — so it never heals for
    /// that, which previously restarted the audio every few seconds.
    fn maybe_heal_decoder(&mut self) {
        let stall = if self.resume_pending {
            Duration::from_millis(400)
        } else {
            Duration::from_secs(2)
        };
        if self.playing
            && self.duration_secs > 0.0
            && self.clock() < self.duration_secs - 5.0
            && self.last_recv.elapsed() > stall
            && self.last_heal.elapsed() > Duration::from_secs(3)
            && self.heal_count < 5
        {
            self.heal_count += 1;
            self.last_heal = Instant::now();
            let pos = self.clock();
            tracing::warn!(
                "No video frames for {}s — restarting decoder at {pos:.1}s",
                self.last_recv.elapsed().as_secs()
            );
            self.restart_video_at(pos);
        }
    }

    pub fn seek(&mut self, pos: f64) {
        tracing::info!("Seek → {pos:.1}s");
        self.restart_at(pos);
    }

    pub fn set_speed(&mut self, speed: f64) {
        if (self.speed - speed).abs() < 0.01 { return; }
        let pos = self.clock(); // position at the OLD speed
        self.speed = speed;
        // Re-anchor the clock so the new speed applies from now on.
        self.video_start = Some(Instant::now());
        self.video_start_pos = pos;
        // The video decoder paces itself from a shared atomic — a speed
        // change is live, no restart, no freeze.
        if let Some(ref v) = self.video {
            v.set_speed(speed);
        }
        tracing::info!("Speed → {speed}× at {pos:.1}s");
        // The audio filter (atempo) is fixed at open, so the audio
        // reader must restart.  Video stays smooth.
        if self.has_audio {
            if let Some(path) = self.file_path.clone() {
                if let Some(ref mut a) = self.audio {
                    a.restart(&path, speed, pos);
                    a.set_volume(self.volume);
                }
            }
        }
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

    pub fn is_playing(&self) -> bool { self.playing }
    pub fn is_paused(&self) -> bool { !self.playing }
    pub fn duration(&self) -> f64 { self.duration_secs }
    pub fn speed(&self) -> f64 { self.speed }
    pub fn volume(&self) -> f32 { self.volume }
    pub fn file_path(&self) -> Option<&str> { self.file_path.as_deref() }

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
        self.maybe_heal_decoder();
        let clock = self.clock();
        let mut chosen: Option<DecodedFrame> = None;
        let mut newest_ahead: Option<DecodedFrame> = None;

        // The frame held from the last vsync.
        if let Some(f) = self.pending.take() {
            if f.pts_secs <= clock {
                chosen = Some(f);
            } else {
                newest_ahead = Some(f);
            }
        }

        // Drain EVERYTHING available this vsync.  At speed > 1 the clock
        // advances several frames per vsync, so we must skip ahead to the
        // newest frame at/before the clock (frames in between are dropped).
        if let Some(video) = &self.video {
            while let Ok(f) = video.frame_rx.try_recv() {
                self.frames_received += 1;
                self.last_recv = Instant::now();
                self.last_recv_pts = f.pts_secs;
                if f.pts_secs <= clock {
                    chosen = Some(f);
                } else {
                    newest_ahead = Some(f);
                }
            }
        }
        self.pending = newest_ahead;

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
        self.resume_pending = false;
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

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the synchronous play/pause state: pause freezes the
    /// clock, resume continues it, and two rapid toggles resume (the
    /// "double space" bug).
    #[test]
    fn test_pause_resume_clock() {
        let mut p = Player::new();
        p.open("/tmp/test_360.mp4").unwrap();
        // Drive the frame loop until the async probe reports duration
        // (try_recv_frame also polls the ready channel).
        for _ in 0..200 {
            let _ = p.try_recv_frame();
            if p.duration() > 1.0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(p.duration() > 1.0, "video should report duration");
        assert!(p.is_playing());

        // Clock advances while playing.
        let t0 = p.clock();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(p.clock() > t0, "clock should advance while playing");

        // Pause freezes the clock.
        p.play_pause();
        assert!(p.is_paused());
        let frozen = p.clock();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!((p.clock() - frozen).abs() < 0.05, "clock should freeze while paused");

        // Resume continues the clock from the pause position (true
        // pause: the in-process decoder froze its position).
        p.play_pause();
        assert!(p.is_playing());
        let resumed = p.clock();
        assert!(
            (resumed - frozen).abs() < 0.2,
            "resume should continue from pause position (frozen {frozen:.2}, got {resumed:.2})"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(p.clock() > resumed, "clock should advance after resume");

        // Rapid double toggle must end up playing (was the bug: the
        // second toggle read the decoder's stale async flag).
        p.play_pause();
        p.play_pause();
        assert!(p.is_playing(), "rapid double space should resume");

        // Frames must be delivered again after a real pause→resume.
        p.play_pause(); // pause
        assert!(p.is_paused());
        std::thread::sleep(std::time::Duration::from_millis(300));
        p.play_pause(); // resume
        let mut delivered = 0u32;
        for _ in 0..100 {
            if p.try_recv_frame().is_some() {
                delivered += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(delivered >= 2, "frames should be delivered again after resume (got {delivered})");

        p.close();
    }

    /// Seek must move the clock to the target (not jump back) and keep
    /// frames flowing; a seek while paused must stay paused.
    #[test]
    fn test_seek_position() {
        let mut p = Player::new();
        p.open("/tmp/test_360.mp4").unwrap();
        for _ in 0..200 {
            let _ = p.try_recv_frame();
            if p.duration() > 1.0 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(p.duration() > 1.0);

        // Seek while playing → clock sits at the target and frames flow.
        p.seek(10.0);
        assert!(p.is_playing());
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            (p.clock() - 10.0).abs() < 2.0,
            "clock should be near seek target after seek (got {:.1})",
            p.clock()
        );
        let mut delivered = 0;
        for _ in 0..100 {
            if p.try_recv_frame().is_some() { delivered += 1; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered >= 2, "frames should flow after seek (got {delivered})");

        // Seek while paused → stays paused at the target.
        p.play_pause();
        assert!(p.is_paused());
        p.seek(5.0);
        assert!(p.is_paused(), "seek while paused should stay paused");
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            (p.clock() - 5.0).abs() < 0.5,
            "paused clock should sit at seek target (got {:.1})",
            p.clock()
        );

        p.close();
    }

    /// A LONG pause must resume instantly without a restart.  Pauses
    /// the ffmpeg pipe for 3s, then checks frames flow again quickly.
    #[test]
    fn test_long_pause_resume() {
        let mut p = Player::new();
        p.open("/tmp/test_360.mp4").unwrap();
        for _ in 0..200 {
            let _ = p.try_recv_frame();
            if p.duration() > 1.0 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(p.duration() > 1.0);

        // Let playback run a moment, then pause for 3s (pipe stalls).
        for _ in 0..30 { let _ = p.try_recv_frame(); std::thread::sleep(Duration::from_millis(20)); }
        p.play_pause();
        assert!(p.is_paused());
        let paused_pos = p.clock();
        std::thread::sleep(Duration::from_secs(3));

        // Resume: frames should flow again within ~1s WITHOUT a restart.
        p.play_pause();
        assert!(p.is_playing());
        let t0 = std::time::Instant::now();
        let mut delivered = 0u32;
        while t0.elapsed().as_secs() < 3 {
            if p.try_recv_frame().is_some() { delivered += 1; }
            if delivered >= 3 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            delivered >= 3,
            "frames should flow quickly after a long pause (got {delivered} in 3s)"
        );
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "first frames should arrive well within 2s (took {:.1}s)",
            elapsed.as_secs_f64()
        );
        // True pause: the position must stay where it was (no jump).
        assert!(
            (p.clock() - paused_pos).abs() < 1.5,
            "position should not jump after resume (paused at {paused_pos:.1}, now {:.1})",
            p.clock()
        );

        p.close();
    }

    /// A speed change must take effect immediately (the clock advances
    /// faster) and keep frames flowing — without a decoder restart.
    #[test]
    fn test_speed_change() {
        let mut p = Player::new();
        p.open("/tmp/test_360.mp4").unwrap();
        for _ in 0..200 {
            let _ = p.try_recv_frame();
            if p.duration() > 1.0 { break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(p.duration() > 1.0);

        // At 1×: clock advances ~0.5s of content per 0.5s of wall time.
        let t0 = p.clock();
        for _ in 0..25 { let _ = p.try_recv_frame(); std::thread::sleep(Duration::from_millis(20)); }
        let adv1 = p.clock() - t0;
        assert!((adv1 - 0.5).abs() < 0.25, "1× clock should advance ~0.5s (got {adv1:.2})");

        // At 2×: clock advances ~twice as fast.
        p.set_speed(2.0);
        let t1 = p.clock();
        for _ in 0..25 { let _ = p.try_recv_frame(); std::thread::sleep(Duration::from_millis(20)); }
        let adv2 = p.clock() - t1;
        assert!(
            adv2 > adv1 * 1.5,
            "2× should advance the clock faster (1× {adv1:.2}, 2× {adv2:.2})"
        );

        // Frames keep flowing at the higher speed.
        let mut delivered = 0;
        for _ in 0..25 {
            if p.try_recv_frame().is_some() { delivered += 1; }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered >= 3, "frames should keep flowing at 2× (got {delivered})");

        p.close();
    }
}
