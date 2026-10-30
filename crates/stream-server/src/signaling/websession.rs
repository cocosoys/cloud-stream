//! One browser signaling session: handshake state machine, SDP/ICE forwarding,
//! the Connected downstream notices and the control-channel command routing
//! (design 4.1, 4.2, 5.2, 5.3, 5.5).
//!
//! Channel split enforced here:
//! * WS socket: `HELLO`, `SESSION`, `SESSION_OK`, the offer/answer line and the
//!   local/remote ICE lines only.
//! * `control` data channel: `pipeline`, `stream_info`, `stream_stats`,
//!   `connection` JSON downstream; `START_VIDEO` / `STOP_VIDEO`, `_stats`,
//!   `ping` and the rest of the classified commands upstream.
//!
//! Cross-task WS output goes through `ws_tx`: the main loop drains the channel
//! onto the socket, so the ICE forwarder does not borrow the socket from its
//! background task.
//!
//! Stage1 holds one live session: a new connection evicts the previous one with
//! close code 4001 (signaling.js:450-453: the evicted page stays down and asks
//! the user to reload; auto-reconnecting would make two pages evict each other
//! forever).
//!
//! After the PC reaches Connected, this task spawns the gated capture/encode
//! loop (#36) and arms the 1 s stats timer; the loop owns the blocking
//! capture/encode worker and the async sample forwarder between them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use log;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame, Message},
    },
    WebSocketStream,
};

use crate::config::Config;
use crate::dispatch::{
    classify, connection, cursor, display, hostui, pipeline, server_settings, stream_info,
    stream_stats, system, Action, ClipboardSink, InputSinkRef, StreamInfo, StreamStats,
    WinInputSink, read_clipboard_update,
};
use crate::pipeline::{spawn_loop, PipelineEvent, PipelineHandle, StreamSettings};
use crate::platform::{HostSample, Sampler};
use crate::security::{self, ClipOp, Device, EventKind, MediaDir, Principal};
use crate::signaling::{
    hello_ack, ice_line, latency_measurement, parse_ws_line, session_ok, sdp_line, HandshakeState,
    IncomingLine, LatencyProbe, SignalingSession, SERVER_PEER_ID,
};
use crate::webrtc::ice::remote_from_json;
use crate::webrtc::{MediaSession, MediaState};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

/// Eviction notice handed to a running session when a newer socket takes over.
struct CloseNotice {
    code: u16,
    reason: String,
}

/// Who is connected and what the pipeline is doing (#93), published for the
/// HTTP health endpoint.
///
/// This is a *report*, not a control surface: the live numbers are read from
/// the same handles the session already owns (`PipelineHandle`,
/// `StreamSettings`), so nothing here has to be kept in sync by hand except the
/// few identity fields set when the client says `HELLO`.
#[derive(Clone, Default)]
pub struct SessionInfo {
    /// Monotonic per-process id, so a finishing session cannot clear the entry
    /// a newer session already published.
    pub id: u64,
    pub connected_at: Option<std::time::Instant>,
    /// Peer address of the signaling socket.
    pub peer_ip: String,
    /// `User-Agent` of the WebSocket upgrade — the only device fingerprint a
    /// browser gives up without being asked.
    pub user_agent: String,
    /// `client_device_id` from the hello (a UUID this browser keeps in
    /// localStorage). Empty when the page predates #93.
    pub device_id: String,
    /// `client_tab_id` from the hello: per-tab, dies with the tab.
    pub tab_id: String,
    /// `client_type` from the hello (`primary` / `secondary` / ...).
    pub role: String,
    /// The live handles, so the health endpoint reads the pipeline rather than a
    /// copy of its numbers.
    pub pipeline: Option<PipelineHandle>,
    pub settings: Option<StreamSettings>,
}

/// What the client told us about itself when it opened the WebSocket.
#[derive(Clone, Debug, Default)]
pub struct ClientMeta {
    pub ip: String,
    pub user_agent: String,
}

/// Shared registry: the newest session holds the handle that evicts the older
/// one. Stage1 single-session scope; a second browser tab evicts the first.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<Option<mpsc::UnboundedSender<CloseNotice>>>>,
    /// #93: the live session's identity, read by `GET /api/v1/health`.
    ///
    /// A **std** mutex, not the tokio one above: the value is a small struct
    /// that is never held across an `await`, and both sides want it
    /// synchronously — the publish happens in the hello branch of a `select!`
    /// arm, the read happens in a plain HTTP handler.
    info: Arc<std::sync::Mutex<Option<SessionInfo>>>,
    /// Next session id; monotonic so `clear_if` can tell whose entry it is.
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl SessionRegistry {
    /// Register this session's notice sender; evict the previous holder first.
    async fn register(&self, notice_tx: mpsc::UnboundedSender<CloseNotice>) {
        let mut guard = self.inner.lock().await;
        if let Some(old) = guard.replace(notice_tx) {
            let _ = old.send(CloseNotice {
                code: 4001,
                reason: "Session superseded by a new connection. Reload to take over.".to_owned(),
            });
        }
    }

    /// Allocate the id this session publishes under.
    fn next_session_id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// Publish (or replace) the live session's identity.
    fn publish(&self, info: SessionInfo) {
        let mut guard = self.info.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(info);
    }

    /// Drop the entry if it is still ours; a newer session's entry survives.
    fn clear_if(&self, id: u64) {
        let mut guard = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if guard.as_ref().map(|i| i.id) == Some(id) {
            *guard = None;
        }
    }

    /// #93: read handle for the HTTP side.
    pub fn info(&self) -> Arc<std::sync::Mutex<Option<SessionInfo>>> {
        self.info.clone()
    }

    /// #93: `POST /api/v1/control/kick` — drop the current session's socket
    /// through the same notice path a newer connection uses.
    pub async fn kick(&self, reason: &str) -> bool {
        let mut guard = self.inner.lock().await;
        match guard.take() {
            Some(tx) => {
                let _ = tx.send(CloseNotice {
                    code: 4002,
                    reason: reason.to_owned(),
                });
                true
            }
            None => false,
        }
    }
}

/// #62: handles of the per-session background tasks. The stats timer, the PC
/// state watcher, the ICE forwarder and the recovery-event sink all outlive a
/// single `select!` iteration, and none of them used to be cancelled — every
/// page reload parked another `Sampler` (a full process scan per second),
/// another PC and two more tasks for the lifetime of the process.
type SessionTasks = Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>;

/// Register a background task so session cleanup can cancel it.
fn push_task(tasks: &SessionTasks, handle: tokio::task::JoinHandle<()>) {
    if let Ok(mut guard) = tasks.lock() {
        guard.push(handle);
    }
}

/// Per-connection context owned by the WS task.
struct SessionCtx {
    cfg: Arc<Config>,
    session: SignalingSession,
    pipeline: PipelineHandle,
    /// #50: live fps / resolution targets, mutated by `_arg_fps` / `r,WxH` /
    /// `SETTINGS` and re-read by the capture worker every beat.
    settings: StreamSettings,
    input: InputSinkRef,
    /// #55: Windows clipboard bridge for REQUEST_CLIPBOARD / cw-cwe uplink.
    clipboard: Arc<ClipboardSink>,
    stats_on: Arc<AtomicBool>,
    /// #78: control-channel round-trip probe. Written by the ping task and the
    /// control-channel hook, read by the stats timer for `rtt_ms`.
    latency: Arc<LatencyProbe>,
    /// #77: stop flag for the game-sound downlink thread. Set on Drop, which
    /// covers every exit path (the thread is a plain OS thread, not a tokio
    /// task, so `tasks.abort()` cannot reach it).
    audio_stop: Arc<AtomicBool>,
    /// #77: set once the audio thread was started for this session, so an ICE
    /// reconnect does not open a second loopback capture on the same endpoint.
    audio_started: bool,
    media: Option<Arc<Mutex<MediaSession>>>,
    /// #62: background tasks owned by this session (see `SessionTasks`).
    tasks: SessionTasks,
    /// Shared WS outbound channel for background tasks (ICE forwarder).
    ws_tx: mpsc::UnboundedSender<Message>,
    /// Ends the WS loop when the PC reaches a terminal state.
    done_tx: mpsc::UnboundedSender<()>,
    /// #93: who connected (peer address, UA) and the id this session publishes
    /// under; the registry also publishes the live state for `/api/v1/health`.
    client: ClientMeta,
    session_id: u64,
    registry: SessionRegistry,
}

