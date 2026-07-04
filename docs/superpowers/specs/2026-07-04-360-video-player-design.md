# 360° 视频播放器设计文档

## 概述

基于 Rust 的桌面原生 360° 视频播放器，支持本地文件和网络流媒体播放，具备 VR 头显兼容、空间音频、交互热点和字幕等完整功能。

## 技术栈

| 模块 | 选型 | 说明 |
|------|------|------|
| 语言 | Rust | edition 2024, rustc 1.96+ |
| 窗口/输入 | `winit` | 跨平台窗口管理和事件循环 |
| 3D 渲染 | `wgpu` | Vulkan/Metal/DX12 后端，WGSL 着色器 |
| 视频解码 | `ffmpeg-next` | FFmpeg 绑定，支持 HW 加速 |
| 音频 | `cpal` | 跨平台音频输出 |
| UI | `egui` | 即时模式 GUI，作为 3D 场景覆盖层 |
| VR | `openxr` | OpenXR 绑定，支持主流 VR 运行时 |
| 字幕 | `ab_glyph` | 字形渲染，SRT/ASS 解析 |
| 网络流 | ffmpeg 内建 | HLS (m3u8) / DASH (mpd) |

## 系统架构

```
┌──────────────────────────────────────────────────────────────┐
│                        winit (窗口/事件循环)                   │
│  ┌────────────────────────────────────────────────────┐      │
│  │              egui UI Overlay                        │      │
│  │  [播放/暂停] [进度条] [音量] [画中画] [截图] [VR] [⚙] │      │
│  └────────────────────────┬───────────────────────────┘      │
│  ┌────────────────────────▼───────────────────────────┐      │
│  │              wgpu 渲染引擎                           │      │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │      │
│  │  │球体网格   │ │投影着色器 │ │后处理链          │   │      │
│  │  │(64×32)   │ │Equirect  │ │畸变/色调/HDR     │   │      │
│  │  │Cubemap   │ │FishEye   │ │                   │   │      │
│  │  └──────────┘ └──────────┘ └──────────────────┘   │      │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │      │
│  │  │字幕渲染   │ │VR双目渲染 │ │空间音频可视化    │   │      │
│  │  └──────────┘ └──────────┘ └──────────────────┘   │      │
│  └────────────────────────────────────────────────────┘      │
└──────────────────────────────────────────────────────────────┘
         ▲ 纹理帧 (wgpu Texture)          ▲ 音频 PCM
┌────────────────────────┐    ┌────────────────────────┐
│  视频解码器管道          │    │  音频子系统             │
│  ffmpeg-next:          │    │  ffmpeg + cpal:        │
│  ┌──────────────────┐  │    │  ┌──────────────────┐  │
│  │ AVFormat (demux) │  │    │  │ Audio Decoder    │  │
│  ├──────────────────┤  │    │  ├──────────────────┤  │
│  │ HW Accel Detect  │  │    │  │ swr (重采样)      │  │
│  │ (VAAPI/NVDEC/   │  │    │  ├──────────────────┤  │
│  │  VideoToolbox)   │  │    │  │ HRTF Spatializer │  │
│  ├──────────────────┤  │    │  ├──────────────────┤  │
│  │ Decoder → Frame  │  │    │  │ cpal Ring Buffer │  │
│  │ Queue (ringbuf)  │  │    │  └──────────────────┘  │
│  └──────────────────┘  │    └────────────────────────┘
└────────────────────────┘
         ▲
┌─────────────────────────────────────┐
│  播放控制器 (PlaybackController)     │
│  ┌──────────┐ ┌────────┐ ┌──────┐  │
│  │状态机    │ │同步    │ │播放  │  │
│  │Play/Pause│ │A/V Sync│ │列表  │  │
│  │Seek/Stop │ │Drop    │ │管理  │  │
│  │          │ │Policy  │ │      │  │
│  └──────────┘ └────────┘ └──────┘  │
│  ┌──────────────────────────────┐  │
│  │ 网络流 (ffmpeg AVIO)          │  │
│  │ HLS / DASH / RTMP           │  │
│  └──────────────────────────────┘  │
└─────────────────────────────────────┘
```

## 模块详解

### 1. 视频解码管道

**输入层：**
- 本地文件: `std::fs::File` → ffmpeg `AVFormatContext`
- 网络流: ffmpeg `AVIO` 内置协议 (HTTP/HLS/DASH)
- 自动检测格式和流索引

