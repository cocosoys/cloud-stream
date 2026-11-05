# L3 音频设计稿：游戏声音下行 + 麦克风上行

> 状态：**两个方向都已实现**。下行 #77（`6a095e2`），上行与开关 #79/#80。
> 已落地：WASAPI loopback 采集 + Opus 48k/立体声 + `audio-0` 轨道 + recvonly 麦克风
> m-line 预留；`on_track` → Opus 解码 → WASAPI render 的上行；`START_AUDIO`/
> `STOP_AUDIO` 与 `ab,<N>` 的接线。
> **麦克风去向（#109，见 §8.2）**：注入**虚拟线缆的 render 端**（Steam Streaming
> Microphone / CABLE Input / VoiceMeeter Input），程序从它的 capture 端读到麦克风，
> 回路不存在。`--mic-device default` 才回到"注入默认扬声器"——那只服务对讲，且
> 必定自听（旧实现只有这一条路）。

---

## 1. 目标与非目标

| 项 | 本期 | 说明 |
| --- | --- | --- |
| 游戏声音下行 | ✅ | 被控机系统混音（WASAPI loopback）→ Opus → WebRTC |
| 麦克风上行 | ✅ | 浏览器麦克风 → Opus → 服务端解码 → 被控机播放 |
| 音量/静音控制 | ✅ | 前端已发 `SETTINGS` 里的音频项，服务端接入 |
| 系统声音与麦克风混音监听 | ❌ | 不需要，loopback 已经包含所有渲染流 |
| 多声道 / 环绕声 | ❌ | 立体声 48 kHz，`multiopus` 不实现 |
| 音频设备热插拔 | ❌ | 会话内固定设备，重连生效 |

---

## 2. SDP 形态（决定一切）

前端的约定写在 `webrtc.js:33-37`：

> the audio and video m-lines **it offers recvonly** are reserved as sendonly
> transceivers for the microphone and the webcam, which are attached later
> with `replaceTrack` and no renegotiation.

翻译成 SDP 要求，服务端 offer 里必须**同时存在两条 audio m-line**：

| 序号 | 方向 | 用途 | 前端行为 |
| --- | --- | --- | --- |
| 1 | `a=sendonly` | 游戏声音下行 | 变 `recvonly`，挂到 `<video>` 播放 |
| 2 | `a=recvonly` | 麦克风上行 | 变 `sendonly`，`_prepareUplinkTransceivers()` 预留，`replaceTrack` 挂麦克风 |

对应 webrtc-rs 的两个调用：

```rust
// 1. 下行：加一条 audio track，自动生成 m=audio sendonly
let audio_sender = pc.add_track(audio_track.as_track_local()).await?;

// 2. 上行：显式开一条 recvonly transceiver
pc.add_transceiver_from_kind(
    RTPCodecType::Audio,
    Some(RTCRtpTransceiverInit {
        direction: RTCRtpTransceiverDirection::Recvonly,
        send_encodings: vec![],
    }),
).await?;
```

**顺序不能反**：前端 `_prepareUplinkTransceivers()` 按 m-line 顺序取
"第一条 recvonly 的 audio"，下行那条是 sendonly，不会被误取。但若把 recvonly 放在
前面，下行音频会被当成麦克风槽位——SDP 里两条 m-line 的顺序就是语义。

---

## 3. 下行：采集 → 编码 → 打包

### 3.1 采集（WASAPI loopback）

被控机实测有活跃渲染端点（`MMDevices\Audio\Render\{1ae2d9c8-...}`，
`DeviceState=0x1`），`Audiosrv` 自启，`vmware` 虚拟声卡可用。

```
IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)
  → IAudioClient::Initialize(AUDCLNT_SHAREMODE_SHARED,
                             AUDCLNT_STREAMFLAGS_LOOPBACK | EVENTCALLBACK,
                             200ms buffer, 0, format, null)
  → IAudioCaptureClient::GetBuffer / ReleaseBuffer
```

要点：

- **loopback 采集的是"混音后"信号**，包含游戏、系统提示音、其他程序。这正是云游戏要的。
- **必须有渲染流才有数据**。系统完全静音/无播放时 `GetBuffer` 返回 0 字节，
  这是正常现象（`AUDCLNT_S_BUFFER_EMPTY`），不是故障——统计里要区分"静音"和"采集卡死"。
