# L3 补齐：二进制剪贴板 / QoS 自适应 / 音画同步

> 状态：**#85 二进制剪贴板、#84 QoS、#83 音画同步三项已实现并部署**。
> 基线 #81，`cargo test -p stream-server` **67 通过 / 0 失败**（新增 26 条）。
> 对应 `roadmap-l2-l3-gaps.md` 的 #18 / #25 / #26。

三项看起来无关，其实是同一类问题的三个面：**协议两端都在，缺的是中间那一段判断**。

---

## 1. 二进制剪贴板（#18）

### 1.1 两个此前没被发现的断点

**断点一：下行根本没被这个页面消费。**
服务端一直发 `clipboardContentUpdate`，而 `selkies-wr-core.js:3470` 的
`onclipboardcontent` 只认 `clipboard-msg` / `clipboard-msg-start` /
`clipboard-msg-data` / `clipboard-msg-end`，其他一律 `console.warn("Unknown
clipboard cmd received")`。所以**在这套前端上，连文本下行都是死的**——不只是
二进制。旧消息只有 WebSocket 版核心（`selkies-ws-core.js:825`）在用。

**断点二：格式不通。** Windows 应用交换 `CF_DIB`（`BITMAPINFOHEADER` + 倒序像素），
浏览器写读的是 `image/png`。Windows 没有任何 API 在两者之间转换。

### 1.2 落地形态

```
上行：cb/cbs+cbd+cbe (image/png) -> decode_png -> to_dib -> CF_DIB
下行：CF_DIB -> dib_to_bmp -> clipboard-msg* (image/bmp) -> 浏览器 toPng 自转 PNG
```

- **下行不转 PNG**：PNG 编码器又是几百行，而浏览器已经有 `reencodeBlobAsPng`。
  发 BMP 让它自己转，是这一层最省的一条路。
- **上行必须转 DIB**：`CF_DIB` 是画图/Word/资源管理器都认的格式，写 `image/png`
  到注册格式只有少数应用支持。
- **`image/bmp` 的 `kindOf` 归类**：前端 `clipboard-sync.js:820` 用
  `mime.startsWith('image/')`，所以 `image/bmp` 会被当图片处理并自动转 PNG。
  如果只认 `image/png` 这条路就不通了。
- **文本优先**：剪贴板同时有文本和 DIB 时发文本。从 Excel 复制区域两样都有，
  用户要的是表格不是那半张图。
- **分片阈值 36 KiB 原始字节**：base64 后 48 KiB，加 JSON 外壳仍在 64 KiB 的
  data-channel 消息上限内。超过就走 `clipboard-msg-start/-data/-end`，
  `total_size` 用**原始字节数**（前端 `receivedSize` 比对的是解码后大小）。

### 1.3 解码器的边界

`dispatch/clipboard_image.rs` 只做「截图/画布会产出」的那一档：

| 支持 | 不支持（记一行 warn 并丢弃） |
| --- | --- |
| PNG 8-bit，colour type 0/2/3/4/6，非隔行 | 16-bit、隔行、CMYK |
| BMP / DIB 24-bit、32-bit `BI_RGB` | RLE4/RLE6、`BI_BITFIELDS` |

拒绝而不是猜，是因为**半解码的图片粘出来比空剪贴板更糟**：一张错位或花屏的图会
让人以为程序坏了，而空剪贴板只表示"没东西"。

32-bit DIB 的 alpha 有一个坑：`BI_RGB` 规定第四字节"未使用"，很多程序写 0。
直接当 alpha 会粘出一块全透明矩形，所以 `dib[o+3] == 0` 时按不透明处理
（`zero_alpha_dib_is_treated_as_opaque`）。

---

## 2. QoS 自适应码率（#25）

### 2.1 此前的状态

码率只有一个来源：客户端 `vb,N` 或 `--bitrate`。链路拥塞时速率**一点不变**，
画面糊成马赛克，PLI 一个接一个，直到人手动拖滑块。`stream_stats.throttled`
恒为 `false`——因为没有任何东西可以问。

### 2.2 两个输入，都是已经测好的

