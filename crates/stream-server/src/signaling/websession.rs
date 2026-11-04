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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

use crate::config::{Config, InputPolicy};
use crate::dispatch::{
    classify, connection, cursor, display, hostui, pipeline, server_settings, stream_info,
    stream_stats, system, Action, ClipboardSink, GamepadHub, InputSinkRef, StreamInfo,
    StreamStats, WinInputSink, read_clipboard_update,
};
use crate::pipeline::{
    DisplayKey, PipelineEvent, PipelineHandle, PipelineRegistry, SharedPipeline, SinkCounters,
    SinkHandle, StreamSettings,
};
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
    /// `client_type` **as the page sent it** (`controller` / `viewer`).
    ///
    /// This is a claim, not a role. Every page that is not told otherwise calls
    /// itself `controller`, so two tabs of the same browser both say it; the
    /// server's answer lives on the program (see [`SharedPipeline::controller_id`])
    /// and is reported separately as `role` by the health endpoint.
    pub client_type: String,
    /// Which display this session attached to, once it did.
    ///
    /// `None` until `Connected`: the key comes from the session's own attach,
    /// and a connection that never got that far has no program to name. The
    /// health endpoint needs it to answer "is this id the one holding the
    /// lease on that program".
    pub display: Option<u32>,
    /// The live handles, so the health endpoint reads the pipeline rather than a
    /// copy of its numbers.
    pub pipeline: Option<PipelineHandle>,
    pub settings: Option<StreamSettings>,
    /// This session's own downlink tally (#102).
    ///
    /// One shared encoder means the *program's* byte count cannot say who spent
    /// it, so the number is kept per sink and hung here after `attach`. `None`
    /// until then — a connection that never reached `Connected` has no downlink
    /// and must not be reported as a zero-traffic viewer.
    pub sink: Option<Arc<SinkCounters>>,
    /// Input messages this session sent and the lease refused (#103).
    ///
    /// Without it, a viewer whose pointer does nothing is indistinguishable
    /// from a viewer whose messages never arrived — the exact ambiguity that
    /// makes "the second tab is unresponsive" unactionable. A non-zero counter
    /// says the input reached the server and the role gate dropped it.
    pub denied: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// How many times this browser has connected before now, keyed by
    /// `device_id`. 0 on the first connection.
    ///
    /// This is the difference between "a user" and "a page in a reload loop":
    /// a client that reconnects every few seconds shows up here as a growing
    /// count while `connected_s` stays near zero, which is the shape of every
    /// reconnect storm this project has had.
    pub reconnects: u32,
}

/// What the client told us about itself when it opened the WebSocket.
#[derive(Clone, Debug, Default)]
pub struct ClientMeta {
    pub ip: String,
    pub user_agent: String,
}

/// One live connection: the handle that can close it and what it told us about
/// itself.
struct LiveSession {
    notice: mpsc::UnboundedSender<CloseNotice>,
    info: SessionInfo,
}

/// Every live signaling session, addressable by id (#101).
///
/// It used to be a single slot: registering a session evicted the previous one
/// with close code 4001, which is what made "one browser tab" a hard invariant
/// of the whole server. That is no longer true — a second tab is a legitimate
/// *viewer* of the same pipeline, and the frontend's multi-display design is
/// literally "one page per screen", so eviction has to go.
///
/// What survives of the old rule is the part that was actually load-bearing: a
/// **reload of the same tab** leaves a zombie socket behind (the browser is
/// already gone, the server has not noticed, and the zombie keeps holding a PC
/// and a control channel). Eviction therefore keys on `client_tab_id` from the
/// hello rather than on "newest connection wins": the same tab reconnecting
/// replaces itself, a different tab is a second citizen.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    /// A tokio mutex: publishing a hello evicts another session, and eviction
    /// must not interleave with a teardown removing its own entry.
    inner: Arc<Mutex<std::collections::HashMap<u64, LiveSession>>>,
    /// Next session id; monotonic so a finishing session can never clear the
    /// entry a newer one already published under the same slot.
    next_id: Arc<std::sync::atomic::AtomicU64>,
    /// Connections seen per `device_id` (#102).
    ///
    /// A std mutex — it is touched only inside `publish`, which never awaits
    /// while holding it, and the map is a handful of entries. Kept outside the
    /// async lock so a slow hello cannot make the counter wait behind a teardown.
    seen: Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>,
}

impl SessionRegistry {
    /// Take a slot for this connection. The hello (and with it the tab id)
    /// arrives later, which is when a same-tab reconnect is detected.
    async fn register(&self, id: u64, notice: mpsc::UnboundedSender<CloseNotice>) {
        let mut guard = self.inner.lock().await;
        guard.insert(
            id,
            LiveSession {
                notice,
                info: SessionInfo {
                    id,
                    ..Default::default()
                },
            },
        );
        log::info!("session {id} registered ({} live)", guard.len());
    }

    /// #106: register a non-signaling session — the `/api/websockets` media
    /// socket. Same table as the signaling sessions so `health` reports one list
    /// of live clients instead of two half-truths.
    ///
    /// The close-notice sender is kept but never used: WS media sessions are not
    /// evicted by tab id (`kick` still reaches them through `remove`). Dropping
    /// the receiver is harmless — `notice.send` would just fail and be ignored.
    pub async fn register_media(&self, info: SessionInfo) -> u64 {
        let id = self.next_session_id();
        let (notice, _notice_rx) = mpsc::unbounded_channel::<CloseNotice>();
        let mut guard = self.inner.lock().await;
        guard.insert(
            id,
            LiveSession {
                notice,
                info: SessionInfo { id, ..info },
            },
        );
        log::info!("media session {id} registered ({} live)", guard.len());
        id
    }

    /// Update the identity fields a WS page sends in its first `SETTINGS` batch.
    pub async fn update_meta(&self, id: u64, tab_id: &str, device_id: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(live) = guard.get_mut(&id) {
            if !tab_id.is_empty() {
                live.info.tab_id = tab_id.to_owned();
            }
            if !device_id.is_empty() {
                live.info.device_id = device_id.to_owned();
            }
        }
    }

    /// Allocate the id this session publishes under.
    fn next_session_id(&self) -> u64 {
        self.next_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// Publish what the hello said, and evict a previous socket for the **same
    /// browser tab**.
    ///
    /// Returns the number of sessions evicted (0 or 1): the caller logs it, and
    /// a test pins that a second tab is not one of them.
    async fn publish(&self, mut info: SessionInfo) -> usize {
        // #102: how many times this browser has connected, *before* this one.
        // A page in a reload loop shows a count climbing while `connected_s`
        // stays near zero — the shape every reconnect storm has had here.
        if !info.device_id.is_empty() {
            let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
            let count = seen.entry(info.device_id.clone()).or_insert(0);
            info.reconnects = *count;
            *count = count.saturating_add(1);
        }
        let mut guard = self.inner.lock().await;
        let tab = info.tab_id.clone();
        let mut evicted = 0usize;
        if !tab.is_empty() {
            let stale: Vec<u64> = guard
                .iter()
                .filter(|(id, live)| {
                    **id != info.id && !live.info.tab_id.is_empty() && live.info.tab_id == tab
                })
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                if let Some(live) = guard.remove(&id) {
                    let _ = live.notice.send(CloseNotice {
                        code: 4001,
                        reason: "This tab reconnected from another socket.".to_owned(),
                    });
                    evicted += 1;
                    log::info!("session {id} evicted: tab {} reconnected", info.id);
                }
            }
        }
        let id = info.id;
        match guard.get_mut(&id) {
            Some(live) => live.info = info,
            None => {
                // A hello for a session that already tore down, or one that
                // never registered: re-registering would need a notice sender we
                // no longer have, so the entry is simply not published.
                log::debug!("hello for unregistered session {id} ignored");
            }
        }
        if evicted == 0 {
            log::info!(
                "session {id} published as '{}' from {} ({} live)",
                guard
                    .get(&id)
                    .map(|l| l.info.client_type.clone())
                    .unwrap_or_default(),
                guard.get(&id).map(|l| l.info.peer_ip.clone()).unwrap_or_default(),
                guard.len()
            );
        }
        evicted
    }