impl SessionCtx {
    fn new(
        cfg: Arc<Config>,
        ws_tx: mpsc::UnboundedSender<Message>,
        done_tx: mpsc::UnboundedSender<()>,
        client: ClientMeta,
        session_id: u64,
        registry: SessionRegistry,
    ) -> Self {
        // #62: the sink needs the live frame size so it can map the frontend's
        // frame-space pointer coordinates onto the physical screen.
        let settings = StreamSettings::from_config(&cfg);
        let input: InputSinkRef = Arc::new(WinInputSink::new(settings.clone()));
        // #80: the audio switches are process-wide (the audio thread is not
        // tied to the select loop), so a fresh connection resets them. Without
        // this, a reload while muted would come back silent with the panel
        // showing "Sound on".
        crate::pipeline::audio::set_enabled(true);
        crate::pipeline::audio::set_bitrate_bps(0);
        Self {
            session: SignalingSession::new(),
            pipeline: PipelineHandle::new(),
            settings,
            input,
            clipboard: Arc::new(ClipboardSink::default()),
            stats_on: Arc::new(AtomicBool::new(false)),
            latency: Arc::new(LatencyProbe::new()),
            audio_stop: Arc::new(AtomicBool::new(false)),
            audio_started: false,
            media: None,
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            cfg,
            ws_tx,
            done_tx,
            client,
            session_id,
            registry,
        }
    }
}

impl Drop for SessionCtx {
    fn drop(&mut self) {
        // #77: the loopback thread exits on this flag; its `write_sample` would
        // otherwise keep pushing into a closed track until the next beat.
        self.audio_stop.store(true, Ordering::Relaxed);
    }
}

/// Run one upgraded signaling socket to its terminal state.
pub async fn run<S>(
    ws: WebSocketStream<S>,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    client: ClientMeta,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (done_tx, done_rx) = mpsc::unbounded_channel::<()>();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel::<CloseNotice>();
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<Message>();
    registry.register(notice_tx).await;

    let session_id = registry.next_session_id();
    let mut ctx = SessionCtx::new(cfg, ws_tx, done_tx, client, session_id, registry);
    let mut ws = ws;

    let outcome = session_loop(&mut ws, &mut ctx, notice_rx, done_rx, ws_rx).await;

    // Cleanup: PC, channels, ICE sender, timers. The media handle lives only in
    // the WS task, so closing here owns both paths the state callback could have
    // parked.
    // #62: release everything the browser left pressed *before* the PC goes
    // away — a vanished tab used to leave WASD held on the host until someone
    // physically touched the keyboard.
    ctx.input.reset();
    if let Some(media) = ctx.media.take() {
        let _ = media.lock().await.close().await;
    }
    ctx.pipeline.stop_loop();
    // #93: the health endpoint must go back to `idle` even if this session
    // crashed its way out; `clear_if` keeps a newer session's entry intact.
    ctx.registry.clear_if(ctx.session_id);
    // #62: cancel the per-session background tasks (stats timer, state watcher,
    // ICE forwarder, recovery sink). Without this a reload leaked them.
    let tasks = {
        let mut guard = ctx.tasks.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    };
    for task in tasks {
        task.abort();
    }
    log::debug!("session background tasks cancelled");

    if let Err(err) = outcome {
        log::warn!("signaling session ended with error: {err:#}");
    } else {
        log::info!("signaling session closed cleanly");
    }
    Ok(())
}

/// The main WS select loop: inbound signaling lines, the eviction notice, the
/// terminal done signal and the shared outbound channel.
async fn session_loop<S>(
    ws: &mut WebSocketStream<S>,
    ctx: &mut SessionCtx,
    mut notice_rx: mpsc::UnboundedReceiver<CloseNotice>,
    mut done_rx: mpsc::UnboundedReceiver<()>,
    mut ws_rx: mpsc::UnboundedReceiver<Message>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(line))) => handle_ws_line(ws, ctx, &line).await?,
                    Some(Ok(Message::Binary(_))) => {
                        log::debug!("binary message on signaling WS ignored (channel split: DC only)");
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Frame(_))) => {
                        // Raw frames are surfaced by the library internals only;
                        // the socket level never delivers them here.
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(err)) => return Err(anyhow!("WS read error: {err}")),
                    None => break,
                }
            }

            outbound = ws_rx.recv() => {
                if let Some(out) = outbound {
                    ws.send(out).await?;
                }
            }

            notice = notice_rx.recv() => {
                if let Some(notice) = notice {
                    let frame = CloseFrame {
                        code: CloseCode::Bad(notice.code),
                        reason: notice.reason.into(),
                    };
                    let _ = ws.send(Message::Close(Some(frame))).await;
                    log::info!("evicted by a newer session (code {})", notice.code);
                    break;
                }
            }

            _ = done_rx.recv() => {
                log::info!("media terminal state reported; closing session");
                let _ = ws.send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "media closed".into(),
                }))).await;
                break;
            }
        }
    }
    Ok(())
}

/// Dispatch one inbound signaling line according to the handshake state.
async fn handle_ws_line<S>(
    ws: &mut WebSocketStream<S>,
    ctx: &mut SessionCtx,
    line: &str,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match parse_ws_line(line) {
        IncomingLine::Hello { peer_type, meta } => {
            log::info!("client hello: type={peer_type}");
            // #93: publish who is connected, for `GET /api/v1/health`. Done
            // here rather than at upgrade time because the device id and the
            // role only exist in the hello.
            let meta_ref = meta.as_ref();
            let read = |key: &str| -> String {
                meta_ref
                    .and_then(|m| m.get(key))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };
            ctx.registry.publish(SessionInfo {
                id: ctx.session_id,
                connected_at: Some(std::time::Instant::now()),
                peer_ip: ctx.client.ip.clone(),
                user_agent: ctx.client.user_agent.clone(),
                device_id: read("client_device_id"),
                tab_id: read("client_tab_id"),
                role: if read("client_type").is_empty() { peer_type.clone() } else { read("client_type") },
                pipeline: Some(ctx.pipeline.clone()),
                settings: Some(ctx.settings.clone()),
            });
            ctx.session.meta = meta;
            ctx.session.transition(HandshakeState::HelloSent);
            ws.send(Message::Text(hello_ack().into())).await?;
        }

        IncomingLine::SessionRequest { target } => {
            log::info!("session request from {target}");
            // #62: a duplicate SESSION on the same socket used to build a
            // second PC (plus a second pipeline and stats timer) and leave the
            // first one running. The handshake is one-shot per connection.
            if ctx.media.is_some() {
                log::warn!("duplicate SESSION ignored: media session already exists");
                return Ok(());
            }
            start_media_session(ctx).await?;
            ctx.session.transition(HandshakeState::SessionReady);
            ws.send(Message::Text(session_ok().into())).await?;

            let Some(media) = &ctx.media else {
                anyhow::bail!("media session missing after SESSION");
            };
            let offer = media.lock().await.make_offer().await?;
            ctx.session.transition(HandshakeState::OfferSent);
            let offer_line = sdp_line(SERVER_PEER_ID, "offer", &offer.sdp);
            ws.send(Message::Text(offer_line.into())).await?;
        }

        IncomingLine::Sdp { peer_id, sdp_type, sdp } => {
            log::debug!("remote SDP {peer_id} type={sdp_type}");
            if sdp_type != "answer" {
                log::warn!("ignoring non-answer remote SDP type={sdp_type} (server always offers)");
                return Ok(());
            }
            let Some(media) = &ctx.media else {
                anyhow::bail!("answer before media session");
            };
            media.lock().await.apply_answer(&sdp_type, &sdp).await?;
            ctx.session.transition(HandshakeState::AnswerApplied);
            // #94: our m-line named one codec, and the answer is where a
            // browser that cannot decode it says no. That refusal is silent —
            // the transport still connects (the data channels are codec-free),
            // so the session would come up healthy with a black picture. Put
            // the process back on the codec everyone can decode and take the
            // reconnect the client already handles.
            let codec = crate::pipeline::codec::current();
            if let Some(reason) = refused_video_codec(&sdp, codec) {
                log::error!(
                    "client refused the {} video line ({reason})",
                    codec.label()
                );
                // Only worth restarting when there is something to fall back
                // *to*. Already on VP8 there is no cheaper codec left, and a
                // kick would turn one refusal into a reload every two seconds.
                if codec != crate::pipeline::codec::VideoCodec::Vp8 {
                    crate::pipeline::codec::set(crate::pipeline::codec::VideoCodec::Vp8);
                    let _ = ctx
                        .registry
                        .kick("video codec not supported by this browser; retrying with VP8")
                        .await;
                }
            }
        }

        IncomingLine::Ice { peer_id, candidate } => {
            log::debug!("remote ICE candidate from {peer_id}");
            let Some(media) = &ctx.media else {
                anyhow::bail!("ICE candidate before media session");
            };
            if let Some(candidate) = remote_from_json(&candidate) {
                if let Err(err) = media.lock().await.add_remote(candidate).await {
                    log::warn!("failed to apply remote candidate: {err}");
                }
            } else {
                log::debug!("remote candidate missing required fields; ignored");
            }
        }

        IncomingLine::Unrecognized(raw) => {
            log::debug!("unrecognized WS line ignored: {raw}");
        }
    }
    Ok(())
}

