use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::HostTrait;

use crate::media::audio::{AudioHandle, spawn_audio_actor};
use crate::media::clock::MediaClock;
use crate::media::demux::Demux;
use crate::media::types::{Command, PlaybackState, VideoFrame};
use crate::media::video::{DecoderCmd, VideoDecoder};

/// A fully-built playback pipeline (demux + audio + video decoders).
///
/// Built on a worker thread (open/seek are slow: file probing, cpal device
/// setup, decoder warm-up) and handed to the controller, which owns it.
/// `Drop` tears every part down, so a pipeline that is discarded because a
/// newer seek superseded it leaves no stray threads behind.
struct Pipeline {
    demux: Option<Demux>,
    video: Option<VideoDecoder>,
    video_cmd: Option<mpsc::Sender<DecoderCmd>>,
    audio: Option<AudioHandle>,
    /// The audio actor thread (owns the cpal stream).  Joined on teardown.
    audio_actor: Option<thread::JoinHandle<()>>,
}

impl Pipeline {
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

        // Join the audio actor.
        if let Some(actor) = self.audio_actor.take() {
            let _ = actor.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Static stream information reported by the demux probe and needed by
/// the UI for audio/video track switching.
#[derive(Debug, Clone, Default)]
struct PipelineInfo {
    duration: f64,
    audio_tracks: Vec<usize>,
    video_tracks: Vec<usize>,
}

/// Seconds of decoded audio the worker pre-rolls before declaring the
/// pipeline ready.  The cpal stream starts paused, so the ring fills with
/// the first samples after the seek target; starting playback then plays
/// audio and video together from the same instant (no startup freeze, no
/// A/V offset).  0.5s gives the ring headroom against the first demux I/O
/// hiccup right after open/seek.
const AUDIO_PREROLL_SECS: f64 = 0.5;
/// Hard cap on the pre-roll wait: a slow disk or a slow decoder must not
/// stall the open/seek forever.
const PREROLL_TIMEOUT: Duration = Duration::from_millis(2000);

/// Build the whole pipeline for `path` at `pos` seconds.  Runs on a worker
/// thread and blocks until the demux probe completes and the decoders have
/// pre-rolled data, so playback can start instantly when the controller
/// installs the result.  Returns the pipeline and the media duration.
fn build_pipeline(
    path: &str,
    pos: f64,
    audio_track: Option<usize>,
    video_track: Option<usize>,
) -> Result<(Pipeline, PipelineInfo), String> {
    // ── Demux ────────────────────────────────────────────────
    let mut demux = Demux::open(path, pos, video_track);

    let info = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match demux.poll_ready() {
                Some(Ok(info)) => break info,
                Some(Err(e)) => {
                    return Err(format!("Demux probe: {e}"));
                }
                None => {
                    if Instant::now() > deadline {
                        return Err("Demux probe timeout".to_string());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    };

    let video_rx = match demux.take_channels() {
        Some(rx) => rx,
        None => return Err("Channels already taken".to_string()),
    };

    // ── Audio ────────────────────────────────────────────────
    // The cpal output stream is !Send, so a dedicated actor thread builds
    // and owns it.  The actor's decode thread reads the AUDIO stream from
    // the file itself — decoupled from the demux, so video backpressure
    // can never starve the audio.
    let host = cpal::default_host();
    let audio_device = host.default_output_device();

    let (audio, audio_actor) = if let Some(dev) = audio_device {
        let (cmd_tx, ready_rx, actor) = spawn_audio_actor(path.to_string(), dev, pos, audio_track);
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(ready)) => (Some(AudioHandle::new(cmd_tx, ready)), Some(actor)),
            Ok(Err(e)) => {
                tracing::warn!("Audio pipeline failed: {e}; proceeding video-only");
                (None, Some(actor))
            }
            Err(_) => {
                tracing::warn!("Audio actor startup timed out; proceeding video-only");
                (None, Some(actor))
            }
        }
    } else {
        (None, None)
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
        let (v, c) = VideoDecoder::from_packets(path, video_rx, pos, video_track);
        (Some(v), Some(c))
    } else {
        // Drop the (never-fed) video receiver immediately.
        (None, None)
    };

    // ── Pre-roll ─────────────────────────────────────────────
    // Wait until the audio ring holds real post-target samples and the
    // video queue has at least one frame, so the transition to Playing is
    // instant and both streams start from the same position.  The audio
    // stream is paused, so nothing is audible until the controller starts
    // playback.
    let deadline = Instant::now() + PREROLL_TIMEOUT;
    loop {
        let audio_ready = match &audio {
            Some(a) => {
                let need = (a.sample_rate as f64 * a.channels as f64 * AUDIO_PREROLL_SECS) as usize;
                a.buffered_samples() >= need
            }
            None => true, // video-only: nothing to pre-roll
        };
        let video_ready = match &video {
            Some(v) => v.buffered() >= 1,
            None => true, // audio-only: nothing to pre-roll
        };
        if (audio_ready && video_ready) || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    Ok((
        Pipeline {
            demux: Some(demux),
            video,
            video_cmd,
            audio,
            audio_actor,
        },
        PipelineInfo {
            duration: info.duration,
            audio_tracks: info.audio_tracks,
            video_tracks: info.video_tracks,
        },
    ))
}

/// Receiving end of an in-flight pipeline build: the worker generation
/// guards against superseded open/seek results.
type PipelineBuildRx = mpsc::Receiver<(u64, Result<(Pipeline, PipelineInfo), String>)>;

pub struct PlaybackController {
    state: PlaybackState,
    volume: f32,
    pipeline: Option<Pipeline>,
    clock: MediaClock,
    has_audio: bool,
    speed: f64,
    duration: f64,
    file_path: Option<String>,
    /// Selected audio/video stream indices (`None` = best/default).
    audio_track: Option<usize>,
    video_track: Option<usize>,
    /// Stream indices available in the current file.
    audio_tracks: Vec<usize>,
    video_tracks: Vec<usize>,
    /// Night mode compresses loud samples in the cpal callback.
    night_mode: bool,
    /// PTS of the last displayed frame.  Sentinel -1.0 so a first frame at
    /// pts 0.0 is displayed (the dedupe threshold is |pts - last_pts| < 0.001).
    last_pts: f64,
    /// Demuxer has reported end-of-file (latched).
    eof_seen: bool,
    /// When the frame queue last delivered a frame; drives the EOF grace
    /// period so buffered frames are shown before `Ended` fires.
    last_frame_at: Option<Instant>,
    /// Audio-clock stall guard state: last observed sample counter.
    last_audio_counter: u64,
    /// Audio-clock stall guard state: when the counter last advanced.
    last_clock_at: Option<Instant>,
    /// True while the wall-clock fallback is active (audio ring dry).
    audio_fallback: bool,
    /// Suppresses transient starvation diagnostics right after open/seek
    /// while fresh decoders produce their first frames.
    startup_grace_until: Option<Instant>,

    // ── Async pipeline build (open/seek run on a worker thread so the
    //    render thread never blocks on file I/O or device probing) ──
    /// Handoff channel from the in-flight build worker.
    pending: Option<PipelineBuildRx>,
    /// Bumped on every teardown/stop: results from superseded workers are
    /// discarded (their `Pipeline` Drop tears the discarded pipeline down).
    generation: u64,
    /// Resume playing once the in-flight open/seek pipeline installs.
    pending_play: bool,
    /// Position the pending pipeline is built for (0.0 on open, target on seek).
    pending_pos: f64,
}

impl Default for PlaybackController {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackController {
    pub fn new() -> Self {
        Self {
            state: PlaybackState::Idle,
            volume: 0.8,
            pipeline: None,
            clock: MediaClock::new(),
            has_audio: false,
            speed: 1.0,
            duration: 0.0,
            file_path: None,
            audio_track: None,
            video_track: None,
            audio_tracks: Vec::new(),
            video_tracks: Vec::new(),
            night_mode: false,
            last_pts: -1.0,
            eof_seen: false,
            last_frame_at: None,
            last_audio_counter: 0,
            last_clock_at: None,
            audio_fallback: false,
            startup_grace_until: None,
            pending: None,
            generation: 0,
            pending_play: false,
            pending_pos: 0.0,
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

    pub fn audio_tracks(&self) -> &[usize] {
        &self.audio_tracks
    }

    pub fn video_tracks(&self) -> &[usize] {
        &self.video_tracks
    }

    pub fn audio_track(&self) -> Option<usize> {
        self.audio_track
    }

    pub fn video_track(&self) -> Option<usize> {
        self.video_track
    }

    pub fn night_mode(&self) -> bool {
        self.night_mode
    }

    /// Ring-buffer underflow count of the audio pipeline (diagnostics).
    pub fn audio_underruns(&self) -> u64 {
        self.pipeline
            .as_ref()
            .and_then(|p| p.audio.as_ref())
            .map(|a| a.underruns())
            .unwrap_or(0)
    }

    /// True for a few seconds right after a pipeline install, while the
    /// fresh decoders are still producing their first frames.  Used to
    /// suppress transient starvation diagnostics and to show a buffering
    /// hint.
    pub fn startup_grace(&self) -> bool {
        self.startup_grace_until
            .map(|t| t > Instant::now())
            .unwrap_or(false)
    }

    /// Whether an audio pipeline is attached (used by diagnostics /
    /// examples; the app infers this from behaviour).
    #[allow(dead_code)]
    pub fn has_audio(&self) -> bool {
        self.has_audio
    }

    /// Decoded video frames waiting ahead of the clock (diagnostics).
    pub fn buffered_frames(&self) -> usize {
        self.pipeline
            .as_ref()
            .and_then(|p| p.video.as_ref())
            .map(|v| v.buffered())
            .unwrap_or(0)
    }

    /// Apply a command: validate, drive the pipeline, and update state.
    pub fn apply(&mut self, cmd: Command) -> Result<(), String> {
        self.poll_pending();
        match cmd {
            Command::Open(path) => self.do_open(&path),
            Command::Play => self.do_play(),
            Command::Pause => self.do_pause(),
            Command::Toggle => self.do_toggle(),
            Command::Seek(pos) => self.do_seek(pos),
            Command::SetSpeed(s) => self.do_set_speed(s),
            Command::SetVolume(v) => self.do_set_volume(v),
            Command::SetNightMode(on) => self.do_set_night_mode(on),
            Command::SetAudioTrack(idx) => self.do_set_audio_track(idx),
            Command::SetVideoTrack(idx) => self.do_set_video_track(idx),
            Command::Stop => self.do_stop(),
        }
    }

    /// Collect a finished open/seek worker and install its pipeline.  Called
    /// from `apply` and `next_video_frame`; cheap when nothing is pending.
    pub fn poll_pending(&mut self) {
        let msg = match self.pending.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(m) => m,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                    if matches!(self.state, PlaybackState::Loading | PlaybackState::Seeking) {
                        self.state = PlaybackState::Error("pipeline worker died".to_string());
                    }
                    return;
                }
            },
            None => return,
        };
        self.pending = None;
        let (worker_gen, result) = msg;
        if worker_gen != self.generation {
            // Superseded by a newer open/seek: dropping the result drops the
            // pipeline, whose Drop tears all its parts down.
            return;
        }
        match result {
            Ok((pipeline, info)) => self.install(pipeline, info),
            Err(e) => {
                tracing::error!("Pipeline build failed: {e}");
                self.state = PlaybackState::Error(format!("Pipeline: {e}"));
            }
        }
    }

    /// Swap a freshly built pipeline in, re-anchor the clock to the target
    /// position, and restore the play/pause state requested at issue time.
    fn install(&mut self, pipeline: Pipeline, info: PipelineInfo) {
        let pos = self.pending_pos;
        let want_play = self.pending_play;
        let was_loading = self.state == PlaybackState::Loading;

        self.duration = info.duration;
        self.audio_tracks = info.audio_tracks;
        self.video_tracks = info.video_tracks;

        // Attach the audio master clock before `reset` so position() derives
        // from the audio counter from the start (reset re-anchors the
        // baseline to `pos`).
        if let Some(a) = &pipeline.audio {
            self.clock
                .attach_audio(a.samples_played.clone(), a.sample_rate, a.channels);
            self.has_audio = true;
        } else {
            self.has_audio = false;
        }

        self.clock.reset(pos);
        self.last_pts = -1.0;
        self.eof_seen = false;
        self.last_frame_at = None;
        self.last_audio_counter = self
            .pipeline
            .as_ref()
            .and_then(|p| p.audio.as_ref())
            .map(|a| a.samples_played.load(Ordering::Relaxed))
            .unwrap_or(0);
        self.last_clock_at = Some(Instant::now());
        self.audio_fallback = false;
        self.startup_grace_until = Some(Instant::now() + Duration::from_secs(3));

        // Re-apply stored speed/volume to the fresh pipeline.
        self.clock.set_speed(self.speed);
        if let Some(a) = &pipeline.audio {
            a.set_speed(self.speed);
            a.set_volume(self.volume);
            a.set_night_mode(self.night_mode);
        }

        self.pipeline = Some(pipeline);

        if want_play {
            self.clock.play(pos);
            if let Some(a) = self.pipeline.as_ref().and_then(|p| p.audio.as_ref()) {
                a.start_stream();
            }
            if let Some(cmd) = self.pipeline.as_ref().and_then(|p| p.video_cmd.as_ref()) {
                let _ = cmd.send(DecoderCmd::Resume);
            }
            self.state = PlaybackState::Playing;
        } else {
            self.clock.pause();
            // Keep the fresh decoders idle (the audio stream stays paused;
            // the video decoder is told not to pull packets).
            if let Some(a) = self.pipeline.as_ref().and_then(|p| p.audio.as_ref()) {
                a.set_paused(true);
            }
            if let Some(cmd) = self.pipeline.as_ref().and_then(|p| p.video_cmd.as_ref()) {
                let _ = cmd.send(DecoderCmd::Pause);
            }
            self.state = if was_loading {
                PlaybackState::Ready
            } else {
                PlaybackState::Paused
            };
        }
    }

    /// Select the frame to display this cycle.
    ///
    /// The decoder keeps a bounded jitter buffer of decoded frames; the
    /// queue pops every frame the media clock has reached and returns the
    /// newest of those (older ones are skipped when the clock is ahead, e.g.
    /// after a speed change).  Frames ahead of the clock stay buffered.
    ///
    /// `lookahead` is the media time until the texture swap takes effect
    /// (about one vsync on a 60 Hz display, scaled by playback speed).  The
    /// app measures the vsync phase so the swap lands on a stable cadence
    /// instead of alternating 2/3 vsyncs as the audio clock jitters.
    pub fn next_video_frame(&mut self, lookahead: f64) -> Option<VideoFrame> {
        self.poll_pending();

        // Audio-clock stall guard: if the audio sample counter has not
        // advanced for 150ms while playing, the audio ring is dry.  Fall
        // back to the wall clock so the video keeps advancing — a frozen
        // video queue blocks the decoder, which blocks the demux, which
        // starves the audio: the fallback breaks that backpressure loop
        // instead of freezing for a full second.  When the counter moves
        // again, trim the ring to the video position and re-attach the
        // audio master clock (continuous position, re-synced audio).
        if self.has_audio && self.state == PlaybackState::Playing {
            let counter = self
                .pipeline
                .as_ref()
                .and_then(|p| p.audio.as_ref())
                .map(|a| a.samples_played.load(Ordering::Relaxed))
                .unwrap_or(0);
            if counter == self.last_audio_counter {
                let stalled = self
                    .last_clock_at
                    .map(|t| t.elapsed() >= Duration::from_millis(150))
                    .unwrap_or(false);
                if stalled && !self.audio_fallback {
                    self.audio_fallback = true;
                    let wall = self.clock.position();
                    self.clock.detach_audio();
                    self.clock.play(wall);
                    tracing::warn!(
                        "audio clock stalled (counter {counter}); falling back to wall clock at {wall:.2}s"
                    );
                }
            } else {
                self.last_audio_counter = counter;
                self.last_clock_at = Some(Instant::now());
                if self.audio_fallback {
                    self.audio_fallback = false;
                    let pos = self.clock.position();
                    if let Some(a) = self.pipeline.as_ref().and_then(|p| p.audio.as_ref()) {
                        a.trim_to(pos);
                        self.clock.attach_audio(
                            a.samples_played.clone(),
                            a.sample_rate,
                            a.channels,
                        );
                    }
                    tracing::info!("audio resumed; re-attached master clock at {pos:.2}s");
                }
            }
        }
        let clock_pos = self.clock.position() + lookahead;
        let (chosen, remaining) = match self.pipeline.as_ref().and_then(|p| p.video.as_ref()) {
            Some(video) => video.drain_upto(clock_pos),
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
        if let Some(pipeline) = &self.pipeline
            && let Some(demux) = &pipeline.demux
            && demux.poll_eof()
        {
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
        self.teardown();

        self.file_path = Some(path.to_string());
        self.audio_track = None;
        self.video_track = None;
        self.audio_tracks.clear();
        self.video_tracks.clear();
        self.pending_pos = 0.0;
        self.pending_play = false;
        // Freeze the clock at 0 while the worker probes/decodes.
        self.clock.pause();
        self.clock.reset(0.0);
        self.state = PlaybackState::Loading;

        self.spawn_pipeline_build(path, 0.0);
        Ok(())
    }

    fn do_play(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Loading | PlaybackState::Seeking => {
                // The pipeline is still being built: remember to start
                // playback the moment it installs.
                self.pending_play = true;
                return Ok(());
            }
            PlaybackState::Ready | PlaybackState::Paused | PlaybackState::Playing => {} // Playing: re-anchor clock, no-op otherwise
            PlaybackState::Ended => {
                // Play after EOF restarts the media from the beginning.
                let path = self
                    .file_path
                    .clone()
                    .ok_or_else(|| "No file open".to_string())?;
                self.teardown();
                self.pending_pos = 0.0;
                self.pending_play = true;
                self.clock.pause();
                self.clock.reset(0.0);
                self.state = PlaybackState::Loading;
                self.spawn_pipeline_build(&path, 0.0);
                return Ok(());
            }
            _ => return Err(format!("cannot play from {:?}", self.state)),
        }

        self.clock.play(self.position());

        if let Some(pipeline) = &self.pipeline {
            if let Some(cmd) = &pipeline.video_cmd {
                let _ = cmd.send(DecoderCmd::Resume);
            }
            if let Some(audio) = &pipeline.audio {
                audio.set_paused(false);
                audio.start_stream();
            }
        }

        self.state = PlaybackState::Playing;
        Ok(())
    }

    fn do_pause(&mut self) -> Result<(), String> {
        match &self.state {
            PlaybackState::Playing | PlaybackState::Seeking => {}
            _ => return Err(format!("cannot pause from {:?}", self.state)),
        }

        self.pending_play = false;
        self.clock.pause();

        if let Some(pipeline) = &self.pipeline {
            if let Some(cmd) = &pipeline.video_cmd {
                let _ = cmd.send(DecoderCmd::Pause);
            }
            if let Some(audio) = &pipeline.audio {
                audio.set_paused(true);
            }
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
            | PlaybackState::Seeking
            | PlaybackState::Loading => self.do_play(),
            _ => Err(format!("cannot toggle from {:?}", self.state)),
        }
    }

    fn do_seek(&mut self, pos: f64) -> Result<(), String> {
        match &self.state {
            PlaybackState::Playing
            | PlaybackState::Paused
            | PlaybackState::Ready
            | PlaybackState::Ended
            | PlaybackState::Seeking
            | PlaybackState::Loading => {} // supersedes the in-flight open
            _ => return Err(format!("cannot seek from {:?}", self.state)),
        }

        let clamped = pos.clamp(0.0, self.duration);
        let was_playing = self.state == PlaybackState::Playing;

        self.teardown();

        let path = match self.file_path.clone() {
            Some(p) => p,
            None => {
                self.state = PlaybackState::Error("No file open".into());
                return Err("No file open".to_string());
            }
        };

        self.pending_pos = clamped;
        self.pending_play = was_playing;
        // Freeze the clock at the target while the worker rebuilds: the
        // transport bar shows the new position immediately.
        self.clock.pause();
        self.clock.reset(clamped);
        self.state = PlaybackState::Seeking;

        self.spawn_pipeline_build(&path, clamped);
        Ok(())
    }

    fn do_set_speed(&mut self, s: f64) -> Result<(), String> {
        self.speed = s;
        self.clock.set_speed(s);
        if let Some(pipeline) = &self.pipeline
            && let Some(audio) = &pipeline.audio
        {
            audio.set_speed(s);
        }
        Ok(())
    }

    fn do_set_volume(&mut self, v: f32) -> Result<(), String> {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(pipeline) = &self.pipeline
            && let Some(audio) = &pipeline.audio
        {
            audio.set_volume(self.volume);
        }
        Ok(())
    }

    fn do_set_night_mode(&mut self, on: bool) -> Result<(), String> {
        self.night_mode = on;
        if let Some(pipeline) = &self.pipeline
            && let Some(audio) = &pipeline.audio
        {
            audio.set_night_mode(on);
        }
        Ok(())
    }

    fn do_set_audio_track(&mut self, idx: usize) -> Result<(), String> {
        if self.audio_tracks.is_empty() {
            return Err("no audio track list".to_string());
        }
        if !self.audio_tracks.contains(&idx) {
            return Err(format!("audio track {idx} not found"));
        }
        self.audio_track = Some(idx);
        let pos = self.position();
        self.do_seek(pos)
    }

    fn do_set_video_track(&mut self, idx: usize) -> Result<(), String> {
        if self.video_tracks.is_empty() {
            return Err("no video track list".to_string());
        }
        if !self.video_tracks.contains(&idx) {
            return Err(format!("video track {idx} not found"));
        }
        self.video_track = Some(idx);
        let pos = self.position();
        self.do_seek(pos)
    }

    fn do_stop(&mut self) -> Result<(), String> {
        self.teardown();
        self.clock = MediaClock::new();
        self.state = PlaybackState::Idle;
        self.last_pts = -1.0;
        self.eof_seen = false;
        self.last_frame_at = None;
        self.last_audio_counter = 0;
        self.last_clock_at = None;
        self.audio_fallback = false;
        self.duration = 0.0;
        self.file_path = None;
        self.pending_play = false;
        self.pending_pos = 0.0;
        Ok(())
    }

    // ── Pipeline lifecycle ───────────────────────────────────────

    /// Spawn the worker that builds the next pipeline.  The result is
    /// collected by `poll_pending`; results from superseded generations are
    /// discarded (and their pipelines torn down by `Drop`).
    fn spawn_pipeline_build(&mut self, path: &str, pos: f64) {
        let worker_gen = self.generation;
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        let p = path.to_string();
        let audio_track = self.audio_track;
        let video_track = self.video_track;
        thread::spawn(move || {
            let result = build_pipeline(&p, pos, audio_track, video_track);
            let _ = tx.send((worker_gen, result));
        });
    }

    /// Tear down the entire pipeline: stop decoders, drop all handles,
    /// join auxiliary threads, detach the audio clock.  Also invalidates
    /// any in-flight worker (its result will be dropped on arrival).
    fn teardown(&mut self) {
        self.generation += 1;
        self.pending = None;
        if let Some(mut pipeline) = self.pipeline.take() {
            pipeline.teardown();
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

    /// Drive `poll_pending` until the controller leaves `wait_states` (or
    /// the deadline passes), so the async open/seek completes.
    fn settle(ctl: &mut PlaybackController, wait_states: &[PlaybackState], timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while wait_states.contains(ctl.state()) && Instant::now() < deadline {
            ctl.poll_pending();
            thread::sleep(Duration::from_millis(10));
        }
    }

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

        // ── Open (async: poll until Ready) ────────────────────────
        ctl.apply(Command::Open(test_path.into())).unwrap();
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
        assert_eq!(
            ctl.state(),
            &PlaybackState::Ready,
            "after Open completes the controller must be Ready"
        );

        // ── Play (and double-Play is valid) ───────────────────────
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);
        ctl.apply(Command::Play).unwrap(); // must be valid (re-anchors clock)
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // ── Drive next_video_frame until we get a frame ────────────
        let mut frame = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while frame.is_none() && Instant::now() < deadline {
            frame = ctl.next_video_frame(1.0 / 60.0);
            if frame.is_none() {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            frame.is_some(),
            "must receive at least one video frame within 5s"
        );

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
        let deadline = Instant::now() + Duration::from_secs(5);
        while frames_after_toggle < 2 && Instant::now() < deadline {
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
        settle(&mut ctl, &[PlaybackState::Seeking], Duration::from_secs(10));
        assert_eq!(
            ctl.state(),
            &PlaybackState::Playing,
            "seek while playing must return to Playing"
        );
        // After seek with video-only (WSL2), poll until position is near 1.0.
        let dl = Instant::now() + Duration::from_secs(5);
        let mut near_target = false;
        while Instant::now() < dl {
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
        let dl = Instant::now() + Duration::from_secs(5);
        while frame_after_seek.is_none() && Instant::now() < dl {
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
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
        assert_eq!(ctl.state(), &PlaybackState::Ready);
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Poll for ~2s, recording (frame PTS, clock position) at the same
        // moment for every frame the controller hands to the (virtual)
        // renderer, plus the wall-clock capture time.
        let t0 = Instant::now();
        let mut samples: Vec<(f64, f64, f64)> = Vec::new(); // (pts, pos, t)
        let deadline = t0 + Duration::from_secs(2);
        while Instant::now() < deadline {
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
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Wait for a frame so the pipeline is clearly running before seek.
        let mut saw_frame = false;
        let dl = Instant::now() + Duration::from_secs(5);
        while !saw_frame && Instant::now() < dl {
            saw_frame = ctl.next_video_frame(1.0 / 60.0).is_some();
            if !saw_frame {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(saw_frame, "must receive a frame before seeking");

        // Seek to 1.5s.  In WSL2 video-only the clock restarts from the
        // seek position, so it should be (near) 1.5 immediately.
        ctl.apply(Command::Seek(1.5)).unwrap();
        settle(&mut ctl, &[PlaybackState::Seeking], Duration::from_secs(10));
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Poll until position is within 0.5s of the target.
        let dl = Instant::now() + Duration::from_secs(5);
        let mut pos_near = false;
        while Instant::now() < dl {
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
        let dl = Instant::now() + Duration::from_secs(5);
        while frame.is_none() && Instant::now() < dl {
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
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
        ctl.apply(Command::Play).unwrap();
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Let playback run ~0.3s, then record the position.
        let dl = Instant::now() + Duration::from_millis(300);
        let mut frames_seen = 0u32;
        while Instant::now() < dl {
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
        let dl = Instant::now() + Duration::from_secs(5);
        while !resumed_frame && Instant::now() < dl {
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
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
        assert_eq!(ctl.state(), &PlaybackState::Ready);

        // Seek near the end (2.8s of 3s) so the test reaches EOF quickly.
        ctl.apply(Command::Seek(2.8)).unwrap();
        ctl.apply(Command::Play).unwrap();
        settle(&mut ctl, &[PlaybackState::Seeking], Duration::from_secs(10));
        assert_eq!(ctl.state(), &PlaybackState::Playing);

        // Drive the (virtual) render loop until the controller reports Ended.
        let deadline = Instant::now() + Duration::from_secs(10);
        while ctl.state() != &PlaybackState::Ended && Instant::now() < deadline {
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

        // Play after Ended restarts from the beginning (async rebuild).
        ctl.apply(Command::Play).unwrap();
        settle(&mut ctl, &[PlaybackState::Loading], Duration::from_secs(10));
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
