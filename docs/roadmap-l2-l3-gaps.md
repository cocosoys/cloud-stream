# L2 / L3 缺口清单与任务分解

> **代码基线**：#120（后端 `ad12ea8` + 本轮未提交改动，前端 `3dac1f2`），
> `cargo test -p stream-server` **331 通过 / 0 失败 / 1 ignored**（2026-11-09 复跑）。
> #120 不是缺口，是一处口径修正：`health.stream.target_kbps` 原来读
> `encoder.bitrate()`，与 QoS 决策值是两个量，于是出现"`throttled: true` + target
> 4527"而实际跑在 1581。现在拆成 `target_kbps`（决策）与 `encoder_kbps`（自报），
> 见 `stage3-clipboard-qos-avsync.md` §2.4 末段。
> 部署机 192.168.59.148：`/api/v1/health` 实测 `current_mode: websockets`、
> `codec: vp9`、`viewer.credential: none`。
>
> **本文档的定位**：`roadmap-future.md` 末尾写「L2/L3 的缺口清单另见
> `roadmap-l2-l3-gaps.md`」，此前一直没写。现在补上，两者严格不重叠——
> 那边收「已分析清楚但暂缓」，这边收「范围内该做、还没做」。
>
> **全量 35 项：35 完成 / 0 部分 / 0 未做**（2026-11-09 再算一次）。最后一个
> ❌（#28 截图/录屏）本轮做掉了截图那一半，录屏拆出去另记（§2.10）。
> 此前的「30 完成 / 3 部分 / 1 未做」是 2026-10-29 的旧数，此后 #17 手柄、
> #19 触控、#33 鉴权、#28 截图相继闭合，但顶部总数一直没跟着改 ——
> 这次约定：**改小节标题必须同步本节，再同步顶部**，且用脚本按表格第三列复算。
>
> 本轮订正的关键一处：**#33 鉴权不是「仍无 token」**。`--auth-token` /
> `--auth-grant tenant:role:token`、五载体提取、常量时间比较、`Grant{tenant,role}`
> 与五处裁决点全都在（`config.rs:173/179`、`security/token.rs`、
> `http/server.rs:254`），缺的只是**线上没开开关**（`C:\dev\stream\auth-token.txt`
> 不存在）。这是「能力已闭环、默认关闭」，不是「没有能力」。
>
> **「完成」不等于「已验收」**：`#21 麦克风上行`、`#24 音量/静音/码率`、
> `#23 统计浮层`、`#79 前端按钮` 四项代码已部署，但服务端日志里
> `findstr "mic uplink|render open|uplink playing"` 命中数为 **0** —— 也就是
> 还没有一次真实开麦记录。判定为 ✅ 依据是链路闭环 + 单测，**实机验收仍待用户操作**。
> `#17 手柄`的浏览器侧同样待人工验收。**带 \* 的状态即"代码闭环、实机待验"。**

---

## 0. 判定口径

| 状态 | 含义 | 证据要求 |
| --- | --- | --- |
| ✅ | 两端闭环，实机跑通过 | 能指向具体提交号或日志实证 |
| 🟡 | 主链路通，但覆盖不完整 | 必须写清「覆盖到哪、从哪断」 |
| ❌ | 只有一端，或两端都没有 | 必须写清断点所在文件行 |

一条 ❌ 要进本文档，需要同时满足：**协议另一端已存在**（否则是设计题不是工程题）、
**改动面能落在现有模块内**（不需要推翻架构）。不满足的进 `roadmap-future.md`。

---

## 1. 全量盘点

### 1.1 L1 传输层（出画面）— 8/8 完成

| # | 能力 | 状态 | 证据 |
| --- | --- | --- | --- |
| 1 | WS 信令握手（hello / session / sdp / ice）+ `_gz` 压缩协商 | ✅ | `signaling/line.rs`、`signaling/websession.rs` |
| 2 | 服务端 offerer + ICE 双向 + 会话登记（同页签重连驱逐，跨页签共存） | ✅ | `websession.rs`（`SessionRegistry`） |
| 3 | DXGI 桌面复制（`no_hdr` 跳过探测、桌面重建恢复） | ✅ | `pipeline/capture.rs`、`stage1-capture-recovery.md` |
| 4 | VP8 编码 → 分片 → RTP 写轨（有界队列 + 溢出丢帧求关键帧） | ✅ | `pipeline/mod.rs` `forward_chunks` |
| 5 | 动态 fps / 分辨率 / 码率（`_arg_fps` / `r,WxH` / `vb,N`） | ✅ | `dispatch/commands.rs`、`pipeline/settings.rs` |
| 6 | 静态画面 repeat 帧 + PLI/FIR 去抖关键帧（500 ms 下限） | ✅ | `pipeline/mod.rs:107-126` |
| 7 | 静态托管 + TLS + 文件传输（列目录 / 下载 / 分片上传） | ✅ | `http/server.rs`、`http/file_transfer.rs` |
| 8 | PC 状态机与断线恢复 | ✅ | `stage1-signaling-session.md` |