/// Build the PC, two channels and the VP8 track once the client asks for a
/// session; subscribe to its state, start the stats timer and the ICE forwarder.
async fn start_media_session(ctx: &mut SessionCtx) -> Result<()> {
    let media = Arc::new(Mutex::new(MediaSession::new().await?));
    ctx.media = Some(media.clone());

    // #62: worker-termination sink. A panicking or erroring capture worker used
    // to leave the PC, both data channels and the WS alive, so the browser sat
    // on a frozen picture with the stats panel still ticking. Closing the
    // signaling socket instead lets the frontend reconnect.
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<String>();
    ctx.pipeline.set_exit_sink(exit_tx);
    let exit_done = ctx.done_tx.clone();
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            if let Some(reason) = exit_rx.recv().await {
                log::error!("pipeline worker stopped unexpectedly: {reason}");
                let _ = exit_done.send(());
            }
        }),
    );

    let state_rx = media.lock().await.state_rx();
    let done_tx = ctx.done_tx.clone();
    let cfg = ctx.cfg.clone();

    // PC state watcher: Connected arms the downstream notices and the DC hook;
    // Failed/Closed report the terminal state to the WS loop. Each task owns its
    // own Arc clones; the shared `media` value stays behind for the ICE forwarder.
    let watcher_media = media.clone();
    let watcher_stats_on = ctx.stats_on.clone();
    let watcher_latency = ctx.latency.clone();
    let watcher_pipeline = ctx.pipeline.clone();
    let watcher_input = ctx.input.clone();
    let watcher_clipboard = ctx.clipboard.clone();
    let watcher_settings = ctx.settings.clone();
    let watcher_tasks = ctx.tasks.clone();
    let watcher_registry = ctx.registry.clone();
    let watcher_audio_stop = ctx.audio_stop.clone();
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            let mut state_rx = state_rx;
            let mut loop_started = false;
            let mut audio_started = false;
            loop {
                if state_rx.changed().await.is_err() {
                    break;
                }
                // Copy the state out before any await: the watch guard is not Send
                // and must not survive across the async arm call below.
                let state = *state_rx.borrow();
                match state {
                    MediaState::Connected => {
                        if let Err(err) = arm_connected_notices(
                            &watcher_media,
                            &watcher_pipeline,
                            &cfg,
                            &watcher_stats_on,
                            &watcher_latency,
                            &watcher_input,
                            &watcher_clipboard,
                            &watcher_settings,
                            &watcher_tasks,
                            &watcher_registry,
                        )
                        .await
                        {
                            log::warn!("Connected notice arming failed: {err:#}");
                            let _ = done_tx.send(());
                            return;
                        }
                        if !loop_started {
                            loop_started = true;
                            // #53: PLI/FIR from the browser -> force keyframes on
                            // the pipeline. Spawn once per session alongside the
                            // capture loop (both exit on PC teardown).
                            watcher_media
                                .lock()
                                .await
                                .spawn_pli_listener(watcher_pipeline.clone());
                            let video = watcher_media.lock().await.video().clone();
                            spawn_loop(
                                cfg.clone(),
                                video,
                                watcher_pipeline.clone(),
                                watcher_settings.clone(),
                            );
                            log::info!("pipeline loop spawned after PC connected");
                        } else {
                            // #62: a second Connected (ICE re-connect after a
                            // transient Disconnected) never re-ran `on_open`, so
                            // the panel kept stale settings / capture state.
                            resend_notices(&watcher_media, &watcher_settings).await;
                        }
                        // #77: the sound track needs the same `Connected` gate as
                        // video, but its own lifecycle — a second Connected must
                        // not open a second loopback capture on the same endpoint.
                        if !audio_started {
                            audio_started = true;
                            // #83: both media clocks restart with the audio
                            // thread, so a reconnect does not inherit the skew of
                            // the session that just ended.
                            crate::pipeline::avsync::reset();
                            let audio = watcher_media.lock().await.audio().clone();
                            crate::pipeline::audio::spawn(audio, watcher_audio_stop.clone());
                            log::info!("audio downlink spawned after PC connected");
                        }
                    }
                    MediaState::Failed | MediaState::Closed => {
                        log::warn!("PC terminal state reported; signaling loop ending");
                        let _ = done_tx.send(());
                        return;
                    }
                    MediaState::New => {}
                }
            }
        }),
    );

    // #78: control-channel RTT probe. One ping per second; the browser answers
    // with a bare `pong,<epoch_s>` on the same ordered channel (webrtc.js:775-
    // 777) and the control hook folds the round trip into `latency`. Started
    // with the session rather than on the first Connected so a reconnect does
    // not leave the panel without a number; while the channel is not open the
    // send fails and the probe is disarmed again instead of counting a loss.
    let ping_media = media.clone();
    let ping_probe = ctx.latency.clone();
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::signaling::PING_INTERVAL);
            // `interval`'s first tick completes immediately; drop it so the
            // first ping follows a real interval.
            tick.tick().await;
            let mut announced = false;
            loop {
                tick.tick().await;
                let line = ping_probe.ping_line();
                let sent = ping_media
                    .lock()
                    .await
                    .channels()
                    .send_control(&line)
                    .await
                    .is_ok();
                if sent {
                    if !announced {
                        announced = true;
                        log::info!("rtt probe: first ping sent on the control channel");
                    }
                } else {
                    ping_probe.cancel_pending();
                }
            }
        }),
    );

    // Stats timer: one sample per second while the client subscribed with
    // `_stats,1`. Pipeline snapshot fills encoded_fps / encode_ms / pipeline_ms;
    // the window resets after each snapshot so the next tick starts fresh.
    let stats_media = media.clone();
    let stats_flag = ctx.stats_on.clone();
    let stats_pipeline = ctx.pipeline.clone();
    let stats_latency = ctx.latency.clone();
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            let mut sampler = match Sampler::new() {
                Ok(s) => s,
                Err(err) => {
                    log::warn!("host sampler unavailable: {err:#}; stats task exits");
                    return;
                }
            };
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            // One line per session at the first sample: it settles "is the host
            // loaded or is this process" without needing the browser, and it
            // proves the whole-machine counter opened (it reads 0 if PDH failed).
            let mut announced = false;
            let mut stats_announced = false;
            loop {
                tick.tick().await;
                if !stats_flag.load(Ordering::SeqCst) {
                    continue;
                }
                sampler.refresh();
                let host: HostSample = sampler.sample();
                if !announced {
                    announced = true;
                    log::info!(
                        "host sample: cpu {:.1}% of {} logical cores (this process {:.1}% of one core), mem {:.0}/{:.0} MiB",
                        host.cpu_percent,
                        host.cpu_cores,
                        host.process_cpu_percent,
                        host.mem_used as f64 / 1048576.0,
                        host.mem_total as f64 / 1048576.0,
                    );
                }
                let stats_arc = stats_pipeline.stats();
                // #62: snapshot + window reset must share one lock. Taken
                // separately, counters accumulated between the two acquisitions
                // were dropped, so `encoded_fps` dipped on a regular cadence.
                let snapshot = {
                    let mut guard = stats_arc.lock().unwrap_or_else(|p| p.into_inner());
                    let mut snapshot = guard.snapshot(std::time::Duration::from_secs(1));
                    // #78: the peak tracker needs `&mut` and its own "now", so
                    // it is closed here rather than inside `snapshot`.
                    snapshot.peak_mbps = guard.peak.take_peak_mbps(std::time::Instant::now());
                    guard.reset_window();
                    snapshot
                };
                let stats = StreamStats::from_host(host)
                    .with_pipeline(&snapshot)
                    .with_rtt(stats_latency.rtt_ms())
                    // #84: the QoS controller lives in the capture loop; this is
                    // the only path from there to the panel.
                    .with_throttled(stats_pipeline.is_throttled());
                // #78: one line when a subscriber arrives. `stream_stats` is
                // otherwise invisible in the log (it only exists while `_stats,1`
                // is active), so "the panel is empty" had no server-side answer:
                // this separates "we never sent" from "the browser drew nothing".
                if !stats_announced {
                    stats_announced = true;
                    log::info!(
                        "stream_stats: first sample sent (video {:.2}/{:.2} Mbps, peak {:.2}, send {:.1} ms, rtt {})",
                        stats.video_mbps,
                        stats.target_mbps,
                        stats.peak_mbps,
                        stats.send_ms,
                        match stats.rtt_ms {
                            Some(rtt) => format!("{rtt:.1} ms"),
                            None => String::from("not measured yet"),
                        }
                    );
                }
                if let Err(err) = stats_media
                    .lock()
                    .await
                    .channels()
                    .send_control(&stream_stats(&stats))
                    .await
                {
                    log::debug!("stream_stats send skipped: {err:#}");
                }
            }
        }),
    );

    // ICE forwarder: drain locally gathered candidates onto the shared WS
    // channel, wrapped as `<peer_id> {"ice": {...}}`. The receiver is owned by
    // the forwarder task (tokio mpsc receivers are not Clone).
    let mut ice_rx = media
        .lock()
        .await
        .take_ice_rx()
        .ok_or_else(|| anyhow!("ICE receiver already taken"))?;
    let ws_tx = ctx.ws_tx.clone();
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            while let Some(raw_json) = ice_rx.recv().await {
                let Ok(value) = hbb_common::serde_json::from_str::<hbb_common::serde_json::Value>(
                    &raw_json,
                ) else {
                    log::warn!("failed to deserialize local candidate: {raw_json}");
                    continue;
                };
                let line = ice_line(SERVER_PEER_ID, &value);
                if ws_tx.send(Message::Text(line.into())).is_err() {
                    break;
                }
            }
        }),
    );

    Ok(())
}