| 信号 | 来源 | 含义 |
| --- | --- | --- |
| `send_ms - pipeline_ms` | #78 加的 `send_timing` / `pipeline_timing` | 帧在交接队列里等的时间。`pipeline_ms` 停在编码器出口，差值就是链路吸收不掉的部分 |
| PLI/s | `pli_requests` 会话总数差分 | 解码器丢了参考链。**只是一半证据**——#119 证明它单独出现时说的是客户端，不是链路（见 2.4） |

**客户端上报的丢包率没有用**，这是一个选择而非遗漏：下行丢包只有接收方
`getStats()` 看得见，服务端要用就得再开一条上行命令；而队列等待已经能回答
「链路是不是跟不上」，多一个可能反了的样本只会白降码率。

### 2.3 状态机

```
Normal --(backlog>=120ms)-------------------------------> Degraded   target *= 0.75
Normal --(backlog>=20ms 且 PLI>=2/s)--------------------> Degraded   ← #119
Degraded --(持续压力)--> 继续 *= 0.75，到 floor 为止
Degraded --(backlog<=40ms 且 PLI<0.5/s)--> Recovering
Recovering --(连续 2 个平静窗口)--> target *= 1.10，回到 base 即 Normal
```

约束：

- **2 秒一次决策**，不是每帧。队列要建起来才看得见，一帧的决定只会是噪声。
- **单步 -25% / +10%**：降要快（用户先看到"能用"），升要慢（避免糊-清-糊振荡）。
- **floor = max(base × 20%, 300 kbps)**。低于这个值的码率确实能让链路空闲，
  但画面也没有信息了。
- **Recovering 只升不降**：刚松一口气的那个窗口不能立刻被下一个积压打回去。
- **新 base 重置一切**：fps 变化或 `vb,N` 到来时，控制器从新 base 重新开始，否则
  用户刚拖大的滑块会被旧的降档值静默吃掉。

`throttled` 现在是真的：`PipelineHandle` 里一个 `AtomicBool`，由 capture loop 每
2 秒写，`stream_stats` 每 1 秒读。HUD 加了一行 `QoS: full rate / throttled`——
不然面板上只会看到 `Video` 跟着 `Target` 往下掉，看起来像编码器坏了。

### 2.4 #119：PLI 单独不构成链路证据

**症状**：`/api/v1/health` 报 `stream.throttled: true`，`target_kbps` 却停在
4527 —— 两个数字互相矛盾，面板上看起来像编码器自己坏了。

**日志原样**（2026-11-09，`.148`）：

```text
qos: degraded -> target 3750 of 5000 kbps (backlog 0 ms, 10 PLI in 2.0s)
qos: degraded -> target 2109 of 5000 kbps (backlog 0 ms,  7 PLI in 2.0s)
qos: degraded -> target 1581 of 5000 kbps (backlog 0 ms,  6 PLI in 2.0s)
```

三条独立证据指向同一个结论——**这不是拥塞**：

| 证据 | 数值 | 说明 |
| --- | --- | --- |
| `backlog` | 每一条都是 **0 ms** | 交接队列一秒都没堆过 |
| 降码率后的 PLI | 降到 1581 仍是 6 PLI/2s | **降码率对丢包毫无影响**，物理上排除"链路丢包" |
| PLI 来源 | 500 行日志里 127 条 `stream_server::webrtc: received PLI` | 全来自 **WebRTC 的 RTCP** |

第三条是根因：`pli_requests` 是**管线级**计数，而 `note_keyframe_request()` 只有
`webrtc/mod.rs:372` 一处调用（WS 侧的 `REQUEST_KEYFRAME` 从不上报）。于是一路
WebRTC 会话的解码器问题，被算到同一条管线上**所有**会话头上——包括那条
`backlog` 恒为 0 的 WS 会话。`encoder.set_bitrate_kbps()` 同样是管线级的，所以
降下去的码率也落在所有人身上。

**修法**：两个信号从"或"改成"队列是必要条件"。

