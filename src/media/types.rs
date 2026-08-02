use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackState {
    Idle, Loading, Ready, Playing, Paused, Seeking, Ended,
    Error(String),
}

#[derive(Debug, Clone)]
pub enum Command {
    Open(String), Play, Pause, Toggle, Seek(f64), SetSpeed(f64), SetVolume(f32), Stop,
}

#[derive(Clone)]
pub struct VideoFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_secs: f64,
}
