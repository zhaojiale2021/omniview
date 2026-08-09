use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::HostTrait;

use crate::media::audio::AudioPipeline;
use crate::media::clock::MediaClock;
use crate::media::demux::Demux;
use crate::media::types::{Command, PlaybackState, VideoFrame};
use crate::media::video::{DecoderCmd, VideoDecoder};


pub struct PlaybackController {
    state: PlaybackState,
    volume: f32,
    demux: Option<Demux>,
    video: Option<VideoDecoder>,
    video_cmd: Option<mpsc::Sender<DecoderCmd>>,
    audio: Option<AudioPipeline>,
    audio_discard: Option<thread::JoinHandle<()>>,
    clock: MediaClock,
    has_audio: bool,
    speed: f64,
    duration: f64,
    file_path: Option<String>,
    /// PTS of the last displayed frame.  Sentinel -1.0 so a first frame at
    /// pts 0.0 is displayed (the dedupe threshold is |pts - last_pts| < 0.001).
    last_pts: f64,
    /// Demuxer has reported end-of-file (latched).
    eof_seen: bool,
    /// When the frame queue last delivered a frame; drives the EOF grace
    /// period so buffered frames are shown before `Ended` fires.
    last_frame_at: Option<Instant>,
    /// Audio-clock stall guard state: last observed clock position.
    last_clock_pos: f64,
    /// Audio-clock stall guard state: when that position was observed.
    last_clock_at: Option<Instant>,
    /// Suppresses transient starvation diagnostics right after open/seek
    /// while fresh decoders produce their first frames.
    startup_grace_until: Option<Instant>,
}

impl PlaybackController {
    pub fn new() -> Self {
        Self {
            state: PlaybackState::Idle,
            volume: 0.8,
            demux: None,
            video: None,
            video_cmd: None,
            audio: None,
            audio_discard: None,
            clock: MediaClock::new(),
            has_audio: false,
            speed: 1.0,
            duration: 0.0,
            file_path: None,
            last_pts: -1.0,
            eof_seen: false,
            last_frame_at: None,
            last_clock_pos: 0.0,
            last_clock_at: None,
            startup_grace_until: None,
        }
    }

