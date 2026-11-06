//! #106: `GET /api/websockets` — 画面走 WebSocket + WebCodecs 的那条线。
//!
//! # 为什么要有这条线
//!
//! 这个 crate 之前只有 WebRTC 一种传输，而 WebRTC 必须有 UDP（或者 TURN 中继）。
//! 局域网里这从来不是问题，一旦要穿过反向代理、云主机安全组或者别人的公司网络，
//! 就是一个绕不过去的硬伤 —— 那些地方能给的往往只有一条 443。
//!
//! 前端的这条通路是现成的：`selkies-ws-core.js` 一万多行，WebCodecs 解码、分行、
//! 音频、剪贴板全都在里面；`lib/mode-switch.js` 还有切换入口。所以这里要做的不是
//! "写一个客户端"，而是**按它认的格式把帧喂过去**。整个模块的存在理由就是这句话。
//!
//! # 线上格式（前端是唯一的规范）
//!
//! 一帧二进制 = 12 字节头 + 裸码流。头按 `selkies-ws-core.js:2417` 的顺序读，
//! 一个字节都不能换位：
//!
//! | 偏移 | 含义 |
//! |---|---|
//! | 0 | 帧类型。整帧恒 `0x04`（`0x01` 是音频，`0x03` 是 JPEG 分行） |
//! | 1 | `codec_id << 4 \| is_key`，见 `lib/wire-codecs.js:23` |
//! | 2..4 | frameId（大端 u16，回绕） |
//! | 4..6 | y（行偏移），整帧恒 0 |
//! | 6..8 | 宽（大端 u16） |
//! | 8..10 | 高（大端 u16） |
//! | 10..12 | reference：本帧依赖的上一帧 frameId，前端用它判断中间有没有丢帧 |
//!
//! `reference` 不是装饰。前端的解码闸门（`lib/decode-gate.js:99`）在
//! `reference !== frameId` 时才做丢帧跟踪，发现依赖缺席就不解这一帧、并要求关键帧。
//! 整帧流里每帧都依赖上一帧，所以填 `frameId - 1` 是唯一正确的答案；填 `frameId`
//! 等于把这套保护关掉，丢一帧之后画面会一直糊到下一个关键帧。
//!
//! # 关键帧位为什么要自己算
//!
//! 编码器知道，但传不过来：管线把每帧摊成 `Sample { data, beat }` 交给
//! [`VideoFanout`]，`key` 标志在 `forward_chunks` 里只用于统计，没进这个结构
//! （那是另一个模块的事，改它会把两条传输搅在一起）。所以这里从码流首位反推，
//! 两个编解码器的规则都是白纸黑字：
//!
//! * VP8：帧标签 bit0，`0` 表示关键帧（RFC 6386 §9.1）
//! * VP9：无压缩头里 bit2 才是 frame_type，`0` 表示关键帧
//!
//! # 它和 WebRTC 会话的关系
//!
//! 不是二选一。两者都通过 [`PipelineRegistry`] 拿同一路程序（一次采集、一次编码），
//! 各自往 [`VideoFanout`] 挂自己的下行。所以"一个页签在 WebRTC、另一个在
//! WebSocket"是天然支持的 —— 这不是设计的巧合，是 fan-out 本来就该有的形状。
//!
//! # 上行：`0x02` 是麦克风（#110）
//!
//! 页面把麦克风编成 `[0x02] + Opus`（24 kHz 单声道、20 ms 一帧，`selkies-core` 的
//! mic worker 写死这组参数），服务端解到 48 kHz 立体声后写**虚拟录音设备**。写哪里
//! 由 `--mic-device` 在启动时决议一次（`platform/virtual_mic.rs`），不是默认扬声器：
//! 写默认扬声器会被下行 loopback 原样采回，就是那个"开麦听到自己"。
//!
//! 目标不可用时回 `MICROPHONE_DISABLED`（页面会关麦），并且**只回一次** —— 电平条
//! 按帧发，每帧回一条文本会把 socket 变成回音壁。`0x06`（摄像头）仍然是回绝，反向
//! track 全项目未实现。

use std::net::IpAddr;
use std::sync::Arc;

use anyhow::Result;
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame, Message},
    },
    WebSocketStream,
};

use crate::config::Config;
use crate::dispatch::clipboard::ClipboardSink;
use crate::dispatch::commands::{classify, Action};
use crate::dispatch::input_stub::InputSink;
use crate::dispatch::win_input::WinInputSink;
use crate::pipeline::codec::VideoCodec;
use crate::pipeline::shared::{DisplayKey, SharedPipeline};
use crate::pipeline::{audio, PipelineEvent, PipelineRegistry, StreamSettings};
use crate::signaling::websession::{
    apply_settings_batch, AdmissionPermit, SessionInfo, SessionRegistry,
};

/// 12 字节头 + 裸码流。
pub const HEADER_LEN: usize = 12;

/// 帧类型字节：整帧。
///
/// **必须是 `0x04`，不是 `0x01`。** 前端的分流发生在 socket worker 里，比视频
/// worker 的 `onWire` 更早：`0x01` 一律当 Opus 音频丢给解码器，`0x03`/`0x04`
/// 才进视频 worker（`selkies-ws-core.js` 的 "0x01 goes straight down a port to
/// the decoder" 与 `t === 0x03 || (t === 0x04 && d.byteLength > 10)`）。用 `0x01`
/// 的后果不是黑屏，而是把 H.264/VP8 码流当 Opus 解——页面上既没有画面，控制台
/// 还会报一串音频解码错误。`0x04` 之后仍沿用同一套 12 字节头。
const TYPE_FULL_FRAME: u8 = 0x04;

/// 服务端音频帧：`[0x01, 0x00] + opus`（`n_red = 0` 的简单路径）。
const TYPE_AUDIO: u8 = 0x01;

/// JPEG 条纹：`[0x03, 0x00, frameId, y] + JPEG`，头 6 字节
/// （`selkies-ws-core.js:2364` 的 `onJpegStripe`，与页面侧 `8166` 一致）。
///
/// 这是"浏览器没有 WebCodecs"时唯一能出画的路径：`0x04` 的 VP8 码流需要
/// `VideoDecoder`，而 `ImageDecoder`/`createImageBitmap` 解码 JPEG 在任何现代
/// 引擎上都有。没有它，这类浏览器看到的是一个永远转圈的黑页。
const TYPE_JPEG_STRIPE: u8 = 0x03;

/// JPEG 条纹的头长度（类型 1 + 保留 1 + frameId 2 + y 2）。
const JPEG_HEADER_LEN: usize = 6;

/// 一帧切成几条。
///
/// 4 条是取中：条数越多，浏览器越能并行解、越能早显示上面几行；但每条都带一份
/// 霍夫曼表与 SOI/EOI（约 600 B），1080p 下 4 条的额外开销可以忽略，16 条就
/// 开始看得见了。
const JPEG_BANDS: u32 = 4;

/// 条纹的 JPEG 质量。
///
/// 75 是桌面文字还能看的临界点：再低一点，小字号开始糊；再高一点，1080p 单帧
/// 就超过 300 KB，而这条路径的带宽本来就不宽裕（它是兜底，不是首选）。
const JPEG_QUALITY: u8 = 75;

/// 下行队列的深度。四帧 ≈ 60fps 下 67 ms 的缓冲：够吸收一次调度抖动，又不至于
/// 让"观众慢"变成"画面滞后"。
const QUEUE: usize = 4;

/// 编解码器的线号，取自 `lib/wire-codecs.js` 的 `WIRE_CODECS`。
///
/// 这张表只有五个值且不会变（改了要同时改前端），所以手写而不是从配置推。
fn codec_id(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::Vp8 => 2,
        VideoCodec::Vp9 => 3,
    }
}

/// 这一帧是不是解码入口。
///
/// 空帧返回 `false`：一个 0 长度的 payload 下去，前端的 `decodeChunk` 会抛异常并
/// 关掉解码器，那比丢一帧贵得多。
pub fn is_keyframe(data: &[u8], codec: VideoCodec) -> bool {
    let Some(&b0) = data.first() else {
        return false;
    };
    match codec {
        VideoCodec::Vp8 => (b0 & 0x01) == 0,
        // VP9 的头按位排：`[7:6]` frame_marker、`[4:5]` profile、
        // `[3]` show_existing_frame、`[2]` frame_type。只取 frame_type，
        // 不校验 marker —— 校验错了会变成"每帧都当残差丢掉"的黑屏。
        VideoCodec::Vp9 => ((b0 >> 2) & 0x01) == 0,
    }
}

/// 一批帧排空之后，关键帧闸还该不该上。
///
/// 判断依据只能是**这一批**有没有丢帧：丢了，参考链就断了，下一帧必须能独立
/// 解码（残差帧在没有参考的情况下解出来是花屏，而花屏比黑屏更像"串流坏了"）；
/// 没丢，闸就不该上。
///
/// #116 之前这里拿的是**整条连接的累计丢弃数**（`dropped`），于是"丢过一次"
/// 变成了"永远丢"：此后每一拍，哪怕这一拍只有一帧、队列是空的，也会重新上闸，
/// 只有每 `keyframe_secs`（默认 10 秒）一个的 IDR 能过。现场表现就是
/// 「服务端 FPS 60、客户端 FPS 从 60 一路跌到 0/1 并停在那里，重连才恢复」。
///
/// 这个 bug 之所以能藏这么久，是因为它同时让**两个**数字看起来正常：
/// `health` 的 `frames_total` / `avg_mbps` 统计的是 fanout 里 `try_send` 的
/// 入队成功（`fanout.rs`），入队的 60 帧里绝大多数随后被这道闸丢掉。于是
/// "服务端 60 / 客户端 0"两个数都"没错"，只是量的不是同一件事。
fn key_gate_after_batch(queued: u32, need_key: bool) -> bool {
    need_key || queued > 0
}

/// 把一帧编码数据封成前端认的线上格式。
pub fn wire_frame(
    data: &Bytes,
    codec: VideoCodec,
    key: bool,
    frame_id: u16,
    reference: u16,
    width: u16,
    height: u16,
) -> Bytes {
    let mut out = BytesMut::with_capacity(HEADER_LEN + data.len());
    out.put_u8(TYPE_FULL_FRAME);
    out.put_u8((codec_id(codec) << 4) | u8::from(key));
    out.put_u16(frame_id);
    out.put_u16(0); // y：整帧没有行偏移
    out.put_u16(width);
    out.put_u16(height);
    out.put_u16(reference);
    out.put_slice(data);
    out.freeze()
}

/// 服务端音频帧：`[0x01, 0x00] + opus`。
///
/// 第二个字节是前端的 `AUDIO_QUIET` 标志位与 RED 计数：`0x00` 表示"没有冗余、
/// 这是一个普通帧"。前端 `extractOpusFrames` 在 `n_red == 0` 时直接
/// `slice(2)` 当 Opus 解，所以这两个字节是全部的头。
pub fn wire_audio(opus: &Bytes) -> Bytes {
    let mut out = BytesMut::with_capacity(2 + opus.len());
    out.put_u8(TYPE_AUDIO);
    out.put_u8(0);
    out.put_slice(opus);
    out.freeze()
}

