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
| 0 | 1 | 帧类型。整帧恒 `0x01`；`0x03`/`0x04` 是 JPEG/H.264 分行模式，这里不发 |
| 1 | 1 | `codec_id << 4 \| is_key`（`lib/wire-codecs.js:23`）：h264=1、vp8=2、vp9=3、av1=4、h265=5 |
| 2 | 2 | frameId，大端 u16，回绕 |
| 4 | 2 | y（行偏移）。整帧恒 0 |
| 6 | 2 | 宽，大端 |
| 8 | 2 | 高，大端 |
| 10 | 2 | reference：本帧依赖的**上一帧** frameId |

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
server → AUTH_SUCCESS,{"role":"controller","slot":1,"credential":"bearer"}   # 仅当配置了口令
server → MODE websockets
server → {"type":"server_settings","settings":{...}}
server → 二进制帧 …
```

三处容易错的细节：

1. **`server_settings` 的形状和 WebRTC 不同。** 数据通道上是
   `{"type":…,"data":{"settings":{…}}}`，WebSocket 上读的是**扁平**的
   `obj.settings`（`selkies-ws-core.js:8644`）。发错一层，页面不会报错 —— 分辨率、
   编码器、帧率全部停在它自己的默认值，下拉框全是假的。
   `ws_media::settings_frame` 就地把 `output::server_settings` 拆一层，单一事实来源
   仍留在 `dispatch/output.rs`。
2. **`AUTH_SUCCESS` 只在有口令时发。** token 模式下前端不从 URL 片段推角色
   （`selkies-ws-core.js:8407`），不发就等于"连上了但没有身份"。
3. **先身份、再 MODE、最后设置。** 前端收到 MODE 就开始初始化输入与音频，此时
   角色还不知道的话，它会按未认证走一遍再补一遍。

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

## 6. 验证

`toolchain/ws-probe.py` 手写 WS 客户端（stdlib，不装库 —— 要验的正是线上格式，
中间夹一层库只会把"库帮我拼对了"混进结论）：

```
python toolchain/ws-probe.py 192.168.59.148 8080 5
```

线上实测（.148，1080p，桌面静态）：

```
handshake: HTTP/1.1 101 Switching Protocols
text: MODE websockets
text: {"settings":{"audio_bitrate":{…},"encoder":{"value":"vp8enc",…},…},"type":"server_settings"}
frame#0: type=0x01 codec=vp8 key=1 id=0 y=0 1920x1080 ref=0 payload=173437B fin=1
frame#1: type=0x01 codec=vp8 key=1 id=1 y=0 1920x1080 ref=0 payload=173247B fin=1
frame#2: type=0x01 codec=vp8 key=0 id=2 y=0 1920x1080 ref=1 payload=46550B fin=1
frame#3: type=0x01 codec=vp8 key=0 id=3 y=0 1920x1080 ref=2 payload=31472B fin=1
frame#4: type=0x01 codec=vp8 key=0 id=4 y=0 1920x1080 ref=3 payload=27759B fin=1
RESULT ok frames=4 keyframes=1 text=2
```

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

## 7. 这一版**没做**什么

| 缺口 | 现状 |
|---|---|
| 键鼠注入 | 前端会把输入当文本命令发上来，服务端目前只记 debug 日志。页面能看不能操作 |
| 音频 | `START_AUDIO` 已忽略；WebSocket 侧的 Opus 分帧（2 字节头）未实现 |
| 剪贴板 | 二进制上行不接（`Message::Binary` 直接丢弃） |
| 摄像头上行 | `server_settings` 里仍声明 `webcam_encoder: vp9`，实现为空 |

最后一条是已知的"广告了没货"：页面会画一个摄像头开关，点了没有反应。下一轮要么
补上，要么把声明改成 unsupported。

## 8. 踩过的坑

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
