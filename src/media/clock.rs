use std::time::{Duration, Instant};

/// 播放位置时钟。平滑 Instant 时钟；音频主时钟在 Task 6 接入（此结构不变，
/// 只是 `position()` 的实现切换为 samples_played）。
#[derive(Debug)]
pub struct MediaClock {
    speed: f64,
    playing: bool,
    start: Option<Instant>,
    start_pos: f64,
    paused_pos: f64,
}

impl MediaClock {
    pub fn new() -> Self {
        Self { speed: 1.0, playing: false, start: None, start_pos: 0.0, paused_pos: 0.0 }
    }
    pub fn set_speed(&mut self, speed: f64) {
        if (self.speed - speed).abs() < 0.01 { return; }
        let pos = self.position();
        self.speed = speed;
        self.start = Some(Instant::now());
        self.start_pos = pos;
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
    pub fn position(&self) -> f64 {
        match self.start {
            Some(t) => self.start_pos + t.elapsed().as_secs_f64() * self.speed,
            None => self.paused_pos,
        }
    }
    pub fn speed(&self) -> f64 { self.speed }
    pub fn reset(&mut self, pos: f64) {
        self.paused_pos = pos;
        self.start = Some(Instant::now());
        self.start_pos = pos;
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
    fn speed_scales_position() {
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
