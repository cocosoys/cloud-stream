//! Capture / encode main loop (#36 design 4.3; #42-1 recovery chain).
//!
//! Beat = `Config::spf()`; duration of every emitted Sample is exactly spf
//! (design 9.2 #4). R1 (#49): the beat uses a deadline accumulator instead of
//! per-iteration `Instant::now()` so a capture/encode overrun never drifts the
//! next beat; an overrun snaps the baseline to completion time + spf. The
//! capture wait is capped at `CAPTURE_TIMEOUT_FRACTION` of spf (~13 ms at
//! 30 fps) so a WouldBlock + repeat encode still fits inside one beat.
//! R2 revision (#62, correctness over cost): a WouldBlock now **re-encodes the
//! last YUV buffer** instead of re-sending the cached encoded payload. A VP8
//! P-frame is a residual against the previous *reconstructed* frame; replaying
//! the same payload under a new RTP timestamp makes the decoder apply that
//! residual twice, so the reference chain drifts frame by frame — visible as
//! colour smear / ghosting on any moving content at a WouldBlock beat. The
//! re-encode produces a genuine repeat frame (residual ~0 against an identical
//! reference) and is capped by `REPEAT_ENCODE_MAX` consecutive frames per new
//! frame, so an idle desktop never burns the encoder forever: past the cap the
//! loop simply sends nothing, which leaves the reference chain intact (a
//! missing frame is invisible, a mismatched one is corruption).
//!
//! #42-1 recovery chain (aligned with RustDesk `video_service.rs`):
//! * DXGI errors (`ConnectionReset` / `InvalidData` / other while on DXGI) get
//!   a finite rebuild budget: `DXGI_RECOVERY_LIMIT = 3` attempts inside
//!   `DXGI_RECOVERY_WINDOW = 10s`; when the budget is exhausted the capturer
//!   switches permanently to the bundled GDI backend.
//! * WouldBlock outside the `DXGI_RECOVERY_FRAME_GRACE` window counts toward
//!   `TRY_GDI_LIMIT = 3`; above that the capturer switches to GDI as well.
//! * Only consecutive errors while already on GDI are fatal (`GDI_FATAL_LIMIT`).
//! * Every recovery / fallback / lost / restored state is handed to the async
//!   session through `PipelineEvent` so the session can mirror it on the data
//!   channel; the blocking worker never touches an await.
//!
//! Threading boundary (RustDesk video_service.rs pattern): the scrap capturer,
//! frame and `EncoderApi` hold non-`Send` Win32 / DXGI raw pointers, so they
//! must never live across an `await`. The synchronous capture/encode body runs
//! on a `spawn_blocking` worker; encoded `Bytes` move to a small async forwarder
//! task through an unbounded channel, which owns `video.write_sample().await`.
//! The shared statistics behind the gate use a `std::sync::Mutex` so the blocking
//! worker can update counters without crossing an await.

pub mod capture;
pub mod cursor;
pub mod encoder;
pub mod sample_map;
pub mod scale;
pub mod settings;
pub mod stats;

pub use stats::{FrameCounters, PipelineSnapshot, PipelineStats};
pub use capture::CaptureSource;
pub use cursor::{cursor_data, cursor_info, cursor_png_b64, cursor_pos, draw_cursor, virtual_origin, CursorData};
pub use encoder::VideoEncoder;
pub use sample_map::video_frame_to_chunks;
pub use scale::bgra_scale;
pub use settings::StreamSettings;

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use log;
use scrap::{convert_to_yuv, EncodeInput, Frame, PixelBuffer, TraitPixelBuffer};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::webrtc::VideoTrack;

/// #63: capture wait as a percentage of the beat (spf). Measured on the
/// 1718x926 VM: a WouldBlock beat spent ~11 ms inside `AcquireNextFrame`, which
/// on its own nearly fills the 16.7 ms a 60 fps beat has. The wait is pure loss
/// when the desktop is static (DXGI yields nothing until something changes), so
/// the poll is now short and the repeat path holds the cadence instead; the only
/// cost is up to one beat of extra latency for brand-new content.
const CAPTURE_TIMEOUT_FRACTION: u64 = 15;
/// DXGI rebuild attempts allowed inside one recovery window (video_service.rs).
const DXGI_RECOVERY_LIMIT: u32 = 3;
/// Sliding window for the DXGI rebuild counter.
const DXGI_RECOVERY_WINDOW: Duration = Duration::from_secs(10);
/// #62: consecutive repeat (WouldBlock) frames that may be re-encoded before
/// the loop stops sending and waits for a real frame. #63 raised this from 8 to
/// 600 after measuring: DXGI yields a real frame only when the desktop changes
/// (an idle desktop produced ~2 new frames/s), so the repeat path *is* what
/// holds the output cadence. With the cap at 8 the browser's fps counter
/// collapsed to 10-16 on a static screen. A repeat re-encode costs ~4 ms at
/// 720p, i.e. ~25% of one core at 60 fps — affordable on this host, and 600
/// keeps a hard bound (10 s at 60 fps) instead of an unbounded loop.
const REPEAT_ENCODE_MAX: u32 = 600;
/// #62: minimum spacing between forced keyframes. The browser sends a PLI for
/// every lost frame, and a keyframe is much larger than a P-frame, so an
/// unthrottled response re-creates the loss that triggered it (PLI storm ->
/// "clear / blurry" oscillation). One keyframe per 500 ms is the RustDesk /
/// Chromium order of magnitude.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// #63: how often the worker prints its own throughput counters. The 1 s
/// `stream_stats` message only exists while the browser has subscribed, and it
/// is invisible in the server log — without this line a low fps report has no
/// server-side evidence at all (is the loop starving, is the capture dry, or is
/// the encoder slow?).
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(5);
/// #62: encode -> `write_sample` hand-off depth, in encoded frames (the sample
/// map yields exactly one chunk per VP8 frame). Bounded on purpose: the
/// forwarder stalls when the transport stalls, and an unbounded queue turns a
/// network hiccup into unbounded latency + memory growth instead of dropped
/// frames. 8 frames is ~0.27 s at 30 fps.
const SAMPLE_QUEUE: usize = 8;
/// #59: while on the GDI backend, periodically retry the DXGI path. An idle
/// desktop (MC main menu, static docs) legitimately yields no new DXGI frame
/// for many seconds; #57's stall-window fallback misclassified that as a
/// broken capturer and permanently parked the session on the slow, low-rate
/// GDI path — which then showed color-smear artifacts. WouldBlock is now
/// never a fault (only a real GetFrame error is), and a GDI session re-opens
/// the capturer every this often to get back onto DXGI when it can.
const GDI_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// #62: how many times a session parked on GDI may try to climb back to DXGI.
/// Without a ceiling a machine where DXGI is permanently unavailable (VMware /
/// RDP / some virtual display drivers) rebuilds the capturer + encoder forever
/// on a 30 s cycle, and every rebuild drops the cached reference state.
const GDI_RETRY_MAX: u32 = 3;
/// Consecutive capturer errors while already on GDI that end the loop.
const GDI_FATAL_LIMIT: u64 = 3;
/// Outer open/rebuild backoff, doubled and capped.
const OPEN_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
const OPEN_BACKOFF_MAX: Duration = Duration::from_secs(2);
/// #60: diagnostic frame dump cadence (every Nth valid frame).
const DUMP_EVERY: u64 = 150;
/// #60: encoded VP8 IVF window length (frames); at 60 fps this is ~5 s of
/// contiguous encoder output, enough for ffmpeg to re-establish reference
/// frames and reproduce a transport-independent artifact.
const IVF_WINDOW: usize = 300;

