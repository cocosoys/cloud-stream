# 阶段 1 · #35 信令协议闭环装配记录（5.1 / 5.2 / 5.3 / 5.5）

> 路线 A：以 RustDesk fork `49c89bd` 为基底。本文件记录 `crates/stream-server/src/http/`
> 与 `src/signaling/`、`src/dispatch/` 从空壳占位到与前端 `selkies-web-core` 信令契约闭环的
> 取证结论、编译期纠偏清单、#36 采集编码循环的对接接口，以及阶段 1 边界。
> **编译口径：debug 构建，`toolchain\build-ws.cmd build -p stream-server -j2`，EXIT=0；
> `cargo test -p stream-server` 2 个路由单测全部通过；stream-server 自身零 warning。
> 残留 warning 全部来自 vendor hbb_common / scrap 已知遗留项（见第 7 节）。**

## 1. 本任务范围与验收结论

### 1.1 已落地（对照设计稿 5.1 / 5.2 / 5.3 / 5.5）

| 设计稿小节 | 落地内容 | 代码位置 |
| --- | --- | --- |
| 5.1 HTTP 分流 | `TcpListener` 明文监听；httparse 解析请求行/头；`GET /api/status` 探活；`GET /api/<app>/signaling/` 走 tungstenite 升级；其余 GET 由静态服务兜底；非 GET 返回 405 | `http\server.rs:156–258`、`http\mod.rs:40–65` |
| 5.2 WS 握手行协议 | `HELLO client <meta>` 回 `HELLO`；`SESSION server` 后回 `SESSION_OK 1` 并由服务端 always offer；`<peer_id> {"sdp":…}` / `<peer_id> {"ice":…}` 双向转发；非 answer SDP 拒绝；不可识别行降级为 debug 不拆链 | `signaling\line.rs:54–122`、`signaling\websession.rs:209–272` |
| 5.3 Connected 后下行白名单 | `pipeline(status)`、`stream_info(StreamInfo 默认 dxgi)`、`connection(poor=false)` 三件套；`_stats,1/0` 控制 1s 周期 `stream_stats`；其余下行类型阶段 1 不发 | `dispatch\output.rs:67–122`、`signaling\websession.rs:383–407`、`326–349` |
| 5.5 上行命令分类 | A 组（START_VIDEO / STOP_VIDEO / `_stats` / `_gz` / ping）驱动 pipeline 闸门、统计闸门、压缩应答、pong 回包；B 组仅 debug 记录；C 组进 NullInputSink 计数不注入 | `dispatch\commands.rs:47–86`、`signaling\websession.rs:416–459` |
| 5.2 会话顶号与终态清理 | 单会话注册表 `SessionRegistry`：新连接以 close code **4001** 顶掉旧会话；PC `Failed/Closed` 以 1000 关闭；会话退出时 `media.close()` + `pipeline.stop()` | `signaling\websession.rs:61–77`、`117–141`、`284–322` |

### 1.2 验收动作与结果

- 编译命令：`toolchain\build-ws.cmd build -p stream-server -j2` → `EXIT=0`，`Compiling stream-server`
  之后无本 crate warning。
- 测试命令：`toolchain\build-ws.cmd test -p stream-server -j2` → `running 2 tests`，
  `routes_signaling_wildcard` / `routes_status_and_static` 均 `ok`，`2 passed; 0 failed`。
- 尚未做端到端出画面验收（属 #37：部署 143、浏览器 `https://192.168.59.143:8080/` 出画面）；
  本任务只闭环信令握手、媒体面装配、下行白名单与上行分类。

## 2. 前端契核对账清单（逐字核对，不凭记忆改写）

