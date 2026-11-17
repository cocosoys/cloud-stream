# 未来优化范围（L1→L4 打通后的暂缓项）

> 状态：**L1→L4 技术路线已验证可跑通**（浏览器出画面 → 输入注入 → 按需画质/工具栏 →
> 自动拉起游戏并进服）。本文件收纳「已经分析清楚、但当前不做」的项，
> 目的是让它们**不丢失、不重复分析、不被误当成缺陷**。
>
> 本文件只写"为什么暂缓"和"什么条件下重新开工"，实现细节指向各自的设计稿。
> 与之并列的还有 `roadmap-multi-display-multi-input.md`（多显示/多操作）与
> `capacity-slicing.md`（一台物理机切成多台可售设备的边际成本与上限），
> 三份文件合计构成当前的"待办背面"。
>
> **2026-11 追加**：§7 登记「多用户各自独立实例（虚拟屏 / 多开 / 输入隔离）」
> 的完整结论。它是 `§3` 与 `roadmap-multi-display-multi-input.md §7` 的展开，
> 把"为什么真并行多指针在 Windows 上做不到"从一句结论变成了带实测数据的判定表。
>
> **2026-11-17 追加**：§8 登记 `main` 分支纯净化。分支收口时把 `web-game` 合进了
> `main`，基线的定位暂时失效；清理方案、临时冻结策略与回访触发都在那一节。

---

## 0. 判定口径

一条项进这张表，需要同时满足：

1. **阻塞已解除**：L1→L4 主链路不依赖它也能跑通（否则它是 bug，不是优化）。
2. **原因已定位**：不是"感觉不对"，而是有具体机制 + 有可复现的量化证据。
3. **有明确的回访触发条件**：写清楚什么现象/什么阶段重新打开，避免无限期挂起。

未满足 1、2 的进 issue，不进本文件。

---

## 1. 游戏手感（本轮新收）

> 详细推导与数据见 `mouse-sensitivity-default.md`。此处只记状态与后续。

### 1.1 已落地（不在"未来"范围，仅作基线记录）

| 项 | 取值 | 生效方式 |
| --- | --- | --- |
| 被控机指针加速 | `MouseSpeed=0 / Threshold1=0 / Threshold2=0` | 启动薄壳内 `SPI_SETMOUSE`，每次启动重放 |
| 游戏内灵敏度默认 | `mouseSensitivity = 0.5` | 启动前写 `options.txt`，附三处快照日志 |
| 浏览器相对增益 | `pointerGain = 1.0`，键名 `selkies-pointer-gain-v2` | 侧边栏滑块 25%~300%，默认 100% |

标定依据：玩家 DPI 分布（800 占 52%）× MC 灵敏度分布（Hypixel 中位 65%）加权，
目标 8 cm/360，反解得 0.472，合理区间 0.42–0.53；0.5 同时是 MC 原生默认值。

### 1.2 暂缓项

#### (a) 端到端输入延迟（P0 候选，尚未量化）

**现象**：玩家报告"手停了视角还在转"，属于**积压回放**而非灵敏度。

**已定位的机制**：

- `input.js` 的合并发送在 backlog 高时选择**延后发送**而不是丢弃，历史位移会排队重放。
- 绝对模式（`m`，control 可靠通道）本身是"设目标位置"，一旦队列有积压，
  服务端会按序重放每一条历史位置 —— 症状就是"停手后继续转"。
- 被控机渲染后端是 **VMware SVGA3D + Mesa 24.1 软渲染**，日志实测
  `Display.update` 12.25 ms/帧 ≈ 81 fps（加载画面；游戏内更低），
  这一段延迟与输入链路无关，是虚拟机图形栈的固有成本。

**为什么暂缓**：真正要改的是"积压时丢帧而不是排队"+"测量链路各段耗时"，
两项都需要先有端到端计时，否则改完无法判断好坏。而当前会话里
`m2=0`（相对通道未被使用），说明玩家实际还在绝对模式，先修输入模式
才能谈延迟（见 1.2(b)）。

**回访触发**：以下任一成立即开工
- 有玩家明确报告"延迟可感知"且能在 `m2>0` 的会话里复现；
- 引入音频后做音画同步，必须先把延迟数字化。

**开工时要做**：浏览器打点（`pointermove` → `send`）→ 服务端打点（`handle_line`
→ `SendInput`）→ 帧级打点（`SendInput` → `encode`）三段分别计时，
在统计行里增加 `in_ms` / `q_depth` 两个字段。

#### (b) 指针锁定是否真的启用（**#124 已落地服务端一侧**）

**现象**：最近多次会话日志持续为 `m=245 m2=0 dm=0 pm=245`，即全部走绝对通道。

**含义**：`m2` 恒为 0 时，`pointerGain` 滑块、相对位移直通（#74）**全部不生效**——
它们只作用于 `m2`。玩家感到"能用"是因为绝对模式在窗口化游戏里也能操作，
但指针锁定下绝对模式会表现为"移到窗口边缘就转不动"。

**#124 做了什么**（对照数字与边界见 `pointer-1to1.md`）：

- 首次 `m2` / `p,1` 自动把被控机指针钉到 1:1（关加速 + 速度档 10）；`p,0`、
  会话结束、30 秒无位移三条路径幂等还原；`ma,` 仍可显式覆盖。
- 锁定 + 有位移时**不再注入** `m`（页面那条被夹到窗口里的坐标会把累积偏移抹掉），
  解锁时补一次绝对定位。
- 锁定 + 有位移时**不合成** OS 光标（游戏自己画准星，多一个就是一个）。
- 统计行补 `pin=` / `ms=` / `clip=` / `dmax=`——其中 `dmax` 是唯一能在服务端看见
  "客户端那一下被 400 px 上限吞了"的证据。
