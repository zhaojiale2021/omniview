use libmpv::{Mpv, FileState};

/// Audio + clock controller powered by mpv.
pub struct AudioPlayer {
    mpv: Mpv,
}

impl AudioPlayer {
    pub fn open(path: &str) -> Result<Self, String> {
        let mpv = Mpv::new().map_err(|e| format!("mpv: {e}"))?;
        mpv.set_property("vo", "null").map_err(|e| format!("mpv: {e}"))?;
        mpv.set_property("video", "no").map_err(|e| format!("mpv: {e}"))?;
        let _ = mpv.playlist_load_files(&[(path, FileState::AppendPlay, None::<&str>)]);
        Ok(Self { mpv })
    }

    pub fn set_paused(&self, p: bool) { let _ = self.mpv.set_property("pause", p); }
    pub fn is_paused(&self) -> bool { self.mpv.get_property::<bool>("pause").unwrap_or(true) }

    pub fn seek(&self, secs: f64) { let _ = self.mpv.command("seek", &[&format!("{secs}"), "absolute"]); }
    pub fn set_speed(&self, s: f64) { let _ = self.mpv.set_property("speed", s); }
    pub fn set_volume(&self, v: f32) { let _ = self.mpv.set_property("volume", (v * 100.0).clamp(0.0, 100.0) as i64); }

    /// Master clock: current playback position from mpv
    pub fn clock(&self) -> f64 {
        self.mpv.get_property::<f64>("time-pos").unwrap_or(0.0)
    }
}