### 1.2 L2 输入面 — 11 完成 / 0 部分 / 0 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 9 | 键盘 keysym→VK（保布局语义、死键、被控端 IME） | ✅ | `win_input.rs` `keysym_to_vk` / `inject_key` |
| 10 | Unicode 文本注入（中文 / 日文 / emoji / BMP 外代理对） | ✅ | #76 `749c8d8`，`keysym_to_unicode` |
| 11 | 绝对指针 `m`（seq 去重、frame→screen 映射） | ✅ | #72 `bd5cf0e` |
| 12 | 相对指针 `m2`（1:1 直通，不再按 screen/frame 缩放） | ✅ | #74 `6999a3c` |
| 13 | 按键 mask 同步 / 滚轮脉冲 / `kr` 全释放 | ✅ | `sync_buttons` / `sync_scroll`。#101 修：`SCROLL_BITS` 曾写成 `0b1100_1000`（漏 bit 4），滚轮**向上**永远没有上升沿 → 上滚无效、下滚正常；现为 `0b1101_1000`，方向表与掩码由 `scroll_edges_cover_both_vertical_directions` 逐位钉住 |
| 14 | 游戏模式光标合成（锁定指针时把 OS 光标画进帧） | ✅ | #70 `976edd6` |
| 15 | 指针渲染权交接（`SET_NATIVE_CURSOR_RENDERING`） | ✅ | #57 |
| 16 | 剪贴板文本双向（`REQUEST_CLIPBOARD` / `cw-cwe`） | ✅ | #55 `dispatch/clipboard.rs` |
| 17 | 手柄（`js,c/d/b/a/h` + `rumble` 下行） | ✅ | #17 `dispatch/gamepad.rs`：ViGEmBus + vendored `ViGEmClient.dll`（动态加载、独占线程、状态量合并、震动去重回程 `system→rumble,<slot>,<strong>,<weak>,<ms>`）。`cargo test ... hardware_roundtrip --ignored` 在被控机上读到 `XInput index 0: buttons 4096 / lt 64 / rt 128 / lx 32767 / ly 32767` |
| 18 | 二进制剪贴板（`cb/cbs/cbd/cbe`） | ✅ | #85，`dispatch/clipboard_image.rs`（PNG/BMP → DIB）+ `clipboard.rs` 上下行；顺带修正下行消息族（旧 `clipboardContentUpdate` 本页根本不收） |
| 19 | 触控 | ✅ | 单指拖动走 `m`/`m2` 已通；**屏幕虚拟摇杆**走 `js,a`（`gamepad.js:125` 监听 `touchgamepadinput`），随 #17 一并可用 |

### 1.3 L3 画质与体验 — 10 完成 / 0 部分 / 0 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 20 | 游戏声音下行（WASAPI loopback → Opus 48k/2ch/64kbps） | ✅ | #77 `6a095e2`，`pipeline/audio.rs` |
| 21 | 麦克风上行（`on_track` → Opus 解码 → WASAPI render） | ✅* | #79，`webrtc/mod.rs` `on_track` + `pipeline/audio.rs` `spawn_uplink` 双线程；*待实机验收 |
| 22 | 服务端可观测性（RTT / video_mbps / send_ms / peak / 关键帧请求） | ✅ | #78 `88e80dd`，`docs/stage3-observability.md` |
| 23 | 统计浮层 HUD（九行，跟随「在画面上显示」开关，无热键） | ✅ | 前端 `lib/stats-hud.js`，`afcf2ad`；开关同步与撤掉 F3/F4 见 `docs/host-knobs.md` §7 |
| 24 | 音量 / 静音 / 音频码率 | ✅* | #80，`Action::SetAudioOn` / `SetAudioBitrate`，静音期间发静音帧（~1.6 kbps 而非停发）；*待实机验收 |
| 25 | QoS 自适应码率（丢包 / 积压反压） | ✅ | #84，`pipeline/qos.rs`（积压 + PLI 率 → 2s 一次决策，-25%/+10%，floor 20%）；`throttled` 现为真实状态 |
| 26 | 音画同步 | ✅ | #83，`pipeline/avsync.rs`：音频媒体时钟钉在墙钟上，偏斜 >80 ms 时以 20 ms 为一格收发帧。首版比两路介质时钟，实测误判成 -16 s（见设计稿 §3.1） |
| 27 | 周期关键帧 / 长参考链守护 | ✅ | **此前误判为未做**；`#86` 又把它从「帧数」改成「秒数」。现在 `--keyframe-secs`（默认 10 s）由采集循环按墙钟执行 `force_keyframe`，`kf_max_dist` 降级为 2 倍周期的兜底网（`encoder.rs:keyframe_backstop_frames`） |
| 28 | 截图 | ✅ | #28：`GET /api/screenshot` → `image/png`，实测 1920×1080、1.32 MB、请求 **93 ms**（其中编码 42–51 ms），见 §2.10。数据源是管线每拍留的 `pristine_bgra`（不含合成指针），编码走 `pipeline::png`（与指针图共用同一份 CRC/chunk 写法）。**录屏仍不做**，理由见 §2.10 末尾 |
| 92 | 视频设置五控件：码率 / 码控(CBR↔CRF) / CRF 真可改；编码器改事实行；覆盖层三项按无 paint-over 隐藏 | ✅ | `docs/video-knobs.md`；`VpxRateControl`（vendor/scrap）、`pipeline::settings::RateControl`、`_rc,`/`_crf,` 已不再是 B 组噪音 |

> #27 的残余问题不是"没有周期关键帧"，而是**间隔不可配且偏短**：60 帧 ≈ 30 fps 下
> 2 秒，1080p 一个 IDR 约 100–200 KB，等于常驻 0.4–0.8 Mbps 花在刷新参考链上。
> 若要做的是"长静置守护"，正确形态是 `--keyframe-interval-sec`（默认 10 s）+
> 保留 PLI 立即响应，即把 60 改大、把秒级守护交回策略层。属调优，不是缺口。
>
> **已落地（#86）**：旗标叫 `--keyframe-secs`（默认 10，clamp 1–120）。实现上不是
> 简单把 60 改大——`kf_max_dist` 的计量单位是**编码帧**，而本管线"变化才发"，
> 同一数值在动的画面是 1 s、在静的桌面是几分钟，用帧数根本表达不了秒。改为采集
> 循环按墙钟计时、到点 `force_keyframe()`，`kf_max_dist` 只留 `fps × secs × 2`
> 的兜底。实测日志：`periodic keyframe: 10.0s since the last IDR (period 10s)`，
> 同一窗口 `kf=1`（`key_frames` 计数取自 VP8 包的 `key` 标志，证明 IDR 真发出去了）。