> 前端根目录：`C:\Users\user\Desktop\selkies-webclient\addons\selkies-web-core\`。
> 证据列为前端源码行号；服务端实现列为本 crate 文件。契约不一致处已在第 3 节纠偏。

### 2.1 WS 握手时序与行格式（`lib/signaling.js`）

| 环节 | 前端行为（行号证据） | 服务端实现 | 对账结论 |
| --- | --- | --- | --- |
| WS URL | `wss://<host>/api/webrtc/signaling/`；appName 在 `selkies-wr-core.js:3049` 硬编码 `"webrtc"`；页面挂根路径（:3105–3107） | `http\mod.rs:52` 同时匹配 `["api",app,"signaling"]` 与带尾斜杠的 `["api",app,"signaling",""]` | 路径两段形态都覆盖；app 段阶段 1 不做严格鉴权校验 |
| 上行 HELLO | `HELLO client <meta-json>`（:290）；peer_type=`client`；meta 含 client_type/display_id/page_size 等 | `signaling\line.rs:60–64` 取 peer_type、meta 解析失败置 None 不报错；`websession.rs:218–223` 存 meta、转 HelloSent、回 `HELLO` | 一致 |
| 下行 HELLO | `event.data === "HELLO"` 精确相等比较（:361）；收到后前端才发 `SESSION server`（:347–348） | `hello_ack()` 返回纯文本 `"HELLO"`（`line.rs:101–103`），无尾随空白 | 一致 |
| 上行 SESSION | `SESSION server`（:347） | `SessionRequest{target}`（`line.rs:65–67`）；`websession.rs:225–238` 建媒体会话、回 `SESSION_OK 1`、生成 offer 上行 | 一致 |
| 下行 SESSION_OK | `event.data.split(" ")[1]` 缓存 server_peer_id（:381–385） | `session_ok()` = `SESSION_OK 1`（`line.rs:106–108`）；`SERVER_PEER_ID="1"`（:25） | 一致；前缀 `1` 用于后续 SDP/ICE 行 |
| 下行 offer | 服务端 always offer；前端只接受 `type=offer`，非 offer 报错（signaling.js:35 注释、webrtc.js:301–302） | `websession.rs:234–237` `make_offer` → `sdp_line("1","offer",…)`；`line.rs:111–116` 包成 `1 {"sdp":{"type":"offer","sdp":"…"}}` | 一致 |
| 上行 answer | `sendSDP(sdp)` = `${server_peer_id} ${JSON.stringify({sdp})}`（:529–532）；`new RTCSessionDescription(msg.sdp)`（:410–411） | `line.rs:76–87` 解出 sdp_type/sdp；`websession.rs:240–251` 仅接受 `answer`，其余 warn 忽略；`apply_answer` 透传 | 一致 |
| 上行 ICE | `sendICE(ice)` = `${server_peer_id} ${JSON.stringify({ice:ice})}`（:520–523）；`new RTCIceCandidate(msg.ice)`（:412–414） | `line.rs:88–92` 取 ice 容器整体；`remote_from_json` 保留 candidate/sdpMid/sdpMLineIndex/usernameFragment（`webrtc\ice.rs`）；`add_remote` 透传 | 一致；camelCase 字段名与前端构造函数对齐 |
| 下行 ICE | 前端把本地候选经 WS 上行；服务端本地候选由 `on_ice_candidate` 序列化进 `ice_rx`，再经 `ice_line("1",…)` 包装回包 | `websession.rs:351–373` ICE forwarder 任务；`line.rs:119–122` | 一致；收集结束 `None` 不发空候选（#34 已定） |
| RESUMED / ERROR | `RESUMED <peer>`（:372–378）、`ERROR peer server not found`（:387–395） | 阶段 1 不实现 resume；收到未识别行走 `Unrecognized` debug，不触发重连 | 边界已记入第 6 节 |
| Close code | 4000=服务端失败带 reason；4001=被新连接顶掉（前端不自动重连，:448–453）；1000/1001=intentional；其余按 resumable 退避 | 4001 顶号（`websession.rs:71–75`、:183–191）；终态 1000 Normal（:195–202）；4000 保留给媒体装配失败路径 | 一致 |

### 2.2 DataChannel 下行路由（`lib/webrtc.js:735–805`）

