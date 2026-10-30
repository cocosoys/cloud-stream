# cloud-stream

Windows 上的**云游戏 / 远程桌面串流服务端**：DXGI 采集 → 视频编码 → WebRTC 推流，
外加一条 HTTP 静态页面 + WebSocket 信令，浏览器打开就能玩。

前端在另一个仓库：[`cocosoys/cloud-stream-web`](https://github.com/cocosoys/cloud-stream-web)。
两端通过一套固定的信令契约对接（见 `docs/stage1-signaling-session.md`），本仓库不含任何前端代码。

```
浏览器 (cloud-stream-web)                 被控机 (本仓库)
  │  GET /                     ┌──────────────────────────────┐
  ├────────────────────────────►│ http/      静态页 + /api/*    │
  │  WS /api/<app>/signaling/   │ signaling/ WS 会话、控制通道  │
  ├────────────────────────────►│ webrtc/    PeerConnection     │
  │◄─────── VP8/opus RTP ──────┤ pipeline/  DXGI→编码→Sample  │
  │         DataChannel         │ dispatch/  SendInput 等副作用 │
  └────────────────────────────►└──────────────────────────────┘
```

## 能力现状

| 面 | 状态 | 说明 |
|---|---|---|
| DXGI 桌面复制采集 | ✅ | 分辨率/帧率可配，缩放到 `--width/--height` |
| VP8 软件编码 | ✅ | vendored `scrap` + libvpx/libyuv；H.264 / VP9 / AV1 未接协商 |
| WebRTC 推流（offer/answer/ICE） | ✅ | 单个视频 track + 两条 DataChannel |
| 键鼠注入 | ✅ | `SendInput` / `SetCursorPos`，含扫描码与修饰键状态 |
| 触屏 / 虚拟摇杆 | 部分 | 触控事件已通，手柄注入缺驱动（见下） |
| 剪贴板 | ✅ | 文本双向 + 图片（二进制）双向 |
| 文件传输 | ✅ | `POST /api/upload`、目录列表、下载 |
| 音频 | ✅ | WASAPI 环回采集下行 + 麦克风上行，音画同步已钉墙钟 |
| QoS 自适应 | ✅ | 链路跟不上时降档，恢复后自行爬回 |
| 周期关键帧 | ✅ | `--keyframe-secs`，按墙钟计时而非帧数 |
| 监听面鉴权骨架 | ✅ | 22 个入口全部插桩，默认 `allow-all`；令牌解析待做 |
| 手柄注入 | ❌ | Windows 无用户态手柄注入 API，需 ViGEmBus 驱动 |
| 截图 / 录屏 | ❌ | 未实现 |

逐项总账与缺口拆解在 [`docs/roadmap-l2-l3-gaps.md`](docs/roadmap-l2-l3-gaps.md)。

## 目录结构

```
crates/stream-server/     服务端主程序（采集/编码/信令/HTTP/输入/鉴权骨架）
crates/bench-capture/     采集与编码基准测试，用来量 CPU 与码率
crates/shell/             独立小工程（不并入 workspace），冒烟用
vendor/scrap/             改过的屏捕 + 编码器（VP8/VP9/AV1 封装）
vendor/hbb_common/        RustDesk 公共层：配置、日志、webrtc feature
vendor/base/              RustDesk 基础工具
vcpkg-ports/              libvpx / libyuv 的 overlay port（realtime 特性、静态三元组）
toolchain/                构建、部署、诊断脚本（详见 docs/deploy-one-click.md）
docs/                     设计稿与现状盘点
```

## 构建

**仅支持 Windows**（DXGI、WASAPI、`SendInput` 都是 Windows API）。

前置：

- Rust **1.75** + MSVC target（`stable-x86_64-pc-windows-msvc`）
- Visual Studio 2022 Build Tools（C++ 工作负载）
- vcpkg，三元组 `x64-windows-static`，需要 `libyuv` 与 `libvpx[realtime]`
  - `toolchain/install-vcpkg-libs.cmd libyuv`
  - `toolchain/install-vcpkg-libs.cmd "libvpx[realtime]"`
- LLVM（`bindgen` 需要 `LIBCLANG_PATH`）
- Ninja + CMake（vcpkg 构建期用）

```powershell
# 1. 装原生依赖（一次性）
toolchain\install-vcpkg-libs.cmd libyuv
toolchain\install-vcpkg-libs.cmd "libvpx[realtime]"

# 2. 编译
cargo build --release -p stream-server
# 首次全量编译内存峰值较高，2 vCPU / 8 GB 机器上加 -j 2
```

> `toolchain\build-ws.cmd` 与 `install-vcpkg-libs.cmd` 里写死了本机的
> `C:\dev\vcpkg`、`C:\dev\llvm`、VS 安装路径。换机器请先改这几行 `PATH`，
> 或者直接在已 `vcvars64.bat` 过的终端里跑 `cargo`。

## 运行

```powershell
target\release\stream-server.exe `
  --dist      C:\dev\selkies-web `
  --port      8080 `
  --width     1920 --height 1080 --fps 60 --bitrate 12000 `
  --tls-cert  C:\dev\cert.pem --tls-key C:\dev\key.pem `
  --upload-dir C:\Users\Public\Desktop
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--host` / `--port` | `0.0.0.0` / `8080` | HTTP(S) 与信令监听地址 |
| `--dist` | — | 前端静态目录（`cloud-stream-web` 的构建产物） |
| `--app` | `app` | 信令路径 `/api/<app>/signaling/` |
| `--width` `--height` | 屏幕原生 | 输出分辨率 |
| `--fps` | `60` | 采集/编码帧率上限 |
| `--bitrate` | — | 目标码率（kbps） |
| `--keyframe-secs` | `10` | 周期强制 IDR 的墙钟间隔，1–120 |
| `--repeat-every` | — | 静态画面时重复送帧的间隔 |
| `--tls-cert` `--tls-key` | — | 给了就走 HTTPS/wss，否则明文（除 `127.0.0.1` 外不建议） |
| `--upload-dir` | — | 文件传输允许访问的根目录 |
| `--dump-frame` | — | 把采集帧导成 bmp 便于排查 |
| `--policy` | `allow-all` | 监听面策略，`deny-all` 用于覆盖率探针 |
| `--log` | `logs\stream-server.log` | 日志落盘路径 |
| `--no-hdr` | — | 关闭 HDR 采集路径 |

启动成功的标志是一行 `listening on https://0.0.0.0:8080 (TLS rustls 1.2/1.3)`。
然后浏览器打开 `https://<host>:8080/`（自签证书需要先信任，或用 `-k` 类工具探测）。

## 部署

一条命令，含上传、切换、体检与回滚：
`docs/deploy-one-click.md`（脚本 `toolchain/deploy.ps1` + `deploy-remote.ps1`）。

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/roadmap-l2-l3-gaps.md`](docs/roadmap-l2-l3-gaps.md) | 全量盘点：已完成 / 部分 / 未做，逐项拆解 |
| [`docs/stage1-signaling-session.md`](docs/stage1-signaling-session.md) | 信令契约与会话状态机 |
| [`docs/stage1-pipeline-loop.md`](docs/stage1-pipeline-loop.md) | 采集→编码→推流的循环与背压 |
| [`docs/stage1-webrtc-media.md`](docs/stage1-webrtc-media.md) | PeerConnection 装配、track、DataChannel |
| [`docs/stage3-clipboard-qos-avsync.md`](docs/stage3-clipboard-qos-avsync.md) | 剪贴板、QoS 降档、音画同步 |
| [`docs/stage3-audio-design.md`](docs/stage3-audio-design.md) | 音频双向设计 |
| [`docs/listener-surface.md`](docs/listener-surface.md) | 22 个监听面清单与鉴权插桩点 |
| [`docs/deploy-one-click.md`](docs/deploy-one-click.md) | 一键部署说明书 + 踩坑记录 |
| [`docs/deploy-new-host.md`](docs/deploy-new-host.md) | 换设备部署：要几台、每台准备什么、验收与换机坑 |
| [`docs/density-budget.md`](docs/density-budget.md) | 密度预算：一台宿主能跑几个实例、CPU 与内存谁是天花板（待办，未实施） |
| [`docs/ghost-cursor.md`](docs/ghost-cursor.md) | 游戏模式幽灵光标：客户端画指针、主机钉 1:1、5 秒淡出（#96） |

## 说明

- `vendor/` 下三个 crate 是 RustDesk / scrap 的**魔改副本**，改动随本仓库一起演进，
  不跟随上游。`vendor/hbb_common/tests/data/*.pem` 是上游测试固件，非私钥。
- 日志默认落在工作目录的 `logs\stream-server.log`，不入库。
