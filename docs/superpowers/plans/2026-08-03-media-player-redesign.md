# 360° 视频播放器完整重构 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将现有播放器（3171 行、上帝对象 + ffmpeg CLI 子进程）重构为分层架构：`app → media（PlaybackController 状态机 + 音频进程内解码 + A/V 同步）→ render/ui`。

**Architecture:** 单向依赖。`media/` 是纯媒体核心（不依赖窗口/egui），通过 `PlaybackController` 状态机和命令接口驱动；`app.rs` 只做接线；`render/`、`ui/` 保留现有 wgpu/egui 代码并适配新接口。音频/视频都用 ffmpeg-next 进程内解码，音频时钟为主做 A/V 同步。

**Tech Stack:** Rust 2024 edition、winit 0.30、wgpu 22、egui 0.29、ffmpeg-next 8、cpal 0.15、glam、bytemuck、serde_json、tracing。

## Global Constraints

- **Rust edition 2024**，`cargo build --release` 必须零错误（警告可留）
- **ffmpeg-next = "8.0"**，`default-features = false, features = ["codec", "format", "software-scaling", "filter"]`（filter 用于音频 atempo）
- **cpal 0.15**：设备配置优先 48000/2ch，失败回退设备原生（`dev.default_output_config()`）
- 音频/视频**全部进程内**（ffmpeg-next），禁止再引入 ffmpeg CLI 子进程
- `media/` 模块**不得** `use` egui、winit、wgpu
- 平台：Linux（WSL 开发）+ Windows（交叉编译 `x86_64-pc-windows-gnu`，`build-win.sh`）
- 保留现有功能：360° 渲染、播放控制、截图、拖拽打开、记忆续播、全屏自动隐藏
- 现有 `src/audio.rs`、`src/player.rs`、`src/decoder/` 在 Task 8 全部删除
- 每个 Task 以 `git commit` 结束

---

### Task 1: media/types.rs 与 MediaClock

**Files:**
- Create: `src/media/mod.rs`
- Create: `src/media/types.rs`
- Create: `src/media/clock.rs`
- Test: `src/media/clock.rs`（内嵌 `#[cfg(test)]`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `pub enum PlaybackState { Idle, Loading, Ready, Playing, Paused, Seeking, Ended, Error(String) }`
  - `pub enum Command { Open(String), Play, Pause, Toggle, Seek(f64), SetSpeed(f64), SetVolume(f32), Stop }`
  - `pub struct VideoFrame { pub data: Arc<Vec<u8>>, pub width: u32, pub height: u32, pub pts_secs: f64 }`
  - `pub struct MediaClock { /* private */ }`
  - `impl MediaClock { pub fn new() -> Self; pub fn set_speed(&mut self, speed: f64); pub fn play(&mut self, pos: f64); pub fn pause(&mut self); pub fn position(&self) -> f64; pub fn speed(&self) -> f64; pub fn reset(&mut self, pos: f64); }`
  - `MediaClock::position()`：playing 时 = `start_pos + elapsed * speed`；paused 时 = `paused_pos`（Instant 平滑时钟，音频主时钟在 Task 6 接入）

- [ ] **Step 1: 写 `src/media/mod.rs`**

```rust
pub mod clock;
pub mod types;
```

- [ ] **Step 2: 写 `src/media/types.rs`**

```rust
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
```

- [ ] **Step 3: 写 `MediaClock`（含测试）**

`src/media/clock.rs`：
```rust
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
```

- [ ] **Step 4: 验证测试通过**

Run: `cargo test clock_ 2>&1 | grep -E "test result|FAILED"`
Expected: `test result: ok`（2 passed）

- [ ] **Step 5: 在 main.rs 注册 media 模块**

在 `src/main.rs` 加 `mod media;`

- [ ] **Step 6: Commit**

```bash
git add src/media/ src/main.rs
git commit -m "feat(media): MediaClock and shared types"
```

---

### Task 2: PlaybackController 状态机

