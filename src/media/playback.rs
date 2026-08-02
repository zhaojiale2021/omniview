use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
    last_pts: f64,
    pending: Option<VideoFrame>,
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
            last_pts: 0.0,
            pending: None,
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
    /// Ported from the legacy player's `try_recv_frame`: drain all
    /// available frames, pick the newest one whose PTS is at/before the
    /// clock, and hold one ahead in `pending`.  Returns None when no new
    /// displayable frame is available.
    pub fn next_video_frame(&mut self) -> Option<VideoFrame> {
        let clock_pos = self.clock.position();
        let mut chosen: Option<VideoFrame> = None;
        let mut newest_ahead: Option<VideoFrame> = None;

        // The frame held from the last call.
        if let Some(f) = self.pending.take() {
            if f.pts_secs <= clock_pos {
                chosen = Some(f);
            } else {
                newest_ahead = Some(f);
            }
        }

        // Drain all available frames this cycle.
        if let Some(ref video) = self.video {
            while let Some(f) = video.recv() {
                if f.pts_secs <= clock_pos {
                    chosen = Some(f);
                } else {
                    newest_ahead = Some(f);
                }
            }
        }
        self.pending = newest_ahead;

        let frame = match chosen {
            Some(f) => f,
            None => return None,
        };

        // Skip frames whose PTS matches the last displayed PTS (avoid
        // re-rendering the same frame).
        if (frame.pts_secs - self.last_pts).abs() < 0.001 {
            return None;
        }
        self.last_pts = frame.pts_secs;
        Some(frame)
    }

    // ── Command handlers ──────────────────────────────────────────

    fn do_open(&mut self, path: &str) -> Result<(), String> {
        self.teardown();

        self.file_path = Some(path.to_string());
        self.state = PlaybackState::Loading;

        self.open_pipeline(path, 0.0)?;

        self.state = PlaybackState::Ready;
        Ok(())
    }

    fn do_play(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Ready
            | PlaybackState::Paused
            | PlaybackState::Ended
            | PlaybackState::Seeking
            | PlaybackState::Playing => {} // Playing: re-anchor clock, no-op otherwise
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

        // Restore play/pause state after the restart.
        if was_playing {
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
        if let Some(ref video) = self.video {
            video.set_speed(s);
        }
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
        self.last_pts = 0.0;
        self.pending = None;
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
                Err(e) => {
                    tracing::warn!("Audio pipeline failed: {e}; proceeding video-only");
                    self.has_audio = false;
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
        let (video, video_cmd) = VideoDecoder::from_packets(path, video_rx, pos);

        self.demux = Some(demux);
        self.video = Some(video);
        self.video_cmd = Some(video_cmd);
        self.audio = audio;
        self.clock.reset(pos);
        self.last_pts = 0.0;
        self.pending = None;

        Ok(())
    }

    /// Tear down the entire pipeline: stop decoders, drop all handles,
    /// join auxiliary threads, detach the audio clock.
    fn teardown(&mut self) {
        // Stop video decoder.
        if let Some(cmd) = self.video_cmd.take() {
            let _ = cmd.send(DecoderCmd::Stop);
        }
        self.video = None;

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
            frame = ctl.next_video_frame();
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
            if ctl.next_video_frame().is_some() {
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
            let _ = ctl.next_video_frame();
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
            frame_after_seek = ctl.next_video_frame();
            if frame_after_seek.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(frame_after_seek.is_some(), "frames must flow after seek");

        // ── Stop → Idle ───────────────────────────────────────────
        ctl.apply(Command::Stop).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Idle);
    }
}
