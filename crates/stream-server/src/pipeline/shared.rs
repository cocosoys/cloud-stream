//! One capture/encode program, shared by every session attached to a display.
//!
//! Why a registry and not a field on the session
//! ---------------------------------------------
//! A pipeline used to be a session field (`SessionCtx::pipeline`), which was
//! only true because a session and a browser tab were the same thing: the
//! registry evicted the older socket on every new connection (close code 4001),
//! so there was never more than one pipeline to worry about.
//!
//! The cost of two pipelines is not CPU, it is the platform: `DuplicateOutput`
//! is exclusive per output, so the second `Capturer` on the same screen fails
//! with `DXGI_ERROR_NOT_CURRENTLY_AVAILABLE` and the recovery chain parks it on
//! GDI. Two sessions watching one desktop must therefore share one encoder and
//! one capture — which is what this registry hands out, reference counted, keyed
//! by the display it captures.
//!
//! Keying by [`DisplayKey`] is deliberate even though this build only opens the
//! primary display (`capture::CaptureSource::open_primary`): it is the seam the
//! multi-monitor work needs, and it costs nothing today. A session asking for
//! another display gets its own entry — and until `open_primary` learns to take
//! an index, that entry fails to open and reports it, which is the honest
//! behaviour rather than silently serving the wrong screen.
//!
//! Lifecycle: the first session to reach `Connected` creates the entry and
//! spawns the loop; later ones attach a sink and ask for a keyframe (a decoder
//! joining mid-stream cannot start on a P-frame). The last one out stops the
//! loop and drops the entry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::config::{Config, InputPolicy};
use crate::pipeline::fanout::{SinkHandle, SinkId, VideoFanout};
use crate::pipeline::{PipelineHandle, PipelineSnapshot, StreamSettings};
use crate::webrtc::VideoTrack;
// Only the production build starts a capture; the test build must not (see
// `start_program`).
#[cfg(not(test))]
use crate::pipeline::spawn_loop;

/// Which screen a pipeline captures.
///
/// `0` is the primary display, the only one this build can open. The index is
/// the DXGI output ordinal (`Display::all()` order), not a session-chosen id:
/// two sessions asking for the same screen must land on the same key or they
/// will fight over the duplication interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayKey(pub u32);

impl DisplayKey {
    /// The primary display — what every session gets today.
    pub const PRIMARY: Self = Self(0);
}

impl Default for DisplayKey {
    fn default() -> Self {
        Self::PRIMARY
    }
}

/// One capture + encoder + fan-out, with the count of sessions holding it.
pub struct SharedPipeline {
    pub display: DisplayKey,
    /// Gate / stats / keyframe requests. Shared, because there is one encoder:
    /// a resolution change moves the program, not one viewer's copy of it.
    pub handle: PipelineHandle,
    /// The program's live targets. Cloned (Arc inside) into every session, so a
    /// viewer's `server_settings` echo reports what the encoder is really doing
    /// rather than the CLI defaults it connected with.
    pub settings: StreamSettings,
    /// The downlinks. Attach on join, detach on leave.
    pub video: Arc<VideoFanout>,
    refs: AtomicUsize,
    /// Whether some session has claimed the 1 Hz stats window for this program.
    /// The counters are reset by the window's owner, so a second ticker would
    /// report roughly half the real rate for both (see `publish_snapshot`).
    stats_owner: AtomicBool,
    /// The most recent `stream_stats` window published by the owner.
    last_snapshot: Mutex<Option<PipelineSnapshot>>,
    /// Which session currently owns input on this program (#103).
    ///
    /// A property of the *program*, not a copy on each session, because the
    /// lease moves: a viewer that takes over must be able to claim it and the
    /// previous holder must start refusing input in the same instant. Two
    /// per-session booleans would need to be updated together and would race
    /// against the input hooks, which run on the data-channel task.
    ///
    /// `None` means nobody has claimed it yet — the window between `acquire`
    /// and the first session's `attach`. Input from any session is refused
    /// while it lasts, which is correct: there is no controller to hand it to.
    controller: Mutex<Option<u64>>,
    /// #105: what the lease above *means* for input.
    ///
    /// Lives on the program because that is the thing being injected into —
    /// two sessions on one program must read the same answer, and a future
    /// per-display override has an obvious place to land.
    input_policy: InputPolicy,
}