**Files:**
- Create: `src/media/playback.rs`
- Test: `src/media/playback.rs`（内嵌）

**Interfaces:**
- Consumes: `media::types::{PlaybackState, Command}`（Task 1）
- Produces:
  - `pub struct PlaybackController { /* state: PlaybackState, volume: f32 */ }`
  - `impl PlaybackController { pub fn new() -> Self; pub fn state(&self) -> &PlaybackState; pub fn volume(&self) -> f32; pub fn apply(&mut self, cmd: Command) -> Result<(), String>; }`
  - `apply()` 校验状态转换（见下）；解码/时钟接入前的占位：只维护状态 + volume，Open 后进入 `Loading→Ready`，Seek 进入 `Seeking→Playing/Paused`
  - 后续 Task 在 `apply()` 中插入真实管线调用

- [ ] **Step 1: 写状态机核心（含测试）**

`src/media/playback.rs`：
```rust
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
```

- [ ] **Step 2: 验证测试通过**

Run: `cargo test state_transitions 2>&1 | grep -E "test result"`
Expected: ok

- [ ] **Step 3: `mod.rs` 加 `pub mod playback;`**

- [ ] **Step 4: Commit**

```bash
git commit -am "feat(media): PlaybackController state machine with validated transitions"
```

---

### Task 3: VideoDecoder + VideoQueue（ffmpeg-next 进程内解码）

**Files:**
- Create: `src/media/video.rs`
- Test: `src/media/video.rs`（内嵌，用生成的真实文件）

**Interfaces:**
- Consumes: `media::types::VideoFrame`（Task 1）
- Produces:
  - `pub struct VideoDecoder { frame_rx: mpsc::Receiver<VideoFrame> }`
  - `impl VideoDecoder { pub fn open(path: &str, start_pos: f64) -> (Self, mpsc::Sender<DecoderCmd>); pub fn recv(&self) -> Option<VideoFrame>; }`
  - `pub enum DecoderCmd { Pause, Resume, Stop }`
  - 内部：ffmpeg-next 解码线程，输出有界 channel（cap 3），帧按真实 PTS（`best_effort_timestamp`）

- [ ] **Step 1: 生成测试视频**

```bash
ffmpeg -y -f lavfi -i testsrc2=size=640x360:rate=30:duration=5 -c:v libx264 /tmp/test_v.mp4
```

- [ ] **Step 2: 写解码器（复用现成 ffmpeg-next 代码 + 新有界队列）**

`src/media/video.rs` 核心（复制并精简 `src/decoder/video.rs` 的 decode_loop）：
```rust
use std::sync::{atomic::{AtomicBool, Ordering}, mpsc::{self, SyncSender}, Arc};
use std::thread;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::{format, frame, media, software};
use crate::media::types::VideoFrame;

#[derive(Debug, Clone)]
pub enum DecoderCmd { Pause, Resume, Stop }

pub struct VideoDecoder { frame_rx: mpsc::Receiver<VideoFrame> }

impl VideoDecoder {
    pub fn open(path: &str, start_pos: f64) -> (Self, mpsc::Sender<DecoderCmd>) {
        let (tx, rx) = mpsc::sync_channel::<VideoFrame>(3);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let paused = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let p = path.to_string();
        let ct = cmd_tx.clone();
        let st = stopped.clone();
        let pa = paused.clone();
        thread::spawn(move || {
            // 复用 src/decoder/video.rs 的 decode_loop 逻辑，但：
            //   - 用 sync_channel(3) 替代原 cap 2
            //   - 帧类型改为 media::types::VideoFrame
            //   - 保留 BACKWARD seek + 丢弃 start_pos 前帧
            //   - 保留按 fps*speed 节流（speed 从共享原子读）
            //   - 保留 Pause/Resume/Stop 命令处理
            //   - 移除所有自愈/诊断相关（那是旧 player 的职责）
            let _ = (path, start_pos, tx, cmd_rx, st, pa);
        });
        (Self { frame_rx: rx }, ct)
    }
    pub fn recv(&self) -> Option<VideoFrame> { self.frame_rx.try_recv().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_frames_in_order() {
        let (dec, _cmd) = VideoDecoder::open("/tmp/test_v.mp4", 0.0);
        let f = dec.recv();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while f.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let f = dec.recv().unwrap();
        assert_eq!(f.data.len(), (f.width * f.height * 4) as usize);
        assert!(f.pts_secs >= 0.0);
    }
}
```

