# 360° 视频播放器 —— 完整架构重构设计

日期：2026-08-03
状态：已批准（用户已确认方案 A：分层/六边形架构）

## Context

现有代码（3171 行）经过多轮迭代，积累了严重的技术债：

- **app.rs 是"上帝对象"**：事件处理、UI 状态同步、动作分发、渲染决策、状态持久化、截图全部挤在一个文件
- **audio.rs 仍是 ffmpeg CLI 子进程**：最后一个子进程，导致 Windows 上弹出 ffmpeg.exe 控制台窗口、192kHz/6ch 下音频噪声、且需要额外分发 ffmpeg.exe/ffprobe.exe
- **player.rs 积累了临时逻辑**：自愈重启、逐帧排空、resume_pending 等打补丁产物
- **decoder/media.rs 是死代码**（mpv/cpal 回退，未编译）
- **无任何测试**；Windows 打包靠脚本 hack

用户决定**完全重构**：保持现有功能，用专业分层架构重写，消除技术债。

## 目标

1. 清晰的分层架构：`app → media → 展示层`，单向依赖，media 层不依赖窗口/UI
2. 音频改为**进程内解码**（ffmpeg-next），消除最后一个子进程，解决噪声和弹窗
3. **真正的 A/V 同步**（音频时钟为主）
4. 保留全部现有功能：360° 渲染、全部播放控制、截图、拖拽打开、记忆续播、全屏自动隐藏
5. 双平台：Windows（交叉编译）+ Linux（WSL 开发）
6. 单元测试 + 集成测试
7. 更干净的 Windows 打包（仅 exe + 7 个 FFmpeg DLL）

## 非目标

- 不新增功能（字幕、播放列表、网络流、VR 等留待未来）
- 不更换 UI 框架（egui）或渲染后端（wgpu）
- 不做硬件解码加速（保持 ffmpeg 软解 + swscale）

## 架构总览

```
┌─ app/ ──────────────────────────────────────────────┐
│  App（薄接线层）                                     │
│   window_event → PlaybackController 命令             │
│   about_to_wait → 取视频帧 → renderer → UI 状态拉取  │
└──────────────┬──────────────────────────────┬───────┘
               ▼                              ▼
┌─ media/（核心，不依赖窗口/UI）────────────────────────┐
│  PlaybackController（状态机 + 命令入口）              │
│  MediaClock（音频主时钟 → Instant 回退）              │
│  Demux（ffmpeg-next 单 demuxer，路由音频/视频包）      │
│  AudioDecoder + AudioQueue → AudioOutput(cpal)        │
│  VideoDecoder + VideoQueue（有界帧队列）               │
└──────────────────────────────┬───────────────────────┘
                               ▼
┌─ render/ ────┬─ ui/ ────────────────────────────────┐
│  wgpu 渲染器    egui 界面（WMP 风格）                 │
│  sphere/quad   controlbar/进度条/音量/全屏           │
└─────────────────────────────────────────────────────┘
```

**单向依赖**：`app → media`、`app → render/ui`。media 层不知道窗口和 egui 的存在。

## 模块设计

### media/playback.rs —— PlaybackController

**职责**：播放状态机 + 命令入口 + 持有解码管线与时钟。

**状态**：
```
Idle → Loading → Ready ⇄ Playing ⇄ Paused
         ↓ seek          ↓ seek
       Seeking → Playing/Paused
Playing → (EOF) → Ended
任何态 → Error(String)
```

**命令接口**（唯一入口，所有播放控制走这里）：
```rust
pub enum Command { Open(String), Play, Pause, Toggle, Seek(f64), SetSpeed(f64), SetVolume(f32), Stop }
```
- **PlaybackController 运行在主线程**（App 持有），命令由 App 同步调用 `controller.apply(cmd)`；后台线程只有 demux + 解码器，通过 command channel 向它们发指令
- `Open`：拆除旧管线 → 启动新 Demux 线程 → 探测后发 Ready → Ready 时启动音频/视频解码线程
- `Seek`：清空音视频队列 → 重置 MediaClock → Demux 执行 keyframe seek（BACKWARD）→ 解码线程丢弃 seek 前的帧
- `SetSpeed`：更新 MediaClock 速度 + 音频 atempo（进程内滤镜）+ 视频节流
- `SetVolume`：透传 AudioOutput

**对外查询**：
```rust
pub fn state() -> PlaybackState
pub fn position() -> f64      // 来自 MediaClock
pub fn duration() -> f64
pub fn next_video_frame(now: f64) -> Option<VideoFrame>  // 渲染层每帧拉取
pub fn take_actions() -> Vec<UiAction>                   // UI 需要的动作反馈
```

### media/clock.rs —— MediaClock

**职责**：播放位置时钟 + A/V 同步基准。