impl SharedPipeline {
    /// How many sessions hold this pipeline, this one included.
    pub fn refs(&self) -> usize {
        self.refs.load(Ordering::SeqCst)
    }

    /// The session that owns input on this program, if one has claimed it.
    pub fn controller_id(&self) -> Option<u64> {
        *self.controller.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// #105: whether every attached session may inject, or only the lease
    /// holder. Read per input message by the data-channel gates, so the answer
    /// has to be cheap and infallible — a `Copy` enum behind a plain field.
    pub fn input_policy(&self) -> InputPolicy {
        self.input_policy
    }

    /// Take the lease only if nobody holds it. Returns whether this session now
    /// owns it.
    ///
    /// #105: this is the repair for a lease that outlived its holder. Before
    /// it, the lease was set **only** by the session that created the program
    /// (`Acquired::created`), so a program that survived its creator — because
    /// another tab was still attached — passed to every later joiner with
    /// `controller == None`. Every tab then reported `role: "viewer"` and no
    /// input worked anywhere, with no way to recover short of restarting the
    /// service: reconnecting could not help, because a joiner never claims.
    ///
    /// Claim-if-free rather than claim-always so a real holder is never
    /// displaced by someone merely reconnecting; the compare-and-set happens
    /// under one lock, so two simultaneous joiners cannot both win.
    pub fn try_claim_controller(&self, session: u64) -> bool {
        let mut guard = self.controller.lock().unwrap_or_else(|p| p.into_inner());
        if guard.is_some() {
            return false;
        }
        *guard = Some(session);
        log::info!(
            "pipeline {}: input lease was unowned, claimed by session {session}",
            self.display.0
        );
        true
    }

    /// Move the input lease to `id`, returning whoever held it before.
    ///
    /// Deliberately unconditional: the caller decides the policy (first session
    /// in, an explicit operator takeover, a future idle-based handover), and
    /// this only records the outcome. Keeping the policy out of here is what
    /// makes an HTTP takeover possible without a second code path.
    ///
    /// It does *not* touch the sessions' own state: every input hook re-reads
    /// this value per message, so the switch is visible to both sides at once
    /// with no fan-out and no window where two sessions both believe they own
    /// the pointer.
    pub fn set_controller(&self, id: Option<u64>) -> Option<u64> {
        let mut guard = self.controller.lock().unwrap_or_else(|p| p.into_inner());
        // `mem::replace`, not `Option::replace`: the guard derefs to
        // `Option<u64>`, so the method would take the *new owner* and leave the
        // previous one dropped on the floor.
        let previous = std::mem::replace(&mut *guard, id);
        if previous != id {
            log::info!(
                "pipeline {}: input lease {:?} -> {:?}",
                self.display.0,
                previous,
                id
            );
        }
        previous
    }

    /// Attach one session's downlink and, for a join into a running program,
    /// force a keyframe.
    ///
    /// The keyframe is not optional: libvpx emits one IDR per `--keyframe-secs`
    /// (default 10 s) plus on demand, so a viewer that attached without asking
    /// would decode nothing but residuals against a reference it never saw —
    /// the classic "connected, black/garbage picture" bug. Requested rather than
    /// waited for, because `force_keyframe` lands on the *next* beat.
    pub fn attach(&self, track: VideoTrack, joining: bool) -> SinkHandle {
        let handle = self.video.attach(track);
        if joining {
            self.handle.request_keyframe();
            log::info!(
                "pipeline {}: viewer joined ({} sinks), keyframe requested",
                self.display.0,
                self.video.len()
            );
        }
        handle
    }

    /// Detach one downlink. Returns the number of sinks still attached.
    pub fn detach(&self, id: SinkId) -> usize {
        self.video.detach(id);
        self.video.len()
    }

    /// Claim the stats window for this program. Only the first caller gets
    /// `true`; everyone else reads [`Self::last_snapshot`].
    ///
    /// This is not politeness: `PipelineStats::snapshot` measures a window that
    /// is then zeroed (`reset_window`), so two tickers resetting each other's
    /// window would each report about half the frames — a wrong number that
    /// looks plausible, which is the worst kind.
    pub fn claim_stats(&self) -> bool {
        !self.stats_owner.swap(true, Ordering::SeqCst)
    }

    /// Publish a window for the other sessions (and for `GET /api/v1/health`).
    pub fn publish_snapshot(&self, snapshot: PipelineSnapshot) {
        let mut guard = self.last_snapshot.lock().unwrap_or_else(|p| p.into_inner());
        *guard = Some(snapshot);
    }

    /// The last window the owner published, if any.
    pub fn last_snapshot(&self) -> Option<PipelineSnapshot> {
        *self.last_snapshot.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A program with no capture and no downlinks, for tests that only read its
    /// handle and settings (the health verdicts).
    ///
    /// `start_program` is a no-op under `cfg(test)`, so this is the same object
    /// `acquire` builds — minus the frames that a unit test must not produce.
    ///
    /// The policy is a parameter rather than `Config::default()`'s value: the
    /// role tests must be able to build the strict program without flipping a
    /// global, and `exclusive` is the only mode in which `role` means anything.
    #[cfg(test)]
    pub fn stub(
        handle: PipelineHandle,
        settings: StreamSettings,
        policy: InputPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            display: DisplayKey::PRIMARY,
            handle,
            settings,
            video: Arc::new(VideoFanout::new()),
            refs: AtomicUsize::new(1),
            stats_owner: AtomicBool::new(false),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: policy,
        })
    }
}

