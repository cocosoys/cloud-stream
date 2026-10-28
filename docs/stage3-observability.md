# 阶段 3 · 可观测性（延迟 / 码率 / 关键帧请求）

> 状态：**P1 已实现（#78）**，待实机验收。
> 上游定义：`stage1-design.html` 5.4 `stream_stats`、5.3 白名单消息类型。
> 前端契约根：`C:\Users\user\Desktop\selkies-webclient\addons\selkies-web-core\`。

## 1. 为什么先做这一块

L3「画质与体验」的每一次调参都要回答同一个问题：**慢在哪一段**。在这之前服务端只能
证明自己编了多少帧，证不了三件事：

1. 帧**发出去**了没有 —— `pipeline_ms` 停在编码器出口，交接队列的排队时间完全不可见；
2. 链路**往返**多久 —— 面板上的 RTT 只有 `RTCPeerConnection.getStats()` 一个来源，
   `currentRoundTripTime` 为 null 的引擎上就是空白；
3. 接收端**丢锚**几次 —— PLI 只写进日志文件，不进任何数字。

于是「画面糊 / 一卡一卡」在日志里长得和「编码器慢」一模一样。P1 把这三件事变成数字。

## 2. RTT：协议方向本来就是反的

前端不主动 ping，它**只回答**：

```js
// lib/webrtc.js:775-777
} else if (msg.type === 'ping') {
    this.sendDataChannelMessage("pong," + new Date().getTime() / 1000);
}
```

```js
// lib/webrtc.js:778-781
} else if (msg.type === 'latency_measurement') {
    this.onlatencymeasurement(msg.data.latency_ms);
}
```

`selkies-wr-core.js:3687` 把它当作 `currentRoundTripTime` 为 null 时的兜底。
所以正确的闭环是：

```
服务端 --{"type":"ping"}--> 浏览器
服务端 <--"pong,<epoch_s>"-- 浏览器        （纯文本，control 通道）
服务端 --{"type":"latency_measurement","data":{"latency_ms":N}}--> 浏览器
```

旧实现把这条链路写反了：`classify("pong,...")` 落到 `Action::Unknown`（debug 级日志，
等于丢弃），而 `Action::Pong` 会**给客户端回一个 pong** —— 回给一个什么都没问的人。
结果：从未发出过 ping，`rtt_ms` 从来是空的。

### 实现要点

- `signaling/latency.rs`：`LatencyProbe`，1 s 一个 ping（`PING_INTERVAL`），
  回执用**本地 `Instant`** 计时，不用 payload 里的 `<epoch_s>` —— 那是浏览器时钟，
  相减得到的是时钟偏差，不是往返。
- 平滑：EMA α = 0.3。单样本抖动太大，纯平均又跟不上真实变化。
- 丢掉：发出后 5 s 无回执计一次 `lost`，并解除待回执状态。
- 通道没开时 `send_control` 失败 → `cancel_pending()`，否则「没发出去」会被记成「丢了」。
- 首个回执到来之前 `rtt_ms` **不出现**在 `stream_stats` 里（面板画「–」），
  不伪造 0 ms。

## 3. 码率与交接延迟

`pipeline_ms` 是「采集 → YUV → 编码完成」，`send_ms` 是「采集 → 写进 track」。
两者之差就是**交接队列的等待**，也就是链路跟不上的直接证据。

- 队列元素从 `Bytes` 变成 `Sample { data, beat }`（`pipeline/mod.rs`），
  `beat` 是该帧的采集起点，由 forwarder 在 `write_sample` 返回后计时。
- `video_mbps` 统计的是**成功入队**的负载字节，不是编码器产出。
  一帧因队列满被丢掉时，那个字节数不该出现在码率里。
- `peak_mbps` 是窗口内最宽的 250 ms（`PeakRate`）。1 s 均值会把
  「一个关键帧 + 两个满 P 帧」这种爆发抹平，而爆发才是解码器断链的原因。
  短于 50 ms 的桶直接丢弃：一个 60 KB 关键帧落在 8 ms 里会读成 60 Mbit。
- `target_mbps` 直接读 `encoder.bitrate()`，所以 `vb,<N>` 覆盖和 fps 缩放后的
  重定向都会同时反映在「Video rate x / y」上。

## 4. 字段对照表

服务端 `stream_stats.data` → 前端消费点（`lib/stream-stats-view.js`）。

| 字段 | 口径 | 面板位置 |
| --- | --- | --- |
| `cpu_percent` | 整机占用（全部逻辑核） | CPU 表 |
| `process_cpu_percent` | 本进程占单核 | 仅诊断 |
| `mem_used` / `mem_total` | 主机内存 | 内存表 |
| `encoded_fps` | 窗口内新帧 + 重复帧 | FPS 图 |
| `encode_ms` | 新帧编码均值 | Encode |
| `pipeline_ms` | 采集 → 编码完成 | Capture to encoded |
| `send_ms` / `send_min_ms` / `send_max_ms` | 采集 → 写进 track | Capture to sent（含区间） |
| `video_mbps` | 实际入队负载速率 | Video rate（与 target 并列） |
| `target_mbps` | 编码器当前目标 | 同上，右半边 |
| `peak_mbps` | 最宽 250 ms | Peak, 250 ms |
| `keyframe_requests` | 会话累计 PLI/FIR | Keyframe requests |
| `rtt_ms` | control 通道 EMA 往返 | 仅 WS 传输；WebRTC 走 `currentRoundTripTime`，此字段作兜底 |
| `throttled` | 恒 false（无 QoS，见 P2） | 告警位 |

**丢包不在服务端算。** 下行丢包只有接收方能看见（浏览器 `getStats()` 的
`packetsLost`），服务端若自己报一个就是编的。前端已在客户端样本里填
`packet_loss_percent`，这块不重复实现。

## 5. 验收

1. 打开前端统计面板（会发 `_stats,1`）。
2. 服务端日志应出现每秒一条 ping，无 `write_sample failed`。
3. 面板应看到：`Capture to sent` 有数且 ≥ `Capture to encoded`；`Video rate` 在
   目标值附近抖动；`Keyframe requests` 在静止画面下基本不涨，快速拖动/大范围视角
   变化时缓慢上涨（每次 +1 意味着一次断链重锚）。
4. 拔网线 5 s：`lost` 增长，`rtt_ms` 停在最后一次值；恢复后数字回落。
5. `rtt_ms` 与浏览器 devtools 里 `currentRoundTripTime` 的差应在几毫秒内
   （DC 走 SCTP，比 RTP 略高是正常的）。

## 6. 仍未覆盖

- **QoS / 自适应码率**（P2）：`throttled` 恒 false，码率只按 fps 与客户端请求变化，
  不跟随丢包与 RTT。
- **服务端视角的下行丢包**：RTCP RR 里其实有，但 webrtc-rs 的 RR 解析要额外接线，
  优先级低于 P2。
- **音频统计**：`audio_buffer_ms` 仍为空，音频下行刚通（#77），没有埋点。