/// 把一帧 BGRA 切成 [`JPEG_BANDS`] 条，各自编成一条 `0x03` 消息。
///
/// 返回的是**整帧**的所有条纹，按 y 从上到下排好 —— 页面按 `y` 把每条画到
/// 后缓冲的对应行上，最后一条覆盖到画面底边时它才呈现（`stripeMaybePresent`）。
/// 顺序本身不重要（页面对每行有重排窗口），但按顺序发能让它尽早开始解码。
///
/// 编码失败不是致命错：少一条，这一帧就是"下半截还是上一帧"，下一帧会补上。
/// 返回空 `Vec` 表示这一帧什么都没发出去。
fn encode_jpeg_stripes(frame: &crate::pipeline::raw::RawFrame, frame_id: u16) -> Vec<Bytes> {
    // JPEG 的宽高是 u16；我们的输出上限是 4080，但这条路径也可能拿到一块
    // 超宽屏的原始帧，越界就如实放弃而不是截断（截断的画面比没有画面更糟）。
    if frame.w == 0 || frame.h == 0 || frame.w > u16::MAX as u32 || frame.h > u16::MAX as u32 {
        log::warn!(
            "ws media: cannot stripe a {}x{} frame over JPEG",
            frame.w,
            frame.h
        );
        return Vec::new();
    }
    let bands = JPEG_BANDS.min(frame.h).max(1);
    let band_h = frame.h.div_ceil(bands);
    let mut out: Vec<Bytes> = Vec::with_capacity(bands as usize);
    let mut y: u32 = 0;
    while y < frame.h {
        let rows = band_h.min(frame.h - y);
        let Some(pixels) = frame.rows(y, rows) else {
            break;
        };
        let mut jpeg: Vec<u8> = Vec::with_capacity((frame.w * rows / 6) as usize);
        {
            let mut encoder = jpeg_encoder::Encoder::new(&mut jpeg, JPEG_QUALITY);
            if let Err(err) = encoder.encode(
                pixels,
                frame.w as u16,
                rows as u16,
                jpeg_encoder::ColorType::Bgra,
            ) {
                log::debug!("ws media: jpeg stripe at y={y} failed: {err}");
                y += rows;
                continue;
            }
        }
        let mut msg = BytesMut::with_capacity(JPEG_HEADER_LEN + jpeg.len());
        msg.put_u8(TYPE_JPEG_STRIPE);
        msg.put_u8(0);
        msg.put_u16(frame_id);
        msg.put_u16(y as u16);
        msg.put_slice(&jpeg);
        out.push(msg.freeze());
        y += rows;
    }
    out
}

/// 一条连接上的 `server_settings` 回执记账（**边沿触发**）。
///
/// # 为什么需要它
///
/// 页面把 `SETTINGS` 当成"告诉服务端我在用什么"，而这里以前**每收到一批就回
/// 一条** `server_settings`。两边各自都守规矩，回路却是无限的：页面对回执会
/// sanitize 一遍，把"被服务端改写过的键"再发回来；于是
///
/// ```text
/// SETTINGS → server_settings（1392 B）→ 页面 sanitize 报 changes → SETTINGS → …
/// ```
///
/// 实测这个环跑到 **400–550 批/秒**：日志 +167 KB/s（约 4 GB/h），下行有一半
/// 带宽是控制面 JSON，进程 CPU 86%，主线程抢不到时间渲染第一帧 —— 表现出来
/// 就是"一直 Waiting for stream"。而画面数据其实一直在流。
///
/// 回执的用途只有一个：让页面知道"你点的这一档，我实际用了什么"。这天然是
/// 边沿触发的 —— 内容没变就不该再发，发第二遍一模一样的东西不会让任何人多
/// 知道一点。
///
/// # 为什么整串比较而不是逐字段比 `StreamPrefs`
///
/// 回执里除了那 8 个值，还有锁定清单、`allowed`、宿主 DPI、麦克风可用性等
/// 一堆字段，其中几项（`scaling_dpi`/`mouse_dpi`）是从宿主**读**出来的。逐字段
/// 比较等于把 `output::server_settings` 抄第二遍，抄漏一个字段就是一条永远
/// 收敛不了的差异；整串比较则天然覆盖全部，且宿主真变了也能重发。
#[derive(Default)]
struct EchoState {
    /// 上一次真发出去的那条回执原文。
    last: Option<String>,
    /// 上一批 `SETTINGS` 的原文与到达时间（用来吸收完全相同的重复批）。
    last_payload: Option<String>,
    last_payload_at: Option<std::time::Instant>,
}

impl EchoState {
    /// 窗口取 100 ms：同一份 payload 在这个窗口内重复到达，是放大器而不是
    /// 用户操作 —— 人手最快也点不出 10 次/秒的同一个开关。
    const DUP_WINDOW: std::time::Duration = std::time::Duration::from_millis(100);

    /// 这批 `SETTINGS` 是不是上一批的复读？是则调用方可以直接丢弃 ——
    /// 连 JSON 都不必解析，因为同样的批上一次已经应用过、也已经记过账。
    fn is_repeat(&mut self, payload: &str) -> bool {
        let now = std::time::Instant::now();
        let repeat = self.last_payload.as_deref() == Some(payload)
            && self
                .last_payload_at
                .map_or(false, |at| now.duration_since(at) < Self::DUP_WINDOW);
        if !repeat {
            self.last_payload = Some(payload.to_owned());
            self.last_payload_at = Some(now);
        }
        repeat
    }

    /// 这条回执要不要发？内容与上一条一致就静默。
    fn should_send(&self, text: &str) -> bool {
        self.last.as_deref() != Some(text)
    }

    /// 记下一条**已经发出去**的回执。
    fn remember(&mut self, text: &str) {
        self.last = Some(text.to_owned());
    }
}

/// `server_settings` 在两条传输上的形状不一样，这是必须记住的一处差异。
///
/// WebRTC 走数据通道，格式是 `{"type":...,"data":{"settings":{...}}}`（前端
/// `dispatch/output.rs` 的既有形状）。WebSocket 那边读的是**扁平**的
/// `{"type":"server_settings","settings":{...}}`（`selkies-ws-core.js:8644`
/// 直接取 `obj.settings`）。发错一层，页面控制台只会说"读不到设置"，然后
/// 分辨率、编码器、帧率全部停在它自己的默认值上 —— 画面是有的，但所有下拉框
/// 都是假的，这种错最难认。
///
/// 与其复制一份 150 行的设置表，不如就地拆包：单一事实来源留在 `output`。
fn settings_frame(settings: &StreamSettings) -> Option<String> {
    let full: serde_json::Value =
        serde_json::from_str(&crate::dispatch::output::server_settings(settings)).ok()?;
    let flat = full.get("data")?.get("settings")?.clone();
    Some(
        serde_json::json!({ "type": "server_settings", "settings": flat }).to_string(),
    )
}

/// `stream_info`，同样是**拆包**而不是复制：WebRTC 的 `{type,data}` 在 WS 上
/// 必须变成 `{"type":"stream_info","info":{...}}`（`selkies-ws-core.js:8636`
/// 取 `obj.info`）。
///
/// 不发的后果不是黑屏，是面板上整块"服务端在做什么"永远空着：编码器、采集
/// 路径、码率、CPU 全是破折号 —— 而画面是好的，所以看起来像"面板坏了"。
///
/// 采集路径取 `handle.stream_info()` 而不是 `StreamInfo::default()`：GDI 回落
/// 之后仍报 DXGI 的话，这份诊断的唯一用途就没了。
///
/// #110: 整个 `SharedPipeline` 而不是 `handle` + `display` —— 报告里的编码器
/// 现在属于 program，多传一个参数只会多一个能传错的地方。
///
/// `name_host` 决定 `encode_node` 发不发（见
/// [`crate::security::Principal::may_name_host`]）。机器名是主机级标识，
/// 不是流的属性。
fn stream_info_frame(
    pipeline: &crate::pipeline::SharedPipeline,
    name_host: bool,
) -> Option<String> {
    let (is_gdi, reason) = pipeline.handle.stream_info();
    let info = crate::dispatch::StreamInfo::from_capture(is_gdi, reason)
        .with_display(pipeline.display.0)
        .with_codec(pipeline.settings.codec())
        .with_node_visible(name_host);
    let full: serde_json::Value =
        serde_json::from_str(&crate::dispatch::output::stream_info(&info)).ok()?;
    let flat = full.get("data")?.clone();
    Some(serde_json::json!({ "type": "stream_info", "info": flat }).to_string())
}

/// `stream_stats`，扁平封套：`{"type":"stream_stats","stats":{...}}`
/// （`selkies-ws-core.js:8639` 取 `obj.stats`）。
fn stream_stats_frame(stats: &crate::dispatch::StreamStats) -> Option<String> {
    let full: serde_json::Value =
        serde_json::from_str(&crate::dispatch::output::stream_stats(stats)).ok()?;
    let flat = full.get("data")?.clone();
    Some(serde_json::json!({ "type": "stream_stats", "stats": flat }).to_string())
}

/// 这条会话收到的是 JPEG 条纹，`server_settings` 必须这么写。
///
/// 前端只在 `currentEncoderMode === 'jpeg'` 时处理 `0x03`（`selkies-ws-core.js`
/// 的 `canProcessJpeg`），而这个变量唯一的来源就是 `server_settings.encoder`。
/// 报 `vp8enc` 而发 JPEG，页面的反应是"每一条都丢掉"——比黑屏更难查，因为
/// 帧确实到了。
///
/// `allowed` 也只留 `jpeg`：这条通道是兜底，让用户在面板里选一个自己解不了的
/// 编码器，等于给他们一个把画面切黑的办法。
fn jpeg_settings_frame(settings: &StreamSettings) -> Option<String> {
    let mut full: serde_json::Value =
        serde_json::from_str(&crate::dispatch::output::server_settings(settings)).ok()?;
    let flat = full.get_mut("data")?.get_mut("settings")?;
    flat["encoder"] = serde_json::json!({ "value": "jpeg", "allowed": ["jpeg"], "locked": false });
    Some(serde_json::json!({ "type": "server_settings", "settings": flat.clone() }).to_string())
}

/// `cursor` 在 WS 上是 **`cursor,{json}`**（逗号前缀），和上面那几种扁平封套
/// 不是同一种写法。
///
/// 页面用 `event.data.startsWith('cursor,')` 分流，再 `JSON.parse(substring(7))`
/// 得到对象、直接交给 `updateServerCursor`，读的是 `curdata / width / height /
/// hotx / hoty / handle` 这一层 —— 所以这里必须把 `output::cursor` 的
/// `{type,data}` 拆开，多包一层页面就只看到 `undefined`。
///
/// 不发它的后果不是黑屏，而是**画面上的指针永远是浏览器默认箭头**：被控机的
/// 光标形状、隐藏状态、"这里是手型/十字"全都没了。WebRTC 那条路一直靠事件
/// sink 在发，WS 这一版此前一个事件都没订阅，整条链是空的。
fn cursor_frame(cd: &crate::pipeline::CursorData) -> Option<String> {
    let full: serde_json::Value =
        serde_json::from_str(&crate::dispatch::output::cursor(cd)).ok()?;
    let flat = full.get("data")?.clone();
    Some(format!("cursor,{flat}"))
}

/// 这一批 `SETTINGS` 里点名要的编码器（字符串或 `{value:...}` 两种形状都认）。
fn encoder_in(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let enc = value.get("encoder")?;
    let name = enc
        .as_str()
        .or_else(|| enc.get("value").and_then(|v| v.as_str()))?;
    Some(name.trim().to_ascii_lowercase())
}

/// #110: 这一批 `SETTINGS` 里有没有**会动到别人**的字段。
///
/// 这条批是复合的，同一份 JSON 里混着三类东西：
///
/// * 身份（`tabId` / `deviceId`）—— 谁都能报，服务端只用来登记；
/// * 选屏（`displayId`）—— 只改变这条连接看哪块屏，不动别人的画面；
/// * 本连接自己的编码兜底（`encoder: "jpeg"`）—— JPEG 条纹由这个会话自己编；
/// * **程序参数**（帧率、分辨率、音频码率、码控、CRF、真编码器）—— 同屏所有
///   会话都看得见，只有操作者能改。
///
/// 前三类要是跟着一起被拒，观众在 `health` 里就没有 `tab_id`、也换不了屏 ——
/// 一条安全规则顺手弄坏两个无害功能，是这类"整条拒绝"最常见的样子。
fn touches_program_settings(payload: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    let Some(map) = value.as_object() else {
        return false;
    };
    for key in [
        "framerate",
        "manual_width",
        "manual_height",
        "audio_bitrate",
        "rate_control",
        "rc",
        "crf",
    ] {
        if map.contains_key(key) {
            return true;
        }
    }
    match map.get("encoder").and_then(|v| v.as_str()) {
        // `jpeg` 是这条连接自己的降级路径，不是程序编码器。
        Some(name) if name.eq_ignore_ascii_case("jpeg") => false,
        Some(_) => true,
        None => false,
    }
}

