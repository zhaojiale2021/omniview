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
}