/// 把控制通道上的一条已分类指令映射成鉴权骨架认的面（#88）。
///
/// 返回 `None` 表示这条指令不构成"访问"：`pong` 是服务端 ping 的回执，
/// `Ignore` 是设计上就该丢弃的 B 组噪音，`Unknown` 是解不出来的行 —— 给它们
/// 记审计只会淹没真正的拒绝。**其余每一条都必须映射**，漏掉一条就等于那条
/// 指令永远绕过策略。
fn surface_of(action: &Action) -> Option<EventKind> {
    Some(match action {
        Action::StartVideo | Action::StopVideo => EventKind::Media(MediaDir::VideoOut),
        Action::SetAudioOn(_) | Action::SetAudioBitrate(_) => EventKind::Media(MediaDir::AudioOut),
        Action::SetFps(_) => EventKind::Control("control.fps"),
        Action::SetResolution(..) => EventKind::Control("control.resolution"),
        // #89: reflowing the host desktop is a bigger side effect than asking
        // the scaler for a size, so it gets its own control surface name — a
        // future policy must be able to allow `control.resolution` and refuse
        // `control.display`.
        Action::SetDisplayResolution(..) | Action::ResetDisplayResolution => {
            EventKind::Control("control.display")
        }
        // #90: these two move the *host's own* settings, not the stream. They
        // are separate surfaces from `control.display` for the same reason that
        // one is separate from `control.resolution`: a policy may well allow a
        // desktop reflow and still refuse to touch the user's pointer.
        Action::SetMouseDpi(_) => EventKind::Control("control.mouse-dpi"),
        Action::SetUiScale(_) => EventKind::Control("control.ui-scale"),
        Action::SetSettings(_) => EventKind::Control("control.settings"),
        Action::SetBitrate(_) => EventKind::Control("control.bitrate"),
        // #92: rate control decides how every subsequent frame is quantised, so
        // it is its own surface: a policy can cap the bitrate without also
        // letting the client swap the encoder's whole allocation strategy.
        Action::SetRateControl(_) => EventKind::Control("control.rate-control"),
        Action::SetCrf(_) => EventKind::Control("control.crf"),
        Action::SetNativeCursorRendering(_) => EventKind::Control("control.cursor-rendering"),
        Action::CompressedNegotiate => EventKind::Control("control.gzip"),
        Action::StatsOn | Action::StatsOff => EventKind::Diagnostics("control.stats"),
        // 统计开关只决定"推不推数字"，改不了主机，归到诊断面。
        Action::ClipboardRequest => EventKind::Clipboard(ClipOp::Read),
        Action::ClipboardChunk(_) => EventKind::Clipboard(ClipOp::Write),
        Action::InputStub(raw) => EventKind::Input(device_of(raw)),
        Action::PongReply(_) | Action::Ignore(_) | Action::Unknown(_) => return None,
    })
}

/// 输入行 → 设备。只看首 token，与 `dispatch::commands::classify` 的切分规则
/// 一致（逗号分隔，裸命令无余量）。
///
/// `kr`（松键）与 `kd`/`ku`/`kh` 是同一类动作，归键盘。`sfe`（焦点事件）与
/// `vp`（视口尺寸）既不按键也不移动指针，落到 `Other` —— 归错类会让"只允许
/// 键鼠"的策略把视口同步一起放行，而那正是想单独关掉的东西。
fn device_of(line: &str) -> Device {
    let head = line.split([',', ' ']).next().unwrap_or("");
    match head {
        "kd" | "ku" | "kh" | "kr" => Device::Keyboard,
        "m" | "m2" | "p" | "_pointer_echo" | "pointer" => Device::Pointer,
        "sf" => Device::Touch,
        "js" => Device::Gamepad,
        _ => Device::Other,
    }
}