- `--pointer-recenter`（**默认关**）把贴边的指针拉回中间，这是"转不动"的物理根因；
  关着是因为它动被控机的真光标。

**还剩什么**：`m2` 到底会不会来取决于前端。服务端这一侧现在把答案写在 `pin=` /
`ms=` 上，不再需要靠猜。

**回访触发**：玩家报告"转了半圈就转不动"或"甩鼠标视角跟不上"——先看 `pin=`。

#### (c) 内核级 HID 相对注入（明确暂缓）

**现状**：相对移动走 `SendInput(MOUSEEVENTF_MOVE)`，会被游戏的
Raw Input 之外路径接收，但绕不开 Windows 指针加速曲线（已通过关加速规避）。

**业界做法**：Moonlight/Sunshine 用虚拟 HID 设备，Parsec 用 `parsecvusba` 驱动，
都是为了**绕过 SendInput 与加速曲线**，让吃 Raw Input 的游戏拿到真实 counts。

**为什么暂缓**：需要内核驱动签名与安装流程，收益仅在"吃 Raw Input 的 FPS 游戏"。
当前目标是 MC（走 LWJGL 的 `Mouse.getDX()`，已被关加速后的 SendInput 满足）。

**回访触发**：接入第二款吃 Raw Input 的游戏且实测手感不可接受。

#### (d) 灵敏度自适应（按客户端 DPI 自动换算）

**现状**：默认 0.5 按 800 DPI 标定，非 800 DPI 的玩家需手动调滑块
（400→190%、1600→42%，gain 与 slider 是立方根关系）。

**为什么暂缓**：浏览器**无法读取物理 DPI**（`devicePixelRatio` 是缩放比，不是 DPI）。
可行路径只有"首次进入时让玩家做一次 360° 转身校准"，属于交互设计问题，
不是工程缺口。

**回访触发**：玩家反馈"默认值明显不对"的比例超过主观阈值。

---

## 2. 分辨率与缩放跟随

**现状**：被控机是 VMware SVGA（`15AD:0405`），`Fit Guest` 会强制把客户机分辨率
拉回宿主窗口尺寸（实测 `VideoModeDescription = 1718x926`，注册表
`ActiveSize.cx/cy = 0x6b6/0x39e`）。服务端因此把客户端的超限请求**钳制到原生尺寸**
（#66），前端 `object-fit: contain` 产生黑边。

**已做的缓解**：游戏模式用 `[data-selkies-gaming-fill]` 属性规则 + 全属性 `!important`
做 `fill` 拉伸，牺牲等比换取铺满。

**为什么暂缓**：根因在虚拟显卡的 `Fit Guest`，不在本工程。要真正 1920×1080 只有两条路：
在 VMware 端关闭 `Fit Guest` 并手动指定分辨率，或改为直通物理 GPU。

**回访触发**：被控机换物理显卡，或 VMware 侧关闭 `Fit Guest`。

**注意（历史坑）**：`server_settings` 里 `manual_resolution` 必须保持 `false`。
它曾硬编码 `true` 并把 `manual_width/height` 回显为"上次客户端请求值"，
形成自指闭环，把一次 1920×1072 永久固化（#71）。

---

## 3. 单设备多显示 / 多操作

完整切口清单见 `roadmap-multi-display-multi-input.md`。此处只登记状态：

| 能力 | 状态 | 前置条件 |
| --- | --- | --- |
| 单设备多显示 | 只留切口 | `capture` 按 `output_index` 枚举、`video-<i>` 多轨、协议 `_d,<display>` 会话绑定 |
| 多操作（多客户端同时操作） | **不做** | Windows 只有一个系统指针，`SendInput` 改的是同一个光标；真并行需多 Windows 会话，等于换掉采集+注入模型 |

---

## 4. 与 L2/L3 的边界

本文件收纳的项**不进入** L2（输入面）/L3（画质与体验）的完善范围，理由是它们的
阻塞点分别是"缺测量"、"缺硬件条件"、"缺交互设计"，而不是"缺功能"。

L2/L3 的缺口清单见 `roadmap-l2-l3-gaps.md`（**全量 35 项：35 完成 / 0 部分 / 0 未做**，
另有 12 项缺口拆解与 1 条收尾），
两者不重叠：那边只收「范围内该做、还没做」，这边只收「已分析清楚、暂时不做」。

---

## 5. 麦克风上行：从"注入扬声器"改为"虚拟录音设备"

> 状态：**探测层（#109）、WS 上行（#110）与默认录音设备切换（P1）都已落地并在 .148 上
> 验穿**（切过去 → 强杀后接管 → `off` 还原 → 再切回去，四步都留了日志与 health 证据）。
> 剩 P2 Parsec IOCTL、P3 Linux 两行 pactl。

**原位问题**：麦克风注入到**默认 render 端点**，而下行 loopback 采集的正是同一端点的
后混音信号，于是开麦时玩家听到自己，延迟一个往返（按代码常量估算 150–330 ms，
不是原文写的 100–200 ms）。这是拓扑决定的，改延迟、改码率都消不掉。

**核对过的事实**（2026-11）：

1. **这在当前传输上根本复现不了**。WS 传输收到麦克风二进制帧（`0x02`）时直接回
   `MICROPHONE_DISABLED`（`http/ws_media.rs:677`），页面随即关麦；上行只存在于
   WebRTC 传输（`webrtc/mod.rs:314`）。所以这不是"已知缺陷"，是"没复刻的功能"。