### 1.4 L4 产品闭环 — 6 完成 / 0 部分 / 0 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 29 | 自动拉起游戏 + 全屏 + 自动进服 | ✅ | `C:\dev\mc\launch-remote.ps1`（`-Server/-Port/-Fullscreen`），schtasks `MCGame` /IT 进交互会话 |
| 30 | 默认灵敏度三层固化（游戏 0.5 / 关加速 / gain 1.0） | ✅ | `mouse-sensitivity-default.md`、`start-mc.ps1` |
| 31 | 会话互斥（同页签重连驱逐旧 socket，跨页签共存） | ✅ | `websession.rs` `SessionRegistry::publish`：按 `client_tab_id` 比对，只踢同一页签的僵尸连接。多页面共享一条采集管线，首个为 `controller`，其余 `viewer` 只读。见 `pipeline/shared.rs` 与 `roadmap-multi-display-multi-input.md §5.1` |
| 32 | 服务自恢复 | ✅ | #81 `0159076`：`MCStreamSrv` 改 `/SC MINUTE /MO 1` + `run-hidden.vbs` 隐藏宿主 + 单实例检查。实测 `taskkill` PID 1904 → 一分钟后 PID 7612 自起，当前跑的正是它 |
| 33 | 鉴权 | ✅* | **已闭环（#88 骨架 → #101 口令 → #110 身份）**：22 个监听面全部插桩、五处裁决点都在副作用之前（HTTP / 信令 / 控制通道 / 指针通道 / 麦克风）；`--auth-token <pw>`（等价 `default:host`）+ 可重复的 `--auth-grant tenant:role:token`（`host` / `operator` / `viewer`）；五种载体 Bearer / `?token=` / cookie / WS 子协议 / URL 片段，常量时间比较；`Static` 与 `Status` 豁免。裁决在 `http/server.rs:254`：**先口令（401 + `WWW-Authenticate`）、再策略（403）**，信令与媒体复用同一个 `grant`。**\*线上未开**：`C:\dev\stream\auth-token.txt` 不存在 → 启动日志 `cred=none`，局域网内任何人打开 8080 仍能直接操作。开只需在被控机跑一次 `set-password.cmd` 再重启任务。见 `listener-surface.md` |
| 34 | 一键部署 | ✅ | #34 `toolchain/deploy.ps1` + `deploy-remote.ps1`：构建 → 上传 → 两端 MD5 → 停任务/禁用 → 备份 → 换文件 → 起任务 → 三重体检（HTTP 200 + 进程启动时间 + 日志 `listening on`）→ 失败自动回滚。实测两次全量部署各 48 s、一次 `-Rollback` 通过；`-Bootstrap` 顺带把 sshd 钉成自动启动。见 `deploy-one-click.md` |

---

## 2. 缺口逐项拆解

每项按「现象 → 根因 → 改动点 → 验收 → 风险」写。改动点给到文件级，
不预设实现细节——实现细节属于各自的设计稿。

### 2.1 麦克风上行（#21，P0，L3）— 已实现 #79，**待实机验收**

**现象**：前端开麦后浏览器显示已发送，被控机没有任何声音，也不报错。

**根因**：`MediaSession` 只做了「加 recvonly transceiver」这一个动作
（`webrtc/mod.rs:147-161`），没有 `on_track` 回调去接
`RTCRtpReceiver::read_rtp`。SDP 协商是成功的（前端 `_prepareUplinkTransceivers`
把那条 m-line 改成 sendonly 并 `replaceTrack`），所以**链路上行是通的，只是没人读**。

**落地形态**（与本文档原计划的差异，以代码为准）：
- `on_track` 的触发点是**第一个 RTP 包**，不是协商完成 —— 所以上行线程从回调里起，
  不能放在 PC Connected 分支（那个时刻麦克风可能还没被按下）。
- 上行是**两个 OS 线程**：`audio-mic-rx` 永久 `block_on(read_rtp)` 把 `(seq,payload)`
  塞进容量 64 的 `sync_channel`；`audio-mic-tx` 每 5 ms 取包解码写 WASAPI render。
  单线程做不到「既不阻塞读、又不丢包」——给 `read_rtp` 加超时会丢掉已经出队的包。
- 播放侧：预滚 60 ms 开播、队列超 500 ms 裁回 60 ms 且保留最新、欠载补零、
  丢包用 Opus PLC 补最多 3 帧。
- 已知物理限制：注入落在默认 render 端点 = loopback 采集的同一端点，所以开麦会听到
  自己一次。三条出路见 `stage3-audio-design.md` §8.2 与 `roadmap-future.md` §5。

**验收**（尚未发生）：`audio: render open` / `mic uplink: remote track` /
`audio: uplink playing` 三行日志目前命中数为 0，说明还没人真正开过麦。

### 2.2 手柄（#17，P1，L2）— 已实现，#17

**现象**：插手柄、按任意键，游戏无反应。前端 `navigator.getGamepads()` 能读到设备。

