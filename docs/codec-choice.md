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

因此选择不能放在 `StreamSettings` 里：那是每次连接从 `Config` 重建的，用户点
VP9 触发的重连会把选择打回默认。它落在 `pipeline::codec`（进程级
`AtomicU8`），`--video-codec vp8|vp9` 决定进程从哪里开始。

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

| | 编码 CPU | 同码率画质 |
|---|---|---|
| VP8 | 基准 | 基准 |
| VP9 | 约 2x（libvpx real-time，`-cpu-used 7` + row-mt） | 文字/细线更锐 |

1080p60 上 VP9 会明显吃满核心（这台 4 vCPU 的机器尤其）。所以默认仍是 VP8，
VP9 是用户在「机器扛得住、想要更清楚」时自己选的。`GET /api/v1/health` 的
`stream.codec` 会告诉你当前在哪一边。

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