- 格式取 **48 kHz / 16-bit / 立体声**。设备原生若是 44.1 kHz，WASAPI 共享模式会
  自动做 SRC；显式要求 48 kHz 可避免自己在 Rust 侧重采样。
- 事件驱动（`SetEventHandle` + `WaitForSingleObject`）比轮询省 CPU，且抖动更小。

实现选择：

| 方案 | 代码量 | 风险 |
| --- | --- | --- |
| A. `wasapi` crate 0.26 | ~150 行 | 需确认它的 `initialize_client` 是否暴露 loopback flag；不暴露就得 fork |
| B. `windows-sys` 手写 COM | ~350 行 | 无第三方语义风险，但样板多、容易漏 `CoInitializeEx(MTA)` |

倾向 **先试 A，失败退 B**。B 的确定性更高，A 的收益是省 200 行。

### 3.2 编码（Opus）

| 参数 | 取值 | 理由 |
| --- | --- | --- |
| 采样率 | 48000 | Opus 原生，避免重采样 |
| 声道 | 2 | 游戏立体声；单声道会丢定位 |
| 帧长 | 20 ms | WebRTC 默认 `ptime`，也是丢包恢复粒度 |
| 码率 | 64 kbps VBR | 音乐/音效够用；静音时 VBR 自动降到 ~8 kbps |
| complexity | 5 | 虚拟机 4 vCPU，10 是浪费 |
| in-band FEC | 开 | 内网也可能丢包，FEC 比 NACK 便宜 |
| DTX | 关 | 游戏音效有持续低音，DTX 会截断尾音 |

编码器依赖 **libopus**（无纯 Rust 实现）：

- 首选 `audiopus` crate（安全封装）+ vcpkg 已编译的 `opus:x64-windows-static`
- `audiopus_sys` 的 build script 需要 `LIBOPUS_LIB_DIR` / `LIBOPUS_INCLUDE_DIR`
  指向 vcpkg 的 `installed/x64-windows-static/{lib,include}`，否则会尝试自己下载源码编译
- **这是全链路唯一的构建风险点**：vcpkg 装 opus 需要从 GitHub 拉源码，
  当前网络对 crates.io 都有超时，成功率要实测

降级路径（若 libopus 装不上）：G.711 PCMU（`MediaEngine` 默认已注册），
零依赖、8 kHz、64 kbps、电话音质。能用但游戏音效会明显发闷，只作为兜底。

### 3.3 打包

复用现有 `TrackLocalStaticSample` 模式（`webrtc/track.rs`），只换能力声明：

```rust
RTCRtpCodecCapability {
    mime_type: "audio/opus".to_owned(),
    clock_rate: 48_000,
    channels: 2,
    sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_owned(),
    rtcp_feedback: vec![], // 音频不需要 PLI/NACK 反馈链
}
```

`write_sample(data, Duration::from_millis(20))`，标签 `audio-0` / `stream-0`。

**节拍不能挂在视频循环上**。视频是 `WouldBlock` 驱动的（静止桌面时几乎不产帧），
音频若跟着它走会直接断流。音频需要**自己的 tokio 任务**：`WaitForSingleObject`
拿到事件 → 读一帧 → 编码 → `write_sample`，20 ms 一次，与视频完全解耦。

---

## 4. 上行：麦克风

```
浏览器 getUserMedia(audio) → replaceTrack 到 recvonly 那条 m-line
  → 服务端 on_track 收到 RTP
  → TrackRemote::read_rtp → Opus 解码
  → WASAPI render（默认输出设备，共享模式）
```

要点：

- 服务端要**解码**再播放。`SendInput` 式的"透传"在这里不存在，
  必须走 libopus decoder（同一个库，`audiopus::coder::Decoder`）。
- 播放端用 WASAPI 共享模式 + 事件驱动，缓冲 100~200 ms。
- **回声**：如果被控机的扬声器声音又被麦克风拾到，会形成回路。内网云游戏场景下
  玩家用耳机通常没问题；不做 AEC（WebRTC 的 AEC 在浏览器侧，下行到被控机的
  声音浏览器不知道）。设计上先不处理，但要在文档里写明这一限制。
- 静音开关：前端 `SETTINGS` 里已有音频项，服务端收到后停 `read_rtp` 或丢包即可。

---

## 5. 与现有模块的接缝

