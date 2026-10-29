# L2 / L3 缺口清单与任务分解

> **代码基线**：#81（后端 `9734d1f`，前端 `e43b56e`），`cargo test -p stream-server`
> **41 通过 / 0 失败**（2026-10-29 复跑）。部署机 192.168.59.148：stream-server
> PID 7612（看门狗拉起）、MC 客户端 java PID 3908，同在 Console 会话 1。
>
> **本文档的定位**：`roadmap-future.md` 末尾写「L2/L3 的缺口清单另见
> `roadmap-l2-l3-gaps.md`」，此前一直没写。现在补上，两者严格不重叠——
> 那边收「已分析清楚但暂缓」，这边收「范围内该做、还没做」。
>
> **全量 34 项：26 完成 / 2 部分 / 6 未做**（2026-10-29 复核）。
> 本轮订正两处此前误判：**#27 周期关键帧其实早已由编码器 `kf_max_dist=60` 覆盖**
> （见 §1.3），**#32 服务自恢复已由 #81 完成并实测**。新增 §2.12「音频无观测」。
>
> **「完成」不等于「已验收」**：`#21 麦克风上行`、`#24 音量/静音/码率`、
> `#23 统计浮层`、`#79 前端按钮` 四项代码已部署，但服务端日志里
> `findstr "mic uplink|render open|uplink playing"` 命中数为 **0** —— 也就是
> 还没有一次真实开麦记录。判定为 ✅ 依据是链路闭环 + 单测，**实机验收仍待用户操作**。

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
| 2 | 服务端 offerer + ICE 双向 + 单会话接管（code 4001 驱逐旧页签） | ✅ | `websession.rs:61-79` |
| 3 | DXGI 桌面复制（`no_hdr` 跳过探测、桌面重建恢复） | ✅ | `pipeline/capture.rs`、`stage1-capture-recovery.md` |
| 4 | VP8 编码 → 分片 → RTP 写轨（有界队列 + 溢出丢帧求关键帧） | ✅ | `pipeline/mod.rs` `forward_chunks` |
| 5 | 动态 fps / 分辨率 / 码率（`_arg_fps` / `r,WxH` / `vb,N`） | ✅ | `dispatch/commands.rs`、`pipeline/settings.rs` |
| 6 | 静态画面 repeat 帧 + PLI/FIR 去抖关键帧（500 ms 下限） | ✅ | `pipeline/mod.rs:107-126` |
| 7 | 静态托管 + TLS + 文件传输（列目录 / 下载 / 分片上传） | ✅ | `http/server.rs`、`http/file_transfer.rs` |
| 8 | PC 状态机与断线恢复 | ✅ | `stage1-signaling-session.md` |

### 1.2 L2 输入面 — 8 完成 / 1 部分 / 2 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 9 | 键盘 keysym→VK（保布局语义、死键、被控端 IME） | ✅ | `win_input.rs` `keysym_to_vk` / `inject_key` |
| 10 | Unicode 文本注入（中文 / 日文 / emoji / BMP 外代理对） | ✅ | #76 `749c8d8`，`keysym_to_unicode` |
| 11 | 绝对指针 `m`（seq 去重、frame→screen 映射） | ✅ | #72 `bd5cf0e` |
| 12 | 相对指针 `m2`（1:1 直通，不再按 screen/frame 缩放） | ✅ | #74 `6999a3c` |
| 13 | 按键 mask 同步 / 滚轮脉冲 / `kr` 全释放 | ✅ | `sync_buttons` / `sync_scroll` |
| 14 | 游戏模式光标合成（锁定指针时把 OS 光标画进帧） | ✅ | #70 `976edd6` |
| 15 | 指针渲染权交接（`SET_NATIVE_CURSOR_RENDERING`） | ✅ | #57 |
| 16 | 剪贴板文本双向（`REQUEST_CLIPBOARD` / `cw-cwe`） | ✅ | #55 `dispatch/clipboard.rs` |
| 17 | 手柄（`js,c/d/b/a/h` + `rumble` 下行） | ❌ | 前端 `input.js:5190-5193` 全套已发；服务端 `win_input.rs:737` 当 informational 丢弃，`grep rumble` 在整个 Rust 侧零命中 |
| 18 | 二进制剪贴板（`cb/cbs/cbd/cbe`） | ❌ | `clipboard.rs` 头部明写 "Binary flavours … logged and dropped in stage 1" |
| 19 | 触控 | 🟡 | 单指拖动走 `m`/`m2` 已通；**屏幕虚拟摇杆走 `js,a`**（`gamepad.js:125` 监听 `touchgamepadinput`），因此随 #17 一起被挡 |

