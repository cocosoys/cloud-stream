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
    classify, connection, cursor, pipeline, server_settings, stream_info, stream_stats, system,
    Action, ClipboardSink, InputSinkRef, StreamInfo, StreamStats, WinInputSink,
    read_clipboard_update,
};
use crate::pipeline::{spawn_loop, PipelineEvent, PipelineHandle, StreamSettings};
use crate::platform::{HostSample, Sampler};
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

/// Shared registry: the newest session holds the handle that evicts the older
/// one. Stage1 single-session scope; a second browser tab evicts the first.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<Option<mpsc::UnboundedSender<CloseNotice>>>>,
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
}

impl SessionCtx {
    fn new(
        cfg: Arc<Config>,
        ws_tx: mpsc::UnboundedSender<Message>,
        done_tx: mpsc::UnboundedSender<()>,
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
pub async fn run<S>(ws: WebSocketStream<S>, cfg: Arc<Config>, registry: SessionRegistry) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (done_tx, done_rx) = mpsc::unbounded_channel::<()>();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel::<CloseNotice>();
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<Message>();
    registry.register(notice_tx).await;

    let mut ctx = SessionCtx::new(cfg, ws_tx, done_tx);
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
    pointer.on_message(Box::new(move |msg| {
        let input = pointer_input.clone();
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
            match classify(&line) {
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
        Box::pin(async move {
            if !msg.is_string {
                log::debug!("binary control message ignored in stage1");
                return;
            }
            let line = String::from_utf8_lossy(&msg.data).trim().to_owned();
            if line.is_empty() {
                return;
            }
            match classify(&line) {
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
                Action::SetSettings(payload) => {
                    apply_settings_batch(&settings, &payload);
                    let _ = control.send_text(server_settings(&settings)).await;
                }
                // #56: `vb,<N>` client bitrate request; the capture loop
                // consumes it next beat (0 = restore automatic).
                Action::SetBitrate(kbps) => {
                    log::info!("client sets bitrate -> {kbps}kbps");
                    pipeline_handle.set_bitrate_override(kbps);
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

/// Apply the `SETTINGS,{json}` batch to the live `StreamSettings` (#50).
/// Only the keys the stage-1 server can honor are consumed: `framerate`,
/// `manual_width`, `manual_height`, `manual_resolution`. Everything else
/// (keyboard layout, clipboard toggles, CRF, rate control, ...) is untouched
/// and belongs to stage 3.
fn apply_settings_batch(settings: &StreamSettings, payload: &str) {
    let value: Result<hbb_common::serde_json::Value, _> = hbb_common::serde_json::from_str(payload);
    let Ok(value) = value else {
        log::debug!("SETTINGS payload is not JSON: {payload}");
        return;
    };
    let Some(map) = value.as_object() else {
        log::debug!("SETTINGS payload is not an object: {payload}");
        return;
    };

    let as_u32 = |v: &hbb_common::serde_json::Value| -> Option<u32> {
        v.as_u64().map(|n| n as u32).or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };

    if let Some(fps) = map.get("framerate").and_then(as_u32) {
        settings.set_fps(fps);
        log::info!("SETTINGS: framerate -> {fps}");
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
}
