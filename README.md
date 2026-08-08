# Omniview — 360° 全景视频播放器

基于 Rust 的桌面视频播放器:支持普通视频与 360° 全景视频,进程内解码(ffmpeg-next),GPU 渲染(wgpu + egui)。

## 架构

```
app (winit 事件循环 / 接线)
 ├─ media/  核心播放管线,不依赖窗口与 UI
 │   ├─ playback.rs   PlaybackController:状态机 + 命令入口
 │   ├─ demux.rs      单 demuxer 线程,路由音/视频包
 │   ├─ video.rs      视频解码线程 + 有界帧队列(帧池复用)
 │   ├─ audio.rs      ffmpeg 解码 → swr 重采样 → 环形缓冲 → cpal 输出
 │   └─ clock.rs      MediaClock(音频主时钟,失败回退 wall clock)
 ├─ renderer/ wgpu 渲染(equirect 球面 / quad)
 └─ ui/        egui 控制条
```

单向依赖:`app → media`、`app → renderer/ui`。media 层不知道窗口和 egui 的存在。

## 环境要求(WSL / Linux)

- Rust 工具链:`rustup` 或发行版包(`cargo 1.8x+`,edition 2024)
- 系统 FFmpeg **仅用于测试 fixture 生成**(`sudo apt install ffmpeg`);播放器本身通过 `ffmpeg-next` crate 进程内解码,不依赖外部二进制
- GUI 需要 WSLg(WSL2 默认开启);音频通过 WSLg 的 PulseAudio/PipeWire 输出
- crates 镜像已在 `.cargo/config.toml` 配置(rsproxy.cn),无需手动设置

## 常用命令

```bash
make build                    # debug 构建
make run FILE=video.mp4       # 运行播放器
make test                      # 生成测试 fixture 并跑全部单元测试
make clippy                    # lint(要求 0 警告)
make release                   # 优化构建
```

Windows 交叉编译(在 WSL 内,需 mingw + BtbN FFmpeg,见 `build-win.sh` 注释):

```bash
make win
```

## 测试

单元测试需要三个 `/tmp` fixture(`test_v.mp4`、`test_av.mp4`、`test_av20.mp4`),
`make test` 会自动生成;也可手动执行:

```bash
bash scripts/gen-test-fixtures.sh
cargo test
```

`examples/decode_bench.rs` 是解码性能基准(软解 + swscale):

```bash
cargo run --release --example decode_bench -- file.mp4
```

## 快捷键

| 键 | 功能 |
| --- | --- |
| `O` | 打开文件 |
| `Space` | 播放/暂停 |
| `F` | 全屏 |
| `S` | 截图(PNG) |
| `M` | 静音 |
| `←/→` | 快退/快进 5 秒 |
| `↑/↓` | 音量 |
| `R` | 重置 360° 视角 |
| `Esc` | 退出全屏;再次按下退出程序 |

全屏下鼠标不动 3 秒自动隐藏控制条。拖拽文件到窗口可直接打开。
