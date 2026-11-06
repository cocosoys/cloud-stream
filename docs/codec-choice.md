# 编码器从「一个」变成「两个」（#94）

面板的「编码器」这一行以前只能是陈述句：`VP8 · 服务端唯一`。这次它能选了，
但代价不是加一个下拉框，而是「编解码器不能热切换」这件事被摊开。

## 1. 为什么以前只有一个——以及这次为什么可以两个

`vendor/scrap` 的 `VpxEncoder` 从来都同时构建 **VP8 与 VP9** 两个 libvpx 编码器
（`vpx_codec_vp8_cx()` / `vpx_codec_vp9_cx()`，由同一个 `VpxEncoderConfig.codec`
选），webrtc-rs 也带 VP9 payloader（`rtp_codec.rs:70` 的 `payloader_for_codec`
里 `MIME_TYPE_VP9` 分支）。所以缺的从来不是编码器，而是**整条链路上的三个字符串**：

1. `pipeline/encoder.rs` 里写死的 `VpxVideoCodecId::VP8`；
2. `webrtc/track.rs` 里写死的 `video/VP8` —— 这一个决定了 SDP 的 `a=rtpmap`，
   也决定了 `TrackLocalStaticSample::bind` 会挑哪个 payloader；
3. `dispatch/output.rs` 宣称的 `allowed: ["vp8enc"]`。

三个都改，VP9 就通了。**H.264 / AV1 不在这次范围内**：这个 `scrap` 的 `hwcodec`
feature 没启用（没有软件 H.264 实现），而 H.264 还需要在 SDP 里带
`sprop-parameter-sets`（编码器输出里才有 SPS/PPS），AV1 连编码器都没有。
菜单里放一个编不出来的选项，等于再造一次 #91 的「面板说 H.264，实际是 VP8」。

## 2. 编解码器是**协商**属性，不能热切换

这是整件事的核心约束，也是为什么切换会掉线 2 秒：

- codec 决定 m-line 的 `a=rtpmap`，协商完成后这条 m-line 就固定了；
- 它同时决定 `bind` 时挑哪个 payloader（VP8 与 VP9 的 RTP 描述符不通用，
  发错了浏览器解出来是噪声，不是「降级」）；
- libvpx 侧它决定实例化哪个编码器上下文，与 `rc_end_usage` 一样属于 open 时定。

这个管线没有 renegotiation 路径（一个会话只有一次 offer/answer），所以切换只有
一条路可走：**记下选择 → 关掉会话 → 客户端自己重连 → 用新 m-line 重新协商**。

因此选择不能放在「按连接重建的东西」里：每次连接都从 `Config` 重建的
`StreamSettings` 会把用户点 VP9 触发的重连打回默认。

#94 的做法是一个进程级 `AtomicU8`（`pipeline::codec`）。#110 把它挪进了**程序**
（`SharedPipeline.settings`）：进程级会让一个观众的菜单改掉整台机器的每一块屏，
那不是设置面板而是广播。

但「程序」也不像当初设想的那样长命 —— 它活到最后一个会话离开为止，而切换编码器
恰恰会赶走最后一个会话。这条副作用在 #110 之后才显形，见 §7。

## 3. 实现要点

| 位置 | 改动 |
|---|---|
| `pipeline/codec.rs`（新） | `VideoCodec::{Vp8,Vp9}`、`wire()`/`encoder_id()`/`sdp_fmtp()`/`vpx_id()`，进程级 `current()`/`set()`（`set` 返回是否真的变了） |
| `pipeline/encoder.rs` | `VideoEncoder::new(..., codec)`：`codec.vpx_id()` 进 `VpxEncoderConfig`，日志里带 codec 名；新增 `codec()` 访问器 |
| `pipeline/mod.rs` | 采集循环多一条 Rebuild 判定（codec 变了就重开编码器）；IVF dump 按 codec 写 `VP80`/`VP90` 魔数 |
| `pipeline/sample_map.rs` | 接受 `Vp9s` union —— **这一条不改就是静默黑屏**：编码器在出帧、`encoded_ok` 在涨、轨道上什么都没有 |
| `webrtc/track.rs` | `VideoTrack::new(codec)`，capability 的 mime/fmtp 来自选择；VP8 分支与改动前逐字节相同 |
| `config.rs` / `main.rs` | `--video-codec`，未知值保留 VP8 并 warn |
| `dispatch/output.rs` | `encoder.allowed = ["vp8enc","vp9enc"]`、`locked: false`、`value` 是当前值；`stream_info.codec` 跟随 |
| `signaling/websession.rs` | `SETTINGS` 批处理接受 `encoder` → 记选择 → 回 `server_settings` → `registry.kick("video codec changed")` |
| `http/health.rs` | `codec` 字段跟随选择 |