### 1.3 L3 画质与体验 — 6 完成 / 3 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 20 | 游戏声音下行（WASAPI loopback → Opus 48k/2ch/64kbps） | ✅ | #77 `6a095e2`，`pipeline/audio.rs` |
| 21 | 麦克风上行（`on_track` → Opus 解码 → WASAPI render） | ✅* | #79，`webrtc/mod.rs` `on_track` + `pipeline/audio.rs` `spawn_uplink` 双线程；*待实机验收 |
| 22 | 服务端可观测性（RTT / video_mbps / send_ms / peak / 关键帧请求） | ✅ | #78 `88e80dd`，`docs/stage3-observability.md` |
| 23 | 统计浮层 HUD（F3，九行） | ✅ | 前端 `lib/stats-hud.js`，`afcf2ad` |
| 24 | 音量 / 静音 / 音频码率 | ✅* | #80，`Action::SetAudioOn` / `SetAudioBitrate`，静音期间发静音帧（~1.6 kbps 而非停发）；*待实机验收 |
| 25 | QoS 自适应码率（丢包 / 积压反压） | ❌ | `output.rs:97` `throttled` 恒 false；#78 已把 `send_ms - pipeline_ms`（积压）做成数字，但没人用它回头调码率 |
| 26 | 音画同步 | ❌ | 音频是独立 OS 线程按样本数推进，视频走 DXGI beat，两者无共享时基，无漂移校正 |
| 27 | 周期关键帧 / 长参考链守护 | ✅ | **此前误判为未做**。`pipeline/encoder.rs:59` 设 `keyframe_interval: Some(60)`，`vendor/scrap/src/common/vpxcodec.rs:102` 映射为 `kf_max_dist = 60` —— 每 60 个**编码帧**强制一个 IDR，而静态画面走 `repeat encode`（`mod.rs:1111`）同样经过编码器，所以静止时也在刷新参考链 |
| 28 | 截图 / 录屏 | ❌ | `grep screenshot\|record` 在 Rust 侧零命中，无 HTTP 端点也无下行命令 |

> #27 的残余问题不是"没有周期关键帧"，而是**间隔不可配且偏短**：60 帧 ≈ 30 fps 下
> 2 秒，1080p 一个 IDR 约 100–200 KB，等于常驻 0.4–0.8 Mbps 花在刷新参考链上。
> 若要做的是"长静置守护"，正确形态是 `--keyframe-interval-sec`（默认 10 s）+
> 保留 PLI 立即响应，即把 60 改大、把秒级守护交回策略层。属调优，不是缺口。

### 1.4 L4 产品闭环 — 4 完成 / 1 部分 / 1 未做

| # | 能力 | 状态 | 证据与断点 |
| --- | --- | --- | --- |
| 29 | 自动拉起游戏 + 全屏 + 自动进服 | ✅ | `C:\dev\mc\launch-remote.ps1`（`-Server/-Port/-Fullscreen`），schtasks `MCGame` /IT 进交互会话 |
| 30 | 默认灵敏度三层固化（游戏 0.5 / 关加速 / gain 1.0） | ✅ | `mouse-sensitivity-default.md`、`start-mc.ps1` |
| 31 | 单会话接管（第二个页签驱逐第一个） | ✅ | `websession.rs:70-78`，属策略非缺陷 |
| 32 | 服务自恢复 | ✅ | #81 `0159076`：`MCStreamSrv` 改 `/SC MINUTE /MO 1` + `run-hidden.vbs` 隐藏宿主 + 单实例检查。实测 `taskkill` PID 1904 → 一分钟后 PID 7612 自起，当前跑的正是它 |
| 33 | 鉴权 | ❌ | `http/mod.rs:7` 明写 "no auth gate in stage1"，`config.rs` 无 `--token`；局域网内任何人打开 8080 即取得鼠标键盘，并可读写 `upload_dir` |
| 34 | 一键部署 | 🟡 | 仍靠手工 `scp` + `certutil` 校验 + `schtasks /Run`；前端还要 `npm run build` 再整目录上传。`toolchain/` 下**没有** `deploy-148.ps1`（只有 `deploy143`，是另一台机器的旧脚本） |

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