2. **业界没有一家把麦克风注入默认扬声器**。Steam 用 `Steam Streaming Microphone`、
   Parsec 用 VUSB 虚拟 USB 麦克风、Selkies（本项目上游）用 PulseAudio 的 null-sink +
   virtual-source。只有 Sunshine 那个未合并的 PR #4078 用了 `mic_sink=default`，
   和原实现同构，关闭理由全是协议层的，没人讨论回声。
3. **注入扬声器只能实现"对讲"**。游戏读的是 capture 端点，写 render 端点永远到不了
   游戏内语音——功能语义和实现方式在这里是绑死的。

**决议**：麦克风落到**虚拟录音设备**上，回声因为回路不存在而消失，一行 AEC 都不用写。
Windows 上"虚拟设备"不必自研，按优先级探测现成的虚拟线缆：

| 优先级 | 提供方 | 我们写入（render 端） | 程序选它当麦克风 | 需要装什么 |
|---|---|---|---|---|
| 1 | Steam | `Steam Streaming Microphone` | 同名 | Steam 自带，普通权限 |
| 2 | VB-CABLE | `CABLE Input` | `CABLE Output` | VB-Audio Virtual Cable |
| 3 | VoiceMeeter | `VoiceMeeter Input` | `VoiceMeeter Output` | VoiceMeeter |
| 4 | Parsec VUSB | 无端点（IOCTL 造 USB 麦克风） | `Parsec Virtual Microphone` | VUSB 驱动 + 管理员；**后端未实现** |

Steam 排第一是因为它不需要额外安装：玩家机器上本来就有，我们只是往它的 render 端起
一个 WASAPI 客户端——和写默认扬声器是同一段代码。

**已落地**：
- **WS 上行（#110）**：`ws_media.rs` 把 `0x02 + Opus` 送进 `audio::MicUplink`，与 WebRTC
  共用同一个解码/写设备循环；页面的 24 kHz 单声道由 Opus 解码器直接出 48 kHz 立体声。
  设备按需打开（第一包到达）、空闲 1.5 s 关闭，只维持 50 ms 排队深度（填满 100 ms 设备
  缓冲会让数据与静音交替，听感是断续的 —— 这一条是实测踩出来的）。
  health 的 `stream.audio.uplink.live` 报 `open/packets/frames/dropped/decode_errors/
  silence/last_packet_age_ms`。
- `platform/virtual_mic.rs`：端点枚举（友好名，容忍驱动版本加的后缀）、决议、
  进程级单例；`--mic-device auto|default|steam|vb-cable|voicemeeter|parsec|<端点名>`。
- 启动日志 `microphone probe | ...` + `microphone target | ...`，health 的
  `stream.audio.uplink` 报 `provider / device / capture / usable / echoes / note`。
- **自听陷阱有硬拦**：虚拟线缆若同时是默认播放端点，决议直接判 `usable:false` 并
  给出改法（这正是 `steam-deck-routing-for-moonlight` README 里踩过的坑）。
- 没有可用设备时**不静默退回默认扬声器**，而是让上行失败并说明装什么——静默退回
  会让"一切看起来正常，只是每次开麦都听见自己"。
- **正向验证（2026-11-05，被控机 192.168.59.148）**：装 VB-CABLE 45 后 `auto` 决议为
  `provider=vb-cable / usable=true / echoes=false`；WS 推 251 帧 24 kHz Opus，
  `mic-probe.exe` 从 `CABLE Output` 读到 rms −22 dBFS、peak −17.9 dBFS、`VERDICT=SIGNAL`
  （未开麦时同一端点只有 −96.7 dBFS 底噪）。读数端与被控端两个工具都在仓库里：
  `crates/stream-server/src/bin/mic-probe.rs`、`toolchain/mic-push.py`。细节见
  `stage3-audio-design.md` §8.5。

- **P1 默认录音设备切换（2026-11-05）**：`platform/default_mic.rs` 用未文档化的
  `IPolicyConfig::SetDefaultEndpoint` 把 console / multimedia / communications 三个
  role 一起切到虚拟麦的 capture 端点（只切前两个的话，走通信角色的 Discord、Steam
  语音仍指着物理麦）。**启动时切**，因为游戏在启动时读一次就缓存。原值落盘
  `logs/mic-default.json` —— `Stop-Process -Force` 不给进程清理机会，文件是唯一能
  把原值带过下一次启动的东西；下次启动读到它就接管还原责任。`--mic-default-device
  off` 兼作撤销（先还原遗留再什么都不做）。health 的
  `stream.audio.uplink.default_device` 报 `action / policy / device / current /
  previous / restored`，其中 `current` 是切完之后系统自己报的默认录音 —— 只有它能
  把"真的改了"和"以为改了"分开。旁路工具 `toolchain/mic-default.ps1`（`-List` /
  `-Name` / `-Id`）不依赖服务，用于核对与手动回退。

**还剩**：

| 项 | 为什么必须 | 量级 |
|---|---|---|
| ~~P0 WS 上行~~ `0x02` Opus → 解码 → 写目标端点 | 不分传输就永远只有 WebRTC 能用 | **已完成**（#110）：两条传输共用一个 `pump`，见 `stage3-audio-design.md` §8.3 |
| P1 切换默认录音设备 | 游戏/Discord 缓存设备名，不切就选不到虚拟麦（Vibepollo 用 2/5/10/20 s 重试绕开） | **已完成**：启动时切（不是会话开始——游戏在启动时读一次就缓存了），三个 role 一起切，原值落盘 `logs/mic-default.json` 以应对强杀，见 `stage3-audio-design.md` §8.6 |
| P2 Parsec VUSB IOCTL 后端 | 驱动在但没端点；需要逆向文档里的 IOCTL 序列 + 管理员 | 大，按需 |
| P3 Linux 侧 | 那边是 `module-null-sink` + `module-remap-source` 两行命令，随平台移植一起做 | 随移植 |