/// 这条连接用什么身份。
///
/// 前端只认 **带前缀** 的 `AUTH_SUCCESS,{json}`（`selkies-ws-core.js:8407`：
/// `startsWith('AUTH_SUCCESS,')` 之后 `substring(13)` 再 parse）。裸 JSON 会落进
/// 下面 `event.data.startsWith('{')` 那个分支，`obj.type` 是 undefined，被静默
/// 丢掉 —— 页面于是永远 `clientRole === null`，`updateControllerSlot` 不调用，
/// viewer 语义与手柄槽位全部失效，而画面正常。这类错在日志里一个字节都不留。
///
/// 现在**无条件发送**（此前只在有口令时发）：无口令的部署就是单用户机器，
/// 它必须和控制器一样能操作；不发只会让页面在"未认证"状态里走一遍再等一条
/// 永远不来的消息。
fn auth_frame(principal: &crate::security::Principal) -> String {
    // #110: `role` 现在说的是**令牌**能做什么，不再是写死的 "controller"。
    // 没有凭据的部署（`credential: none`）仍然是控制者 —— 那是单用户机器；
    // 但一张 `viewer` 令牌现在会如实说明自己是观众，页面据此隐藏控制项，
    // 而不是先显示能操作、再被服务端一条条拒掉。
    let role = if principal.may_operate() {
        "controller"
    } else {
        "viewer"
    };
    let body = serde_json::json!({
        "role": role,
        "slot": 1,
        "credential": principal.credential.label(),
        // #110: 页面现在不读它，但它是排障时第一个要问的字段 —— "我看到的
        // 是哪个租户的屏"。多出来的键对前端是无害的（它按 `role`/`slot` 取值）。
        "tenant": principal.tenant.as_str(),
    })
    .to_string();
    format!("AUTH_SUCCESS,{body}")
}

/// 一次 `stream_stats` 采样，`None` 表示这一秒没有可发的（宿主采样器建不起来）。
///
/// 窗口所有权的规则与 WebRTC 侧完全一样（`pipeline::shared::claim_stats`）：一个
/// 程序只有一个 1 s 窗口，快照会把计数器清零，两个 ticker 同时采样会让两边的
/// `encoded_fps` 都变成大约一半 —— 一个看起来完全合理的错数。所以这里也先争，
/// 争到就发布，没争到就读别人发布的上一窗。
fn sample_stats(
    pipeline: &SharedPipeline,
    sampler: &mut Option<crate::platform::Sampler>,
    owner: &mut bool,
) -> Option<String> {
    if sampler.is_none() {
        match crate::platform::Sampler::new() {
            Ok(s) => *sampler = Some(s),
            Err(err) => {
                log::warn!("ws media: host sampler unavailable ({err:#}); no stream_stats");
                return None;
            }
        }
    }
    let sampler = sampler.as_mut()?;
    sampler.refresh();
    let host = sampler.sample();
    let handle = pipeline.handle.clone();
    if !*owner && pipeline.claim_stats() {
        *owner = true;
    }
    let snapshot = if *owner {
        let stats = handle.stats();
        let snap = {
            let mut guard = stats.lock().unwrap_or_else(|p| p.into_inner());
            let mut snapshot = guard.snapshot(std::time::Duration::from_secs(1));
            snapshot.peak_mbps = guard.peak.take_peak_mbps(std::time::Instant::now());
            guard.reset_window();
            snapshot
        };
        // 发布同时续租：`claim_stats` 的租约靠这一步保持，否则 3 秒后别人会
        // 以为窗口空了并把同一秒再采一次。
        pipeline.publish_snapshot(snap);
        snap
    } else {
        // 别人的窗口，最多旧一秒。还没有人发布过就是全零 —— 面板画空白，
        // 而不是画一个编出来的数。
        pipeline.last_snapshot().unwrap_or_default()
    };
    let stats = crate::dispatch::StreamStats::from_host(host)
        .with_pipeline(&snapshot)
        .with_throttled(handle.is_throttled());
    stream_stats_frame(&stats)
}