| 下行 type | 前端读取字段（行号证据） | 服务端构造（`dispatch/output.rs`） | 对账结论 |
| --- | --- | --- | --- |
| `pipeline` | `msg.data.status` 写入状态条（:736–737） | `pipeline(status)` → `{"type":"pipeline","data":{"status":…}}`（:67–69） | 一致 |
| `stream_info` | `onstreaminfo(msg.data)` 整体上送 `streamStats.setInfo`（:738–741；core :3306） | `stream_info(info)` 含 backend/capture/zero_copy/zero_copy_available/capture_reason/encoder/hardware/gpu_present/hardware_expected/encoder_reason/codec/fullcolor/bit_depth/striped/gpu/driver/encode_node（:73–97） | 字段名与 `lib/stream-stats.js:46–71` typedef 逐项对齐；renderer 组为 wayland 可选字段，Windows 省略 |
| `stream_stats` | `onstreamstats(msg.data)`（:742–745；core :3307）；服务端新鲜度阈值 3000ms | `stream_stats(s)` 含 cpu_percent（整机 0–100）/process_cpu_percent（本进程单核）/mem_used/mem_total/encoded_fps/encode_ms/pipeline_ms/throttled（:124–146）；gpu 组与 rtt_ms 有意省略 | 一致；#36 前 pipeline 指标为零值占位 |
| `connection` | `onconnection(!!(msg.data && msg.data.poor))`（:746–749） | `connection(poor)` → `{"type":"connection","data":{"poor":…}}`（:120–122） | 一致；阶段 1 固定 `false` |
| 其余 type | 未识别会报 `Unhandled message received`（:802–804） | 白名单外类型阶段 1 一律不发送 | 边界已记入第 6 节 |

### 2.3 上行命令分组（`selkies-wr-core.js` 发射点 + `lib/webrtc.js` open 动作）

| 分组 | 命令（前端发射点见取证摘要） | 分类动作（`dispatch/commands.rs`） | 阶段 1 处置（`websession.rs:416–459`） |
| --- | --- | --- | --- |
| A 驱动 | `START_VIDEO` / `STOP_VIDEO` | `StartVideo` / `StopVideo` | `pipeline.start()` / `pipeline.stop()` 翻闸门；#36 循环读闸门写帧 |
| A 闸门 | `_stats,1` / `_stats,0` | `StatsOn` / `StatsOff` | `stats_on` AtomicBool 置位/清位；统计周期任务据此跳过 tick |
| A 压缩 | `_gz,1` | `CompressedNegotiate` | control 通道回 `_gz,0`；阶段 1 上行不压缩 |
| A 心跳 | `ping,<ts>` | `Pong(ts)` | 回 `pong,<ts>`；阶段 1 服务端不主动发起 ping |
| B 忽略 | `SETTINGS` / `r,WxH` / `s,DPI` / `vb,kbps` / `ab,bps` / `_arg_fps,N` / `_crf,N` / `_rc,mode` / `_ebc,bool` / `cmd,command` / `_stats_video,{json}` / `_f,fps` / `_l,ms` / `REQUEST_CLIPBOARD` / `START_AUDIO` / `STOP_AUDIO` / `cr` / `pong` | `Ignore(head)` | 仅 debug 日志，不回包、不改状态 |
| C 输入 | `kd` / `ku` / `kh` / `m` / `m2` / `p` / `SET_NATIVE_CURSOR_RENDERING` / `js` / `_pointer_echo` / `pointer` / `sf` / `vp`；裸 `kr` / `sfe` / `pointer,none` | `InputStub(raw)` | `input.note_stub(line)` 计入 NullInputSink `stub_lines`；不解析参数、不注入 |
| 未识别 | 其他任意行 | `Unknown(raw)` | debug 日志，不拆链 |

### 2.4 DC open 时序对账

- 前端：非 `pointer` label 的通道存 `_send_channel`，`binaryType='arraybuffer'`；`onopen` 时若有 `CompressionStream` 先发 `_gz,1`，再触发 `ondatachannelopen`（`webrtc.js:610–626`）。
- 服务端：`pointer` / `control` 两条通道在 #34 `ControlChannels::open` 创建（`pointer` 无序零重传、`control` 有序可靠）；Connected 后 `control.on_open` 一次性发三件套，`on_message` 挂命令分类器。
- **竞态决策**：PC `Connected` 回调可能略早于 `control` 通道 `open` 瞬间，不能在状态回调里直接 `send_control`（未就绪会返回未打开错误）。三件套改由 `on_open` 驱动，与前端 `ondatachannelopen` 时序对称（详见第 3 节纠偏 #8）。

