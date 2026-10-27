# 阶段 1 · #34 WebRTC 媒体面装配记录（6.1–6.5）

> 路线 A：以 RustDesk fork `49c89bd` 为基底。本文件记录 `crates/stream-server/src/webrtc/`
> 四个模块从空壳占位到真实 webrtc-rs 装配的取证结论、对设计稿草稿的纠偏、以及 #35
> 信令闭环与 #36 采集编码循环的对接接口。**编译口径：debug 构建，`build-ws.cmd build
> -p stream-server -j2`，EXIT=0，stream-server 自身零 warning；残留 warning 全部来自 vendor
> hbb_common / scrap 的已知遗留项（见第 7 节）。**

## 1. 本任务范围与验收结论

### 1.1 已落地（对照设计稿 6.1–6.5）

| 设计稿小节 | 落地内容 | 代码位置 |
| --- | --- | --- |
| 6.1 PC 创建配置 | SettingEngine（mDNS Disabled、fe80::/10 过滤）、MediaEngine 注册默认 codec、默认拦截器链、APIBuilder 三挂载、RTCConfiguration（`ice_servers=[]`、policy `All`） | `webrtc/mod.rs:97–126` |
| 6.2 VP8 能力 + 下行轨道 + 双 DataChannel | VP8 capability（四条 RTCP feedback）、`TrackLocalStaticSample`、`add_track`；`control` 有序可靠、`pointer` 无序零重传 | `webrtc/track.rs:37–87`、`webrtc/channels.rs:36–84` |
| 6.3 Sample 装配与写轨 | 六字段 `Sample` 字面量、`write_sample` 异步封装、duration 由调用方传入 spf | `webrtc/track.rs:93–104` |
| 6.4 Offer / Answer / ICE | `make_offer` → `set_local_description`；`apply_answer` + 远端候选排队/冲刷；本地候选 `to_json` 经通道转发；收集结束 `None` 不发空消息 | `webrtc/mod.rs:233–273`、`webrtc/ice.rs` |
| 6.5 状态回调 + 下行挂钩 | `on_peer_connection_state_change`：Connected 通知、Failed/Closed 终态清理、Disconnected 不拆链；`on_data_channel` 空实现；`send_control` 封装 | `webrtc/mod.rs:176–213`、`webrtc/channels.rs:89–98` |

### 1.2 验收动作与结果

- 编译命令：`toolchain\build-ws.cmd build -p stream-server -j2`
- 结果：`EXIT=0`；`Compiling stream-server v0.1.0` 之后无任何 stream-server 专属 warning / error。
- 类型验收：`MediaSession / VideoTrack / ControlChannels / RemoteCandidate` 全部通过真实
  webrtc-rs API 类型检查；尚未做端到端出画面验收（属 #37 部署 143 阶段）。

## 2. API 取证纠偏清单（设计稿草稿 → fork 49c89bd 实测）

> 下列条目以 fork 源码为准，设计稿 `docs\stage1-design.html` 对应草稿文字已被本轮实测修正；
> 后续编码不得回退到草稿写法。

| # | 设计稿草稿写法 | fork 49c89bd 实测结论 | 证据 |
| --- | --- | --- | --- |
| 1 | `RTCPFeedback::new("goog-remb", None)` | **不存在 `new()` 构造函数**；结构体仅两个公开字段 `typ:String`、`parameter:String`，按字面量构造，无参数时 `parameter:String::new()` | `webrtc\src\rtp_transceiver\mod.rs:64–74` |
| 2 | 未提 MediaEngine 必须注册 codec | `MediaEngine::default()` 的 codecs 列表为空；不调 `register_default_codecs()` 则 offer 无视频 m-line。**必须** `let mut m = MediaEngine::default(); m.register_default_codecs()?; APIBuilder::new().with_media_engine(m).build()` | `webrtc\src\peer_connection\sdp\session_description.rs:153–155 / 178–180 / 197–199 / 220–222`（测试反复实证） |
| 3 | 草稿仅写 with MediaEngine | 必须同时挂默认拦截器链（NACK、RTCP 报表、TWCC）：`Registry::new()` → `register_default_interceptors(registry, &mut m)?` → `.with_interceptor_registry(registry)` | `webrtc\src\api\interceptor_registry\mod.rs:20–31`；`examples\play-from-disk-h264.rs:114–123` |
| 4 | 回调名 `on_connection_state_change` | 实际方法名 **`on_peer_connection_state_change`** | `webrtc\src\peer_connection\mod.rs:637`；类型别名 `OnPeerConnectionStateChangeHdlrFn:132–136` |
| 5 | 草稿本地候选前缀写 `2` | 固定服务端 peer id = **`1`**（`signaling\line.rs:SERVER_PEER_ID`），与前端 `SESSION_OK` 缓存一致 | 前端 `lib\signaling.js:381–385`；本工程 `signaling\line.rs:25` |
| 6 | 草稿把 `Disconnected` 列入清理动作 | **`Disconnected` 是瞬时可恢复 ICE 状态，不拆链**；只有 `Failed` / `Closed` 触发终态清理（hbb_common 同口径） | `peer_connection_state.rs:33–44`；`vendor\hbb_common\src\webrtc.rs:787–803` |
| 7 | 外部直接构造 `RTCSessionDescription { sdp_type, sdp, ..Default }` | `parsed` 是 `pub(crate)`，外部 crate 不能在字面量里填，`..Default` 也不能补私有字段（E0451）。Answer 用静态构造器 `RTCSessionDescription::answer(sdp)?`；`set_remote_description` 内部自行 `unmarshal` 覆盖 `parsed` | `session_description.rs:18–28 / 33–44`；`peer_connection\mod.rs:1356` |
| 8 | `add_track(track.clone())` | 必须显式转型：`Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>`，Rust 不做 Arc trait 对象 coercion | `examples\play-from-disk-h264.rs:157–159`；`peer_connection\mod.rs:1713` |
| 9 | 草稿写 `peers` ICE 前缀 `2` 系笔误 | 服务端只在握手后创建一次 PC；前端只接受 `type=offer`，服务端 **always offer**、客户端 answer | 前端 `lib\signaling.js:35 / 301–302`；`lib\webrtc.js:300–362` |

## 3. 偏离设计稿草稿的决策：不启用 `detach_data_channels()`

### 3.1 实测结论

设计稿 6.1 草稿照抄 hbb_common 范式开启 `detach_data_channels()`，本轮核实 fork 源码后**关闭**该选项：

- detach 模式下，库内**不再启动内部 `read_loop`**（`data_channel\mod.rs:282` 的
  `if !self.setting_engine.detach.data_channels` 条件）；
- 不启动 `read_loop` 意味着 `on_message` 永远不会被分派（`read_loop` 在
  `:308–345` 完成 message / close / error 分派）；
- hbb_common 之所以 detach，是为了拿到底层 `data::data_channel::DataChannel` 自研大消息
  分片与背压队列（`vendor\hbb_common\src\webrtc.rs:9` 注释）；阶段 1 用标准 `send_text` +
  `on_message`，不需要这套自管队列；
- 不 detach 时 `send_text` 不受影响：`send` 走 `self.data_channel.lock()` 里已在
  `handle_open` 注入的底层通道（`data_channel\mod.rs:273–277 / 387–396`）；不 detach 仅会在
  `on_open` 后打一条 `didn't Detach` 的 warn（`:241–245`），不影响收发。

