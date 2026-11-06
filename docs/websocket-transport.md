# WebSocket 传输（#106）

画面除了 WebRTC，还能走一条 `wss://<host>:8080/api/websockets`。两条传输读的是
**同一路程序**：一次 DXGI 采集、一次编码，各自挂一个下行。

## 1. 为什么需要它

WebRTC 必须有 UDP，或者一台 TURN 中继。局域网里从来不是问题，一旦要穿反向代理、
云主机安全组、别人的公司网络，这就是硬伤 —— 那些地方通常只给一条 443。

前端这条通路是现成的（`selkies-web-core/selkies-ws-core.js`，10672 行，WebCodecs
解码、分行、音频、剪贴板都在里面），所以服务端要做的不是写客户端，而是**按它认的
格式把帧喂过去**。

## 2. 线上格式

一帧二进制 = **12 字节头 + 裸码流**。头按 `selkies-ws-core.js:2417` 的顺序读：

| 偏移 | 长度 | 含义 |
|---|---|---|
| 0 | 1 | 帧类型。**视频整帧是 `0x04`**；`0x03` 是 JPEG 分行、`0x01` 是音频（见 §2.1）。发错这一个字节，页面会把码流当 Opus 解 —— 见 §8 |
| 1 | 1 | `codec_id << 4 \| is_key`（`lib/wire-codecs.js:23`）：h264=1、vp8=2、vp9=3、av1=4、h265=5 |
| 2 | 2 | frameId，大端 u16，回绕 |
| 4 | 2 | y（行偏移）。整帧恒 0 |
| 6 | 2 | 宽，大端 |
| 8 | 2 | 高，大端 |
| 10 | 2 | reference：本帧依赖的**上一帧** frameId |

### 2.1 帧类型字节是最容易错的一处

前端在 **socket worker** 里分流，比视频 worker 的 `onWire` 更早：先看第一个字节，
`0x01` 直接进音频解码器，`0x03` 走 JPEG 分行，`0x04` 且长度 > 10 才当成视频整帧。
所以视频整帧必须是 `0x04`。这一版之前发的是 `0x01`，症状是"连上了、日志有帧、
页面永远黑"—— 码流被当成 Opus 塞进音频解码器，视频这条路一次也没被走到过。

音频是 `[0x01, flags] + opus`：第二字节低 7 位是 RED 冗余帧数（这里是 0），
最高位 `0x80` 表示静音段；前端 `extractOpusFrames` 直接 `slice(2)`。
一条 loopback 采到的 PCM 只在程序级编码一次，结果按 sink 分发（`AudioFanout`），
和视频的 fan-out 同构 —— 观众数不线性放大 CPU。

`0x03` 是 JPEG 条纹，头只有 6 字节（类型、保留、frameId、y），见 §2.2。

### 2.2 JPEG 条纹（`0x03`）：没有 WebCodecs 时的唯一出路

`0x04` 的 VP8 码流需要 `VideoDecoder`。`ImageDecoder` / `createImageBitmap` 解码
JPEG 则在任何现代引擎上都有 —— 所以当页面报告自己没有 `VideoDecoder` 时（它会把
编码器切成 `jpeg` 并随 `SETTINGS` 发上来），这条连接改走条纹：

```
[0x03, 保留, frameId_hi, frameId_lo, y_hi, y_lo] + 一整段 JPEG
```

- 一帧切成 4 条横带（`JPEG_BANDS`），每条自带 JPEG 头，**同一条帧共用一个 frameId**
- `y` 是这条带顶端在画面里的行号；页面把每条画到后缓冲的 `y` 行，最后一条覆盖到
  底边时整帧才呈现（`stripeMaybePresent`）
- 每条都是完整 JPEG（SOI…EOI），所以丢一条只损失那几行，下一帧就补上 ——
  不像 `0x04` 那样丢一帧会污染整条参考链
- 页面按 `y` 有重排窗口（`JPEG_STRIPE_REORDER_WINDOW`），乱序到达不会花屏

服务端这一侧是**每会话**的，不是全局编码器：

1. `SETTINGS` 里 `encoder: "jpeg"` → `ws_media::Intent { jpeg: Some(true) }`
2. 主循环订阅 `pipeline::raw`（采集循环的 BGRA 旁路），起一个
   `spawn_blocking` 的 JPEG 编码任务，**同时只许一个**在飞
3. 编好的 4 条按 `y` 顺序写回 socket；VP8 下行这条连接直接跳过

所以一个"老浏览器"观众不会把别的观众拖进 JPEG —— 采集与 VP8 编码照旧，多出来的
只是这条连接自己的一次拷贝与一次 JPEG 编码。反过来说，**没有订阅者时旁路是零成本**：
`raw::publish` 先读一个原子计数，为 0 就直接返回。

`server_settings` 在切过去时必须报 `encoder: "jpeg"`（`allowed` 也只留 `jpeg`）：
前端只在 `currentEncoderMode === 'jpeg'` 时处理 `0x03`，报 `vp8enc` 而发 JPEG 的
结果是"每一条都被丢掉"，比黑屏更难查 —— 帧确实到了。

