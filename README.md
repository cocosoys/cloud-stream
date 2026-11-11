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
| VP8 / VP9 软件编码 | ✅ | vendored `scrap` + libvpx/libyuv；面板可切（#94）。H.264 / AV1 未接 |
| WebRTC 推流（offer/answer/ICE） | ✅ | 单个视频 track + 两条 DataChannel |
| WebSocket 推流（WebCodecs） | ✅ | #106：视频 `0x04`、音频 `0x01`、输入、剪贴板、选屏全接；无 WebCodecs 的浏览器降 JPEG 条纹 `0x03` |
| 键鼠注入 | ✅ | `SendInput` / `SetCursorPos`，含扫描码与修饰键状态 |
| 触屏 / 虚拟摇杆 | ✅ | 触控走 `m`/`m2`，屏幕虚拟摇杆走 `js,a`（#17） |
| 剪贴板 | ✅ | 文本双向 + 图片（二进制）双向 |
| 文件传输 | ✅ | `POST /api/upload`、目录列表、下载 |
| 音频 | ✅ | WASAPI 环回采集下行 + 麦克风上行，音画同步已钉墙钟 |
| QoS 自适应 | ✅ | 链路跟不上时降档，恢复后自行爬回 |
| 周期关键帧 | ✅ | `--keyframe-secs`，按墙钟计时而非帧数 |
| 监听面鉴权 | ✅ | `--auth-token`；五载体（Bearer / cookie / query / WS 子协议 / 片段），`Static` 与 `Status` 豁免 |
| 手柄注入 | ✅ | #17：ViGEmBus 虚拟 Xbox 360 手柄 + vendored `ViGEmClient.dll`，含震动回程 |
| 截图 | ✅ | #28：`GET /api/screenshot` → `image/png`，取当前帧（不等关键帧），1080p 约 93 ms |
| 录屏 | ❌ | 未实现（需要容器封装，见 roadmap §2.10） |

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