    pub fn state(&self) -> &PlaybackState {
        &self.state
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn position(&self) -> f64 {
        self.clock.position()
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    pub fn paused(&self) -> bool {
        self.state == PlaybackState::Paused
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    /// Ring-buffer underflow count of the audio pipeline (diagnostics).
    pub fn audio_underruns(&self) -> u64 {
        self.audio.as_ref().map(|a| a.underruns()).unwrap_or(0)
    }

    /// True for a few seconds right after open/seek, while the fresh
    /// decoders are still producing their first frames.  Used to suppress
    /// transient starvation diagnostics and to show a buffering hint.
    pub fn startup_grace(&self) -> bool {
        self.startup_grace_until
            .map(|t| t > std::time::Instant::now())
            .unwrap_or(false)
    }

    /// Decoded video frames waiting ahead of the clock (diagnostics).
    pub fn buffered_frames(&self) -> usize {
        self.video.as_ref().map(|v| v.buffered()).unwrap_or(0)
    }

    /// Apply a command: validate, drive the pipeline, and update state.
    pub fn apply(&mut self, cmd: Command) -> Result<(), String> {
        match cmd {
            Command::Open(path) => self.do_open(&path),
            Command::Play => self.do_play(),
            Command::Pause => self.do_pause(),
            Command::Toggle => self.do_toggle(),
            Command::Seek(pos) => self.do_seek(pos),
            Command::SetSpeed(s) => self.do_set_speed(s),
            Command::SetVolume(v) => self.do_set_volume(v),
            Command::Stop => self.do_stop(),
        }
    }

    /// Select the frame to display this cycle.
    ///
    /// The decoder keeps a bounded jitter buffer of decoded frames; the
    /// queue pops every frame the media clock has reached and returns the
    /// newest of those (older ones are skipped when the clock is ahead, e.g.
    /// after a speed change).  Frames ahead of the clock stay buffered.
    /// Select the frame to display this cycle.
    ///
    /// `lookahead` is the media time until the texture swap takes effect
    /// (about one vsync on a 60 Hz display, scaled by playback speed).  The
    /// app measures the vsync phase so the swap lands on a stable cadence
    /// instead of alternating 2/3 vsyncs as the audio clock jitters.
    pub fn next_video_frame(&mut self, lookahead: f64) -> Option<VideoFrame> {
        // Audio-clock stall guard: if the audio master clock has not
        // advanced for a full second while playing, the audio pipeline is
        // dead (silent ring buffer).  Fall back to the wall clock so the
        // video keeps playing instead of freezing forever.
        if self.has_audio && self.state == PlaybackState::Playing {
            let pos = self.clock.position();
            if (pos - self.last_clock_pos).abs() < 0.001 {
                let stalled = self
                    .last_clock_at
                    .map(|t| t.elapsed() >= Duration::from_secs(1))
                    .unwrap_or(false);
                if stalled {
                    let stalled_for = self
                        .last_clock_at
                        .map(|t| t.elapsed().as_secs())
                        .unwrap_or(0);
                    tracing::warn!(
                        "audio clock stalled for {stalled_for}s; switching video to wall clock"
                    );
                    self.clock.detach_audio();
                    self.has_audio = false;
                    self.clock.play(pos);
                    self.last_clock_pos = pos;
                    self.last_clock_at = None;
                }
            } else {
                self.last_clock_pos = pos;
                self.last_clock_at = Some(Instant::now());
            }
        }
        let clock_pos = self.clock.position() + lookahead;
        let (chosen, remaining) = match self.video {
            Some(ref video) => video.drain_upto(clock_pos),
            None => (None, 0),
        };
        if chosen.is_some() || remaining > 0 {
            self.last_frame_at = Some(Instant::now());
        }

        let frame = match chosen {
            Some(f) => f,
            None => {
                self.check_eof();
                return None;
            }
        };

        // Skip frames whose PTS matches the last displayed PTS (avoid
        // re-rendering the same frame).
        if (frame.pts_secs - self.last_pts).abs() < 0.001 {
            return None;
        }
        self.last_pts = frame.pts_secs;
        Some(frame)
    }

    /// If the demuxer reached the end of the stream and the decoder has no
    /// more frames to offer, transition to `Ended` and freeze the clock so
    /// the UI stops at the last frame instead of drifting past the end.
    ///
    /// Only fires from `Playing`: while paused the demuxer may already have
    /// buffered to EOF on short files, and a user pause must not be
    /// reinterpreted as "ended".  Resuming then reaches Ended normally.
    ///
    /// The demuxer reports EOF as soon as the file is fully read, which can
    /// happen while decoded frames are still buffered (decode runs at full
    /// speed into the queue).  The signal is latched and the transition only
    /// fires once the queue has been empty for a grace period, so every
    /// buffered frame is shown before the end state.  The grace also covers
    /// slow decoders (e.g. 8K) that may still be producing the final frames.
    fn check_eof(&mut self) {
        if self.state != PlaybackState::Playing {
            return;
        }
        if let Some(ref demux) = self.demux
            && demux.poll_eof() {
                self.eof_seen = true;
            }
        if !self.eof_seen {
            return;
        }
        let idle = self
            .last_frame_at
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);
        if idle > Duration::from_millis(400) {
            self.state = PlaybackState::Ended;
            self.clock.pause();
        }
    }

    // ── Command handlers ──────────────────────────────────────────

    fn do_open(&mut self, path: &str) -> Result<(), String> {
        self.startup_grace_until = Some(std::time::Instant::now() + Duration::from_secs(3));
        self.teardown();

        self.file_path = Some(path.to_string());
        self.state = PlaybackState::Loading;

        self.open_pipeline(path, 0.0)?;
        // Re-apply stored speed/volume to the fresh decoders.
        self.do_set_speed(self.speed)?;
        self.do_set_volume(self.volume)?;

        self.state = PlaybackState::Ready;
        Ok(())
    }

    fn do_play(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Ready
            | PlaybackState::Paused
            | PlaybackState::Seeking
            | PlaybackState::Playing => {} // Playing: re-anchor clock, no-op otherwise
            PlaybackState::Ended => {
                // Play after EOF restarts the media from the beginning.
                let path = self
                    .file_path
                    .clone()
                    .ok_or_else(|| "No file open".to_string())?;
                self.teardown();
                self.open_pipeline(&path, 0.0)?;
                self.do_set_speed(self.speed)?;
                self.do_set_volume(self.volume)?;
                self.clock.play(0.0);
                self.state = PlaybackState::Playing;
                return Ok(());
            }
            _ => return Err(format!("cannot play from {:?}", self.state)),
        }

        self.clock.play(self.position());

        if let Some(ref cmd) = self.video_cmd {
            let _ = cmd.send(DecoderCmd::Resume);
        }

        if let Some(ref audio) = self.audio {
            audio.set_paused(false);
        }

        self.state = PlaybackState::Playing;
        Ok(())
    }

    fn do_pause(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Playing | PlaybackState::Seeking => {}
            _ => return Err(format!("cannot pause from {:?}", self.state)),
        }

        self.clock.pause();

        if let Some(ref cmd) = self.video_cmd {
            let _ = cmd.send(DecoderCmd::Pause);
        }

        if let Some(ref audio) = self.audio {
            audio.set_paused(true);
        }

        self.state = PlaybackState::Paused;
        Ok(())
    }