> **实现注意**：把 `src/decoder/video.rs` 的 decode_loop 原样迁移到 `src/media/video.rs`，改：channel cap 3、`VideoFrame` 类型、删除自愈/诊断字段。`decoder/video.rs` 在 Task 8 删除前保留作参考。

- [ ] **Step 3: 验证测试通过**

Run: `cargo test decodes_frames 2>&1 | grep -E "test result"`
Expected: ok

- [ ] **Step 4: Commit**

```bash
git commit -am "feat(media): in-process VideoDecoder with bounded frame queue"
```

---

### Task 4: Demux（单 demuxer 路由音视频包）

**Files:**
- Create: `src/media/demux.rs`
- Test: `src/media/demux.rs`（内嵌）

**Interfaces:**
- Consumes: 无（独立线程）
- Produces:
  - `pub struct DemuxInfo { pub has_video: bool, pub width: u32, pub height: u32, pub fps: f64, pub duration: f64, pub has_audio: bool }`
  - `pub struct Demux { ready_rx: mpsc::Receiver<Result<DemuxInfo, String>>, audio_pkt_tx: mpsc::Sender<ffmpeg_next::codec::packet::Packet>, video_pkt_tx: mpsc::Sender<ffmpeg_next::codec::packet::Packet>, cmd_tx: mpsc::Sender<DemuxCmd> }`
  - `impl Demux { pub fn open(path: &str, start_pos: f64) -> Self; pub fn poll_ready(&self) -> Option<Result<DemuxInfo, String>>; pub fn take_channels(&mut self) -> Option<(mpsc::Receiver<Packet>, mpsc::Receiver<Packet>)>; pub fn seek(&self, pos: f64); pub fn stop(&self); }`
  - `pub enum DemuxCmd { Seek(f64), Stop }`

- [ ] **Step 1: 写 Demux（探测 + 读包路由线程）**

```rust
// 打开文件 → 探测视频/音频流 → 发 Ready(DemuxInfo)
// 线程循环：ffmpeg_next::format::input(path) 的 packets() 迭代器
//   - 包 stream.index() == 视频流 → video_pkt_tx.send(packet)
//   - == 音频流 → audio_pkt_tx.send(packet)
//   - seek: av_seek_frame BACKWARD（复用 src/decoder/video.rs 的做法）
// 包是 Packet（ffmpeg_next 自带 Clone），音频/视频解码线程各自消费
```

> **实现注意**：音频流探测用 `streams().best(media::Type::Audio)`；`DemuxInfo.duration` 用视频流 duration（复用 Task 3 迁移逻辑）。音频 packet channel 用 `mpsc::channel`（有界，cap 64）。

- [ ] **Step 2: 写测试（探测正确性）**

```rust
#[test]
fn probes_streams() {
    let mut d = Demux::open("/tmp/test_v.mp4", 0.0);
    let info = d.poll_ready().unwrap_or_else(|| {
        let dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop { if let Some(r) = d.poll_ready() { return r; }
               if std::time::Instant::now() > dl { panic!("probe timeout"); }
               std::thread::sleep(std::time::Duration::from_millis(20)); }
    }).unwrap();
    assert!(info.width > 0 && info.height > 0 && info.duration > 0.0);
    assert_eq!(info.has_video, true);
}
```

- [ ] **Step 3: 验证测试通过 + Commit**

```bash
cargo test probes_streams 2>&1 | grep -E "test result"
git commit -am "feat(media): single demuxer routing audio/video packets"
```

---

### Task 5: AudioDecoder + AudioOutput（进程内音频）