# 2. 编译 —— 用脚本，不要直接敲 cargo
toolchain\build.ps1 -Task build -CargoArgs --release
# 首次全量编译内存峰值较高，2 vCPU / 8 GB 机器上再加 -CargoArgs '-j','2'
```

> **为什么要这层壳**：`rustc` 链接时去 PATH 找 `link.exe`，Git Bash 里会命中
> PortableGit 的 GNU coreutils `link`，报 `missing operand after '\377\376'`；
> 裸 PowerShell / cmd 里则根本没有 MSVC，报 `linker not found`。而 `cargo check`
> 不链接，所以它会全绿 —— 最容易让人误判成代码有 bug。脚本自己定位 VS/SDK 与
> 三个原生依赖（`VCPKG_ROOT`、`LIBCLANG_PATH`、`LIBOPUS_LIB_DIR`），拼好环境再
> 转手给 cargo。细节与手工方式见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 运行

日常不用手敲参数：部署脚本会把“被控端需要的全部东西”汇总成
`target\output\cloud-stream`（运行根）。里面的 `start-stream.cmd` 按自身位置
推导所有路径，只传三类与环境相关的东西（前端目录在哪、证书在不在、令牌文件
有没有），**其余参数在 `stream-config.toml`**：

```powershell
target\output\cloud-stream\start-stream.cmd
```

改分辨率、帧率、码率、编码器、输入策略，都是编辑那份 TOML，改完重启计划任务
生效。想知道“现在到底生效了什么”，跑：

```powershell
stream-server.exe --config stream-config.toml --print-config
```

它打出来的是**当前生效值**（不是默认值），带注释，可以直接存回去当模板；
加 `json` 得到同样的内容给程序读。优先级是「默认值 < 配置文件 < 命令行」，
所以脚本里那几条 `--flag` 永远压过文件。键表、空串语义、改错时的行为见
[`docs/configuration.md`](docs/configuration.md)。版本与构建指纹走
`/api/v1/version`（`version` / `commit` / `dirty` / `built_at`），供更新器判断
“要不要升级”。

整包（含前端、证书）还有一个同名 zip，拷到新设备解压即可跑，见
`docs/deploy-one-click.md` §1.1。

**装一台新设备**是双击 `cloud-setup.exe`（就是控制台那个 exe，构建时用
`--features embed` 把整包嵌了进去，约 9 MB）：它自己提权，然后一次做完解包、
放行入站、建看门狗任务、立刻启动并验证（打印 `[ok]` 和本机访问地址），细节见
`docs/deploy-new-host.md` §3.0。要撤掉一台机器就还双击它，点「卸载」——五步
（停任务/杀进程/还录音设备/删规则/删目录）走完，删不掉的文件会点名告诉你。
整包目录里**没有任何 .cmd / .ps1 / .vbs 启动脚本**，见
[`docs/operations-install.md`](docs/operations-install.md)。

发给别人装时用同一批产物里的 `cloud-stream-setup.exe`（约 7.8 MB）：外面套了
一层 Inno 向导，可以自己选装到哪个盘，装完在「应用和功能」里能卸。它是外壳不是
另一套实现 —— 建任务、防火墙、录音设备仍然由 `cloud-setup.exe --finish` /
`--uninstall --keep` 做，两条命令和上面那个双击流程完全一样。

目标机连 ssh 都还没有时（`deploy.ps1` 正是靠 ssh 才够得着它），先拷
`toolchain\enable-ssh.ps1` 与 `enable-ssh.cmd` 过去双击一次：装 sshd、只对构建机
放行 22、写入公钥并**先自测再关密码登录**，见 `docs/deploy-ssh-access.md`。

服务进程自己会做两件事，不需要人守着：`--run` 起来的进程另起一个 `--probe`
子进程探 loopback（最多 30 秒），把结果写成 `logs\watchdog.log` 里的一行
`PREFLIGHT listening on 8080 after N probe(s)` 或 `PREFLIGHT NOT listening ...`
加三条候选原因。所以“启动了但打不开”永远有带时间戳的一行可查。

手工调试时才需要自己拼参数：

```powershell
cd target\output\cloud-stream
..\..\release\stream-server.exe `
  --dist       selkies-web `
  --port       8080 `
  --width      1920 --height 1080 --fps 60 --bitrate 12000 `
  --tls-cert   cert.pem --tls-key key.pem `
  --upload-dir "$env:USERPROFILE\Desktop"
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--host` / `--port` | `0.0.0.0` / `8080` | HTTP(S) 与信令监听地址 |
| `--dist` | `selkies-web`（相对工作目录） | 前端静态目录（`cloud-stream-web` 的构建产物） |
| `--app` | `app` | 信令路径 `/api/<app>/signaling/` |
| `--width` `--height` | 屏幕原生 | 输出分辨率 |
| `--fps` | `60` | 采集/编码帧率上限 |
| `--bitrate` | — | 目标码率（kbps） |
| `--keyframe-secs` | `10` | 周期强制 IDR 的墙钟间隔，1–120 |
| `--repeat-every` | — | 静态画面时重复送帧的间隔 |
| `--tls-cert` `--tls-key` | — | 给了就走 HTTPS/wss，否则明文（除 `127.0.0.1` 外不建议） |
| `--upload-dir` | — | 文件传输允许访问的根目录 |
| `--auth-token` | — | 访问口令。设了之后 `/api/*` 全部要求它（Bearer / cookie / `?token=` / WS subprotocol）；静态资源与 `/api/status` 豁免 |
| `--dump-frame` | — | 把采集帧导成 bmp 便于排查 |
| `--policy` | `allow-all` | 监听面策略，`deny-all` 用于覆盖率探针 |
| `--encoder` | `auto` | 编码后端策略：`auto` / `software` / `nvenc` / `amf` / `qsv`。`auto` 永不失败，只把探测结论写进日志；点名一个不存在的硬编会在启动时直接报错 |
| `--display` | `0` | 默认采集哪块屏（索引 0 = 主屏，其余按枚举顺序）。页面的 `display_id`（`primary` / `display2`）会覆盖它；指定的屏不存在时采集如实失败，不会回落成主屏 |
| `--mode` | `websockets` | 新浏览器（本地还没有选择记录）默认用哪种传输：`websockets` / `webrtc`。页面从 `/api/status` 读它，面板切换的模式选择优先于它 |
| `--video-codec` | `vp9` | 视频编码：`vp9` / `vp8` / `h264`。`h264` 是硬编与 Safari/WebCodecs 兼容的通道，软编下也能跑 |
| `--webrtc-udp-port-min` / `--webrtc-udp-port-max` | `40000` / `40049` | ICE 只会绑这一段 UDP（含两端），让防火墙规则写得出来。改了要让规则跟上：管理员跑一次 `cloud-setup.exe --inbound`；程序规则 `cloud-stream app` 仍覆盖任意端口 |
| `--tune` | `tuned` | 软编实时参数组；`legacy` 复现 #95 之前的参数，仅用于同机 A/B |
| `--log` | `logs\stream-server.log` | 日志落盘路径 |
| `--no-hdr` | — | 关闭 HDR 采集路径 |
| `--restore-mic` | — | 只把上次遗留的默认录音设备还原，然后立刻退出（不绑端口、不开日志）。给卸载流程用；没有遗留记录时打印 `MICRESTORE nothing-to-do` |