/// #52: session bitrate target = configured `--bitrate` scaled by fps/30
/// (the configured value is defined at 30 fps). A 60 fps / 1080p session gets
/// ~2x the budget so VP8 keeps a sane per-frame quality instead of block
/// artifacts; clamped to a sane floor.
fn bitrate_for(base_kbps: u32, fps: u32) -> u32 {
    ((base_kbps as u64 * fps as u64 / 30).clamp(200, 40_000)) as u32
}

/// Events the blocking worker hands to the async session sink (#42-1). The
/// session maps each one to a whitelisted outbound data-channel message; the
/// worker itself never sends on the channel (thread boundary).
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// DXGI rebuild attempt `n` (1..=`DXGI_RECOVERY_LIMIT`).
    RecoveryStart(u32),
    /// Switched permanently to GDI; short English reason whitelist tag.
    FallbackGdi(String),
    /// Unrecoverable; the loop terminates right after this event.
    Lost(String),
    /// A valid frame arrived after an abnormal state; clear the frontend notice.
    Restored,
    /// Refresh the panel capture row with the live backend / reason.
    StreamInfo { is_gdi: bool, reason: String },
    /// C2 (#51): the OS cursor identity changed; the session serializes this
    /// into the selkies `cursor` message (base64 PNG). Emitted only on identity
    /// change / hide, never per-frame.
    Cursor(CursorData),
}

/// Shared mutable state between the signaling gate, the stats timer and the
/// capture loop. `video_on` / `stopped` live behind `Arc<AtomicBool>` because
/// the signaling task and the loop task observe them on different handlers;
/// `stats` is a shared `std::sync::Mutex` so both the blocking worker and the
/// async 1 s stats tick can touch the counters without an await inside the lock;
/// `events` is a one-way sink installed by the session after the media channel
/// opens.
#[derive(Clone)]
pub struct PipelineHandle {
    video_on: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    stats: Arc<Mutex<PipelineStats>>,
    events: Arc<Mutex<Option<mpsc::UnboundedSender<PipelineEvent>>>>,
    /// #53: set by the session's RTCP PLI/FIR listener; the capture loop
    /// consumes it on the next beat and forces a keyframe.
    keyframe_requested: Arc<AtomicBool>,
    /// #56: `vb,<N>` from the client settings panel; 0 = automatic (the
    /// fps-scaled `--bitrate` budget). The capture loop consumes it on the
    /// next beat and re-targets the VP8 encoder.
    bitrate_override: Arc<AtomicU32>,
    /// #57: `SET_NATIVE_CURSOR_RENDERING,0|1`. The frontend renders the
    /// pointer itself (browser-native cursor, no pointer lock) at 0; at 1
    /// (pointer lock) it hides its own cursor and expects the server to
    /// composite the OS cursor into the video frames (C1). Default 0 so the
    /// unfocused browser never shows a duplicated composited cursor.
    cursor_inject: Arc<AtomicBool>,
    /// #62: the session installs a sink here; the blocking worker reports its
    /// own termination (error or panic) through it. Without this a dead worker
    /// left the PC / data channels alive, so the browser showed a frozen picture
    /// with live stats and no error.
    exit_sink: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
}

impl Default for PipelineHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineHandle {
    pub fn new() -> Self {
        // Default ON, aligned with RustDesk video_service: a connected session
        // captures/encodes immediately. START_VIDEO / STOP_VIDEO are only the
        // later pause / resume commands; the frontend resume watchdog omits its
        // first START_VIDEO while the play button waits for a user gesture, so an
        // off initial gate would leave the feed idle until that path fires.
        Self {
            video_on: Arc::new(AtomicBool::new(true)),
            stopped: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(PipelineStats::default())),
            events: Arc::new(Mutex::new(None)),
            keyframe_requested: Arc::new(AtomicBool::new(false)),
            bitrate_override: Arc::new(AtomicU32::new(0)),
            cursor_inject: Arc::new(AtomicBool::new(false)),
            exit_sink: Arc::new(Mutex::new(None)),
        }
    }

    /// `START_VIDEO`: idempotent; repeated watchdog sends only set the flag again.
    pub fn start(&self) {
        self.video_on.store(true, Ordering::SeqCst);
    }

    /// `STOP_VIDEO`: stop writing samples; resources stay alive.
    pub fn stop(&self) {
        self.video_on.store(false, Ordering::SeqCst);
    }

    pub fn is_on(&self) -> bool {
        self.video_on.load(Ordering::SeqCst)
    }

    /// Session cleanup: stop the loop task and close the write gate together.
    pub fn stop_loop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.video_on.store(false, Ordering::SeqCst);
    }

    /// #53: RTCP PLI/FIR listener entry point. The capture loop consumes the
    /// flag on its next beat (see `run_inner_beat`) and forces a keyframe.
    pub fn request_keyframe(&self) {
        self.keyframe_requested.store(true, Ordering::SeqCst);
    }

    /// #56: `vb,<N>` client bitrate request. Stored for the capture loop,
    /// which consumes it on the next beat and re-targets the VP8 encoder.
    /// `0` restores the automatic fps-scaled budget.
    pub fn set_bitrate_override(&self, kbps: u32) {
        self.bitrate_override.store(kbps, Ordering::SeqCst);
    }

    /// #57: frontend pointer-rendering handover. `true` (pointer lock,
    /// `SET_NATIVE_CURSOR_RENDERING,1`): the frontend hides its own pointer
    /// and the pipeline composites the OS cursor into frames (C1). `false`
    /// (default): the frontend shows its browser-native cursor and the
    /// pipeline must not composite, or the user sees a duplicated pointer.
    pub fn set_cursor_inject(&self, on: bool) {
        self.cursor_inject.store(on, Ordering::SeqCst);
        log::info!("cursor injection -> {}", if on { "on (server composite)" } else { "off (frontend native)" });
    }

    /// Whether the pipeline should composite the OS cursor this beat.
    pub fn cursor_inject_on(&self) -> bool {
        self.cursor_inject.load(Ordering::SeqCst)
    }

    /// Shared aggregator for the stats timer's snapshot / reset window.
    pub fn stats(&self) -> Arc<Mutex<PipelineStats>> {
        self.stats.clone()
    }

    /// Install the session-side event sink. Replacing an earlier sink idempotently
    /// for a reconnect; the sink channel is `try_send`-sized so a slow frontend
    /// never blocks the capture beat.
    pub fn set_event_sink(&self, tx: mpsc::UnboundedSender<PipelineEvent>) {
        if let Ok(mut guard) = self.events.lock() {
            *guard = Some(tx);
        }
    }

    /// Emit one recovery event to the session sink; a missing sink (session not
    /// armed yet) is a silent no-op, matching the no-op default gate.
    pub fn emit_event(&self, ev: PipelineEvent) {
        let tx = self
            .events
            .lock()
            .ok()
            .and_then(|g| g.clone());
        if let Some(tx) = tx {
            let _ = tx.send(ev);
        }
    }

    /// #62: install the worker-termination sink (the session closes the socket
    /// on a report so the browser reconnects instead of showing a frozen frame).
    pub fn set_exit_sink(&self, tx: mpsc::UnboundedSender<String>) {
        if let Ok(mut guard) = self.exit_sink.lock() {
            *guard = Some(tx);
        }
    }

    /// #62: report an unexpected worker exit. A session-driven stop
    /// (`stop_loop`) is not an exit worth reporting, so it is filtered here.
    fn signal_exit(&self, reason: String) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let tx = self.exit_sink.lock().ok().and_then(|g| g.clone());
        if let Some(tx) = tx {
            let _ = tx.send(reason);
        }
    }
}