**硬件解码策略：**
```
平台检测 →
  Linux:   VAAPI (Intel/AMD) / NVDEC (NVIDIA)
  Windows: DXVA2 / NVDEC
  macOS:   VideoToolbox
  回退:    sw (CPU 解码)
```

硬件解码路径：`hw_frames_ctx` → GPU 内存 → 通过 `wgpu` import 外部纹理 → 零拷贝渲染。

软件解码路径：`avcodec_decode_video2` → `swscale` YUV→RGBA → 上传 `wgpu Texture`。

**帧队列：**
```rust
struct FrameQueue {
    frames: VecDeque<DecodedFrame>,
    max_size: usize,
    // 时间戳管理
    last_pts: i64,
    // 丢帧策略
    drop_policy: DropPolicy,
}

enum DropPolicy {
    /// 渲染落后时跳过中间帧
    SkipLate { threshold_ms: u32 },
    /// 严格按序渲染（可能卡顿）
    Strict,
}
```

**支持的格式：**
- 容器: MP4, MKV, WebM, AVI, MOV, TS
- 视频: H.264, H.265/HEVC, VP9, AV1, MPEG-4
- 音频: AAC, MP3, Opus, Vorbis, FLAC, PCM

### 2. 3D 渲染引擎

**球体网格生成：**
```rust
struct Sphere {
    // 经度分段: segments_u (默认 64)
    // 纬度分段: segments_v (默认 32)
    // 顶点: position, uv, normal
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    index_count: u32,
}
```

细分精度可配置：低 (32×16) 用于性能优先，高 (256×128) 用于质量优先。

**投影着色器 (WGSL)：**

- **Equirectangular** (默认): `uv → textureSample(videoTexture, sampler, uv)`
- **Cubemap**: 方向向量 `(x,y,z)` → 选择面 + 计算 UV
- **EAC (Equiangular Cubemap)**: `tan(uv)` 重映射后再采 6 面
- **Fisheye**: 鱼眼反畸变 `(r,θ) → (φ,λ)` 映射
- **Little Planet**: UV Y 轴翻转 + 小视场角压缩

**渲染流程（每帧）：**
1. 从 FrameQueue 取当前 PTS 对应的帧
2. 上传帧数据到 wgpu Texture（或复用已存在的 GPU 纹理）
3. 设置 uniform: 视角矩阵 (yaw/pitch/roll/fov)
4. 绘制球体网格 (视口覆盖整个屏幕)
5. 后处理: 色调映射 (HDR→SDR), 伽马校正
6. VR 模式：额外 Barrel Distortion
7. 叠加 UI (egui) 和字幕
8. 交换链呈现 (present)

**视角交互：**
```rust
struct OrbitCamera {
    yaw: f32,    // 水平旋转 (-180° ~ 180°)
    pitch: f32,  // 垂直旋转 (-90° ~ 90°)
    roll: f32,   // 滚动 (默认 0)
    fov: f32,    // 视野 (60° ~ 120°, 默认 90°)
    sensitivity: f32, // 鼠标灵敏度
}
```

| 交互 | 操作 |
|------|------|
| 拖拽旋转 | 鼠标左键/单指拖动 |
| 缩放 | 滚轮 / 双指捏合 |
| 重置视角 | 双击 / R 键 |
| 全屏 | F 键 / 双击 |
| 快速旋转 | Shift + 拖动 |

### 3. VR 子系统 (OpenXR)

**设计原则：** 非 VR 模式作为第一公民；VR 支持作为可选扩展。

```
非 VR 模式: winit 窗口 → wgpu 单通道渲染
VR 模式:    OpenXR → wgpu 双通道渲染 → HMD 呈现

动态切换: 启动时检测 OpenXR runtime，用户可在设置中开启/关闭 VR
```

**OpenXR 集成点：**
- `openxr` 实例 + 会话管理
- 双目 swapchain 创建 (与 wgpu 共享 device)
- 参考空间: `ViewSpace` (头部追踪), `LocalSpace` (房间定位)
- 输入: 控制器 trigger/触摸板

**Barrel Distortion 着色器：**
为 VR 镜头补偿光学畸变，使用 OpenXR 提供的畸变参数在片元着色器中逆向映射。

### 4. 音频子系统

```rust
struct AudioPipeline {
    // ffmpeg 解码
    audio_decoder: AudioDecoder,
    // 重采样到统一格式
    resampler: SwrContext,
    // 空间音频处理器
    spatializer: HrtfSpatializer,
    // cpal 输出
    output: CpalOutput,
}
```