`reference` 不是装饰。前端解码闸门（`lib/decode-gate.js:99`）在
`reference !== frameId` 时才做丢帧跟踪：发现依赖缺席就不解这一帧并要求关键帧。
整帧流里每帧依赖上一帧，所以填 `frameId - 1` 是唯一正确的答案；填 `frameId`
等于关掉这套保护，丢一帧之后画面会一直糊到下一个关键帧。

关键帧位要自己算 —— 编码器知道，但 `Sample { data, beat }` 没把它带过来（`key`
在 `forward_chunks` 里只进了统计）。规则：

| 编码 | 判定 |
|---|---|
| VP8 | 首字节 bit0 == 0（RFC 6386 §9.1） |
| VP9 | 首字节 bit2 == 0（无压缩头的 frame_type） |

VP9 那条容易踩：`bit2` 紧挨着 `profile` 位，取错一位就把每个关键帧都当残差丢掉，
表现是"连上了、永远黑屏"。

## 3. 握手顺序

```
client → GET /api/websockets (Upgrade: websocket)
server → AUTH_SUCCESS,{"role":"controller","slot":1,"credential":"none"}
server → MODE websockets
server → {"type":"server_settings","settings":{...}}
server → {"type":"stream_info","info":{...}}
server → 二进制帧 …
```

四处容易错的细节：

1. **`server_settings` 的形状和 WebRTC 不同。** 数据通道上是
   `{"type":…,"data":{"settings":{…}}}`，WebSocket 上读的是**扁平**的
   `obj.settings`（`selkies-ws-core.js:8644`）。发错一层，页面不会报错 —— 分辨率、
   编码器、帧率全部停在它自己的默认值，下拉框全是假的。
   `ws_media::settings_frame` 就地把 `output::server_settings` 拆一层，单一事实来源
   仍留在 `dispatch/output.rs`。`stream_info`（`obj.info`）与 `stream_stats`
   （`obj.stats`）是同一个坑的另外两个入口，`settings_frame` 的注释里写着为什么
   必须**拆包**而不是复制一份。
2. **`AUTH_SUCCESS` 必须带前缀，且现在无条件发送。** 前端是
   `startsWith('AUTH_SUCCESS,')` 之后 `substring(13)` 再 parse
   （`selkies-ws-core.js:8407`）。发裸 JSON 会落进 `event.data.startsWith('{')`
   那个分支，`obj.type` 是 undefined，被静默丢掉 —— 页面永远 `clientRole === null`，
   `updateControllerSlot` 不调用，viewer 语义与手柄槽位全失效，而画面完全正常。
   现在无口令部署也发（`credential: "none"`）：单用户机器就是控制器，不发只会让
   页面在"未认证"状态里空转。
3. **先身份、再 MODE、最后设置。** 前端收到 MODE 就开始初始化输入与音频，此时
   角色还不知道的话，它会按未认证走一遍再补一遍。
4. **`stream_stats` 只在 `_stats,1` 之后每秒一条。** 面板打开才订阅
   （`lib/stream-stats.js`），关掉就静默；这是 `select!` 的分支守卫，不是分支内的
   判断，所以空闲连接不会每秒醒一次。

## 3.1 页面怎么决定用哪种传输

这条线以前是**构建期钉死**的：`selkies-dashboard/index.html` 里一行
`window.__SELKIES_STREAMING_MODE__ = "webrtc"`。理由当时成立 —— 服务端只有 WebRTC，
而 selkies-core 的硬默认是 WebSockets，新设备会去连一个不存在的端点。

两条都实现之后那行就是多余且有害的：它的优先级**高于** localStorage，用户在面板里
切到 WebSockets、页面重载，又会被它按回 WebRTC。现在的顺序是：

| 来源 | 作用 |
|---|---|
| `?transport=webrtc\|websockets` | 一次性覆盖：写进 localStorage 后**从地址栏移除**（否则重载时又赢了） |
| `GET /api/status` → `current_mode` | 只有这个浏览器**从没选过**时才写进 localStorage |
| localStorage | 用户/上一次的选择，最终生效 |

`/api/status` 同时回 `enable_dual_mode: true`，面板的模式切换按钮据此渲染
（`Sidebar.jsx:2804` 的 `canSwitchMode`）。`server_settings` 里也有同名键 ——
前者让切换在**连上之前**就可见（WebRTC 连不上时这是唯一的退路），后者在流建立后
再确认一次。

`/api/status` 的响应体从纯文本 `ok` 变成了 JSON：

```json
{"status":"ok","current_mode":"websockets","enable_dual_mode":true,
 "transports":["webrtc","websockets"]}
```

探针/看门狗只看状态码，不受影响；`main.jsx` 的 `detectInitialMode()` 以前
`resp.json()` 抛异常被 catch 吞掉，所以那个探测从来没生效过。`current_mode` 的
来源是 `--mode webrtc|websockets`，**#117 起默认 `websockets`**：新设备首屏只占用
页面已经在用的那个 TCP 端口，不必先放通 UDP 段。要让新设备直接走 WebRTC
（需要 NACK/PLI/FEC 的那类链路），启动参数写 `--mode webrtc`。

## 4. `/api/switch`

前端模式菜单点一下会 `POST /api/switch {"mode":"websockets"}`，成功后才重载
（`lib/mode-switch.js:43`）。这里**只答应一声**：两种传输读同一路程序，服务端没有
状态要改，页面重载后自己会去连新端点。