**回访触发**：出现"游戏内语音 / 远程会议要说话"的实际需求，或 WS 上行开工。
"多人同服语音"不再算回声的回访触发——它是虚拟麦克风的需求，已经含在这条里。

---

## 6. 硬件编码（NVENC / VAAPI）

> 探测与「决议」的设计见 `hardware-encoding.md`。本文补的是**真正接线之后**的设计。
> 那份文档 §5 写「只有 `Backend::Hardware` 那一支要改」——2026-11-05 逐层核对后
> **不成立**：断点是「四处语义 + 一道传输层缺口 + 一次依赖 vendor 化」，见 6.2 / 6.3。

### 6.1 先分清维度：这四个不是同类项

| 名字 | 是什么 | 在本项目里产出 | 现状 |
| --- | --- | --- | --- |
| VP8 / VP9 | **codec**（编出什么格式） | libvpx 软编的 VP8/VP9 码流 | 已上线，面板可切 |
| NVENC / VAAPI | **encoder API**（谁来算） | ffmpeg 的 H.264 / HEVC | 只探测，从未编译进来 |

所以「NVENC vs VP8」不是二选一，是「硬编 H.264」vs「软编 VP8」。硬编做不出
VP8/VP9，软编也接不了 NVENC——两者的产物在传输层是两个不同的 codec_id。

### 6.2 现状：三层各断一处

| 层 | NVENC | VAAPI | 证据 |
| --- | --- | --- | --- |
| 项目层探测 | ✅ 有（PCI id + 名字分档 + `nvEncodeAPI64.dll` 存在性） | ❌ 无。枚举走 `EnumDisplayDevicesW`，整份文件是 Windows 专有 | `platform/hwenc.rs:460-507` |
| 决议 | ❌ 三个分支全返回 `Backend::Software`，`Backend::Hardware` 全仓无构造点 | 同 | `platform/hwenc.rs:292-368` |
| 中间层封装 | ⚠️ `HwRamEncoder` 完整，但 `cfg(feature="hwcodec")` 关闭 | 同 | `vendor/scrap/src/common/codec.rs:53-56` |
| native | ✅ 依赖里有 `h264_nvenc`/`hevc_nvenc` | ✅ 有 `h264_vaapi`（`hevc_vaapi` 上游已注释，理由「某台机器画质差」） | `hwcodec/src/ffmpeg_ram/encode.rs:218-260` |

顺带一句：`hwenc.rs:318` 和 `hardware-encoding.md:87` 提示的
`--features hwcodec` **跑不通**——`stream-server` 没有 `[features]` 段，实测
`cargo build -p stream-server --features hwcodec` 直接报
`none of the selected packages contains these features`。真做时要加一个转发 feature。

### 6.3 硬编和软编不是同一个形状：四条语义要对齐

| 缺口 | 软编（vpx） | 硬编（hwcodec） | 不补的后果 | 补法 |
| --- | --- | --- | --- | --- |
| 强制关键帧 | `force_kf` flag，两代通用 | 走 trait 默认 **no-op**（`codec.rs:85`），`HwRamEncoder` 没覆写 | PLI 与 `--keyframe-secs` 定时 IDR 全失效；硬编 gop 是 `INT16_MAX`，丢一次包就花屏到会话结束 | ffmpeg_ram_encode 加 `force_idr` 入口（`frame->pict_type = AV_PICTURE_TYPE_I`），`HwRamEncoder::force_keyframe` 转发 |
| 本轮无输出 | `VpxNoOutput` → `Ok(None)` | 返回 `Err("no valid frame")`（`hwcodec.rs:139-141`） | 每帧计入 `encode_err`（`pipeline/mod.rs:1633`），日志噪音 + 计数器失去意义 | 归一成 `Ok(None)` |
| 改码率 | `set_quality(ratio)` / `set_target_bitrate` | 仅 CBR，`set_target_bitrate` 走默认 `Err` | 面板拖码率对硬编无效，而且**不报错** | 映射到 hwcodec 已有的 `Encoder::set_bitrate` |
| CQ / CRF | 支持（`VP8E_SET_CQ_LEVEL` / `rc_end_usage=CQ`） | 只支持 CBR（`hwcodec.rs:246-252`） | 面板选 CRF 会被静默当成 CBR，画质档位是假的 | 新增能力位，面板在硬编下禁用 CRF |

### 6.4 前置：H.264 通道（P0，可独立验收，与硬编无关）

硬编只产 H.264/H.265（`hwcodec.rs:133-137` 只有这两个分支），而当前 WS 帧头的
`codec_id` 只有 `Vp8=2` / `Vp9=3`（`http/ws_media.rs:122-126`）。**没有这一步，
硬编接上去也没有地方输出**。这一步用**软编 H.264 也能验**，所以它排在硬编前面。

- [ ] wire：`VideoCodec` 加 `H264`，`codec_id` 取值需与前端 `wire-codecs.js` 对齐
      （4 bit 字段，建议 `1`），帧头、`SPS/PPS` 前置、按关键帧判定的逻辑同步
- [ ] 前端：`wire-codecs.js` 加 id 与 `WebCodecs` 的 `avc1.*` 描述（H.264 需要
      `description` = AVCC 的 extradata，VP8/VP9 不需要——这是最容易漏的一条）
- [ ] 面板：`dispatch/output.rs:555` 的 `["vp8enc","vp9enc"]` 加 `h264enc`，
      并给 `codec.rs` 补 `wire()`/`label()`/`encoder_id()`