/// After Connected: register the control-channel upstream command hook and arm
/// the one-shot downstream notices to fire when the channel actually opens. The
/// PC `Connected` state can arrive a moment before the `control` channel does,
/// so sending on the state callback risks an `ErrClosedPipe`; the `on_open`
/// handler matches the frontend's `ondatachannelopen` ordering (webrtc.js:620).
async fn arm_connected_notices(
    media: &Arc<Mutex<MediaSession>>,
    pipeline_handle: &PipelineHandle,
    cfg: &Arc<Config>,
    stats_on: &Arc<AtomicBool>,
    latency: &Arc<LatencyProbe>,
    input: &InputSinkRef,
    clipboard: &Arc<ClipboardSink>,
    settings: &StreamSettings,
    tasks: &SessionTasks,
    // #94: a codec pick has to restart the session, and the restart is this
    // registry's close notice — the same one `/api/v1/control/session/kick`
    // sends. Passed in rather than reached for globally so the DC hook stays a
    // plain set of handles.
    registry: &SessionRegistry,
) -> Result<()> {
    // #72: both channels get a handler; the frontend splits input across them
    // (motion on `pointer`, everything else on `control`), and webrtc-rs drops
    // messages on a channel with no `on_message`.
    let (control, pointer) = {
        let session = media.lock().await;
        let channels = session.channels();
        (channels.control().clone(), channels.pointer().clone())
    };
    // Recovery event sink (#42-1): the blocking capture worker emits
    // `PipelineEvent` on rebuild / GDI fallback / fatal loss / restore; this task
    // maps each event to the whitelisted downstream message so the state stays
    // visible on the frontend status bar instead of living only in server logs.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PipelineEvent>();
    pipeline_handle.set_event_sink(event_tx);
    let event_control = control.clone();
    push_task(
        tasks,
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                let line = match ev {
                    PipelineEvent::RecoveryStart(n) => Some(system(&format!("capture_recovery,{n}"))),
                    PipelineEvent::FallbackGdi(reason) => {
                        Some(system(&format!("capture_fallback_gdi,{reason}")))
                    }
                    PipelineEvent::Lost(reason) => Some(system(&format!("capture_lost,{reason}"))),
                    PipelineEvent::Restored => Some(system("capture_restored")),
                    PipelineEvent::StreamInfo { is_gdi, reason } => {
                        Some(stream_info(&StreamInfo::from_capture(is_gdi, reason)))
                    }
                    PipelineEvent::Cursor(cd) => Some(cursor(&cd)),
                };
                let Some(line) = line else { continue };
                if event_control.ready_state() != RTCDataChannelState::Open {
                    log::debug!("recovery event skipped: control channel not open");
                    continue;
                }
                if let Err(err) = event_control.send_text(&line).await {
                    log::debug!("recovery event send skipped: {err:#}");
                }
            }
        }),
    );

    // One-shot downstream notices, queued as plain lines for the open handler.
    // #50: `server_settings` is re-announced on every client settings request so
    // the frontend layout (manual resolution, framerate span) tracks the live
    // targets; the one-shot here seeds the first connect.
    let notices = [
        pipeline("Connected to server"),
        stream_info(&StreamInfo::default()),
        server_settings(settings),
        connection(false),
    ];
    let control_open = control.clone();
    control.on_open(Box::new(move || {
        Box::pin(async move {
            for line in notices {
                if let Err(err) = control_open.send_text(&line).await {
                    log::warn!("downstream notice skipped: {err:#}");
                }
            }
            log::debug!("control channel open: pipeline / stream_info / connection sent");
        })
    }));

    // Cheap clones: the DC hook only touches the gate flag, the stats flag and
    // its own channel handle. The hook owns separate Arc clones so the
    // `on_message(&self)` borrow does not move the outer `control`.
    let pipeline_handle = pipeline_handle.clone();
    let stats_on = stats_on.clone();
    let input = input.clone();
    let clipboard = clipboard.clone();
    let control_hook = control.clone();
    // #78: the RTT probe the `pong` reply feeds.
    let latency = latency.clone();
    // #72: `Arc<dyn InputSink>` clone for the pointer-channel hook. Taken
    // before `input` is moved into the control hook below.
    let pointer_input = input.clone();
    // #50: the settings writer is shared with the pipeline worker.
    let hook_settings = settings.clone();
    // #94: the codec pick's restart path.
    let codec_registry = registry.clone();

    // #72: the unordered `pointer` channel carries exactly the coalesced pointer
    // motion — `Input._sendPointer(fields, motion=true)` goes to
    // `webrtc.sendMotionMessage` (selkies-wr-core.js:3189) which sends on the
    // `pointer` channel while it is open (webrtc.js:898). webrtc-rs hands a
    // message to the channel's `on_message` and **drops it when there is none**,
    // so until this hook existed the server threw away every `m` (absolute,
    // coalesced) and every `m2` (pointer-locked relative) message. Clicks kept
    // working because a press is sent with `motion=false` on `control`, which
    // made the failure look like "keyboard and clicking are fine, the pointer
    // just does not move" (#70).
    //
    // Only input is accepted here: `control` owns settings, clipboard and the
    // downstream channel, and none of that belongs on an unreliable stream.
    // #88: DataChannel 层的 principal 是空的 —— peer 地址只在 TCP 建连那一刻
    // 存在，属于 WS 升级时的上下文，这里拿不到；会话号也还没引入。未来做
    // 鉴权时，正确做法是在 WS 升级把 token 解析成角色、随 MediaSession 带进
    // 来，这里换掉 `Principal::default()` 即可，插桩点不用动。
    let dc_principal = Principal::default();
    // 两个闭包都是 `move`，各拿一份；`Principal` 是小结构，clone 比 Arc 便宜。
    let pointer_principal = dc_principal.clone();
    let control_principal = dc_principal;

    pointer.on_message(Box::new(move |msg| {
        let input = pointer_input.clone();
        let principal = pointer_principal.clone();
        Box::pin(async move {
            if !msg.is_string {
                log::debug!("binary message on pointer channel ignored");
                return;
            }
            let line = String::from_utf8_lossy(&msg.data).trim().to_owned();
            if line.is_empty() {
                return;
            }
            // #73: counted before classification — arrival is the fact under
            // test, not what the line parses to.
            crate::dispatch::win_input::note_pointer_channel_msg(&line);
            let action = classify(&line);
            // #88: 裁决必须在 `handle_line` 之前。放在后面就成了"先注入再
            // 记账"，拒绝没有任何意义。
            if let Some(kind) = surface_of(&action) {
                if !security::allow(kind, &principal) {
                    log::debug!("pointer message denied: {}", kind.label());
                    return;
                }
            }
            match action {
                Action::InputStub(raw) => input.handle_line(&raw),
                _ => log::debug!("non-input message on pointer channel dropped: {line}"),
            }
        })
    }));

    control.on_message(Box::new(move |msg| {
        let control = control_hook.clone();
        let pipeline_handle = pipeline_handle.clone();
        let stats_on = stats_on.clone();
        let latency = latency.clone();
        let input = input.clone();
        let clipboard = clipboard.clone();
        let settings = hook_settings.clone();
        let principal = control_principal.clone();
        // #94: cloned per call like every other handle here — the closure is
        // `FnMut` and must not move anything out of its captures.
        let codec_registry = codec_registry.clone();
        Box::pin(async move {
            if !msg.is_string {
                log::debug!("binary control message ignored in stage1");
                return;
            }
            let line = String::from_utf8_lossy(&msg.data).trim().to_owned();
            if line.is_empty() {
                return;
            }
            let action = classify(&line);
            // #88: 控制通道是所有非指针指令的唯一入口（设置、码率、剪贴板、
            // 音频开关、视频开关），所以一处裁决就覆盖了整片 —— 这也是为什么
            // 它值得插桩而不是每个分支各写一遍。
            if let Some(kind) = surface_of(&action) {
                if !security::allow(kind, &principal) {
                    log::debug!("control message denied: {}", kind.label());
                    return;
                }
            }
            match action {
                Action::StartVideo => {
                    pipeline_handle.start();
                    log::debug!("START_VIDEO: capture gate opened");
                }
                Action::StopVideo => {
                    pipeline_handle.stop();
                    log::debug!("STOP_VIDEO: capture gate closed");
                }
                Action::StatsOn => stats_on.store(true, Ordering::SeqCst),
                Action::StatsOff => stats_on.store(false, Ordering::SeqCst),
                Action::CompressedNegotiate => {
                    // Stage1 never compresses upstream; answer the capability
                    // probe with gzip disabled.
                    let _ = control.send_text("_gz,0").await;
                }
                Action::PongReply(_ts) => {
                    // #78: the answer to our own ping. The payload is the
                    // browser's clock and is not used; the round trip is the
                    // time since we armed the probe. Announce the smoothed
                    // figure so engines without `currentRoundTripTime` have
                    // one (`lib/webrtc.js:778-781`).
                    if let Some(rtt) = latency.on_reply() {
                        let _ = control.send_text(&latency_measurement(rtt)).await;
                    }
                }
                // #50 setting requests: mutate the live StreamSettings; the
                // pipeline re-bases its cadence / rebuilds on the next beat.
                Action::SetFps(fps) => {
                    log::info!("client sets fps -> {fps}");
                    settings.set_fps(fps);
                    let _ = control.send_text(server_settings(&settings)).await;
                }
                Action::SetResolution(w, h) => {
                    log::info!("client sets resolution -> {w}x{h}");
                    settings.set_resolution(w, h);
                    let _ = control.send_text(server_settings(&settings)).await;
                }
                // #89: `dr,WxH` reflows the *host* desktop so the capture is
                // 1:1 with the client viewport. `ChangeDisplaySettingsW` blocks
                // for a few hundred milliseconds while the driver re-syncs, so
                // it goes to a blocking worker; the session is deliberately
                // kept waiting, because the frames are about to change size and
                // every input in flight would be mapped against the old one.
                Action::SetDisplayResolution(w, h) => {
                    let (w, h) = (even_side(w), even_side(h));
                    log::info!("client sets display mode -> {w}x{h}");
                    match tokio::task::spawn_blocking(move || display::apply(w, h)).await {
                        Ok(Ok(applied)) => {
                            log::info!(
                                "display mode {}x{} -> {}x{} (strategy={}, exact={})",
                                applied.requested.0,
                                applied.requested.1,
                                applied.applied.0,
                                applied.applied.1,
                                applied.strategy,
                                applied.exact
                            );
                            // Pin the scaler target to the new desktop size:
                            // asking for anything else would reintroduce the
                            // ratio this switch exists to remove. The pipeline
                            // sees the size change and rebuilds the pair.
                            settings.set_resolution(applied.applied.0, applied.applied.1);
                            let _ = control.send_text(&display_mode_report(&applied)).await;
                            let _ = control.send_text(server_settings(&settings)).await;
                        }
                        Ok(Err(reason)) => {
                            log::warn!("display mode switch refused: {reason}");
                            let _ = control
                                .send_text(&display_mode_failure(w, h, &reason))
                                .await;
                        }
                        Err(join) => {
                            log::warn!("display mode worker failed: {join}");
                            let _ = control
                                .send_text(&display_mode_failure(w, h, "worker join error"))
                                .await;
                        }
                    }
                }
                // #89: `dr,reset` — back to whatever the host had before the
                // first switch of this process. Also re-pins the scaler target,
                // so the restored desktop is transmitted 1:1 instead of being
                // squeezed back into the viewport-sized buffer.
                Action::ResetDisplayResolution => {
                    match tokio::task::spawn_blocking(display::restore).await {
                        Ok(Ok((w, h))) => {
                            log::info!("display mode restored -> {w}x{h}");
                            settings.set_resolution(w, h);
                            let _ = control.send_text(server_settings(&settings)).await;
                        }
                        Ok(Err(reason)) => log::warn!("display restore refused: {reason}"),
                        Err(join) => log::warn!("display restore worker failed: {join}"),
                    }
                }
                Action::SetSettings(payload) => {
                    let switched = apply_settings_batch(&settings, &payload);
                    // The echo goes out first: the panel repaints from it, and
                    // after a codec pick it is the only chance to show the new
                    // value before the socket closes.
                    let _ = control.send_text(server_settings(&settings)).await;
                    // #94: a negotiated codec cannot change inside a live
                    // PeerConnection. The pick is recorded, so the restart is
                    // the whole mechanism: the client reconnects on its own
                    // (~2 s, code 4002) and its next offer carries the new
                    // m-line. Nothing is silently deferred — a session that
                    // stayed on the old codec while the panel said otherwise is
                    // the #91 lie in a new place.
                    if let Some(codec) = switched {
                        log::info!(
                            "video codec -> {}: restarting the session so the new m-line can be negotiated",
                            codec.wire()
                        );
                        if !codec_registry.kick("video codec changed; reconnecting").await {
                            log::warn!("codec changed but there is no live session to restart yet");
                        }
                    }
                }
                // #90: `md,<dpi>` — the host's pointer speed, expressed as DPI.
                // `SPI_SETMOUSESPEED` is a user-session call (it writes the
                // profile and broadcasts a settings change), so it goes to a
                // blocking worker like the display switch does. The reply names
                // the step that actually landed: Windows only has 20 of them, so
                // a typed 780 becomes 800 and the panel says so.
                Action::SetMouseDpi(dpi) => {
                    log::info!("client sets host pointer dpi -> {dpi}");
                    match tokio::task::spawn_blocking(move || hostui::apply_mouse_dpi(dpi)).await {
                        Ok(Ok(applied)) => {
                            log::info!(
                                "host pointer dpi {} -> {} (speed={}, exact={}, via={})",
                                applied.requested,
                                applied.applied,
                                applied.speed,
                                applied.exact,
                                applied.strategy
                            );
                            let _ = control.send_text(&mouse_dpi_report(&applied)).await;
                        }
                        Ok(Err(reason)) => {
                            log::warn!("host pointer dpi refused: {reason}");
                            let _ = control
                                .send_text(&host_knob_failure("mouse_dpi", dpi, &reason))
                                .await;
                        }
                        Err(join) => {
                            log::warn!("host pointer dpi worker failed: {join}");
                            let _ = control
                                .send_text(&host_knob_failure("mouse_dpi", dpi, "worker join error"))
                                .await;
                        }
                    }
                }
                // #90: `dd,<dpi>` — the host's UI scaling (96 = 100 %).
                Action::SetUiScale(dpi) => {
                    log::info!("client sets host ui scale -> {dpi} dpi");
                    match tokio::task::spawn_blocking(move || hostui::apply_scale(dpi)).await {
                        Ok(Ok(applied)) => {
                            log::info!(
                                "host ui scale {} -> {} (strategy={}, exact={})",
                                applied.requested,
                                applied.applied,
                                applied.strategy,
                                applied.exact
                            );
                            let _ = control.send_text(&ui_scale_report(&applied)).await;
                            // The scale moves what the desktop draws, not the
                            // framebuffer, so the settings payload is unchanged;
                            // re-announcing it keeps the panel's copy honest for
                            // any client that only listens for that.
                            let _ = control.send_text(server_settings(&settings)).await;
                        }
                        Ok(Err(reason)) => {
                            log::warn!("host ui scale refused: {reason}");
                            let _ = control
                                .send_text(&host_knob_failure("ui_scale", dpi, &reason))
                                .await;
                        }
                        Err(join) => {
                            log::warn!("host ui scale worker failed: {join}");
                            let _ = control
                                .send_text(&host_knob_failure("ui_scale", dpi, "worker join error"))
                                .await;
                        }
                    }
                }
                // #56: `vb,<N>` client bitrate request; the capture loop
                // consumes it next beat (0 = restore automatic).
                Action::SetBitrate(kbps) => {
                    log::info!("client sets bitrate -> {kbps}kbps");
                    pipeline_handle.set_bitrate_override(kbps);
                }
                // #92: the panel's rate-control pair. Both only write the shared
                // settings; the capture loop notices the change on its next beat
                // and rebuilds the encoder (libvpx fixes `rc_end_usage` at init).
                Action::SetRateControl(mode) => match crate::pipeline::settings::RateControl::from_wire(&mode) {
                    Some(rc) => {
                        log::info!("client sets rate control -> {}", rc.as_str());
                        settings.set_rate_control(rc);
                    }
                    None => log::warn!("ignoring unknown rate control mode {mode:?}"),
                },
                Action::SetCrf(crf) => {
                    log::info!("client sets crf -> {crf}");
                    settings.set_crf(crf);
                }
                // #80: sound on/off and its encoder bitrate. Process-wide
                // statics rather than session state: the audio thread outlives
                // the select loop and only reads them once per frame.
                Action::SetAudioOn(on) => {
                    log::info!("client sets audio -> {on}");
                    crate::pipeline::audio::set_enabled(on);
                }
                Action::SetAudioBitrate(bps) => {
                    log::info!("client sets audio bitrate -> {bps} bps");
                    crate::pipeline::audio::set_bitrate_bps(bps as i32);
                }
                // #57: pointer-rendering handover. `true` (pointer lock):
                // frontend hides its pointer, server composites the OS cursor
                // (C1). `false`: frontend renders its native cursor, server
                // must not composite (single-cursor contract).
                Action::SetNativeCursorRendering(on) => {
                    pipeline_handle.set_cursor_inject(on);
                }
                Action::Ignore(who) => {
                    log::debug!("B-group command ignored in stage1: {who}");
                }
                Action::InputStub(raw) => {
                    // #54: real injection. The classifier routes every C-group
                    // line here; WinInputSink parses and injects via Win32.
                    input.handle_line(&raw);
                }
                // #55: Ctrl/Cmd+C in the browser — read the host clipboard and
                // push the `clipboardContentUpdate` JSON on the control DC.
                Action::ClipboardRequest => {
                    let messages = read_clipboard_update();
                    log::info!(
                        "REQUEST_CLIPBOARD: answered with {} message(s), {} bytes of JSON",
                        messages.len(),
                        messages.iter().map(|m| m.len()).sum::<usize>()
                    );
                    for payload in messages {
                        if let Err(err) = control.send_text(&payload).await {
                            log::debug!("clipboard reply skipped: {err:#}");
                            break;
                        }
                    }
                }
                // #55: browser Ctrl/Cmd+V uplink — reassemble and write the
                // host clipboard (text flavours; binary logged/dropped).
                Action::ClipboardChunk(raw) => {
                    clipboard.handle_line(&raw);
                }
                Action::Unknown(raw) => {
                    log::debug!("unclassified control command: {raw}");
                }
            }
        })
    }));

    log::info!(
        "session connected: command hook armed (control + pointer motion); notices wait for control open (cfg {}x{}@{}fps)",
        cfg.width,
        cfg.height,
        cfg.fps
    );
    Ok(())
}