但它必须存在。没有它，菜单拿到 404、`switchStreamMode` 抛错、页面不重载，用户看到
的是"点了没反应" —— 那个控制项是别人前端里写好的广告，缺货的是我们这一侧。

GET 该路径返回 405：一个永远成功的探针比没有探针更糟。

## 5. 下行怎么接进来的

`VideoFanout` 原本只能挂 `VideoTrack`（RTP）。新增 `attach_stream(mpsc::Sender<Bytes>)`
作为第二种下行：

- 编码一次，`Bytes::clone` 是 `Arc` 自增，第二路传输只多一次 channel send
- **不 await**：`try_send`，队列满就丢这一帧（丢最新的，不是最旧的），慢观众只
  惩罚自己。丢帧造成的 id 空洞由前端的解码闸门负责要关键帧
- 队列深度 4 ≈ 60fps 下 67 ms，够吸收一次调度抖动，又不会让画面滞后
- `len()` / `is_empty()` 同时统计两类下行：注册表靠它判断"还有没有人看"，
  漏掉 socket 会让采集在还有人看的时候被停掉

所以"一个页签 WebRTC、另一个 WebSocket"天然成立，这不是巧合，是 fan-out 本来
该有的形状。

### 5.1 控制类下行：管线事件

视频/音频是"流"，而有些东西是"状态"：被控机的**指针形状**、采集降级/恢复。
WebRTC 用控制数据通道发它们（`websession.rs` 里那个 `PipelineEvent` 转发循环），
WS 版本起初一条都没有 —— `run()` 里根本没有 `add_event_sink`。

症状不是"报错"，是"少了一个东西"：页面的 `updateServerCursor` 从没被调用，
于是指针要么是浏览器的默认箭头、要么（在页面已经把本地箭头藏起来的模式里）
干脆没有。服务端日志一个字节都不留，因为没有一条路径在等它。

现在按**程序**（不是按连接）订阅：

- `pipeline.handle.add_event_sink(tx)` 拿到一个 `UnboundedSender`，
  `select!` 里多一支 `events.recv()`，只消费 `Cursor` 就发 `cursor,{…}`；
- sink 属于程序，换屏换程序时必须 `remove_event_sink(old)` + 重新挂；
- 订阅只收"以后"的事件，所以每个新连接/新程序还要补发一次**当前**形状
  （`handle.last_cursor()`，在 `emit_event` 里顺手缓存）。

补发这里有个顺序陷阱：`last_cursor()` 要等采集循环第一次 beat 才有值，而那一
beat 在 `handle.start()` 之后，补发跑在它前面 —— 十有八九拿到 `None`。指针形状
又不会自己变（`update_cursor` 只在 `id` 变化时 emit），所以"等下一次变化"可能
永远等不到。加了一个 250 ms 的补位定时器，拿到一条就把这一支永久关掉。

订上事件通道之后，.148 上还是 0 条。一行一次性诊断把原因分开了：

```text
WARN #111 host cursor: id 0x10005 is single-colour or unusable
```

被控机上那个光标是 `CreateCursor()` 造出来的**单色光标**（`hbmColor == 0`，
只有 AND/XOR 两张 1bpp 掩码）—— 文本 I 形、十字、缩放箭头、以及 VMware /
VirtualBox 这类虚拟机的控制台指针全都走这条路。`cursor_data` 原来对它是直接
`Ok(None)` 跳过的（注释还写着"现代 Windows 上很罕见"），于是事件永远是空的。
这一条补上之后同一条连接立刻拿到 `cursor,`、32×32、hot=(8,9)。

## 6. 验证

`toolchain/ws-probe.py` 手写 WS 客户端（stdlib，不装库 —— 要验的正是线上格式，
中间夹一层库只会把"库帮我拼对了"混进结论）：

```
python toolchain/ws-probe.py 192.168.59.148 8080 5
```

线上实测（.148，1080p，桌面静态，2026-11-05）：

```
handshake: HTTP/1.1 101 Switching Protocols
text: AUTH_SUCCESS,{"credential":"none","role":"controller","slot":1,"tenant":"default"}
text: MODE websockets
text: {"settings":{"audio_bitrate":{…},"microphone_enabled":{"locked":false,"value":false},…}}
text: {"info":{"backend":"windows","codec":"VP8","display":0,"encode_node":"CLIENT001",…}}
text: clipboard_reply,cr
text: MICROPHONE_DISABLED
frame#0: type=0x04 codec=vp8 key=1 id=0 y=0 1920x1080 ref=0 payload=24165B fin=1
frame#1: type=0x04 codec=vp8 key=0 id=1 y=0 1920x1080 ref=0 payload=75980B fin=1
…
RESULT ok frames=8 keyframes=1 audio=0 text=7 1920x1080 mic=disabled pre_switch=0
```

`clipboard_reply,cr` 是连接建立时页面发的 `cr` 的回执：没有它，页面的
`taggedClipboardFetch` 会一直等在那儿，剪贴板同步永远不开始。

这次 `mic=disabled` 是因为探针连上去时**另一个会话正握着输入租约**（同一块屏的
`exclusive` 模式）—— 探针是观众，麦克风这道门对它关着，服务端如实回绝而不是
静默丢弃。日志同一时刻写着 `identified as tenant=default grant=viewer operates=yes
cred=none` 与 `microphone refused (grant=viewer, operates=yes, lease=no)`。