### 3.2 落地口径

- `SettingEngine` 只保留 `set_ice_multicast_dns_mode(Disabled)` 与 `set_ip_filter`；
- 阶段 1 的 `control.on_message` 将在 #35 挂接上行命令分类器；`pointer` 通道阶段 1 空挂。
- 若阶段 3 出现大消息（剪贴板分块 / 长日志）需要自研背压，再评估是否切回 detach 路径。

## 4. 落盘文件清单与职责映射

| 文件 | 职责 | 关键公开符号 |
| --- | --- | --- |
| `webrtc\mod.rs` | PC 装配、offer/answer/ICE 生命周期、状态回调、会话清理 | `MediaSession`、`MediaState`（New/Connected/Failed/Closed） |
| `webrtc\track.rs` | VP8 capability、下行轨道封装、Sample 装配写轨 | `VideoTrack`（`new / as_track_local / write_sample`） |
| `webrtc\channels.rs` | 双 DataChannel 创建、open/close 日志、下行发送 | `ControlChannels`（`open / send_control / control / close_all`） |
| `webrtc\ice.rs` | 远端候选解析、排队结构体、to_init 转换 | `RemoteCandidate`、`remote_from_json`、`to_init`（pub(crate)） |

### 4.1 构造时序（服务端 offerer，#35 驱动）

