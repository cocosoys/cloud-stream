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
//! | 0 | 帧类型。整帧恒 `0x01`；`0x03`/`0x04` 是 JPEG/H.264 分行模式，不在这里发 |
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
use crate::pipeline::codec::{self, VideoCodec};
use crate::pipeline::shared::DisplayKey;
use crate::pipeline::{PipelineRegistry, StreamSettings};
use crate::signaling::websession::AdmissionPermit;

/// 12 字节头 + 裸码流。
pub const HEADER_LEN: usize = 12;

/// 帧类型字节：整帧。前端 `onWire` 只把 `0x03`/`0x04` 当分行模式，别的值一律按
/// 整帧 12 字节头处理。
const TYPE_FULL_FRAME: u8 = 0x01;

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

/// 这条连接用什么身份。
///
/// 有口令的部署要先发 `AUTH_SUCCESS`：前端在 token 模式下不从 URL 片段推角色，
/// 而是等这条消息里的 `role`/`slot`（`selkies-ws-core.js:8407`）。不发就等于
/// "连上了但没有身份"，输入层不会初始化。
fn auth_frame(credential: crate::security::Credential) -> String {
    serde_json::json!({
        "role": "controller",
        "slot": 1,
        "credential": credential.label(),
    })
    .to_string()
}

/// 一条 WebSocket 画面会话。
///
/// `peer` 与 `credential` 只用于日志与 `AUTH_SUCCESS`：策略裁决在 `server.rs`
/// 升级之前就做完了，这里不该再做一次（两处判定迟早会不一致）。
pub async fn run<S>(
    ws: WebSocketStream<S>,
    cfg: Arc<Config>,
    pipelines: PipelineRegistry,
    // 只为 Drop：这个许可代表这个进程里的一个会话名额，连接结束时自动归还。
    // 不 `use` 它是有意的 —— 它的价值就是活得和这条连接一样长。
    _permit: AdmissionPermit,
    peer: IpAddr,
    credential: crate::security::Credential,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut source) = ws.split();
    let codec_now = codec::current();

    // 顺序是有意的：先身份、再模式、最后设置。前端收到 `MODE websockets` 就会
    // 开始初始化输入与音频，此时若还不知道自己的角色，它会按"未认证"走一遍，
    // 然后在 `AUTH_SUCCESS` 到达后再补一遍 —— 少发一条消息就能避免的抖动。
    if !matches!(credential, crate::security::Credential::None) {
        sink.send(Message::Text(auth_frame(credential).into()))
            .await?;
    }
    sink.send(Message::Text("MODE websockets".into())).await?;

    // 共享管线：如果此刻已有一个 WebRTC 会话在跑，这里是**加入**而不是新开一路
    // 采集 —— `DuplicateOutput` 对同一个输出是独占的，新开只会失败并退到 GDI。
    let acquired = pipelines.acquire(DisplayKey::PRIMARY, &cfg).await;
    let pipeline = acquired.pipeline.clone();

    if let Some(settings) = settings_frame(&pipeline.settings) {
        sink.send(Message::Text(settings.into())).await?;
    } else {
        log::warn!("ws media: server_settings could not be reshaped; the page keeps its own defaults");
    }

    // 网关打开，编码器开始出帧。
    pipeline.handle.start();
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    let sink_handle = pipeline.video.attach_stream(tx);
    // 观众刚到，手上没有任何参考帧：不主动要一个关键帧，他要到下一个周期性的
    // 才能出画面（默认 10 s）。这和 `SharedPipeline::attach(track, joining=true)`
    // 是同一条理由。
    pipeline.handle.request_keyframe();
    log::info!(
        "ws media: {peer} attached ({} downlink(s), codec {:?}, queue {QUEUE})",
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

    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    let Some(first) = frame else {
                        // 编码器停了（程序被释放或重建中）。
                        log::info!("ws media: pipeline stopped feeding this downlink");
                        break;
                    };
                    // 只要最新的一帧。慢观众的正确惩罚是"少看几帧"，不是"看几秒前的画面"；
                    // 队列里留着旧帧反而会让延迟越积越长，最后看起来像卡死。
                    let mut latest = first;
                    while let Ok(next) = rx.try_recv() {
                        latest = next;
                        dropped += 1;
                    }
                    if dropped > 0 && need_key == false {
                        need_key = true;
                    }
                    if !streaming {
                        continue;
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

                msg = source.next() => {
                    match msg {
                        Some(Ok(Message::Text(line))) => {
                            let line = line.as_str().trim();
                            match line {
                                "START_VIDEO" => {
                                    streaming = true;
                                    need_key = true;
                                    pipeline.handle.start();
                                    pipeline.handle.request_keyframe();
                                    log::info!("ws media: START_VIDEO");
                                }
                                // 页签切到后台。不关管线（可能还有 WebRTC 观众），
                                // 只是不再往这条连接上写。
                                "STOP_VIDEO" => {
                                    streaming = false;
                                    log::info!("ws media: STOP_VIDEO");
                                }
                                "KEYFRAME" | "REQUEST_KEYFRAME" => {
                                    need_key = true;
                                    pipeline.handle.request_keyframe();
                                }
                                other => {
                                    // 输入、剪贴板、音频的指令都会落到这里。这一版
                                    // 只做只读画面，收到就记一行 debug —— 静默丢弃
                                    // 会让"为什么点了没反应"变成没法回答的问题。
                                    if let Some(kbps) = other.strip_prefix("vb,").and_then(|v| v.trim().parse::<u32>().ok()) {
                                        pipeline.handle.set_bitrate_override(kbps);
                                        log::info!("ws media: bitrate override {kbps} kbps");
                                    } else {
                                        log::debug!("ws media: ignoring upstream `{}`", short(other));
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(frame))) => {
                            log::info!("ws media: client closed ({:?})", frame.map(|f| f.code));
                            break;
                        }
                        // 二进制上行（剪贴板图片、摄像头）这一版一概不接。
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            log::debug!("ws media: socket error: {err}");
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    pipeline.detach(sink_handle.id());
    // 不能漏：`acquire` 加了一次引用，最后一个走的负责停掉采集循环。
    pipelines.release(DisplayKey::PRIMARY).await;
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

    fn le(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[test]
    fn header_layout_matches_the_client_parser() {
        // 前端 `onWire`：head[0] 类型、head[1] 码型+关键位、[2..4] id、
        // [4..6] y、[6..8] w、[8..10] h、[10..12] reference。
        let data = Bytes::from_static(&[0x10, 0x20, 0x30]);
        let pkt = wire_frame(&data, VideoCodec::Vp8, true, 0x1234, 0x1233, 1920, 1080);
        assert_eq!(pkt.len(), HEADER_LEN + data.len());

        assert_eq!(pkt[0], 0x01, "整帧类型");
        assert_eq!(pkt[1], 0x21, "VP8(2)<<4 | key(1)");
        assert_eq!(&pkt[2..4], &[0x12, 0x34], "frameId 大端");
        assert_eq!(&pkt[4..6], &[0, 0], "y 恒零");
        assert_eq!(&pkt[6..8], &[0x07, 0x80], "1920");
        assert_eq!(&pkt[8..10], &[0x04, 0x38], "1080");
        assert_eq!(&pkt[10..12], &[0x12, 0x33], "reference 是上一帧");
        assert_eq!(&pkt[HEADER_LEN..], &data[..], "payload 原样不动");
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
}