/// #62: re-announce the state that the one-shot `on_open` notices carried, for
/// the case where the PC reaches Connected a second time (transient ICE
/// `Disconnected` -> `Connected`). `on_open` only fires on the first open, so
/// the panel kept stale capture / settings values across a re-connect.
async fn resend_notices(media: &Arc<Mutex<MediaSession>>, settings: &StreamSettings) {
    let control = media.lock().await.channels().control().clone();
    if control.ready_state() != RTCDataChannelState::Open {
        log::debug!("re-connect notices skipped: control channel not open");
        return;
    }
    let notices = [
        stream_info(&StreamInfo::default()),
        server_settings(settings),
        connection(false),
    ];
    for line in notices {
        if let Err(err) = control.send_text(&line).await {
            log::debug!("re-connect notice skipped: {err:#}");
            return;
        }
    }
    log::info!("re-connected: stream_info / server_settings / connection re-sent");
}

/// Round a display dimension down to an even number (#89).
///
/// The VP8 encoder wants even rows to avoid padding the last chroma line, and a
/// browser viewport is routinely an odd number of CSS pixels. Two pixels are
/// invisible; an odd buffer is a visible seam on some drivers.
fn even_side(v: u32) -> u32 {
    v.max(2) & !1
}

/// `display_resolution` ack (#89): what the host is at now, and whether it is
/// exactly what the browser asked for. The frontend uses `exact` to decide
/// between "1:1" and "1:1 at the nearest the driver offers".
fn display_mode_report(a: &display::Applied) -> String {
    hbb_common::serde_json::json!({
        "type": "display_resolution",
        "requested": format!("{}x{}", a.requested.0, a.requested.1),
        "applied": format!("{}x{}", a.applied.0, a.applied.1),
        "width": a.applied.0,
        "height": a.applied.1,
        "exact": a.exact,
        "strategy": a.strategy,
    })
    .to_string()
}

