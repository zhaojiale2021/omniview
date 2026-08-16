use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
    /// Baseline for the audio master clock: the sample counter value and the
    /// position it maps to.  Re-anchored on attach/play/reset/set_speed so
    /// the audio branch behaves like the Instant branch (baseline + delta),
    /// giving position discontinuities-free speed changes and seeks.
    audio_base_samples: u64,
    audio_base_pos: f64,
}

impl Default for MediaClock {
    fn default() -> Self {
        Self::new()
    }
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
            audio_base_samples: 0,
            audio_base_pos: 0.0,
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
        let base_samples = samples_played.load(Ordering::Relaxed);
        let base_pos = self.position();
        self.audio_clock = Some(AudioClockRef { samples_played, sample_rate, channels });
        self.audio_base_samples = base_samples;
        self.audio_base_pos = base_pos;
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
        // Re-anchor the audio master clock at the same counter value with the
        // OLD position so a live speed change is seamless (no jump).
        if let Some(a) = &self.audio_clock {
            self.audio_base_samples = a.samples_played.load(Ordering::Relaxed);
            self.audio_base_pos = pos;
        }
    }

    pub fn play(&mut self, pos: f64) {
        self.playing = true;
        self.start = Some(Instant::now());
        self.start_pos = pos;
        // Re-anchor so the audio branch starts at `pos` from the current
        // counter value (seeks at speed != 1 land on `pos`, not pos*speed).
        if let Some(a) = &self.audio_clock {
            self.audio_base_samples = a.samples_played.load(Ordering::Relaxed);
            self.audio_base_pos = pos;
        }
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
                let played = a.samples_played.load(Ordering::Relaxed);
                // Baseline + delta, mirroring the Instant branch.  `played`
                // is monotonic and `audio_base_samples` was captured from it,
                // so saturating_sub guards any transient skew.
                let delta = played.saturating_sub(self.audio_base_samples) as f64;
                self.audio_base_pos
                    + delta / (a.sample_rate as f64 * a.channels as f64) * self.speed
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

    pub fn reset(&mut self, pos: f64) {
        self.paused_pos = pos;
        if self.playing {
            self.start = Some(Instant::now());
            self.start_pos = pos;
        }
        // Re-anchor the audio master clock at `pos` from the current counter
        // (open/seek at speed != 1 must not multiply the target by speed).
        if let Some(a) = &self.audio_clock {
            self.audio_base_samples = a.samples_played.load(Ordering::Relaxed);
            self.audio_base_pos = pos;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn audio_clock_speed_change_is_continuous() {
        // Fake audio master clock: no cpal device needed.  48 kHz / 2 ch.
        let rate = 48000u32;
        let ch = 2u16;
        let sec = rate as u64 * ch as u64; // samples per second
        let counter = Arc::new(AtomicU64::new(0));

        let mut c = MediaClock::new();
        c.attach_audio(counter.clone(), rate, ch);
        c.play(0.0);

        // Simulate 10s played at 1x: counter = 10s of samples.
        counter.store(10 * sec, Ordering::Relaxed);
        assert!((c.position() - 10.0).abs() < 0.001, "10s @ 1x");

        // Live speed change to 2x must NOT jump to 20s — it stays at 10s
        // (continuity), and subsequent samples advance at 2x.
        c.set_speed(2.0);
        assert!(
            (c.position() - 10.0).abs() < 0.001,
            "position must stay ≈10s after set_speed(2.0), got {}",
            c.position()
        );

        // Advance the counter by 1 more second of samples → +1s × 2.0 = +2s.
        counter.store(11 * sec, Ordering::Relaxed);
        assert!(
            (c.position() - 12.0).abs() < 0.001,
            "10s + 1s at 2x must be ≈12s, got {}",
            c.position()
        );

        // reset() re-anchors: position must jump to the target, not
        // target × speed, and stay put until the counter advances.
        c.reset(5.0);
        assert!(
            (c.position() - 5.0).abs() < 0.001,
            "reset(5.0) at speed 2.0 must be ≈5s, got {}",
            c.position()
        );
        counter.store(12 * sec, Ordering::Relaxed); // +1s of samples
        assert!(
            (c.position() - 7.0).abs() < 0.001,
            "5s + 1s at 2x must be ≈7s, got {}",
            c.position()
        );
    }
}