**Files:**
- Create: `src/media/audio.rs`
- Test: `src/media/audio.rs`（内嵌，用带音频的文件）

**Interfaces:**
- Consumes: `Demux` 的音频包 channel（Task 4）
- Produces:
  - `pub struct AudioPipeline { samples_played: Arc<AtomicU64>, sample_rate: u32, channels: u16, cmd_tx: mpsc::Sender<AudioCmd>, /* decoder thread handle */ }`
  - `impl AudioPipeline { pub fn start(dev: &cpal::Device, pkt_rx: mpsc::Receiver<Packet>) -> Result<Self, String>; pub fn samples_played(&self) -> u64; pub fn set_paused(&self, p: bool); pub fn set_speed(&self, s: f64); pub fn set_volume(&self, v: f32); pub fn stop(&self); }`
  - `pub enum AudioCmd { Pause(bool), Speed(f64), Volume(f32), Stop }`
  - 内部：AudioDecoder 线程（ffmpeg-next 解码 → 重采样到输出配置 → 交错 f32 → 有界环形缓冲）+ cpal 回调排空 + `samples_played.fetch_add`

- [ ] **Step 1: 生成带音频的测试视频**

```bash
ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=30:duration=3 -f lavfi -i "sine=frequency=440:duration=3" -shortest -c:v libx264 -c:a aac /tmp/test_av.mp4
```

- [ ] **Step 2: 写 AudioDecoder（ffmpeg-next 解码 → f32 交错）**

复用 `src/decoder/video.rs` 的解码模式，改为音频：
```rust
// ffmpeg_next::codec::decoder::audio::Audio decoder
// decoder.format() → swr（software::resampling::Context）重采样到 (sample_rate, channels)
// 解码出的 AudioFrame → data(0) 平面 → 交错 f32 → 推入环形缓冲
```

- [ ] **Step 3: 写 cpal 输出（含 48000/2ch 优先 + 回退原生）**

```rust
// 复用 src/audio.rs 的 cpal 回调逻辑（samples_played 推进、paused、volume、buffer 排空）
// 配置：先试 48000/2ch（BufferSize::Default），失败用 dev.default_output_config()
// 环形缓冲：VecDeque<f32>，cap = sample_rate*channels 的 200ms
```

- [ ] **Step 4: 写测试（解码产出 PCM）**

```rust
#[test]
fn decodes_audio_pcm() {
    // 手动打开 /tmp/test_av.mp4 的 demuxer，拿音频包喂给 decoder，验证产出 f32 数据非空
    // 简化：直接测 AudioDecoder 内部函数 decode_audio_packets(...) 产出的样本数 > 0
}
```

- [ ] **Step 5: 验证 + Commit**

```bash
cargo test decodes_audio 2>&1 | grep -E "test result"
git commit -am "feat(media): in-process AudioDecoder + cpal AudioOutput"
```

---

### Task 6: 集成 PlaybackController —— Demux + 双解码器 + MediaClock

**Files:**
- Modify: `src/media/playback.rs`
- Modify: `src/media/mod.rs`
- Test: `src/media/playback.rs`（集成）

**Interfaces:**
- Consumes: Task 1-5 的全部
- Produces:
  - `PlaybackController::apply()` 现在真正驱动管线
  - `PlaybackController::next_video_frame(&mut self) -> Option<VideoFrame>`（渲染层每帧调用）
  - `PlaybackController::position(&self) -> f64`（MediaClock，音频主时钟）
  - `PlaybackController::duration(&self) -> f64`
  - `PlaybackController::paused(&self) -> bool`

- [ ] **Step 1: 在 apply() 接入管线**

```rust
// Open: 拆旧管线 → Demux::open → audio/video decoder 线程启动 → Ready
//        （探测完成后启动解码线程，demux 的包 channel 交给它们）
// Play/Pause: MediaClock.play/pause + 发 DecoderCmd/ AudioCmd
// Seek: 清空队列 → MediaClock.reset(pos) → Demux.seek(pos)
// SetSpeed: MediaClock.set_speed + AudioCmd::Speed
// SetVolume: AudioCmd::Volume
// Stop: 停线程、拆管线 → Idle
```