- **音频为主**：`position = samples_played / sample_rate`（AudioOutput 回调推进 samples_played）。音频是真实的播放节奏，视频跟随。
- **无音频回退**：`position = start_pos + elapsed * speed`（Instant 平滑时钟）。
- **倍速**：音频主时钟天然按 atempo 后播放速度推进；视频回退时钟用 `elapsed * speed`。
- 对外：`position()/duration()/speed()/is_paused()`；`set_speed()/reset_to(pos)`。

### media/demux.rs —— Demux

**职责**：打开文件、探测流、读包路由。

- 用 ffmpeg-next 打开输入，探测视频流 + 音频流（一个 demuxer 同时服务两者，共享时间基）
- 线程内循环：`av_read_frame` → 按 stream index 路由：
  - 音频包 → 音频解码线程的有界 packet channel
  - 视频包 → 视频解码线程的有界 packet channel
- 发送探测结果（width/height/fps/duration、音频格式）→ 控制器
- 支持 `seek(seconds)`（BACKWARD keyframe + 目标前帧丢弃交给解码端）
- 文件结束 → 通知控制器 → `Ended`

### media/audio.rs —— AudioDecoder + AudioOutput

**职责**：进程内解码音频 → PCM → cpal 输出；提供 samples_played 给时钟。

- **AudioDecoder**（ffmpeg-next）：从 packet channel 收包 → `send_packet/receive_frame` → 解码出音频帧 → **重采样**到输出配置（`swresample`）→ 交错 f32 → 推入 AudioQueue（有界环形缓冲）
- **AudioOutput**（cpal）：回调排空 AudioQueue → 设备；`samples_played.fetch_add`
- **倍速**：音频包在解码前经过 ffmpeg-next **atempo 滤镜**（保音调）；速度变化动态生效
- **设备配置**：优先 48000/2ch，失败回退设备原生配置（用户 Realtek 为 192kHz/6ch）
- **缓冲**：环形缓冲容量保证 ≥ 200ms，避免下溢爆音

### media/video.rs —— VideoDecoder + VideoQueue

**职责**：进程内解码视频 → RGBA 帧队列。

- **VideoDecoder**（ffmpeg-next）：收视频包 → 解码 → `swscale` 到 RGBA → 推入 VideoQueue
- **VideoQueue**：有界队列（cap 3），渲染层拉取最新 ≤clock 的帧；帧超前时保留最新待用，落后的丢弃
- **倍速**：解码线程按 `fps × speed` 节流（等效 -readrate），渲染层靠时钟跳帧
- **暂停**：解码线程停止拉包（进程内，位置冻结），恢复从原地续解

### app.rs —— 薄接线层

- `window_event`：输入 → `controller.send_command(...)`；拖拽/滚轮/快捷键映射
- `about_to_wait`：`controller.next_video_frame(clock)` → `renderer.render(...)`；UI 状态从 controller 拉取
- 不再包含业务逻辑（状态持久化、截图等移入独立小模块或 controller 旁）

### ui/ 与 render/

- **ui/**：保留现有 WMP 风格（播放/暂停图标、进度条、音量静音、倍速下拉、全屏、360° 切换）；从 controller 读状态、发命令
- **render/**：保留现有 wgpu 管线（sphere/quad/camera）；`renderer.render(video_frame, egui_shapes)`

## 错误处理

- ffmpeg 打开/解码失败 → 控制器进入 `Error(String)` → UI 显示错误
- 文件缺失/无视频流/无音频流 → 分别处理（音频缺失不影响视频播放）
- 音频设备不可用 → 自动视频-only 模式（Instant 时钟）

## Windows 打包

- 音频进程内后，**不再需要 ffmpeg.exe/ffprobe.exe**（彻底移除运行时 ffmpeg CLI 依赖）
- 只需：`my-project.exe` + 7 个 FFmpeg DLL（avcodec/avformat/avutil/swscale/swresample/avdevice/avfilter）——来自 BtbN 自包含 shared 构建
- `build-win.sh`：FFMPEG_DIR 指向 BtbN 构建（链接导入库）→ 编译 → 自动复制 DLL 到 exe 旁

## 测试策略

**单元测试**：
- `media/playback.rs`：状态机转换合法性、命令在错误状态被拒绝、seek/speed 边界
- `media/clock.rs`：音频时钟推进、Instant 回退、倍速计算、暂停冻结
- `media/video.rs`：帧选择（超前/落后/丢帧）、队列有界性
- `media/audio.rs`：环形缓冲、重采样正确性

**集成测试**：
- 用生成的真实音视频文件，验证：解码出音视频帧、A/V 同步偏差在容忍内、seek 后从正确位置续播、暂停恢复位置不变

## 里程碑

1. `media/` 核心：PlaybackController 状态机 + MediaClock + 视频/音频解码管线（纯逻辑，可单测）
2. `app.rs` 接线 + UI 状态从 controller 拉取
3. 渲染层接入 VideoQueue
4. Windows 打包简化 + 双平台验证
5. 集成测试 + 性能验证（4K 360 视频流畅播放）