- [ ] health：`http/health.rs:422/463` 的 `"encoder": "libvpx"` 改成实际后端
- [ ] `pipeline/sample_map.rs` 接受 `H264s` union（不改就是静默黑屏，与 #94 的 VP9 同一个坑）

### 6.5 NVENC（Windows）

**触发条件**：被控机有 GTX 10 系及以上且驱动完整（探测已覆盖），并且 H.264 通道已通。

**数据流**（与软编只差中间一段，抓帧转换不用动）：

```
DXGI 采集 BGRA → convert_to_yuv(NV12, stride/offset 取自 encoder.yuvfmt())
  → HwRamEncoder(h264_nvenc) → VideoFrame{h264s} → sample_map → WS(codec_id=H264)
```

`yuvfmt()` 这条契约现在已经是**编码器说了算**（`pipeline/mod.rs:890/1365/1505`），
NV12 + `HW_STRIDE_ALIGN` 会被自动遵守 —— 这是现有代码里唯一为硬编准备好的地方。

| 改动点 | 改什么 |
| --- | --- |
| `vendor/hwcodec/`（新） | **必须 vendor 进来**。现在它是 `Cargo.lock` 里的 git 依赖（`hwcodec#febec27`），补不了 6.3 那四条；CI 也要确定性 |
| `crates/stream-server/Cargo.toml` | 加转发 feature `hwcodec = ["scrap/hwcodec"]`，修掉 6.2 那条假提示 |
| `pipeline/encoder.rs` | `VideoEncoder` 内部改成 `enum Inner { Vpx(..), Hw(..) }`，`new()` 按 `Backend` 分派；`force_keyframe`/`set_bitrate`/`encode` 三个方法与 6.3 对齐 |
| `platform/hwenc.rs` | `decide()` 的 `Auto` 分支在候选可用时返回 `Backend::Hardware`；`--encoder nvenc` 不再「检测到了还回落」 |
| `pipeline/mod.rs` | Rebuild 判定加一条「后端变了」；`is_hardware()` 进 `stream_info` |
| `docs/` | 6.2 的假提示、「只有一支要改」的结论都要改 |

**并发事实**：消费级 NVIDIA 驱动对同时 NVENC session 数有限制（新版驱动已放宽到 8），
而本项目**一个进程只有一路编码**（共享管线 + fanout）。所以这个限制只影响「一台机器
跑几个实例」，不影响「一路几个人看」——与 `capacity-slicing.md` 的路数预算直接相关，
验收时要单独记一个数。

### 6.6 VAAPI（Linux）

**它的门槛不是编码器，是整个平台。** native 侧已经就绪（设备走
`av_hwdevice_ctx_create(VAAPI)` 默认 `/dev/dri/renderD128`，`sw_pix_fmt=NV12`，
`async_depth=1`），项目侧是零：

- 9 个文件直接调 Win32：`dispatch/win_input.rs`、`cursor.rs`、`display.rs`、
  `clipboard.rs`、`gamepad.rs`、`hostui.rs`、`pipeline/audio.rs`（WASAPI）、
  `platform/hwenc.rs`、`main.rs`；采集侧是 DXGI。
- 所以顺序只能是：**Linux 移植（采集 X11/PipeWire + 注入 evdev/uinput + 音频）→
  再谈 VAAPI**。把 VAAPI 单独立项会得到一个编译不过的 crate。
- 移植完成后要新增的探测（Linux 分支）：`/sys/class/drm/renderD*/device/vendor`
  取厂商，渲染节点优先取 `DRM_RENDER_NODE`，否则第一个 `renderD*`。**仍然不在启动时
  试开编码器**——理由与 `hardware-encoding.md` §2 相同（探测会撒谎），真正的判定放在
  `HwRamEncoder::new` 失败时回落。
- 只用 `h264_vaapi`，不碰 `hevc_vaapi`（上游因画质问题注释掉，没有理由逆着来）。
- VAAPI 不支持改码率（`support_changing_quality()==false`，`hwcodec.rs:196-198`），
  面板要有对应的禁用态，而不是让人拖了没反应。

### 6.7 为什么暂缓

1. **被控机没有 GPU**。`.148` 是 VMware SVGA（`PCI\VEN_15AD&DEV_0405`），探针实测
   `candidate=none ... VMware SVGA 3D [other too-old]`。现在把硬编编进去，得到的只是
   一条「已回落软件」的日志。
2. **H.264 通道还没做**，硬编即使能开也没有输出格式。
3. **在当前瓶颈上，换 codec 比接硬编更划算**。2026-11-05 用 `bench-capture.exe` 在
   开发机实测（1718×926、60 目标、各 8 s、四轮交替）：VP9 每帧 **8.0 ms**、VP8
   **22.7 ms**，实测帧率 30.6 vs 22.6，码率 224 vs 547 kbps —— 与
   `codec-choice.md:86` 的「VP9 约 2x CPU」方向相反。机制上对得上仓库自己的注释：
   VP8 的并行不是 token-partition（`vendor/scrap/src/common/vpxcodec.rs:621`），
   而 VP9 那条分支开了 `row-mt` + `tile-columns` + 屏幕内容工具。**这条数据只在
   开发机成立，被控机是 4 vCPU 的 VM，必须先复测再动默认值。**
4. 硬编的收益只在「CPU 已经打满、且降分辨率不再可接受」时才变现。当前 `.148`
   静态桌面 0.49 核/路，还没到那一步。

### 6.8 回访触发（任一成立即开工）