**空间音频：**
- 用户旋转视角时，声场锁定在世界空间（不跟随头部旋转）
- 使用 HRTF (Head-Related Transfer Function) 实现简单的双耳空间化
- 支持 5.1/7.1 声道映射到空间位置

**音视频同步策略：**
- 主时钟: 音频时钟（audio 驱动 video）
- 同步机制: 比较 audio pts 和 video pts，差值超过阈值则追赶/等待
- 丢帧阈值: 视频落后 > 40ms 则跳过帧

### 5. UI 叠加层 (egui)

egui 与 wgpu 共享渲染上下文，作为透明叠加层绘制在 3D 场景之上。

**UI 布局：**

| 区域 | 内容 |
|------|------|
| 底部栏 | 播放/暂停、进度条、时间显示、音量滑块、画中画切换 |
| 顶部栏 | 文件名、窗口控制（原生）、截图按钮、VR 模式切换、设置 |
| 设置面板 | 投影格式切换、渲染质量、音频设备、字幕样式、快捷键 |
| 右键菜单 | 播放列表、打开文件、最近播放 |
| HUD | 当前分辨率、帧率、码率信息 |

**交互路由：**
```
winit 原始事件
    ↓
egui::Context::raw_input() 投递事件
    ↓
egui 判断是否命中 UI 控件
  ├─ 是 → 由 UI 处理，不传递给 3D 场景
  └─ 否 → 传递给 OrbitCamera / VR 控制器
```

### 6. 字幕系统

```rust
enum SubtitleFormat {
    SRT,
    ASS(AssParser),  // 支持 ASS 样式/动画
    WebVTT,
}

struct SubtitleRenderer {
    parser: SubtitleParser,
    font_loader: FontLoader,      // ab_glyph
    tracks: Vec<SubtitleTrack>,
    style: SubtitleStyle,
    // 渲染到纹理 → 叠加到 3D 场景或帧上
    render_target: wgpu::Texture,
}
```

- SRT: 时间轴 + 纯文本
- ASS: 完整样式解析（字体、颜色、位置、Karaoke）
- WebVTT: 类似 SRT 的 Web 标准

### 7. 热点交互系统 (Hotspots)

```rust
struct Hotspot {
    id: String,
    // 球面上的位置
    theta: f32,   // 经度
    phi: f32,     // 纬度
    // 交互
    action: HotspotAction,
    label: String,
    // 渲染: 在 3D 场景中标记位置
    icon: Option<wgpu::Texture>,
}

enum HotspotAction {
    OpenUrl(String),
    SwitchCamera { yaw: f32, pitch: f32 },
    ShowInfo(String),
    PlayAudio(String),
}
```

- 热点从外部配置文件 (JSON/XML) 载入
- 在 3D 空间中渲染发光指示器
- 鼠标悬停显示标签，点击触发动作

### 8. 播放控制与状态机

```rust
enum PlaybackState {
    Idle,        // 尚未加载媒体
    Loading,     // 正在加载/缓冲
    Playing,     // 播放中
    Paused,      // 暂停
    Seeking,     // 正在跳转
    Ended,       // 播放结束
    Error(String), // 错误状态
}

struct PlaybackController {
    state: PlaybackState,
    playlist: Vec<MediaItem>,
    current_index: usize,
    // 时间
    position: Duration,
    duration: Duration,
    // 同步
    sync: AVSync,
}
```

### 9. 画中画模式 (PiP)

在桌面非 VR 模式下：
- 点击画中画 → 播放器切换到一个小型浮动窗口（始终置顶）
- 独立于主窗口的渲染循环
- 可拖动位置、调整大小
- 适合边工作边观看

## 项目结构