**根因**：两层缺失。①服务端 `win_input.rs:737` 把 `js` 归到 informational；
②即便解析了，Windows **没有用户态注入手柄的 API**——`SendInput` 只管键盘鼠标，
游戏读的是 XInput / Raw Input 设备栈。业界做法是 ViGEmBus（签名内核驱动，
虚拟出 Xbox 360 手柄），Moonlight/Sunshine 走的正是这条路。

**落地形态**：
- `dispatch/gamepad.rs`（新增）：`GamepadHub`。DLL **动态加载**
  （`LoadLibraryW` + `GetProcAddress`，不引新 crate，只加 `Win32_System_LibraryLoader`
  feature），失败即降级；ViGEm 句柄只在一条独占线程上用；输入路径只写
  `Mutex<[Pad;4]>` + `Condvar`，**持锁不进 DLL**（回调可能同步发生在 `update` 内）；
  回调去重（`AtomicU16` 打包 large/small）+ 限流 50 ms，停止边沿永不丢。
- `dispatch/win_input.rs:737`：`js` 从 informational 分支取出，
  `Some(hub) => hub.handle_js(rest)`，`None => debug 一行`（无驱动的旧行为）。
- `signaling/websession.rs`：会话持有 hub，teardown 调 `release_all()`
  （关页签不发 `js,d`，虚拟手柄是 OS 设备，不清会留下占着 XInput 0 号位的幽灵手柄）；
  `arm_connected_notices` 里装一条震动泵 → `system("rumble,…")`，每会话只取一次接收端。
- `vendor/vigem/`：vendored `ViGEmClient.dll`（140,288 B，x64，`/MT`，11 个导出），
  由 `toolchain/build-vigem-client.ps1` 从 pinned 提交构建并校验导出符号。
- `toolchain/install-vigem.ps1`：被控机一次性装驱动 + 预置 `xusb22.inf`。
- 部署链：`deploy.ps1` 上传 `ViGEmClient.new.dll`，`deploy-remote.ps1` 在**停进程之后**
  覆盖（已加载的 DLL 是共享锁）。

**环境前置（踩过的坑，见 `vendor/vigem/README.md`）**：Windows 11 把 Xbox 360 手柄的
驱动 `xusb22.inf` 改成按需下发，没外网的虚拟机永远拿不到 → 子设备停在
"需要进一步安装" → `vigem_target_add` 返回 `VIGEM_ERROR_TARGET_NOT_PLUGGED_IN`。
这个码是**二次错误**（子设备未就绪后 ViGEmClient 顺手 `remove`，而 `remove` 在未连接
状态返回的正是它），真因被盖住。`pnputil /add-driver C:\Windows\INF\xusb22.inf /install`
预置后子设备创建即绑定，**不需要重启**（安装器报的 `0x00000bc3` 是总线驱动包的状态）。

**验收**：
- 硬件回环（不需要浏览器、不需要前端）：`cargo test --release -p stream-server
  hardware_roundtrip -- --ignored --nocapture` → 2026-11-02 在 192.168.59.148 通过，
  读回 `buttons 4096(A) / left_trigger 64 / right_trigger 128 / lx 32767 / ly 32767`，
  即 12 字节 `XUSB_REPORT` 按值传递的 ABI 与映射都正确。
- 浏览器侧（**待人工验收**）：插手柄 → 游戏能动；摇杆连续无跳变；扳机是模拟量；
  双马达震动可复现；关页签后 `joy.cpl` 里设备消失；重连后只有一个手柄。

### 2.3 音量 / 静音 / 音频码率（#24，P1，L3）— 已实现，#80

**现象**：侧边栏把音频关掉，本地确实安静了（前端置 `videoElement.muted`），
但服务端仍在以 64 kbps 编码并发送——纯浪费上行；调"音频码率"没有任何效果。

**根因**：`START_AUDIO`/`STOP_AUDIO` 与 `ab,<N>` 全被归到 Ignore
（`commands.rs:81`、`:123`），`audio.rs:49` 的 `BITRATE_BPS` 是 `const`。

**改动点**：
- `pipeline/audio.rs`：`BITRATE_BPS` 改 `AtomicI32`，每帧前读一次；
  `AudioTrack` 侧塞静音帧或直接停 `write_sample`（停发会让接收端 jitter buffer
  枯竭，建议**继续发静音帧**，与现在的 `AUDCLNT_BUFFERFLAGS_SILENT` 路径复用）。
- `commands.rs`：`ab` 解析成 `Action::SetAudioBitrate(u32)`；`START_AUDIO`/`STOP_AUDIO`
  解析成 `Action::SetAudioOn(bool)`，由 `SessionCtx` 持有 `audio_on: AtomicBool`。
- 面板上的静音按钮语义要对齐：前端已经本地静音，服务端**不需要**再做音量增益，
  只要"停发/续发"。

**验收**：关音频 → `stream_stats` 之外再加一行 `audio_kbps`，应掉到 0（或静音帧的
极低值）；改码率到 32k → 抓包看 Opus 码率变化；重新打开 100 ms 内有声。

### 2.4 QoS 自适应（#25，P1，L3）— 已实现 #84

**现象**：链路一差，画面就糊成马赛克并持续糊下去，恢复后也不会自己变清晰；
`throttled` 永远是 false，面板上那个字段是装饰。

**根因**：码率只有一个来源——客户端手调 `vb,N` 或默认值。服务端拿到了
`send_ms` 与 `pipeline_ms` 的差值（#78 刚做出来），但没有任何回压逻辑。

