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
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub pts_secs: f64,
}