| 位置 | 改动 |
| --- | --- |
| `webrtc/track.rs` | 新增 `AudioTrack`，与 `VideoTrack` 并列；能力换成 opus |
| `webrtc/mod.rs` | `MediaSession::new()` 里加 `add_track(audio)` + `add_transceiver_from_kind(Audio, Recvonly)`；`close()` 停音频任务 |
| 新 `pipeline/audio.rs` | loopback 采集线程 + Opus 编码 + `write_sample`，节拍自持 |
| 新 `dispatch/mic.rs` | `on_track` → 解码 → WASAPI render |
| `dispatch/commands.rs` | `START_AUDIO` / `STOP_AUDIO` 从 `Ignore` 改为真实启停 |
| `dispatch/output.rs` | `stream_info` 里 `audio` 字段从缺省改为真实编码器信息 |
| `signaling/websession.rs` | 会话清理时停音频任务（与视频同生命周期） |

---

## 6. 验收标准

| 项 | 判据 |
| --- | --- |
| 能听到声音 | 游戏内 BGM/音效在浏览器端可闻，与画面同步 |
| 静音不报错 | 系统无播放时采集返回 0 字节，日志不刷错误，不中断流 |
| 带宽 | 有声时 ~64 kbps，静音时 <10 kbps（VBR 生效） |
| CPU | 4 vCPU 虚拟机，采集+编码增量 <8% 整机 |
| 延迟 | 音画差 <150 ms（`ping`/`latency_measurement` 上线后可量化） |
| 拉流稳定性 | 连续 10 分钟无爆音、无累积漂移（漂移会造成音画越走越偏） |
| 麦克风 | 浏览器说话能在被控机播放；静音开关生效 |

其中**"无累积漂移"是最容易漏的一条**：音频按 20 ms 固定节拍产出，
若采集侧时钟比真实时间快 0.1%，十分钟后就会多出 600 ms 的积压。
必须在采集侧以"实际拿到的采样数"驱动节拍，而不是用 `tokio::time::interval`
死循环——后者会在设备时钟与系统时钟不一致时越积越多。

---

## 7. 分三步交付

| 步 | 内容 | 可独立验收 |
| --- | --- | --- |
| 1 | 依赖落地：libopus 构建 + `audiopus` 编码一个固定正弦波 | 能 `cargo build`，产出有效 Opus 包 |
| 2 | 下行闭环：loopback 采集 + 编码 + track + SDP | 浏览器能听到系统声音 |
| 3 | 上行闭环：recvonly m-line + 解码 + WASAPI render | 麦克风可用 |

第 1 步是**风险闸门**。若 libopus 在当前网络/工具链下装不出来，整条路线要么改
G.711 兜底，要么 vendor 一份预编译的 `opus.lib` 进仓库（与 `vendor/` 下其他
魔改依赖同样的处理方式）。在动手写采集之前必须先过这一关。

三步均已落地。上行（第 3 步）的实现取舍记在 §8。

---

## 8. 上行为什么是两个线程，以及那个绕不过去的回声

### 8.1 两个线程

`TrackRemote::read_rtp` 会**一直阻塞到下一个包**。单线程同时做"读网络"和"喂设备"
只有两种写法，都很糟：

- 让读一直阻塞 → 没人说话时没人喂设备，端点欠载，第一句话被切头；
- 给读加 5 ms 超时 → 每分钟 200 次把 `RTPReadStream::recv` 的 future 丢掉，
  而被丢掉的 future 可能已经把包从流里取出来了，等于随机丢音。

所以拆成两个：`audio-mic-rx` 永久阻塞读、把 `(seq, payload)` 丢进容量 64 的有界
通道；`audio-mic-tx` 每 5 ms 从通道取包、解码、写设备。解码器归写线程所有
（`audiopus::Decoder` 不是 `Send`），读完即弃的旧包不做重排——UDP 内网乱序极少，
而重排引入的延迟是确定的代价。

播放侧三条规则：

1. **预滚 60 ms 才开始播**，否则第一句话每次都要重新缓冲；
2. **队列超过 500 ms 就裁回 60 ms**，保留最新而不是最旧——网络突发应该"跳一下"
   而不是"慢一路"；
3. **不够就补零**。设备要多少给多少，欠载只掉这一帧，不产生爆音。

丢包用 Opus 的 PLC 补，一个缺失包补一帧、最多补 3 帧：长间隔是停顿或闭麦，
不是丢包，凭空造 30 帧噪声比静音更糟。

### 8.2 回声：换成虚拟录音设备，回路就不存在（#109）