两处值得单独说的细节：

**强制关键帧改成编码 flags。** `force_keyframe` 原来用
`VP8E_SET_FRAME_FLAGS`，那是 VP8 专属控制项；VP9 上下文会返回
`VPX_CODEC_UNSUP_BITSTREAM`。而这条路径是「每 10 秒一次关键帧」和「PLI 重锚」
唯一入口——坏掉不会报错，只会让参考链慢慢漂（画面渐暗/糊、编码耗时上升）。
现在改成在 `VpxEncoder` 上挂一个 `force_kf` 标记，下一次 `vpx_codec_encode` 时
带上 `VPX_EFLAG_FORCE_KF`，两端通用。

**应答里没有这个 codec 就回退。** 浏览器不支持 VP9 时不会报错，它会把视频
m-line 的端口置 0（或在 `a=rtpmap` 里不列该 codec），ICE 照样连通（数据通道与
codec 无关），于是会话「健康」但画面全黑。`refused_video_codec()` 扫应答的
video 段：没这个 codec → 记 error、把进程打回 VP8、踢会话让客户端用 VP8 重连。
`refused_video_codec` 有单测，三种形态（接受 / 端口 0 / 列了别的 codec）。

## 4. 面板行为

- `allowed` 两个、`locked: false` → 下拉框出现（一个选项时才降级成事实行）。
- 选了之后服务端回 `server_settings`（`value` 已是新值），再关 socket；
  客户端 2 秒内自动重连。
- `Sidebar.jsx` 里 `encoder` 的同步顺序**反过来了**：以前「本地存的优先」，
  现在「服务端说的优先」。理由是这个服务端已经会拒绝选择（见上一节的回退），
  存的偏好压过事实就等于重复 #91 的谎。回执总是在服务端应用之后才到，所以
  不会跟用户抢。
- 顺带：`canPlayEncoder()` 仍然过滤菜单，所以绝大多数不支持的浏览器在选之前
  就被挡掉了，回退是第二道闸。

## 5. 成本

| | 编码耗时（1718x926 实测） | 同码率画质 |
|---|---|---|
| VP8 | 21–23 ms/帧（22.6 fps、547 kbps） | 基准 |
| VP9 | 7–8 ms/帧（30.6 fps、224 kbps） | 文字/细线更锐 |

**这张表原来写反了**（原文是「VP9 约 2x」，即更贵）。实测方向相反，而且差距不是
四舍五入能解释的，原因有两条：

- VP8 的 `g_threads=4` 并不真的并行：libvpx 的 VP8 real-time 编码器只在开
  token partitions 时才多线程，实测只用了 ~3 个 worker；
- 这边给 VP9 开了 `row-mt=1` + `tile-columns=4` + `VP9E_CONTENT_SCREEN`（屏幕内容
  工具，对桌面这种大片静态区域特别有效），VP8 没有等价开关。

#117 起默认**改成 VP9**（`--video-codec vp9`）：实测它在这台机器上更便宜、画面
也更清楚，而桌面这种大片静态内容正是 `VP9E_CONTENT_SCREEN` 的目标。留在 VP8 的
理由只剩兼容性——VP8 是 WebRTC 的强制实现，任何浏览器都能解；VP9 需要在 SDP 里
单独协商，也要求 WebCodecs 有对应解码器。这条风险仍被上一节的 `refused_video_codec`
兜着：解不了 VP9 的浏览器会被自动降回 VP8，所以默认值是起点，不是单行道。想更清楚、更省带宽时选 VP9，`GET /api/v1/health` 的 `stream.codec` 会
告诉你当前在哪一边。

（1080p60 没有再单独复测，上面这组数是开发机 1718x926、8 s x 4 轮交替的基准。）

## 6. 验证