这台虚拟机上 `audio=0` 是预期结果 —— 机器不播放任何声音，loopback 没有事件可采。
有声音的机器上同一个探针应给出非零值。

另一条断言是 JPEG 兜底（§2.2），也是"老浏览器能不能看"的唯一自动证据：

```
python toolchain/ws-probe.py 192.168.59.148 8080 8 - jpeg
stripes=8 frames=2 bands=4 jpeg=1
RESULT ok stripes=8 frames=2 bands=4 audio=0 text=5 jpeg=1 pre_switch_vp8=0
```

`pre_switch_vp8` 是切换生效前漏出来的整帧数：改成 0 说明 `SETTINGS` 一到就切过去了，
多于一两个说明服务端在犹豫（那条连接上有几帧是页面根本解不了的 VP8）。

同一次连接在服务端留下的日志（注意 `sink 1` 是已在线的 WebRTC 页签，`stream sink 2`
是这次 WS 探针 —— 同一个 program，一个编码器）：

```
pipeline 0: program created, capture loop spawned
video fanout: sink 1 attached (1 live)
video fanout: stream sink 2 attached (1 live)
ws media: 192.168.59.146 attached (2 downlink(s), codec Vp8, queue 4)
ws media: client closed (Some(Normal))
video fanout: sink 2 detached (1 live)
ws media: 192.168.59.146 detached (1 downlink(s) left, 0 frame(s) dropped)
```

第三条自动证据是**输入租约的交接**（§7.2），它验的是"指针到底能不能驱动桌面"：

```
python toolchain/lease-adopt-probe.py 192.168.59.148 8080
server: pid=11180 input_policy=exclusive
A: id=7 role=controller denied=0
B: id=8 role=controller denied=0
A after B: role=viewer
after closing B: A role=controller B=gone
A input: denied 0 -> 0
RESULT ok transport-switch lease handover works
```

`A input: denied 0 -> 0` 之后，服务端下一个 5 s 窗口的统计行必须是 `m=30`（正好是探针
发出的条数）—— `role=controller` 只说明门禁放行，`m=` 才说明字节走到了注入器：

```text
pipeline 5.0s: loop=60.0Hz new=4 repeat=296 … paint=0 m=30 m2=0 dm=0 pm=0
```

## 7. 上行：输入与剪贴板

上行**不另写解析器**：`lib/input.js` 在两种传输下是同一份，所以 WS 侧直接复用
`dispatch::commands::classify` 的结果，落到和 WebRTC 相同的 `WinInputSink`。

| 上行 | 处理 |
|---|---|
| `SETTINGS,{json}` | 与 WebRTC 同形，复用 `apply_settings_batch`（`encoder` / `framerate` / `manual_resolution`）；另外从中读出 **`displayId`** 与 **`tabId`**（见下） |
| `KEYFRAME` / `REQUEST_KEYFRAME` | `pipeline.handle.request_keyframe()` |
| `cr` | 回 `clipboard_reply,cr`，然后把当前剪贴板按 WS 家族发下去 |
| `m` / `m2` / `kd` / `ku` / `kh` / `kr` / `p` / `sf` / `js` | `classify` → `InputStub`，按 `--input-policy` 放行或计入 `denied` |
| `cw` / `cws` / `cwd` / `cwe` / `cb` / `cbs` / `cbd` / `cbe` | `ClipboardSink`：读请求、写开始、分块、结束 |
| `LOST_FRAME` / `DECODE_PACE` / `CLIENT_FRAME_ACK` / `vp,…` | 忽略（前端自己的流控） |
| 二进制 `0x02`（麦克风） | 写虚拟录音设备，回 `MICROPHONE_READY,{…}` 或 `MICROPHONE_DISABLED`，见 §8 |
| 二进制 `0x06`（摄像头） | 回一条 `WEBCAM_DISABLED`，见 §8 |

### 7.1 选屏：`SETTINGS.displayId`

页面在 URL hash 里选屏（`#display2`），但 **hash 不会发到服务端**；它把选择写在
`SETTINGS` 批里（`selkies-ws-core.js:3811` 的 `settingsToSend['displayId']`）。所以
WS 侧能选屏的唯一信息来源就是这一批 —— 这一版之前直接无视它，于是 `#display2` 的
页面看到的是主屏，而它自己的面板写着 "Secondary"。

处理顺序（`ws_media::run`）：

1. 连接先按服务端默认屏（`--display`）开一路，因为此刻还不知道页面要哪块；
2. `SETTINGS` 到了，若 `displayId` 与当前不同，按 detach → release → acquire 换过去。
   **顺序不能反**：`DuplicateOutput` 对同一个输出是独占的，新的一路要先开起来，某些
   卡会让第一路直接失败并退到 GDI，变成"一个 DXGI + 一个慢路径"两个程序；
3. 换完发 `PIPELINE_RESETTING <displayId>`，用**页面的名字**（`display2`，不是索引 1）：
   页面拿这个名字和自己的 `displayId` 比，不等就什么都不做（`selkies-ws-core.js:8996`）；
