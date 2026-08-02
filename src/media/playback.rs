use crate::media::types::{Command, PlaybackState};

pub struct PlaybackController {
    state: PlaybackState,
    volume: f32,
}

impl PlaybackController {
    pub fn new() -> Self {
        Self { state: PlaybackState::Idle, volume: 0.8 }
    }
    pub fn state(&self) -> &PlaybackState { &self.state }
    pub fn volume(&self) -> f32 { self.volume }

    /// 应用命令：校验状态转换，更新状态。管线操作在 Task 4-6 接入。
    pub fn apply(&mut self, cmd: Command) -> Result<(), String> {
        match cmd {
            Command::Open(_) => {
                self.state = PlaybackState::Loading;
                // Task 4: 启动 Demux，探测完成后进 Ready
                self.state = PlaybackState::Ready;
                Ok(())
            }
            Command::Play => {
                self.state = match &self.state {
                    PlaybackState::Ready | PlaybackState::Paused | PlaybackState::Ended
                    | PlaybackState::Seeking => PlaybackState::Playing,
                    _ => return Err(format!("cannot play from {:?}", self.state)),
                };
                Ok(())
            }
            Command::Pause => {
                self.state = match &self.state {
                    PlaybackState::Playing | PlaybackState::Seeking => PlaybackState::Paused,
                    _ => return Err(format!("cannot pause from {:?}", self.state)),
                };
                Ok(())
            }
            Command::Toggle => {
                match &self.state {
                    PlaybackState::Playing => self.state = PlaybackState::Paused,
                    PlaybackState::Paused | PlaybackState::Ready | PlaybackState::Ended => {
                        self.state = PlaybackState::Playing
                    }
                    _ => return Err(format!("cannot toggle from {:?}", self.state)),
                }
                Ok(())
            }
            Command::Seek(_) => {
                if !matches!(self.state, PlaybackState::Playing | PlaybackState::Paused
                                    | PlaybackState::Ready | PlaybackState::Ended | PlaybackState::Seeking) {
                    return Err(format!("cannot seek from {:?}", self.state));
                }
                let was_playing = self.state == PlaybackState::Playing;
                self.state = PlaybackState::Seeking;
                // Task 4: 实际 seek 完成后
                self.state = if was_playing { PlaybackState::Playing } else { PlaybackState::Paused };
                Ok(())
            }
            Command::SetSpeed(_) => Ok(()),
            Command::SetVolume(v) => { self.volume = v.clamp(0.0, 1.0); Ok(()) }
            Command::Stop => { self.state = PlaybackState::Idle; Ok(()) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_transitions_are_validated() {
        let mut c = PlaybackController::new();
        assert_eq!(c.state(), &PlaybackState::Idle);
        assert!(c.apply(Command::Pause).is_err()); // Idle 不能 Pause
        c.apply(Command::Open("/x".into())).unwrap();
        assert_eq!(c.state(), &PlaybackState::Ready);
        c.apply(Command::Toggle).unwrap();
        assert_eq!(c.state(), &PlaybackState::Playing);
        c.apply(Command::Pause).unwrap();
        assert_eq!(c.state(), &PlaybackState::Paused);
        c.apply(Command::Seek(5.0)).unwrap();
        assert_eq!(c.state(), &PlaybackState::Paused); // 暂停中 seek 保持暂停
        c.apply(Command::SetVolume(1.5)).unwrap();
        assert_eq!(c.volume(), 1.0);
    }
}