**改动点**（建议做成一个独立的 `pipeline/qos.rs`，不要散在采集循环里）：
- 输入：`send_ms - pipeline_ms`（积压）、`pli_requests`（丢锚频率）、
  客户端 `stream_stats` 里的 `packet_loss_percent`（前端已算，服务端可直接用
  `onlatencymeasurement` 同路扩一条）。
- 输出：`encoder.set_bitrate()` 的上下调。**必须限速**：每 2 秒最多调一次，
  单次不超过 ±25%，否则会振荡成"糊-清-糊"。
- 状态机：`Normal → Degraded → Recovering`，`Recovering` 期间只升不降，
  避免刚恢复就被下一帧的积压又打下去。
- `throttled` 字段改成真实状态（当前恒 false，见 `output.rs:85`）。

**验收**：用 `tc`/clumsy 注 3% 丢包 200 ms 延迟，画面应在 5 秒内降档稳住而不是
持续花屏；撤掉注入后 10 秒内回到目标码率；全程 `keyframe_requests` 不应暴涨
（暴涨说明降档没起作用，只是 PLI 在打架）。

### 2.5 鉴权（#33，P1，L4）— 已实现 #88 → #101 → #110，**线上未开开关**

**原现象**（2026-10 记录）：局域网内任何设备浏览器打开 `https://192.168.59.148:8080/`
即可操作被控机鼠标键盘、读写 `upload_dir` 下的文件。

**这道缺口已经补完，现在的状态是「能力齐全、默认关闭」**：

| 层 | 落地形态 | 位置 |
| --- | --- | --- |
| 口令表 | `--auth-token <pw>`（单条，等价 `default:host`，空串=关闭）+ 可重复的 `--auth-grant <tenant>:<role>:<token>`（令牌放最后，因为它可能含冒号） | `config.rs:173` / `:179` / `:453` / `:460` |
| 身份 | `Grant{tenant, role}`，`Role` = `Host` / `Operator` / `Viewer`（默认 Viewer："没配凭据时谁都是观众"） | `security/token.rs:44`、`security/mod.rs:323` |
| 提取 | 五载体，顺序即优先级：`Authorization: Bearer` → `?token=`（百分号解码）→ `selkies_token` cookie → WS 子协议 `selkies.token.<b64url>` | `security/token.rs:201` |
| 比较 | 常量时间、走完整张表不提前 break（不泄漏"命中的是第几条"） | `security/token.rs:167` |
| 裁决 | HTTP 全路由（`Static` / `Status` 豁免）→ 401 带 `Bearer realm="Selkies"`；随后策略 403。**口令判定在策略之前**，否则等于告诉未认证者"这里有东西" | `http/server.rs:254` |
| 复用 | 信令与媒体拿到同一个 `grant`，`AUTH_SUCCESS` / `identified as tenant=… role=… cred=…` 落日志；观众令牌不能拿输入租约（`a_viewer_token_never_owns_input`） | `websession.rs:786`、`ws_media.rs:576` |

`--policy allow-all|deny-all` 仍在，作为覆盖率探针：`deny-all` 实测三个入口全 403
且进程照常启动。

**残余（唯一一条）：线上没设口令。** 被控机 `C:\dev\stream\auth-token.txt` 不存在，
`start-stream.cmd` 因此不传 `--auth-token`，启动日志为 `cred=none`。也就是说
**能力已经在了，风险一点没减** —— 这是配置问题，不是代码问题。

**开启方式**（无需改代码、无需重新部署）：
```powershell
# 在被控机（192.168.59.148）上
C:\dev\stream\set-password.cmd            # 写入 auth-token.txt（ASCII）
schtasks /End /TN MCStreamSrv; schtasks /Run /TN MCStreamSrv
```

**验收**：无 token 打开得到 401 且 WS 握手被拒；`?token=<pw>` 正常；
改口令后旧链接失效。`docs/deploy-one-click.md` §7.26 有 `--auth-token test123`
的实测记录（端口 18080），这条验收早已做过，只是没在这台生效。

**为什么还不算问题**：这台机器目前在局域网内、只有一个用户、无端口映射到公网。
一旦要给别人用，这一步必须前置 —— 而 `--auth-grant` 的 `tenant` 正是多用户
方案里最先要用到的东西（见 `roadmap-future.md` §7）。

### 2.6 服务自恢复（#32，P1，L4）— 已实现 #81

**现象**：`stream-server.exe` 崩溃或被杀后不会自己起来，要人 `schtasks /Run`。

**根因**：`MCStreamSrv` 是 `ONCE` 任务、`重复: 已禁用`。

**落地形态**：`/SC MINUTE /MO 1` + 单实例检查 + `wscript run-hidden.vbs` 隐藏宿主。
注意**不能**改 `sc.exe create`：Windows 服务跑在 Session 0，会丢掉 Session 1 的
桌面捕获，等于没有画面。

**已实测**：`taskkill /F /IM stream-server.exe` 杀掉 PID 1904，一分钟后 PID 7612
自起，`/api/status` 返回 200，浏览器重连即出画面。

**残余**：只覆盖「进程死」。**卡死（活着但不推流）不在覆盖范围** —— 看门狗只看
进程在不在。要做健康检查得在 `start-stream.cmd` 里加一步 `/api/status` 探活并
超时自杀，属增强项。

### 2.7 音画同步（#26，P2，L3）— 已实现 #83

**现象**：长时间观看后声音逐渐比画面慢/快（尚未被用户确认，属预防性）。

**根因**：视频由 DXGI beat 驱动（`WouldBlock` 决定快慢），音频由 WASAPI 样本数
驱动（`audio.rs` 头部注释里已自证"计数不会漂移"）。**两者各自准，但互不相干**。