4. 帧号从 0 重新开始 —— 新程序是新的编码器，接着旧号会让解码闸门把第一帧当成
   "依赖缺席"；同时 `set_target_display` 把输入的目标屏换过去。

`tabId` 同样在这一批里，到了就 `registry.update_meta(...)` 补登记 —— 此前 WS 会话在
`health` 里 `tab_id` 永远是空串，"同一个页签重连"与"第二个观众"分不开。

### 7.2 输入租约：`lease` 是**会失效**的

`--input-policy exclusive`（#110 起的默认）下，键鼠只有租约持有者能投。租约挂在
**程序**上而不是会话上，所以接管对下一条消息立刻生效、不需要重连。

问题在于它一度是**永久所有权**：`try_claim_controller` 是 claim-if-free，谁也挤不掉，
只在连接收尾时才 `successor()` 归还。而"页面换传输"这件事同时制造了两个条件 ——

- 旧传输的会话**还活着**（PeerConnection 没拆、下行 sink 还在收字节，所以 `successor`
  也认它），
- 它**再也不会投一条输入**（页面已经在另一条传输上了）。

于是切到 WebSocket 之后租约锁在旧会话手里，新 media 连接的每一条 `m,` 都被门禁丢掉。
页面上的现象：画面在流、指针跟着鼠标动（那是页面自己画的）、桌面一动不动 —— 用户
描述成"指针像是再次渲染上去的，悬停和点击都没有反馈"，而且重连治不好。

修法是给租约加**失效判据**，只用一条硬事实：持有者多久没真的投过输入。

| 常量 | 值 | 用在哪 |
|---|---|---|
| `LEASE_ADOPT_IDLE` | 1.5 s | 连接落座时 `adopt_lease_if_stale`：旧持有者这么久没投过输入 → 接过来 |
| `LEASE_TAKEOVER_IDLE` | 3 s | 每条真输入时 `may_inject` 的兜底：静默这么久 → 接管 |

三条配套规则，缺一条都会退化成"指针时灵时不灵"：

1. **时间戳只认注入点。** `note_input()` 只在 `Action::InputStub` 分发之前调（WS 的
   `handle_text` 与 WebRTC 指针通道各一次）。门禁本身也被控制通道的每条消息问一次
   （`pong`、`SETTINGS` 回执），在那里盖章会让一条只会回 `pong` 的死会话永远"在用"。
2. **只有真输入能触发接管。** `may_inject(session, may_operate, is_input)` 的第三个
   参数就是这个：控制类指令不驱动指针，让它们接管只会造出"有人接管了但什么都没发生"。
3. **认领过但一次都没投过输入，等于悬空。** `input_idle() == None` 与"陈旧"同义。
   不这么判，两条都不注入的连接会各握半边锁，谁也投不进去 —— 而且永远不会自愈。

接管本身是显式的（`set_controller` + 一行 info），日志里要能直接读出结论：

```text
ws media: … grant=viewer operates=yes lease=yes cred=none prev_idle=never
pipeline 0: input lease Some(7) -> Some(8) (holder was not injecting; new connection adopted it)
ws media: session 8 adopted the input lease (the previous holder was not injecting)
```

`prev_idle=` 这一栏是刻意加的：只看 `lease=no` 分不清"换传输留下的悬空租约"和
"正常的两页签"，而这两种情况的处置完全相反。

`successor()`（持有者收尾时的交接）同时改了排序：**先看有没有下行（`sink`），再看
新旧**。原来是"最老的那条"，而换传输之后最老的恰恰是被抛弃的那条传输 —— 线上就是
它把租约从 WebSocket 会话手里接回去的。

验收工具：`toolchain/lease-adopt-probe.py`（A 认领后不投输入，B 必须能接管，
且接管后的 30 条 `m,` 一条都不许被拒）。

**代价，写清楚：** 这两个阈值把 `exclusive` 从"先到先得、永久"改成了"谁在投输入
谁说了算"。同一台机器上只有一个操作者时，这严格更好（换传输、换页签、重连都不用
管租约了）；真正有两个人在抢同一个指针时，静默超过 3 s 的一方会丢掉租约，而他下
一条输入会在 3 s 内再拿回来 —— 表现是轮流生效，而不是一方被永久锁死。上一版的
"永久"在持有者活着但不再注入时就是一把打不开的锁，那个失败模式比轮流更糟。
两个常量没有做成配置项：它们描述的是"人停手"的量级，不是部署差异；真要调，改
`pipeline/shared.rs` 里那两行，测试会跟着走。


下行剪贴板与 WebRTC **不是同一族**，必须按 WS 的发：
`clipboard,<b64>` / `clipboard_binary,<mime>,<b64>` / `clipboard_start,<mime>,<total>` /
`clipboard_data,<b64>` / `clipboard_finish` / `clipboard_reply,cr` / `clipboard_secret`。
发成 WebRTC 那套 `cb,…` 页面只会当成未知消息丢掉。

## 8. 麦克风与摄像头

一条上行做了、一条没做，但两者的契约是同一条：**任何一次 `0x02` / `0x06` 都必须
得到恰好一次答复**。沉默是最坏的结果 —— 页面电平条照跳、对面一个字听不到，而
客户端没有任何线索可查。