## 3. 编译期纠偏清单（本轮七轮收敛的真实问题）

> 下列条目是本轮编译/类型检查实际报错后按 fork / tungstenite / tokio 源码修正的结论；
> 后续编码不得回退到草稿写法。

| # | 最初写法 / 直觉 | 实测结论与修法 | 证据位置 |
| --- | --- | --- | --- |
| 1 | `Message::Text(String)` 直接传入 | tungstenite 0.29 `Message::Text(Utf8Bytes)`；`String` 必须 `.into()` 转换。`hello_ack()` / `session_ok()` / offer 行 / ICE 行四处下行发送均已改 | `tungstenite-0.29.0\src\protocol\message.rs:159` |
| 2 | `tungstenite::frame::coding::CloseCode` | 正确路径是 `tungstenite::protocol::frame::coding::CloseCode`；`CloseFrame{code, reason}` 同属 `protocol` 模块 | websession.rs:33–37 导入段 |
| 3 | `httparse::Header::new("")` 数组初始化 | `httparse::Header` 无 `new()`；用 `std::array::from_fn(\|_| Header { name:"", value:&[] })` 构造 64 个槽位 | `http\server.rs:162–164` |
| 4 | `#[derive(Unpin)]` 标注 `PrefixedStream` | 含 `TcpStream` 字段的结构由 derive 路径推断失败；改为 `impl Unpin for PrefixedStream {}`（字段全部 Unpin，手写 impl 安全） | `http\server.rs:81` |
| 5 | 把 `WebSocketStream` 当成 tungstenite 子模块路径 | 类型定义在 `tokio_tungstenite::WebSocketStream`；`tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message}` 才是 tungstenite 协议件 | websession.rs:32–37；server.rs:30–34 |
| 6 | `ice_rx.clone()` 让后台 ICE 任务持有副本 | **tokio `mpsc::UnboundedReceiver` 不实现 `Clone`**（与 std sync mpsc 不同）。`MediaSession` 新增 `take_ice_rx()`：`mem::replace` 出 owned 接收器并置空 sender，`None` 表示重复取用；ICE forwarder 任务独占该接收器 | `webrtc\mod.rs` `take_ice_rx`；websession.rs:354–358 |
| 7 | `match *state_rx.borrow()` 闭包内跨 `.await` | `watch::Ref` 不是 `Send`，guard 跨 await 使 spawn future 非 Send。先 `let state = *state_rx.borrow()` 复制 Copy 状态值、释放 guard，再进入 `Connected` 异步分支 | websession.rs:296–302 |
| 8 | Connected 状态回调里直接发三件套 | 状态 Connected 与 control open 存在瞬间竞态；`send_control` 预检查 `ready_state==Open`，未就绪会 bail 并误杀会话。改为 `control.on_open` 内顺序发送 `pipeline` / `stream_info` / `connection`，单条失败仅 warn 不中断 | websession.rs:387–407 |
| 9 | 后台任务 `async move` 共享同一个 `media` / `stats_on` 句柄 | 第一个 spawn 把 `Arc` move 走后，后续 stats 任务与 ICE 任务再借用即 E0382。每个后台任务在 spawn 前独立 `media.clone()` / `ctx.stats_on.clone()` / `ctx.pipeline.clone()` / `ctx.input.clone()`，共享原句柄留给 ICE 任务 | websession.rs:287–289、326–327、359 |
| 10 | `let control = channels.control().clone(); control.on_message(Box::new(move |… control.clone() …))` | `on_message(&self)` 的借用与闭包 `move control` 冲突（E0505）。先取 `control_hook = control.clone()` 独立 Arc 进闭包；`control` 本体只用于挂回调，不被 move | websession.rs:412–417 |
| 11 | 只覆盖 Text/Binary/Ping/Pong/Close 五个消息变体 | 0.29 仍有 `Message::Frame(Frame)` 内部变体；`match` 穷尽式必须显式补兜底分支，否则 E0004；阶段 1 收到即 debug 忽略 | message.rs:173；websession.rs:167–170 |
| 12 | `use crate::signaling::websession::{self, SessionRegistry}` | `SessionRegistry` 由 `signaling::mod.rs` 再导出，`websession` 子模块内并无该名字的再导出；拆成两条 `use`：`websession` 模块与 `signaling::SessionRegistry` | server.rs:38–39；signaling/mod.rs:14 |
| 13 | 未读字节 `head` 直接丢弃后交给升级回调 | `accept_hdr_async` 会重新读请求行/头；已读头部必须由 `PrefixedStream` 先回放、再代理 `TcpStream`，否则握手请求丢失 | server.rs:75–116、220 |