/// 一条 WebSocket 画面会话。
///
/// `peer` 与 `grant` 用于日志、`AUTH_SUCCESS` 与**策略裁决**（#110 起这条路
/// 也接上了 `security::allow`；在此之前它一条都没有，装上真策略也是空心的）。
///
/// #106 之后这条连接不再只是"只读画面"：输入、剪贴板、音频都从同一个 socket
/// 走（文本是命令，二进制是帧），因为前端 `selkies-ws-core.js` 本来就是这么发
/// 的——服务端缺的只是接收端。WebRTC 与它共用同一条采集/编码程序。
pub async fn run<S>(
    ws: WebSocketStream<S>,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    pipelines: PipelineRegistry,
    // 只为 Drop：这个许可代表这个进程里的一个会话名额，连接结束时自动归还。
    // 不 `use` 它是有意的 —— 它的价值就是活得和这条连接一样长。
    _permit: AdmissionPermit,
    peer: IpAddr,
    grant: Option<crate::security::token::Grant>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut source) = ws.split();

    // #110: 身份在第一条消息之前就要定下来 —— `AUTH_SUCCESS` 要报角色，而角色
    // 来自令牌。会话号要等登记之后才有，所以先带 `session: None`，登记完补上。
    let mut principal = crate::security::token::principal(peer, grant);

    // 顺序是有意的：先身份、再模式、最后设置。前端收到 `MODE websockets` 就会
    // 开始初始化输入与音频，此时若还不知道自己的角色，它会按"未认证"走一遍，
    // 然后在 `AUTH_SUCCESS` 到达后再补一遍 —— 少发一条消息就能避免的抖动。
    sink.send(Message::Text(auth_frame(&principal).into()))
        .await?;
    sink.send(Message::Text("MODE websockets".into())).await?;

    // 共享管线：如果此刻已有一个 WebRTC 会话在跑，这里是**加入**而不是新开一路
    // 采集 —— `DuplicateOutput` 对同一个输出是独占的，新开只会失败并退到 GDI。
    // #109: WS 页面没有 hello，先按服务端默认（`--display`）开一路，页面的
    // `SETTINGS.displayId` 到了再决定要不要换 —— 换屏是 `PendingDisplay` 那段。
    let acquired = pipelines.acquire(DisplayKey(cfg.display), &cfg).await;
    let mut pipeline = acquired.pipeline.clone();
    // #110: the codec belongs to the program we just joined, so it is read here
    // rather than at connection time (there was no program yet) and re-read
    // after every display switch.
    let mut codec_now = pipeline.settings.codec();

    // #111: 这条连接的回执记忆。第一次同步下面就会记上，于是页面随后发来的
    // 第一批 `SETTINGS` 只要不改变任何东西，就不会再收到一条一模一样的回执。
    let mut echo = EchoState::default();
    if let Some(settings) = settings_frame(&pipeline.settings) {
        echo.remember(&settings);
        sink.send(Message::Text(settings.into())).await?;
    } else {
        log::warn!("ws media: server_settings could not be reshaped; the page keeps its own defaults");
    }
    // #106: 面板的"服务端在做什么"那一栏。发一次就够，采集路径变化会由
    // `PIPELINE_RESETTING` 之后的重新连接重发；它不带时间敏感的数值。
    if let Some(info) = stream_info_frame(&pipeline, principal.may_name_host()) {
        sink.send(Message::Text(info.into())).await?;
    }

    // #111: 订阅管线事件。WS 这一版从做出来到今天**一个事件都没订**，于是
    // `cursor`（被控机指针形状）和 `system`（采集恢复/降级）在这条传输上永远
    // 收不到 —— 页面上表现为"视频里的指针是浏览器默认箭头"。判据很硬：日志里
    // 连一行 `#70 pointer handover` 都没有，因为页面的 `SET_NATIVE_CURSOR_RENDERING`
    // 在那边也没有对手方。
    //
    // sink 是**per-program** 的：换屏那段要摘掉旧的、挂上新的。
    let (event_tx, mut events) = mpsc::unbounded_channel();
    let mut event_id = pipeline.handle.add_event_sink(event_tx);
    // 订阅只收"以后"的事件，而光标早就已经在那儿了：补发一次当前形状。
    // `Some(id == 0)` 是"现在隐藏"——页面据此收起自己的箭头，和"还没读到过"
    // （`None`，什么都不发）是两件事。
    //
    // 但补发这一次经常**空手**：`last_cursor()` 要等采集循环第一次 beat，而
    // 那一 beat 在下面的 `handle.start()` 之后。指针形状又不会自己变（箭头一直
    // 是箭头，`update_cursor` 只在 id 变化时 emit），所以"等下一次变化"可能永远
    // 等不到 —— 页面上就是"没有指针"。`cursor_sent=false` 时 loop 里那个 250 ms
    // 的补位定时器会接着问。
    let mut cursor_sent = false;
    if let Some(cd) = pipeline.handle.last_cursor() {
        if let Some(line) = cursor_frame(&cd) {
            send_text(&mut sink, line).await?;
            cursor_sent = true;
        }
    }

    // 网关打开，编码器开始出帧。
    pipeline.handle.start();
    // #106: 这条连接也是 health 里的一个客户端。不登记的话，"谁在占带宽"只剩
    // `pipelines[].downlinks` 一个数字，而 `clients` 里一个 WS 观众都看不到。
    let session_id = registry
        .register_media(SessionInfo {
            peer_ip: peer.to_string(),
            client_type: "websocket".to_owned(),
            connected_at: Some(std::time::Instant::now()),
            display: Some(pipeline.display.0),
            // #110: 谁连着 —— `health` 按这一行决定这条记录给谁看。
            tenant: principal.tenant.clone(),
            ..Default::default()
        })
        .await;
    // 输入租约：空着就认领。`exclusive`（#110 起的默认）下这就是"能不能操作"，
    // 所以先到的那条连接成为操作者、后来的是观众；`shared` 下只影响 `health`
    // 的 `role` 与 `session/take`。
    //
    // #110: 但只有**能操作**的令牌才认领。`viewer` 令牌若在这里认领，一个观众
    // 会握住租约、把之后连上来的操作者永远挡在门外（认领是 claim-if-free，
    // 没人能把它挤掉）—— 那会表现成"操作者连上了，键鼠没反应"。
    let may_operate = principal.may_operate();
    // #113: 先量一下上一任持有者静默了多久 —— 必须在认领/接管**之前**取，
    // 之后这份时间戳已经被接管路径刷新成"刚刚"了。
    let idle_before = match pipeline.input_idle() {
        Some(idle) => format!("{:.1}s", idle.as_secs_f32()),
        None => String::from("never"),
    };
    if may_operate {
        pipeline.try_claim_controller(session_id);
    }

    // #113: 认领只赢在"没人持有"。换传输之后租约在旧会话手里（它还活着、还在
    // 收字节，只是再也不会投输入），claim-if-free 就成了一道锁 —— 实测症状是
    // 切回 WebSocket 后 `lease=no`，页面上指针跟着鼠标动、桌面一动不动。
    // 判据是"旧持有者多久没投过输入"，不是身份：tab id 要到 `SETTINGS` 才来，
    // 而第一下点击在这之前。
    if may_operate && pipeline.adopt_lease_if_stale(session_id, crate::pipeline::LEASE_ADOPT_IDLE) {
        log::info!(
            "ws media: session {session_id} adopted the input lease (the previous holder was not injecting)"
        );
    }

    // #110: 会话号登记完才补得上 —— 身份本身（租户/角色）在连接开头就定了，
    // 那时还没有 session 可填。
    principal.session = Some(session_id);
    // #110: 两个问题是分开的，日志也必须分两栏写。`grant` 是令牌上写的角色，
    // `operates` 是这条连接**实际**能不能动手：角色够格 **且** 手里有租约。
    // 无门部署（`cred=none`）里 grant 是 `viewer`、operates 是 yes，只写一栏的话
    // 每次复盘都要重新推一遍这个差。
    //
    // #112: `operates` 以前只写角色（`may_operate`），于是"租约被别人握着"这种
    // 最需要它说话的情况刚好被写成 yes —— 排查时我按这一行否掉了租约这条线，
    // 多绕了一圈。现在它回答的是"现在能不能动手"。
    //
    // #113: 再补一栏 `prev_idle` —— 上一任持有者静默了多久。这一栏是"换传输
    // 之后为什么指针没反应"的唯一直接证据：没有它就只能从 `lease=no` 反推，而
    // `lease=no` 在正常的两页签场景里同样是 `no`。
    log::info!(
        "ws media: {peer} identified as tenant={} grant={} operates={} lease={} cred={} prev_idle={}",
        principal.tenant.as_str(),
        principal.role.as_str(),
        if may_operate { "yes" } else { "no" },
        if pipeline.owns_input(session_id) { "yes" } else { "no" },
        principal.credential.label(),
        idle_before
    );

    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    let mut sink_handle = pipeline.video.attach_stream(tx);
    let denied = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    registry
        .set_downlink(session_id, pipeline.display, sink_handle.counters(), denied.clone())
        .await;
    // 观众刚到，手上没有任何参考帧：不主动要一个关键帧，他要到下一个周期性的
    // 才能出画面（默认 10 s）。这和 `SharedPipeline::attach(track, joining=true)`
    // 是同一条理由。
    pipeline.handle.request_keyframe();

    // 音频：与视频同构，程序只有一条 loopback，这里挂一个裸 Opus 下行。
    let (audio_tx, mut audio_rx) = mpsc::channel::<Bytes>(8);
    let mut audio_sink = pipeline.audio.attach_stream(audio_tx);

    // 输入与剪贴板都是纯软件对象，创建成本可以忽略；`WinInputSink` 持有自己的
    // 按钮/滚轮/按键状态，所以必须每会话一个，不能全局共享。
    let input = WinInputSink::new(pipeline.settings.clone());
    // #109: 非主屏的绝对坐标要先加到那块屏的 origin 上，并用虚拟桌面归一化 ——
    // 主屏之外按 0,0 归一会把鼠标送到左上角那块屏去。WS 的指针空间就是采集
    // 画面本身，所以这里和 WebRTC 的 attach 之后做的是同一件事。
    if let Some(geom) = crate::pipeline::capture::geometry_at(pipeline.display.0) {
        input.set_target_display(geom.origin, (geom.width, geom.height));
    }
    let clipboard = ClipboardSink::default();

    log::info!(
        "ws media: {peer} attached as session {session_id} ({} downlink(s), codec {:?}, queue {QUEUE})",
        pipeline.video.len(),
        codec_now
    );

    let mut frame_id: u16 = 0;
    let mut reference: u16 = 0;
    // 已经丢过帧，等下一个关键帧再接着喂 —— 残差帧在没有参考的情况下解出来是
    // 花屏，而花屏比黑屏更容易被当成"串流坏了"。
    let mut need_key = true;
    let mut streaming = true;
    let mut dropped: u64 = 0;
    // #109: 页面在 `SETTINGS` 里点名了另一块屏。换屏要拆掉这条连接挂着的
    // 视频/音频 sink、释放旧程序、再 acquire 新的 —— 全都在 `select!` 之外做，
    // 因为在分支体里改 `rx` / `pipeline` 会和同一轮 `select!` 的其他分支借用打架。
    // #106: 同一批 `SETTINGS` 还可能要求切到 JPEG 条纹兜底，也走这条。
    let mut intent = Intent::default();
    // 上行的麦克风/摄像头回绝只发一次：页面的电平条按帧发 `0x02`，每帧回一条
    // 文本会把 socket 变成回音壁。
    let mut mic_refused = false;
    let mut cam_refused = false;
    // #110: 麦克风注入点。第一次收到 `0x02` 才起线程 —— 没人开麦的连接不该占一个
    // 音频线程和一个 render 客户端。连接结束时 `Drop` 停掉它。
    let mut mic: Option<audio::MicUplink> = None;
    // #106: `_stats,1` 之后每秒一条 `stream_stats`。页面只在面板打开时才订阅
    // （`lib/stream-stats.js`），所以默认整条通道上是安静的 —— 一个不看的统计
    // 不该占用带宽，也不该每秒唤醒两条任务。
    let mut stats_on = false;
    let mut stats_owner = false;
    let mut sampler: Option<crate::platform::Sampler> = None;
    let mut stats_tick = tokio::time::interval(std::time::Duration::from_secs(1));
    // 一轮没跟上就顺延，不要补发：补发的样本描述的是同一条 1 s 窗口，面板会
    // 看到两个一模一样的点。
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // #106: JPEG 条纹兜底。这套状态只在页面点名 `encoder: jpeg` 后才活起来；
    // 它和 `pipeline` 的 VP8 下行并行，互不干扰（别的观众照旧收 0x04）。
    let mut jpeg_on = false;
    let mut jpeg_busy = false;
    let mut jpeg_id: u16 = 0;
    let mut jpeg_sub: Option<crate::pipeline::raw::RawSubscription> = None;
    // 无界通道 + 同时只允许一个编码任务：积压最多一帧，而 `send` 不会因为
    // "通道满"失败 —— 失败一次就再也不会复位 `jpeg_busy`，那条连接从此没有画面。
    let mut jpeg_tx: Option<mpsc::UnboundedSender<Vec<Bytes>>> = None;
    let mut jpeg_rx: Option<mpsc::UnboundedReceiver<Vec<Bytes>>> = None;
    // #111: 光标的补位定时器。存在的唯一理由是顺序：`last_cursor()` 在
    // `handle.start()` 之前多半还是 `None`（第一次 beat 还没跑），而形状不会
    // 自己变，于是"等事件"就变成了"永远没有指针"。250 ms 问一次，拿到就停。
    let mut cursor_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    cursor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` 的第一次 tick 立即就绪；先吃掉它，这一支要表达的是"250 ms 后
    // 再看"，不是"立刻再看"。
    cursor_tick.tick().await;

    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    let Some(first) = frame else {
                        // 编码器停了（程序被释放或重建中）。
                        log::info!("ws media: pipeline stopped feeding this downlink");
                        break;
                    };
                    // #106: 兜底模式下这条连接收的是 0x03，VP8 码流对它没有意义。
                    // 不 detach 是有意的：切回来时不用再要一次关键帧。
                    if jpeg_on {
                        continue;
                    }
                    // 只要最新的一帧。慢观众的正确惩罚是"少看几帧"，不是"看几秒前的画面"；
                    // 队列里留着旧帧反而会让延迟越积越长，最后看起来像卡死。
                    let mut latest = first;
                    let mut queued: u32 = 0;
                    while let Ok(next) = rx.try_recv() {
                        latest = next;
                        queued += 1;
                    }
                    dropped += u64::from(queued);
                    // #116: 上闸的依据只能是**这一批**有没有丢帧。
                    let gate = key_gate_after_batch(queued, need_key);
                    if gate && !need_key {
                        // 上闸等于"等到下一个 IDR 之前什么都别发"，而周期 IDR 是
                        // `keyframe_secs`（默认 10 秒）一个 —— 不主动要一个，观众
                        // 就要卡 10 秒。这一批已经证明参考链断了，要一次就够：
                        // 多要只会让编码器多做几次全帧。
                        pipeline.handle.request_keyframe();
                    }
                    need_key = gate;
                    if !streaming {
                        continue;
                    }
                    // 编码器现在属于**这块屏**（#110），面板切 VP8/VP9 只会重建
                    // 这一路。这条连接手里缓存的 `codec_now` 立刻过期，继续按旧
                    // 编号发帧等于让浏览器用错误的解码器解新码流。关掉让它重连，
                    // 重连时会读到新值。
                    if pipeline.settings.codec() != codec_now {
                        log::info!(
                            "ws media: codec changed {} -> {}; closing so the page reconnects",
                            codec_now.wire(),
                            pipeline.settings.codec().wire()
                        );
                        break;
                    }
                    let key = is_keyframe(&latest, codec_now);
                    if need_key && !key {
                        continue;
                    }
                    if key {
                        need_key = false;
                    }
                    // `output()` 是"真正离开编码器的尺寸"，但它由采集循环在打开
                    // 编码器之后才写，第一帧之前是 `(0, 0)`。拿它直接填头，前端
                    // `VideoDecoder.configure({codedWidth:0})` 会抛异常，表现为
                    // "连上了、一帧都解不出来"。所以零值回落到请求尺寸 —— 那是
                    // 编码器实际打开的尺寸（未缩放时两者相等）。
                    let (mut w, mut h) = pipeline.settings.output();
                    if w == 0 || h == 0 {
                        w = pipeline.settings.width();
                        h = pipeline.settings.height();
                    }
                    let packet = wire_frame(
                        &latest,
                        codec_now,
                        key,
                        frame_id,
                        reference,
                        w.min(u16::MAX as u32) as u16,
                        h.min(u16::MAX as u32) as u16,
                    );
                    // reference 必须是"上一帧的 id"：前端的解码闸门靠它判断依赖
                    // 有没有缺席（见模块注释）。
                    reference = frame_id;
                    frame_id = frame_id.wrapping_add(1);
                    sink.send(Message::Binary(packet)).await?;
                }

                // #106: 音频帧。`[0x01, 0x00] + opus` 是前端 `extractOpusFrames`
                // 的 `n_red == 0` 路径；第二个字节的 0x80 是"静音闸门关闭"的标记，
                // 这里不启用（静音由服务端发静音帧表达）。
                frame = audio_rx.recv() => {
                    let Some(opus) = frame else { continue };
                    sink.send(Message::Binary(wire_audio(&opus))).await?;
                }

                // #106: 1 Hz 的服务端采样。`if stats_on` 是**守卫**而不是分支内的
                // 判断：关掉的时候这个分支根本不参与 `select!`，一条空闲连接
                // 不会每秒醒一次。
                _ = stats_tick.tick(), if stats_on => {
                    let Some(frame) = sample_stats(&pipeline, &mut sampler, &mut stats_owner) else {
                        continue;
                    };
                    sink.send(Message::Text(frame.into())).await?;
                }

                // #106: 兜底路径的原始帧。编码在 `spawn_blocking` 里做，这条循环
                // 只负责"拿到帧就派活、编完就发"，绝不在 async 上下文里做 JPEG。
                // 同时在飞的只有一个任务：JPEG 比采集慢时正确的行为是少发几帧，
                // 而不是把任务堆到内存里。
                raw = async {
                    match jpeg_sub.as_mut() {
                        Some(sub) => sub.recv().await,
                        None => std::future::pending().await,
                    }
                }, if jpeg_on => {
                    let Some(raw) = raw else { continue };
                    // 双屏机器上全局旁路会收到别的屏的帧。
                    if raw.display != pipeline.display.0 || jpeg_busy {
                        continue;
                    }
                    jpeg_busy = true;
                    let id = jpeg_id;
                    jpeg_id = jpeg_id.wrapping_add(1);
                    let tx = jpeg_tx.clone();
                    tokio::spawn(async move {
                        let stripes =
                            tokio::task::spawn_blocking(move || encode_jpeg_stripes(&raw, id))
                                .await
                                .unwrap_or_default();
                        if let Some(tx) = tx {
                            let _ = tx.send(stripes);
                        }
                    });
                }

                // 编好的条纹。一帧的多条一起到，按 y 顺序发出去。
                Some(stripes) = async {
                    match jpeg_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if jpeg_on => {
                    jpeg_busy = false;
                    for stripe in stripes {
                        sink.send(Message::Binary(stripe)).await?;
                    }
                }

                // #111: 管线事件。目前只消费 `Cursor` —— 它决定页面画的是被控机
                // 的真实指针（手型、十字、隐藏），还是浏览器自己的默认箭头。
                // 其余几类（`RecoveryStart`/`FallbackGdi`/`Lost`/`Restored`）
                // 对应前端的 `system,` 消息，接上要一并核对页面文案，留作下一步。
                Some(ev) = events.recv() => {
                    if let PipelineEvent::Cursor(cd) = ev {
                        if let Some(line) = cursor_frame(&cd) {
                            send_text(&mut sink, line).await?;
                            cursor_sent = true;
                        }
                    }
                }

                // #111: 补位（见 `cursor_sent` 与 `cursor_tick` 的注释）。拿到
                // 一条就把这一支永久关掉 —— 之后的变化都走上面那个事件通道。
                _ = cursor_tick.tick(), if !cursor_sent => {
                    if let Some(cd) = pipeline.handle.last_cursor() {
                        if let Some(line) = cursor_frame(&cd) {
                            send_text(&mut sink, line).await?;
                            cursor_sent = true;
                        }
                    }
                }

                msg = source.next() => {
                    match msg {
                        Some(Ok(Message::Text(line))) => {
                            let line = line.as_str().trim();
                            intent = handle_text(
                                line,
                                &pipeline,
                                &input,
                                &clipboard,
                                &registry,
                                &mut sink,
                                &mut streaming,
                                &mut need_key,
                                &mut stats_on,
                                &denied,
                                session_id,
                                &principal,
                                &mut echo,
                            )
                            .await?;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            log::info!("ws media: client closed ({:?})", frame.map(|f| f.code));
                            break;
                        }
                        // 二进制上行：麦克风 `0x02`、摄像头 `0x06`。
                        //
                        // #110 麦克风：页面把 `[0x02] + Opus`（24 kHz 单声道、
                        // 20 ms 一帧，来自 `selkies-core` 的 mic worker）发上来，
                        // 服务端解到 48 kHz 立体声后写**虚拟录音设备**。目的地由
                        // `--mic-device` 在启动时决议一次，不是默认扬声器 —— 写默认
                        // 扬声器会被下行 loopback 原样采回，就是那个"开麦听到自己"。
                        //
                        // 摄像头 `0x06` 仍然回绝：反向 track 全项目未实现，而静默
                        // 吞掉会让页面显示一路永远不动的摄像头。回绝必须是**一次**：
                        // 电平条按帧发，每帧回一条文本会把 socket 变成回音壁。
                        Some(Ok(Message::Binary(data))) => {
                            let kind = data.first().copied().unwrap_or(0);
                            match kind {
                                0x02 if !mic_refused => {
                                    // 租约与指针/键盘同一条门禁：`shared` 谁都能说，
                                    // `exclusive` 只有持租约的人能说。拒绝会计数，否则
                                    // "开了麦没声音"分不清是没到还是被挡了。
                                    // #110: 角色是这道门的另一半 —— 麦克风是往主机
                                    // 送声音，`viewer` 令牌在任何策略下都不该有它。
                                    // #112: 与键鼠同一条门禁（含自愈），否则会出现
                                    // "键鼠能动手、麦克风被挡"这种一眼看不出原因的差。
                                    // #113: 上行声音也算"在投输入" —— 用户可能只在开麦说话，
                                    // 此时他显然是在用这条连接，不该被一条静默的租约挡住。
                                    let may = pipeline.may_inject(session_id, may_operate, true);
                                    if !may {
                                        // 回绝而不是静默丢弃。两种拒绝（没租约、令牌是
                                        // 观众）在页面上的正确反应都是"关麦"：继续采
                                        // 只会让一条永远不动的声音白占带宽，而用户看到
                                        // 的是电平条在跳、对面没声音 —— 最坏的一种
                                        // "看起来正常"。
                                        denied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                        mic_refused = true;
                                        log::info!(
                                            "ws media: microphone refused (grant={}, operates={}, lease={})",
                                            principal.role.as_str(),
                                            if may_operate { "yes" } else { "no" },
                                            if pipeline.owns_input(session_id) { "yes" } else { "no" }
                                        );
                                        send_text(
                                            &mut sink,
                                            "MICROPHONE_DISABLED".to_owned(),
                                        )
                                        .await?;
                                    } else if data.len() > 1 {
                                        if mic.is_none() {
                                            match audio::MicUplink::start() {
                                                Ok(up) => {
                                                    let (where_to, provider, echoes) =
                                                        match crate::platform::virtual_mic::target()
                                                        {
                                                            Some(t) => (
                                                                t.note.clone(),
                                                                t.provider.as_str(),
                                                                t.echoes(),
                                                            ),
                                                            None => (
                                                                "默认 render 端点".to_owned(),
                                                                "default",
                                                                true,
                                                            ),
                                                        };
                                                    log::info!(
                                                        "ws media: microphone uplink open ({} B 首帧) | {where_to}",
                                                        data.len() - 1
                                                    );
                                                    mic = Some(up);
                                                    // #110: 正向确认。页面发 `0x02` 之后
                                                    // 只有"被拒"才有回信，于是"开了麦"和
                                                    // "发出去的帧被服务端吃了"在客户端完全
                                                    // 同形。这一行让探针（以及页面将来）能
                                                    // 说出"上行通到哪个端点"，`echoes` 是为
                                                    // 了让自听这件事在日志和面板里都看得见。
                                                    send_text(
                                                        &mut sink,
                                                        format!(
                                                            "MICROPHONE_READY,{}",
                                                            serde_json::json!({
                                                                "target": provider,
                                                                "note": where_to,
                                                                "echoes": echoes,
                                                            })
                                                        ),
                                                    )
                                                    .await?;
                                                }
                                                Err(note) => {
                                                    // 目标不可用：明确回绝，并让页面把
                                                    // 麦克风关掉 —— 让它继续采、继续发，
                                                    // 用户只会看到一条白跳的电平条。
                                                    mic_refused = true;
                                                    log::warn!(
                                                        "ws media: microphone refused: {note}"
                                                    );
                                                    send_text(
                                                        &mut sink,
                                                        "MICROPHONE_DISABLED".to_owned(),
                                                    )
                                                    .await?;
                                                }
                                            }
                                        }
                                        if let Some(up) = mic.as_mut() {
                                            if !up.push(&data[1..]) {
                                                log::debug!(
                                                    "ws media: mic frame dropped ({} B)",
                                                    data.len() - 1
                                                );
                                            }
                                        }
                                    }
                                }
                                0x02 => {} // 已经拒绝过：静默丢弃，不再回文本
                                0x06 if !cam_refused => {
                                    cam_refused = true;
                                    log::info!(
                                        "ws media: webcam uplink is not implemented; telling the page ({})",
                                        data.len()
                                    );
                                    send_text(&mut sink, "WEBCAM_DISABLED".to_owned()).await?;
                                }
                                _ => log::debug!(
                                    "ws media: binary upstream type {kind:#04x} ignored ({} B)",
                                    data.len()
                                ),
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            log::debug!("ws media: socket error: {err}");
                            break;
                        }
                        None => break,
                    }
                }
            }

            // #106: 这条会话要不要走 JPEG 条纹兜底。设置在 `select!` 之外生效，
            // 因为它要动订阅（`raw::subscribe` 是全局旁路的注册）与下游的帧类型。
            if let Some(want) = intent.jpeg {
                if want && !jpeg_on {
                    let (tx, rx) = mpsc::unbounded_channel();
                    jpeg_tx = Some(tx);
                    jpeg_rx = Some(rx);
                    jpeg_sub = Some(crate::pipeline::raw::subscribe());
                    jpeg_on = true;
                    jpeg_busy = false;
                    jpeg_id = 0;
                    // 页面据此把 `currentEncoderMode` 切到 jpeg 并开始处理 0x03；
                    // 不告诉它，它会把每一条条纹都丢掉。
                    if let Some(settings) = jpeg_settings_frame(&pipeline.settings) {
                        // 记进 `echo`：这条内容是真的变了（encoder=jpeg），必须发；
                        // 记下来之后，紧接着那批 `SETTINGS` 的回执就不会再重复它。
                        echo.remember(&settings);
                        let _ = sink.send(Message::Text(settings.into())).await;
                    }
                    // 兜底路径从下一帧开始，不必等关键帧：JPEG 每帧都是自足的。
                    log::info!(
                        "ws media: session {session_id} switched to JPEG stripes ({} band(s)/frame)",
                        JPEG_BANDS
                    );
                } else if !want && jpeg_on {
                    jpeg_on = false;
                    jpeg_sub = None;
                    jpeg_tx = None;
                    jpeg_rx = None;
                    jpeg_busy = false;
                    // 切回编解码器模式：页面要重新学会帧头，也要一个关键帧才能接上。
                    need_key = true;
                    pipeline.handle.request_keyframe();
                    if let Some(settings) = settings_frame(&pipeline.settings) {
                        echo.remember(&settings);
                        let _ = sink.send(Message::Text(settings.into())).await;
                    }
                    log::info!("ws media: session {session_id} left JPEG stripes");
                }
            }

            // #109: 页面换了屏。顺序是 detach → release → acquire，因为
            // `DuplicateOutput` 对同一个输出是独占的：先在旧程序还活着的时候去
            // 开新一路，某些卡会直接失败并退到 GDI，然后两个程序一个 DXGI 一个
            // 慢路径；而先释放再打开，最坏只是中间黑几帧。
            if let Some(wanted) = intent.display.take() {
                if wanted == pipeline.display {
                    continue;
                }
                let previous = pipeline.display;
                pipeline.audio.detach(audio_sink);
                pipeline.detach(sink_handle.id());
                pipelines.release(previous).await;

                let acquired = pipelines.acquire(wanted, &cfg).await;
                let next = acquired.pipeline.clone();
                next.handle.start();
                let (next_tx, next_rx) = mpsc::channel::<Bytes>(QUEUE);
                let next_sink = next.video.attach_stream(next_tx);
                let (next_audio_tx, next_audio_rx) = mpsc::channel::<Bytes>(8);
                let next_audio = next.audio.attach_stream(next_audio_tx);
                // 刚到一块新的屏：手上没有参考帧，不要关键帧就只能等周期性的那个。
                next.handle.request_keyframe();
                registry
                    .set_downlink(
                        session_id,
                        next.display,
                        next_sink.counters(),
                        denied.clone(),
                    )
                    .await;
                // 输入的目标屏也要跟着换，否则点哪儿都在上一块屏上。
                match crate::pipeline::capture::geometry_at(next.display.0) {
                    Some(geom) => {
                        input.set_target_display(geom.origin, (geom.width, geom.height))
                    }
                    None => log::warn!(
                        "ws media: display {} has no geometry; input stays on the previous screen",
                        next.display.0
                    ),
                }
                // 帧号必须从头开始：新程序是新的编码器，接着旧号会让页面的
                // 解码闸门把第一帧当成"依赖缺席"。
                frame_id = 0;
                reference = 0;
                need_key = true;
                // #111: 事件 sink 属于**程序**，不是这条连接。换屏就是换 program，
                // 旧 handle 的 sink 必须摘掉（它的 receiver 马上就要被替换，留着
                // 只是一个每次 emit 都要失败的条目），新的在下面重新挂。
                pipeline.handle.remove_event_sink(event_id);
                pipeline = next;
                // #110: the new program may encode with a different codec, and
                // the framing below reads this value.
                codec_now = pipeline.settings.codec();
                rx = next_rx;
                audio_rx = next_audio_rx;
                sink_handle = next_sink;
                audio_sink = next_audio;
                if let Some(settings) = if jpeg_on {
                    jpeg_settings_frame(&pipeline.settings)
                } else {
                    settings_frame(&pipeline.settings)
                } {
                    echo.remember(&settings);
                    let _ = sink.send(Message::Text(settings.into())).await;
                }
                // 帧号与新程序的编码器对齐；JPEG 模式的帧号也是同一条线。
                jpeg_id = 0;
                // 页面自己的"服务端把流重启了"协议：它据此清掉帧号跟踪、解码器
                // 与画布，然后等新关键帧。**必须用页面的名字**（`display2`），
                // 它拿这个名字和自己的 `displayId` 比，发索引等于什么都没做。
                let reset = format!("PIPELINE_RESETTING {}", pipeline.display.hello_name());
                let _ = sink.send(Message::Text(reset.into())).await;
                // #111: 新程序的事件 sink，外加它当前的光标形状（订阅只收以后
                // 的变化，而这块屏的指针形状早就有了）。
                let (next_ev_tx, next_ev_rx) = mpsc::unbounded_channel();
                event_id = pipeline.handle.add_event_sink(next_ev_tx);
                events = next_ev_rx;
                // 换了一块屏 = 换了一份指针形状，重新补位。
                cursor_sent = false;
                if let Some(cd) = pipeline.handle.last_cursor() {
                    if let Some(line) = cursor_frame(&cd) {
                        let _ = sink.send(Message::Text(line.into())).await;
                        cursor_sent = true;
                    }
                }
                log::info!(
                    "ws media: session {session_id} moved from display {} to {}",
                    previous.0,
                    pipeline.display.0
                );
            }
        }
        Ok(())
    }
    .await;

    // #112: 租约不能随连接一起消失。这是信令路径 #103 的同一条规矩，WS 这条路
    // 一直漏着 —— 只挂一个页面时看不出来（程序跟着走，租约一起销毁），一旦同时
    // 挂着第二条连接就现形：换传输的那几十秒里旧 socket 还没关、新页面已经连上，
    // 新连接的 `try_claim_controller` 撞上"有人持有"而放弃，随后旧连接收尾又
    // **没有归还** —— 于是剩下的人看着一块摸不动的桌面，重连也修不好（租约挂在
    // 程序上，不在 socket 上）。
    //
    // 交给同屏最老的另一条连接，没人接手就明确置空（让 `may_inject` 的自愈
    // 有东西可以认领）。必须在 `registry.remove` **之前**算，否则"下一个是谁"
    // 里会剩下自己。
    let held_lease = pipeline.controller_id() == Some(session_id);
    let display_index = pipeline.display.0;
    let inherited = if held_lease {
        registry.successor(session_id, display_index).await
    } else {
        None
    };

    // 松开所有还按着的键：页面崩掉不会发 `kr`，而被控机上按键是全局状态。
    input.reset();
    pipeline.audio.detach(audio_sink);
    pipeline.detach(sink_handle.id());
    registry.remove(session_id).await;
    if held_lease {
        pipeline.set_controller(inherited);
        match inherited {
            Some(next) => log::info!(
                "pipeline {display_index}: input lease handed to session {next} (holder left)"
            ),
            None => log::info!(
                "pipeline {display_index}: input lease released (holder left, nobody to hand it to)"
            ),
        }
    }
    // 不能漏：`acquire` 加了一次引用，最后一个走的负责停掉采集循环。
    // #109: 必须释放**这一条**连接真正加入的那个 key，不能写死成 PRIMARY ——
    // 否则 `--display 1` 下程序永远不会被回收，而 PRIMARY 的计数会被扣成负数。
    pipelines.release(pipeline.display).await;
    log::info!(
        "ws media: {peer} detached ({} downlink(s) left, {dropped} frame(s) dropped)",
        pipeline.video.len()
    );

    // 正常退出也给一个正常关闭帧：半开的 socket 会让页面的重连阶梯当成网络故障，
    // 而我们明明知道为什么结束。
    let _ = sink
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "media closed".into(),
        })))
        .await;
    outcome
}

/// 一条上行文本命令要求主循环做什么。
///
/// 这两件事都**不能**在 `handle_text` 里就地做：换屏要拆掉 `select!` 正借着的
/// 两个 receiver，切 JPEG 要起一个编码任务并换掉下行的帧类型。所以这里只回
/// 意图，循环统一执行 —— 和 `pending_display` 当初同一条理由。
#[derive(Default, Debug)]
struct Intent {
    /// 页面点名了另一块屏。
    display: Option<DisplayKey>,
    /// `Some(true)` 这条会话要 JPEG 条纹，`Some(false)` 回到编解码器模式。
    jpeg: Option<bool>,
}

/// 一条上行文本命令。
///
/// 走的是**和 WebRTC 数据通道同一套动词**（`lib/input.js` 两边共用），所以分类
/// 直接复用 `dispatch::commands::classify`，不另写一份解析器 —— 两份解析器迟早
/// 会对同一个动词给出不同答案。
#[allow(clippy::too_many_arguments)]
async fn handle_text<S>(
    line: &str,
    pipeline: &Arc<SharedPipeline>,
    input: &WinInputSink,
    clipboard: &ClipboardSink,
    registry: &SessionRegistry,
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    streaming: &mut bool,
    need_key: &mut bool,
    stats_on: &mut bool,
    denied: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    session_id: u64,
    // #110: who is asking. The WS transport had no policy surface at all —
    // `security::allow` was called from the HTTP routes, the WebRTC data
    // channels and the signaling path, but never from here, so a policy
    // installed to stop input or settings changes simply did not apply to a
    // WebSocket viewer.
    principal: &crate::security::Principal,
    // 这条连接的回执记账：`SETTINGS` 是回执触发的，而回执本身会被页面当成
    // "我的设置被改了"再发回来 —— 见 [`EchoState`]。
    echo: &mut EchoState,
) -> Result<Intent>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // 这几个不是 `classify` 的动词：前端在 WS 上有自己的确认/节流家族。
    match line {
        "KEYFRAME" | "REQUEST_KEYFRAME" => {
            *need_key = true;
            pipeline.handle.request_keyframe();
            return Ok(Intent::default());
        }
        // 连接建立时的"只读缓存"剪贴板请求：先应答再给载荷，页面的
        // `taggedClipboardFetch` 靠这条 `clipboard_reply,cr` 结束等待。
        "cr" => {
            send_text(sink, "clipboard_reply,cr".to_owned()).await?;
            for msg in crate::dispatch::clipboard::read_clipboard_ws_messages() {
                send_text(sink, msg).await?;
            }
            return Ok(Intent::default());
        }
        // 帧确认/解码节奏/多屏布局都是浏览器侧的节流信息；服务端这一版不据此
        // 调速（`vb,` 才是码率入口），记 debug 不当作错误。
        _ if line.starts_with("LOST_FRAME")
            || line.starts_with("DECODE_PACE")
            || line.starts_with("CLIENT_FRAME_ACK")
            || line.starts_with("vp,") =>
        {
            log::debug!("ws media: `{}` noted", short(line));
            return Ok(Intent::default());
        }
        _ => {}
    }

    let action = classify(line);
    // #110: 与 WebRTC 控制通道**同一套**门禁、同一个顺序：
    //   1) 角色 —— 这块屏的操作者才能动它；观众只允许白名单里的读操作；
    //   2) 策略 —— `security::allow` 按 surface 裁决。
    // 两步都必须在副作用之前，否则"拒绝"只是记账。
    //
    // 在这之前这条路径一条 `security::allow` 都没有：装上真策略之后，走 WS
    // 的观众仍然能注入输入、改设置、取剪贴板 —— 策略是空心的。
    // `SETTINGS` 是复合批：身份（`tabId`/`deviceId`）、选屏（`displayId`）、本连接
    // 自己的 JPEG 兜底，这三样谁都能发；只有"改程序参数"那部分需要独占。整条
    // 拒绝会连带丢掉身份登记和选屏，所以它自己在分支里按字段裁决。
    let compound = matches!(action, Action::SetSettings(_));
    // #110: "能不能动这块屏"是**两个**条件 —— 租约（两个操作者里谁说了算）
    // 与角色（你是不是操作者）。以前只有前者，于是 `--input-policy shared`
    // 下任何持 `viewer` 令牌的连接都能注入输入。
    //
    // #112: 用 `may_inject` 而不是 `owns_input`：租约空着就顺手认领。悬空的
    // 租约（前一条连接换了传输、socket 半开就走了）在这里自愈，否则这一路
    // 键鼠全被丢掉，而页面上只是"指针会动、桌面没反应"。
    //
    // #113: 第三个参数说明"这一行是不是真的输入"。只有真输入才有资格把租约
    // 从一条没人用的持有者手里接过来 —— 让 `pong`/`_stats`/`SETTINGS` 也触发
    // 接管，接管日志会指向一条根本没在动指针的连接。
    let is_input = matches!(action, Action::InputStub(_));
    let operates = pipeline.may_inject(session_id, principal.may_operate(), is_input);
    if !compound && !operates && !crate::signaling::viewer_may_run(&action) {
        denied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log::debug!("ws media: viewer refused `{}`", short(line));
        return Ok(Intent::default());
    }
    if let Some(kind) = crate::signaling::surface_of(&action) {
        if !crate::security::allow(kind, principal) {
            log::debug!("ws media: policy denied {}", kind.label());
            return Ok(Intent::default());
        }
    }

    match action {
        Action::StartVideo => {
            *streaming = true;
            *need_key = true;
            pipeline.handle.start();
            pipeline.handle.request_keyframe();
            log::info!("ws media: START_VIDEO");
        }
        Action::StopVideo => {
            *streaming = false;
            log::info!("ws media: STOP_VIDEO");
        }
        // #106: 面板打开/关闭统计。页面只在面板可见时订阅（`lib/stream-stats.js`
        // 的 `statsOpen`），所以这两条就是"现在有没有人看"的全部信号。
        Action::StatsOn => {
            if !*stats_on {
                *stats_on = true;
                log::info!("ws media: stream_stats subscribed");
            }
        }
        Action::StatsOff => {
            if *stats_on {
                *stats_on = false;
                log::info!("ws media: stream_stats unsubscribed");
            }
        }
        Action::SetFps(fps) => {
            pipeline.settings.set_fps(fps);
            log::info!("ws media: fps -> {}", pipeline.settings.fps());
        }
        Action::SetResolution(w, h) => {
            pipeline.settings.set_resolution(w, h);
            log::info!("ws media: resolution -> {w}x{h}");
        }
        Action::SetBitrate(kbps) => {
            pipeline.handle.set_bitrate_override(kbps);
            log::info!("ws media: bitrate override {kbps} kbps");
        }
        Action::SetAudioOn(on) => {
            pipeline.settings.set_audio_enabled(on);
            log::info!("ws media: audio {}", if on { "on" } else { "off" });
        }
        Action::SetAudioBitrate(bps) => {
            let applied = pipeline.settings.set_audio_bitrate_bps(bps as i32);
            log::info!("ws media: audio bitrate -> {applied} bps");
        }
        Action::SetNativeCursorRendering(on) => {
            pipeline.handle.set_cursor_inject(on);
            log::info!("ws media: native cursor rendering -> {on}");
        }
        Action::SetSettings(payload) => {
            // #111: 先挡住**复读**。页面的回写是"收到回执 → sanitize → 再发一整批"，
            // 批的内容逐字节相同；这种东西一秒能来 400 次。同样的批上一次已经
            // 应用过、记过账，再解析一遍只会把 CPU 和日志一起烧掉（实测日志
            // 4 GB/h、CPU 86%）。丢弃是安全的：它不是用户操作，不需要第二次确认。
            if echo.is_repeat(&payload) {
                log::debug!("ws media: dropping a repeated SETTINGS batch ({} B)", payload.len());
                return Ok(Intent::default());
            }
            // #106: 页面在挑不到编码器时会点名 `jpeg`（`Kc()` → `fa('jpeg', …)`）。
            // 这不是全局编码器，而是**这条连接**的兜底：JPEG 条纹由本会话自己
            // 从原始帧编，别的观众照旧收 VP8。所以它不进 `apply_settings_batch`
            // （那里改的是整个程序），而是回一个意图让主循环切过去。
            let wants = encoder_in(&payload);
            let jpeg = match wants.as_deref() {
                Some("jpeg") => Some(true),
                Some("vp8enc") | Some("vp9enc") => Some(false),
                _ => None,
            };
            // 与信令会话共用同一个批处理：两套解析会把同一个 SETTINGS 解成两种
            // 结果，而面板只会相信自己那一路。
            //
            // #110: 但先按字段裁一次 —— 这一批里只有"改程序参数"需要操作者身份。
            if touches_program_settings(&payload) && !operates {
                denied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                log::debug!("ws media: viewer refused a program settings change");
                // 回一份**真实**值：面板刚点下去的那一档必须被顶回去，否则它
                // 会一直显示一个没有生效的帧率。
                //
                // 这一条**不走边沿触发**：内容很可能和上一条完全相同（观众反复
                // 点同一档），但页面此刻的本地状态是新点的值，它需要被纠正一次。
                // 记进 `echo` 只是为了让之后那条正常回执不必再重复。
                if let Some(settings) = settings_frame(&pipeline.settings) {
                    echo.remember(&settings);
                    send_text(sink, settings).await?;
                }
                let identity = crate::dispatch::settings_identity(&payload);
                return Ok(Intent {
                    display: identity.display.filter(|d| *d != pipeline.display),
                    jpeg,
                });
            }
            if let Some(codec) = apply_settings_batch(&pipeline.settings, &payload) {
                // 这条连接这次不重连就没有意义了：帧头里的 codec 是启动时缓存的。
                log::info!("ws media: codec -> {}; closing for renegotiation", codec.wire());
                return Err(anyhow::anyhow!("video codec changed"));
            }
            // #109/#110: 这一批里还带着"这是哪块屏"和"这是哪个页签"。WS 没有
            // hello，这**是唯一一处**页面告诉服务端它在哪块屏上 —— 无视它，
            // 页面上的 "#display2" 就只能看主屏。
            let identity = crate::dispatch::settings_identity(&payload);
            // #110: 页签 id 到了就补登记。WS 会话没有 hello，此前 health 里这条
            // 连接的 `tab_id` 一直是空串，"同一个页签重连"与"第二个观众"分不开。
            if let Some(tab) = identity.tab_id.as_deref() {
                registry.update_meta(session_id, tab, "").await;
                log::debug!("ws media: session {session_id} tab -> {tab}");
            }
            // 回设置时要按**这条连接**的真实情况写：JPEG 模式下报 `vp8enc`，
            // 页面会把每一条 0x03 都丢掉。
            let echoed = if jpeg == Some(true) {
                jpeg_settings_frame(&pipeline.settings)
            } else {
                settings_frame(&pipeline.settings)
            };
            // #111: **边沿触发**。内容与上一条一致就不发 —— 这条判断就是那个
            // 400 批/秒自激环的闸门（详见 [`EchoState`]）。语义上它也没有丢掉
            // 信息：页面手上的值和这条要发的一模一样。
            if let Some(settings) = echoed {
                if echo.should_send(&settings) {
                    echo.remember(&settings);
                    send_text(sink, settings).await?;
                } else {
                    log::debug!("ws media: settings unchanged; suppressing an identical echo");
                }
            }
            // 换屏交给调用方：这里没有第二个 receiver，也没有 acquire 的引用计数，
            // 就地重建会把这几个状态撕成两半。返回意图、循环里统一处理。
            return Ok(Intent {
                display: identity.display.filter(|d| *d != pipeline.display),
                jpeg,
            });
        }
        Action::ClipboardRequest => {
            for msg in crate::dispatch::clipboard::read_clipboard_ws_messages() {
                send_text(sink, msg).await?;
            }
        }
        Action::ClipboardChunk(raw) => clipboard.handle_line(&raw),
        Action::InputStub(raw) => {
            // 角色门禁在上面统一做过了（`may_inject` + `viewer_may_run`），拒绝
            // 时已经记进 `denied`。这里只剩"放行"这一种情况。
            //
            // #113: 放行即"在用" —— 这一条时间戳是租约能不能失效的唯一依据。
            // 只在这里盖，不在门禁里盖：门禁也被 `SETTINGS`/`pong` 问，那样一条
            // 只会回 `pong` 的死会话会被永远喂热。
            pipeline.note_input();
            input.handle_line(&raw);
        }
        other => log::debug!("ws media: ignoring `{:?}`", other),
    }
    Ok(Intent::default())
}

async fn send_text<S>(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    text: String,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    sink.send(Message::Text(text.into())).await?;
    Ok(())
}

/// `POST /api/switch` —— 前端模式菜单用的入口（`lib/mode-switch.js:43`）。
///
/// # 为什么这里只是"答应一声"
///
/// 上游的语义是：把旧的传输拆掉，然后页面重载进新的。这个实现不需要这么做 ——
/// 两种传输读的是同一路程序（一次采集、一次编码），WebSocket 只是多挂了一个下行。
/// 所以"切到 websockets"没有服务端状态要改：页面重载后自然会去连 `/api/websockets`。
///
/// 反过来说，这个端点必须存在。没有它，模式菜单会拿到 404，前端 `switchStreamMode`
/// 抛错、`__selkiesModeSwitching` 复位、页面不重载 —— 用户看到的是"点了没反应"，
/// 而真正的原因是一条谁都没实现的路由。这就是那个"广告了没货"的老问题，只是这次
/// 广告写在别人的前端里。
///
/// `mode` 只做白名单校验：这两个值是前端仅有的两种，写错一个是客户端 bug，应该
/// 在第一次请求就暴露，而不是静默接受然后让人等一个永远不来的画面。
pub async fn handle_switch<S>(
    stream: &mut S,
    body: &[u8],
) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mode = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("mode").and_then(|m| m.as_str()).map(str::to_owned));
    match mode.as_deref() {
        Some("websockets") | Some("webrtc") => {
            let mode = mode.unwrap_or_default();
            log::info!("transport switch requested: {mode}");
            let body = serde_json::json!({ "status": "ok", "mode": mode }).to_string();
            crate::http::write_http(
                stream,
                200,
                "OK",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )
            .await
        }
        Some(other) => {
            let body = serde_json::json!({
                "status": "error",
                "message": format!("unknown mode `{other}`; expected webrtc or websockets"),
            })
            .to_string();
            crate::http::write_http(
                stream,
                400,
                "Bad Request",
                "application/json; charset=utf-8",
                body.as_bytes(),
            )
            .await
        }
        None => {
            crate::http::write_http(
                stream,
                400,
                "Bad Request",
                "application/json; charset=utf-8",
                br#"{"status":"error","message":"expected {\"mode\":\"webrtc\"|\"websockets\"}"}"#,
            )
            .await
        }
    }
}

/// 日志里的命令截断：一条上行可能是几百字节的 JSON，整条打出来会把行撑爆。
fn short(s: &str) -> &str {
    if s.len() <= 40 {
        s
    } else {
        // 按字节切要在字符边界上，否则中文会把 UTF-8 切断并 panic。
        match s.char_indices().nth(40) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #111：`cursor,{json}` 必须是页面认的那个形状。
    ///
    /// 页面侧（`selkies-ws-core.js:8931`）只认 `cursor,` 前缀，再把 `substring(7)`
    /// 当 JSON 解，直接喂给 `updateServerCursor`。这里少一个字段，指针就会画到
    /// 左上角、或者干脆不画 —— 而服务端没有任何报错，因为字节是合法的。
    #[test]
    fn a_cursor_frame_carries_the_pages_flat_shape() {
        let cd = crate::pipeline::CursorData {
            id: 0x1234,
            // 2x2 BGRA：大红、大绿、大蓝、白。内容不重要，长度要对。
            colors: vec![
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
            ],
            width: 2,
            height: 2,
            hotx: 1,
            hoty: 2,
        };
        let line = cursor_frame(&cd).expect("a visible cursor must frame");
        let body = line
            .strip_prefix("cursor,")
            .expect("the prefix the page matches on");
        let v: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
        assert_eq!(v["handle"], 0x1234);
        assert_eq!(v["width"], 2);
        assert_eq!(v["height"], 2);
        assert_eq!(v["hotx"], 1);
        assert_eq!(v["hoty"], 2);
        assert!(
            v["curdata"].as_str().map(|s| s.len()).unwrap_or(0) > 40,
            "curdata is a base64 PNG, not a path: {v}"
        );

        // 隐藏是指针的另一个状态，不是"没有指针"：空图 + 0 尺寸 + handle 0，
        // 页面据此收起自己的箭头。
        let hidden =
            cursor_frame(&crate::pipeline::CursorData { id: 0, ..cd }).expect("0 is a state, not a gap");
        let body = hidden.strip_prefix("cursor,").expect("same shape");
        let v: serde_json::Value = serde_json::from_str(body).expect("body is JSON");
        assert_eq!(v["curdata"], "");
        assert_eq!(v["width"], 0);
        assert_eq!(v["handle"], 0);
    }

    fn le(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    /// #110：`SETTINGS` 的字段裁决必须把"会动到别人的"和"只动自己的"分开。
    ///
    /// 这条区分是"观众也能换屏、也能报到自己是哪个页签"与"观众不能改我的
    /// 帧率"之间唯一的分界线；写错了，要么观众被整条拒掉（health 里没有
    /// `tab_id`、换不了屏），要么观众能改所有人的画面。
    #[test]
    fn only_program_settings_need_the_operator() {
        // 身份与选屏：谁都能发，不该被挡。
        assert!(!touches_program_settings(r#"{"tabId":"t1","deviceId":"d1"}"#));
        assert!(!touches_program_settings(r#"{"displayId":"display2"}"#));
        // 本连接自己的 JPEG 兜底：不是程序编码器。
        assert!(!touches_program_settings(r#"{"encoder":"jpeg"}"#));
        assert!(!touches_program_settings(r#"{"encoder":{"value":"jpeg"}}"#));
        // 程序参数：同屏所有会话都看得见，只有操作者能改。
        assert!(touches_program_settings(r#"{"framerate":60}"#));
        assert!(touches_program_settings(r#"{"manual_width":1280}"#));
        assert!(touches_program_settings(r#"{"audio_bitrate":96000}"#));
        assert!(touches_program_settings(r#"{"crf":30}"#));
        assert!(touches_program_settings(r#"{"encoder":"vp9enc"}"#));
        // 混合批：只要有任何一个程序参数在里面，整批就要操作者身份。
        assert!(touches_program_settings(
            r#"{"displayId":"display2","tabId":"t1","framerate":60}"#
        ));
        // 不可解析的载荷按"什么都没改"处理：后面 `apply_settings_batch` 会
        // 自己记一条 debug，这里不该把它当成越权。
        assert!(!touches_program_settings("not json"));
        assert!(!touches_program_settings("[]"));
    }

    /// #111：回执必须**边沿触发**。
    ///
    /// 这条盯的就是那个 400 批/秒的自激环：同样的 `SETTINGS` 反复到达，不该
    /// 反复产生回执 —— 页面对每条回执都会回写一整批，回执就是放大器的心跳。
    #[test]
    fn an_identical_echo_is_only_sent_once() {
        let mut echo = EchoState::default();
        // 连上时那一次同步：页面手上没有值，必须发。
        let first = r#"{"type":"server_settings","settings":{"framerate":{"value":60}}}"#;
        assert!(echo.should_send(first), "the first sync is always sent");
        echo.remember(first);
        // 同样的内容再来：页面已经知道了，再发一遍只是把环路点起来。
        assert!(!echo.should_send(first));
        // 真的有变化（JPEG 兜底 / 换编码器）：必须发。
        let changed = r#"{"type":"server_settings","settings":{"encoder":{"value":"jpeg"}}}"#;
        assert!(echo.should_send(changed));
        echo.remember(changed);
        // 变回去同样算变化 —— 记的是"最后发出去的是什么"，不是"发过没有"。
        assert!(echo.should_send(first));
    }

    /// 复读批只在窗口内被丢弃。
    ///
    /// 100 ms 之外的同一批**不能**丢：用户过一会儿又点回同一档是正常操作，
    /// 而那时程序可能已经换过（换屏、重连），这一批仍然需要应用一次。
    #[test]
    fn repeated_batches_are_dropped_only_inside_the_window() {
        let mut echo = EchoState::default();
        let batch = r#"{"tabId":"t1","framerate":60}"#;
        assert!(!echo.is_repeat(batch), "the first arrival is real work");
        assert!(echo.is_repeat(batch), "the same bytes 0 ms later are the amplifier");
        // 另一批不是复读，并且会把窗口重置到它自己身上。
        assert!(!echo.is_repeat(r#"{"framerate":30}"#));
        assert!(echo.is_repeat(r#"{"framerate":30}"#));
        // 窗口过去之后，同一批重新算一次真活。
        echo.last_payload_at = Some(
            std::time::Instant::now() - EchoState::DUP_WINDOW - std::time::Duration::from_millis(1),
        );
        assert!(!echo.is_repeat(batch));
    }

    /// 一块 `w`x`h` 的假 BGRA 画面：从上到下渐变，好让条纹之间不撞码流。
    fn raw_frame(w: u32, h: u32) -> crate::pipeline::raw::RawFrame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = (x % 256) as u8;
                data[i + 1] = (y % 256) as u8;
                data[i + 2] = 0x40;
                data[i + 3] = 0xff;
            }
        }
        crate::pipeline::raw::RawFrame {
            display: 0,
            w,
            h,
            data: Bytes::from(data),
        }
    }

    #[test]
    fn a_frame_becomes_the_declared_number_of_stripes() {
        let frame = raw_frame(64, 40);
        let stripes = encode_jpeg_stripes(&frame, 7);
        assert_eq!(stripes.len(), JPEG_BANDS as usize, "一条不落地切成 N 条");
        for s in &stripes {
            assert_eq!(s[0], TYPE_JPEG_STRIPE, "类型字节必须是 0x03");
            assert_eq!(s[1], 0, "保留字节");
            assert_eq!(u16::from_be_bytes([s[2], s[3]]), 7, "同一帧共用一个帧号");
            assert_eq!(&s[JPEG_HEADER_LEN..JPEG_HEADER_LEN + 2], &[0xff, 0xd8], "SOI");
        }
    }

    #[test]
    fn the_rows_are_contiguous_and_end_at_the_bottom() {
        let frame = raw_frame(32, 100);
        let stripes = encode_jpeg_stripes(&frame, 0);
        let mut expected_y = 0u16;
        for s in &stripes {
            let y = u16::from_be_bytes([s[4], s[5]]);
            assert_eq!(y, expected_y, "每条从上一条的末尾开始");
            expected_y += 25;
        }
        // 4 条覆盖 100 行：最后一条的起点 + 25 = 100，页面据此判"画到底了"。
        assert_eq!(expected_y, 100);
    }

    #[test]
    fn a_tiny_frame_still_produces_one_stripe() {
        // 高度小于条数：不能切出 0 条（那会让页面永远等一个不存在的帧）。
        let frame = raw_frame(8, 3);
        let stripes = encode_jpeg_stripes(&frame, 1);
        assert_eq!(stripes.len(), 3, "一条一行，不会因为不够 4 条就什么都不发");
    }

    #[test]
    fn the_encoder_setting_is_recognised_in_both_shapes() {
        assert_eq!(
            encoder_in(r#"{"encoder":"jpeg"}"#).as_deref(),
            Some("jpeg"),
            "页面发的是字符串"
        );
        assert_eq!(
            encoder_in(r#"{"encoder":{"value":"JPEG"}}"#).as_deref(),
            Some("jpeg"),
            "面板发的是对象形状，值在 value 里"
        );
        assert!(encoder_in(r#"{"framerate":30}"#).is_none());
        assert!(encoder_in("not json").is_none());
    }

    #[test]
    fn the_jpeg_settings_say_jpeg_and_offer_nothing_else() {
        let settings = StreamSettings::default();
        let line = jpeg_settings_frame(&settings).expect("the base settings reshape");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(parsed["type"], "server_settings");
        assert_eq!(parsed["settings"]["encoder"]["value"], "jpeg");
        assert_eq!(
            parsed["settings"]["encoder"]["allowed"],
            serde_json::json!(["jpeg"]),
            "让用户选一个自己解不了的编码器，就是给他一个把画面切黑的办法"
        );
        // 扁平封套：前端取 `obj.settings`，不是 `obj.data.settings`。
        assert!(parsed.get("data").is_none(), "不能复用 WebRTC 的 {type,data}");
    }

    #[test]
    fn header_layout_matches_the_client_parser() {
        // 前端 `onWire`：head[0] 类型、head[1] 码型+关键位、[2..4] id、
        // [4..6] y、[6..8] w、[8..10] h、[10..12] reference。
        let data = Bytes::from_static(&[0x10, 0x20, 0x30]);
        let pkt = wire_frame(&data, VideoCodec::Vp8, true, 0x1234, 0x1233, 1920, 1080);
        assert_eq!(pkt.len(), HEADER_LEN + data.len());

        // 0x04，不是 0x01：socket worker 把 0x01 当音频，视频只认 0x03/0x04。
        assert_eq!(pkt[0], 0x04, "整帧类型必须走视频分支");
        assert_eq!(pkt[1], 0x21, "VP8(2)<<4 | key(1)");
        assert_eq!(&pkt[2..4], &[0x12, 0x34], "frameId 大端");
        assert_eq!(&pkt[4..6], &[0, 0], "y 恒零");
        assert_eq!(&pkt[6..8], &[0x07, 0x80], "1920");
        assert_eq!(&pkt[8..10], &[0x04, 0x38], "1080");
        assert_eq!(&pkt[10..12], &[0x12, 0x33], "reference 是上一帧");
        assert_eq!(&pkt[HEADER_LEN..], &data[..], "payload 原样不动");
    }

    #[test]
    fn audio_frames_are_the_two_byte_header_the_page_slices_off() {
        let opus = Bytes::from_static(&[0xfc, 0x01, 0x02]);
        let pkt = wire_audio(&opus);
        assert_eq!(&pkt[..2], &[0x01, 0x00], "类型 0x01 + n_red=0");
        assert_eq!(&pkt[2..], &opus[..], "Opus 原样跟在头后面");
    }

    #[test]
    fn delta_frames_clear_the_key_bit() {
        let pkt = wire_frame(&le(4), VideoCodec::Vp9, false, 7, 6, 640, 480);
        assert_eq!(pkt[1] & 0x0f, 0x00, "残差帧");
        assert_eq!(pkt[1] >> 4, 3, "VP9 是 3");
    }

    #[test]
    fn vp8_key_frame_is_the_low_bit_of_the_frame_tag() {
        // RFC 6386 §9.1：帧标签 bit0 = 0 是关键帧。
        assert!(is_keyframe(&[0x00, 0xff], VideoCodec::Vp8));
        assert!(is_keyframe(&[0x50], VideoCodec::Vp8));
        assert!(!is_keyframe(&[0x01], VideoCodec::Vp8));
        assert!(!is_keyframe(&[0x51], VideoCodec::Vp8));
    }

    #[test]
    fn vp9_key_frame_is_bit_two() {
        // `0b1000_0000`：marker=10b、profile 0、frame_type(bit2)=0 → 关键帧
        assert!(is_keyframe(&[0b1000_0000], VideoCodec::Vp9));
        // `0b1001_0000`：profile_high 置位（profile 2），frame_type 仍是 0。
        // 这一条是重点：profile 位挨着 frame_type，取错一位就会把每个关键帧都
        // 当成残差丢掉，表现为"连上了、永远黑屏"。
        assert!(is_keyframe(&[0b1001_0000], VideoCodec::Vp9));
        // frame_type(bit2)=1 → 帧间预测
        assert!(!is_keyframe(&[0b1000_0100], VideoCodec::Vp9));
    }

    #[test]
    fn an_empty_payload_is_never_a_key_frame() {
        // 0 长度下去前端会抛异常并关解码器，代价比丢一帧大。
        assert!(!is_keyframe(&[], VideoCodec::Vp8));
        assert!(!is_keyframe(&[], VideoCodec::Vp9));
    }

    #[test]
    fn frame_id_wraps_without_panicking() {
        let pkt = wire_frame(&le(2), VideoCodec::Vp8, false, u16::MAX, u16::MAX - 1, 8, 8);
        assert_eq!(&pkt[2..4], &[0xff, 0xff]);
        // 回绕之后 reference 仍然"上一帧"，前端按 16 位差值算距离。
        let next = wire_frame(&le(2), VideoCodec::Vp8, true, 0, u16::MAX, 8, 8);
        assert_eq!(&next[10..12], &[0xff, 0xff]);
    }

    #[test]
    fn short_never_splits_a_utf8_codepoint() {
        let long = "中".repeat(50);
        let cut = short(&long);
        assert!(cut.len() <= 120, "截断后长度受控");
        assert!(cut.chars().all(|c| c == '中'), "没有切出半个字符");
    }

    /// #116 的回归。现场的症状是"服务端 60 fps、客户端 0/1 浮动、重连才恢复"，
    /// 成因是上闸用了累计丢弃数：丢过一次之后每一拍都重新上闸，只剩周期 IDR
    /// 能过。这一组断言把"批次"与"累计"的差别钉死。
    #[test]
    fn one_skipped_batch_does_not_gate_the_stream_forever() {
        // 第一拍排空出了多余帧 → 上闸（这一批确实丢了，参考链断了）。
        let gated = key_gate_after_batch(2, false);
        assert!(gated, "丢过帧的这一批要上闸");

        // 关键帧到达、闸被放下之后，下一拍队列是空的（`queued == 0`）。
        // 旧实现这里传的是累计 `dropped`（=2），于是又上一道闸 —— 永远出不去。
        assert!(
            !key_gate_after_batch(0, false),
            "这一拍没丢帧就不该上闸，否则这条连接从此刻起只发关键帧"
        );

        // 闸已经在上面时保持不动：`queued` 为 0 也不该把它放下来。
        assert!(key_gate_after_batch(0, true), "已在闸上就要等关键帧");
    }

    /// 稳态：队列一拍恰好一帧，永远不上闸。这是静态桌面的常态（每秒 60 拍），
    /// 也是被这个 bug 打成 0/1 的那条路径。
    #[test]
    fn a_steady_one_frame_per_beat_never_gates() {
        let mut need_key = false;
        for _ in 0..600 {
            need_key = key_gate_after_batch(0, need_key);
            assert!(!need_key);
        }
    }
}