### 2.2 手柄（#17，P1，L2）

**现象**：插手柄、按任意键，游戏无反应。前端 `navigator.getGamepads()` 能读到设备。

**根因**：两层缺失。①服务端 `win_input.rs:737` 把 `js` 归到 informational；
②即便解析了，Windows **没有用户态注入手柄的 API**——`SendInput` 只管键盘鼠标，
游戏读的是 XInput / Raw Input 设备栈。业界做法是 ViGEmBus（签名内核驱动，
虚拟出 Xbox 360 手柄），Moonlight/Sunshine 走的正是这条路。

**改动点**：
- `dispatch/win_input.rs`：解析 `js,c,<idx>,<b64id>,<axes>,<buttons>` /
  `js,d,<idx>` / `js,b,<idx>,<btn>,<val>` / `js,a,<idx>,<axis>,<val>` / `js,h,...`，
  维护每个 slot 的状态机。
- 新增 `dispatch/vgamepad.rs`：ViGEmBus client（`vigemclient` crate 或直接调
  `ViGEmClient.dll`）。**需要先在被控机装驱动**（ViGEmBus Setup，一次即可）。
- 下行 `rumble` 消息：从 `IGamepad::notification` 回调转成 control 通道 JSON。

**风险 / 前置**：装内核驱动属于环境变更，且 ViGEmBus 已停止维护（上游 2023 归档）。
若不愿引入驱动，本项应转入 `roadmap-future.md`，明确"不装驱动就不支持手柄"。

**验收**：Xbox 手柄插在被控机或客户端任一侧（客户端侧经浏览器转发）都能驱动游戏；
摇杆走位连续无跳变；扳机是模拟量不是开关；`rumble` 能让手柄震动。

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

### 2.4 QoS 自适应（#25，P1，L3）

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

### 2.5 鉴权（#33，P1，L4）

**现象**：局域网内任何设备浏览器打开 `https://192.168.59.148:8080/` 即可操作
被控机鼠标键盘、读写 `upload_dir` 下的文件。

**根因**：`http/mod.rs:7` 与 `server.rs:148` 明确不做校验。前端有
`lib/auth-guard.js` / `lib/session-token.js`，但那是**客户端侧**的把戏，
服务端完全信任 URL 里的 app 段。

**改动点**：
- 最小可用：`--token <str>` 启动参数，校验 `?token=` 或 Cookie；
  WS 升级与 `/api/files`、`/api/upload` 三条路径统一过一道
  `check_auth(&req, &cfg)`，失败 401。
- 正确做法：一次性登录换 session cookie（HttpOnly + Secure），WS 与 API 共用。
- 前端 `session-token.js` 已能承载，不需要大改。

**验收**：无 token 打开得到 401 且 WS 握手被拒；带 token 正常；改 token 后旧
链接失效。

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

### 2.7 音画同步（#26，P2，L3）

**现象**：长时间观看后声音逐渐比画面慢/快（尚未被用户确认，属预防性）。

**根因**：视频由 DXGI beat 驱动（`WouldBlock` 决定快慢），音频由 WASAPI 样本数
驱动（`audio.rs` 头部注释里已自证"计数不会漂移"）。**两者各自准，但互不相干**。

**改动点**：让音频线程读视频侧的最新 `beat`，计算 A/V offset；超过 ±80 ms 时
丢/补一个 Opus 帧（丢帧比补帧听感好）。或用 RTP 时间戳对齐（更正确，改动更大）。

**验收**：连续播放 30 分钟，用拍手画面 + 声音比对，偏差 < 100 ms 且不随时间增长。

### 2.8 二进制剪贴板（#18，P2，L2）

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

### 2.10 截图 / 录屏（#28，P3，L3）

**现象**：无。属产品功能缺失。

**改动点**：截图最容易——`pipeline` 里缓存的最后一帧 YUV/BGRA 直接编 PNG，
加 `GET /api/screenshot`（需鉴权）。录屏要落容器（IVF 已有 `build-ivf` 痕迹），
成本高。

**建议**：先只做截图，录屏等有明确需求再说。

### 2.11 一键部署（#34，P2，L4）

**现状**：后端 `scp` + `certutil` + `schtasks /Run` 三步手工；前端 `npm run build`
后整目录上传（旧目录改名备份）。每次都靠人记。

