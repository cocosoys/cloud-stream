# 视频设置里那五个「改不动」的控件（#92）

用户报告：「编码器」「编码器码率控制模式」「视频码率」「覆盖层 CRF」「覆盖层连发」
都改不了。五条，三类原因，只有一类是 bug。

| 控件 | 之前为什么动不了 | 现在 |
|---|---|---|
| 视频码率 | **bug**：服务端没宣告 `video_bitrate`，面板判禁用看的是 `min === max`，两个 `undefined` 相等 → 灰 | 宣告 `min 200 / max 40000 / default 8000`；`vb,<kbps>` 一直通到 `set_bitrate_override` |
| 编码器码率控制模式 | 只有 CBR：`allowed:["cbr"]` → 面板把下拉框判灰 | 真做了 CQ 模式，`allowed:["cbr","crf"]`，可选 |
| CRF | 上一条的从属项：码控没有 CRF，这个滑块永远不出现/不可用 | CQ 模式下的量化器，`_crf,N` 现在真的落到编码器 |
| 覆盖层 CRF | **bug 同视频码率**（`undefined === undefined`）＋**没有对应能力**：本管线没有区域重绘，paint-over 不存在 | 服务端宣告 `locked:true`，面板整行隐藏 |
| 覆盖层连发 | 同上 | 同上 |
| 编码器 | 只有一个实现，`allowed:["vp8enc"] + locked`；面板仍然画一个灰掉的下拉框 | 改成**事实行**：`VP8 · 服务端唯一`，不再是"坏掉的控件" |

## 1. `min === max` 与 `undefined === undefined`

面板的禁用判据有两套：

- `renderableSettings`（`Sidebar.jsx:1042`）：`min !== undefined && max !== undefined && min === max` → 整行隐藏
- 控件自身：`disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}`

第二套没有"键不存在"这一支。服务端不宣告 `video_bitrate`，于是 `undefined === undefined`
为真，滑块**看起来像坏掉**，而不是像"服务端没提供"。这已经是本类问题的第三次
（`audio_bitrate` 在 #91、`scaling_dpi` 在 #90）：**面板的"没有这个键"必须被服务端
显式回答**，否则面板只能用本地默认值编一个答案。

## 2. 码控：libvpx 的 `rc_end_usage` 不能热改

CQ（constant quality）在 libvpx 里是 `rc_end_usage = VPX_CQ` + 量化器钉住
（`rc_min_quantizer = rc_max_quantizer = cq_level`，VP8 还要 `VP8E_SET_CQ_LEVEL`）。
这些字段由 `vpx_codec_enc_config_set` 校验，和初始化时的值不一致就拒绝 ——
所以模式**必须在 open 时定**。

因此实现是：

- `StreamSettings` 增加 `rate_control()` / `crf()`（原子变量，默认 CBR / 25）
- `VideoEncoder::new(..., rc, crf)` 把它们交给 `VpxEncoderConfig.rc / cq_level`
- 采集循环每拍比对 `settings.rate_control() != encoder.rate_control()`（CQ 下再比 crf），
  不一致就走**和改分辨率同一条 `InnerOutcome::Rebuild`** —— 重开采集与编码器
- CQ 下 `set_bitrate_kbps` 只搬上限（新的 `set_target_bitrate`），不会像
  `set_quality(ratio)` 那样把量化器重算一遍、把 CRF 悄悄抹掉

默认仍是 CBR，所以没人动这个开关时，编码行为与 #92 之前逐字节相同。

## 3. 覆盖层（paint-over）不存在

`video_paintover_crf` / `video_paintover_burst_frames` / `use_paint_over_quality`
是 Selkies 的增量重绘参数：静态区域发一次高质量静帧，之后不再编码。本管线每一拍
要么编新帧、要么重编上一帧的 YUV，**没有区域重绘**。宣告成可调就是给用户一个
"拖了没反应"的滑块（这正是 #91 修掉的谎言），所以三把键都 `locked:true`，
面板按 `isRenderable` 的规则整行隐藏。

要让它们真的有意义，得先做区域重绘（独立的大改），或把 Turbo/paint-over 一起
重新定义成本管线的"静态画面重复帧节流"。

## 4. 编码器只有一个

`pipeline/encoder.rs` 只构造一个 `VpxVideoCodecId::VP8`，WebRTC 侧也是一条 VP8
轨道（`main.rs`：VP8 track + VP8 payloader）。要真的能选 VP9，需要动 RTP 打包器
与 codec 协商，不是面板能决定的事。所以这一行的正确形态是**陈述**而不是选择：
`VP8 · 服务端唯一`，鼠标悬停说明"统计信息的「编码」读的是同一个事实"。

## 5. 验证

- `cargo test -p stream-server`：107 通过，其中新增
  - `cq_encoder_opens_at_the_panel_bounds`：在 crf 5/25/50 下真的打开一个 CQ 编码器
    （libvpx 会整份校验配置，open 成功即配置合法），并确认 CBR 仍能打开
  - `rate_control_defaults_to_cbr_and_clamps_crf`：默认值、clamp（0→4，1000→63）
  - `wire_names_round_trip`：`cbr`/`crf`/`cq` 与拒绝 `vbr`
  - `rate_control_pair_reaches_the_settings`：`_rc,crf` / `_crf,25` 不再是 B 组噪音
  - `settings_describe_the_encoder_and_audio_the_pipeline_has`：断言两个滑块有真实
    `min < max`、paint-over 三把键是 locked
- 线上：`pid=8508`，日志 `VP8 encoder opened: … rc=cbr crf=25`，`new encoder: … rc: Cbr, cq_level: 25`
- 前端：`index-B0_6Avev.js` / `index-BlzAENbX.css`（`.setting-fact`）

## 6. 还没做的

- **VP9 / 硬件编码**：要动 RTP 轨道与协商
- **paint-over（区域重绘）**：Turbo 与「覆盖层 CRF/连发」的共同前提
- **CRF 的 QoS 交互**：CQ 下 QoS 只调上限，画面质量恒定——这是有意的，但
  "码率爆了自动降质"这条保护在 CQ 模式下等于不存在，值得单独评估
