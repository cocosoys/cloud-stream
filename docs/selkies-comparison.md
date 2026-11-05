# 与上游 Selkies（Linux 桌面）的对照

**一句话**：前端是同一份代码的两个分支，后端是**两个不同的问题**——上游在
Linux 上「为每个用户造一个桌面」，本项目在 Windows 上「接管一台已经开着的机器」。
绝大多数能力差异不是完成度差异，是这句话的直接后果。

上游指 [selkies-project/selkies](https://github.com/selkies-project/selkies) 2.0
（GStreamer 已移出运行时，采集编码交给 `pixelflux`、音频交给 `pcmflux` 两个 Rust
扩展，Python 只做控制面）。本文所有本项目结论以代码为准，不引用 README 的自述。

---

## 1. 最本质的差异：桌面从哪来

| | 本项目 | 上游 Selkies |
|---|---|---|
| 桌面实体 | 被控机上**已存在**的交互式桌面（登录用户的 Session 1） | 会话**自带**的显示：X11 走 Xvfb + RandR，Wayland 走 headless 合成器 |
| 一机多用户 | 一块屏 N 个观众（协作/带看） | 一个用户一个桌面（各自独立，容器隔离） |
| 第二块屏 | 必须是**真实**显示器（DXGI 输出） | 可以**凭空插一块**：RandR output + 独立 CRTC，随页面来去 |
| 服务身份 | 必须跑在登录用户会话里（Session 0 看不到桌面） | root/systemd 用户服务，与图形会话解耦 |
| 隔离边界 | 进程边界（同一个桌面，谁都能看见谁） | 容器/命名空间边界 |

这一条决定了后面所有「能不能」：

- 上游能给每个观众发**独立桌面**，本项目只能给所有人发**同一块屏**。
- 上游的「第二屏」是软件定义的，能随浏览器页签插拔；本项目的第二屏取决于用户
  是否真的插了第二台显示器（`--display` / `display_id` 只是选哪一块）。
- 上游可以在一台机器上开几十个会话（每个 CPU 几十 MB + 一份软编）；本项目的
  会话数受限于一块 GPU 的 `DuplicateOutput` 与一份编码器，靠 fanout 复用。

## 2. 逐层对照

### 2.1 采集与编码

| 面 | 本项目 | 上游 |
|---|---|---|
| 采集 API | DXGI Desktop Duplication（独占，失败退 GDI） | `pixelflux`：X11 走 NvFBC / DRI3，Wayland 直接读合成器 GPU buffer |
| 零拷贝 | ❌ staging 拷回 CPU 再转 YUV | ✅ X11/Wayland 都在 GPU 上完成 |
| 编码器 | **libvpx VP8 / VP9 两个**（软编，CBR 或 CQ） | h264 / h265 / vp8 / vp9 / av1，硬编 NVENC / VA-API，软编 x264/OpenH264/x265/kvazaar/libvpx/SVT-AV1 |
| 硬编 | ⚠️ 只**探测**，`Backend::Hardware` 是保留值，二进制里没有实现 | ✅ 按 GPU 引擎选，探测失败逐级回落 |
| 兜底路径 | 无（浏览器必须支持 VP8/VP9） | CPU 条带 H.264、JPEG 条纹（无 WebCodecs 的浏览器也能看） |
| 色度 | 4:2:0 | 4:4:4 随编码器，服务端声明 |

**这是 CPU 成本的结构差**：本项目每帧要走 DXGI staging → BGRA→YUV → libvpx，
60 fps / 1080p 大约吃满一个核；上游在 GPU 上闭环，CPU 只做打包。这直接决定了
「一台机器能带几个会话」的天花板（见 `docs/density-budget.md`）。

### 2.2 传输

| 面 | 本项目 | 上游 |
|---|---|---|
| 默认 | **WebRTC**（`webrtc-rs`），UDP，需放行端口 | **WebSocket**，单 TCP 8080 口 |
| 另一条 | WebSocket `/api/websockets`（#106 起补齐视频/音频/输入/剪贴板） | WebRTC opt-in（`--mode=webrtc`，vendored aiortc fork） |
| 运行中切换 | ❌ 需要重连 | ✅ `--enable-dual-mode` |
| 丢包恢复 | RTP/RTCP：NACK、PLI → 强制关键帧、RTT 采样 | WS 路径靠应用层丢帧 + 关键帧请求；WebRTC 路径靠 SRTP/RTCP |
| 穿透 | 需要 UDP 直连或 TURN | 单 TCP 端口，不需要 STUN/TURN |

**方向是相反的，且都有理由**：上游面向容器/K8s/HPC，一个 TCP 口最好交付；
本项目面向游戏/桌面，WebRTC 的 NACK+FEC 在丢包时的观感更好，而部署是「自建、
能控端口」。代价是本项目在受限网络里需要 TURN，上游不需要。

### 2.3 音频（差距最大的一层）

| 面 | 本项目 | 上游 |
|---|---|---|
| 下行 | WASAPI **loopback**（渲染端点回环）→ Opus | PulseAudio / PipeWire monitor → Opus（`pcmflux`） |
| 冗余 | ❌ `n_red = 0`，无 RFC 2198 | ✅ 可开 RED |
| 上行 | WebRTC track → Opus decode → **播到默认输出端点** | 发布成一条普通 **PulseAudio source**，应用可以选它当麦克风 |
| 回声 | 结构性问题：播到哪儿就被 loopback 采回哪儿 | 由虚拟设备隔离，source 与 sink 不重叠 |
| WS 侧上行 | ❌ 无接收端，收到即回 `MICROPHONE_DISABLED` | ✅ 与 WebRTC 同一套 |

`pipeline/audio.rs` 的模块注释自己写清了这条限制：注入的麦克风落在默认渲染端点，
就是下行 loopback 采集的同一个端点，所以开扬声器会听到自己延迟一次；「进程内无法
用电学方式断开」。**Linux 上这个问题不存在**，因为 PipeWire 有虚拟 source 这个概念，
而 WASAPI 没有——要补得装虚拟声卡驱动（VB-Cable 一类），不是改代码能解决的。

> 因此 README 里「麦克风上行 ✅」要打折看：语义是「浏览器声音能被主机听见」，
> 不是「游戏能把浏览器麦克风选作输入设备」。后者在 Windows 上需要驱动。

### 2.4 输入

| 面 | 本项目 | 上游 |
|---|---|---|
| 注入层 | `SendInput` / `SetCursorPos`（用户态消息层） | `uinput` / evdev / interposer（**内核设备层**） |
| 应用视角 | 前台窗口收消息；RawInput/DirectInput 独占的游戏经常收不到 | 看到的是真实输入设备，与物理键鼠无差别 |
| 手柄 | ViGEmBus 驱动（虚拟 Xbox 360）+ 震动回程 | joystick interposer / uinput，按 slot 绑定 |
| 指针锁定 | 服务端合成光标（ghost cursor）+ 客户端 1:1 钉住 | 原生 pointer lock + gaming mode |
| 多用户授权 | controller 租约 + `--input-policy shared\|exclusive`（服务端拒绝） | viewer / player-2-4 link，服务端强制 role + slot |

Windows 没有「用户态造一个输入设备」的接口——这就是本项目为什么要写 ghost cursor、
扫描码映射、`TargetSpace` 虚拟桌面归一化这一整套补偿逻辑，而上游只要往
`/dev/uinput` 写事件。**这一层的复杂度差不是设计品味，是平台税。**

### 2.5 其余面

| 面 | 本项目 | 上游 |
|---|---|---|
| 剪贴板 | 文本 + 图片，双向；两套线上家族（`cb,` / `clipboard,`） | 文本 + 图片 + **富文本**，统一信封 |
| 文件传输 | HTTP `POST /api/upload` + 目录浏览 + 下载（不在媒体通道里） | 双向，且按视频流 pacing，避免大文件卡住画面 |
| 多屏 | 物理输出；`hello.display_id` / `SETTINGS.displayId` 选屏 | RandR 虚拟输出，可插拔 |
| 观测 | `/api/v1/health`（clients / pipelines / admission）、`stream_stats`、QoS 自动降档 | 统计面板，由**采集**上报 backend / GPU / encoder / 是否拷贝 |
| 应用面板 | ❌ | ✅ 启动会话内的 desktop entries |
| 鉴权 | `--auth-token` 五载体 + 监听面策略 + 401 的 `WWW-Authenticate` | basic-auth、secure mode（session token 带 role/slot）、TLS 热重载 |
| 打包 | Windows exe + 一键部署脚本 + 看门狗 + 静态前端 | wheel / deb / rpm / apk / Arch / AppImage / 多架构容器 |

## 3. 本项目**超出**上游的部分

不是所有差异都是欠账，这几项是上游没有的：

- [x] **单机一键部署**：`deploy.ps1` 上传-切换-体检-回滚，`setup-watchdog.bat` 一次做完
      放行入站、建任务、自检。上游的等价物是「写 Dockerfile / 起容器」。
- [x] **进程内多观众 fanout**：一块屏一次采集、一次编码，N 个 sink
      （`VideoFanout` / `AudioFanout`），观众数不线性放大 CPU。上游是每会话一套。
- [x] **健康与准入可观测**：`/api/v1/health` 里有 `clients{role, denied_inputs}`、
      `pipelines{display, downlinks, holders}`、`admission{live, max}`，以及
      `session/kick`、`session/take` 两个控制动作。
- [x] **游戏向的补偿逻辑**：ghost cursor 5 秒淡出、指针灵敏度、长按重复键、
      静态画面重复编码保帧率（`REPEAT_ENCODE_MAX`）。
- [x] **前端资产哈希 + ETag**：`/api/v1/manifest`，增量更新（`docs/asset-manifest.md`）。

## 4. 差距分类：哪些能补，哪些补不了

### 4.1 平台决定的（改代码解决不了）

- [ ] **虚拟输入设备**：键鼠只能走 `SendInput`。要等价于 uinput，得写驱动
      （手柄已经用 ViGEmBus 绕过去了，键鼠没有等价的成熟驱动）。
- [ ] **虚拟音频设备**：WASAPI 没有「造一个 source」的抽象。麦克风上行要么播到
      输出端点（现在这样，有回声），要么随包一个虚拟声卡驱动。
- [ ] **虚拟显示器**：要凭空造第二块屏，得写 IddCx 间接显示驱动。
- [ ] **Session 0 隔离**：服务必须是登录用户的进程，不能做成 Windows 服务直接采集。

### 4.2 实现选择（可以补，按性价比排序）

- [ ] **H.264 编码**：浏览器兼容性最好、硬编生态最全。当前 VP8/VP9 之外的编码器
      全部缺席，是「有些设备就是不出画面」的头号原因。
- [ ] **硬编接线**：`platform/hwenc.rs` 的探测、驱动文件校验、`--encoder` 取舍
      全都写好了，缺的是 `hwcodec` feature 下的实际编码路径。接上后 CPU 天花板
      抬高一档。
- [ ] **WS 的 JPEG 条纹路径**（`0x03`）：前端能解，服务端不发。没有 WebCodecs 的
      浏览器在 WS 模式下目前是黑屏。
- [ ] **音频 RED**：前端 `extractOpusFrames` 已经按 RED 计数解析，服务端固定
      `n_red = 0`。弱网下音频断续的直接原因。
- [ ] **富文本剪贴板**：现在只有纯文本 + 图片。
- [ ] **流内文件传输**：现在走 HTTP，与视频不共享拥塞信号。
- [ ] **多用户 slot**：现在只有 controller / viewer 两档，没有 player-2..4 的
      独立手柄绑定。

## 5. 顺手发现：README 与代码已经不同步

盘点时对出来的两处，与本轮改动无关但会误导人：

| README 行 | 代码实际 |
|---|---|
| 「VP8 软件编码 ✅ … H.264 / VP9 / AV1 未接协商」 | `pipeline/codec.rs` 的 `ALL` 是 `[Vp8, Vp9]`，面板里是两个真实可选项（#94） |
| 「WebSocket 推流 🟡 … 输入/音频/剪贴板尚未接」 | `http/ws_media.rs` 已接输入、音频（`0x01`+Opus）、剪贴板（`clipboard,` 家族），见 `docs/websocket-transport.md` |

## 6. 结论

- 前端层面，本项目是上游的**收敛分支**：同一套 `selkies-web-core`，砍掉了 Linux
  专属模块（RandR 刷新、interposer 面板），补了 Windows 需要的（ghost cursor、
  资产清单、鉴权插桩）。
- 后端层面，两者**不是同一类系统**：上游是「多租户虚拟桌面平台」，本项目是
  「一台真实 Windows 机器的远程出口」。用上游的能力表逐项打勾会得出「本项目
  差很多」的结论，但其中一半的格子（虚拟显示、虚拟输入、虚拟音频、多桌面隔离）
  在 Windows 上要付驱动的代价，不是排期的代价。
- 真正该补且能补的，是 §4.2 那张清单的前三项：**H.264、硬编、WS 的 JPEG 兜底**。
  它们不改变架构，只抬兼容性与 CPU 天花板。
