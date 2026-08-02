use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Lightweight handle to the audio output's monotonic sample counter.
/// Stored by MediaClock so `position()` can derive real time from the
/// audio device (master clock).  No cpal types in here — this is plain
/// primitives so `media/` stays free of platform audio deps.
#[derive(Debug, Clone)]
pub struct AudioClockRef {
    pub samples_played: Arc<AtomicU64>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Playback position clock.  When an audio source is attached and playing,
/// `position()` reads the audio sample counter (master clock); otherwise it
/// falls back to an Instant-based clock.
#[derive(Debug)]
pub struct MediaClock {
    speed: f64,
    playing: bool,
    start: Option<Instant>,
    start_pos: f64,
    paused_pos: f64,
    audio_clock: Option<AudioClockRef>,
}

impl MediaClock {
    pub fn new() -> Self {
        Self {
            speed: 1.0,
            playing: false,
            start: None,
            start_pos: 0.0,
            paused_pos: 0.0,
            audio_clock: None,
        }
    }

    /// Attach an audio master clock.  While playing, `position()` will
    /// derive time from `samples_played / (sample_rate * channels)` instead
    /// of the Instant-based clock.
    pub fn attach_audio(
        &mut self,
        samples_played: Arc<AtomicU64>,
        sample_rate: u32,
        channels: u16,
    ) {
        self.audio_clock = Some(AudioClockRef { samples_played, sample_rate, channels });
    }

    /// Detach the audio master clock, falling back to the Instant path.
    pub fn detach_audio(&mut self) {
        self.audio_clock = None;
    }

    pub fn set_speed(&mut self, speed: f64) {
        if (self.speed - speed).abs() < 0.01 {
            return;
        }
        let pos = self.position(); // position at the OLD speed — continuity
        self.speed = speed;
        if self.playing {
            self.start = Some(Instant::now());
            self.start_pos = pos;
        }
    }

    pub fn play(&mut self, pos: f64) {
        self.playing = true;
        self.start = Some(Instant::now());
        self.start_pos = pos;
    }

    pub fn pause(&mut self) {
        self.paused_pos = self.position();
        self.playing = false;
        self.start = None;
    }

    /// Current playback position in seconds.
    ///
    /// When an audio clock is attached and the clock is playing, the
    /// position is derived from the audio device's monotonic sample counter
    /// (the "audio master clock").  Otherwise it falls back to an
    /// Instant-based wall clock.
    pub fn position(&self) -> f64 {
        if self.playing {
            if let Some(a) = &self.audio_clock {
                let played = a.samples_played.load(Ordering::Relaxed) as f64;
                (played / (a.sample_rate as f64 * a.channels as f64)) * self.speed
            } else {
                match self.start {
                    Some(t) => self.start_pos + t.elapsed().as_secs_f64() * self.speed,
                    None => self.paused_pos,
                }
            }
        } else {
            self.paused_pos
        }
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn reset(&mut self, pos: f64) {
        self.paused_pos = pos;
        if self.playing {
            self.start = Some(Instant::now());
            self.start_pos = pos;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_advances_only_while_playing() {
        let mut c = MediaClock::new();
        c.play(0.0);
        std::thread::sleep(Duration::from_millis(200));
        assert!(c.position() > 0.1);
        c.pause();
        let p = c.position();
        std::thread::sleep(Duration::from_millis(100));
        assert!((c.position() - p).abs() < 0.02);
    }
    #[test]
    fn clock_speed_scales_position() {
        let mut c = MediaClock::new();
        c.play(0.0);
        std::thread::sleep(Duration::from_millis(100));
        let p1 = c.position();
        c.set_speed(2.0);
        std::thread::sleep(Duration::from_millis(200));
        let p2 = c.position();
        assert!((p2 - p1) > 0.3); // 2x over 200ms ≈ 0.4s
    }
}