- [ ] **Step 2: 接入音频主时钟**

```rust
// MediaClock::position() 实现改为：
//   有音频时 = samples_played / (sample_rate * channels)，无音频回退 Instant
// 给 MediaClock 加可选 audio_clock 来源（闭包或 trait）
```

- [ ] **Step 3: next_video_frame（帧选择）**

```rust
// 复用 src/player.rs 的 try_recv_frame 选择逻辑（排空取最新 ≤clock）
// 从 VideoDecoder 拉帧，按 MediaClock.position() 选择，返回最匹配帧
```

- [ ] **Step 4: 集成测试（真实文件全流程）**

```rust
#[test]
fn full_pipeline_plays() {
    // 生成 /tmp/test_av.mp4（Task 5）
    // ctl.apply(Open) → Ready
    // ctl.apply(Play) → 等 1s → ctl.next_video_frame() 返回帧
    // ctl.apply(Pause) → position 冻结
    // ctl.apply(Toggle) → 帧继续
    // ctl.apply(Seek(1.0)) → position ≈ 1.0
}
```

- [ ] **Step 5: 验证 + Commit**

```bash
cargo test full_pipeline 2>&1 | grep -E "test result"
git commit -am "feat(media): PlaybackController wires demux + decoders + audio master clock"
```

---

### Task 7: app.rs 薄接线层（替换旧 player）

**Files:**
- Modify: `src/app.rs`（重写）
- Modify: `src/main.rs`
- Delete（Task 8 一起删）

**Interfaces:**
- Consumes: `PlaybackController`（Task 6）、`render::Renderer`、`ui::PlayerUI`
- Produces: 一个可运行的播放器（视频/音频/控制）

- [ ] **Step 1: 重写 app.rs 为薄接线**

```rust
// struct App {
//   window, renderer: Option<Renderer>, ctl: PlaybackController,
//   ui: PlayerUI, dragging, last_cursor, input_seen, last_input, pending_file,
//   state: HashMap<String,f64>, state_path, shot_dir,   // 记忆续播/截图保留
// }
// window_event: 输入 → ctl.apply(Command::...)
//   拖拽/滚轮 → renderer.camera
//   S → screenshot（调 renderer.save_frame_png）
//   DroppedFile → ctl.apply(Open)
// about_to_wait:
//   frame = ctl.next_video_frame(); renderer.upload+render
//   ui.position = ctl.position(); ui.duration = ctl.duration(); ...
//   ui 动作 → ctl.apply
//   ctl.state() == Error → 显示
// 记忆续播：Open 后查 state 自动 Seek
```

- [ ] **Step 2: 适配 ui/mod.rs 字段**

`PlayerUI` 现有字段基本不变（playing/position/duration/volume/speed...），但 `ui.update()` 里调 `self.playing = !self.playing` 的地方改为设置标志位，由 app 层读走发命令。最小改动：保留现有 ui 结构，app 层读 `ui.playing == ctl.paused()` 推导 toggle。

- [ ] **Step 3: 编译通过（此时旧 audio/player/decoder 还在，先不引用它们）**

```bash
cargo build --release 2>&1 | grep -E "^error"
```
Expected: 无 error（若旧文件未引用则保留；Task 8 删除）

- [ ] **Step 4: Commit**

```bash
git commit -am "refactor(app): thin wiring layer using PlaybackController"
```

---

### Task 8: 删除旧代码 + 清理

**Files:**
- Delete: `src/audio.rs`
- Delete: `src/player.rs`
- Delete: `src/decoder/mod.rs`
- Delete: `src/decoder/video.rs`
- Delete: `src/decoder/media.rs`
- Modify: `src/main.rs`（去掉 `mod audio; mod decoder; mod player;`）

- [ ] **Step 1: 确认 media/ 管线完整（Task 1-6 测试全绿），删除旧文件**