    fn do_toggle(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Playing => self.do_pause(),
            PlaybackState::Paused
            | PlaybackState::Ready
            | PlaybackState::Ended
            | PlaybackState::Seeking => self.do_play(),
            _ => Err(format!("cannot toggle from {:?}", self.state)),
        }
    }

    fn do_seek(&mut self, pos: f64) -> Result<(), String> {
        match &self.state {
            PlaybackState::Playing
            | PlaybackState::Paused
            | PlaybackState::Ready
            | PlaybackState::Ended
            | PlaybackState::Seeking => {}
            _ => return Err(format!("cannot seek from {:?}", self.state)),
        }

        let clamped = pos.clamp(0.0, self.duration);
        let was_playing = self.state == PlaybackState::Playing;

        self.startup_grace_until = Some(std::time::Instant::now() + Duration::from_secs(3));
        self.state = PlaybackState::Seeking;
        self.teardown();

        let path = match self.file_path.clone() {
            Some(p) => p,
            None => {
                self.state = PlaybackState::Error("No file open".into());
                return Err("No file open".to_string());
            }
        };

        self.open_pipeline(&path, clamped)?;
        // Re-apply stored speed/volume to the fresh decoders.
        self.do_set_speed(self.speed)?;
        self.do_set_volume(self.volume)?;

        // Restore play/pause state after the restart.
        if was_playing {
            self.clock.play(self.position());
            self.state = PlaybackState::Playing;
        } else {
            self.state = PlaybackState::Paused;
            if let Some(ref cmd) = self.video_cmd {
                let _ = cmd.send(DecoderCmd::Pause);
            }
            if let Some(ref audio) = self.audio {
                audio.set_paused(true);
            }
            self.clock.pause();
        }

        Ok(())
    }

    fn do_set_speed(&mut self, s: f64) -> Result<(), String> {
        self.speed = s;
        self.clock.set_speed(s);
        if let Some(ref audio) = self.audio {
            audio.set_speed(s);
        }
        Ok(())
    }

    fn do_set_volume(&mut self, v: f32) -> Result<(), String> {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(ref audio) = self.audio {
            audio.set_volume(self.volume);
        }
        Ok(())
    }

    fn do_stop(&mut self) -> Result<(), String> {
        self.teardown();
        self.clock = MediaClock::new();
        self.state = PlaybackState::Idle;
        self.last_pts = -1.0;
        self.eof_seen = false;
        self.last_frame_at = None;
        self.last_clock_pos = 0.0;
        self.last_clock_at = None;
        self.duration = 0.0;
        self.file_path = None;
        Ok(())
    }

    // ── Pipeline lifecycle ───────────────────────────────────────

    /// Set up demux, audio, and video for the given file at `pos` seconds.
    /// Called by both `do_open` and `do_seek`.  The caller is responsible
    /// for tearing down the old pipeline first and for setting the final
    /// state after this returns.
    fn open_pipeline(&mut self, path: &str, pos: f64) -> Result<(), String> {
        // ── Demux ────────────────────────────────────────────────
        let mut demux = Demux::open(path, pos);

        let info = {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match demux.poll_ready() {
                    Some(Ok(info)) => break info,
                    Some(Err(e)) => {
                        self.state = PlaybackState::Error(format!("Demux probe: {e}"));
                        return Err(format!("Demux probe: {e}"));
                    }
                    None => {
                        if std::time::Instant::now() > deadline {
                            self.state =
                                PlaybackState::Error("Demux probe timeout".into());
                            return Err("Demux probe timeout".to_string());
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        };

        self.duration = info.duration;

        let (video_rx, audio_rx) = match demux.take_channels() {
            Some(ch) => ch,
            None => {
                self.state =
                    PlaybackState::Error("Channels already taken".into());
                return Err("Channels already taken".to_string());
            }
        };

        // ── Audio ────────────────────────────────────────────────
        let host = cpal::default_host();
        let audio_device = host.default_output_device();

        let audio = if let Some(ref dev) = audio_device {
            match AudioPipeline::start(path, dev, audio_rx, pos) {
                Ok(a) => {
                    let sp = a.samples_played.clone();
                    let rate = a.sample_rate;
                    let ch = a.channels;
                    self.clock.attach_audio(sp, rate, ch);
                    self.has_audio = true;
                    Some(a)
                }
                Err((e, rx)) => {
                    tracing::warn!("Audio pipeline failed: {e}; proceeding video-only");
                    // Drain audio packets so the demux doesn't block.
                    self.has_audio = false;
                    let handle = thread::spawn(move || {
                        while rx.recv().is_ok() {
                            // discard
                        }
                    });
                    self.audio_discard = Some(handle);
                    None
                }
            }
        } else {
            // WSL2 / no audio device: drain audio packets so the demux
            // doesn't block.
            self.has_audio = false;
            let handle = thread::spawn(move || {
                while audio_rx.recv().is_ok() {
                    // discard
                }
            });
            self.audio_discard = Some(handle);
            None
        };

        // ── Video ────────────────────────────────────────────────
        // Only spawn the video decoder when the file actually has a video
        // stream.  `decode_packets_loop` early-returns on "No video stream"
        // (dropping its packet receiver); the demux's next video send would
        // then hit Disconnected, break 'outer, and silently starve the audio
        // decoder while the controller stays Playing.  With has_video ==
        // false we keep video None — the demux never routes video packets (no
        // video stream index), so no Disconnected is ever observed.
        let (video, video_cmd) = if info.has_video {
            let (v, c) = VideoDecoder::from_packets(path, video_rx, pos);
            (Some(v), Some(c))
        } else {
            // Drop the (never-fed) video receiver immediately.
            (None, None)
        };

        self.demux = Some(demux);
        self.video = video;
        self.video_cmd = video_cmd;
        self.audio = audio;
        self.clock.reset(pos);
        self.last_pts = -1.0;
        self.eof_seen = false;
        self.last_frame_at = None;
        self.last_clock_pos = pos;
        self.last_clock_at = Some(Instant::now());

        Ok(())
    }

    /// Tear down the entire pipeline: stop decoders, drop all handles,
    /// join auxiliary threads, detach the audio clock.
    fn teardown(&mut self) {
        // Stop video decoder.
        if let Some(cmd) = self.video_cmd.take() {
            let _ = cmd.send(DecoderCmd::Stop);
        }
        // Wake the decoder if it is blocked pushing into a full queue, so it
        // can process the Stop command instead of leaking.
        if let Some(video) = self.video.take() {
            video.interrupt();
        }

        // Stop audio pipeline.
        if let Some(audio) = self.audio.take() {
            audio.stop();
        }

        // Stop demux.
        if let Some(demux) = self.demux.take() {
            demux.stop();
        }

        // Join the packet-discard thread (WSL2 path).
        if let Some(handle) = self.audio_discard.take() {
            let _ = handle.join();
        }

        self.clock.detach_audio();
        self.has_audio = false;
    }
}

impl Drop for PlaybackController {
    fn drop(&mut self) {
        self.teardown();
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transitions_are_validated() {
        let mut c = PlaybackController::new();
        assert_eq!(c.state(), &PlaybackState::Idle);
        // Cannot pause/play/toggle from Idle.
        assert!(c.apply(Command::Pause).is_err());
        assert!(c.apply(Command::Play).is_err());
        assert!(c.apply(Command::Toggle).is_err());
        // SetVolume works from any state.
        c.apply(Command::SetVolume(1.5)).unwrap();
        assert_eq!(c.volume(), 1.0);
        // Stop from Idle is valid and stays Idle.
        c.apply(Command::Stop).unwrap();
        assert_eq!(c.state(), &PlaybackState::Idle);
    }

    #[test]
    fn full_pipeline_plays() {
        // Uses the real test file created by the Task 5 test.
        let test_path = "/tmp/test_av.mp4";
        assert!(
            std::path::Path::new(test_path).exists(),
            "test file {test_path} must exist (run the demux/audio tests first)"
        );

        let mut ctl = PlaybackController::new();

        // ── Open ──────────────────────────────────────────────────
        ctl.apply(Command::Open(test_path.into())).unwrap();
        assert_eq!(
            ctl.state(),
            &PlaybackState::Ready,
            "after Open the controller must be Ready"
        );

        // ── Play (and double-Play is valid) ───────────────────────
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);
        ctl.apply(Command::Play).unwrap(); // must be valid (re-anchors clock)
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // ── Drive next_video_frame until we get a frame ────────────
        let mut frame = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while frame.is_none() && std::time::Instant::now() < deadline {
            frame = ctl.next_video_frame(1.0 / 60.0);
            if frame.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(frame.is_some(), "must receive at least one video frame within 5s");

        // ── Pause → position freezes ──────────────────────────────
        ctl.apply(Command::Pause).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Paused);
        let pos_before = ctl.position();
        thread::sleep(Duration::from_millis(200));
        let pos_after = ctl.position();
        assert!(
            (pos_after - pos_before).abs() < 0.05,
            "position must freeze when paused (was {pos_before}, now {pos_after})"
        );

        // ── Toggle (Paused → Playing) → frames flow again ─────────
        ctl.apply(Command::Toggle).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);
        let mut frames_after_toggle = 0u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while frames_after_toggle < 2 && std::time::Instant::now() < deadline {
            if ctl.next_video_frame(1.0 / 60.0).is_some() {
                frames_after_toggle += 1;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // At least 1 frame must flow; position must advance.
        assert!(
            frames_after_toggle >= 1,
            "frames must flow after toggle resume ({frames_after_toggle} received)"
        );
        let pos_after_resume = ctl.position();
        assert!(
            pos_after_resume > pos_before + 0.01,
            "position must advance after resume (was {pos_before}, now {pos_after_resume})"
        );

        // ── Seek to 1.0 → position ≈ 1.0, frames still flow ──────
        ctl.apply(Command::Seek(1.0)).unwrap();
        // After seek with video-only (WSL2), poll until position is near 1.0.
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        let mut near_target = false;
        while std::time::Instant::now() < dl {
            let p = ctl.position();
            if (p - 1.0).abs() < 0.5 {
                near_target = true;
                break;
            }
            // Drain frames to keep the pipeline moving.
            let _ = ctl.next_video_frame(1.0 / 60.0);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            near_target,
            "position after seek to 1.0 must approach 1.0 (was {})",
            ctl.position()
        );

        // Frames must flow after seek too.
        let mut frame_after_seek = None;
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while frame_after_seek.is_none() && std::time::Instant::now() < dl {
            frame_after_seek = ctl.next_video_frame(1.0 / 60.0);
            if frame_after_seek.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(frame_after_seek.is_some(), "frames must flow after seek");

        // ── Stop → Idle ───────────────────────────────────────────
        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }

    #[test]
    fn av_sync_within_tolerance() {
        // Same fixture as full_pipeline_plays.  On WSL2 there is no audio
        // device, so the controller falls back to video-only: MediaClock
        // uses the Instant clock and the decoder paces frames at content
        // rate in real time.  That keeps the displayed frame's PTS within
        // one frame interval + startup offset of the clock position.
        let test_path = "/tmp/test_av.mp4";
        assert!(
            std::path::Path::new(test_path).exists(),
            "test file {test_path} must exist (run the demux/audio tests first)"
        );

        let mut ctl = PlaybackController::new();
        ctl.apply(Command::Open(test_path.into())).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Ready);
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Poll for ~2s, recording (frame PTS, clock position) at the same
        // moment for every frame the controller hands to the (virtual)
        // renderer, plus the wall-clock capture time.
        let t0 = std::time::Instant::now();
        let mut samples: Vec<(f64, f64, f64)> = Vec::new(); // (pts, pos, t)
        let deadline = t0 + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Some(f) = ctl.next_video_frame(1.0 / 60.0) {
                samples.push((f.pts_secs, ctl.position(), t0.elapsed().as_secs_f64()));
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !samples.is_empty(),
            "frames must flow during playback ({} samples)",
            samples.len()
        );

        // The deviation must stay BOUNDED over the window, not just be
        // small at the earliest (start-aligned) samples.  A clock running
        // at a wrong constant rate (e.g. 2x content) matches at start and
        // then diverges monotonically — a min-based check would pass it, a
        // max-based check catches it.  Skip the ~0.5s startup transient,
        // then require the max |frame PTS - position| to stay under one
        // frame period's worth of slack: the frame selector holds at most
        // one frame ahead and only chooses frames <= clock, so steady-state
        // deviation is ~one frame period (~0.033s) well under 0.15s.
        const GRACE: f64 = 0.5;
        let steady: Vec<f64> = samples
            .iter()
            .filter(|(_, _, t)| *t >= GRACE)
            .map(|(pts, pos, _)| (pts - pos).abs())
            .collect();
        assert!(
            !steady.is_empty(),
            "must collect steady-state samples after the {GRACE:.1}s startup \
             grace period ({} total samples)",
            samples.len()
        );
        let max_dev = steady.iter().cloned().fold(0.0f64, f64::max);
        assert!(
            max_dev < 0.15,
            "|frame PTS - position| must stay below 0.15s in steady state \
             (max was {max_dev:.3}s over {} steady samples after {GRACE:.1}s \
             grace; {} total collected)",
            steady.len(),
            samples.len()
        );

        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }

    #[test]
    fn seek_resumes_at_position() {
        // Seeks mid-stream: the pipeline restarts at the target and the
        // fresh decoder must deliver frames at (or just past) the seek
        // position — frames below it are discarded.
        let test_path = "/tmp/test_av.mp4";
        assert!(
            std::path::Path::new(test_path).exists(),
            "test file {test_path} must exist (run the demux/audio tests first)"
        );

        let mut ctl = PlaybackController::new();
        ctl.apply(Command::Open(test_path.into())).unwrap();
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Wait for a frame so the pipeline is clearly running before seek.
        let mut saw_frame = false;
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while !saw_frame && std::time::Instant::now() < dl {
            saw_frame = ctl.next_video_frame(1.0 / 60.0).is_some();
            if !saw_frame {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(saw_frame, "must receive a frame before seeking");

        // Seek to 1.5s.  In WSL2 video-only the clock restarts from the
        // seek position, so it should be (near) 1.5 immediately.
        ctl.apply(Command::Seek(1.5)).unwrap();

        // Poll until position is within 0.5s of the target.
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        let mut pos_near = false;
        while std::time::Instant::now() < dl {
            if (ctl.position() - 1.5).abs() < 0.5 {
                pos_near = true;
                break;
            }
            let _ = ctl.next_video_frame(1.0 / 60.0); // keep the pipeline moving
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            pos_near,
            "position must approach 1.5s after seek (was {})",
            ctl.position()
        );

        // The fresh decoder discards frames below the seek target, so the
        // next displayed frame should carry a PTS near 1.5s.
        let mut frame = None;
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while frame.is_none() && std::time::Instant::now() < dl {
            frame = ctl.next_video_frame(1.0 / 60.0);
            if frame.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        let f = frame.expect("must receive a frame after seeking");
        assert!(
            (f.pts_secs - 1.5).abs() < 0.5,
            "frame PTS after seek to 1.5 must be near 1.5 (got {})",
            f.pts_secs
        );

        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }

    #[test]
    fn pause_resume_preserves_position() {
        // Pause must freeze the clock; resume must continue from the same
        // position and let frames flow again.
        let test_path = "/tmp/test_av.mp4";
        assert!(
            std::path::Path::new(test_path).exists(),
            "test file {test_path} must exist (run the demux/audio tests first)"
        );

        let mut ctl = PlaybackController::new();
        ctl.apply(Command::Open(test_path.into())).unwrap();
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Let playback run ~0.3s, then record the position.
        let dl = std::time::Instant::now() + Duration::from_millis(300);
        let mut frames_seen = 0u32;
        while std::time::Instant::now() < dl {
            if ctl.next_video_frame(1.0 / 60.0).is_some() {
                frames_seen += 1;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(frames_seen >= 1, "frames must flow before pausing");
        let pos_at_pause = ctl.position();

        // Pause: position must freeze (allow <50ms drift for llvmpipe).
        ctl.apply(Command::Pause).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Paused);
        thread::sleep(Duration::from_secs(1));
        let pos_frozen = ctl.position();
        assert!(
            (pos_frozen - pos_at_pause).abs() < 0.05,
            "position must freeze while paused (was {pos_at_pause}, now {pos_frozen})"
        );

        // Resume (Toggle from Paused → Playing): position advances and
        // frames flow again.
        ctl.apply(Command::Toggle).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);
        thread::sleep(Duration::from_millis(300));
        let pos_after_resume = ctl.position();
        assert!(
            pos_after_resume > pos_frozen + 0.01,
            "position must advance after resume (frozen {pos_frozen}, now {pos_after_resume})"
        );

        let mut resumed_frame = false;
        let dl = std::time::Instant::now() + Duration::from_secs(5);
        while !resumed_frame && std::time::Instant::now() < dl {
            resumed_frame = ctl.next_video_frame(1.0 / 60.0).is_some();
            if !resumed_frame {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(resumed_frame, "frames must flow after resume");

        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }

    #[test]
    fn eof_transitions_to_ended_and_play_restarts() {
        // At end-of-file the controller must report Ended (not keep Playing
        // forever), freeze the clock, and restart from 0 when Play is
        // pressed again.
        let test_path = "/tmp/test_av.mp4"; // 3s fixture
        assert!(
            std::path::Path::new(test_path).exists(),
            "test file {test_path} must exist (run the demux/audio tests first)"
        );

        let mut ctl = PlaybackController::new();
        ctl.apply(Command::Open(test_path.into())).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Ready);

        // Seek near the end (2.8s of 3s) so the test reaches EOF quickly.
        ctl.apply(Command::Seek(2.8)).unwrap();
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Drive the (virtual) render loop until the controller reports Ended.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while ctl.state() != &PlaybackState::Ended && std::time::Instant::now() < deadline {
            let _ = ctl.next_video_frame(1.0 / 60.0);
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            ctl.state(),
            &PlaybackState::Ended,
            "playback must reach Ended at EOF (position {:.2}s of {:.2}s)",
            ctl.position(),
            ctl.duration()
        );

        // The clock must freeze once Ended (allow small drift).
        let pos = ctl.position();
        thread::sleep(Duration::from_millis(200));
        assert!(
            (ctl.position() - pos).abs() < 0.05,
            "position must freeze after Ended"
        );

        // Play after Ended restarts from the beginning.
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);
        assert!(
            ctl.position() < 1.0,
            "restart must begin near 0 (was {:.2}s)",
            ctl.position()
        );

        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }
}
