use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackState {
    Idle, Loading, Ready, Playing, Paused, Seeking, Ended,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum Command {
    Open(String),
    Play,
    #[allow(dead_code)] // explicit pause binding; UI/keyboard currently uses Toggle
    Pause,
    Toggle,
    Seek(f64),
    SetSpeed(f64),
    SetVolume(f32),
    #[allow(dead_code)] // controller-level stop; window close tears down via Drop
    Stop,
}

#[derive(Clone)]
pub struct VideoFrame {
    /// Luma plane (NV12), row-aligned to the GPU upload stride.
    pub y: Arc<Vec<u8>>,
    /// Interleaved CbCr plane (NV12), row-aligned to the GPU upload stride.
    pub uv: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Bytes per row of `y` (multiple of 256, required by wgpu uploads).
    pub y_stride: u32,
    /// Bytes per row of `uv` (multiple of 256).
    pub uv_stride: u32,
    pub pts_secs: f64,
    /// FNV-1a checksum of the Y plane computed when the frame was pushed;
    /// the renderer recomputes it before upload to detect data corruption
    /// (a mismatch means buffer reuse raced the producer).  Diagnostics.
    pub y_checksum: u64,
}

/// FNV-1a 64-bit hash (fast, no dependencies).  Used to fingerprint frame
/// plane data so corruption can be detected across thread boundaries.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
