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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::config::{Config, InputPolicy};
use crate::pipeline::audio::AudioFanout;
use crate::pipeline::fanout::{SinkHandle, SinkId, VideoFanout};
use crate::pipeline::{PipelineHandle, PipelineSnapshot, StreamPrefs, StreamSettings};
use crate::webrtc::VideoTrack;
// Only the production build starts a capture; the test build must not (see
// `start_program`).
#[cfg(not(test))]
use crate::pipeline::spawn_loop;

/// How long a stats-window owner keeps its claim without publishing.
///
/// The tick is 1 s, so three missed ticks means the owner is gone (a closed
/// tab, a torn-down session, a task the runtime dropped). Short enough that a
/// reconnect gets its panel back; long enough that a slow tick does not hand
/// the window to a second ticker mid-second.
const STATS_LEASE: std::time::Duration = std::time::Duration::from_secs(3);

/// Which screen a pipeline captures.
///
/// `0` is the primary display. The index is the ordinal of
/// `capture::ordered_displays()` — primary first, then the rest in DXGI order —
/// not a session-chosen id: two sessions asking for the same screen must land
/// on the same key or they will fight over the duplication interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayKey(pub u32);

impl DisplayKey {
    /// The primary display — the default for a session that says nothing.
    pub const PRIMARY: Self = Self(0);

    /// Map the hello's `display_id` onto an index.
    ///
    /// The page sends `primary`, `display2`, `display3`… (`signaling.js` builds
    /// the string from the thumbnail the user clicked). `displayN` means index
    /// `N-1`, so `display2` is the second screen — the same off-by-one the
    /// frontend uses. Unknown values fall back to the primary: an old page or a
    /// typo should still get a picture, and reloading fixes a wrong screen,
    /// whereas refusing the hello strands the user on a black tab.
    pub fn from_hello(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("primary") {
            return Self::PRIMARY;
        }
        let Some(rest) = raw
            .strip_prefix("display")
            .or_else(|| raw.strip_prefix("DISPLAY"))
        else {
            // A bare number is accepted too: `--display 1` users tend to type
            // the index directly into config files.
            return match raw.parse::<u32>() {
                Ok(n) => Self(n),
                Err(_) => Self::PRIMARY,
            };
        };
        match rest.parse::<u32>() {
            Ok(n) if n >= 1 => Self(n - 1),
            // `display1` would be index 0 spelled wrong; treat it as primary
            // instead of wrapping to u32::MAX - 1.
            _ => Self::PRIMARY,
        }
    }

    /// The page's name for this screen — the inverse of [`Self::from_hello`].
    ///
    /// Needed downstream because the page matches on the *name*, not the index:
    /// `PIPELINE_RESETTING` is ignored unless the id in it equals the page's own
    /// `displayId` (`selkies-ws-core.js:8996`), so sending `1` there resets
    /// nothing and the decoder keeps a stale reference.
    pub fn hello_name(&self) -> String {
        match self.0 {
            0 => "primary".to_owned(),
            n => format!("display{}", n + 1),
        }
    }
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
    /// #108: the audio downlinks. One loopback capture + one Opus encode feeds
    /// every attached track and every socket, exactly like `video` above.
    pub audio: Arc<AudioFanout>,
    /// Set when the program is torn down; the audio thread polls it.
    audio_stop: Arc<std::sync::atomic::AtomicBool>,
    refs: AtomicUsize,
    /// Until when some session holds the 1 Hz stats window for this program.
    ///
    /// A lease rather than a one-way flag: the counters are zeroed by the
    /// window's owner, so two tickers must not both snapshot — but a flag that
    /// never clears also means the *first* session to ask owns the window
    /// forever, and everyone after it reads a snapshot that stopped updating
    /// the moment that session closed. The owner refreshes the lease every time
    /// it publishes, so a live owner keeps it and a departed one lets it lapse.
    stats_lease: Mutex<Option<std::time::Instant>>,
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
    /// #113: 租约持有者**最近一次真的投进输入**的时刻。
    ///
    /// 租约以前只有"谁认领了"，没有"还在不在用"。`exclusive` 下这是个永久
    /// 所有权：claim-if-free、没人能挤掉、收尾时才归还 —— 于是任何一条**活着
    /// 但不再投输入**的连接都会把指针锁死到进程重启为止。切换传输就是这条路：
    /// 页面换到 WebSocket 之后，上一次的 WebRTC 会话还挂在注册表里（它的
    /// PeerConnection 没拆、下行的 sink 还在收字节），租约归它、新的 media
    /// 连接每次认领都撞车。
    ///
    /// 症状（实测日志）：`ws media … lease=no` + 页面上"指针跟着鼠标动、
    /// 悬停和点击全无反馈" —— 指针是页面自己画的，桌面一个事件都没收到。
    ///
    /// 所以租约必须能失效。判据不用猜身份（tab id 在 `SETTINGS` 里，太晚），
    /// 只用一条硬事实：**持有者多久没投过输入了**。`None` 表示程序建起来之后
    /// 一次都没投过 —— 那和悬空等价。
    input_at: Mutex<Option<std::time::Instant>>,
}