| 上行 | 页面收到 | 页面行为 |
|---|---|---|
| `0x02` 麦克风 Opus | `MICROPHONE_READY,{"target":…,"note":…,"echoes":…}` | 上行已开，电平条继续 |
| `0x02` 麦克风 Opus | `MICROPHONE_DISABLED` | `microphoneEnabled = false`，停采集，状态条关掉 |
| `0x06` 摄像头帧 | `WEBCAM_DISABLED` | `webcamEnabled = false`，停采集 |

每条连接只回一次：页面的电平条按帧发 `0x02`，每帧回一条文本会把 socket 变成回音壁。
所以两个答复都挂在 `mic_refused` / 状态机后面，不是"每帧答一次"。

### 8.1 麦克风：写虚拟录音设备

`MicUplink`（`pipeline/audio.rs`）把 Opus 解成 48 kHz 立体声，写进由
`platform/virtual_mic.rs` 决议出的**虚拟线缆 render 端**：Steam Streaming
Microphone → VB-CABLE（`CABLE Input`）→ VoiceMeeter，都没有才退回默认播放端点
（`--mic-device default`，只用于"对讲"，必定自听）。

为什么不写默认扬声器：下行 loopback 采的就是默认端点的后混音信号，注入的声音会被
自己采回去，说话的人听到自己的声音延迟一个往返 —— 这是拓扑决定的，改延迟改码率都
消不掉。`MICROPHONE_READY` 里的 `echoes: true` 就是这件事的机器可读形式。

`MICROPHONE_DISABLED` 有三种来因，日志会写明是哪一种：

1. **没有输入租约**（`exclusive` 下另一个会话正在操作）—— 麦克风是往主机送声音，
   和键鼠同一道门；
2. **令牌是 `viewer`** —— 任何策略下都不给；
3. **目标不可用**（`--mic-device` 指的端点不存在，或虚拟线缆被设成了默认播放设备）。

第 1、2 种以前是**静默丢弃 + denied 计数**，页面无从得知；现在一并回绝，因为
"观众点了麦克风"在页面上的正确反应和"服务端没接收端"完全一样：关掉它。

### 8.2 摄像头：仍然没做

反向视频 track 全项目未实现（WebRTC 侧同样没有），所以 `0x06` 一律回绝。这一条
与麦克风不同：不是"接一个分支"的量级，要一条反向 track 加一路解码。

同时 `server_settings` 里麦克风的声明现在跟随 `--mic-device` 的决议结果（`locked`
只在真的没有可用目标时才为 true），摄像头仍是 `{value: false, locked: true}`
（前端的 `lockedOff` 写法）—— 只发设置挡不住手动点，只回消息则启动时会先弹一次
权限请求，两条都要有。

注意这些答复只在 WebSocket 这条路上发：WebRTC 的页面（`selkies-wr-core.js`）没有
这些分支，发过去只会被当成未知消息。

## 9. 还差什么

**多屏的物理验收。** 代码、协议与坐标数学都到位了（`win_input::tests` 用合成的虚拟
桌面覆盖了主屏/右侧屏/左侧屏三种归一化，含"左侧屏的第一个像素就是桌面的第一个像素"
这条最容易写错的），但**没有第二块物理屏就点不了**。真机验收两步：

1. `python toolchain/ws-probe.py <host> 8080 5 display2` —— 期望 `PIPELINE_RESETTING
   display2` + 之后每帧都是第二块屏的尺寸；单屏机上期望"同一行文本 + 没有帧"（服务端
   如实失败，不会把主屏错发过来）。
2. 在这个页面上把鼠标点到第二块屏的四角：落点必须在该屏的四角。左/上超过主屏的
   显示器是最容易错的那台（虚拟桌面原点是负的）。

**真正的人力浏览器验收。** 探针验证的是字节，不是产品：`?transport=websockets`
打开一次、看面板有没有编码器/码率/CPU、切一次屏、按一次输入 —— 这几步至今只在
探针层面验过。

## 10. 踩过的坑

- **视频整帧发成 `0x01`**：这是本文件最贵的一次错误。`0x01` 是音频，码流进了 Opus
  解码器，视频一次都没被解开；探针当时也断言 `0x01`，于是"验证通过"——测试和实现
  一起错了，没人发现。探针现在先判 `0x01` 当音频并计数，视频必须 `0x04`。
- **`shared.rs` 的引用计数释放要按 join 的 key**：`WS` 侧 `acquire` 用的是服务端
  默认屏，`release` 若写死 `PRIMARY`，`--display 1` 下程序永远不会被回收。

- **探针只存了前 120 个字符**做断言，`server_settings` 是一整行设置表，于是"有没有
  这条消息"的检查变成假阴性，第一次跑出 `RESULT fail no server_settings` 而服务端
  其实是对的。断言看全文，打印才截断。
- **`settings.output()` 第一帧之前是 `(0, 0)`**（由采集循环在打开编码器后写）。直接
  填进头里，前端 `VideoDecoder.configure({codedWidth: 0})` 会抛异常，症状是"连上了、
  一帧都解不出来"。零值回落到请求尺寸。
- **`serde_json` 的 Map 默认按字典序**，所以 `{"settings":…,"type":…}` 里 `type` 在
  最后。前端用 `JSON.parse` 不看顺序，但任何"以某字段开头"的字符串匹配都会误判。