1. `MediaSession::new().await?`：建 PC、挂 VP8 轨道、建双 DC、注册 ICE / 状态 / 空 DC 回调；
2. `make_offer().await?`：`create_offer(None)` → `set_local_description` → 把 `offer.sdp`
   交 #35 包装成 WS 行发出；
3. 本地候选由 `on_ice_candidate` 逐个序列化进 `ice_rx`，#35 包装转发；`None`（收集结束）
   不发空消息；
4. 前端 answer SDP 回到会话 → `apply_answer` → 置 `remote_applied` → 冲刷排队候选；
5. PC 进入 `Connected` → `state_rx` 收到 `Connected`，#35 启动 #36 采集循环并下发
   `stream_info` / `connection` / `pipeline`；
6. `Failed` / `Closed` → `state_rx` 收到终态，#35 停循环、关 DC、关 PC、关 WS。

## 5. #35 / #36 对接接口面（落码直接引用）

### 5.1 #35 信令层调用 `MediaSession` 的方法

```rust
use crate::webrtc::MediaSession;
use crate::webrtc::ice::remote_from_json;

// 构造：SESSION_OK 之后
let mut media = MediaSession::new().await?;

// 生成 offer，取描述文本交给 signaling::sdp_line(SERVER_PEER_ID, "offer", &offer.sdp)
let offer = media.make_offer().await?;
let offer_line = signaling::sdp_line(crate::signaling::SERVER_PEER_ID, "offer", &offer.sdp);

// 本地候选：循环从 ice_rx 取裸 init JSON，交给 signaling::ice_line(SERVER_PEER_ID, &value)
while let Some(json) = media.ice_rx().recv().await {
    let value: hbb_common::serde_json::Value = hbb_common::serde_json::from_str(&json)?;
    let ice_line = signaling::ice_line(crate::signaling::SERVER_PEER_ID, &value);
    // WS 行发送由 #35 的 WS 任务持有 writer 完成
}

// 前端 answer：signaling::parse_ws_line 得到 Sdp{ sdp_type, sdp }
media.apply_answer(&sdp_type, &sdp).await?;

// 前端候选：解析出 Ice{candidate: Value}
if let Some(c) = remote_from_json(&candidate) {
    media.add_remote(c).await?;
}

// 状态订阅：watch::Receiver<MediaState>，changed() 等待新值
let mut state_rx = media.state_rx();
// Connected => 启动 pipeline、下发一次性下行；Failed/Closed => 清理

// 下行 JSON：dispatch::output 构造好整行后走 control 通道
media.channels().send_control(&line).await?;

// control.on_message 挂上行命令分类器（#35）：
let _dc = media.channels().control().clone();

// 清理
media.close().await?;
```

### 5.2 #36 采集编码循环调用轨道

```rust
// media.video() 返回 &VideoTrack；duration 固定 cfg.spf()（repeat 帧同口径，设计稿 9.2 #4）
media.video().write_sample(bytes, cfg.spf()).await?;
// write_sample 内部已构造 Sample：timestamp=now、duration=spf、其余字段零值；
// 轨道未绑定（offer 前 / PC Closed）时库内静默 Ok 不报错；绑定后由 packetizer 分包。
```

### 5.3 WS 与 DataChannel 消息分工（边界不可串）