**改动点**：让音频线程读视频侧的最新 `beat`，计算 A/V offset；超过 ±80 ms 时
丢/补一个 Opus 帧（丢帧比补帧听感好）。或用 RTP 时间戳对齐（更正确，改动更大）。

**验收**：连续播放 30 分钟，用拍手画面 + 声音比对，偏差 < 100 ms 且不随时间增长。

**落地差异（重要）**：原计划是"读视频侧的 beat 算 A/V offset"。**这个前提是错的**——
本管线视频是"变化才发"，静态桌面 1–2 帧/秒，视频介质时钟与墙钟无关。首版照原计划
实现，部署后实测 `av=-16199ms/40fix`，一分钟内误校正 40 次。改为**把音频媒体时钟
钉在墙钟上**，与视频无关。完整复盘见 `stage3-clipboard-qos-avsync.md` §3.1。

**2026-11-09 追加（#118）：墙钟这个基准本身也被算错了 —— 空转了两天半。**

线上每行日志都带 `av=`，实测是 `av=-140687920ms/70963fix`：音频媒体时钟被判成比
墙钟慢 **39 小时**，每 2 秒补一个 20 ms 静音帧，两天半补了 70963 次。

根因不在算法，在**取数**：`Loopback::drain()` 在没有声音播放时 500 ms 超时返回空
（注释自己写着 "a machine with no playback at all legitimately never signals"），
于是 `emit_frame` 不被调用、`note_audio` 不前进，而 `fix()` 拿的是
`now - started_at` —— 墙钟照走、媒体钟不动，差值就是"服务运行了多久"。

补观测后第一次实测量到的就是这个：连续几窗
`audio 5.0s: cap=0 ... (no frames: endpoint idle)`；而在被控机上播一段声音，
立刻变成 `cap=240 ... => 48.0 fps, 61.9 kbps` —— **空闲端点确实一个包都不产**。

修法：墙钟**只在两次音频帧之间累加**。间隔超过 `MAX_FRAME_GAP`（250 ms）算停顿
（空闲桌面、设备重启、调度抖动），恢复时把两条时钟**重新对齐**而不是把缺口算成
漂移；`fix()` 在轨道空闲时直接返回 `None`；`av=` 后面多一个 `idle=`，表示"这一行
的偏斜来自一个已经停住的时钟，不要当健康读数"。

实测（在被控机的交互会话里播 15 s 声音，然后静音）：

| 时刻 | 日志 | 含义 |
| --- | --- | --- |
| 播放中 | `av=+228ms/2fix` → `+188ms/5fix` | 首批缓冲让媒体钟超前约 200 ms，按 2 s 一步回压，**不再无限累积** |
| 静音 4.4 s | `av=+188ms/5fix idle=4391ms` | `fixes` **停在 5** |
| 静音 9.4 s | `av=+188ms/5fix idle=9398ms` | 仍是 5 |
| 静音 14.4 s | `av=+188ms/5fix idle=14398ms` | 仍是 5 —— 旧版这里已经又多了 7 次 |

新增 4 条单测，其中 `an_idle_gap_is_not_drift` 直接复现这次事故的形状：
10 分钟有声 + 8 小时静音 + 再出声，要求 `fixes` 不变且 `|skew| < 80 ms`。

### 2.8 二进制剪贴板（#18，P2，L2）— 已实现 #85

**现象**：从浏览器复制一张图粘到被控机，粘贴出来是空的或旧内容。

**根因**：`cb/cbs/cbd/cbe` 分片被显式丢弃（`clipboard.rs` 文件头）。

**改动点**：把 `ChunkState` 从 `Vec<u8>` 文本改成按 MIME 分流的字节缓冲，
写入时按类型选 `CF_DIB` / `CF_HDROP` / `CF_UNICODETEXT`；
下行读取侧要能把 `CF_DIB` 转成 PNG 再 base64（前端只认图片 base64）。

**风险**：DIB→PNG 需要编码器，现有 `pipeline/cursor.rs` 里已有一段手写 PNG
（IHDR/IDAT），可复用但只支持编码不透明位图。

**验收**：截图后 Ctrl+C 在浏览器 → 被控机 Ctrl+V 进画图能出图；反向亦然。

### 2.9 周期关键帧（#27）— 复核结论：**不是缺口**

**现象**（原记录）：静态画面挂十几分钟后，偶发一次「整屏错位一瞬再恢复」；推测
repeat 帧一直在 P 帧链上累积。

**复核**：`pipeline/encoder.rs:59` 已经设了 `keyframe_interval: Some(60)`，
`vendor/scrap/src/common/vpxcodec.rs:100-102` 把它写成 `c->kf_max_dist = 60`，
libvpx 会每 60 个编码帧产出一次关键帧；静态画面走的是 `repeat encode`
（`pipeline/mod.rs:1108-1118`，仍然是 `encoder.encode(...)`），所以画面静止时
参考链同样在被刷新。此前的 ❌ 判断是只看了 `pipeline` 的 PLI 分支、没往下看
编码器配置。

**残余（调优，非缺口）**：60 帧在 60 fps 下约 1 秒、30 fps 下约 2 秒，1080p 一个
IDR 约 100–200 KB，等于持续付出 0.4–0.8 Mbps。若要的是「长静置守护」，应改成
`--keyframe-interval-sec`（默认 10 s）并保留 PLI 立即响应 —— 这在做 QoS 时顺手做，
因为降档会被过于频繁的 IDR 直接抵消。