/// Failure counterpart of `display_mode_report`; the host desktop is unchanged.
fn display_mode_failure(w: u32, h: u32, reason: &str) -> String {
    hbb_common::serde_json::json!({
        "type": "display_resolution",
        "requested": format!("{w}x{h}"),
        "applied": hbb_common::serde_json::Value::Null,
        "exact": false,
        "error": reason,
    })
    .to_string()
}

/// #90: what the host pointer ended up at. `exact: false` means Windows has no
/// step for the requested DPI and the nearest one is in force — the panel says
/// so under the slider rather than pretending the typed number took.
///
/// #94: `strategy` says which write landed it. `broadcast-failed` is the one
/// worth reading in a bug report: the speed moved, and only the WM_SETTINGCHANGE
/// broadcast was refused (a hung or elevated top-level window vetoes it). The
/// panel ignores the field; the log and `docs/host-knobs.md` do not.
fn mouse_dpi_report(a: &hostui::AppliedMouse) -> String {
    hbb_common::serde_json::json!({
        "type": "mouse_dpi",
        "requested": a.requested,
        "applied": a.applied,
        "speed": a.speed,
        "exact": a.exact,
        "strategy": a.strategy,
    })
    .to_string()
}

/// #90: what the host's UI scaling ended up at, and which `uiParam` reading of
/// `SPI_SETLOGICALDPIOVERRIDE` this build accepted (`absolute` / `relative` /
/// `already`). The strategy is in the reply so a wrong guess is visible in the
/// log instead of being absorbed into a scale nobody can explain.
fn ui_scale_report(a: &hostui::AppliedScale) -> String {
    hbb_common::serde_json::json!({
        "type": "ui_scale",
        "requested": a.requested,
        "applied": a.applied,
        "strategy": a.strategy,
        "exact": a.exact,
    })
    .to_string()
}

/// #90: a refused host knob, shaped like `display_mode_failure` so the panel can
/// use one handler for both.
fn host_knob_failure(kind: &str, requested: u32, reason: &str) -> String {
    hbb_common::serde_json::json!({
        "type": kind,
        "requested": requested,
        "applied": hbb_common::serde_json::Value::Null,
        "exact": false,
        "error": reason,
    })
    .to_string()
}

/// #94: did the remote answer accept the video codec we offered?
///
/// `None` means yes. `Some(reason)` means the answer carried no usable video
/// m-line for this codec — either the m-section was rejected (port 0) or its
/// `a=rtpmap` list does not contain the codec. Only the negotiated list is
/// inspected: the offer always names every codec the media engine knows, so
/// "it appears in the answer" is the only statement that means anything.
///
/// Kept as a plain SDP scan rather than a parsed description because the only
/// question is a string membership test inside the video section; a full parse
/// would be a second SDP model to keep in step with the one webrtc-rs owns.
fn refused_video_codec(answer: &str, codec: crate::pipeline::codec::VideoCodec) -> Option<String> {
    let mut in_video = false;
    let mut rejected = false;
    let mut seen: Vec<String> = Vec::new();
    for raw in answer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if in_video {
                break;
            }
            if rest.starts_with("video ") {
                in_video = true;
                // `m=video <port> <proto> <fmt list>`; a rejected m-line is the
                // port-0 form, which browsers send instead of dropping the line
                // so the m-line indices stay stable.
                rejected = rest.split_whitespace().nth(1) == Some("0");
            }
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // `a=rtpmap:98 VP9/90000`
            if let Some(name) = rest.split_whitespace().nth(1) {
                if let Some((encoding, _rate)) = name.split_once('/') {
                    seen.push(encoding.to_ascii_uppercase());
                }
            }
        }
    }
    if !in_video {
        return Some(String::from("the answer has no video section"));
    }
    if rejected {
        return Some(String::from("the video m-line was rejected (port 0)"));
    }
    if !seen.iter().any(|c| c == codec.label()) {
        return Some(format!(
            "the answer offers {} but not {}",
            seen.join("/"),
            codec.label()
        ));
    }
    None
}