启动成功的标志是一行 `listening on https://0.0.0.0:8080 (TLS rustls 1.2/1.3)`。
然后浏览器打开 `https://<host>:8080/`（自签证书需要先信任，或用 `-k` 类工具探测）。

## 部署

一条命令，含上传、切换、体检与回滚：
`docs/deploy-one-click.md`（脚本 `toolchain/deploy.ps1` + `deploy-remote.ps1`）。

## 文档

| 文档 | 内容 |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | **架构：模块边界、依赖规则、五个扩展点、模块化欠账** |
| [`docs/configuration.md`](docs/configuration.md) | **配置：三层优先级、完整键表、`--print-config` 回环、改错时的行为** |
| [`docs/operations-rollback.md`](docs/operations-rollback.md) | **快照与回滚：`--ops snapshot/rollback/verify`，四个动作的失败兜底** |
| [`docs/operations-update.md`](docs/operations-update.md) | **更新：`--update check/apply`，三种验版本口径、自动回滚、更新包放哪** |
| [`docs/operations-run.md`](docs/operations-run.md) | **服务怎么被拉起来：`--run`、GUI 子系统与 stdio 的三种处境、单实例、退出码** |
| [`docs/operations-install.md`](docs/operations-install.md) | **安装：一个 exe 装出整台机器 —— 两段提权、任务 XML 里三个改了就毁的设置、结论文件契约** |
| [`docs/console.md`](docs/console.md) | **可视化控制台 `cloud-setup.exe`：四个动作的按钮版，七个按钮各落到哪条命令** |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | **参与开发：环境、构建、测试、提交规范、硬规矩** |
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
| [`docs/capacity-slicing.md`](docs/capacity-slicing.md) | 容量切分与超卖：一台物理机怎么切成多台可售设备、边际成本与切分上限（待办，未实施） |
| [`docs/ghost-cursor.md`](docs/ghost-cursor.md) | 游戏模式幽灵光标：客户端画指针、主机钉 1:1、5 秒淡出（#96） |
| [`docs/asset-manifest.md`](docs/asset-manifest.md) | 前端 bundle 哈希表、ETag/304 与增量更新（#99） |

## 说明

- `vendor/` 下三个 crate 是 RustDesk / scrap 的**魔改副本**，改动随本仓库一起演进，
  不跟随上游。`vendor/hbb_common/tests/data/*.pem` 是上游测试固件，非私钥。
- 日志默认落在工作目录的 `logs\stream-server.log`，不入库。