    /// Drop this session's entry on teardown.
    pub async fn remove(&self, id: u64) {
        let mut guard = self.inner.lock().await;
        if guard.remove(&id).is_some() {
            log::info!("session {id} unregistered ({} live)", guard.len());
        }
    }

    /// Hang this session's downlink counters, display key and refusal counter on
    /// its entry (#102, #103).
    ///
    /// The hello publishes identity long before `Connected` creates the sink, so
    /// none of this can travel in the same message. Called by the PC-state
    /// watcher right after `attach`; a session that tears down before then
    /// simply never gets one, and `health` reports `null` rather than a
    /// misleading zero.
    pub async fn set_downlink(
        &self,
        id: u64,
        display: DisplayKey,
        counters: Arc<SinkCounters>,
        denied: Arc<AtomicU64>,
    ) {
        let mut guard = self.inner.lock().await;
        if let Some(live) = guard.get_mut(&id) {
            live.info.sink = Some(counters);
            live.info.display = Some(display.0);
            live.info.denied = Some(denied);
        }
    }

    /// The session that should inherit the input lease when `leaving` goes away:
    /// the oldest other session attached to the same display, if any (#103).
    ///
    /// Handing the lease to nobody would leave the program injecting nothing —
    /// the survivor would keep watching a desktop it cannot touch, and no
    /// reconnect would fix it because the lease is keyed on the program, not on
    /// the socket. Oldest rather than newest so the choice does not depend on
    /// who happened to connect last.
    pub async fn successor(&self, leaving: u64, display: u32) -> Option<u64> {
        let guard = self.inner.lock().await;
        guard
            .values()
            .filter(|live| live.info.id != leaving)
            .filter(|live| live.info.display == Some(display))
            .map(|live| live.info.id)
            .min()
    }

    /// Every live session, oldest first. The health endpoint reports this list;
    /// the oldest entry is also the one whose pipeline the report describes.
    pub async fn infos(&self) -> Vec<SessionInfo> {
        let guard = self.inner.lock().await;
        let mut out: Vec<SessionInfo> = guard.values().map(|l| l.info.clone()).collect();
        out.sort_by_key(|i| i.id);
        out
    }

    /// `POST /api/v1/control/session/kick` — close every session (`id == None`)
    /// or one specific one (#101). Returns how many were asked to close.
    ///
    /// The id form exists so a stuck viewer can be dropped without taking the
    /// whole stream down: with N sessions, "kick" can no longer mean "kick the
    /// only one".
    pub async fn kick(&self, reason: &str, id: Option<u64>) -> usize {
        let mut guard = self.inner.lock().await;
        let targets: Vec<u64> = match id {
            Some(want) if guard.contains_key(&want) => vec![want],
            Some(_) => Vec::new(),
            None => guard.keys().copied().collect(),
        };
        let n = targets.len();
        for want in targets {
            if let Some(live) = guard.remove(&want) {
                let _ = live.notice.send(CloseNotice {
                    code: 4002,
                    reason: reason.to_owned(),
                });
            }
        }
        n
    }
}

/// How many signaling sessions this process will hold at once (#102).
///
/// Before this there was no limit at all: `handle_inner` upgraded every
/// WebSocket it saw. That is the wrong default for a host that can encode one
/// stream — every extra sink costs a packetizer pass and a full copy of the
/// bitrate, so an unbounded fan-out does not "serve more people", it degrades
/// the picture for the people already watching. The cap is the difference
/// between a late viewer being told "try again" and a room where nothing works.
///
/// A counter plus a guard rather than a `Semaphore`, because the slot must be
/// taken **before** the upgrade (so a rejected client never gets a 101) and
/// released when the session task ends — including the paths where it unwinds.
/// `Drop` covers all of them and needs no `async`.
#[derive(Clone)]
pub struct Admission {
    max: usize,
    live: Arc<std::sync::atomic::AtomicUsize>,
}

