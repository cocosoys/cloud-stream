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

use crate::config::{Config, InputPolicy};
use crate::dispatch::clipboard::ClipboardSink;
use crate::dispatch::commands::{classify, Action};
use crate::dispatch::input_stub::InputSink;
use crate::dispatch::win_input::WinInputSink;
use crate::pipeline::codec::{self, VideoCodec};
use crate::pipeline::shared::{DisplayKey, SharedPipeline};
use crate::pipeline::{audio, PipelineRegistry, StreamSettings};
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
    // #109: WS 页面没有 hello，选不了屏幕，所以这里跟服务端默认走（`--display`）。
    let acquired = pipelines.acquire(DisplayKey(cfg.display), &cfg).await;
    let pipeline = acquired.pipeline.clone();

    if let Some(settings) = settings_frame(&pipeline.settings) {
        sink.send(Message::Text(settings.into())).await?;
    } else {
        log::warn!("ws media: server_settings could not be reshaped; the page keeps its own defaults");
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
            ..Default::default()
        })
        .await;
    // 输入租约：空着就认领。`shared` 策略下这一步不影响能不能操作，但 `health`
    // 的 `role` 与 `session/take` 都以租约为准，空着会让 WS 会话永远显示 viewer。
    pipeline.try_claim_controller(session_id);

    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    let sink_handle = pipeline.video.attach_stream(tx);
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
    let audio_sink = pipeline.audio.attach_stream(audio_tx);

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
                    // 编码器是全局单例，面板切 VP8/VP9 会重建它。这条连接手里
                    // 缓存的 `codec_now` 立刻过期，继续按旧编号发帧等于让浏览器
                    // 用错误的解码器解新码流。关掉让它重连，重连时会读到新值。
                    if codec::current() != codec_now {
                        log::info!(
                            "ws media: codec changed {} -> {}; closing so the page reconnects",
                            codec_now.wire(),
                            codec::current().wire()
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

                msg = source.next() => {
                    match msg {
                        Some(Ok(Message::Text(line))) => {
                            let line = line.as_str().trim();
                            handle_text(
                                line,
                                &pipeline,
                                &input,
                                &clipboard,
                                &mut sink,
                                &mut streaming,
                                &mut need_key,
                                &denied,
                                session_id,
                            )
                            .await?;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            log::info!("ws media: client closed ({:?})", frame.map(|f| f.code));
                            break;
                        }
                        // 二进制上行：麦克风 `0x02`、摄像头 `0x06`。这两条这条
                        // 服务端都还没有接收端（麦克风走 WebRTC 的 track，摄像头
                        // 全项目未实现），记一行 debug 而不是静默吞掉。
                        Some(Ok(Message::Binary(data))) => {
                            let kind = data.first().copied().unwrap_or(0);
                            log::debug!("ws media: binary upstream type {kind:#04x} ignored ({} B)", data.len());
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
        }
        Ok(())
    }
    .await;

    // 松开所有还按着的键：页面崩掉不会发 `kr`，而被控机上按键是全局状态。
    input.reset();
    pipeline.audio.detach(audio_sink);
    pipeline.detach(sink_handle.id());
    registry.remove(session_id).await;
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
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    streaming: &mut bool,
    need_key: &mut bool,
    denied: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    session_id: u64,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // 这几个不是 `classify` 的动词：前端在 WS 上有自己的确认/节流家族。
    match line {
        "KEYFRAME" | "REQUEST_KEYFRAME" => {
            *need_key = true;
            pipeline.handle.request_keyframe();
            return Ok(());
        }
        // 连接建立时的"只读缓存"剪贴板请求：先应答再给载荷，页面的
        // `taggedClipboardFetch` 靠这条 `clipboard_reply,cr` 结束等待。
        "cr" => {
            send_text(sink, "clipboard_reply,cr".to_owned()).await?;
            for msg in crate::dispatch::clipboard::read_clipboard_ws_messages() {
                send_text(sink, msg).await?;
            }
            return Ok(());
        }
        // 帧确认/解码节奏/多屏布局都是浏览器侧的节流信息；服务端这一版不据此
        // 调速（`vb,` 才是码率入口），记 debug 不当作错误。
        _ if line.starts_with("LOST_FRAME")
            || line.starts_with("DECODE_PACE")
            || line.starts_with("CLIENT_FRAME_ACK")
            || line.starts_with("vp,") =>
        {
            log::debug!("ws media: `{}` noted", short(line));
            return Ok(());
        }
        _ => {}
    }

    match classify(line) {
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
            audio::set_enabled(on);
            log::info!("ws media: audio {}", if on { "on" } else { "off" });
        }
        Action::SetAudioBitrate(bps) => {
            audio::set_bitrate_bps(bps as i32);
            log::info!("ws media: audio bitrate -> {} bps", audio::current_bitrate_bps());
        }
        Action::SetNativeCursorRendering(on) => {
            pipeline.handle.set_cursor_inject(on);
            log::info!("ws media: native cursor rendering -> {on}");
        }
        Action::SetSettings(payload) => {
            // 与信令会话共用同一个批处理：两套解析会把同一个 SETTINGS 解成两种
            // 结果，而面板只会相信自己那一路。
            if let Some(codec) = apply_settings_batch(&pipeline.settings, &payload) {
                // 这条连接这次不重连就没有意义了：帧头里的 codec 是启动时缓存的。
                log::info!("ws media: codec -> {}; closing for renegotiation", codec.wire());
                return Err(anyhow::anyhow!("video codec changed"));
            }
            if let Some(settings) = settings_frame(&pipeline.settings) {
                send_text(sink, settings).await?;
            }
        }
        Action::ClipboardRequest => {
            for msg in crate::dispatch::clipboard::read_clipboard_ws_messages() {
                send_text(sink, msg).await?;
            }
        }
        Action::ClipboardChunk(raw) => clipboard.handle_line(&raw),
        Action::InputStub(raw) => {
            // 租约与 WebRTC 一侧同一套语义：`shared` 谁都能操作，`exclusive`
            // 只有租约持有者。拒绝要计数，否则"指针不动"永远分不清是没到还是
            // 被门挡了。
            let may = match pipeline.input_policy() {
                InputPolicy::Shared => true,
                InputPolicy::Exclusive => pipeline.controller_id() == Some(session_id),
            };
            if may {
                input.handle_line(&raw);
            } else {
                denied.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        other => log::debug!("ws media: ignoring `{:?}`", other),
    }
    Ok(())
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
}