/// Result of [`PipelineRegistry::acquire`].
pub struct Acquired {
    pub pipeline: Arc<SharedPipeline>,
    /// `true` when this call created the program — i.e. this session is the one
    /// that opened the capture, owns the settings and may inject input.
    pub created: bool,
}

/// The process-wide map from display to the pipeline capturing it.
#[derive(Clone, Default)]
pub struct PipelineRegistry {
    /// A tokio mutex: `acquire` spawns the capture loop and `release` stops it,
    /// and both must not interleave with each other or a stop would land on the
    /// entry another session just created.
    inner: Arc<AsyncMutex<HashMap<DisplayKey, Arc<SharedPipeline>>>>,
}

impl PipelineRegistry {
    /// Attach to the pipeline for `display`, creating and starting it when this
    /// is the first session for that screen.
    pub async fn acquire(&self, display: DisplayKey, cfg: &Arc<Config>) -> Acquired {
        let mut guard = self.inner.lock().await;
        if let Some(existing) = guard.get(&display) {
            existing.refs.fetch_add(1, Ordering::SeqCst);
            log::info!(
                "pipeline {}: session attached to the running program ({})",
                display.0,
                describe(existing)
            );
            return Acquired {
                pipeline: existing.clone(),
                created: false,
            };
        }

        let settings = StreamSettings::from_config(cfg);
        let handle = PipelineHandle::new();
        let video = Arc::new(VideoFanout::new());
        start_program(cfg, &video, &handle, &settings);
        let entry = Arc::new(SharedPipeline {
            display,
            handle,
            settings,
            video,
            refs: AtomicUsize::new(1),
            stats_owner: AtomicBool::new(false),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: cfg.input_policy,
        });
        guard.insert(display, entry.clone());
        log::info!("pipeline {}: program created, capture loop spawned", display.0);
        Acquired {
            pipeline: entry,
            created: true,
        }
    }