```
src/
├── main.rs                    # 入口
├── app.rs                     # 应用主循环 (winit 事件循环)
├── renderer/
│   ├── mod.rs                 # 渲染引擎
│   ├── pipeline.rs            # wgpu 渲染管道
│   ├── sphere.rs              # 球体网格生成
│   ├── shaders/               # WGSL 着色器
│   │   ├── equirect.wgsl
│   │   ├── cubemap.wgsl
│   │   ├── post_process.wgsl
│   │   └── barrel_distortion.wgsl
│   ├── texture.rs             # 视频帧纹理管理
│   └── camera.rs              # 轨道相机 + VR 相机
├── decoder/
│   ├── mod.rs                 # 解码器模块
│   ├── video.rs               # 视频解码 (ffmpeg)
│   ├── audio.rs               # 音频解码 (ffmpeg)
│   ├── hw_accel.rs            # 硬件加速检测/初始化
│   └── frame_queue.rs         # 帧队列 + 同步
├── audio/
│   ├── mod.rs
│   ├── output.rs              # cpal 输出
│   ├── spatial.rs             # HRTF 空间音频
│   └── sync.rs                # A/V 同步
├── ui/
│   ├── mod.rs                 # egui UI 主模块
│   ├── controls.rs            # 播放控件
│   ├── settings.rs            # 设置面板
│   ├── playlist.rs            # 播放列表
│   ├── pip.rs                 # 画中画
│   └── hud.rs                 # 信息 HUD
├── subtitle/
│   ├── mod.rs
│   ├── parser.rs              # SRT/ASS/WebVTT 解析
│   └── renderer.rs            # ab_glyph 渲染
├── hotspot/
│   ├── mod.rs
│   └── loader.rs              # 热点配置加载
├── vr/
│   ├── mod.rs                 # OpenXR 集成
│   └── render.rs              # 双目渲染管道
├── playback/
│   ├── mod.rs                 # 播放控制器
│   ├── state.rs               # 状态机
│   └── playlist.rs            # 播放列表管理
└── network/
    ├── mod.rs
    └── streaming.rs           # HLS/DASH 流处理
```

## 依赖清单 (Cargo.toml)

```toml
[dependencies]
# 窗口 & 事件
winit = "0.30"
# 3D 渲染
wgpu = "24"
wgpu-profiler = "0.18"        # 性能分析（开发用）
# 视频解码
ffmpeg-next = "7"              # FFmpeg 绑定
# 音频
cpal = "0.15"
# UI
egui = "0.29"
eframe = "0.29"               # egui + winit/wgpu 集成
# VR
openxr = "0.11"
# 字幕
ab_glyph = "0.2"
# 图片处理（截图）
image = "0.25"
# 序列化（热点配置）
serde = { version = "1", features = ["derive"] }
serde_json = "1"
# 异步（可选，用于网络流）
tokio = { version = "1", features = ["full"], optional = true }
# 日志
tracing = "0.1"
tracing-subscriber = "0.3"
```

## 实现的阶段性划分

### Phase 1：核心播放器 (MVP)
- [x] winit 窗口 + wgpu 初始化
- [x] ffmpeg 视频解码 (CPU)
- [x] equirectangular 球体渲染
- [x] 鼠标视角控制
- [x] 播放/暂停/进度控制
- [x] 基础 egui UI

### Phase 2：体验增强
- [x] 硬件加速解码
- [x] 音频解码 + cpal 输出
- [x] A/V 同步
- [x] 多种投影格式
- [x] 全屏模式 + 快捷键
- [x] 本地文件对话框

### Phase 3：完整功能
- [x] SRT/ASS 字幕
- [x] 播放列表
- [x] 截图
- [x] HLS/DASH 网络流
- [x] 画中画
- [x] 设置持久化

### Phase 4：高级功能
- [x] VR 头显 (OpenXR)
- [x] 空间音频 (HRTF)
- [x] 热点交互系统
- [x] 性能优化 (GPU 零拷贝)

## 错误处理策略

```rust
enum PlayerError {
    // 视频解码
    NoStream,                  // 无视频流
    UnsupportedCodec,          // 不支持的编码
    DecodeError(String),       // 解码失败
    // 文件/网络
    FileNotFound,              // 文件不存在
    NetworkError(String),      // 网络错误
    UnsupportedFormat,         // 不支持的容器
    // 渲染
    RenderError(String),       // wgpu 错误
    DeviceLost,                // GPU 设备丢失
    // VR
    VrRuntimeNotFound,         // 未检测到 VR 运行时
    VrSessionError(String),    // OpenXR 错误
    // UI
    UiError(String),           // egui 错误
}
```

所有用户可见错误通过 UI 提示，可恢复错误（如网络超时）自动重试。

## 测试策略

- **单元测试**: 解码器解析、帧队列、状态机、字幕解析
- **集成测试**: 渲染管道加载、UI 交互路由
- **手动测试**: 360 视频实机播放、不同投影格式、VR 兼容性

## 性能目标

- 4K 360 视频: 60 FPS (硬件解码)
- 8K 360 视频: 30 FPS (硬件解码)
- 内存: < 2GB (4K 流)
- 启动时间: < 3 秒