/// DXGI finite-recovery counter, mirroring `DxgiRecoveryState` in
/// `video_service.rs:77-120`. The window slides: once expired the attempt count
/// restarts from zero on the next recoverable error.
struct DxgiRecoveryState {
    attempts: u32,
    window_started: Option<Instant>,
}

impl DxgiRecoveryState {
    fn new() -> Self {
        Self {
            attempts: 0,
            window_started: None,
        }
    }

    /// Register one recoverable DXGI error and return the attempt number
    /// (`Some(1..=LIMIT)`), or `None` when the budget is exhausted and the
    /// caller must switch permanently to GDI.
    fn next_attempt(&mut self, now: Instant) -> Option<u32> {
        let expired = match self.window_started {
            None => true,
            Some(t) => now.duration_since(t) >= DXGI_RECOVERY_WINDOW,
        };
        if expired {
            self.attempts = 0;
            self.window_started = Some(now);
        }
        if self.attempts >= DXGI_RECOVERY_LIMIT {
            return None;
        }
        self.attempts += 1;
        Some(self.attempts)
    }
}

/// Outcome handed back from the inner beat loop to the outer recovery scheduler.
enum InnerOutcome {
    /// Session cleanup stopped the loop; the outer scheduler exits too.
    NormalStop,
    /// A recoverable capture error: drop capturer + encoder and open a fresh
    /// pair. `from_gdi` reports whether the abandoned pair was on the GDI
    /// backend, which is what the DXGI-restore retry budget counts.
    Rebuild { from_gdi: bool },
    /// Unrecoverable failure; message already emitted, return `Err` to end worker.
    Fatal(String),
}

/// Spawn the blocking capture/encode worker plus the async sample forwarder once
/// the PC reaches Connected. The gate inside still waits for `START_VIDEO`
/// before grabbing frames; the track is accepted by the packetizer only after
/// binding, which happens on offer/answer.
pub fn spawn_loop(
    cfg: Arc<Config>,
    video: VideoTrack,
    handle: PipelineHandle,
    settings: StreamSettings,
) {
    // #62: bounded hand-off (see `SAMPLE_QUEUE`); the producer uses `try_send`
    // so a stalled transport can never grow the queue without bound.
    let (tx, mut rx) = mpsc::channel::<Bytes>(SAMPLE_QUEUE);
    let forward_handle = handle.clone();
    let forward_settings = settings.clone();

    // Async forwarder: only this task may touch `video.write_sample().await`.
    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if forward_handle.stopped.load(Ordering::SeqCst) {
                break;
            }
            // #50: the sample duration follows the *live* fps so a client
            // frame-rate change re-paces the RTP timestamps without a restart.
            let spf = forward_settings.spf();
            if let Err(err) = video.write_sample(data, spf).await {
                log::debug!("write_sample failed: {err:#}");
            }
        }
        log::debug!("sample forwarder exited");
    });

    let worker_handle = handle.clone();
    let exit_handle = handle.clone();
    tokio::task::spawn_blocking(move || {
        // #62: `spawn_blocking` swallows a panic into a `JoinError` nobody
        // awaits, which used to leave the PC + data channels alive with a
        // frozen picture. Catch it here and report on the exit sink instead.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_blocking_loop(cfg, tx, worker_handle, settings)
        }));
        match outcome {
            Ok(Ok(())) => log::info!("pipeline worker exited"),
            Ok(Err(err)) => {
                log::error!("pipeline loop ended: {err:#}");
                exit_handle.emit_event(PipelineEvent::Lost(format!(
                    "pipeline_exit: {}",
                    short_reason(&*err)
                )));
                exit_handle.signal_exit(format!("{err:#}"));
            }
            Err(_) => {
                log::error!("pipeline worker panicked; reporting to the session");
                exit_handle.emit_event(PipelineEvent::Lost("pipeline_panic".to_string()));
                exit_handle.signal_exit("pipeline worker panicked".to_string());
            }
        }
    });
}

/// Outer recovery scheduler: open capturer + encoder, hand the pair to the inner
/// beat loop, rebuild the pair on a recoverable DXGI error *or* a client
/// resolution change (#50), and only exit on session cleanup or a fatal failure.
fn run_blocking_loop(
    cfg: Arc<Config>,
    tx: mpsc::Sender<Bytes>,
    handle: PipelineHandle,
    settings: StreamSettings,
) -> Result<()> {
    let mut backoff = OPEN_BACKOFF_INITIAL;
    let mut open_fails: u32 = 0;
    // #62: how many DXGI-restore rebuilds a GDI-parked session has already
    // spent; at `GDI_RETRY_MAX` the 30 s retry stops so a machine without a
    // working duplication path stops thrashing the capturer.
    let mut gdi_retries: u32 = 0;
    // Outlasts a Rebuild: a recovery attempt leaves the inner beat in an abnormal
    // state, so the next inner beat's first valid frame reports `Restored` to the
    // frontend notice that the recovery hint can be cleared.
    let mut abnormal = false;

    loop {
        if handle.stopped.load(Ordering::SeqCst) {
            log::info!("pipeline loop stopped by session cleanup");
            break;
        }

        let source = match CaptureSource::open_primary() {
            Ok(source) => source,
            Err(err) => {
                open_fails += 1;
                log::warn!("capture source open retry #{open_fails}: {err:#}");
                handle.emit_event(PipelineEvent::RecoveryStart(open_fails.min(DXGI_RECOVERY_LIMIT)));
                if open_fails > DXGI_RECOVERY_LIMIT {
                    let reason = short_reason(&*err);
                    handle.emit_event(PipelineEvent::Lost(format!("open_failed: {reason}")));
                    anyhow::bail!("capture source open failed after recovery budget: {err:#}");
                }
                sleep_with_stop(backoff, &handle);
                backoff = backoff.checked_mul(2).unwrap_or(OPEN_BACKOFF_MAX).min(OPEN_BACKOFF_MAX);
                continue;
            }
        };
        open_fails = 0;
        backoff = OPEN_BACKOFF_INITIAL;

        let (src_w, src_h) = source.dimensions();
        // #50: a client that never asked for an explicit resolution captures the
        // native display 1:1 (no scaling); an explicit request turns the native
        // frame into a scaler target below.
        settings.seed_native(src_w, src_h);
        let (enc_w, enc_h) = (settings.width(), settings.height());
        let mut encoder = match VideoEncoder::new(enc_w, enc_h, cfg.bitrate_kbps) {
            Ok(encoder) => encoder,
            Err(err) => {
                let reason = short_reason(&*err);
                handle.emit_event(PipelineEvent::Lost(format!("encoder_open_failed: {reason}")));
                anyhow::bail!("VP8 encoder open failed: {err:#}");
            }
        };
        // #52: re-target the libvpx bitrate right after open so the configured
        // `--bitrate` (not the internal per-resolution curve) governs the stream
        // from the first frame; the target scales with the session fps so a
        // 60 fps / 1080p session gets ~2x the 30 fps budget (VP8 block artifacts
        // under 1 Mbps are exactly the corruption seen in the field).
        encoder.set_bitrate_kbps(bitrate_for(cfg.bitrate_kbps, settings.fps()));

        match run_inner_beat(
            tx.clone(),
            handle.clone(),
            source,
            encoder,
            settings.clone(),
            cfg.bitrate_kbps,
            cfg.dump_frame.clone(),
            &mut abnormal,
            gdi_retries < GDI_RETRY_MAX,
        ) {
            InnerOutcome::NormalStop => break,
            InnerOutcome::Rebuild { from_gdi } => {
                if from_gdi {
                    gdi_retries += 1;
                    if gdi_retries >= GDI_RETRY_MAX {
                        log::warn!(
                            "DXGI restore abandoned after {gdi_retries} attempts; \
                             session stays on the GDI backend"
                        );
                        handle.emit_event(PipelineEvent::StreamInfo {
                            is_gdi: true,
                            reason: "dxgi_unavailable".to_string(),
                        });
                    }
                } else {
                    gdi_retries = 0;
                }
                log::warn!("outer: rebuilding capturer/encoder after recoverable capture error");
                sleep_with_stop(OPEN_BACKOFF_INITIAL, &handle);
                continue;
            }
            InnerOutcome::Fatal(msg) => {
                return Err(anyhow::anyhow!(msg));
            }
        }
    }

    Ok(())
}