## 4. 落盘文件清单与职责映射

| 文件 | 职责 | 关键公开符号 |
| --- | --- | --- |
| `http\server.rs` | 明文 accept 循环、请求头预读、httparse 分流、PrefixedStream 回放、静态/探活手写响应、WS 升级与会话任务启动 | `serve(cfg)` |
| `http\mod.rs` | 路由枚举、路径分类（含 `?/#` 剥离、段归一）、两个路由单测 | `Route`、`classify(path, app_name)` |
| `http\static.rs` | 固定 MIME 表、目录穿越防护、index.html 兜底、404 文本 | `StaticFile`、`serve(dist, url_path)`、`not_found()` |
| `signaling\line.rs` | WS 行解析器与序列化器、固定 peer id | `parse_ws_line`、`hello_ack`、`session_ok`、`sdp_line`、`ice_line`、`IncomingLine`、`SERVER_PEER_ID` |
| `signaling\session.rs` | 握手状态机枚举、会话状态与 HELLO meta 持有 | `HandshakeState`、`SignalingSession`（`transition` / `is_connected`） |
| `signaling\websession.rs` | 单会话上下文、四路口 select 主循环、握手分派、媒体会话装配、状态 watcher、统计周期任务、ICE forwarder、Connected 下行与 DC 命令 hook、顶号注册表与终态清理 | `SessionRegistry`、`run(ws, cfg, registry)` |
| `dispatch\commands.rs` | 上行命令 A/B/C/Unknown 四分类 | `Action`、`classify(line)` |
| `dispatch\output.rs` | 下行白名单四类 JSON 构造、StreamInfo / StreamStats 结构 | `pipeline`、`stream_info`、`stream_stats`、`connection`、`StreamInfo`、`StreamStats` |
| `dispatch\input_stub.rs` | 输入注入 trait 与阶段 1 NullInputSink 计数实现 | `InputSink`、`NullInputSink`、`InputCounters`、`InputSinkRef`、`note_stub` |

### 4.1 一次会话的装配时序（服务端 offerer）

1. 浏览器 TCP 连接 → `http::serve` accept → `handle_inner` 预读头部、httparse 分流；
2. WS 路径：校验 `Upgrade: websocket` + `Connection: upgrade`，`accept_hdr_async` 升级为 `WebSocketStream`，进入 `websession::run`；
3. `SessionRegistry::register` 先给旧会话发 4001 顶号；
4. 上行 `HELLO client <meta>` → 回 `HELLO`，状态 `HelloSent`；
5. 上行 `SESSION server` → `MediaSession::new().await?`（#34：PC + VP8 轨道 + 双 DC）→ `SESSION_OK 1` → `make_offer` → offer 行上行，状态 `OfferSent`；
6. 并行三个后台任务：PC 状态 watcher（Connected 装配下行/hook，Failed/Closed 终态）、1s 统计周期（`_stats,1` 后才上行 stream_stats）、ICE forwarder（本地候选 → WS 行）；
7. 前端 answer 回到 → `apply_answer` → `AnswerApplied`；前端 ICE 候选到 → `add_remote`；
8. PC `Connected` → `arm_connected_notices`：挂 `control.on_open` 三件套与 `control.on_message` 分类器；
9. `control` open → 顺序发 `pipeline` / `stream_info` / `connection`；
10. 前端 DC open：发 `_gz,1` → 回 `_gz,0`；`ondatachannelopen` → `_stats,1`、`cr`、SETTINGS、START_VIDEO（#36 后闸门打开写帧）；
11. 终态：PC Failed/Closed、WS Close、顶号、`START_VIDEO` 之外的任意退出 → `media.close()` + `pipeline.stop()` 后关闭 WS。