- [ ] 被控机换物理显卡，或开 vGPU / GPU 直通（NVENC 的前提）
- [ ] 决定用 Linux 主机承载实例（VAAPI 的前提，与平台移植同一次变更）
- [ ] `capacity-slicing.md` 的路数撞墙：单机路数被 CPU 限住，且降分辨率/降帧率都不再可接受
- [ ] 出现 4K、或 >60 fps 的需求（软编在这台机器上已经不够）

### 6.9 开工顺序

- [ ] **P0** H.264 通道（6.4，软编可验）
- [ ] **P0.5** `hwcodec` vendor 进 `vendor/`，打 6.3 的四条补丁
- [ ] **P1** 转发 feature + `VideoEncoder` 分派 + `decide()` 返回 `Backend::Hardware`（NVENC）
- [ ] **P1** 验收：`enc_ms`、`process_cpu_percent`、fps、PLI 重锚（拔网线 3 s 后 1 帧内恢复）、
      定时 IDR（`keyframes` 计数按 `--keyframe-secs` 增长）、改码率生效、连续 2 h 无崩溃
- [ ] **P2** Linux 移植（独立路线，不与本文混）
- [ ] **P2** VAAPI 探测 + 接线 + 同样的验收清单

### 6.10 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| `hwcodec` 是 git 依赖，未 vendored | 补不了 6.3 的四条；构建不可复现 | 先 vendor，再动手 |
| 静态 ffmpeg + C++ 全树 | 部署包显著变大；构建需要 `VCPKG_ROOT`（`hwcodec/build.rs:103` 直接 `unwrap`） | 单独产一个 `stream-server-hw.exe`，别让软件版背上 |
| 消费卡 NVENC 并发 session 上限 | 一机多实例的路数被 GPU 限住 | 验收时记数，进 `capacity-slicing.md` |
| 硬编锁死 H.264 码流 | 前端必须支持 `avc1` + AVCC extradata；老 Safari 之外反而更稳 | P0 就用软编 H.264 把前端这条路验完 |
| VAAPI 画质 | 上游注释 `hevc_vaapi` 就是这个原因 | 只用 h264_vaapi，别碰 hevc |
| 没有 GPU 的机器无法验收 | 只验到「回落正确」 | 验收必须在有卡的机器上做，VM 只能验回落 |

---

## 7. 多用户各自独立实例（虚拟屏 / 多开 / 输入隔离）

> 状态：**已分析清楚，暂缓**。分析日期 2026-11-09，需求原话是"开出多个虚拟屏，
> 让不同用户连接到不同的虚拟屏，同时互相之间输入隔离"，后续收敛为
> "多开程序，把输入发给各自的进程自行处理，不再考虑桌面"。
>
> 本节的价值不在于"将来要做什么"，而在于**把一条看似可行的路堵死并留下证据**：
> 同类需求以后还会被提出，没有这张表就要把同一个分析再做三遍。

### 7.1 需求的三次收窄

| 提法 | 期望 | 收敛后的真实约束 |
| --- | --- | --- |
| 多块虚拟屏 + 不同用户各连一块 | 每人一块自己的屏 | 画面可隔离，**输入不可** |
| 输入发给各自进程自行处理 | 不再经系统输入队列 | 只对"读窗口消息"的程序成立，**游戏多读 Raw Input** |
| 多开同一程序（沙盘等） | 程序之间不打架 | 沙盘只隔离文件/注册表/命名对象，**不隔离输入** |

### 7.2 一条不可绕过的硬约束：键盘没有坐标

| 输入类型 | 能否按屏/按窗口隔离 | 依据 |
| --- | --- | --- |
| 指针绝对定位 | ✅ 已实现 | `win_input.rs` 的 `TargetSpace` + `MOUSEEVENTF_VIRTUALDESK`，含负 `origin` |
| 指针点击 | ⚠️ 毫秒级窗口内会串 | 系统光标只有一个，两次注入之间会被别的会话改写 |
| 滚轮 | ❌ | `WM_MOUSEWHEEL` 发给**前台窗口**，不看坐标下是谁 |
| 键盘 | ❌ | 按键进会话唯一的 input queue，由唯一前台窗口取走 |

所以"两个真人同时打字互不干扰" = **需要 N 个内核输入队列** = N 个会话 / VM /
容器设备命名空间。这跟代码怎么写无关，是 Windows 的会话模型决定的。

**Linux 为什么没有这个问题**：容器内的 `/dev/uinput` 创建设备，只对该容器的
设备命名空间可见（Wolf 的 `inputtino` 就是这么做的）。**Windows 没有"设备命名空间"
这一层** —— HID 设备全局共享，Raw Input 把全部设备广播给所有注册窗口。这是所有
Windows 方案只能退到"会话/VM"的根本原因。

### 7.3 本机实测条件（2026-11-09，`.148`）

| 项 | 实测值 | 含义 |
| --- | --- | --- |
| 系统 | Win11 Pro，build **26200** | WGC、WASAPI process loopback 都支持 |
| `Manufacturer` / `Model` | **VMware, Inc.** / **VMware7,1** | 这台本身就是 VM，SMBIOS 直读可得 |
| `BIOS` / `Serial` | VMware, Inc. / `VMware-56 4d d6 ...` | 同上 |
| `HypervisorPresent` | **True** | 已有一个 hypervisor 层 |
| `VirtualizationFirmwareEnabled` / `SLAT` / `VMMonitorModeExtensions` | **全 False** | **嵌套虚拟化未开放 → Hyper-V / Windows Sandbox 起不来** |
| 内存 | 8 GB，空闲常驻 **3.5 GB**，可用 4.47 GB，**Commit 8.62 GB** | 已经在用页面文件，再加一台 guest 必然换页 |
| CPU | 4 vCPU | 一路 1080p60 软编已吃 45–48% |
| 显示 | 单屏 1920×1080（`\\.\DISPLAY1`） | 多屏需要驱动或 VMware 多显示器 |
| VBS | 0 | 当前未启用（启用后反作弊判定会更糟，见 7.6） |