| 通道 | 承载内容 | 构造 / 序列化责任方 |
| --- | --- | --- |
| WS 信令 | `HELLO` / `SESSION` / `SESSION_OK` / 双向 SDP / 双向 ICE candidate | `signaling\line.rs`；#35 取 `MediaSession` 产物包装 |
| DC `control`（有序可靠） | 下行 `pipeline` / `stream_info` / `stream_stats` / `connection` JSON；上行 `START_VIDEO` / `STOP_VIDEO` / `_stats` / `ping` 等命令 | 下行 `dispatch\output.rs`；上行分类 `dispatch\commands.rs` |
| DC `pointer`（无序零重传） | 阶段 1 空挂；阶段 2 运动类输入（绝对/相对移动、按键掩码、滚轮） | 阶段 2 接入 |

## 6. 阶段 1 已知边界与遗留项

| 项 | 阶段 1 口径 | 后续阶段 |
| --- | --- | --- |
| RTCP 读取循环 | `add_track` 返回的 `RTCRtpSender` 存为 `_sender`，未启动 `sender.read` 循环；NACK / PLI 由默认拦截器在收包侧响应，未做主动周期关键帧 | 阶段 3 QoS 补齐 sender RTCP 读取与静态刷新 |
| `pointer` 通道句柄 | 结构体保留字段但阶段 1 无读取方法；匹配前端按 label 分流到 `_motion_channel` 的分支 | 阶段 2 输入面接入 |
| `ice_tx` 字段 | 仅用于会话 drop 时关闭发送侧、让 `ice_rx` 收到 None；阶段 1 无方法读取，被 `main.rs` 顶部 `#![allow(dead_code)]` 暂存标注覆盖 | #35/#36 接线完成后随暂存标注一并清理 |
| 动态分辨率 / 帧率 / 码率切换 | 不实现；编码配置由 #36 启动时一次定型 | 阶段 3 工具栏状态对接 |
| 音频 m-line | 不创建；前端找不到 audio mid 返回 null 不报错 | 不计划（云游戏画面优先） |
| SDP munging | 服务端不做；`sps-pps-idr-in-keyframe=1` 等由前端 answer 侧自行补齐（`webrtc.js:311–318`） | — |

### 6.1 暂存标注移除条件

- `main.rs:20` 的 `#![allow(dead_code)]` 与各旧骨架 `mod.rs` 顶部 `#![allow(unused_imports)]`
  是 #33 骨架期暂存；#35 信令、#36 采集循环接线完成后必须移除，重新编译让真实死代码 /
  未用导入重新暴露，不得长期保留掩盖问题。

## 7. 编译口径与遗留 warning（非阻塞，勿当错误）

本轮 `build-ws.log` 中 stream-server 自身零 warning；下列残留项全部来自 vendor 依赖，与
#34 改动无关：

| 来源 | warning | 处置 |
| --- | --- | --- |
| `vendor\hbb_common\src\password_security.rs:213` | `secretbox` 冗余导入 | 沿用 vendor 现状，阶段 4 服务化时统一清 |
| `vendor\hbb_common\src\fingerprint.rs:251 / 283` | unknown lint `static_mut_refs` | rustc 1.75 无此 lint，忽略 |
| `vendor\scrap\src\common\vpxcodec.rs:181` | unreachable pattern | 沿用裁剪后现状 |
| `vendor\scrap` | `cargo:rustc-check-cfg requires -Zcheck-cfg flag` | 构建脚本提示，不影响产物 |

## 8. 取证基线（fork 49c89bd 本地 checkout）

- checkout 根：`C:\Users\user\.cargo\git\checkouts\webrtc-97786153e5a03210\49c89bd`
- 上游缓存前缀：`C:\Users\user\.cargo\registry\src\rsproxy.cn-0dccff568467c15b\`
- 主仓范式参照：`vendor\hbb_common\src\webrtc.rs:633–834`（PC 装配、弱引用防循环、终态判定）
- 前端契约根：`C:\Users\user\Desktop\selkies-webclient\addons\selkies-web-core\`