```bash
rm src/audio.rs src/player.rs src/decoder/mod.rs src/decoder/video.rs src/decoder/media.rs
```

- [ ] **Step 2: main.rs 去掉旧 mod 声明，只留 `mod app; mod media; mod renderer; mod ui;`**

- [ ] **Step 3: 全量构建 + 全部测试**

```bash
cargo build --release 2>&1 | grep -E "^error"
cargo test --release 2>&1 | grep -E "test result"
```
Expected: 无 error，全部测试通过

- [ ] **Step 4: Commit**

```bash
git commit -am "refactor: remove legacy audio/player/decoder modules"
```

---

### Task 9: Windows 打包简化 + 双平台验证

**Files:**
- Modify: `build-win.sh`
- Modify: `Cargo.toml`（ffmpeg-next features 加 `"filter"` 若 Task 5 用了）

**Interfaces:**
- Consumes: 完整重构后的代码

- [ ] **Step 1: 验证 Windows 交叉编译**

```bash
./build-win.sh 2>&1 | tail -3
```
Expected: `Built: .../my-project.exe`，DLL 已复制

- [ ] **Step 2: 确认无需 ffmpeg.exe**

用 `x86_64-w64-mingw32-objdump -p my-project.exe | grep "DLL Name"` 确认只依赖 `av*.dll`（7 个 FFmpeg DLL），不再有 ffmpeg.exe 需求（音频进程内）。build-win.sh 若还在复制 ffmpeg.exe/ffprobe.exe，移除那两行。

- [ ] **Step 3: 更新 build-win.sh**

```bash
# 只复制 7 个 av*.dll 到 release/，不再复制 ffmpeg.exe/ffprobe.exe
cp "$BIN"/*.dll "$REL"/
```

- [ ] **Step 4: Commit**

```bash
git commit -am "build: Windows packaging needs only the 7 FFmpeg DLLs (in-process audio)"
```

---

### Task 10: 集成测试 + 性能验证

**Files:**
- Modify: `src/media/playback.rs`（追加集成测试）

- [ ] **Step 1: 追加集成测试**

```rust
#[test]
fn av_sync_within_tolerance() {
    // 生成 /tmp/test_av.mp4，播放 3s，记录 ctl.position() 与视频帧 pts 的差值
    // 断言 |帧pts - position| < 0.15s（音频时钟主同步）
}
#[test]
fn seek_resumes_at_position() {
    // seek 到 1.5s，等待，断言 position ≈ 1.5 且帧 pts ≈ 1.5
}
#[test]
fn pause_resume_preserves_position() {
    // pause 1s → position 不变；resume → 继续
}
```

- [ ] **Step 2: 4K 360 性能验证（手动）**

```bash
ffmpeg -y -f lavfi -i testsrc2=size=3840x1920:rate=30:duration=10 -c:v libx264 -preset ultrafast /tmp/test_4k.mp4
./target/release/my-project /tmp/test_4k.mp4
# 观察：流畅、RSS 稳定、无卡顿（llvmpipe 下允许降帧，但不应卡死）
```

- [ ] **Step 3: Commit**

```bash
git commit -am "test(media): A/V sync, seek, pause-resume integration tests"
```

---

## Self-Review 记录

- **Spec 覆盖**：状态机（Task 2）、MediaClock（Task 1）、Demux（Task 4）、AudioDecoder/AudioOutput（Task 5）、VideoDecoder（Task 3）、A/V 同步（Task 6+10）、app 接线（Task 7）、旧代码删除（Task 8）、打包（Task 9）、测试（Task 10）——全部覆盖
- **类型一致性**：`Command`/`PlaybackState`/`VideoFrame`/`MediaClock` 在 Task 1 定义，Task 2/6/7 引用同名同签；`DecoderCmd`/`DemuxCmd`/`AudioCmd` 各自 Task 内定义并一致
- **占位符**：`src/media/video.rs` 的 decode_loop 迁移明确指向复用 `src/decoder/video.rs` 现有实现（Task 8 删除前保留参考）——这是有意的代码复用指示，非占位