最初的做法是把麦克风注入**默认 render 端点**，而下行 loopback 采集的正是同一个端点的
后混音信号：

```
浏览器麦克风 → 服务端注入默认扬声器 → loopback 采到 → 编码下行 → 浏览器播放
                                                                    ↑
                                                          玩家听到自己，延迟一个往返
```

这不是 bug，是拓扑：注入在混音**之前**，采集在混音**之后**，没有算法能把两者分离。
按代码常量算的往返是 150–330 ms（麦克风帧 20 + 取包 ≤5 + 设备缓冲 100 + loopback
周期 10 + 下行帧 20 + 浏览器抖动缓冲 50–150 + 输出 20–40）。

**决议（2026-11）**：不修延迟、不做 AEC，改注入目标。麦克风写到**虚拟线缆的 render
端**，程序从它的 capture 端读到麦克风；注入信号从此不经过默认端点，回路不可达。

| 提供方 | 写入 | 程序选它当麦克风 | 装什么 |
|---|---|---|---|
| Steam | `Steam Streaming Microphone` | 同名 | Steam 自带 |
| VB-CABLE | `CABLE Input` | `CABLE Output` | VB-Audio Virtual Cable |
| VoiceMeeter | `VoiceMeeter Input` | `VoiceMeeter Output` | VoiceMeeter |

探测与决议在 `platform/virtual_mic.rs`，`--mic-device` 可选 `auto`（默认）、`default`、
家族名或具体端点名。`default` 保留旧行为给"对讲"（就是要从被控机扬声器出声、接受
自听），其余情况下自听不应再出现。

两条硬规则写进了代码：

1. **虚拟线缆不能同时是默认播放端点**。是的话决议判 `usable: false` —— 那会让 loopback
   采到我们写进去的东西，自听以"看起来全都装对了"的方式复发。
2. **没有可用设备时不静默退回默认扬声器**。退回等于悄悄把回路接回来；宁可让上行失败
   并说明装什么。

历史：这条限制的来龙去脉见 `roadmap-future.md` §5；上游 Selkies 的做法是 PulseAudio
的 null-sink + virtual-source（PR #188），本项目的分歧正在于此。

### 8.3 WS 上行：两条传输共用一个"泵"（#110）

在 #110 之前，上行只存在于 WebRTC（`TrackRemote` → Opus → 设备），WS 传输收到
麦克风帧只回一句 `MICROPHONE_DISABLED`。现在两条传输都接了，实现上刻意不让它们分叉：

```
WebRTC: TrackRemote --(RTP 包)--> ┐
                                  ├─> pump(stop, rx, idle) ─> Opus 解码 ─> 写端点
WS:     `0x02` + Opus -----------> ┘
```

差别只在两处参数，不在算法：

| | WebRTC | WS |
|---|---|---|
| 数据源 | `TrackRemote::read_rtp` | 页面二进制帧，首字节 `0x02` 后面全是 Opus |
| 队列深度 | 64 包（1.28 s） | 8 包（160 ms）—— WS 不会乱序，迟到的语音比丢掉更糟 |
| 设备开关 | 会话在就开着 | 第一包到达才开，空闲 1.5 s 后关 |
| 丢包补偿 | 用 RTP 序号伪造 ≤3 帧 | 序号本地递增，永远不触发 |

**页面的编码参数必须对上**：`selkies-core` 的 mic worker 写死 **24 kHz 单声道、
20 ms 一帧**（`AudioEncoder` 无 WASM 回落时也是同一组参数）。设备要的是 48 kHz 立体声。
不需要在服务端再写重采样器 —— Opus 允许解码到与编码不同的采样率与声道数，解码器自己
重采样并把单声道复制到双声道。这条假设由 `a_24k_mono_packet_decodes_into_one_48k_stereo_frame`
固定住：换掉解码器参数而不改这条测试，症状会是"有声音但音调快一倍"，极难往回追。

页面的拒绝语义没变：目标端点不可用（没装虚拟线缆、或线缆成了默认播放设备）时回
`MICROPHONE_DISABLED`，页面会把麦克风关掉。静默失败会让用户盯着一条白跳的电平条。

### 8.4 设备缓冲：100 ms 是量出来的，但填满它是错的

`Output::open()` 里 `Initialize` 请求 100 ms，实测 `GetBufferSize()` 就返回 4800 帧
（100 ms）—— 这段延迟是真的，不是估算。