/// Synchronous capture / encode body on the blocking worker. No `await` may
/// appear here: the scrap capturer, frame and encoder are non-`Send` inside this
/// thread boundary.
///
/// #50 dynamic cadence / resolution: every beat re-reads the live
/// `StreamSettings`. A fps change re-bases the deadline accumulator and swaps
/// the capture timeout and the sample duration; a resolution change returns
/// `Rebuild` so the outer scheduler reopens the capturer and recreates the
/// encoder at the new target size (the native frame is scaled by
/// `bgra_scale` when the target differs).
#[allow(clippy::too_many_arguments)]
fn run_inner_beat(
    tx: mpsc::Sender<Bytes>,
    handle: PipelineHandle,
    mut source: CaptureSource,
    mut encoder: VideoEncoder,
    settings: StreamSettings,
    base_bitrate_kbps: u32,
    dump_frame: Option<PathBuf>,
    abnormal: &mut bool,
    // #62: false once the GDI -> DXGI restore budget is spent, which disables
    // the periodic rebuild below (no more 30 s thrash on a machine where DXGI
    // is genuinely unavailable).
    allow_gdi_retry: bool,
) -> InnerOutcome {
    let mut yuv: Vec<u8> = Vec::new();
    let mut mid_data: Vec<u8> = Vec::new();
    // #50 scaling scratch: native BGRA copy + scaled BGRA, reused across beats.
    let mut scratch_bgra: Vec<u8> = Vec::new();
    let mut scaled_buf: Vec<u8> = Vec::new();
    // #51 cursor state: identity cache (bitmap re-captured only on handle
    // change) + the last emitted id so hide transitions fire once.
    let mut cursor_cache: Option<CursorData> = None;
    let mut last_cursor_id: Option<u64> = None;
    // R2 (#62): the YUV of the last *encoded* frame. A WouldBlock re-encodes it
    // (a real repeat frame) so the VP8 reference chain stays valid; the two
    // buffers are swapped instead of copied, so this costs nothing per frame.
    let mut last_yuv: Vec<u8> = Vec::new();
    let mut repeat_budget: u32 = 0;
    // #62: last forced keyframe, for the PLI debounce below.
    let mut last_keyframe: Option<Instant> = None;
    let mut gdi_err: u64 = 0;
    // #63: loop-rate / frame-count probe (see `STATS_LOG_INTERVAL`).
    let mut beats: u64 = 0;
    let mut prev_counters = FrameCounters::default();
    let mut last_stats_log = Instant::now();
    // #63: per-window cost split — capture block vs. BGRA->YUV+scale.
    let mut cap_wait_sum: f64 = 0.0;
    let mut cap_wait_n: u64 = 0;
    let mut conv_sum: f64 = 0.0;
    let mut conv_n: u64 = 0;
    // Last beat's conversion path, reported by the next stats line.
    let mut last_scale = false;
    let mut last_copy = false;
    let mut recovery = DxgiRecoveryState::new();
    // #59: last beat a DXGI restore was attempted while parked on GDI.
    let mut last_gdi_retry: Option<Instant> = None;
    let mut reason = String::new();
    // #60: frame dump counter (valid frames only).
    let mut dump_idx: u64 = 0;
    // #60: encoded-VP8 IVF window buffer + sequence.
    let mut ivf_frames: Vec<(Vec<u8>, i64)> = Vec::new();
    let mut ivf_seq: u64 = 0;

    let start = Instant::now();
    let stats = handle.stats.clone();
    let yuvfmt = encoder.yuvfmt();
    // #50: encoder target resolution (=== settings after seed_native/rebuild).
    let (enc_w, enc_h) = (settings.width(), settings.height());
    let (src_w, src_h) = source.dimensions();
    // The fps the cadence was last aligned to; a change re-bases the deadline.
    let mut last_fps = settings.fps();
    let mut spf = settings.spf();
    // R1: the first beat starts immediately, every following beat settles on a
    // sliding deadline; an overrun snaps the baseline so drift never accumulates.
    let mut next_deadline = Instant::now() + spf;

    log::info!(
        "pipeline loop started: target {}x{}@{}fps spf={:?} bitrate={}kbps hw={}",
        enc_w,
        enc_h,
        last_fps,
        spf,
        encoder.bitrate(),
        encoder.is_hardware()
    );

    loop {
        if handle.stopped.load(Ordering::SeqCst) {
            log::info!("pipeline loop stopped by session cleanup");
            return InnerOutcome::NormalStop;
        }
        // #63: periodic self-report so a "fps is low" report has server-side
        // evidence (loop rate vs. capture rate vs. encode cost). Counters are
        // differenced against the previous window; the session's 1 s stats tick
        // keeps owning `reset_window`, so nothing is cleared here.
        beats += 1;
        let since_log = last_stats_log.elapsed();
        if since_log >= STATS_LOG_INTERVAL {
            let (c, enc_ms, pipe_ms) = {
                let guard = stats.lock().unwrap_or_else(|p| p.into_inner());
                (
                    guard.counters,
                    guard.encode_timing.avg_ms(),
                    guard.pipeline_timing.avg_ms(),
                )
            };
            let d = |now: u64, before: u64| now.saturating_sub(before);
            let cap_avg = if cap_wait_n > 0 {
                cap_wait_sum / cap_wait_n as f64
            } else {
                0.0
            };
            let conv_avg = if conv_n > 0 {
                conv_sum / conv_n as f64
            } else {
                0.0
            };
            log::info!(
                "pipeline {:.1}s: loop={:.1}Hz new={} repeat={} skip={} wb={} inv={} cap_err={} \
                 enc_ok={} enc_no_out={} enc_err={} ws_err={} cap_wait={:.2}ms to={:.2}ms conv={:.2}ms \
                 enc_avg={:.1}ms pipe_avg={:.1}ms scale={} carry={}",
                since_log.as_secs_f64(),
                beats as f64 / since_log.as_secs_f64(),
                d(c.new_frames, prev_counters.new_frames),
                d(c.repeat_ok, prev_counters.repeat_ok),
                d(c.repeat_skipped, prev_counters.repeat_skipped),
                d(c.would_block, prev_counters.would_block),
                d(c.invalid, prev_counters.invalid),
                d(c.capture_err, prev_counters.capture_err),
                d(c.encoded_ok, prev_counters.encoded_ok),
                d(c.encode_no_output, prev_counters.encode_no_output),
                d(c.encode_err, prev_counters.encode_err),
                d(c.write_sample_err, prev_counters.write_sample_err),
                cap_avg,
                capture_timeout_for(spf).as_secs_f64() * 1000.0,
                conv_avg,
                enc_ms,
                pipe_ms,
                last_scale,
                last_copy
            );
            prev_counters = c;
            beats = 0;
            cap_wait_sum = 0.0;
            cap_wait_n = 0;
            conv_sum = 0.0;
            conv_n = 0;
            last_stats_log = Instant::now();
        }
        // #53: PLI/FIR from the browser -> force the next encode to a keyframe
        // so the receiver re-anchors its reference chain after packet loss.
        // #62: debounced. The browser emits one PLI per lost frame and a
        // keyframe is far bigger than a P-frame, so an unthrottled answer
        // re-creates the loss that asked for it (self-sustaining oscillation).
        if handle.keyframe_requested.swap(false, Ordering::SeqCst) {
            let due = match last_keyframe {
                Some(t) => t.elapsed() >= KEYFRAME_MIN_INTERVAL,
                None => true,
            };
            if due {
                last_keyframe = Some(Instant::now());
                log::info!("client PLI/FIR: forcing keyframe");
                encoder.force_keyframe();
                // #62: an exhausted repeat budget means an idle desktop sends
                // nothing; a forced keyframe must still find an encode to ride
                // on, otherwise a PLI during a still picture is never answered.
                repeat_budget = REPEAT_ENCODE_MAX;
            } else {
                log::debug!("PLI coalesced: keyframe sent less than {KEYFRAME_MIN_INTERVAL:?} ago");
            }
        }
        // #56: `vb,<N>` from the settings panel -> re-target the encoder; a
        // non-zero override also pins the budget so the fps-scaled automatic
        // re-target below leaves the user's choice in place.
        let bitrate_override = handle.bitrate_override.swap(0, Ordering::SeqCst);
        if bitrate_override > 0 {
            log::info!("client requested bitrate -> {bitrate_override}kbps");
            encoder.set_bitrate_kbps(bitrate_override.clamp(200, 40_000));
        }
        // #50: re-read the live targets every beat (lock-free atomics).
        spf = settings.spf();
        let capture_timeout = capture_timeout_for(spf);
        let cur_fps = settings.fps();
        let (tw, th) = (settings.width(), settings.height());
        if cur_fps != last_fps {
            log::info!("client requested fps {cur_fps}: cadence re-based");
            last_fps = cur_fps;
            next_deadline = Instant::now() + spf;
            // #52: fps change re-targets the bitrate (30 fps is the base of the
            // configured `--bitrate`); a 60 fps session doubles the budget.
            // A #56 user override stays in place until the next `vb` request.
            if handle.bitrate_override.load(Ordering::SeqCst) == 0 {
                encoder.set_bitrate_kbps(bitrate_for(base_bitrate_kbps, cur_fps));
            }
        }
        if (tw, th) != (enc_w, enc_h) {
            log::info!(
                "client requested resolution {tw}x{th}: rebuilding capture/encoder pair"
            );
            return InnerOutcome::Rebuild { from_gdi: source.is_gdi() };
        }
        // Gate closed: idle beat without touching the capturer or encoder; the
        // deadline keeps advancing so a resume lands on a clean cadence.
        if !handle.is_on() {
            settle_beat(&mut next_deadline, spf);
            continue;
        }

        let beat = Instant::now();
        // Snapshot the backend before `frame()` borrows `&mut source`; the
        // match guards below must not call `source.is_gdi()` while the frame
        // borrow is alive.
        let on_gdi = source.is_gdi();
        // #59: a session parked on the GDI backend periodically re-opens the
        // capturer (DXGI-first in scrap) so a transient DXGI failure or an
        // idle-desktop misclassification never leaves the stream stuck on the
        // slow, artifact-prone software path. Rebuild is cheap and the outer
        // scheduler falls back to GDI again if DXGI is genuinely gone.
        if on_gdi && allow_gdi_retry {
            let now = Instant::now();
            let due = match last_gdi_retry {
                Some(t) => now.duration_since(t) >= GDI_RETRY_INTERVAL,
                // First beat on GDI: record the timestamp so the interval is
                // measured from the fallback itself, not from a stale reset.
                None => {
                    last_gdi_retry = Some(now);
                    false
                }
            };
            if due {
                log::info!("on gdi for a while: attempting DXGI restore");
                return InnerOutcome::Rebuild { from_gdi: true };
            }
        }

        let cap_start = Instant::now();
        let frame_res = source.frame(capture_timeout);
        // #63: how long the capturer actually blocked this beat, sampled on every
        // beat (including WouldBlock). If it stays near zero, the capture poll is
        // not the cost and the per-frame conversion/encode is.
        cap_wait_sum += cap_start.elapsed().as_secs_f64() * 1000.0;
        cap_wait_n += 1;
        let frame = match frame_res {
            Ok(frame) => frame,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // #59: an idle desktop WouldBlocks on nearly every beat — DXGI
                // intentionally yields no new frame while the screen is static
                // (an MC main menu can sit still for many seconds). That is
                // normal, not a fault: counting WouldBlocks toward a GDI
                // fallback misclassified healthy-but-idle captures as broken
                // and swapped the session onto the low-rate GDI backend, where
                // the stream showed color-smear artifacts. Only a real
                // GetFrame error (the recovery/rebuild chain below) moves the
                // session to GDI now.
                bump(&stats, |s| s.counters.would_block += 1);
                // R2 (#62): re-encode the cached YUV of the last frame. Sending
                // the previous *payload* again would double-apply its residual
                // against the decoder's reference frame (colour smear on any
                // moving content); a fresh encode of identical input yields a
                // near-empty residual and keeps the chain exact.
                // #63: the budget is large (600) because this path is what keeps
                // the frame cadence alive on a static desktop — see
                // `REPEAT_ENCODE_MAX`.
                if repeat_budget > 0 && !last_yuv.is_empty() {
                    repeat_budget -= 1;
                    let pts = start.elapsed().as_millis() as i64;
                    match encoder.encode(EncodeInput::YUV(&last_yuv), pts) {
                        Ok(Some(encoded)) => {
                            bump(&stats, |s| {
                                s.counters.repeat_ok += 1;
                                s.counters.repeat_frames += 1;
                            });
                            forward_chunks(&tx, encoded, &stats, &handle);
                        }
                        Ok(None) => bump(&stats, |s| s.counters.repeat_skipped += 1),
                        Err(err) => {
                            log::warn!("repeat encode failed: {err:#}");
                            bump(&stats, |s| s.counters.encode_err += 1);
                            repeat_budget = 0;
                        }
                    }
                } else {
                    bump(&stats, |s| s.counters.repeat_skipped += 1);
                }

                settle_beat(&mut next_deadline, spf);
                continue;
            }
            Err(e) if !on_gdi => {
                // Recoverable DXGI family: ConnectionReset / InvalidData / other
                // while still on the DXGI duplication path. Finite rebuild first,
                // permanent GDI switch after the window budget is exhausted.
                match recovery.next_attempt(beat) {
                    Some(n) => {
                        log::warn!("dxgi recovery attempt {n}/{DXGI_RECOVERY_LIMIT}: {e}");
                        handle.emit_event(PipelineEvent::RecoveryStart(n));
                        *abnormal = true;
                        return InnerOutcome::Rebuild { from_gdi: false };
                    }
                    None => {
                        source.set_gdi();
                        reason = "dxgi_recovery_exhausted".to_string();
                        log::warn!("fall back to gdi after recovery exhausted: {e}");
                        handle.emit_event(PipelineEvent::FallbackGdi(reason.clone()));
                        handle.emit_event(PipelineEvent::StreamInfo {
                            is_gdi: true,
                            reason: reason.clone(),
                        });
                        *abnormal = true;
                        gdi_err = 0;
                        settle_beat(&mut next_deadline, spf);
                        continue;
                    }
                }
            }
            Err(e) => {
                // Already on GDI: only consecutive GDI errors are fatal.
                gdi_err += 1;
                bump(&stats, |s| s.counters.capture_err = gdi_err);
                log::warn!("gdi capturer error #{gdi_err}: {e}");
                if gdi_err >= GDI_FATAL_LIMIT {
                    let rs = short_reason(&e);
                    handle.emit_event(PipelineEvent::Lost(format!("gdi_fatal: {rs}")));
                    return InnerOutcome::Fatal(format!(
                        "capturer failed under gdi fallback after {gdi_err} consecutive errors: {e:#}"
                    ));
                }
                settle_beat(&mut next_deadline, spf);
                continue;
            }
        };

        if !frame.valid() {
            bump(&stats, |s| s.counters.invalid += 1);
            settle_beat(&mut next_deadline, spf);
            continue;
        }

        // #63: how long the capturer actually blocked this beat. If this stays
        // near zero on WouldBlock beats, the capture poll is not the cost and the
        // per-frame conversion is.
        cap_wait_sum += cap_start.elapsed().as_secs_f64() * 1000.0;
        cap_wait_n += 1;

        // New frame resets the repeat / fatal counters.
        gdi_err = 0;
        repeat_budget = REPEAT_ENCODE_MAX;

        // #50+#51+#63: convert to YUV. A mutable native-resolution BGRA copy is
        // only required when the OS cursor must be composited (C1, pointer-lock
        // mode) or the #60 dump is armed; in the default path the frontend draws
        // its own cursor, so the scale/convert runs **directly on the captured
        // buffer**. That removes a 6.4 MB memcpy from every frame, which was a
        // measurable slice of the 60 fps beat budget on this VM.
        let cursor = update_cursor(&mut cursor_cache, &mut last_cursor_id, &handle);
        let native_len = (src_w * src_h * 4) as usize;
        let scale_needed = (src_w, src_h) != (tw, th);
        let cursor_on = handle.cursor_inject_on();
        let need_native_copy = cursor_on || dump_frame.is_some();
        last_scale = scale_needed;
        last_copy = need_native_copy;

        let Frame::PixelBuffer(pb) = &frame else {
            log::warn!("frame conversion requires a pixel buffer frame");
            bump(&stats, |s| s.counters.encode_err += 1);
            settle_beat(&mut next_deadline, spf);
            continue;
        };
        // #62: the buffer length is the only cheap invariant here, and
        // `copy_from_slice` panics on a mismatch — which used to kill the
        // blocking worker silently (frozen picture, live stats). A desktop resize
        // lands here before the outer scheduler knows about it, so treat it as a
        // rebuild trigger instead.
        if pb.width() != src_w as usize
            || pb.height() != src_h as usize
            || pb.data().len() < native_len
        {
            log::warn!(
                "frame geometry changed ({}x{}, {} bytes) vs {}x{} ({} bytes): rebuilding",
                pb.width(),
                pb.height(),
                pb.data().len(),
                src_w,
                src_h,
                native_len
            );
            bump(&stats, |s| s.counters.invalid += 1);
            return InnerOutcome::Rebuild { from_gdi: on_gdi };
        }

        let conv_start = Instant::now();
        // #57 fix: the conversion target must match the *encoded* frame (tw, th),
        // not whatever encoder the loop opened with; deriving it from the encoder
        // can lag a resolution handover and trips scrap's `src rect > dst rect`
        // guard (1920x1080 > 1718x926).
        let mut yuv_target = yuvfmt.clone();
        yuv_target.w = tw as usize;
        yuv_target.h = th as usize;

        let conv_result = if need_native_copy {
            scratch_bgra.resize(native_len, 0);
            scratch_bgra.copy_from_slice(&pb.data()[..native_len]);
            if scale_needed {
                scaled_buf.resize((tw * th * 4) as usize, 0);
                bgra_scale(
                    &scratch_bgra,
                    src_w as usize,
                    src_h as usize,
                    &mut scaled_buf,
                    tw as usize,
                    th as usize,
                );
                // #57: composite only when the frontend handed pointer rendering
                // over (pointer lock); otherwise it draws its native cursor and a
                // second composited pointer would appear.
                if cursor_on {
                    if let Some(cur) = &cursor {
                        paint_cursor(&mut scaled_buf, tw, th, src_w, src_h, cur);
                    }
                }
                let scaled_pb = PixelBuffer::with_BGRA(&scaled_buf, tw as usize, th as usize);
                convert_to_yuv(&scaled_pb, yuv_target, &mut yuv, &mut mid_data)
            } else {
                if cursor_on {
                    if let Some(cur) = &cursor {
                        paint_cursor(&mut scratch_bgra, src_w, src_h, src_w, src_h, cur);
                    }
                }
                let pb_view = PixelBuffer::with_BGRA(&scratch_bgra, src_w as usize, src_h as usize);
                convert_to_yuv(&pb_view, yuv_target, &mut yuv, &mut mid_data)
            }
        } else if scale_needed {
            scaled_buf.resize((tw * th * 4) as usize, 0);
            bgra_scale(
                &pb.data()[..native_len],
                src_w as usize,
                src_h as usize,
                &mut scaled_buf,
                tw as usize,
                th as usize,
            );
            let scaled_pb = PixelBuffer::with_BGRA(&scaled_buf, tw as usize, th as usize);
            convert_to_yuv(&scaled_pb, yuv_target, &mut yuv, &mut mid_data)
        } else {
            let pb_view = PixelBuffer::with_BGRA(&pb.data()[..native_len], src_w as usize, src_h as usize);
            convert_to_yuv(&pb_view, yuv_target, &mut yuv, &mut mid_data)
        };
        conv_sum += conv_start.elapsed().as_secs_f64() * 1000.0;
        conv_n += 1;
        if let Err(err) = conv_result {
            log::warn!("frame YUV conversion failed: {err:#}");
            bump(&stats, |s| s.counters.encode_err += 1);
            settle_beat(&mut next_deadline, spf);
            continue;
        }
        let input = EncodeInput::YUV(&yuv);
        // `input` borrows `frame`/`yuv` through the encode call below; the live
        // backend this beat already equals the `on_gdi` snapshot taken before
        // `frame()`, so query the capturer again only after encoding completes.

        // #60 diagnostic dump: attribute a smeared frame to capturer / scaler /
        // encoder by landing the pre-scale and post-scale BGRA buffers as BMP
        // files every `DUMP_EVERY` valid frame.
        if let Some(dir) = &dump_frame {
            dump_idx += 1;
            if dump_idx % DUMP_EVERY == 0 {
                let src_name = format!("pre-{dump_idx:05}-{src_w}x{src_h}.bmp");
                write_bgra_bmp(dir.join(src_name), &scratch_bgra, src_w as usize, src_h as usize);
                if (src_w, src_h) != (tw, th) {
                    let dst_name = format!("post-{dump_idx:05}-{tw}x{th}.bmp");
                    write_bgra_bmp(dir.join(dst_name), &scaled_buf, tw as usize, th as usize);
                }
                log::info!("#60 dump: {dump_idx} pre {src_w}x{src_h} + post {tw}x{th}");
            }
        }

        if *abnormal {
            *abnormal = false;
            handle.emit_event(PipelineEvent::Restored);
            handle.emit_event(PipelineEvent::StreamInfo {
                is_gdi: on_gdi,
                reason: reason.clone(),
            });
        }

        let enc_start = Instant::now();
        let pts_ms = start.elapsed().as_millis() as i64;
        let encoded = match encoder.encode(input, pts_ms) {
            Ok(Some(encoded)) => Some(encoded),
            Ok(None) => {
                let enc_ms = enc_start.elapsed().as_secs_f64() * 1000.0;
                bump(&stats, |s| {
                    s.counters.encode_no_output += 1;
                    s.encode_timing.add(enc_ms);
                });
                None
            }
            Err(err) => {
                log::warn!("new-frame encode failed: {err:#}");
                let enc_ms = enc_start.elapsed().as_secs_f64() * 1000.0;
                bump(&stats, |s| {
                    s.counters.encode_err += 1;
                    s.encode_timing.add(enc_ms);
                });
                None
            }
        };
        // R2 (#62): hand the just-converted YUV to the repeat path. The swap is
        // O(1) and leaves `yuv` holding the previous buffer, which the next
        // conversion overwrites in place (capacity is reused).
        std::mem::swap(&mut yuv, &mut last_yuv);

        if let Some(encoded) = encoded {
            let enc_ms = enc_start.elapsed().as_secs_f64() * 1000.0;
            let pipeline_ms = beat.elapsed().as_secs_f64() * 1000.0;
            bump(&stats, |s| {
                s.counters.new_frames += 1;
                s.counters.encoded_ok += 1;
                s.encode_timing.add(enc_ms);
                s.pipeline_timing.add(pipeline_ms);
                s.first_frame_seen = true;
                s.warmed_up = true;
            });
            // #60: encoded VP8 dump — a window of raw encoder output as an IVF
            // file, so `ffmpeg -i enc-*.ivf` can decode it off-line and prove
            // whether the artifact originates in the encoder output or later in
            // the RTP/transport path.
            if let Some(dir) = &dump_frame {
                if let Some(union) = &encoded.union {
                    if let base::message_proto::video_frame::Union::Vp8s(frames) = union {
                        for f in &frames.frames {
                            ivf_frames.push((f.data.to_vec(), f.pts));
                        }
                        if ivf_frames.len() >= IVF_WINDOW {
                            let name = format!("enc-{ivf_seq:04}.ivf");
                            write_ivf(
                                dir.join(name),
                                tw as usize,
                                th as usize,
                                settings.fps(),
                                &ivf_frames,
                            );
                            ivf_seq += 1;
                            ivf_frames.clear();
                        }
                    }
                }
            }
            forward_chunks(&tx, encoded, &stats, &handle);
        }

        settle_beat(&mut next_deadline, spf);
    }
}

