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

## 4. 编码器只有一个（#94 之后不再是）

当时 `pipeline/encoder.rs` 只构造一个 `VpxVideoCodecId::VP8`，WebRTC 侧也是一条
VP8 轨道，所以那一行的正确形态是**陈述**而不是选择：`VP8 · 服务端唯一`。

#94 把它做成了真的选择（VP8 / VP9 两个 libvpx 编码器），代价与约束写在
[`codec-choice.md`](codec-choice.md)：编解码器是协商属性，选完要重连一次。
H.264 / AV1 仍然没有——不是没做，是这个构建里编不出来。

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

- **硬件编码（NVENC / AMF / QSV）**：能力探测与 `--encoder` 策略已落地（见
  [`hardware-encoding.md`](hardware-encoding.md)），但真正的编码器还没编进二进制——
  要启用 `scrap/hwcodec` 并处理 D3D11 纹理路径，是独立工程
- **H.264 / AV1**：H.264 缺软件编码器与 `sprop-parameter-sets`，AV1 无编码器
- **paint-over（区域重绘）**：Turbo 与「覆盖层 CRF/连发」的共同前提
- **CRF 的 QoS 交互**：CQ 下 QoS 只调上限，画面质量恒定——这是有意的，但
  "码率爆了自动降质"这条保护在 CQ 模式下等于不存在，值得单独评估

## 7. #95：软编的实时参数组（证据 + 实测）

「优化软编」在动手前先排除了三个**看起来该改、其实改不动**的点。三条都读的是
vendored libvpx 1.15.2 源码，不是凭印象：

| 曾经的设想 | 源码结论 | 结果 |
| --- | --- | --- |
| 关掉 lookahead 降延迟 | VP8 默认就是 `g_lag_in_frames = 0`（`vp8/vp8_cx_iface.c:1346`），但 **VP9 默认 25**（`vp9/vp9_cx_iface.c:2197`） | VP8 无操作；**VP9 是真的 bug**，60fps 下白送 0.42 s 延迟（30fps 下 0.83 s），实测 `effective: lag=25` |
| 加 token partitions 提吞吐 | VP8 是**按 MB 行并行**（`vp8/encoder/ethreading.c:490`，条件 `oxcf.multi_threaded > 1`），与 partition 无关；且本构建 `CONFIG_ONTHEFLY_BITPACKING` 关闭，多分区只多花字节 | 不改，改了反而更慢 |
| 用 `VP8E_SET_STATIC_THRESHOLD` 跳过静态块 | 该值只进 `extra_cfg.static_thresh`，真正消费它的是 `vp8_set_roi_map` 的 segment breakout（`onyx_if.c:1552` 检查 `use_roi_static_threshold`，该标志只在设 ROI map 时置位） | 单独设它是死旋钮，不写进代码 |

真正有效的四个（`VpxTuning`，默认即调优组）：

1. **`rc_buf_sz` 6000 → 1000 ms**（初值 600 / 目标 700）。6 秒的码控缓冲让控制器在
   静态桌面上攒余量、再在下一次画面突变时一次花掉，瞬时码率因此大幅越界
2. **`g_error_resilient` = 0**。原来的 `VPX_ERROR_RESILIENT_DEFAULT` 让每帧自包含，
   而链路本身已有 NACK + PLI 驱动的 `force_keyframe` 兜底
3. **VP9 `VP9E_SET_TUNE_CONTENT = SCREEN`**：调色板 + 帧内块拷贝，libvpx 唯一一个
   为「扁平色块 + 文字」设计的工具集
4. **`VP8E_SET_MAX_INTRA_BITRATE_PCT = 300`**：1080p 关键帧原本 100–200 KB，一次
   砸进接收端抖动缓冲

### 7.1 实测（1920×1080、目标 60 fps、屏幕有动画）

静态桌面上 DXGI 几乎每帧返回 WouldBlock，测出来的是「重复编码同一帧」，没有意义。
所以基准跑在一扇自绘动画窗口上（`motion.ps1`），每帧全屏换像素。

| 组 | 实测 fps | capture | convert | encode | 实际码率（目标 2073） | 最大关键帧 |
| --- | --- | --- | --- | --- | --- | --- |
| VP8 legacy（改动前） | 28.1 | 23.4 ms | 2.01 ms | 9.91 ms | 3580 kbps（+73%） | 12588 B |
| **VP8 tuned（改动后）** | **29.3** | 22.1 ms | 1.82 ms | **9.56 ms** | **2854 kbps（+38%）** | **7567 B（−40%）** |
| VP9 tuned | 22.0 | 24.2 ms | 2.04 ms | 18.47 ms | 1109 kbps（−47%） | 24636 B |

读法：

- **码率越界从 +73% 收到 +38%，关键帧小 40%，编码耗时基本不变**（9.91→9.56 ms，
  在 VM 噪声范围内）。这就是这组参数买到的东西：同样目标码率下更贴预算。
- VP9 在这台机器上编码耗时翻倍、帧率掉到 22，**桌面场景仍应留在 VP8**。
- **真正的瓶颈不是编码器**：capture 22–24 ms 已经吃掉了 60fps 的 16.7 ms 预算。
  这台是 VMware 虚拟机（`VMware SVGA 3D`，2 插槽 ×2 核 = 4 vCPU），DXGI 复制走
  软件路径。换成真显卡后 capture 通常 3–8 ms，1080p60 才谈得上「够不够」。
  也就是说：**在这台机器上加硬编也不会变快**，这是选型结论，不是自谦。

### 7.2 复现方式

```bat
:: 同一份二进制内 A/B，别换构建——换构建会把「参数变了」和「机器忙了」混在一起
bench-capture.exe 15 60 --tune legacy
bench-capture.exe 15 60 --tune tuned
bench-capture.exe 15 60 --tune buf,er,screen,peak   :: 单变量推进
:: 线上：--tune legacy 复现旧参数，日志里 `encoder tuning | ...` 一行写明当前组
```

`bench-capture` 的输出里 `effective: threads=.. lag=.. buf=../../..ms err_res=..`
是从**跑起来的编码器**读回来的（`vpx_codec_enc_config_get`），不是把请求回显一遍
——正是靠它发现 VP9 的 `lag=25`。

### 7.3 触过的坑

- **`call :run "a" buf,er` 里的逗号会被 `call` 当分隔符**：`%~2` 只拿到 `buf`，
  「buf,er」组实际只开了 buf 一项，表格里那一行是假的。分组 A/B 要么一个参数一个
  子程序，要么别用逗号。
- **`Form` 不 `Show()` 就不会绘制**：第一版动画窗口只 `Add_Paint` + `DoEvents`，
  进程活着、日志正常、DXGI 一帧新画面都没有，测出来跟静止桌面一模一样。**看起来
  成功、其实没跑**，是这类基准最容易骗人的失败方式。