但"把 free 全填上"是错的。旧实现每 5 ms 取一次 `free = buffer - padding` 并填满，
缓冲里于是永远是 `数据 20 ms + 静音 80 ms` 的条纹：设备按实时速率消费，100 ms 里只有
前 20 ms 有声音。实测 4.8 s 连续语音被补了 58560 个采样/声道（≈1.2 s）静音，听感是
断续的"哒哒"声。麦克风是每 20 ms 来一包的数据源，不是随时有货的扬声器。

现在两条规则：

1. 只补到 **50 ms** 的排队深度（`RENDER_TARGET_MS`），不填满设备缓冲；
2. 播放中**只写手上真有的帧**，没数据就什么都不写 —— 若照样补到目标深度，静音会始终
   领在数据前面（实测改成 50 ms 后静音反而涨到 123840 个采样）。静音只用于"还没开播"
   时给端点保活。

同一条 4.8 s 语音的复测：数据 240 帧、静音恒为 4 帧（80 ms，只在开播前），不再随
时长增长。副作用是注入延迟从 100 ms 降到 50 ms，对自听也是好事。

### 8.5 虚拟线缆的正向验证：从 CABLE Output 读回波形（2026-11-05）

到这一节为止，"注入成功"的证据只有"写端点的调用没报错"。写调用成功并不等于声音进了
线缆：两头没接上、驱动没搬数据，返回值一样是 0。于是在被控机上补了真正的读数端。

**装了什么**：VB-CABLE Driver Pack 45（`VBCABLE_Setup_x64.exe -i -h`，需管理员，
不用重启；驱动包 `vbMmeCable64_win10.inf` 3.3.1.7，设备节点 `ROOT\MEDIA\0000`）。

**端点变化**：

| 方向 | 之前 | 之后 |
|---|---|---|
| 播放 | 扬声器 (High Definition Audio Device) | 多出 `CABLE Input`、`CABLE In 16ch` |
| 录音 | 麦克风 (High Definition Audio Device) | 多出 `CABLE Output` |
| 默认播放 | 扬声器 | **不变**（线缆绝不能抢默认，见 §8.2 硬规则 1） |

`--mic-device auto` 的决议（health 里可直接看）：

```json
{"provider":"vb-cable","device":"CABLE Input (VB-Audio Virtual Cable)",
 "capture":"CABLE Output (VB-Audio Virtual Cable)","usable":true,"echoes":false}
```

**端到端读数**：WS 推 251 帧 24 kHz 单声道 Opus（ffmpeg 生成的 440 Hz 正弦），同时
`mic-probe.exe --capture CABLE` 从 `CABLE Output` 抓 16 s：

```
t=  8.6s rms= -37.9 dBFS peak= -18.0 dBFS   <- 首帧落地
t=  9.1s rms= -21.8 dBFS peak= -17.4 dBFS
...
total: 16000 ms captured, 7580 ms above -50 dBFS
VERDICT=SIGNAL
```

没开麦时同一端点是 `-96.7 dBFS`（驱动的抖动底噪）。**"有包"不等于"有声音"**：只看
`GetBuffer` 有没有返回数据，静音线缆也一样通过，所以判定必须看电平（-50 dBFS）。

信号持续 7.1 s，与客户端实际发送跨度 6.7 s 对得上 —— 差值来自探针客户端（`await send
+ sleep(20ms)` 实测 26.7 ms/帧），不是服务端拉长或压缩了时间轴。

**两个工具**（都在仓库里，装完线缆的机器上先跑第一个）：

- `crates/stream-server/src/bin/mic-probe.rs` → `mic-probe.exe [--list] [--capture SUBSTR]
  [--seconds N]`：从录音端点抓 N 秒，每 500 ms 报 rms/peak，最后一行
  `VERDICT=SIGNAL|SILENT`，可直接 grep。
- `toolchain/mic-push.py`：像页面那样发 `[0x02] + Opus`。默认 `exclusive` 策略下，一条
  新连接若已有页签持租约就是 viewer，`0x02` 会被回 `MICROPHONE_DISABLED` —— 脚本因此先
  用 `POST /api/v1/control/session/take/<id>` 借走租约，测完还给原来那条。日志里
  `microphone refused (grant=viewer, operates=yes, lease=no)` 这一行就是这件事的指纹：
  `grant` 是令牌角色、`operates` 是能不能动手、`lease` 是租约在不在手上，三者必须分开看。