- **`Cargo` 的 `S: Send` 边界**：`ws_media::run` 里没有 spawn，就不该要求 `Send`，
  否则 `handle_inner` 的泛型参数对不上（`S cannot be sent between threads safely`）。
- **`AUTH_SUCCESS` 少了前缀**：裸 JSON 被前端静默丢掉，症状是"画面好的、但角色永远
  是 null"——而角色决定 viewer 能否注入输入。这类"少一个字符串前缀"的错，日志里
  一个字节都不留，只有对照前端的 `startsWith` 才能发现。
- **`jpeg-encoder` 0.7 要 edition 2024**：本仓的 Cargo 是 1.75，拉 0.7 会在
  "解析 manifest"这一步失败（错误信息看着像网络问题，其实是工具链太旧）。钉 0.6。
- **统计窗口的所有权不能是单向开关**：它原来是 `AtomicBool::swap(true)`，第一个
  订阅者永久拥有；那个会话关掉之后，快照就停在最后一秒，后来的人看到的面板是冻结的。
  现在是 3 秒租约，发布即续租（`STATS_LEASE`）。
- **`server_settings` 回执必须是边沿触发的**（#111，本文件最贵的一次自激）。原来的
  设计是"每收到一批 `SETTINGS` 就回一条 `server_settings`"，看着无害；但页面对回执
  会 sanitize 一遍，把"被服务端改写过的键"再发回来，而回执里那四个锁定布尔
  （`use_cpu` / `video_fullcolor` / `video_10bit` / `webcam_enabled`）在前端 locked
  分支里拿 `!serverValue` 当默认值比较、又因为 `fromServer=true` 使所有 store 变成
  空操作 —— 于是**每轮回执都重新报"设置被改了"，永远收敛不了**：

  ```text
  SETTINGS → server_settings（1392 B）→ 页面 sanitize 报 changes → SETTINGS → …
  ```

  实测这个环跑到 **400–550 批/秒**：`stream-server.log` 涨到 337 MB、+167 KB/s
  （≈4 GB/h），下行约 4.7 Mbps 是同一条 JSON，进程 CPU 86%，主线程抢不到时间渲染
  第一帧 —— 用户看到的是"一直 Waiting for stream"，而画面数据其实一直在流
  （60 fps / 5.6 Mbps / `write_err=0`）。只在 WS 上出现：WebRTC 没有"每批回执"这个
  设计，回路形不成。

  两侧同时修：服务端 `EchoState` 记住上一条回执原文，内容不变就不发，另加 100 ms
  内**完全相同 payload 直接丢弃**（连 JSON 都不解析）；逐字段的 `SETTINGS:` 日志降到
  debug，收尾按 `StreamPrefs` 前后差异留一条聚合 info。前端 locked 分支的默认值改成
  `serverValue`，并让 `fromServer` 的 payload **一律不回写**（回执不是用户操作）。
  验收工具：`toolchain/echo-probe.py`（200 批逐字节相同的 SETTINGS → 0 条回执，真实
  改一档 → 1 条）。
- **`cursor,` 这条线上有三个各自独立的坑，凑齐才"有指针"**（#111）：
  1. WS 路径**没订管线事件** —— 不是坏了，是从来没做过；服务端零日志，页面零报错。
  2. 补发赶在采集第一 beat 之前 → `last_cursor()` 是 `None`，而形状又不会自己变。
  3. 单色光标被 `cursor_data` 跳过 —— 虚拟机上最常见的那一类。

  另外两个只在真机上才炸的细节：`cursor_png_b64` 把 BGRA 直接写进声明为 RGBA 的
  PNG（红蓝对调，白箭头看不出来）；`base64_encode` 不补 `=`，而这段 base64 进的是
  CSS `url("data:image/png;base64,…")`，没有 `atob` 的宽容规则 —— 静默退回默认箭头。
  验收工具：`toolchain/cursor-probe.py`（`WSPLAIN=1` 可用于本地明文服）。
- **"指针是叠上去的"其实是租约锁在旧传输上**（#113，见 §7.2）。这一条值得单独记：
  现象全在浏览器侧（指针跟手、悬停无反馈），证据全在服务端（`lease=no`、`denied`
  在涨），而最初的排查方向被上一轮自己写的日志骗了一次 —— `operates` 那栏当时写的是
  **角色**，不是"能不能动手"，于是"租约被别人握着"被打印成了 `yes`。日志要回答的
  是当下的事实，不是配置。

- **"客户端 FPS 0 而服务端 60"不是一路数**（#114）。面板上那两条线来自两个完全不同的
  地方：`encoded_fps` 是服务端每秒发来的样本，`fps` 是**页面自己数的呈现帧**。后者挂在
  一个很容易断的链条上 —— `videoWatch` 只在统计打开的那一刻绑一次（那一刻 `<video>`
  可能还不存在）、worker 只有被明确告知"统计开着"才会计数、canvas 在自己的绘制点计数。
  任何一环没接上，计数就整场为 0，而画面明明是好的。现在规则是：**这一秒没有任何呈现
  证据时，报线上帧率 `window.fps`**，不报 0 —— 0 是一句假话，而且它会把读的人引向
  一条完全正常的流。同时补绑 `videoWatch`（元素后到也能跟上）。
  排查工具：`toolchain/ws-fps-probe.py`（服务端帧号是否每秒 60 个不同值）、
  `toolchain/page-probe.py`（无头 Edge + CDP，直接读页面里的 `window.fps` /
  `stream_stats.latest`，并区分"帧没到"与"到了没记账"）。