impl Admission {
    pub fn new(max: usize) -> Self {
        Self {
            // 0 would lock the door for everybody, including the operator who
            // typed it; 1 is the only sane reading of "no more than zero".
            max: max.max(1),
            live: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Take one of the `max` slots, or `None` when they are all held.
    ///
    /// Compare-exchange rather than load-then-store: two upgrades arriving in
    /// the same tick would otherwise both read `max - 1` and both get in, which
    /// is exactly the case a limit exists for.
    pub fn try_enter(&self) -> Option<AdmissionPermit> {
        loop {
            let now = self.live.load(Ordering::SeqCst);
            if now >= self.max {
                return None;
            }
            if self
                .live
                .compare_exchange(now, now + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(AdmissionPermit {
                    live: self.live.clone(),
                });
            }
        }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    /// Slots in use right now. Reported by `/api/v1/health`; not the same as
    /// the session count, because a slot is taken before the hello arrives.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

/// One held slot; dropping it is the release.
pub struct AdmissionPermit {
    live: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
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

/// This session's hold on a shared pipeline (#101), filled at `Connected`.
///
/// A cell rather than four fields because two tasks own different halves of it:
/// the PC-state watcher attaches (it is the one that sees `Connected`), while
/// the WS task detaches on teardown, and the control hook reads the handle. None
/// of them can hand the others a `&mut`, and a snapshot taken at construction
/// would leave whoever read it first driving a dead placeholder.
struct Attachment {
    /// The placeholder until `Connected`; the shared program's handle after.
    handle: PipelineHandle,
    shared: Option<Arc<SharedPipeline>>,
    /// This session's downlink slot in the program's fan-out.
    sink: Option<SinkHandle>,
    /// #108: this session's audio slot in the program's shared downlink.
    audio: Option<crate::pipeline::audio::AudioSinkId>,
    /// Input messages the role gate refused, published to the registry so the
    /// health endpoint can tell "refused" from "never arrived" (#103).
    denied: Arc<AtomicU64>,
}

impl Default for Attachment {
    fn default() -> Self {
        Self {
            handle: PipelineHandle::new(),
            shared: None,
            sink: None,
            audio: None,
            denied: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Per-connection context owned by the WS task.
struct SessionCtx {
    cfg: Arc<Config>,
    session: SignalingSession,
    /// The pipeline this session is attached to (#101); see [`Attachment`].
    attach: Arc<std::sync::Mutex<Attachment>>,
    /// #109: which display this session watches. Comes from the hello's
    /// `display_id` (`primary` / `display2`), before `Connected` decides which
    /// program to join. A session that never says anything gets the primary.
    display: DisplayKey,
    /// #62: the worker-termination sender, registered with the shared handle at
    /// `Connected` (the handle does not exist before that).
    exit_tx: Option<mpsc::UnboundedSender<String>>,
    /// #50: live fps / resolution targets, mutated by `_arg_fps` / `r,WxH` /
    /// `SETTINGS` and re-read by the capture worker every beat.
    settings: StreamSettings,
    input: InputSinkRef,
    /// #17: the session's virtual gamepad bus. Session-scoped rather than
    /// process-wide: the pads are OS devices that must disappear with the page
    /// they belong to, and the rumble pump is per control channel.
    gamepad: Arc<GamepadHub>,
    /// #55: Windows clipboard bridge for REQUEST_CLIPBOARD / cw-cwe uplink.
    clipboard: Arc<ClipboardSink>,
    stats_on: Arc<AtomicBool>,
    /// #78: control-channel round-trip probe. Written by the ping task and the
    /// control-channel hook, read by the stats timer for `rtt_ms`.
    latency: Arc<LatencyProbe>,
    /// #77/#108: set once this session attached its audio track, so an ICE
    /// reconnect does not attach a second sink (and a second stream) to the
    /// program's shared downlink.
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
    /// The shared programs, so this session can join the one for its display
    /// instead of opening a second capture on it (#101).
    pipelines: PipelineRegistry,
}

impl SessionCtx {
    fn new(
        cfg: Arc<Config>,
        ws_tx: mpsc::UnboundedSender<Message>,
        done_tx: mpsc::UnboundedSender<()>,
        client: ClientMeta,
        session_id: u64,
        registry: SessionRegistry,
        pipelines: PipelineRegistry,
    ) -> Self {
        // #62: the sink needs the live frame size so it can map the frontend's
        // frame-space pointer coordinates onto the physical screen.
        let settings = StreamSettings::from_config(&cfg);
        // #17: the gamepad bus loads ViGEmClient.dll and connects to ViGEmBus on
        // its own thread, so a machine without the driver connects normally and
        // simply has no virtual pads. The reason lands in the log once.
        let gamepad = GamepadHub::spawn();
        let input: InputSinkRef =
            Arc::new(WinInputSink::with_gamepad(settings.clone(), Some(gamepad.clone())));
        // #80: the audio switches are process-wide (the audio thread is not tied
        // to the select loop), so a *reload* has to reset them — otherwise a
        // reload while muted comes back silent with the panel showing "Sound
        // on". #101: they moved out of here, because a second tab is not a
        // reload: resetting them from the viewer's constructor would unmute the
        // controller's stream. The reset now happens where a program is created
        // (see `Connected` below), which only the controller does.
        Self {
            session: SignalingSession::new(),
            attach: Arc::new(std::sync::Mutex::new(Attachment::default())),
            // #109: the server's `--display` is the default, not a hard
            // override — a session that names a screen in its hello moves off
            // this value. Starting here rather than at `PRIMARY` keeps a
            // single-monitor box on `--display 0` and a wall-display box on
            // whatever it was launched with, without the page having to know.
            display: DisplayKey(cfg.display),
            exit_tx: None,
            settings,
            input,
            gamepad,
            clipboard: Arc::new(ClipboardSink::default()),
            stats_on: Arc::new(AtomicBool::new(false)),
            latency: Arc::new(LatencyProbe::new()),
            audio_started: false,
            media: None,
            tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            cfg,
            ws_tx,
            done_tx,
            client,
            session_id,
            registry,
            pipelines,
        }
    }

    /// Whether this session currently owns input and the program's settings.
    ///
    /// Read through the program rather than from a field of our own (#103): the
    /// lease moves — an operator takeover hands it to another session — and a
    /// cached boolean would leave the old holder injecting for as long as it
    /// lives. Everything that gates on the role calls this per message, so the
    /// switch is atomic from both sides' point of view.
    fn is_controller(&self) -> bool {
        let shared = self
            .attach
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shared
            .clone();
        shared
            .map(|program| program.controller_id() == Some(self.session_id))
            .unwrap_or(false)
    }

    /// The handle the data-channel hooks use to ask, per message, whether this
    /// session still holds the lease.
    ///
    /// Built by the state watcher (which is the task that attaches), not here:
    /// the watcher owns the `Attachment` cell for the duration of `Connected`,
    /// and a second lock round-trip would only add a window where the gate is
    /// armed before the display key is known.
    #[allow(dead_code)]
    fn control_gate(&self) -> Option<ControlGate> {
        let att = self.attach.lock().unwrap_or_else(|p| p.into_inner());
        let program = att.shared.clone()?;
        Some(ControlGate {
            program,
            session: self.session_id,
            denied: att.denied.clone(),
        })
    }
}

/// A session's claim on a program's input lease, checked per message (#103).
///
/// Passed to the pointer/control hooks instead of a `bool` so that a takeover
/// takes effect on the *next* message rather than on the next reconnect.
#[derive(Clone)]
struct ControlGate {
    program: Arc<SharedPipeline>,
    session: u64,
    /// Shared with the registry entry, so a refusal is visible in `health`
    /// (see `SessionInfo::denied`).
    denied: Arc<AtomicU64>,
}

impl ControlGate {
    /// Whether this session may act on the program right now.
    ///
    /// #105: the answer is no longer only "do I hold the lease". Under the
    /// default [`InputPolicy::Shared`] every attached session may inject, which
    /// is what upstream selkies does and what the single-operator deployments
    /// actually need — two tabs of the same page are one person, and refusing
    /// the second one looked indistinguishable from "input is broken" (the
    /// server was counting hundreds of refused pointer moves a minute). The
    /// exclusive lease is still there for the multi-user case, where it will be
    /// paired with real isolation.
    fn owns(&self) -> bool {
        match self.program.input_policy() {
            InputPolicy::Shared => true,
            InputPolicy::Exclusive => self.program.controller_id() == Some(self.session),
        }
    }

    /// Count a message the lease refused. Called on every dropped input, so it
    /// is a relaxed add on a shared cache line — cheap enough for the pointer
    /// channel's rate and the only way the count stays truthful.
    fn refuse(&self) {
        self.denied.fetch_add(1, Ordering::Relaxed);
    }
}

/// Run one upgraded signaling socket to its terminal state.
pub async fn run<S>(
    ws: WebSocketStream<S>,
    cfg: Arc<Config>,
    registry: SessionRegistry,
    pipelines: PipelineRegistry,
    client: ClientMeta,
    permit: AdmissionPermit,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // #102: the slot is held for exactly as long as this session lives. Bound
    // to a local rather than dropped here: the guard releases on return, and
    // every exit path (clean close, ICE failure, eviction, panic) goes through
    // the same `Drop`.
    let _permit = permit;
    let (done_tx, done_rx) = mpsc::unbounded_channel::<()>();
    let (notice_tx, notice_rx) = mpsc::unbounded_channel::<CloseNotice>();
    let (ws_tx, ws_rx) = mpsc::unbounded_channel::<Message>();

    let session_id = registry.next_session_id();
    registry.register(session_id, notice_tx).await;

    let mut ctx = SessionCtx::new(cfg, ws_tx, done_tx, client, session_id, registry, pipelines);
    let mut ws = ws;

    let outcome = session_loop(&mut ws, &mut ctx, notice_rx, done_rx, ws_rx).await;

    // Cleanup: PC, channels, ICE sender, timers. The media handle lives only in
    // the WS task, so closing here owns both paths the state callback could have
    // parked.
    // #62: release everything the browser left pressed *before* the PC goes
    // away — a vanished tab used to leave WASD held on the host until someone
    // physically touched the keyboard.
    ctx.input.reset();
    // #17: a closed tab never sends `js,d`, and a virtual pad is an OS device
    // that outlives the page. Without this the next connection finds a ghost
    // "Xbox 360 Controller" holding XInput slot 0 and the game reads nothing.
    // The unplug itself happens on the gamepad thread; `Drop` joins it.
    ctx.gamepad.release_all();
    // #17: `release_all` only clears `want`; the thread and the bus client end
    // in `Drop`, and `Drop` waits for the *last* `Arc<GamepadHub>` — which the
    // data-channel closures also hold (they close over the input sink, and the
    // sink owns one). A teardown path that leaves a DC behind therefore leaves
    // the pad thread and the ViGEm client running until that clone goes.
    // Measured on the target host: four `connected` lines against two
    // `bus disconnected`, and one extra thread per reload. Stopping here makes
    // the release unconditional — a lingering `Arc` afterwards is an idle
    // struct, not a thread with a driver connection.
    ctx.gamepad.shutdown();
    // #96: a game-mode pin moves the *host's* pointer settings, so it outlives
    // the page that asked for it unless the release runs here. A tab closed
    // while locked away from pointer lock would otherwise leave the desktop at
    // 1:1 with acceleration off — a change nobody made and nobody can see. A
    // no-op when nothing is pinned.
    //
    // #101: only the controller may undo it. A viewer leaving must not revert a
    // pin the person in the other tab is still using.
    if ctx.is_controller() {
        if let Err(reason) = hostui::unpin_pointer() {
            log::warn!("host pointer not restored on session teardown: {reason}");
        }
    }
    if let Some(media) = ctx.media.take() {
        let _ = media.lock().await.close().await;
    }
    // #101: leave the shared program. Detach before release so the fan-out stops
    // writing into a track whose peer connection is already gone, and release so
    // the last session out is the one that stops the capture — a viewer leaving
    // must not kill the controller's stream.
    //
    // The guard is dropped before the await below: a std mutex held across one
    // would serialise the watcher's attach against the teardown of another
    // session on the same thread pool.
    let (shared, sink, audio) = {
        let mut att = ctx.attach.lock().unwrap_or_else(|p| p.into_inner());
        (att.shared.take(), att.sink.take(), att.audio.take())
    };
    match (shared, sink) {
        (Some(shared), Some(sink)) => {
            // #102: read the tally before detaching — the handle keeps working
            // after the fan-out entry is gone, but this is the last moment the
            // numbers are certainly final (no write can be in flight once the
            // session's own tasks have stopped).
            let (bytes, frames) = (sink.bytes(), sink.frames());
            // #103: if the lease is ours, it must not die with us — the other
            // tab on this program would inherit a desktop nobody can touch, and
            // no reconnect would fix that (the lease lives on the program, not
            // on a socket). Handed over *before* `release`: when we are the last
            // one out the program is about to be dropped and `None` is right.
            let held_lease = shared.controller_id() == Some(ctx.session_id);
            let display = shared.display.0;
            // #108: leave the audio downlink before the video one. A track left
            // attached would keep receiving packets for a peer connection that
            // is about to close.
            if let Some(audio) = audio {
                shared.audio.detach(audio);
            }
            let left = shared.detach(sink.id());
            log::info!(
                "pipeline {}: session left, {} downlink(s) remain (sent {frames} frames / {:.1} MiB)",
                display,
                left,
                bytes as f64 / (1024.0 * 1024.0)
            );
            if held_lease {
                let next = ctx.registry.successor(ctx.session_id, display).await;
                if let Some(next) = next {
                    log::info!(
                        "pipeline {display}: input lease handed to session {next} (holder left)"
                    );
                }
                shared.set_controller(next);
            }
            ctx.pipelines.release(shared.display).await;
        }
        // Never reached Connected: nothing was ever attached.
        _ => {}
    }
    // #93/#101: the health endpoint drops this entry, and a same-tab reconnect
    // becomes possible again.
    ctx.registry.remove(ctx.session_id).await;
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
            // #109: `display_id` picks the program this session joins. The
            // page sends `primary` / `display2` / `display3`… (`signaling.js`
            // builds it from the selected thumbnail); anything else — an old
            // page, a typo, an out-of-range index — stays on the primary
            // rather than failing the handshake, because a wrong screen is
            // recoverable by reloading and a refused hello is not.
            ctx.display = DisplayKey::from_hello(&read("display_id"));
            // Keep the session's own settings in step with the answer: the
            // downstream `stream_info` / `server_settings` builders read
            // `display()` off this object, and a copy still saying "primary"
            // would report the wrong screen for the whole session.
            ctx.settings.set_display(ctx.display.0);
            if ctx.display != DisplayKey::PRIMARY {
                log::info!("client asked for display {}", ctx.display.0);
            }
            ctx.registry
                .publish(SessionInfo {
                    id: ctx.session_id,
                    connected_at: Some(std::time::Instant::now()),
                    peer_ip: ctx.client.ip.clone(),
                    user_agent: ctx.client.user_agent.clone(),
                    device_id: read("client_device_id"),
                    tab_id: read("client_tab_id"),
                    client_type: if read("client_type").is_empty() {
                        peer_type.clone()
                    } else {
                        read("client_type")
                    },
                    // #101: the pipeline report now comes from
                    // `PipelineRegistry` (a program outlives and outnumbers
                    // sessions); a handle snapshot here would be the
                    // placeholder this session has not attached yet.
                    pipeline: None,
                    settings: Some(ctx.settings.clone()),
                    // #103: filled by `set_downlink` right after the attach —
                    // the display and the role are decided there, not here.
                    display: None,
                    denied: None,
                    // #102: the downlink does not exist yet — it is attached at
                    // `Connected`, which is when `set_sink` fills this in. The
                    // byte count therefore starts at "no downlink", not at zero
                    // traffic, and health reports `bytes_total: 0` for a session
                    // that is still negotiating.
                    sink: None,
                    // Filled by `publish` from the per-device counter.
                    reconnects: 0,
                })
                .await;
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
                    // #101: the codec is a property of the *program*, and every
                    // session shares the one encoder — so a browser that refuses
                    // it poisons all of them. Restarting every session is the
                    // only recovery that leaves them all decodable.
                    let n = ctx
                        .registry
                        .kick(
                            "video codec not supported by this browser; retrying with VP8",
                            None,
                        )
                        .await;
                    log::info!("restarted {n} session(s) for the VP8 fallback");
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
    //
    // #101: the sender is only *kept* here. The handle it belongs to is the
    // shared program's, which does not exist until `Connected`, so the actual
    // registration happens there — and `PipelineHandle` delivers to every
    // attached session, not just the one that armed it.
    let (exit_tx, mut exit_rx) = mpsc::unbounded_channel::<String>();
    ctx.exit_tx = Some(exit_tx.clone());
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
    let watcher_attach = ctx.attach.clone();
    let watcher_pipelines = ctx.pipelines.clone();
    let watcher_exit = ctx.exit_tx.clone();
    let watcher_input = ctx.input.clone();
    let watcher_clipboard = ctx.clipboard.clone();
    let mut watcher_settings = ctx.settings.clone();
    let watcher_gamepad = ctx.gamepad.clone();
    let watcher_tasks = ctx.tasks.clone();
    let watcher_registry = ctx.registry.clone();
    // #102: the watcher learns the session id so it can hang this session's
    // downlink counters on the registry entry the health endpoint reads.
    let watcher_session_id = ctx.session_id;
    // #109: the program this session will join is keyed by the display the
    // hello named; captured before the task moves.
    let watcher_display = ctx.display;
    push_task(
        &ctx.tasks,
        tokio::spawn(async move {
            let mut state_rx = state_rx;
            let mut attached = false;
            let mut pli_started = false;
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
                        if !attached {
                            attached = true;
                            // #101: join the capture/encode program for this
                            // display, creating and starting it when this is the
                            // first session. A second session must not call
                            // `spawn_loop`: `DuplicateOutput` is exclusive per
                            // output, so its capturer would fail and the recovery
                            // chain would park it on GDI while the first kept
                            // DXGI — two encoders, one of them on the slow path.
                            let acquired = watcher_pipelines.acquire(watcher_display, &cfg).await;
                            let controller = acquired.created;
                            if controller {
                                // #80: the audio switches are process-wide, so
                                // only the session that owns the program resets
                                // them. A viewer connecting is not a reload and
                                // must not unmute the controller's stream.
                                crate::pipeline::audio::set_enabled(true);
                                crate::pipeline::audio::set_bitrate_bps(0);
                                // #103: the lease lives on the program and is
                                // claimed here rather than inferred from
                                // `created` at every use site, so a takeover can
                                // move it later without touching this session's
                                // state.
                                acquired.pipeline.set_controller(Some(watcher_session_id));
                            } else {
                                // #105: a joiner only inherits an *unowned* lease.
                                // Without this, a program that outlived its
                                // creator (the usual case: a refresh, while the
                                // other tab is still attached) was joinable but
                                // permanently input-dead — every tab reported
                                // `viewer` and no reconnect could fix it,
                                // because the lease is keyed on the program and
                                // only `created` ever wrote it.
                                acquired.pipeline.try_claim_controller(watcher_session_id);
                            }
                            if let Some(tx) = watcher_exit.clone() {
                                acquired.pipeline.handle.add_exit_sink(tx);
                            }
                            // Record the hold *before* the awaits below: if this
                            // task is aborted mid-attach (session teardown aborts
                            // every per-session task), a released-but-unrecorded
                            // entry would keep the capture running with nobody
                            // watching and no way to find it.
                            {
                                let mut att = watcher_attach
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                att.handle = acquired.pipeline.handle.clone();
                                att.shared = Some(acquired.pipeline.clone());
                            }
                            let video = watcher_media.lock().await.video().clone();
                            let sink = acquired.pipeline.attach(video, !controller);
                            // #109: absolute input is relative to the frame's
                            // monitor. Without this the pointer on a secondary
                            // display lands on the primary at the same relative
                            // position — every click is off by the monitor's
                            // origin, and on a host whose secondary sits left of
                            // the primary it is off-screen entirely.
                            if let Some(geom) =
                                crate::pipeline::capture::geometry_at(acquired.pipeline.display.0)
                            {
                                watcher_input
                                    .set_target_display(geom.origin, (geom.width, geom.height));
                            }
                            // #102: keep the counter block, then hand it to the
                            // registry so `health` can report this session's own
                            // bytes. The handle stays with the session for its
                            // final tally on teardown; both point at the same
                            // atomics, so no shared ownership problem.
                            let sink_counters = sink.counters();
                            // A joiner reports the *program's* settings, not the
                            // CLI defaults it connected with: the panel must show
                            // the frame rate and size the encoder is really at.
                            watcher_settings = acquired.pipeline.settings.clone();
                            let denied = {
                                let mut att = watcher_attach
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                att.sink = Some(sink);
                                att.denied.clone()
                            };
                            watcher_registry
                                .set_downlink(
                                    watcher_session_id,
                                    acquired.pipeline.display,
                                    sink_counters,
                                    denied,
                                )
                                .await;
                            log::info!(
                                "pipeline {}: {} — {} downlink(s), {} holder(s)",
                                acquired.pipeline.display.0,
                                if controller { "program created" } else { "joined as viewer" },
                                acquired.pipeline.video.len(),
                                acquired.pipeline.refs()
                            );
                        } else {
                            // #62: a second Connected (ICE re-connect after a
                            // transient Disconnected) never re-ran `on_open`, so
                            // the panel kept stale settings / capture state.
                            resend_notices(&watcher_media, &watcher_settings).await;
                        }
                        let (handle, gate) = {
                            let att = watcher_attach
                                .lock()
                                .unwrap_or_else(|p| p.into_inner());
                            // #103: the hooks ask the program per message rather
                            // than capture a bool, so a takeover from another
                            // session takes effect immediately.
                            let gate = att.shared.clone().map(|program| ControlGate {
                                program,
                                session: watcher_session_id,
                                denied: att.denied.clone(),
                            });
                            (att.handle.clone(), gate)
                        };
                        // #101: only the first arm installs the recovery-event
                        // sink. `PipelineHandle` now *adds* subscribers instead of
                        // replacing the slot, so re-arming on an ICE reconnect
                        // would leave the previous task alive and sending the same
                        // `system` notices twice.
                        let first_arm = !pli_started;
                        if let Err(err) = arm_connected_notices(
                            &watcher_media,
                            &handle,
                            &cfg,
                            &watcher_stats_on,
                            &watcher_latency,
                            &watcher_input,
                            &watcher_clipboard,
                            &watcher_settings,
                            &watcher_gamepad,
                            &watcher_tasks,
                            &watcher_registry,
                            gate,
                            first_arm,
                        )
                        .await
                        {
                            log::warn!("Connected notice arming failed: {err:#}");
                            let _ = done_tx.send(());
                            return;
                        }
                        if !pli_started {
                            pli_started = true;
                            // #53: PLI/FIR from the browser -> force keyframes.
                            // One listener per session, all aimed at the shared
                            // encoder (its 500 ms debounce coalesces them).
                            watcher_media.lock().await.spawn_pli_listener(handle.clone());
                        }
                        // #77: the sound track needs the same `Connected` gate as
                        // video, but its own lifecycle — a second Connected must
                        // not open a second loopback capture on the same endpoint.
                        //
                        // #101: the loopback capture stays per session on purpose.
                        // WASAPI shared mode allows several clients on one
                        // endpoint, and an Opus stereo encoder is a few percent of
                        // a core — far less than the bookkeeping a shared,
                        // multi-sink audio fan-out would add here. The switches
                        // above remain process-wide, which is why only the
                        // controller resets them.
                        if !audio_started {
                            audio_started = true;
                            // #108: the program already owns the one loopback
                            // capture and Opus encoder; this session only adds
                            // its own track. Spawning a thread here was what made
                            // "two tabs" mean two captures of the same endpoint.
                            //
                            // #83: the media clock restarts with the first
                            // listener, so a reconnect does not inherit the skew
                            // of the session that just ended.
                            crate::pipeline::avsync::reset();
                            let audio_track = watcher_media.lock().await.audio().clone();
                            let program = {
                                let att = watcher_attach
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                att.shared.clone()
                            };
                            if let Some(program) = program {
                                let sink = program.audio.attach(audio_track);
                                let mut att = watcher_attach
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                att.audio = Some(sink);
                                log::info!("audio downlink attached to the shared program");
                            }
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
    let stats_attach = ctx.attach.clone();
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
            let mut stats_owner = false;
            loop {
                tick.tick().await;
                if !stats_flag.load(Ordering::SeqCst) {
                    continue;
                }
                // #101: nothing is attached until the PC reports Connected, and
                // `stream_stats` before that would describe a pipeline that does
                // not exist yet.
                let Some(shared) = stats_attach
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .shared
                    .clone()
                else {
                    continue;
                };
                let pipeline_handle = shared.handle.clone();
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
                let stats_arc = pipeline_handle.stats();
                // #101: exactly one session ticks this window. The counters are
                // zeroed after every snapshot, so a second ticker would halve
                // both panels' `encoded_fps` — a wrong number that looks
                // plausible. The owner publishes, everyone else reads.
                let snapshot = if !stats_owner && shared.claim_stats() {
                    stats_owner = true;
                    true
                } else {
                    false
                };
                let snapshot = if snapshot {
                    // #62: snapshot + window reset must share one lock. Taken
                    // separately, counters accumulated between the two
                    // acquisitions were dropped, so `encoded_fps` dipped on a
                    // regular cadence.
                    let snap = {
                        let mut guard = stats_arc.lock().unwrap_or_else(|p| p.into_inner());
                        let mut snapshot = guard.snapshot(std::time::Duration::from_secs(1));
                        // #78: the peak tracker needs `&mut` and its own "now",
                        // so it is closed here rather than inside `snapshot`.
                        snapshot.peak_mbps = guard.peak.take_peak_mbps(std::time::Instant::now());
                        guard.reset_window();
                        snapshot
                    };
                    shared.publish_snapshot(snap);
                    snap
                } else {
                    // The owner's window, up to one second stale. Zero before the
                    // owner's first tick — the panel shows a blank sample rather
                    // than a fabricated one.
                    shared.last_snapshot().unwrap_or_default()
                };
                let stats = StreamStats::from_host(host)
                    .with_pipeline(&snapshot)
                    .with_rtt(stats_latency.rtt_ms())
                    // #84: the QoS controller lives in the capture loop; this is
                    // the only path from there to the panel.
                    .with_throttled(pipeline_handle.is_throttled());
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
        // #96: pinning the pointer is a third host-pointer surface. It is the
        // most intrusive of the three — it moves both the speed and the
        // acceleration curve, and a policy that lets a user pick a DPI may
        // still refuse to let a session decide the curve for them.
        Action::SetPointerDirect(_) => EventKind::Control("control.pointer-direct"),
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

/// Whether a **viewer** may run this command (#101).
///
/// A viewer's job is to watch the program, not to steer it: one capture, one
/// encoder, one desktop, so anything that reflows the host, moves the encoder or
/// injects input belongs to the controller. Reads stay open — a second tab
/// showing the stats panel is exactly what a second tab is for.
///
/// The list is an allow-list on purpose. A new `Action` defaults to
/// controller-only, which fails safe: the cost of forgetting to add a harmless
/// verb is a viewer whose panel does not update; the cost of an accidental
/// permissive default is two browsers fighting over one pointer.
///
/// `StartVideo` is the one write that is **not** symmetric with its opposite.
/// The video gate (`PipelineHandle::video_on`) belongs to the shared program, and
/// with the gate shut the capture loop does not even capture — so every session
/// goes black, not just the one that asked. A controller tab going to the
/// background therefore closes the gate for everybody, and the visible viewer
/// would be stuck on a frozen picture it cannot un-freeze. The frontend makes
/// this concrete: when no chunks arrive for 3 s it resends `START_VIDEO`, three
/// times, then forces a websocket reconnect (selkies-ws-core.js:4612-4630) — with
/// the resend denied, that watchdog turns a transient into a reload loop.
///
/// Opening the gate is idempotent and costs only the encoder that the controller
/// already wanted running. Closing it is destructive to the other sessions, so
/// `StopVideo` stays controller-only: a viewer switching tabs must not blank
/// someone else's screen. The acknowledged cost is that a deliberate "pause
/// video" by the controller is not durable while a visible viewer is watching —
/// which is the right trade, because the alternative failure (a viewer that
/// cannot recover) has no way out from the browser side.
fn viewer_may_run(action: &Action) -> bool {
    matches!(
        action,
        Action::StartVideo
            | Action::StatsOn
            | Action::StatsOff
            | Action::PongReply(_)
            | Action::CompressedNegotiate
            | Action::ClipboardRequest
            | Action::Ignore(_)
            | Action::Unknown(_)
    )
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
    // #17: the session's virtual gamepad bus. Only the rumble receiver is taken
    // from it here; input already reaches it through `input`.
    gamepad: &Arc<GamepadHub>,
    tasks: &SessionTasks,
    // #94: a codec pick has to restart the session, and the restart is this
    // registry's close notice — the same one `/api/v1/control/session/kick`
    // sends. Passed in rather than reached for globally so the DC hook stays a
    // plain set of handles.
    registry: &SessionRegistry,
    // #101: whether this session owns the program. Everything that moves the
    // encoder, the host desktop or the injected input is refused for a viewer —
    // the client is told nothing about the refusal, but a viewer that could
    // reflow the host desktop under the controller's pointer is worse than one
    // that silently does nothing.
    //
    // #103: asked per message through the program, not captured once, so an
    // operator takeover moves the gate with the lease. `None` cannot happen (the
    // hooks are armed right after the attach that sets it) and is treated as
    // "owns" so a future path that arms earlier degrades to the old behaviour
    // rather than to a dead keyboard.
    gate: Option<ControlGate>,
    // #101: whether this is the session's first `Connected` — only then is the
    // recovery-event sink installed (see the call site).
    first_arm: bool,
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
    if first_arm {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<PipelineEvent>();
        pipeline_handle.add_event_sink(event_tx);
        let event_control = control.clone();
        // #109: the events below do not carry the display, but the snapshot
        // they build does; the settings object knows which screen this session
        // watches (set from the hello, before the attach).
        let event_display = settings.display();
        push_task(
            tasks,
            tokio::spawn(async move {
                while let Some(ev) = event_rx.recv().await {
                    let line = match ev {
                        PipelineEvent::RecoveryStart(n) => {
                            Some(system(&format!("capture_recovery,{n}")))
                        }
                        PipelineEvent::FallbackGdi(reason) => {
                            Some(system(&format!("capture_fallback_gdi,{reason}")))
                        }
                        PipelineEvent::Lost(reason) => Some(system(&format!("capture_lost,{reason}"))),
                        PipelineEvent::Restored => Some(system("capture_restored")),
                        PipelineEvent::StreamInfo { is_gdi, reason } => Some(stream_info(
                            &StreamInfo::from_capture(is_gdi, reason).with_display(event_display),
                        )),
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
    }

    // #17: rumble downlink. ViGEm notifies us when a game moves the motors, and
    // the client's pad plays it. The receiver is handed out once per session —
    // an ICE reconnect re-arms these notices and must not install a second
    // consumer (it would halve the messages between two pumps).
    //
    // 5000 ms is the frontend's own ceiling (`RUMBLE_MAX_MS`, gamepad.js:71):
    // `playEffect` is replace semantics, so giving each edge the full window
    // means a game that re-issues "rumble" every frame never sees it lapse, and
    // the stop edge (`0,0`) is what actually ends it.
    if let Some(mut rumble_rx) = gamepad.take_rumble_rx() {
        const RUMBLE_MS: u32 = 5000;
        let rumble_control = control.clone();
        push_task(
            tasks,
            tokio::spawn(async move {
                while let Some(rumble) = rumble_rx.recv().await {
                    if rumble_control.ready_state() != RTCDataChannelState::Open {
                        continue;
                    }
                    let line = system(&format!(
                        "rumble,{},{:.3},{:.3},{}",
                        rumble.slot, rumble.large, rumble.small, RUMBLE_MS
                    ));
                    if let Err(err) = rumble_control.send_text(&line).await {
                        log::debug!("rumble send skipped: {err:#}");
                        break;
                    }
                }
            }),
        );
    }

    // One-shot downstream notices, queued as plain lines for the open handler.
    // #50: `server_settings` is re-announced on every client settings request so
    // the frontend layout (manual resolution, framerate span) tracks the live
    // targets; the one-shot here seeds the first connect.
    let notices = [
        pipeline("Connected to server"),
        // #109: the same snapshot now names the screen, so "connected but the
        // picture is of the wrong monitor" is answerable from the stream_info
        // alone.
        stream_info(&StreamInfo::default().with_display(settings.display())),
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
    // #103: the lease handle is an Arc clone per channel; both ask the same
    // program, so the two hooks can never disagree about who owns input.
    let pointer_gate = gate.clone();
    let control_gate = gate;

    pointer.on_message(Box::new(move |msg| {
        let input = pointer_input.clone();
        let principal = pointer_principal.clone();
        let gate = pointer_gate.clone();
        Box::pin(async move {
            if let Some(gate) = gate.as_ref() {
                if !gate.owns() {
                    // #101: a viewer never injects. Not logged per message (a
                    // moving pointer is thousands of lines a second); the
                    // count in `health` says how much was dropped, and the
                    // session said so once when it attached (#103).
                    gate.refuse();
                    return;
                }
            }
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
        let gate = control_gate.clone();
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
            // #101: a viewer may read, not write. Checked before the policy
            // gate so a refusal is a role refusal (logged as such) and not a
            // policy denial the operator would go looking for in the rules.
            // #103: the lease is asked per message, so a takeover lands here.
            let owns = gate.as_ref().map(|gate| gate.owns()).unwrap_or(true);
            if !owns && !viewer_may_run(&action) {
                if let Some(gate) = gate.as_ref() {
                    gate.refuse();
                }
                log::debug!("control message ignored: viewer session ({line})");
                return;
            }
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
                        // #101: the encoder is shared, so the new m-line has to
                        // be negotiated by every session, not just this one.
                        let n = codec_registry
                            .kick("video codec changed; reconnecting", None)
                            .await;
                        if n == 0 {
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
                // #96: `ma,1` pins the host pointer to 1:1 while the client
                // draws its own ghost pointer in game mode; `ma,0` puts back
                // the speed step and the acceleration curve the pin replaced.
                // Both are `SystemParametersInfo` calls on the user session, so
                // both go through the blocking pool like `md,`.
                //
                // A release is also a no-op when nothing is pinned, which is
                // what lets the client send `ma,0` on every unlock instead of
                // tracking whether it ever sent `ma,1`.
                Action::SetPointerDirect(on) => {
                    log::info!("client sets pointer direct -> {on}");
                    let outcome = tokio::task::spawn_blocking(move || {
                        if on {
                            hostui::pin_pointer().map(|applied| {
                                log::info!(
                                    "host pointer pinned to 1:1 (speed={}, accel_off={}, \
                                     strategy={})",
                                    applied.speed,
                                    applied.accel,
                                    applied.strategy
                                );
                                pointer_direct_report(&applied)
                            })
                        } else {
                            hostui::unpin_pointer().map(|()| {
                                log::info!("host pointer released back to the session's own settings");
                                pointer_direct_release()
                            })
                        }
                    })
                    .await;
                    match outcome {
                        Ok(Ok(report)) => {
                            let _ = control.send_text(&report).await;
                        }
                        Ok(Err(reason)) => {
                            log::warn!("host pointer direct refused: {reason}");
                            let _ = control.send_text(&pointer_direct_failure(&reason)).await;
                        }
                        Err(join) => {
                            log::warn!("host pointer direct worker failed: {join}");
                            let _ =
                                control.send_text(&pointer_direct_failure("worker join error")).await;
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
        stream_info(&StreamInfo::default().with_display(settings.display())),
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

/// #96: the host pointer after a game-mode pin. `pinned: true` with
/// `strategy: "already"` means the host was 1:1 to begin with and the pin only
/// recorded what the release has to put back. The panel ignores the message;
/// the log and `docs/host-knobs.md` read it when a ghost pointer drifts.
fn pointer_direct_report(a: &hostui::AppliedPin) -> String {
    hbb_common::serde_json::json!({
        "type": "pointer_direct",
        "pinned": true,
        "speed": a.speed,
        "accel": a.accel,
        "strategy": a.strategy,
    })
    .to_string()
}

/// #96: the release's ack. `pinned: false` is also what a release with nothing
/// pinned answers, which is the normal case for a client that sends `ma,0` on
/// every unlock without tracking whether it ever sent `ma,1`.
fn pointer_direct_release() -> String {
    hbb_common::serde_json::json!({
        "type": "pointer_direct",
        "pinned": false,
    })
    .to_string()
}

/// #96: a pin or release the host refused. `pinned: null` because the host is
/// in neither state the client asked for — saying `false` would claim the
/// release succeeded.
fn pointer_direct_failure(reason: &str) -> String {
    hbb_common::serde_json::json!({
        "type": "pointer_direct",
        "pinned": hbb_common::serde_json::Value::Null,
        "error": reason,
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
pub(crate) fn apply_settings_batch(settings: &StreamSettings, payload: &str) -> Option<crate::pipeline::codec::VideoCodec> {
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

    /// #102：准入的边界必须恰好是 `max`，多一个都不行。
    ///
    /// 这条不是形式主义：共享编码下「再来一个人」不是免费的——每个 sink 都要
    /// 过一遍打包器、整份码率再拷一次，所以无上限的扇出不是「服务更多人」，
    /// 而是把正在看的人的画质拖下去。上限的作用是让迟到者得到一个体面的
    /// `503 + Retry-After`，而不是让整屋子人一起变卡。
    #[test]
    fn admission_holds_exactly_the_configured_number_of_slots() {
        let admission = Admission::new(2);
        let first = admission.try_enter().expect("first slot");
        let second = admission.try_enter().expect("second slot");
        assert!(admission.try_enter().is_none(), "the third must be refused");
        assert_eq!(admission.live(), 2);

        // A released slot is immediately reusable: a kicked, crashed or
        // reloading session must not permanently consume capacity.
        drop(first);
        assert_eq!(admission.live(), 1);
        let third = admission.try_enter().expect("after a release");
        assert_eq!(admission.live(), 2);

        drop((second, third));
        assert_eq!(admission.live(), 0, "every permit was returned");
    }

    /// 并发到达不能超发。
    ///
    /// 用 `load` 再 `store` 的写法下，同一 tick 到达的两个升级会都读到 `max - 1`
    /// 然后都进门——那正是限流存在的场景（一次刷新风暴里 N 个页签同时重连）。
    #[test]
    fn concurrent_arrivals_cannot_exceed_the_limit() {
        let admission = Admission::new(4);
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let admission = admission.clone();
                std::thread::spawn(move || admission.try_enter())
            })
            .collect();
        // Collect first, count second: a permit dropped inside `is_some()` would
        // free its slot while later threads are still racing for one, and the
        // test would then "prove" that 16 threads all got in — the exact failure
        // the compare-exchange exists to prevent.
        let permits: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect();
        let admitted = permits.iter().filter(|p| p.is_some()).count();
        assert_eq!(admitted, 4, "exactly the configured number got in");
        assert_eq!(admission.live(), 4);
    }

    /// `0` 会被夹到 `1`，而不是把运维自己锁在门外。
    #[test]
    fn a_zero_limit_is_clamped_instead_of_locking_everyone_out() {
        let admission = Admission::new(0);
        assert_eq!(admission.max(), 1);
        // The first permit must be **held**: `assert!(x.try_enter().is_some())`
        // would drop it on the same line and the next call would succeed, which
        // is a property of the test rather than of the code.
        let _held = admission.try_enter().expect("the single slot");
        assert!(admission.try_enter().is_none());
    }

    /// #101：角色门禁的非对称性——开门放行，关门只归 controller。
    ///
    /// 这条挡的是「第二个页面看着看着黑屏，而且永远好不了」：视频门在共享程序
    /// 上，controller 的页签切到后台就会发 `STOP_VIDEO` 把门关上（前端在可见性
    /// 变化时正是这条），循环随即连采集都不做，于是**所有**会话一起黑。viewer
    /// 唯一的自救是重发 `START_VIDEO`（前端 3 秒无帧重发三次后强制重连），
    /// 这一条被拒就把一次瞬断变成了自我维持的刷新循环。
    ///
    /// 反过来 `StopVideo` 必须挡：否则任何一个 viewer 切页签都会让别人的屏幕黑掉。
    #[test]
    fn a_viewer_may_open_the_video_gate_but_not_close_it() {
        assert!(
            viewer_may_run(&Action::StartVideo),
            "opening is idempotent: it costs the encoder the controller already wanted"
        );
        assert!(
            !viewer_may_run(&Action::StopVideo),
            "closing the gate takes every session down, not just the one that asked"
        );
        // 输入面与设置面一律不放行：这两类都是"有人在操作这台机器"的声明。
        assert!(!viewer_may_run(&Action::InputStub("kd,65".to_owned())));
        assert!(!viewer_may_run(&Action::SetFps(30)));
        assert!(!viewer_may_run(&Action::SetAudioOn(false)));
        // 读类照旧放行，否则第二个页签连统计面板都看不了。
        assert!(viewer_may_run(&Action::StatsOn));
        assert!(viewer_may_run(&Action::StatsOff));
    }

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
            Action::SetPointerDirect(true),
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

    /// #103：输入租约挂在**程序**上，所以接管对下一条消息立刻生效。
    ///
    /// 这条钉的是「不缓存」这件事本身。把 `controller` 存成会话字段的实现也
    /// 能通过「第一个连上的人是 controller」的测试，但接管之后旧会话会一直
    /// 往被控端注入，直到它自己重连为止。
    #[test]
    fn the_input_lease_is_read_from_the_program_not_cached_on_the_session() {
        use crate::pipeline::SharedPipeline;

        let program = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&Config::default()),
            // 严格模式：这条测试问的就是租约本身，共享模式下 `owns()` 恒真。
            InputPolicy::Exclusive,
        );
        let denied_a = Arc::new(AtomicU64::new(0));
        let denied_b = Arc::new(AtomicU64::new(0));
        let a = ControlGate {
            program: program.clone(),
            session: 1,
            denied: denied_a.clone(),
        };
        let b = ControlGate {
            program: program.clone(),
            session: 2,
            denied: denied_b.clone(),
        };

        program.set_controller(Some(1));
        assert!(a.owns());
        assert!(!b.owns());

        // The switch is visible to both sides on the next message: no reconnect,
        // and no window in which two sessions both believe they own the pointer.
        program.set_controller(Some(2));
        assert!(!a.owns());
        assert!(b.owns());

        // Refusals are counted per session, which is what makes "the viewer's
        // pointer does nothing" diagnosable instead of mysterious.
        a.refuse();
        a.refuse();
        assert_eq!(denied_a.load(Ordering::Relaxed), 2);
        assert_eq!(denied_b.load(Ordering::Relaxed), 0);
    }

    /// #105：默认共享输入时，非持有租约的会话也能操作。
    ///
    /// 上一版把第二个页签挡在门外（`role: viewer`），运维看到的却是「谁按都
    /// 没反应」——因为租约在程序重建后是空的，连第一个页签也是 viewer。
    /// 共享模式把这条歧路整个关掉；严格模式仍在，只是要显式打开。
    #[test]
    fn shared_input_lets_a_session_without_the_lease_act() {
        use crate::pipeline::SharedPipeline;

        let program = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&Config::default()),
            InputPolicy::Shared,
        );
        let gate = ControlGate {
            program: program.clone(),
            session: 7,
            denied: Arc::new(AtomicU64::new(0)),
        };
        program.set_controller(Some(6));
        assert!(
            gate.owns(),
            "共享输入：租约在别人手里也不该挡住这个会话"
        );
        // 全公开的只读白名单在共享模式下不再有机会被触发，但保留它对
        // `exclusive` 仍然有效。
        assert!(viewer_may_run(&Action::StatsOn));
    }

    /// #103：持租约的会话退出时，租约交给同一块屏上**最早**的幸存者。
    ///
    /// 交给 `None` 也能编译、也能让程序继续出图，但剩下的人会看着一个碰不了
    /// 的桌面，而且重连也修不好——租约按程序记，不按 socket 记。
    #[tokio::test]
    async fn the_lease_passes_to_the_oldest_survivor_when_its_holder_leaves() {
        let registry = SessionRegistry::default();
        for id in 1..=3u64 {
            let (notice, _rx) = mpsc::unbounded_channel();
            registry.register(id, notice).await;
            registry
                .publish(SessionInfo {
                    id,
                    // Distinct tabs: `publish` evicts same-tab sessions, and an
                    // empty tab id would make all three evict each other.
                    tab_id: format!("tab-{id}"),
                    display: Some(0),
                    ..Default::default()
                })
                .await;
        }

        // The middle session leaves: session 1 is older and still attached.
        assert_eq!(registry.successor(2, 0).await, Some(1));
        registry.remove(2).await;
        // The oldest leaves: only session 3 is left on this display.
        assert_eq!(registry.successor(1, 0).await, Some(3));
        // Another display has no survivor on this program.
        assert_eq!(registry.successor(1, 1).await, None);
    }
}