```rust
let backlog_pressure = sample.backlog_ms >= DEGRADE_BACKLOG_MS;
let pli_pressure =
    pli_rate >= DEGRADE_PLI_PER_SEC && sample.backlog_ms >= PLI_BACKLOG_FLOOR_MS;
let pressure = backlog_pressure || pli_pressure;
```

`PLI_BACKLOG_FLOOR_MS = 20` 的依据：健康会话的 backlog 常年是 0，而真丢包时
pacer 不会瞬间吐完，`send_ms` 会先涨到一个肉眼可分的量级。20 ms 是"确实在排队、
但还没到 120 ms 那一步"的边界——给真丢包留了响应路径，同时把"客户端解码器在
要关键帧"挡在门外。

**验证**（部署 pid 9224，同样 2 个客户端、同样 1920×1080@60 VP9）：

| | 修复前 | 修复后 |
| --- | --- | --- |
| `throttled` | `true` | `false` |
| `target_kbps` | 被压到 1581（面板显示 4527） | 稳定 5000 |
| `qos:` 降级日志 | 每 2 秒一条 | **一条都没有** |
| `pli_requests` 增量 | 5–10 次/2s | 1 次/15s |
| 关键帧 | 正常 | 仍是 `kf=0..3`/5s，周期 IDR 未受影响 |

**刻意没改的部分**：`calm` 判据不动。降级一旦发生，PLI 高仍然阻止恢复——那时队列
确实堆过，PLI 高说明丢包还在，不该急着松手。改的是"要不要因为 PLI **进入**降级"，
不是"要不要因为 PLI **留在**降级"。

**同轮修掉的口径 bug（#120）**：上表那句"被压到 1581（面板显示 4527）"里，矛盾
不在控制器，也不在面板——`health.stream.target_kbps` 读的 `encoder.bitrate()` 和
控制器内部的 `target_kbps` **本来就是两个量**：前者是 libvpx 按 `base × ratio` 算出
的配置值（CBR 下 `set_bitrate_kbps` 走的是 `set_quality`），后者是"想让流跑多快"。
用一个字段承载两个量，等于把"降档了"和"没降档"混成同一个数字。

现在拆开：

| 字段 | 来自 | 含义 |
| --- | --- | --- |
| `target_kbps` | `qos.target_kbps()` / `vb,` 覆盖 / fps 缩放预算 | **决策**，`throttled` 就是拿它判的 |
| `encoder_kbps` | `encoder.bitrate()` | **自报**，改档有没有落下去 |

相等 = 落地；不等 = 被拒或被量化。`stream_stats` 也带 `encoder_kbps`（新键，旧面板
不认识就忽略，不会画错）。写入点只有三处：QoS 步进、`vb,` 覆盖、fps 重基准——都在
做出决策的地方，而不是在"每帧读一次编码器"的地方。

---

## 3. 音画同步（#26）

### 3.1 第一版是错的，真实日志一眼看出来

第一版比较的是**两路介质时钟**：音频每帧推进 20 ms、视频每帧推进 `spf`，
"音频超前就丢一帧"。部署到 148 后日志是这样：

```
pipeline 5.0s: ... av=-16199ms/40fix
```

一分钟内 40 次校正、偏斜负 16 秒。原因不是实现错，是**前提错**：

**这条管线不发固定帧率的视频。** 它"变化才发"——静态桌面 1–2 帧/秒
（repeat 还被 `--repeat-every` 限流），而音频严格 50 帧/秒。视频的介质时钟推进速度
取决于桌面变化快慢，跟墙钟无关；`write_sample(data, spf)` 对每帧都盖 `spf`，
所以 RTP 时间戳在这里是**解码器增量**，不是时间轴。

拿两路介质时钟相比，测到的是"桌面有没有在动"，不是漂移。

### 3.2 真实的约束

音频**确实**是固定媒体速率：一个 loopback 周期一帧 20 ms，每秒 50 帧。能让它偏离
真实时间的只有设备晶振（慢 0.1% 就是十分钟 60 ms、一小时半秒）。

所以正确做法是**把音频的媒体时钟钉在墙钟上**：接收端按媒体时间放音频、按到达时间
放视频，音频媒体时间跟上真实时间，两者就不会分家；跑偏的音频设备会被抓住，
而不是一路累积。