/// Split one encoded `VideoFrame` into chunks and hand each to the async
/// forwarder through the bounded queue (#62). A full queue means the transport
/// is behind: the remaining chunks of this frame are dropped and a keyframe is
/// requested, so the receiver re-anchors instead of decoding a partial frame.
/// A closed queue means the session is tearing down.
fn forward_chunks(
    tx: &mpsc::Sender<Bytes>,
    encoded: base::message_proto::VideoFrame,
    stats: &Arc<Mutex<PipelineStats>>,
    handle: &PipelineHandle,
) {
    let chunks = video_frame_to_chunks(encoded);
    for chunk in chunks {
        match tx.try_send(chunk.data) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                bump(stats, |s| s.counters.write_sample_err += 1);
                handle.request_keyframe();
                break;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                bump(stats, |s| s.counters.write_sample_err += 1);
                break;
            }
        }
    }
}

/// Short critical section on the shared statistics lock. The closure never
/// crosses an await; poisoning from a prior panic is treated as empty data.
fn bump<F>(stats: &Arc<Mutex<PipelineStats>>, f: F)
where
    F: FnOnce(&mut PipelineStats),
{
    if let Ok(mut guard) = stats.lock() {
        f(&mut guard);
    }
}

/// Sleep the remainder of the beat; stop early when session cleanup sets the
/// stop flag so a teardown does not wait out a full backoff window.
fn sleep_with_stop(backoff: Duration, handle: &PipelineHandle) {
    let end = Instant::now() + backoff;
    while Instant::now() < end {
        if handle.stopped.load(Ordering::SeqCst) {
            return;
        }
        let remaining = end.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

/// R1 capture wait budget: fraction of one beat, so a WouldBlock + repeat
/// encode + settle sleep still fits inside the beat. Pure function kept out of
/// the loop for a unit test.
fn capture_timeout_for(spf: Duration) -> Duration {
    let micros = spf.as_micros() as u64;
    Duration::from_micros(micros.saturating_mul(CAPTURE_TIMEOUT_FRACTION) / 100)
}

/// R1 beat settle: sleep the remainder of the current deadline, then advance
/// the deadline by exactly `spf`. An overrun does not sleep but still snaps the
/// baseline to completion time + spf, so drift never accumulates.
/// C1/C2 (#51): refresh the cursor cache when the OS cursor identity changes;
/// emits the C2 `cursor` message (with a base64 PNG) on appearance / change
/// and a hide message (id 0) when the cursor disappears. Returns the cursor to
/// paint this beat, `None` when hidden or absent.
fn update_cursor(
    cache: &mut Option<CursorData>,
    last_id: &mut Option<u64>,
    handle: &PipelineHandle,
) -> Option<CursorData> {
    match cursor_info() {
        Ok(Some(id)) => {
            if *last_id != Some(id) {
                *last_id = Some(id);
                *cache = match cursor_data(id) {
                    Ok(Some(cd)) => Some(cd),
                    Ok(None) => None,
                    Err(err) => {
                        log::debug!("cursor bitmap capture failed: {err:#}");
                        None
                    }
                };
                if let Some(cd) = &*cache {
                    log::debug!("cursor changed -> id {id:#x} {}x{}", cd.width, cd.height);
                    handle.emit_event(PipelineEvent::Cursor(cd.clone()));
                }
            }
            cache.clone()
        }
        Ok(None) => {
            if last_id.is_some() {
                log::debug!("cursor hidden");
                *last_id = None;
                *cache = None;
                handle.emit_event(PipelineEvent::Cursor(CursorData {
                    id: 0,
                    colors: Vec::new(),
                    width: 0,
                    height: 0,
                    hotx: 0,
                    hoty: 0,
                }));
            }
            None
        }
        Err(err) => {
            log::debug!("cursor info failed: {err:#}");
            None
        }
    }
}

/// C1: paint the OS cursor at its hotspot position, translated from screen
/// coordinates into the final (possibly scaled) frame coordinates.
fn paint_cursor(
    buf: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    src_w: u32,
    src_h: u32,
    cur: &CursorData,
) {
    let (ox, oy) = virtual_origin();
    let (csx, csy) = cursor_pos();
    let hx = ((csx - ox) as i64 * dst_w as i64 / src_w as i64) as i32;
    let hy = ((csy - oy) as i64 * dst_h as i64 / src_h as i64) as i32;
    draw_cursor(
        buf,
        dst_w as usize * 4,
        dst_w as i32,
        dst_h as i32,
        hx - cur.hotx,
        hy - cur.hoty,
        cur,
    );
}

fn settle_beat(next_deadline: &mut Instant, spf: Duration) {
    let now = Instant::now();
    if now < *next_deadline {
        std::thread::sleep(*next_deadline - now);
    }
    let done = Instant::now();
    *next_deadline = done.max(*next_deadline) + spf;
}

/// #60: write a BGRA8 buffer as a 24-bpp bottom-up BMP (BGR rows, standard
/// file layout) for off-line inspection. Only a diagnostic helper: a malformed
/// frame here must never break the stream, so every failure is ignored.
fn write_bgra_bmp(path: PathBuf, data: &[u8], w: usize, h: usize) {
    use std::io::Write;
    if data.len() < w * h * 4 {
        return;
    }
    let row = w * 3;
    let padded = (row + 3) & !3;
    let size = 54 + padded * h;
    let mut buf = vec![0u8; size];
    buf[0] = b'B';
    buf[1] = b'M';
    buf[2..6].copy_from_slice(&(size as u32).to_le_bytes());
    buf[10..14].copy_from_slice(&54u32.to_le_bytes());
    buf[14..18].copy_from_slice(&40u32.to_le_bytes());
    buf[18..22].copy_from_slice(&(w as u32).to_le_bytes());
    buf[22..26].copy_from_slice(&(h as u32).to_le_bytes());
    buf[26..28].copy_from_slice(&1u16.to_le_bytes());
    buf[28..30].copy_from_slice(&24u16.to_le_bytes());
    for y in 0..h {
        let src = (h - 1 - y) * w * 4;
        let dst = 54 + y * padded;
        for x in 0..w {
            let s = src + x * 4;
            let d = dst + x * 3;
            buf[d] = data[s + 2]; // B
            buf[d + 1] = data[s + 1]; // G
            buf[d + 2] = data[s]; // R
        }
    }
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(&buf);
    }
}

/// #60: wrap a window of raw VP8 encoder frames as an IVF file (DKIF header,
/// one 4-byte size + 8-byte pts + payload record per frame) so ffmpeg's VP8
/// demuxer/decoder can play the exact bytes the pipeline produced. Write
/// failures are ignored — diagnostics must never break the stream.
fn write_ivf(path: PathBuf, w: usize, h: usize, fps: u32, frames: &[(Vec<u8>, i64)]) {
    use std::io::Write;
    let mut out = Vec::with_capacity(32 + frames.len() * 12 + frames.iter().map(|f| f.0.len()).sum::<usize>());
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"VP80");
    out.extend_from_slice(&(w as u16).to_le_bytes());
    out.extend_from_slice(&(h as u16).to_le_bytes());
    out.extend_from_slice(&fps.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (data, pts) in frames {
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(*pts as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(&out);
    }
}

/// Truncate an error for the frontend `system` action: 64 chars max, commas
/// replaced by semicolons so the comma-separated action whitelist stays stable.
fn short_reason(err: &dyn std::error::Error) -> String {
    let raw = err.to_string();
    raw.replace(',', ";").chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dxgi_budget_allows_three_then_blocks() {
        let mut state = DxgiRecoveryState::new();
        let now = Instant::now();
        assert_eq!(state.next_attempt(now), Some(1));
        assert_eq!(state.next_attempt(now), Some(2));
        assert_eq!(state.next_attempt(now), Some(3));
        assert_eq!(state.next_attempt(now), None);
    }

    #[test]
    fn dxgi_window_resets_after_expiry() {
        let mut state = DxgiRecoveryState::new();
        let now = Instant::now();
        assert_eq!(state.next_attempt(now), Some(1));
        assert_eq!(state.next_attempt(now), Some(2));
        let later = now + DXGI_RECOVERY_WINDOW + Duration::from_millis(1);
        assert_eq!(state.next_attempt(later), Some(1));
    }

    #[test]
    fn dxgi_window_keeps_count_inside_span() {
        let mut state = DxgiRecoveryState::new();
        let now = Instant::now();
        assert_eq!(state.next_attempt(now), Some(1));
        let inside = now + DXGI_RECOVERY_WINDOW - Duration::from_millis(1);
        assert_eq!(state.next_attempt(inside), Some(2));
    }

    #[test]
    fn short_reason_caps_length_and_escapes_commas() {
        let err = std::io::Error::new(
            std::io::ErrorKind::Other,
            "alpha, beta, gamma".repeat(20),
        );
        let s = short_reason(&err);
        assert!(s.len() <= 64);
        assert!(!s.contains(','));
        assert!(s.contains(';'));
    }

    #[test]
    fn capture_timeout_is_fraction_of_spf() {
        // 30 fps -> 33.3 ms -> 40% -> ~13.3 ms; the repeat + settle must fit
        // in the remaining ~20 ms of the beat.
        let spf = Duration::from_micros(33_333);
        let t = capture_timeout_for(spf);
        assert_eq!(t, Duration::from_micros(13_333));
        assert!(t < spf);
        // Zero / tiny spf must not panic or wrap.
        assert_eq!(capture_timeout_for(Duration::ZERO), Duration::ZERO);
        assert!(capture_timeout_for(Duration::from_micros(1)) >= Duration::ZERO);
    }
}