## 5. #36 采集编码循环对接接口面（落码直接引用）

### 5.1 闸门与统计句柄

```rust
// PipelineHandle 由会话上下文持有；START_VIDEO / STOP_VIDEO 已在 hook 内翻转
ctx.pipeline.start();   // video_on = true
ctx.pipeline.stop();    // video_on = false
ctx.pipeline.is_on();   // 采集循环每拍先读此闸门，关闭时不抓帧、不编码、不写轨

// 主机口径采样器（platform::Sampler）：refresh() 后 sample() 出 HostSample
let mut sampler = platform::Sampler::new().unwrap_or_default();
sampler.refresh();
let host: platform::HostSample = sampler.sample();   // cpu_percent=整机 CPU（0–100）；
                                                    // process_cpu_percent=本进程单核；
                                                    // mem_used/mem_total=主机口径
let stats = dispatch::StreamStats::from_host(host);
// #36 再把 pipeline 聚合器的 encoded_fps / encode_ms / pipeline_ms / throttled 填入 stats
```

### 5.2 写轨

```rust
// media.video() 返回 &VideoTrack；duration 固定 cfg.spf()（repeat 帧同口径，设计稿 9.2 #4）
media.video().write_sample(bytes, cfg.spf()).await?;
// 轨道未绑定（offer 前 / PC Closed）时库内静默 Ok 不报错；绑定后由 packetizer 分包。
```

### 5.3 下行统计周期任务现状（#36 替换零值）

- 当前 stats 任务每 1s 在 `stats_on=true` 时构造 `StreamStats::from_host(host)`，pipeline 三字段为
  `Default` 零值；#36 接入 `PipelineStats::snapshot(window)` 后，把 `encoded_fps / encode_ms /
  pipeline_ms / throttled` 合并进同一 `StreamStats` 再发 `stream_stats`。
- `cpu_percent` 口径为**整机** CPU（全逻辑核平均，0–100）；`mem_used / mem_total` 为主机口径。
  2026-10-27 用户明确要求由「本进程单核占比」改为整机口径，理由是面板要让用户看设备健康度；
  旧口径保留在 `process_cpu_percent`（后端 JSON 有，前端只作悬停说明）。`mem_used/mem_total`
  仍为主机口径，不得回退为进程内存。

### 5.4 通道边界（不可串）

| 通道 | 承载 | 阶段 1 责任方 |
| --- | --- | --- |
| WS 信令 | HELLO / SESSION / SESSION_OK / offer / answer / 双向 ICE | `signaling\line.rs` 构造，`websession.rs` 取 `MediaSession` 产物包装 |
| DC `control`（有序可靠） | 下行 pipeline / stream_info / stream_stats / connection；上行 START_VIDEO / STOP_VIDEO / `_stats` / `_gz` / ping 与 B/C 组命令 | 下行 `dispatch\output.rs`；上行 `dispatch\commands.rs` 分类 |
| DC `pointer`（无序零重传） | 阶段 1 空挂；阶段 2 运动类输入 | 阶段 2 接入 |

## 6. 阶段 1 已知边界与遗留项

| 项 | 阶段 1 口径 | 后续阶段 |
| --- | --- | --- |
| Resume 会话 | 不实现 `RESUMED <peer>` 与 `resume_key` 续线；断线即新会话，前端按其自身退避重连 | 阶段 3 视需要补 resume |
| 音频 | 不创建 audio m-line；前端找不到 audio mid 返回 null 不报错；START_AUDIO/STOP_AUDIO 仅 Ignore | 不计划（云游戏画面优先） |
| 剪贴板 | `REQUEST_CLIPBOARD` / `cr` 仅计数忽略；不下发 `clipboard-msg*` 分块 | 阶段 3 工具栏对接时评估 |
| 下行 `cursor` 光标位图 | 不发；前端原生光标渲染开关由 SET_NATIVE_CURSOR_RENDERING 命令承载，阶段 1 记为 InputStub | 阶段 2 输入面与光标方案一并处理 |
| 静态刷新 / 关键帧周期 | 未实现周期关键帧与丢包静态刷新；NACK/PLI 由默认拦截器收包侧响应 | 阶段 3 QoS |
| 采集 / 编码 / 真实统计 | `pipeline\capture.rs`、`encoder.rs`、`sample_map.rs` 仍为 bail 占位；`platform\sysinfo.rs` Sampler 为默认零值；`stream_stats` pipeline 字段为零值 | #36 |
| TLS | 本任务只走明文 TcpListener；`Config::tls_enabled()` 双路径齐备但 acceptor 未装配 | #37 部署 143 前接 tokio-rustls 0.26 |
| 鉴权 | 无 token / display_id 校验；HELLO meta 只存日志 | 阶段 4 服务化 |
| 多会话并发 | 单会话注册表，新连接顶掉旧连接 | 阶段 4 多实例调度 |
| 未用暂存标注 | `main.rs:15` `#![allow(dead_code)]` 与 `dispatch\mod.rs:6` / `platform\mod.rs` / `pipeline\mod.rs` / `signaling\mod.rs` 顶部 `#![allow(unused_imports)]` 为骨架期暂存 | #36 采集循环接线完成后必须移除，让真实死代码 / 未用导入重新暴露 |