```
偏斜 = audio_media_us - wall_us    （音频首帧为原点）
偏斜 < -80 ms（音频落后） -> 多发一帧静音，媒体时钟加速
偏斜 > +80 ms（音频超前） -> 丢一帧样本，媒体时钟减速
```

**为什么要"多发静音"而不是"少等一会"**：RTP 时间戳只能靠发帧推进，多一帧就是
多 20 ms 媒体时间，这是唯一的加速手段。

**限速**：一格 20 ms / 2 秒，且只在超过 80 ms 时动手。差 30 ms 是听不出来的，
去追它等于拿一个听不出来的误差换一次能听出来的咔哒。

**头 1 秒不校正**：loopback 客户端的第一个周期是缓冲突发，不是时钟样本。

**测试怎么钉住**：`Rig` 模拟一个按 `rate` 出帧的假设备（1.0 准、0.997 慢、
1.003 快），跑 400 秒墙钟，断言慢设备只被 Advance、快设备只被 Retard、
且校正次数不超过 2 秒一次。另有一条 `only_the_wall_clock_matters` 专门锁住
这次的回归：**没有任何视频帧时也必须不产生校正**。

### 3.3 观测

`pipeline 5.0s:` 行尾 `av=+13ms/0fix`。`-` 表示音频还没开始。
部署后实测静态桌面 `+13ms / +22ms / +15ms，0fix`——正是想要的：不动它。

偏斜长期 > 80 ms 且 `fix` 数不涨 → 校正没生效；`fix` 每 2 秒一次 → 设备真跑偏
（虚拟声卡、远程桌面端点常见），应检查端点采样率与 48 kHz 是否整除。

---

## 4. 验收

### 二进制剪贴板

1. 被控机开画图，画点东西 → Ctrl+A/C → 浏览器 Ctrl+V → 出现图片。
   服务端：`REQUEST_CLIPBOARD: image answered (N bytes DIB -> M bytes BMP)`。
2. 浏览器截图 → Ctrl+V → 被控机画图 Ctrl+V → 出图。
   服务端：`clipboard uplink (image/png, single chunk, N bytes)` +
   `clipboard uplink: image/png WxH -> CF_DIB N bytes`。
3. 大图（>36 KiB）走分片：`clipboard uplink start: image/png tid=... total=...`。
4. 文本双向仍然可用（现在走 `clipboard-msg`，不再是那条没人收的旧消息）。

### QoS

用 `clumsy` 或 `tc` 注 3% 丢包 + 200 ms 延迟：

1. 5 秒内 `qos: degraded -> target ...`，面板 `QoS` 变 `throttled`，
   `Video/Target` 一起下降且**画面不再持续花屏**；
2. 撤掉注入后 10–30 秒 `qos: recovering -> ...` 回到 `normal`；
3. 全程 `Keyframes` 不应暴涨（暴涨说明降档没起作用，只是 PLI 在打架）。

### 音画同步

连续播放 30 分钟，开一次声音，比对拍手画面与声音：

1. 偏差 < 100 ms 且**不随时间增长**；
2. `av=` 长期稳定在 ±40 ms 内，`fix` 数不增长或极慢增长；
3. 若 `fix` 每 2 秒一次，说明时钟是真跑偏（虚拟声卡/远程桌面常见），
   此时应检查 WASAPI 端点采样率是否与 48 kHz 不整除。

---

## 5. 已知限制

- **剪贴板只支持图片与文本**：HTML、RTF、文件（`CF_HDROP`）未做。复制文件粘到
  被控机，行为仍然是"旧内容"，不是报错。
- **JPEG/WebP 上行被丢**：浏览器通常会把本地图片统一成 PNG 再发，但源应用给
  JPEG 时会被拒绝并记一行 warn。
- **QoS 不感知上行**：音频码率没有降档逻辑（64 kbps 本来就很小）。
- **音画同步以发送端为准**：接收端 jitter buffer 的排队延迟不参与校正，
  极端网络下两者会有几十毫秒的残差。