结论：**这台机器上"每用户一台 VM"连启动条件都不满足**。即使打开嵌套虚拟化，
按 `capacity-slicing.md §3`，8 GB + 全量镜像 `n_max = 1`，瘦身镜像才刚好 2 台，
而那 2 台各自只剩 2.9 GB 可用且**没有 GPU**。

### 7.4 行业把隔离单元放在哪一层

| 形态 | 代表 | 隔离单元 | 输入怎么进各自的世界 |
| --- | --- | --- | --- |
| 云游戏 | GeForce NOW | **一用户一 VM**，GPU 整卡或 2–4 路时间片 | VM 内普通 Win32 输入栈，零技巧 |
| | Xbox Cloud Gaming | **一 SoC 一玩家**（刀片直通） | 等价于一台真主机 |
| | Stadia | **一会话独占 GPU 切片** | 同上 |
| 云桌面 | AVD / RDS / Citrix | **Windows 会话** | RDP 把键鼠作为 PDU 送进目标会话，win32k 保证每会话独立 input queue |
| 开源自建 | Wolf（games-on-whales） | **一会话一容器** + headless Wayland compositor | 容器内 `uinput` 虚拟设备 |
| 多开器 | 各种"多开分身" | **无隔离**，逐进程 hook | 注入 + API hook（被反作弊视为作弊工具） |

共同点：**没有任何一家在"进程层"做输入隔离**。唯一的例外是多开器，而它不被
认可的原因见 7.6。

### 7.5 三条候选路线（本机口径）

| 路线 | 内存模型 | 每实例额外常驻 | 8 GB 能否 2 实例 | 输入隔离 | 结论 |
| --- | --- | --- | --- | --- | --- |
| 每用户一台 VM | 独立内核 + 独立页缓存，**同一份资源占 N 份** | 3.4 GB（全量）/ 1.1 GB（瘦身） | ❌ 全量 1 台；瘦身 2 台但无 GPU | ✅ | 本机不可行 |
| 容器（Wolf 式） | 共享内核 + **共享页缓存，只占一份** | 0.3–0.8 GB | ✅ 宽裕 | ✅ 天然（uinput 命名空间） | 需换 Linux 宿主 |
| Windows 进程级多开 | 共享内核 + image section 共享 | 0.3–1 GB | ✅ 余量最大 | ⚠️ 要 `PostMessage` 或注入 shim | 本机唯一可行 |

"内存重叠"在 VM 模型下不是配置问题，是**结构性重复** —— Hyper-V 没有跨 VM 页共享
（KSM/UKSM 那一类是 Linux 的东西）。而进程/容器共享页缓存，所以同一份游戏资源
只占一份物理内存。这条直接决定了密度，也是本机最终倾向进程级多开的原因。

VM 还有三个独立坑：**动态内存气球在游戏负载下是反效果**（回收→guest 内 page fault
→再从 vmdk 读回，形成 guest 页文件→宿主页文件→虚拟磁盘的三级延迟）；
**Smart Paging 会撞 commit 上限**（现在 8.62 GB 已经 > 物理 8 GB）；**没有 GPU 分区**
（GPU-P / DDA 在这台 VMware SVGA 3D 上都不成立，guest 只能 WARP 软渲染）。

行业反而不超配的原因也在这里：`GPU 3D 吞吐 > NVENC 并发数 > CPU 核数 > 内存`，
游戏负载对帧时间敏感，任何一次换页都变成可见卡顿。所以云游戏的密度靠**硬件分区**
（MIG/SR-IOV/整卡直通）+ **硬件编码**，不靠挤内存。

### 7.6 反作弊：这条线不是技术问题，是授权问题

| 观察 | 后果 |
| --- | --- |
| 这台机器 SMBIOS 直接写着 VMware | 带内核反作弊的游戏第一轮检查就过不去 |
| 裸金属上启用 Hyper-V / VBS 后，BattlEye 也会报 "Virtual machine detected" | 就算换物理机，"用 Hyper-V 多开打网游"同样不成立 |
| GFN 等平台能跑，是因为反作弊厂商把它们**登记为受支持环境**（协议 + 硬件透明化） | 这个资格个人买不到，也不是靠"藏得好" |
| 隐藏 CPUID hypervisor 位 / 伪造 SMBIOS / 抹平时序差异 | 这就是作弊工具的技术栈，**本项目不做**，而且军备竞赛赢不了 |

因此：
- **带内核反作弊的在线游戏** → 只能一人一台**物理机**，与"隔离"无关，因为无解。
- **无内核反作弊**（自建 MC 服、私服、单机联机）→ 上面三条路线都不会碰红线，
  反作弊不是变量。

`capacity-slicing.md` 的切分商品同样受这条约束：**切出来的 VM 天然是 VM**，
租给客户打网游会直接进不去。这一点写进了那份文档 §5.2。

### 7.7 回访触发（任一成立即重新打开）

- [ ] 目标游戏清单里确定**不含内核反作弊**，且出现"第二个真人要同时玩"的实际需求
- [ ] 被控机换成裸金属，且决定"一人一台"（此时 §7.5 全表作废，改为 `capacity-slicing.md` 口径）
- [ ] 决定用 Linux 主机承载实例（与 §6.6 的 VAAPI / 平台移植同一次变更，走 Wolf 式容器）
- [ ] 出现"同一台机器上必须同时开 ≥3 个游戏实例"的硬需求（此时进程级多开的内存优势才会成为决定因素）