**改动点**：`toolchain/deploy-148.ps1`：构建 → MD5 比对 → 停任务 → 备份 →
覆盖 → 启任务 → 轮询 `/api/status` 直到 200 → 断言日志里出现
`listening on`。失败即回滚到 `.bak`。

**验收**：一条命令从干净工作区到可连；中途失败能自动回滚。

### 2.12 音频没有观测（新增，P1，L3）

**现象**：静音到底生效了没有、音频在发多少码率、麦克风有没有真的在送数据 ——
服务端一行日志都不打，只能抓包或拿耳朵判断。

**根因**：`pipeline/audio.rs` 里 `totals()`（captured / encoded / bytes / silence）
与 `uplink_totals()`（packets / frames / concealed / silence / dropped）**都已经写好了，
但全仓没有一处调用**；`stream_stats` 里也没有任何 `audio_*` 字段。这两组计数器
是 #77/#79 留下的半成品，只差一个消费者。

**改动点**：
- `pipeline/audio.rs`：每 5 s 打一行摘要（与 `pipeline 5.0s:` 同格式），
  静音切换时打状态变化而不是每帧打。
- `dispatch/output.rs`：`StreamStats` 加 `audio_kbps` / `audio_on` / `mic_kbps`
  三个字段，前端 `stream-stats-view.js` 与 HUD 各加一行。
- 判据：关音频后 `audio_kbps` 掉到个位数（静音帧），开回来 100 ms 内有声；
  开麦后 `mic_kbps` 从 0 变正数。

**为什么排 P1**：这是 #79/#80 的验收工具。没有它，那两项只能算「代码写完了」，
出问题时也无从定位是编码、传输还是设备。

---

## 3. 任务排序

依赖关系用「← 表示前置」标。同一优先级内，序号即建议执行顺序。
**已划掉的项表示本轮已完成**，保留在表里是为了让排序理由可回溯。

| 优先级 | 任务 | 依赖 | 一句话理由 |
| --- | --- | --- | --- |
| ~~P0~~ | ~~2.1 麦克风上行~~ | — | 已实现 #79，**待实机验收** |
| ~~P1~~ | ~~2.6 服务自恢复~~ | — | 已实现 #81，已实测杀掉自起 |
| ~~P1~~ | ~~2.3 音量/静音/码率~~ | — | 已实现 #80，**待实机验收** |
| **P1** | **2.12 音频观测** | — | 是上面两项的验收工具；不做就只能靠耳朵 |
| P1 | 2.5 鉴权 | — | 局域网内裸奔，属安全债 |
| P1 | 2.4 QoS 自适应 | ← 先做 2.9 残余（关键帧间隔可配） | 唯一能把"糊"变成"自动恢复"的机制 |
| P2 | 2.11 一键部署 | — | 减少每次验证的摩擦；当前每轮验证要手工七步 |
| P2 | 2.7 音画同步 | ← 2.1 验收通过 | 有麦克风后音画关系才真正复杂 |
| P2 | 2.2 手柄 | ← 需先决定是否装 ViGEmBus 驱动 | 前端全套已就绪，卡在驱动 |
| P2 | 2.8 二进制剪贴板 | — | 依赖 PNG 编解码，收益中等 |
| P3 | 2.10 截图 | ← 2.5（鉴权） | 纯增量为先不碰 |

**建议下一轮**：2.12 音频观测 → 2.5 鉴权 → 2.4 QoS。
2.12 放第一不是为了功能，而是因为它把「麦克风到底通没通」从主观变成一行数；
2.5 是唯一的安全债，越晚做越难补（前端 token 通道已就绪，越往后改路由面越大）；
2.4 放最后是因为它需要 2.9 的残余先落地，否则降档会和过于频繁的 IDR 打架。

---

## 4. 明确不在本文档范围

以下各项**不在此处**，避免重复分析：

- 游戏手感（端到端延迟、指针锁定复现、HID 注入、DPI 自适应）→ `roadmap-future.md` §1
- 分辨率跟随 / `Fit Guest` → `roadmap-future.md` §2
- 单设备多显示 / 多客户端并行操作 → `roadmap-multi-display-multi-input.md`
- 硬编（NVENC/QSV）→ 不可做：VMware SVGA `15AD:0405`，`gpu_present:false`