    /// Drop this session's hold. The last one out stops the loop so a closed
    /// browser does not leave a capturer running (DXGI would refuse the next
    /// one) and so `stream_stats` cannot report an idle encoder forever.
    pub async fn release(&self, display: DisplayKey) {
        let mut guard = self.inner.lock().await;
        let Some(entry) = guard.get(&display).cloned() else {
            return;
        };
        let live = entry.refs.load(Ordering::SeqCst);
        if live <= 1 {
            guard.remove(&display);
            entry.handle.stop_loop();
            log::info!(
                "pipeline {}: last session left, capture loop stopped",
                display.0
            );
            return;
        }
        entry.refs.fetch_sub(1, Ordering::SeqCst);
        log::info!(
            "pipeline {}: session released ({} holders left, {} sinks)",
            display.0,
            entry.refs(),
            entry.video.len()
        );
    }

    /// The live program for `display`.
    ///
    /// The health endpoint reads [`Self::all`] instead (it reports every
    /// program), so this is the lookup the multi-monitor step needs: "is this
    /// screen already being captured, and may I join it".
    #[allow(dead_code)]
    pub async fn get(&self, display: DisplayKey) -> Option<Arc<SharedPipeline>> {
        self.inner.lock().await.get(&display).cloned()
    }

    /// Every live program, ordered by display.
    pub async fn all(&self) -> Vec<Arc<SharedPipeline>> {
        let guard = self.inner.lock().await;
        let mut out: Vec<_> = guard.values().cloned().collect();
        out.sort_by_key(|p| p.display);
        out
    }
}

fn describe(p: &SharedPipeline) -> String {
    format!(
        "{} holders, {} downlinks, gate {}",
        p.refs(),
        p.video.len(),
        if p.handle.is_on() { "open" } else { "closed" }
    )
}

/// Start the capture/encode program.
///
/// A unit test must not: `DuplicateOutput` is exclusive on the console session,
/// it would fight the developer's own stream, and the blocking worker outlives
/// the test. Everything the registry tests assert (refcount, sharing, teardown)
/// is reachable without a single captured frame.
#[cfg(not(test))]
fn start_program(
    cfg: &Arc<Config>,
    video: &Arc<VideoFanout>,
    handle: &PipelineHandle,
    settings: &StreamSettings,
) {
    spawn_loop(cfg.clone(), video.clone(), handle.clone(), settings.clone());
}