- **一次队列排空就把这条连接永久降级成"只发关键帧"**（#116）。这是本轮找到的
  真根因，也是上面 #114 那条一直没解释干净的半边。

  下行循环里那句判断写的是"这一批丢过帧，所以下一帧必须能独立解码"，用的却是
  **整条连接的累计数**：

  ```rust
  let mut latest = first;
  while let Ok(next) = rx.try_recv() { latest = next; dropped += 1; }   // dropped 在循环外
  if dropped > 0 && need_key == false { need_key = true; }              // 于是"丢过一次"= "永远丢"
  ```

  `dropped` 只增不减，所以排空过一次队列之后，此后**每一拍**都会重新上闸 ——
  哪怕这一拍只有一帧、队列是空的。闸上之后只有关键帧能过，而周期 IDR 是
  `keyframe_secs`（默认 10 秒）一个：60 帧/秒被削成 0.1 帧/秒。面板取整就是 0，
  偶尔有一个落在 1 秒窗口里就是 1 —— 用户看到的"**一直在 0 和 1 之间浮动**"。
  它是**永久**的：只有重连（`dropped` 归 0）才恢复。

  触发点就是用户说的"在桌面上随便动一动"：右击弹出菜单是一次大面积变化，帧变大、
  浏览器有一拍跟不上，4 深的队列就积起来了。

  为什么两个数字当时都"对"：`health` 的 `frames_total` / `avg_mbps` 取自
  `fanout.rs` 里 `try_send` **入队成功**那一刻的计数，不是真正写进 socket 的字节。
  入队的 60 帧里绝大多数随后被这道闸丢掉，于是"服务端 60、客户端 0"两边都不算错，
  只是量的不是同一件事。

  修法两条：闸只看**这一批**（`key_gate_after_batch(queued, need_key)`），上闸的
  那一刻顺手 `request_keyframe()` —— 真实的丢帧仍然要等关键帧，但不该等 10 秒。
  回归工具 `toolchain/gate-latch.py`：连上去正常读两秒，然后**两秒不 recv**（真实
  的 TCP 背压 → 服务端队列积压），再恢复读，数后面每秒还能收多少帧。修复前停读
  一次就再也回不到 60；修复后实测"恢复第 1 秒 181 帧（积压+关键帧），第 2 秒起
  稳定 59-63 帧/秒"。

  同一轮还给前端补了一句"报 0 的时候把缘由写出来"：那一拍里明明收到了视频分块却
  报 0，只可能是三种成因（共享观众、条纹模式、报文头不可用），事后面板上一个都看
  不出来。日志要回答当下的事实 —— 这条教训在 #113 已经付过一次学费了。

## #117 默认值：WebSockets + VP9

产品默认变了，三层各改一处，任何一层漏改都会表现为"默认没变"：

| 层 | 位置 | 改前 | 改后 |
|---|---|---|---|
| 服务端传输 | `config.rs` 的 `default_mode`（`--mode`） | `webrtc` | `websockets` |
| 服务端编码器 | `config.rs` 的 `video_codec`（`--video-codec`） | `vp8` | `vp9` |
| 面板编码器回退值 | `Sidebar.jsx` 的 `DEFAULT_ENCODER` | `encoderOptions[0]`（= `h264enc`） | `vp9enc` |
| 核心本地回退值 | `selkies-ws-core.js` / `selkies-wr-core.js` 的 `encoder` | `h264enc` | `vp9enc` |

为什么不是只改面板：`server_settings.encoder.value` 回显的是**正在跑**的编码器
（`dispatch/output.rs` 的 `s.codec().encoder_id()`），并且 #94 起服务端说的优先于
本地存的。所以只改面板默认值，会先显示 VP9、随即被服务端的 VP8 覆盖回去。

`encoderOptions[0]` 是 `h264enc` 这件事本身也是坑：那是上游 selkies 的菜单顺序，
这个构建只有 libvpx（VP8/VP9），任何"退到第一个选项"的路径都会落到一个编不出来
的编码器上。

**验证**（`toolchain/defaults-probe.py`，用一次性 profile 起无头 Edge，展开"视频
设置"分组后读 DOM）：

```
storedMode    websockets      ← main.jsx 从 /api/status 播下去的值
panelMode     websockets      ← #streamModeSelect
panelEncoder  vp9enc          ← #encoderSelect
transport     1 (OPEN)        ← 实际连的是 WS
```

服务端侧：`/api/status` → `current_mode: websockets`；连接后 `/api/v1/health` 的
`stream.codec` 为 `vp9`；`ws-fps-probe.py` 稳态 60 个不同帧号/秒。

回退方式仍然是命令行：`--mode webrtc`、`--video-codec vp8`，或者面板里直接选。
已有浏览器**保留自己存的选择**（`main.jsx` 只在 localStorage 为空时播种），所以
这套默认值只影响"从没选过"的浏览器 —— 想立刻看到效果就清掉站点数据或换一个
profile。