**已做完（#86）**：旗标落地为 `--keyframe-secs`（默认 10，clamp 1–120）。关键点是
不能只把 60 改大：`kf_max_dist` 数的是**编码帧**，本管线又是"变化才发"，同一数值
在动的画面是 1 s、在静的桌面是几分钟 —— 帧数表达不了秒。所以采集循环每拍检查
`last_keyframe.elapsed() >= keyframe_period`，到点 `force_keyframe()`；`kf_max_dist`
退成 `fps × secs × 2` 的兜底（`encoder.rs:keyframe_backstop_frames`，附 4 条单测）。
同时新增 `FrameCounters::key_frames`，在 `forward_chunks` 里按 VP8 包的 `key` 标志
计数 —— 请求了不等于发出去了，只有这个计数能证明。实测：日志每 10 s 一行
`periodic keyframe: 10.0s since the last IDR (period 10s)`，对应窗口 `kf=1`。

### 2.10 截图（#28，P3，L3）— 已实现；录屏明确不做

**原来的判断**：「`pipeline` 里缓存的最后一帧 YUV/BGRA 直接编 PNG，加
`GET /api/screenshot`」。

**落地形态**（实现时改了两处原计划）：

| 项 | 做法 | 为什么 |
| --- | --- | --- |
| 取哪一帧 | 管线每拍留的 `pristine_bgra`（#70 的无指针快照，目标分辨率、紧排 BGRA） | 编码器交出来的是 **YUV**：转回 RGB 要重写色彩矩阵和色度上采样，4:2:0 会把字边糊掉 —— 而截图就是为了看清字 |
| 何时取 | 节拍**开头**（用上一拍的快照），不是新帧分支里 | 桌面静止时新帧只有 2–5 张/秒，等新帧会给每个请求加最多半秒；代价是一次 memcpy（1080p 约 0.6 ms），帧龄 ≤16 ms |
| 在哪编 | 独立线程 `screenshot-png`，同时只允许一张 | zlib 一帧 42–51 ms，放在 60 Hz 节拍里就是可见卡顿 |
| 路由 | `GET /api/screenshot` → `image/png`，`Cache-Control: no-store` | 走的是 `Endpoint::Screenshot`（只读面），**自动受口令门保护**（非 `Static`/`Status`）；没有会话时 **503 + 原因**，而不是空图或旧图 |

新增文件：`pipeline/screenshot.rs`（请求槽 + 编码线程 + `capture()`）、
`pipeline/png.rs`（PNG 编码，指针图与整屏共用同一份 CRC/chunk 写法，
`cursor.rs` 里那份重复实现已删）。

**实测**（1920×1080 桌面，任务管理器开着）：

| 版本 | 总耗时 | 其中编码 | 文件 |
| --- | --- | --- | --- |
| 默认 zlib + 等新帧（第一版） | **0.86–0.96 s** | 未单独计时 | 876 KB |
| `Compression::fast` + 节拍开头用上一帧（最终） | **0.093 s** | 42–51 ms | 1.32 MB |

日志：`screenshot: 1920x1080 png=1318790B encode=47ms`。图像人工核对过：
壁纸紫蓝渐变正常（没有 BGRA/RGBA 通道错位）、任务栏与窗口文字清晰。

**录屏不做**：要落容器（MP4/WebM 需要 muxer + 时间戳重排），而现有管线是
"变化才发"、帧率不恒定，录出来的文件要么时长对不上要么播放器跳帧。真要做，
正确形态是复用 `--dump-frame` 那套 IVF 路径（`build-ivf` 已有痕迹）产出
`.ivf`，再离线转封装 —— 那是独立一件事，不在"补个端点"的量级。

### 2.11 一键部署（#34，P2，L4）— 已实现