### 7.8 若触发，开工顺序

1. **量密度**（零改动）：两个 MC 实例 + 两路流，采三个数 —— 第二个实例的
   commit 增量、CPU 峰值、两实例之间的帧时间抖动。把 §7.5 的估算换成实测。
2. **WGC 按窗口采集**：`vendor/scrap` 是 DXGI **output** duplication（抓显示器），
   要按实例出图必须换 `Windows.Graphics.Capture`（26200 支持；`windows` crate 0.52
   已在依赖里，只加 feature）。约 600–900 行 COM/WinRT，`CaptureSource` 边界已就绪。
   限制：最小化抓不到、独占全屏抓不到；被遮挡**不影响**（抓的是窗口自己的 surface）。
3. **输入后端**：`WinInputSink` 加 `TargetWindow` 分支，走 `PostMessage`；`SendInput`
   原样保留做单实例路径。只对"读窗口消息"的程序有效（Win32/Electron/WPF/GLFW）。
4. **多开器本体**：每实例独立 profile/存档/Java 参数；先确认沙盘只用于文件与命名对象隔离。
5. **（可选，仅无反作弊）shim**：hook `RegisterRawInputDevices` / `GetAsyncKeyState` /
   `DirectInput8Create` + 伪造 `GetForegroundWindow`。只对自用、自建服、量小的场景成立，
   维护成本随游戏更新线性增长。

**中文输入**的准确解法在 2/3 两步之后：系统 IME 挂在**焦点窗口**上，`PostMessage`
不改变焦点，所以必须**把 IME 挪到前端**（浏览器组好字，只上传成品字符串），
服务端用 `WM_CHAR` 序列上屏。可做到"能直接打"，但失去目标程序内的预编辑与候选窗。

### 7.9 明确不做

- 隐藏虚拟机 / 改写 `CPUID` / 伪造 `SMBIOS` / 抹平时序特征 —— 作弊工具技术栈
- 在**同一会话内**伪造多指针（滚轮和键盘做不到，且行为无法向用户解释）
- 用 `termsrv.dll` 补丁 / RDPWrap 破并发（违反 Pro 授权、每次更新即失效、
  还会打乱"有交互式已登录会话"这个采集前提）
- 把"多用户"当成多租户商品功能立项（那是 `capacity-slicing.md` 的题，形态被锁定为 VM）

---

## 8. `main` 分支纯净化

> 登记于 2026-11-17。与 §1–§7 不同，这一项不是产品能力，是仓库结构债。

### 8.1 现状与来由

`main` 的定位是**纯净基线**：一条只含"能跑通的最小可信状态"的线，留作模板，
便于以它为起点改造出别的专门内容（换皮、换场景、派生第二个产品线）。
2026-11-17 为了收口两仓分支一致性，把 `web-game`（S1/S2 游戏托管 + S3 输入锁定，
共 4 个提交）以 `--no-ff` 合入 `main`，`main` 从此成了开发线的镜像。

污染的代价不是"多几个提交"，而是三件事同时失效：

1. **回滚基线没了**。"回到干净起点重来"从一次 `reset` 变成 `reset --hard` +
   `force-push` 级别的破坏性操作。
2. **派生模板不干净**。以 `main` 起新项目会把 MC 托管、kiosk 注册表策略这些与
   新产品无关的东西一并带走。
3. **对照失效**。`main..web-game` 原本能一眼看出"这一轮加了什么"，现在恒为 0。

### 8.2 暂缓期间的临时策略（当前生效）

- 日常开发、提交、合并全部在 `web-game` 上进行，两仓同名。
- `main` **冻结**：不合入、不快进、不从它拉特性分支。它的 tip 停在后端 `ee6331c`、
  前端 `e66f7d3`。
- 有人从 `main` 开分支做特性，视为冻结失效，按 8.4 触发处理。

### 8.3 纯净化方案（待执行）

1. **定锚点**：后端 `dc56df0`（S1 之前最后一个提交，也是原 `origin/main`），
   前端 `3dac1f2`（原 `origin/main`）。两者都在现有历史里，不需要重建。
2. **先留逃生门**：两仓各打 tag 或备份分支（如 `baseline-2026-11`）指向锚点，
   `web-game` 全量保留 —— 纯净化只挪 `main` 这个引用，不删任何提交。
3. **重置并强推** `main` 到锚点（需临时解除 GitHub 分支保护），完成后立即恢复。
4. **固化规则**：`main` 只接受"基线级"改动（构建脚本、许可证、通用文档）；
   特性一律走 `web-game` 或两仓同名的特性分支。
5. **校验**：`main..web-game` 重新等于 S1 之后的全部提交；两仓 `main` 与各自锚点一致；
   两仓当前检出分支名仍然相同。

### 8.4 回访触发（任一成立即开工）

- [ ] S4（工具箱「游戏」页 + 「输入锁定」页）收尾，`web-game` 稳定并打完一个可发布版本
- [ ] 出现"以本仓为模板派生第二个产品线 / 客户版本"的实际需求
- [ ] 有人从 `main` 开分支提交（冻结失效的信号）
- [ ] 需要对外交付"干净基线"（开源、交付第三方二次开发）

### 8.5 明确不做

- 不为了 `main` 好看而 rebase / 重写 `web-game` 的历史
- 不删 `web-game`，不删任何已推送的提交（纯净化只挪引用）
- 不用 orphan commit 重建一个"无历史"的 `main`：指针 1:1、采集链路这些诊断历史
  恰恰是基线最有价值的部分