### 6.1 待 #36 清理的暂存符号

- `SignalingSession.video_on` / `SignalingSession.stats_on` 闲置字段已在本轮删除；闸门状态由
  `PipelineHandle.video_on`（`Arc<AtomicBool>`）与 `ctx.stats_on` 独立承载。
- `InputSink` 的八个具体事件方法在阶段 1 只有 NullInputSink 空计数实现；阶段 2 解析 C 组行后
  按事件类型分派，trait 形状不变。

## 7. 编译口径与遗留 warning（非阻塞，勿当错误）

本轮 `build-ws.log` 中 stream-server 自身零 warning；下列残留全部来自 vendor 依赖，与 #35 改动无关：

| 来源 | warning | 处置 |
| --- | --- | --- |
| `vendor\hbb_common\src\password_security.rs:213` | `secretbox` 冗余导入 | 沿用 vendor 现状，阶段 4 服务化时统一清 |
| `vendor\hbb_common\src\fingerprint.rs:251 / 283` | unknown lint `static_mut_refs` | rustc 1.75 无此 lint，忽略 |
| `vendor\scrap\src\common\vpxcodec.rs:181` | unreachable pattern | 沿用裁剪后现状 |
| `vendor\scrap` | `cargo:rustc-check-cfg requires -Zcheck-cfg flag` | 构建脚本提示，不影响产物 |

## 8. 取证基线

- 前端契约根：`C:\Users\user\Desktop\selkies-webclient\addons\selkies-web-core\`
  - `lib\signaling.js`：握手时序、SEND 格式、close code 语义（:290 / 347 / 361 / 372–395 / 399–414 / 520–538 / 437–465）
  - `lib\webrtc.js`：DC label 分流、onopen 压缩探测、下行路由表、answer SDP 与 transceiver 预留（:300–407 / 595–634 / 735–805 / 1173–1194）
  - `lib\stream-stats.js`：StreamInfo / StreamClient / StreamSample 字段 typedef（:46–110）
  - `lib\stream-stats-view.js`：encoder / capture / decoder 行渲染与 null 判定（:79–134）
- 协议件版本：tungstenite 0.29.0（registry 缓存前缀见 #34 记录第 8 节）；tokio 1.44.2；
  tokio-tungstenite fork rev `c241b11b03de4881faf1ecd2fe6e867c92f3013b`。
- 媒体面 API 取证基线沿用 `docs\stage1-webrtc-media.md` 第 8 节（fork `49c89bd` checkout 路径）。
- 编译包装：`toolchain\build-ws.cmd`；最新构建日志 `toolchain\build-ws.log`；测试日志
  `toolchain\test-ws.log`。