**落地**：`toolchain/deploy.ps1`（本机）+ `toolchain/deploy-remote.ps1`（被控机）。
一次 `pwsh toolchain/deploy.ps1` 走完八个步骤：预检 → 构建后端 → 构建前端 →
上传 → 两端 MD5 → 停任务并**禁用**触发器 → 杀进程 → 换 exe/DLL → 前端解压到
暂存目录校验后换名 → 启任务 → 三重体检 → **归档到构建端**。
被控机上**不留 `.prev`**（每台一份 18.6 MB exe + 2000+ 文件目录副本、没人轮转）；
失败时按"这一轮动过哪些部件"从构建端 `target\deploy-archive\` 重发，也可以
`-Rollback` 手工发回上一版。

**三个设计决定**（都是为了少一类失败）：

1. 远端动作写成**文件**再 `-File` 执行，不塞内联命令 —— 内联要穿四层引号，
   `$p=C:\dev\x` 会被吃掉（本轮踩过）。
2. 换文件前 `Disable-ScheduledTask`：看门狗每分钟触发，不在那几秒里关掉它，
   它会把你正要覆盖的 exe 重新拉起来占用。
3. 体检不只查端口：还比对**进程启动时间晚于部署开始时间**与日志里的
   `listening on`。只看 200 会把"新 exe 没起来、旧进程被看门狗拉起"判成成功。

**已实测**：全量部署 495.6 s（含 `cargo build --release` 与前端构建）、
`-Rollback` 59 s 不编译、`-SkipBackend -SkipBuild` 只换前端且**不重启进程**
（会话保持），另有 `-Bootstrap` / `-DryRun` 各一次。文档
`deploy-one-click.md` 含部署内容清单、路径契约、22 条踩坑记录、手工等价命令
与重启后自检表。

**残余**：`sshd` 自启由脚本幂等保证，但**拿到一台新机器时第一次仍要人**到控制台
执行三行（脚本连不上就只能打印它们）。这一条无法自动化，写进了文档 §3.1。

### 2.12 音频观测（新增，P1，L3）— **日志已补，`StreamStats` 三字段仍缺**

**原现象**：静音到底生效了没有、音频在发多少码率、麦克风有没有真的在送数据 ——
服务端一行日志都不打，只能抓包或拿耳朵判断。

**2026-11-09 已做**（#118 顺带）：`pipeline` 的 5 s 摘要下面多了一行音频孪生：

```
audio 5.0s: cap=240 enc=240 bytes=38721 sil=0 => 48.0 fps, 61.9 kbps
audio 5.0s: cap=0 enc=0 bytes=0 sil=0 => 0.0 fps, 0.0 kbps (no frames: endpoint idle or device gone)
```

- `cap` = 这一窗写进轨道的 20 ms 帧数，`=> N fps` 就是"每秒产生多少音频帧"；
- `sil` 是设备标了 `AUDCLNT_BUFFERFLAGS_SILENT` 的包数，**与 `cap=0` 不是一回事**：
  静音的混音仍然产包，空闲端点一个都不产；
- `cap=0` 时行尾明写原因，否则一串 0 会被读成"音频坏了"。

`/api/v1/health` 的 `stream.audio` 早就有 `uplink.live`（麦克风侧）全字段，
本轮核对确认它已经在被消费（`http/health.rs:520` → `audio::uplink_stats()`），
原文那句「全仓没有一处调用」已作废。

**仍缺**：`dispatch/output.rs` 的 `StreamStats` 里没有任何 `audio_*` 字段
（`output.rs:100` 有 cpu/encode/pipeline/send/video_mbps/rtt/keyframe，没有音频），
所以前端 HUD 与面板统计里看不到音频 —— 要么把它们塞进 `stream_stats`，要么让
面板直接轮询 `/api/v1/health`（后者零改动，但 HUD 是 1 Hz 的本地循环，
多一条网络请求不如多三个字段）。`pipeline::audio::totals()` 现在有消费者了
（就是上面那行日志），`uplink_stats()` 则继续供 health。

**为什么仍是 P1**：health 只在被查询时是"现在"，日志才是"刚才"。#79/#80 的
验收还需要一次开麦，而 `last_packet_age_ms` 这类字段必须连续采样才有意义。

---

## 3. 任务排序

依赖关系用「← 表示前置」标。同一优先级内，序号即建议执行顺序。
**已划掉的项表示本轮已完成**，保留在表里是为了让排序理由可回溯。

| 优先级 | 任务 | 依赖 | 状态 / 一句话理由 |
| --- | --- | --- | --- |
| ~~P0~~ | ~~2.1 麦克风上行~~ | — | 已实现 #79，**待实机验收** |
| ~~P1~~ | ~~2.6 服务自恢复~~ | — | 已实现 #81，已实测杀掉自起 |
| ~~P1~~ | ~~2.3 音量/静音/码率~~ | — | 已实现 #80，**待实机验收** |
| ~~P1~~ | ~~2.4 QoS 自适应~~ | — | **已实现 #84**（比原计划早：没等关键帧调优，因为降档后 IDR 的相对开销问题要靠后续调 `kf_max_dist` 解决，不阻塞） |
| ~~P2~~ | ~~2.7 音画同步~~ | — | **已实现 #83** |
| ~~P2~~ | ~~2.8 二进制剪贴板~~ | — | **已实现 #85**，PNG 解码器是自写的（flate2），没引新依赖 |
| ~~P2~~ | ~~2.9 残余：关键帧秒级可配~~ | — | **已实现 #86**，`--keyframe-secs` 默认 10 s，墙钟为准、帧数是兜底 |
| ~~P2~~ | ~~2.11 一键部署~~ | — | **已实现 #34**，`deploy.ps1` + `deploy-remote.ps1`，两次全量 + 一次回滚实测 |
| **P1** | **2.12 音频观测（收尾）** | — | 5 s 摘要日志已补（`audio 5.0s: cap=… => N fps`），health 的 `stream.audio.*` 已在用；**只剩** `StreamStats` 的 `audio_*` 三字段（HUD 才看得到） |
| ~~P1~~ | ~~2.5 鉴权~~ | — | **已实现 #88→#101→#110**（口令表 / 五载体 / 租户+角色 / 五处裁决）。剩下的不是代码，是**在被控机跑一次 `set-password.cmd`** |
| ~~P2~~ | ~~2.2 手柄~~ | — | **已实现 #17**，`dispatch/gamepad.rs` + vendored `ViGEmClient.dll`；硬件回环在被控机通过，浏览器侧待人工验收 |
| ~~P3~~ | ~~2.10 截图~~ | — | **已实现 #28**：`GET /api/screenshot`，1920×1080 请求 93 ms（编码 42–51 ms）。录屏拆出另议 |

**剩 1 项收尾 + 1 条配置债**：2.12 的 `StreamStats` 三字段（HUD 才看得到音频），
以及 2.5 的口令（`set-password.cmd` + 重启任务，两步，不需要构建）。
2.11 落地之后，剩下的每一项验收都只要一条命令，摩擦不再是理由。

---

## 4. 明确不在本文档范围

以下各项**不在此处**，避免重复分析：

- 游戏手感（端到端延迟、指针锁定复现、HID 注入、DPI 自适应）→ `roadmap-future.md` §1
- 分辨率跟随 / `Fit Guest` → `roadmap-future.md` §2
- 单设备多显示 / 多客户端并行操作 → `roadmap-multi-display-multi-input.md`
- 硬编（NVENC/QSV）→ 不可做：VMware SVGA `15AD:0405`，`gpu_present:false`