- `cargo test -p stream-server --bin stream-server`：**127 通过**（+10）
  - `vp9_opens_and_reports_itself`：真的在 320x240 开一个 VP9 编码器
    （不是断言「列表里有」——那正是会骗人的写法）
  - `panel_ids_and_wire_names_round_trip` / `unsupported_codecs_are_refused`
  - `setting_the_same_codec_reports_no_change`：同一档不触发重连
  - `the_answer_decides_whether_the_codec_pick_survives`：三种应答形态
  - `settings_describe_the_encoder_and_audio_the_pipeline_has`：`allowed` 两项、
    `locked: false`
- 线上：日志出现 `VP9 encoder opened: …`、`new encoder: VpxEncoderConfig { … codec: VP9 … }`，
  `GET /api/v1/health` 的 `stream.codec` 为 `vp9`

## 7. 编码器选择必须活过它自己触发的那次重连

**症状**：WebSocket 传输下把编码器选成 VP9，页面一直停在
`WebSocket disconnected. Attempting to reconnect...`，一帧都看不到。

**循环**（每一步单独看都正确，合起来是死锁）：

1. 页面发 `SETTINGS,{"encoder":"vp9enc"}`；
2. 程序把 codec 改成 VP9（`StreamSettings::set_codec` 返回 `true`）；
3. 握手期定下的 m-line 与 libvpx 上下文不能中途改，所以 `ws_media` 关掉这条
   连接要求重协商（§2）；
4. 单观众时这条连接就是**最后一个持有者**，`PipelineRegistry::release` 摘掉程序、
   停掉采集；
5. 页面 1 秒后重连，`acquire` 出来的新程序是 `StreamSettings::from_config(cfg)` ——
   CLI 默认（当时是 **VP8**；#117 起默认 VP9，同一个环只是编码器名字对调）；
6. 页面读到 `server_settings.encoder = vp8enc`，把它记着的偏好再推一次，回到第 1 步。

关键的错误前提写在 `settings.rs` 的注释里：「程序 outlive 它上面的每个会话」。
这只在**还有别的会话挂着**时成立。单观众页签是最常见的情况，于是断言在最常见的
路径上失效。

**修法**：`PipelineRegistry` 按屏记住上一个程序的参数（`StreamPrefs` 值快照），
`release` 摘下程序时保存、`acquire` 建新程序时套用。日志上能看到这一对：

```
pipeline 0: last session left, capture loop stopped (settings kept for the next program: codec vp9)
pipeline 0: inheriting the previous program's settings (codec vp9, 60 fps, 1920x1080, audio on @ 64000 bps)
```

两条容易写错的地方：

- **不能拿 `StreamSettings::clone()` 当快照。** 它的 `Clone` 是浅拷贝，字段全是
  `Arc<Atomic*>`，克隆体和原体共用同一批原子量 —— 存进注册表等于存了个别名。
  `prefs_are_a_snapshot_not_an_alias` 盯着这个错误。
- **快照按屏记，不是全局。** 主屏选了 VP9 不该让第二块屏跳过协商直接上 VP9：
  那是另一条程序、另一个浏览器视图（`a_pick_is_not_inherited_across_displays`）。

**没进快照的东西**：`vb,<kbps>` 的码率覆盖是 one-shot（`PipelineHandle` 里取走即
清零，作用于下一次编码器初始化），不是持久参数；`out_width/out_height` 是采集
循环回报的事实；`dirty` 是「客户端动过没有」的记账。进程重启仍然回到 CLI 默认 ——
这是有意的：`--video-codec` 是机器级的起点。

**验证**（exclusive 策略下，页签持着租约时探针只是 viewer，`encoder` 属于程序参数、
会被 `touches_program_settings` 拒绝，所以要先
`POST /api/v1/control/session/take/<id>` 借租约，测完还回去）：

- 首页签在 vp9 时重连，`server_settings` 报 `vp9enc`（不再回落）；
- 以 controller 身份重推同一条 `SETTINGS,{"encoder":"vp9enc"}`：3 秒内收到 182 帧、
  帧头 `[1] >> 4 == 3`、连接未关闭 —— 即页面重连后的那次重推不再引发新的断开；
- `cargo test -p stream-server`：**299 通过**（+4：`a_codec_pick_outlives_the_reconnect_it_forces`、
  `a_pick_is_not_inherited_across_displays`、`prefs_restore_onto_a_fresh_program`、
  `prefs_are_a_snapshot_not_an_alias`）。