/// Apply the `SETTINGS,{json}` batch to the live `StreamSettings` (#50).
/// Only the keys the stage-1 server can honor are consumed: `framerate`,
/// `audio_bitrate`, `manual_resolution`/`manual_width`/`manual_height`, and
/// (#92) the `rate_control_mode` + `video_crf` pair. Everything else in the
/// batch belongs to a capability this server does not have.
///
/// #94: `encoder` is the one key here that does **not** land in
/// `StreamSettings`. The codec is not a stream parameter — it is the m-line and
/// the libvpx context, both fixed before a frame is encoded — so the pick is
/// recorded in `pipeline::codec` and the caller restarts the session when it
/// changes. The returned `Some(codec)` is that signal; `None` means nothing that
/// needs a reconnect moved.
fn apply_settings_batch(settings: &StreamSettings, payload: &str) -> Option<crate::pipeline::codec::VideoCodec> {
    let value: Result<hbb_common::serde_json::Value, _> = hbb_common::serde_json::from_str(payload);
    let Ok(value) = value else {
        log::debug!("SETTINGS payload is not JSON: {payload}");
        return None;
    };
    let Some(map) = value.as_object() else {
        log::debug!("SETTINGS payload is not an object: {payload}");
        return None;
    };

    let as_u32 = |v: &hbb_common::serde_json::Value| -> Option<u32> {
        v.as_u64().map(|n| n as u32).or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };

    let mut codec_changed = None;

    // #94: the encoder row. `vp8enc` / `vp9enc` are the panel's ids; anything
    // else (h264enc, av1enc, or a typo) is a codec this build cannot encode, and
    // accepting it would leave the menu claiming a stream that does not exist.
    if let Some(id) = map.get("encoder").and_then(|v| v.as_str()) {
        match crate::pipeline::codec::VideoCodec::from_encoder_id(id) {
            Some(pick) => {
                let previous = crate::pipeline::codec::current();
                if crate::pipeline::codec::set(pick) {
                    log::info!("SETTINGS: video codec {} -> {}", previous.wire(), pick.wire());
                    codec_changed = Some(pick);
                }
            }
            None => log::warn!(
                "SETTINGS: encoder {id:?} is not one this build can encode (have: vp8enc, vp9enc)"
            ),
        }
    }

    if let Some(fps) = map.get("framerate").and_then(as_u32) {
        settings.set_fps(fps);
        log::info!("SETTINGS: framerate -> {fps}");
    }
    // #91: the audio slider's pick. `ab,<bps>` carries the same number on the
    // live path, but a pick also rides this batch (`picked`), and a batch that
    // dropped it would leave the two channels disagreeing about the target.
    // 0 means "back to the built-in default", which `set_bitrate_bps` encodes.
    if let Some(bps) = map.get("audio_bitrate").and_then(as_u32) {
        crate::pipeline::audio::set_bitrate_bps(bps as i32);
        log::info!(
            "SETTINGS: audio bitrate -> {bps} bps (applied {})",
            crate::pipeline::audio::current_bitrate_bps()
        );
    }
    // #92: the panel's CRF pair rides the batch too. `_rc,` / `_crf,` carry the
    // same picks on the live path, but a batch that dropped them would leave the
    // two channels disagreeing — the same trap `audio_bitrate` had in #91.
    if let Some(mode) = map.get("rate_control_mode").and_then(|v| v.as_str()) {
        match crate::pipeline::settings::RateControl::from_wire(mode) {
            Some(rc) => {
                settings.set_rate_control(rc);
                log::info!("SETTINGS: rate control -> {}", rc.as_str());
            }
            None => log::warn!("SETTINGS: unknown rate control mode {mode:?}"),
        }
    }
    if let Some(crf) = map.get("video_crf").and_then(as_u32) {
        settings.set_crf(crf);
        log::info!("SETTINGS: crf -> {}", settings.crf());
    }
    // manual_resolution=true switches the frontend to fixed dimensions; the
    // width/height pair is honored together when both are present.
    if let Some(true) = map.get("manual_resolution").and_then(|v| v.as_bool()) {
        if let (Some(w), Some(h)) = (
            map.get("manual_width").and_then(as_u32),
            map.get("manual_height").and_then(as_u32),
        ) {
            settings.set_resolution(w, h);
            log::info!("SETTINGS: manual resolution -> {w}x{h}");
        }
    }
    codec_changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #88 的核心断言：控制通道上的**每一条**指令都要有面，或明确豁免。
    /// 新增一个 `Action` 变体时这条会失败 —— 那正是想要的提醒，因为漏映射
    /// 等于那条指令永远绕过策略，而且是静默绕过。
    #[test]
    fn every_action_maps_to_a_surface_or_is_exempt() {
        let actions = [
            Action::StartVideo,
            Action::StopVideo,
            Action::StatsOn,
            Action::StatsOff,
            Action::CompressedNegotiate,
            Action::PongReply("0".to_owned()),
            Action::Ignore("s".to_owned()),
            Action::InputStub("kd,65".to_owned()),
            Action::SetFps(30),
            Action::SetResolution(1280, 720),
            Action::SetDisplayResolution(1280, 720),
            Action::ResetDisplayResolution,
            Action::SetMouseDpi(800),
            Action::SetUiScale(144),
            Action::SetSettings("{}".to_owned()),
            Action::ClipboardRequest,
            Action::ClipboardChunk("cw,1,2,3,abcd".to_owned()),
            Action::SetBitrate(2500),
            Action::SetRateControl("crf".to_owned()),
            Action::SetCrf(25),
            Action::SetAudioOn(true),
            Action::SetAudioBitrate(128_000),
            Action::SetNativeCursorRendering(true),
            Action::Unknown("bogus".to_owned()),
        ];

        for action in actions {
            let exempt = matches!(
                action,
                Action::PongReply(_) | Action::Ignore(_) | Action::Unknown(_)
            );
            let surface = surface_of(&action);
            if exempt {
                assert!(
                    surface.is_none(),
                    "{action:?} is exempt but still maps to a surface"
                );
            } else {
                assert!(surface.is_some(), "{action:?} has no surface — a policy cannot see it");
            }
        }
    }

    /// 豁免的三个不能顺手扩大：`PongReply` 是服务端自己 ping 的回执，
    /// 拒绝它只会让 RTT 统计失真，挡不住任何人。
    #[test]
    fn exempt_actions_are_exactly_the_three() {
        assert!(surface_of(&Action::PongReply(String::new())).is_none());
        assert!(surface_of(&Action::Ignore("s".to_owned())).is_none());
        assert!(surface_of(&Action::Unknown("?".to_owned())).is_none());
    }

    /// 输入行到设备的归类决定"只允许键鼠"这类策略的实际边界。
    #[test]
    fn input_lines_classify_by_device() {
        assert_eq!(device_of("kd,65"), Device::Keyboard);
        assert_eq!(device_of("ku,65"), Device::Keyboard);
        assert_eq!(device_of("kh,1,65"), Device::Keyboard);
        assert_eq!(device_of("kr"), Device::Keyboard);
        assert_eq!(device_of("m,0.5,0.5"), Device::Pointer);
        assert_eq!(device_of("m2,1,-2"), Device::Pointer);
        assert_eq!(device_of("p,left,1"), Device::Pointer);
        assert_eq!(device_of("sf,1,2,3"), Device::Touch);
        assert_eq!(device_of("js,0,1"), Device::Gamepad);
        // `sfe`（焦点）与 `vp`（视口）既不按键也不移动指针，归 Other 才能让
        // 策略单独拒绝它们。
        assert_eq!(device_of("sfe,1"), Device::Other);
        assert_eq!(device_of("vp,1280,720"), Device::Other);
    }

    /// 剪贴板两个方向必须分开：读主机剪贴板（`REQUEST_CLIPBOARD`）和写主机
    /// 剪贴板（`cb*`）在权限模型里从来不是一回事。
    #[test]
    fn clipboard_read_and_write_are_distinct() {
        assert_eq!(
            surface_of(&Action::ClipboardRequest),
            Some(EventKind::Clipboard(ClipOp::Read))
        );
        assert_eq!(
            surface_of(&Action::ClipboardChunk("cb,1,2,3,abcd".to_owned())),
            Some(EventKind::Clipboard(ClipOp::Write))
        );
    }
    /// #94: a codec pick only means something if the answer takes it. The
    /// three shapes below are the ones a browser actually sends: accepted,
    /// rejected as a whole m-line (port 0), and accepted-but-without-our-codec.
    #[test]
    fn the_answer_decides_whether_the_codec_pick_survives() {
        use crate::pipeline::codec::VideoCodec;

        let accepted = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98\r\na=rtpmap:98 VP9/90000\r\n";
        assert_eq!(refused_video_codec(accepted, VideoCodec::Vp9), None);

        // The browser keeps the m-line and zeroes its port rather than dropping
        // it, so the offer's m-line indices stay stable.
        let rejected = "v=0\r\nm=video 0 UDP/TLS/RTP/SAVPF 98\r\na=rtpmap:98 VP9/90000\r\n";
        let reason = refused_video_codec(rejected, VideoCodec::Vp9).expect("port 0 is a refusal");
        assert!(reason.contains("port 0"), "unexpected reason: {reason}");

        // The audio section must not be mistaken for the video one: an Opus
        // rtpmap is not a VP8 answer.
        let only_audio = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";
        assert!(refused_video_codec(only_audio, VideoCodec::Vp8).is_some());

        let wrong_codec = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 VP8/90000\r\n";
        let reason = refused_video_codec(wrong_codec, VideoCodec::Vp9)
            .expect("VP8 in the answer is not a VP9 answer");
        assert!(reason.contains("VP8"), "the reason names what arrived: {reason}");
        assert_eq!(refused_video_codec(wrong_codec, VideoCodec::Vp8), None);
    }
}