/// #113: 新连接落座时，从一条"没人用"的租约手里接过来。
///
/// 1.5 s 而不是 0：同一页面的两个页签是常态，先到的那个正在用（每几毫秒一条
/// `m,`），第二个页签不该把它顶掉；而换传输时旧会话最后一次输入至少是几秒前
/// （页面重载、协商、首帧），1.5 s 足够分辨这两件事。
pub const LEASE_ADOPT_IDLE: std::time::Duration = std::time::Duration::from_millis(1500);

/// #113: 每收一条输入时的兜底 —— 持有者静默这么久就把租约接过来。
///
/// 比 [`LEASE_ADOPT_IDLE`] 宽，因为这条路径是"抢活人"：正常两页签场景里，
/// 用户晃一下鼠标就该轮到他，而不是等 3 s。3 s 的选择是"人停手读屏幕"的量级，
/// 再短会在两个同时打开的页签之间来回跳。
pub const LEASE_TAKEOVER_IDLE: std::time::Duration = std::time::Duration::from_millis(3000);

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

    /// #110: whether `session` may act on this program right now.
    ///
    /// The single answer both transports ask: `ControlGate::owns()` on the
    /// WebRTC data channels and the inline checks in `ws_media` used to spell
    /// this match out twice, which is two places to forget when the policy
    /// grows a third variant. `shared` means everybody (the pre-#110 default),
    /// `exclusive` means the lease holder only.
    pub fn owns_input(&self, session: u64) -> bool {
        match self.input_policy {
            InputPolicy::Shared => true,
            InputPolicy::Exclusive => self.controller_id() == Some(session),
        }
    }

    /// #112: 输入门禁，带**自愈**。
    ///
    /// `owns_input` 只回答"现在是不是你的"。租约一旦因为持有者消失而悬空 ——
    /// socket 半开、页面换传输那几十秒、某条收尾路径忘了归还 —— 它就永远回答
    /// `false`，而 `try_claim_controller` 是 claim-if-free：只有**主动去认领**
    /// 的人才能把它救回来，而一条连接只在 `acquire` 之后认领一次。
    ///
    /// 症状很有欺骗性：画面照流、页面把自己画的指针跟着鼠标挪，桌面却一点反应
    /// 都没有（键鼠全被门禁丢掉，`denied` 在涨），而且重连治不好。用户看到的就是
    /// "指针是叠上去的，悬停没有反馈"。
    ///
    /// 所以每收一条输入就顺手补一次：空着就归我。活着的主人抢不走 ——
    /// `try_claim_controller` 的 compare-and-set 语义一个字没改。
    ///
    /// #113: "活着的主人"不等于"在用的人"。换传输之后旧会话既活着、又握着租约，
    /// 却再也不会投一条输入 —— 这时 claim-if-free 就是一道锁。所以再加第三档：
    /// 持有者已经静默 [`LEASE_TAKEOVER_IDLE`] 就把租约接过来。接管是**显式**的
    /// （走 `set_controller`，日志里留一行 `holder idle …`），不是绕过 CAS。
    ///
    /// 这里**不盖时间戳**：门禁也被控制通道的每条消息问一次（`pong`、`SETTINGS`
    /// 回执），在那里盖章会把一条只会回 `pong` 的死会话永远喂热，失效逻辑就白
    /// 做了。盖章由真正的注入点做（见 [`Self::note_input`]），而接管只允许从
    /// 真正的输入发起（`is_input`）。
    pub fn may_inject(&self, session: u64, may_operate: bool, is_input: bool) -> bool {
        self.may_inject_within(session, may_operate, is_input, LEASE_TAKEOVER_IDLE)
    }

    /// [`Self::may_inject`] 的本体，阈值由调用方给 —— 单测据此构造时间线，
    /// 生产代码永远走上面那个常量。
    fn may_inject_within(
        &self,
        session: u64,
        may_operate: bool,
        is_input: bool,
        idle_limit: std::time::Duration,
    ) -> bool {
        if !may_operate {
            return false;
        }
        if self.owns_input(session) {
            return true;
        }
        if self.try_claim_controller(session) {
            return true;
        }
        // 只有"我正在投输入"才有资格把租约从一条没人用的持有者手里接过来。
        // 控制类指令（`pong`、`_stats`、`SETTINGS`）不参与：它们不驱动指针，
        // 让它们触发接管只会在日志里造出"有人接管了但什么都没发生"的假象。
        if !is_input {
            return false;
        }
        if self.lease_is_stale(idle_limit) {
            let idle = self.input_idle();
            let previous = self.set_controller(Some(session));
            log::info!(
                "pipeline {}: input lease {previous:?} -> Some({session}) (holder idle {}, taking over)",
                self.display.0,
                match idle {
                    Some(d) => format!("{:.1}s", d.as_secs_f32()),
                    None => String::from("since the program started"),
                }
            );
            // 接管即"从现在起是我在驱动"，盖一次时间戳。不盖的话，两条都不投
            // 输入的连接会在每一条消息上互相抢 —— 每一次都以为自己更该动手，
            // 结果两边的输入各丢一半。
            self.note_input();
            return true;
        }
        false
    }

    /// #113: 这条租约还算不算"有人用"。
    ///
    /// `None`（程序起来之后一次输入都没有）**与陈旧等价**：一个认领了却从不
    /// 注入的持有者，和悬空的租约对用户是同一种结果 —— 指针不动。把它当"活着"
    /// 处理，就是两条各握着半边锁的连接互相等，谁也投不进去。
    fn lease_is_stale(&self, limit: std::time::Duration) -> bool {
        match self.input_idle() {
            None => true,
            Some(idle) => idle >= limit,
        }
    }

    /// #113: 记下"这条连接刚刚真的投了一条输入"。
    ///
    /// 只在注入点调用 —— WS 的 `Action::InputStub` 分支和 WebRTC 指针通道在
    /// `InputStub` 之后各一次。时间戳只服务一件事：让租约能失效。
    pub fn note_input(&self) {
        *self.input_at.lock().unwrap_or_else(|p| p.into_inner()) = Some(std::time::Instant::now());
    }

    /// #113: 租约持有者静默了多久。`None` = 程序起来之后一次输入都没有过，
    /// 与"悬空"同义。
    pub fn input_idle(&self) -> Option<std::time::Duration> {
        self.input_at
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|t| t.elapsed())
    }

    /// #113: 新连接落座时调用**一次** —— 租约若是一条没人用的，接过来。
    ///
    /// 与 [`Self::may_inject`] 的兜底是两件事，都需要：
    /// - 这条路径是"落座即接管"，判决依据是**旧持有者的输入年龄**，所以换传输
    ///   之后的第一下鼠标就能生效，不用等 3 s 的兜底，也不用等用户动第二次；
    /// - 兜底那条处理"落座时还没到 1.5 s、之后才彻底静下来"的情形。
    ///
    /// 返回是否接管了。`max_idle` 由调用方给（见 [`LEASE_ADOPT_IDLE`]），单测
    /// 才好构造两条时间线。
    pub fn adopt_lease_if_stale(&self, session: u64, max_idle: std::time::Duration) -> bool {
        if self.controller_id() == Some(session) {
            return false;
        }
        if !self.lease_is_stale(max_idle) {
            return false;
        }
        let previous = self.set_controller(Some(session));
        if previous.is_some() {
            log::info!(
                "pipeline {}: input lease {previous:?} -> Some({session}) (holder was not injecting; new connection adopted it)",
                self.display.0
            );
        }
        // 接管**不**盖时间戳。盖了就等于宣称"这条连接在用"，而它可能一次都
        // 没投过 —— 页面加载时同一页会连两条（信令 + 媒体），盖了的话先落座的
        // 那条会把后落座的那条挡在门外，而真正投输入的恰恰是后者。
        true
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

    /// Claim (or keep) the stats window for this program.
    ///
    /// Returns `true` while this caller owns the window; everyone else reads
    /// [`Self::last_snapshot`]. The owner extends the lease by calling this,
    /// or — which is what the running code does — by publishing.
    ///
    /// This is not politeness: `PipelineStats::snapshot` measures a window that
    /// is then zeroed (`reset_window`), so two tickers resetting each other's
    /// window would each report about half the frames — a wrong number that
    /// looks plausible, which is the worst kind.
    pub fn claim_stats(&self) -> bool {
        let now = std::time::Instant::now();
        let mut lease = self.stats_lease.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(until) = *lease {
            if until > now {
                // Someone else holds it and is still alive: read, do not reset.
                return false;
            }
        }
        *lease = Some(now + STATS_LEASE);
        true
    }

    /// Publish a window for the other sessions (and for `GET /api/v1/health`),
    /// and keep the lease alive: a publisher is by definition still ticking.
    pub fn publish_snapshot(&self, snapshot: PipelineSnapshot) {
        {
            let mut lease = self.stats_lease.lock().unwrap_or_else(|p| p.into_inner());
            *lease = Some(std::time::Instant::now() + STATS_LEASE);
        }
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
            audio: Arc::new(AudioFanout::new()),
            audio_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refs: AtomicUsize::new(1),
            stats_lease: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: policy,
            input_at: Mutex::new(None),
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
    /// 每块屏**上一个**程序的操作参数，新建程序时继承。
    ///
    /// 只在"最后一个会话离开、程序被摘掉"那一刻写：程序活着的时候它自己的
    /// `settings` 就是真相，注册表里的旧值没有意义。没有这一份，切换编码器
    /// 就会失败 —— 选中的编码器会强制断开那条唯一的连接，新程序从 `Config`
    /// 重建回 CLI 默认（VP8），页面再推一次 VP9、再被断开，循环到超时。
    /// 详见 [`StreamPrefs`]。
    ///
    /// 用普通 `Mutex` 而不是 `AsyncMutex`：临界区只有一次 HashMap 读写，没有
    /// `.await`，且 `acquire` / `release` 都是在持有 `inner` 时顺带看一眼，
    /// 两处加锁顺序一致（inner → prefs），不会反向嵌套。
    prefs: Arc<Mutex<HashMap<DisplayKey, StreamPrefs>>>,
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

        // The settings carry the display this program captures: the loop reads
        // it to open the right output, and the input sink reads it to place the
        // pointer. `from_config` seeds the CLI default; the key this call was
        // made with is the truth.
        let mut settings = StreamSettings::from_config(cfg);
        // 这块屏上一个程序留下的操作参数。第一次开这块屏时没有记录，那就是
        // CLI 默认值 —— 与修复前行为一致，只有"曾经有人改过"才不同。
        //
        // 这一行是"切换编码器不再无限重连"的关键：换编码器会断开唯一那条连接，
        // 程序随之被摘掉，若不带回上一次的选择，新程序就是 VP8，页面再推 VP9、
        // 再被断开。详见 `StreamPrefs`。
        let inherited = self
            .prefs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&display)
            .copied();
        if let Some(prefs) = inherited {
            settings.apply_prefs(prefs);
            log::info!(
                "pipeline {}: inheriting the previous program's settings (codec {}, {} fps, {}x{}, audio {} @ {} bps)",
                display.0,
                prefs.codec.wire(),
                prefs.fps,
                prefs.width,
                prefs.height,
                if prefs.audio_enabled { "on" } else { "off" },
                prefs.audio_bps
            );
        }
        settings.set_display(display.0);
        let handle = PipelineHandle::new();
        let video = Arc::new(VideoFanout::new());
        start_program(cfg, &video, &handle, &settings);
        // #108: one loopback for the whole program. It starts with the program
        // (the first `Connected`) and stops with the last holder, so a second
        // viewer adds a sink rather than a second WASAPI client and encoder.
        let audio = Arc::new(AudioFanout::new());
        let audio_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        start_audio(&audio, &audio_stop, &settings);
        let entry = Arc::new(SharedPipeline {
            display,
            handle,
            settings,
            video,
            audio,
            audio_stop,
            refs: AtomicUsize::new(1),
            stats_lease: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: cfg.input_policy,
            input_at: Mutex::new(None),
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
            // 程序要走了，把它这一轮的操作参数留给下一个（见 `StreamPrefs`）。
            // 先读再摘：`entry` 是 Arc，摘掉之后对象还在手上，但"摘之前读"让
            // 这段的意图不用猜 —— 值取自即将销毁的那个程序，不是之后补的。
            let carried = entry.settings.prefs();
            guard.remove(&display);
            self.prefs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(display, carried);
            entry.handle.stop_loop();
            entry.audio_stop.store(true, Ordering::SeqCst);
            log::info!(
                "pipeline {}: last session left, capture loop stopped (settings kept for the next program: codec {})",
                display.0,
                carried.codec.wire()
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

/// Start the shared audio downlink (production only — a unit test must not open
/// the console session's audio endpoint, and the thread outlives the test).
///
/// #110: the two atomics come from the program's settings, not from
/// process-wide statics, so this thread's mute button and bitrate slider belong
/// to the screen it is capturing.
#[cfg(not(test))]
fn start_audio(
    audio: &Arc<AudioFanout>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
    settings: &StreamSettings,
) {
    let (bps, enabled) = settings.audio_handles();
    crate::pipeline::audio::spawn(audio.clone(), stop.clone(), bps, enabled);
}

#[cfg(test)]
fn start_audio(
    _audio: &Arc<AudioFanout>,
    _stop: &Arc<std::sync::atomic::AtomicBool>,
    _settings: &StreamSettings,
) {
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

    /// 这是 "WebSocket disconnected. Attempting to reconnect..." 的回归测试。
    ///
    /// 换编码器一定会断开当前那条唯一的连接（m-line 与 libvpx 上下文是握手期
    /// 定下的），于是这个会话成了最后一个持有者、程序被摘掉。如果新程序不继承
    /// 上一次的选择，页面重连后会再推一次同样的选择、再被断开 —— 页面永远停在
    /// 重连提示上，一帧都看不到。
    #[tokio::test]
    async fn a_codec_pick_outlives_the_reconnect_it_forces() {
        let registry = PipelineRegistry::default();
        let first = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        assert_eq!(
            first.pipeline.settings.codec(),
            VideoCodec::Vp9,
            "#117: CLI 默认已是 vp9"
        );
        assert!(first.pipeline.settings.set_codec(VideoCodec::Vp8));
        // 顺手改两个别的参数，确认继承的是整份快照而不是只有编码器。
        first.pipeline.settings.set_fps(45);
        first.pipeline.settings.set_resolution(1280, 720);
        first.pipeline.settings.set_audio_enabled(false);

        // 页面切编码器后服务端关掉这条连接：它正是最后一个持有者。
        registry.release(DisplayKey::PRIMARY).await;
        assert!(registry.get(DisplayKey::PRIMARY).await.is_none());

        let second = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        assert!(second.created, "程序已被摘掉，这次必须重新开一路");
        assert_eq!(
            second.pipeline.settings.codec(),
            VideoCodec::Vp8,
            "新程序必须继承上一次的编码器选择"
        );
        assert_eq!(second.pipeline.settings.fps(), 45);
        assert_eq!(second.pipeline.settings.width(), 1280);
        assert_eq!(second.pipeline.settings.height(), 720);
        assert!(!second.pipeline.settings.audio_enabled());
    }

    /// 另一个方向：继承是按屏记的，别把主屏的选择带给第二块屏。
    #[tokio::test]
    async fn a_pick_is_not_inherited_across_displays() {
        let registry = PipelineRegistry::default();
        let primary = registry.acquire(DisplayKey::PRIMARY, &cfg()).await;
        // 与 CLI 默认相反的那个方向，别让"两边都是 vp9"把这条测试变成同义反复。
        primary.pipeline.settings.set_codec(VideoCodec::Vp8);
        registry.release(DisplayKey::PRIMARY).await;

        let other = registry.acquire(DisplayKey(1), &cfg()).await;
        assert_eq!(
            other.pipeline.settings.codec(),
            VideoCodec::Vp9,
            "第二块屏没有任何历史，必须从 CLI 默认开始"
        );
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
            audio: Arc::new(AudioFanout::new()),
            audio_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refs: AtomicUsize::new(1),
            stats_lease: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: InputPolicy::default(),
            input_at: Mutex::new(None),
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
            audio: Arc::new(AudioFanout::new()),
            audio_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            refs: AtomicUsize::new(1),
            stats_lease: Mutex::new(None),
            last_snapshot: Mutex::new(None),
            controller: Mutex::new(None),
            input_policy: InputPolicy::default(),
            input_at: Mutex::new(None),
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

    /// #112：门禁要能救回一条**悬空**的租约。
    ///
    /// 线上形态（换传输必现）：WS 那条连接持着租约，用户切到 WebRTC，新页面
    /// 先连上来，旧 socket 还半开着 —— 新连接的 `try_claim_controller` 撞上
    /// "有人持有"而放弃，旧连接收尾时又没归还（那条路以前确实漏了）。之后
    /// 剩下的人看着一块摸不动的桌面：画面在流、页面把自己画的指针跟着鼠标挪，
    /// 键鼠全被门禁丢掉，重连也治不好。用户看到的现象是"指针是叠上去的、
    /// 悬停没有反馈"。
    ///
    /// `may_inject` 就是"每收一条输入顺手补一次"：空着就归我，绝不动活着的
    /// 持有者。
    #[test]
    fn the_gate_repairs_a_lease_that_its_holder_never_released() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        // 旧连接握着租约，然后无声无息地走了。
        assert!(entry.try_claim_controller(3));
        // 新连接先撞了一次墙（认领失败就放弃是 `try_claim_controller` 的语义）。
        assert!(!entry.try_claim_controller(4));

        // 旧连接收尾：没人可以交接，明确置空 —— 这就是"归还"。
        entry.set_controller(None);
        // 新连接的下一条输入就把它认领回来。
        assert!(entry.may_inject(4, true, true), "an unowned lease must be claimed on use");
        assert_eq!(entry.controller_id(), Some(4));
        assert!(entry.may_inject(4, true, true), "held by me: still mine");
        // #113: "活着"的判据是**正在投输入**，不是"认领过"。旧测试在这一步之前
        // 没有盖章，于是它验的是"认领过就永久归它" —— 那正是这次的故障。
        entry.note_input();
        assert!(!entry.may_inject(5, true, true), "never steal from an injecting holder");
        assert_eq!(entry.controller_id(), Some(4));
        // 角色不够格的人拿到空租约也不行（`viewer` 令牌）。
        entry.set_controller(None);
        assert!(!entry.may_inject(5, false, true), "a viewer must not claim by trying");
        assert_eq!(entry.controller_id(), None);
    }

    /// #113: 换传输之后的那条时间线 —— 旧会话活着、握着租约、但一条输入都不投。
    /// 以前这份租约是永久的，新的 WebSocket 连接每次认领都撞车，"指针会动、
    /// 桌面没反应"于是只能靠重启进程解决。
    #[test]
    fn a_new_connection_adopts_a_lease_nobody_is_using() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        // 新的 WebRTC 会话在建程序时认领，然后页面切成 WebSocket 再没投过输入。
        assert!(entry.try_claim_controller(2));
        assert_eq!(entry.input_idle(), None, "程序起来之后还没人投过输入");

        // 新连接落座：这条租约与悬空等价，直接接过来。
        assert!(
            entry.adopt_lease_if_stale(3, LEASE_ADOPT_IDLE),
            "a lease whose holder never injected must be adoptable"
        );
        assert_eq!(entry.controller_id(), Some(3));

        // 页面加载时同一页会连两条（信令 + 媒体）。接管不盖时间戳，所以后落座
        // 的那条**也能**拿到 —— 真正投输入的是它，把租约留给先落座的信令会话
        // 就是这次线上故障的形状。
        assert!(
            entry.adopt_lease_if_stale(4, LEASE_ADOPT_IDLE),
            "still nobody injecting: the newest connection takes it"
        );
        assert_eq!(entry.controller_id(), Some(4));

        // 一旦真的开始投输入，后来者就不能再抢了。
        entry.note_input();
        assert!(
            !entry.adopt_lease_if_stale(5, LEASE_ADOPT_IDLE),
            "an injecting holder keeps the lease"
        );
        assert_eq!(entry.controller_id(), Some(4));
    }

    /// 反过来的那一半同样重要：**正在用的人不能被顶掉**。两个页签是常态，
    /// 先到的那个每几毫秒一条 `m,`，第二个页签落座时必须看见一个新鲜的租约。
    #[test]
    fn adoption_leaves_an_active_holder_alone() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        assert!(entry.try_claim_controller(2));
        entry.note_input();

        assert!(
            !entry.adopt_lease_if_stale(3, LEASE_ADOPT_IDLE),
            "an injecting holder keeps the lease"
        );
        assert_eq!(entry.controller_id(), Some(2));
    }

    /// 落座时没赶上（旧持有者在 1.5 s 内还动过），之后才彻底静下来 —— 兜底那条
    /// 路径要在下一条输入上把租约收回来，否则又变成"重连治不好"。
    #[test]
    fn a_gate_takes_the_lease_from_a_holder_that_stopped_injecting() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        assert!(entry.try_claim_controller(2));
        entry.note_input();
        // 2 刚刚还动过 → 没到阈值，谁也不许抢。
        assert!(!entry.may_inject_within(3, true, true, LEASE_TAKEOVER_IDLE));
        assert_eq!(entry.controller_id(), Some(2));

        // 控制类指令不参与接管：`pong` 按秒回一次，让它接管等于给死会话续命。
        assert!(
            !entry.may_inject_within(3, true, false, std::time::Duration::from_nanos(1)),
            "a control line must never take the lease over"
        );
        assert_eq!(entry.controller_id(), Some(2));

        // 时间过去（阈值缩到纳秒，避免单测真的睡 3 秒）。
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(
            entry.may_inject_within(3, true, true, std::time::Duration::from_nanos(1)),
            "a holder that stopped injecting must not pin the pointer"
        );
        assert_eq!(entry.controller_id(), Some(3));
        // 接管即"从现在起是我在驱动"，所以紧接着的下一条不会被别人抢走 ——
        // 否则两条真在投输入的连接会每一条消息换一次主人，各丢一半。
        // 这里必须用**生产阈值**问：拿纳秒阈值问的话，接管后的一刻当然又"陈旧"了。
        assert!(!entry.may_inject_within(4, true, true, LEASE_TAKEOVER_IDLE));
        // 角色不够格的人永远走不到接管那一步。
        assert!(!entry.may_inject_within(4, false, true, std::time::Duration::from_nanos(1)));
        assert_eq!(entry.controller_id(), Some(3));
    }

    /// #113: 认领过但一次都没投过输入的持有者，对用户等于"没有主人"。
    /// 不这么判，两条都不注入的连接会各握半边锁，谁也投不进去。
    #[test]
    fn a_claim_without_any_input_never_counts_as_a_live_holder() {
        let entry = SharedPipeline::stub(
            PipelineHandle::new(),
            StreamSettings::from_config(&cfg()),
            InputPolicy::Exclusive,
        );
        assert!(entry.try_claim_controller(2));
        assert_eq!(entry.input_idle(), None);
        assert!(
            entry.may_inject(3, true, true),
            "a holder that never injected is as good as a dangling lease"
        );
        assert_eq!(entry.controller_id(), Some(3));
    }

    /// #109: the hello's `display_id` is a *name*, not an index, and the page
    /// numbers from one (`display2` is the second screen). Getting this
    /// off-by-one wrong is invisible on a single-monitor host and shows up on a
    /// wall as "the second tab watches the first screen".
    #[test]
    fn display_id_names_map_onto_zero_based_indices() {
        assert_eq!(DisplayKey::from_hello("primary"), DisplayKey::PRIMARY);
        assert_eq!(DisplayKey::from_hello("display2"), DisplayKey(1));
        assert_eq!(DisplayKey::from_hello("display3"), DisplayKey(2));
    }

    /// Anything unrecognised must still get a picture. A refused hello strands
    /// the user on a black tab; the wrong screen is one reload away from right.
    #[test]
    fn an_unknown_display_id_falls_back_to_the_primary() {
        for raw in ["", "  ", "primary-ish", "display", "display0", "display1", "displayx"] {
            assert_eq!(
                DisplayKey::from_hello(raw),
                DisplayKey::PRIMARY,
                "{raw:?} must not produce a bogus index"
            );
        }
    }

    /// A bare number is accepted because `--display 1` users type the index;
    /// whitespace and case come from hand-edited config, not the page.
    #[test]
    fn a_bare_index_and_case_are_accepted() {
        assert_eq!(DisplayKey::from_hello("1"), DisplayKey(1));
        assert_eq!(DisplayKey::from_hello(" 2 "), DisplayKey(2));
        assert_eq!(DisplayKey::from_hello("DISPLAY2"), DisplayKey(1));
        assert_eq!(DisplayKey::from_hello("PRIMARY"), DisplayKey::PRIMARY);
    }

    /// #109: the name → index → name round trip has to be exact, because the
    /// index goes in the log and the name goes back to the page.
    #[test]
    fn hello_name_is_the_inverse_of_from_hello() {
        for (name, index) in [("primary", 0u32), ("display2", 1), ("display3", 2)] {
            let key = DisplayKey::from_hello(name);
            assert_eq!(key.0, index, "{name} resolved wrong");
            assert_eq!(key.hello_name(), name, "index {index} names wrong");
        }
    }
}
