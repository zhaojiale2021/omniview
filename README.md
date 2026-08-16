# Omniview — 360° 全景视频播放器

[![CI](https://github.com/zhaojiale2021/omniview/actions/workflows/ci.yml/badge.svg)](https://github.com/zhaojiale2021/omniview/actions/workflows/ci.yml)

基于 Rust 的桌面视频播放器：支持普通视频与 360° 全景视频，进程内解码（ffmpeg-next），GPU 渲染（wgpu + egui）。

## 功能特性

- 普通视频 / 360° 全景视频（equirect 球面渲染，可拖拽视角、缩放、重置）
- 进程内音视频解码，精确暂停/恢复（ffmpeg-next，不依赖外部 ffmpeg.exe）
- 播放/暂停、倍速（0.25×–4×，切换延迟 < 500ms）、音量/静音
- 进度条单击跳转、拖拽 seek，悬浮显示时间气泡 + 缩略图预览
- 音频输出采样率/通道数自适应（优先设备默认配置，自动回退 48kHz/44.1kHz 立体声）
- 视频分辨率自适应：2D 播放按源宽高比 letterbox/pillarbox，不拉伸变形
- 播放列表：打开多个文件或整个文件夹，顺序/列表循环/单曲循环/随机连播，`[` / `]` 或界面按钮切换上/下一集
- 音轨/视频轨切换（多音轨/多视频轨文件可在顶栏选择）
- 外挂 `.srt` / `.ass` 字幕（与视频同目录同名）自动加载并显示
- 夜间模式：柔和限制大声压片段（`N` 键或底栏按钮）
- 截图（S 键）、拖拽打开文件、全屏自动隐藏控制条
- 记忆续播 + 音量/倍速/360° 模式/窗口位置大小持久化
- 长按左右方向键连续快退/快进
- 中文界面字体回退（自动加载系统中文字体）

## 架构

```
app (winit 事件循环 / 接线)
 ├─ media/  核心播放管线，不依赖窗口与 UI
 │   ├─ playback.rs   PlaybackController：状态机 + 命令入口
 │   ├─ demux.rs      单 demuxer 线程，路由视频包
 │   ├─ video.rs      视频解码线程 + 有界帧队列（帧池复用）
 │   ├─ audio.rs      ffmpeg 解码 → swr 重采样 → 环形缓冲 → cpal 输出
 │   ├─ clock.rs      MediaClock（音频主时钟，失败回退 wall clock）
 │   └─ thumb.rs      后台缩略图解码，供进度条悬浮预览
 ├─ renderer/ wgpu 渲染（equirect 球面 / 2D aspect-fit quad）
 └─ ui/        egui 控制条
```

单向依赖：`app → media`、`app → renderer/ui`。media 层不知道窗口和 egui 的存在。

## 环境要求（WSL / Linux）

- Rust 工具链：`rustup` 或发行版包（`cargo 1.8x+`，edition 2024）
- 系统 FFmpeg **仅用于测试 fixture 生成**（`sudo apt install ffmpeg`）；播放器本身通过 `ffmpeg-next` crate 进程内解码，不依赖外部二进制
- GUI 需要 WSLg（WSL2 默认开启）；音频通过 WSLg 的 PulseAudio/PipeWire 输出
- crates 镜像已在 `.cargo/config.toml` 配置（rsproxy.cn），无需手动设置

> 已知问题：WSLg 下全屏可能由 Windows 侧合成器留下旧窗口残影，属 WSLg 渲染限制；Windows 原生构建无此问题，正式使用请优先 Windows 版本。

## 常用命令

```bash
make build                    # debug 构建
make run FILE=video.mp4       # 运行播放器
make test                     # 生成测试 fixture 并跑全部单元测试
make clippy                   # lint（要求 0 警告）
make release                  # 优化构建
```

Windows 交叉编译（在 WSL 内，需 mingw + BtbN FFmpeg，见 `build-win.sh` 注释）：

```bash
make win
```

## CI / Release

- GitHub Actions 会在 push / PR 时自动执行 `cargo check`、`cargo clippy`、`cargo test` 和 `cargo fmt --check`。
- 推送 `v*` 标签（如 `v0.1.0`）或手动触发 Release 工作流时，会构建并上传：
  - `omniview-linux-x86_64.tar.gz`
  - `omniview-windows-x86_64.zip`（含运行所需 FFmpeg DLL）

## 测试

单元测试需要三个 `/tmp` fixture（`test_v.mp4`、`test_av.mp4`、`test_av20.mp4`），
`make test` 会自动生成；也可手动执行：

```bash
bash scripts/gen-test-fixtures.sh
cargo test
```

`examples/decode_bench.rs` 是解码性能基准（软解 + swscale）：

```bash
cargo run --release --example decode_bench -- file.mp4
```

## 快捷键

| 键 | 功能 |
| --- | --- |
| `O` | 打开文件 |
| `Space` | 播放/暂停 |
| `F` | 全屏 |
| `S` | 截图（PNG） |
| `M` | 静音 |
| `←/→` | 快退/快进 5 秒（长按连续 seek） |
| `[` / `]` | 上一个 / 下一个视频（播放列表） |
| `L` | 循环播放列表模式（顺序/列表循环/单曲循环/随机） |
| `N` | 夜间模式开关 |
| `↑/↓` | 音量 |
| `R` | 重置 360° 视角 |
| `Esc` | 退出全屏；再次按下退出程序 |

全屏下鼠标不动 3 秒自动隐藏控制条。拖拽文件到窗口可直接打开。
