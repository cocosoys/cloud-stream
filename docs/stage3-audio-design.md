# L3 音频设计稿：游戏声音下行 + 麦克风上行

> 状态：**设计，未实现**。这是 L3 完善的第一项（P0）。
> 现状：服务端没有任何 audio m-line，`START_AUDIO` 在 `commands.rs:78` 直接 `Ignore`。
> 前端 audio 区块（设备选择、码率、麦克风）因此在空转。

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