#[cfg(test)]
fn start_program(
    _cfg: &Arc<Config>,
    _video: &Arc<VideoFanout>,
    _handle: &PipelineHandle,
    _settings: &StreamSettings,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::codec::VideoCodec;

    fn cfg() -> Arc<Config> {
        // Defaults only: `acquire` reads nothing but the CLI stream seeds, and
        // the tests never start the capture (see `start_program`).
        Arc::new(Config::default())
    }

    #[tokio::test]
    async fn the_second_session_joins_the_same_program() {
        let registry = PipelineRegistry::default();
        let first = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        assert!(first.created, "the first session must open the capture");
        let second = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        assert!(
            !second.created,
            "a second session on the same display must not open a second capture"
        );
        assert_eq!(second.pipeline.refs(), 2);
        assert!(
            Arc::ptr_eq(&first.pipeline, &second.pipeline),
            "both sessions must share one handle, one settings object and one fanout"
        );
    }

    #[tokio::test]
    async fn the_program_survives_until_the_last_session_leaves() {
        let registry = PipelineRegistry::default();
        let first = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        let _second = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        registry.release(DisplayKey::PRIMARY).await;
        assert!(
            registry.get(DisplayKey::PRIMARY).await.is_some(),
            "one holder left: the capture must keep running"
        );
        assert_eq!(first.pipeline.refs(), 1);
        registry.release(DisplayKey::PRIMARY).await;
        assert!(
            registry.get(DisplayKey::PRIMARY).await.is_none(),
            "no holder left: the entry must be dropped"
        );
        assert!(!first.pipeline.handle.is_on());
    }

    #[tokio::test]
    async fn releasing_twice_does_not_underflow() {
        // A teardown path can run twice (PC terminal state + WS loop exit); the
        // second release must be a no-op rather than wrapping the counter.
        let registry = PipelineRegistry::default();
        let first = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        registry.release(DisplayKey::PRIMARY).await;
        registry.release(DisplayKey::PRIMARY).await;
        assert!(registry.get(DisplayKey::PRIMARY).await.is_none());
        assert!(!first.pipeline.handle.is_on());
    }

    #[tokio::test]
    async fn displays_get_independent_programs() {
        let registry = PipelineRegistry::default();
        let primary = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        let second = registry.acquire(DisplayKey(1), &cfg()).await;
        assert!(second.created, "another display is another capture");
        assert!(!Arc::ptr_eq(&primary.pipeline, &second.pipeline));
        assert_eq!(registry.all().await.len(), 2);
    }

    #[test]
    fn only_one_session_owns_the_stats_window() {
        // Built directly: the claim is synchronous and needs no runtime.
        let entry = SharedPipeline {
            display: DisplayKey::PRIMARY,
            handle: PipelineHandle::new(),
            settings: StreamSettings::from_config(&cfg()),
            video: Arc::new(VideoFanout::new()),
            refs: AtomicUsize::new(1),
            stats_owner: AtomicBool::new(false),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: InputPolicy::default(),
        };
        assert!(entry.claim_stats());
        assert!(!entry.claim_stats(), "a second ticker must read, not reset");
        assert!(entry.last_snapshot().is_none());
        let mut snap = PipelineSnapshot::default();
        snap.encoded_fps = 59.5;
        entry.publish_snapshot(snap);
        assert_eq!(entry.last_snapshot().map(|s| s.encoded_fps), Some(59.5));
    }

    #[test]
    fn attaching_a_viewer_asks_for_a_keyframe() {
        let entry = SharedPipeline {
            display: DisplayKey::PRIMARY,
            handle: PipelineHandle::new(),
            settings: StreamSettings::from_config(&cfg()),
            video: Arc::new(VideoFanout::new()),
            refs: AtomicUsize::new(1),
            stats_owner: AtomicBool::new(false),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: InputPolicy::default(),
        };
        let track = VideoTrack::new(VideoCodec::Vp8).expect("VP8 track");
        let handle = entry.attach(track, false);
        assert_eq!(entry.video.len(), 1);
        assert_eq!(entry.detach(handle.id()), 0);
    }

    /// #103: the lease is a property of the program, and handing it over must
    /// be visible to both sides without any per-session bookkeeping.
    #[test]
    fn the_input_lease_moves_and_reports_the_previous_holder() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        assert_eq!(entry.controller_id(), None, "nobody owns it before attach");
        assert_eq!(entry.set_controller(Some(7)), None);
        assert_eq!(entry.controller_id(), Some(7));
        assert_eq!(entry.set_controller(Some(9)), Some(7));
        assert_eq!(entry.controller_id(), Some(9));
        // Releasing the lease leaves the program without an owner, which is the
        // state after the last session leaves; input is refused until someone
        // claims it again.
        assert_eq!(entry.set_controller(None), Some(9));
        assert_eq!(entry.controller_id(), None);
    }

    /// #105：程序比它的创建者活得久时，租约不能永久空着。
    ///
    /// 线上形态：session 2 建了程序并持有租约，它断开时同屏没有别的已 attach
    /// 会话（另一个页签当时还没走到 Connected），`successor` 返回 None，租约变
    /// 空但程序还在。之后 6、7 进来都是 `created == false`，旧代码里没人再写过
    /// `controller`，于是两块屏都成了 viewer、谁按都没反应、重连也修不好。
    #[test]
    fn an_unowned_lease_is_claimed_by_the_next_joiner_and_only_once() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        assert!(entry.try_claim_controller(6), "an unowned lease must be claimable");
        assert_eq!(entry.controller_id(), Some(6));
        assert!(
            !entry.try_claim_controller(7),
            "a reconnecting tab must not take the lease from a live holder"
        );
        assert_eq!(entry.controller_id(), Some(6));
        // The holder leaving leaves it unowned again — the next attach repairs it.
        entry.set_controller(None);
        assert!(entry.try_claim_controller(7));
        assert_eq!(entry.controller_id(), Some(7));
    }
}
