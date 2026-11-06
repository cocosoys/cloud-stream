//! Runtime stream settings, mutated from inbound client commands (#50).
//!
//! The stage1 defaults come from `Config` (`--fps/--width/--height`); the
//! signaling session writes new values on `_arg_fps,N` / `r,WxH` /
//! `SETTINGS,{manual_width,manual_height,framerate}`. Lock-free atomics keep the
//! capture/encode worker able to re-read targets every beat without blocking the
//! async session.
//!
//! `dirty` distinguishes "still the CLI default" from "client explicitly asked":
//! the pipeline seeds an untouched `StreamSettings` with the *native* display
//! size so the default path does zero scaling; only a client-requested
//! resolution change turns on the BGRA scaler.
//!
//! #68 — DO NOT CONFLATE "requested" WITH "transmitted".
//! `width()/height()` are the client *request* (what the settings panel asked
//! for, e.g. 3840x2160). Since #66 the pipeline clamps that request to the
//! native display and encodes at the clamped size (e.g. 1718x926), and the
//! browser decodes exactly the clamped size (`videoWidth/videoHeight`). So the
//! frames on the wire are `output()`, **not** `width()/height()`.
//! Pointer input (`m,x,y`) arrives in *transmitted frame pixels* and must be
//! divided by `output()` — using `width()` there silently scales every click by
//! `request/actual` and produces a growing cursor offset toward the edges
//! (the #68 regression: 1718/3840 = 44.7 % ⇒ clicks landed at ~45 % of the
//! intended position). `output()` is (0, 0) until the capture loop publishes
//! its real encoder size; callers must fall back explicitly.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::pipeline::codec::VideoCodec;

/// Lower/upper clamp for client-requested values.
///
/// `FPS_MAX` moved 120 -> 240 so the panel's slider can offer the rates a
/// 144/165/240 Hz display actually runs at; the encoder backstop and the
/// bitrate budget are both scaled off the same ceiling (`keyframe_backstop_frames`,
/// `bitrate_for`), so nothing here assumes a 60 Hz session.
const FPS_MIN: u32 = 1;
const FPS_MAX: u32 = 240;
const RES_MIN: u32 = 320;
const RES_MAX: u32 = 4080;

/// The "no limit" frame rate: the panel's rightmost slider stop.
///
/// Zero is not a rate, so it never reaches `spf()`'s division. It means the
/// beat is paced by the capture thread instead of a timer — one encode per
/// frame the desktop actually produced, which is the lowest latency this
/// pipeline can reach and is capped only by the display's own refresh. See
/// `pipeline::settle_beat` for the wait, and `sample_spf` for what a sample is
/// stamped with when there is no nominal rate to stamp.
pub const FPS_UNCAPPED: u32 = 0;

/// The rate an uncapped session is budgeted and keyframed as. There is no rate
/// to scale by, so the ceiling stands in: a session told to run flat out is not
/// budgeted as if it ran at one frame per second.
pub const FPS_BUDGET: u32 = FPS_MAX;

/// #92: the quantizer index bounds for constant quality.
///
/// VP8 quantizers run 0..=63, where 0 is near-lossless and 63 is a smear. The
/// floor is 4 rather than 0 because lossless-ish VP8 at 1080p is several hundred
/// megabits — a "quality" setting nobody can run. The panel starts at
/// `CRF_DEFAULT` (25), which is roughly what CRF 23 means for x264: transparent
/// on text, still cheap on video.
pub const CRF_MIN: u32 = 4;
pub const CRF_MAX: u32 = 63;
pub const CRF_DEFAULT: u32 = 25;

/// #92: how the encoder spends its bitrate. Fixed for the life of an encoder —
/// libvpx refuses to change `rc_end_usage` on a live one, so a change here makes
/// the capture loop rebuild the encoder (same path as a resolution change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate: the target is held, quantizers float.
    Cbr,
    /// Constant quality: quantizers are pinned, the target is the ceiling.
    Cq,
}

impl RateControl {
    /// The panel's `_rc,<mode>` vocabulary, which is also what
    /// `server_settings.rate_control_mode.value` carries.
    pub fn from_wire(mode: &str) -> Option<Self> {
        match mode.trim().to_ascii_lowercase().as_str() {
            "cbr" => Some(RateControl::Cbr),
            "crf" | "cq" => Some(RateControl::Cq),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RateControl::Cbr => "cbr",
            RateControl::Cq => "crf",
        }
    }

    fn as_u32(self) -> u32 {
        match self {
            RateControl::Cbr => 0,
            RateControl::Cq => 1,
        }
    }

    fn from_u32(v: u32) -> Self {
        if v == 1 {
            RateControl::Cq
        } else {
            RateControl::Cbr
        }
    }
}

#[derive(Clone)]
pub struct StreamSettings {
    /// Which display this program captures (#101 的第二个维度)。
    ///
    /// 不是 `Arc<AtomicU32>`：显示器是**程序**的身份，不是可以中途改的参数
    /// —— 换屏意味着换一条采集管线（`PipelineRegistry` 按它索引），所以在
    /// 建程序时定下来，运行期只读。
    display: u32,
    fps: Arc<AtomicU32>,
    width: Arc<AtomicU32>,
    height: Arc<AtomicU32>,
    /// #68: geometry of the frames the encoder *actually* emits (the size the
    /// browser decodes). Published by the capture loop, never by the client.
    /// Kept separate from `width/height` because #66 clamps an over-large
    /// client request to the native display: the request stays at 3840x2160
    /// while the wire is 1718x926, and input mapping must use the wire size.
    /// (0, 0) = the capture loop has not reported yet.
    out_width: Arc<AtomicU32>,
    out_height: Arc<AtomicU32>,
    /// #92: `RateControl` as a number (0 = CBR, 1 = CQ) plus the quantizer it
    /// pins when it is CQ. Read every beat; a change makes the loop rebuild the
    /// encoder, since libvpx will not switch `rc_end_usage` in place.
    rc: Arc<AtomicU32>,
    crf: Arc<AtomicU32>,
    dirty: Arc<AtomicBool>,
    /// #110: the video codec **this program** encodes with.
    ///
    /// It used to be a process-wide `AtomicU8` in `codec`, on the reasoning that
    /// a pick has to outlive the session that carried it (the pick forces a
    /// reconnect, and `StreamSettings` was rebuilt from `Config` on every
    /// connect). Keeping it process-wide made one viewer's menu change every
    /// screen on the machine and kick every other session
    /// (`codec_registry.kick`), which is not a settings panel — it is a
    /// broadcast.
    ///
    /// So it moved onto the program. But "the program outlives the sessions on
    /// it" is only true while *some* session is attached: the pick forces the
    /// last one to leave, `PipelineRegistry::release` drops the program, and the
    /// reconnect builds a new one from `Config` — back to VP8, which the page then
    /// picks again. That is the reconnect loop, and [`StreamPrefs`] is what
    /// breaks it: the registry keeps the outgoing program's values and seeds the
    /// next one with them.
    codec: Arc<AtomicU8>,
    /// #110: the Opus target for this program's audio downlink, in bps.
    ///
    /// Was `audio::TARGET_BPS`. The audio thread is already per-program (one
    /// loopback capture feeds every sink on it), so the one value it read had no
    /// business being shared with the other programs.
    audio_bps: Arc<AtomicI32>,
    /// #110: whether this program's audio downlink actually carries sound.
    /// Was `audio::ENABLED`.
    audio_enabled: Arc<AtomicBool>,
}

/// #112: 上一次被点名的视频编码器 —— 进程级的一格记忆。
///
/// WebRTC 换编码器只有一条路：断开重连，让**新会话**的 m-line 带上新编码器
/// （见 `websession` 的 #94 注释）。那条路有一个前提 —— 新会话得记得上一次点了
/// 什么。可会话设置是 `from_config` 现场造的，只认 CLI 默认值，于是：
///
/// 1. 页面（localStorage 里的 `encoder`）还是 `vp9enc`，一连上就整批发来；
/// 2. 服务端 `set_codec(vp9)` 认为"变了" → 踢掉会话要求重新协商；
/// 3. 新会话又从 VP8 报价，页面又点 VP9 → 再踢……
///
/// 这个环在 WS → WebRTC 的切换上必现（WS 那边刚把编码器点成 VP9），页面上就是
/// "黑屏 + 一直重连"，实测一轮切换触发了 4 次重启、`reconnects` 涨到 554。
///
/// 所以这里留一格**只用于给新会话当初始值**的记忆。它不参与在线程序的编码决定
/// （那是 `StreamSettings::codec` 的事），所以 #110 "一个观众换编码器不该重排
/// 别人的屏" 的结论没有被推翻。
static PREFERRED_CODEC: AtomicU8 = AtomicU8::new(UNSET_CODEC);

/// "还没人选过"。`VideoCodec::as_byte` 把 VP8 记成 0，所以不能拿 0 当空值。
const UNSET_CODEC: u8 = u8::MAX;

/// 记下一次编码器选择，供**之后新建的会话/程序**当初始值。
///
/// 幂等、无锁、随便调：它记的是"用户偏好"，不是任何一条连接的运行状态。
pub fn note_preferred_codec(codec: VideoCodec) {
    PREFERRED_CODEC.store(codec.as_byte(), Ordering::Relaxed);
}

/// 新会话应该从哪个编码器起步：上次点过就用它，否则用调用方的默认值。
///
/// 故意**不**放进 `StreamSettings::from_config`：那个构造函数是"CLI 默认值"的
/// 定义，让它去读进程状态会把每个单测都变成有顺序依赖的。种子只该撒在
/// "一条连接刚建立、马上要用它报价" 的那一处（`SessionCtx::new`）。
pub fn preferred_codec_or(fallback: VideoCodec) -> VideoCodec {
    match PREFERRED_CODEC.load(Ordering::Relaxed) {
        UNSET_CODEC => fallback,
        byte => VideoCodec::from_byte(byte),
    }
}

/// 测试用：把记忆恢复成"还没人选过"，免得一条用例污染后面的用例。
#[cfg(test)]
pub fn forget_preferred_codec() {
    PREFERRED_CODEC.store(UNSET_CODEC, Ordering::Relaxed);
}

/// 一个程序操作参数的**值快照**，用来跨程序生命周期继承用户的选择。
///
/// 为什么需要它：设置活不过程序本身。最后一个会话离开时
/// `PipelineRegistry::release` 会把 `SharedPipeline` 摘掉并停掉采集循环，下一个
/// 连接 `acquire` 出来的新程序是 `StreamSettings::from_config(cfg)` —— CLI 默认值。
/// 而"换编码器"恰恰会**强制断开当前这条唯一的连接**（握手期定下的 m-line 与
/// libvpx 上下文都不能中途改），于是形成自循环：
///
/// ```text
/// 页面选 VP9 → 服务端改设置 → 断开重协商 → 新程序回 VP8 → 页面再推 VP9 → …
/// ```
///
/// 症状就是页面一直停在 "WebSocket disconnected. Attempting to reconnect..."，
/// 一次都连不上画面。
///
/// 为什么不能用 `StreamSettings::clone()`：它的 `Clone` 是浅拷贝，字段全是
/// `Arc<Atomic*>`，克隆体和原体共用同一批原子量 —— 存进注册表等于存了个别名，
/// 下一个程序一改，所谓"历史值"跟着一起变。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamPrefs {
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// `RateControl::as_u32()`：0 = CBR、1 = CQ。
    pub rc: u32,
    pub crf: u32,
    pub codec: VideoCodec,
    pub audio_bps: i32,
    pub audio_enabled: bool,
}

impl StreamSettings {
    /// Seed from the CLI config; `dirty=false` until the client asks.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            display: cfg.display,
            fps: Arc::new(AtomicU32::new(cfg.fps)),
            width: Arc::new(AtomicU32::new(cfg.width)),
            height: Arc::new(AtomicU32::new(cfg.height)),
            out_width: Arc::new(AtomicU32::new(0)),
            out_height: Arc::new(AtomicU32::new(0)),
            rc: Arc::new(AtomicU32::new(RateControl::Cbr.as_u32())),
            crf: Arc::new(AtomicU32::new(CRF_DEFAULT)),
            dirty: Arc::new(AtomicBool::new(false)),
            codec: Arc::new(AtomicU8::new(
                VideoCodec::from_wire(&cfg.video_codec)
                    .unwrap_or_default()
                    .as_byte(),
            )),
            audio_bps: Arc::new(AtomicI32::new(crate::pipeline::audio::default_bitrate_bps())),
            audio_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The codec this program encodes with.
    ///
    /// Read by the capture loop every beat (a pick forces a rebuild), by
    /// `server_settings`/`stream_info`, and by the SDP builder when the program's
    /// first session negotiates — all of them from the program, so they cannot
    /// disagree.
    pub fn codec(&self) -> VideoCodec {
        VideoCodec::from_byte(self.codec.load(Ordering::Relaxed))
    }

    /// Record a codec pick. Returns `true` when it actually changed, which is
    /// what the caller uses to decide whether a restart is worth forcing.
    pub fn set_codec(&self, codec: VideoCodec) -> bool {
        let previous = self.codec.swap(codec.as_byte(), Ordering::Relaxed);
        VideoCodec::from_byte(previous) != codec
    }

    /// 记下一份可跨程序保存的**值**快照（见 [`StreamPrefs`]）。
    ///
    /// 只收客户端能改的那些参数。`out_width/out_height` 是采集循环回报的
    /// 事实，不是设置；`dirty` 是"客户端动过没有"的记账，同样不该继承 ——
    /// 新程序是不是脏的，取决于新连接有没有提要求。`display` 由 `acquire`
    /// 用 key 覆盖，也不进快照。
    pub fn prefs(&self) -> StreamPrefs {
        StreamPrefs {
            fps: self.fps(),
            width: self.width(),
            height: self.height(),
            rc: self.rc.load(Ordering::SeqCst),
            crf: self.crf(),
            codec: self.codec(),
            audio_bps: self.audio_bps.load(Ordering::SeqCst),
            audio_enabled: self.audio_enabled(),
        }
    }

    /// 把一份快照套到新程序上。
    ///
    /// 刻意走 setter 而不是直接 `store`：这些值当初就是被 setter 校验过的，
    /// 再走一遍等于用同一条路径复原，clamp 与 `dirty` 的语义不会出现"继承来的
    /// 值"和"用户设的值"两条不同轨迹。
    pub fn apply_prefs(&self, prefs: StreamPrefs) {
        self.set_fps(prefs.fps);
        self.set_resolution(prefs.width, prefs.height);
        self.set_rate_control(RateControl::from_u32(prefs.rc));
        self.set_crf(prefs.crf);
        self.set_codec(prefs.codec);
        self.set_audio_bitrate_bps(prefs.audio_bps);
        self.set_audio_enabled(prefs.audio_enabled);
    }

    /// The audio downlink's Opus target, in bps.
    pub fn audio_bitrate_bps(&self) -> u32 {
        self.audio_bps.load(Ordering::Relaxed).max(0) as u32
    }

    /// Re-aim the audio encoder for this program. `<= 0` means "back to the
    /// built-in default"; anything outside the Opus range is clamped, so the
    /// panel's slider can only hold values the encoder will take.
    pub fn set_audio_bitrate_bps(&self, bps: i32) -> i32 {
        let clamped = crate::pipeline::audio::clamp_bps(bps);
        self.audio_bps.store(clamped, Ordering::Relaxed);
        clamped
    }

    /// Whether this program's downlink carries game sound at all.
    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled.load(Ordering::Relaxed)
    }

    /// Mute or unmute this program's downlink.
    pub fn set_audio_enabled(&self, on: bool) {
        self.audio_enabled.store(on, Ordering::Relaxed);
    }

    /// #110: the audio thread's two handles.
    ///
    /// Cloned `Arc`s rather than a `&StreamSettings` because the thread owns its
    /// own copy for the life of the program: it must keep reading after the
    /// settings struct that spawned it has been dropped, and both atomics are
    /// shared with the session that changes them.
    pub fn audio_handles(&self) -> (Arc<AtomicI32>, Arc<AtomicBool>) {
        (self.audio_bps.clone(), self.audio_enabled.clone())
    }

    /// Which display this program captures (DXGI output ordinal).
    pub fn display(&self) -> u32 {
        self.display
    }

    /// Bind the settings to a display. Called by the registry when a session
    /// asks for a non-default screen, before the capture loop reads them.
    pub fn set_display(&mut self, display: u32) {
        self.display = display;
    }

    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::SeqCst)
    }

    /// Did the client ask for no frame-rate limit (`FPS_UNCAPPED`)?
    pub fn uncapped(&self) -> bool {
        self.fps() == FPS_UNCAPPED
    }

    pub fn width(&self) -> u32 {
        self.width.load(Ordering::SeqCst)
    }

    pub fn height(&self) -> u32 {
        self.height.load(Ordering::SeqCst)
    }

    /// Has the client explicitly set any value?
    pub fn dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    pub fn set_fps(&self, fps: u32) {
        // 0 is the panel's "no limit" stop, not an absurd rate: keep it rather
        // than clamping it up to `FPS_MIN`, which would turn unlimited into
        // one frame per second.
        let fps = if fps == FPS_UNCAPPED {
            FPS_UNCAPPED
        } else {
            fps.clamp(FPS_MIN, FPS_MAX)
        };
        self.fps.store(fps, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Client asked for an explicit resolution (a scaler target).
    pub fn set_resolution(&self, width: u32, height: u32) {
        self.width
            .store(width.clamp(RES_MIN, RES_MAX), Ordering::SeqCst);
        self.height
            .store(height.clamp(RES_MIN, RES_MAX), Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Pipeline seeds the native display size when the client never asked, so
    /// the default path captures and encodes 1:1 (no scaling).
    pub fn seed_native(&self, width: u32, height: u32) {
        if !self.dirty() {
            self.width.store(width, Ordering::SeqCst);
            self.height.store(height, Ordering::SeqCst);
        }
    }

    /// #68: publish the geometry of the frames that actually leave the encoder.
    /// Called by the capture loop whenever it (re)opens the encoder — i.e. after
    /// the `min(request, native)` clamp of #66 — so pointer mapping has the same
    /// divisor the browser uses (`videoWidth/videoHeight`).
    ///
    /// READ BEFORE TOUCHING: this is the only correct source for frame→screen
    /// pointer math. Do **not** replace it with `width()/height()`; those hold
    /// the un-clamped client request and reintroduce the #68 cursor offset.
    pub fn set_output(&self, width: u32, height: u32) {
        self.out_width.store(width, Ordering::SeqCst);
        self.out_height.store(height, Ordering::SeqCst);
    }

    /// #68: the transmitted frame size, or `(0, 0)` when the capture loop has
    /// not reported a size yet (callers fall back to `width()/height()`).
    pub fn output(&self) -> (u32, u32) {
        (
            self.out_width.load(Ordering::SeqCst),
            self.out_height.load(Ordering::SeqCst),
        )
    }

    /// Seconds per frame at the current fps; re-read by the worker each beat.
    ///
    /// Zero for an uncapped session — the caller must read that as "no beat
    /// deadline at all" (`pipeline::settle_beat`) rather than as a duration.
    pub fn spf(&self) -> Duration {
        let fps = self.fps();
        if fps == FPS_UNCAPPED {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(1.0 / fps as f64)
    }

    /// The duration stamped on one emitted sample: `spf`, except for an uncapped
    /// session, where there is no nominal rate and a zero duration would stamp
    /// every frame with the same RTP timestamp (the browser reads the rest of
    /// the burst as duplicates of one frame). One millisecond keeps the media
    /// clock moving forward; the forwarder overrides it with the interval the
    /// beat actually ran at, which is the honest figure.
    pub fn sample_spf(&self) -> Duration {
        let spf = self.spf();
        if spf.is_zero() {
            Duration::from_millis(1)
        } else {
            spf
        }
    }

    /// The rate a bitrate budget and a keyframe backstop scale by: the live fps,
    /// or `FPS_BUDGET` for an uncapped session.
    pub fn budget_fps(&self) -> u32 {
        let fps = self.fps();
        if fps == FPS_UNCAPPED {
            FPS_BUDGET
        } else {
            fps
        }
    }

    /// #92: how the encoder allocates its bitrate.
    pub fn rate_control(&self) -> RateControl {
        RateControl::from_u32(self.rc.load(Ordering::SeqCst))
    }

    /// #92: the pinned quantizer used under `RateControl::Cq`.
    pub fn crf(&self) -> u32 {
        self.crf.load(Ordering::SeqCst)
    }

    pub fn set_rate_control(&self, rc: RateControl) {
        self.rc.store(rc.as_u32(), Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Clamped to the quantizer range a VP8 encoder actually has.
    pub fn set_crf(&self, crf: u32) {
        self.crf
            .store(crf.clamp(CRF_MIN, CRF_MAX), Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
    }
}

impl Default for StreamSettings {
    /// Going through `Config::default()` rather than hand-writing a second set
    /// of numbers: a default that disagrees with `from_config` is a default that
    /// only shows up in tests and then quietly lies.
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn seeds_config_defaults_and_stays_clean() {
        let s = StreamSettings::from_config(&cfg());
        assert_eq!(s.fps(), 30);
        assert_eq!(s.width(), 1920);
        assert_eq!(s.height(), 1080);
        assert!(!s.dirty());
        assert_eq!(s.spf(), Duration::from_secs_f64(1.0 / 30.0));
    }

    /// #112：编码器的选择必须活过"断开重连"。
    ///
    /// WebRTC 换编码器的唯一机制是重启协商（新会话的 m-line 带新编码器）。如果
    /// 新会话不记得上一次点了什么，它就会退回 VP8 报价，页面再点一次 VP9，服务端
    /// 再踢一次 —— 一个永不收敛的重启环。这条钉的就是那格记忆。
    ///
    /// 端点单独收尾：记忆是进程级的，留着会污染其它用例对"默认编码器"的假设。
    #[test]
    fn a_codec_pick_survives_into_the_next_session() {
        forget_preferred_codec();
        let fallback = VideoCodec::Vp8;
        assert_eq!(preferred_codec_or(fallback), fallback, "没人点过就是默认值");

        note_preferred_codec(VideoCodec::Vp9);
        assert_eq!(
            preferred_codec_or(fallback),
            VideoCodec::Vp9,
            "新会话要用用户点的那一档报价，否则重启协商永远落不了地"
        );

        // 浏览器解不了 VP9 时服务端会把偏好压回 VP8（`refused_video_codec` 那条
        // 路），压回去之后也必须真的生效。
        note_preferred_codec(VideoCodec::Vp8);
        assert_eq!(preferred_codec_or(VideoCodec::Vp9), VideoCodec::Vp8);
        forget_preferred_codec();
    }

    #[test]
    fn seed_native_only_when_clean() {
        let s = StreamSettings::from_config(&cfg());
        s.seed_native(1718, 926);
        assert_eq!((s.width(), s.height()), (1718, 926));
        s.set_resolution(1280, 720);
        s.seed_native(1718, 926); // dirty: no-op
        assert_eq!((s.width(), s.height()), (1280, 720));
    }

    #[test]
    fn fps_change_recomputes_spf() {
        let s = StreamSettings::from_config(&cfg());
        s.set_fps(60);
        assert!(s.dirty());
        assert_eq!(s.fps(), 60);
        assert_eq!(s.spf(), Duration::from_secs_f64(1.0 / 60.0));
    }

    #[test]
    fn clamps_absurd_requests() {
        let s = StreamSettings::from_config(&cfg());
        s.set_fps(999);
        assert_eq!(s.fps(), 240);
        s.set_resolution(10, 10);
        assert!(s.width() >= 320);
        s.set_resolution(99999, 99999);
        assert!(s.width() <= 4080);
    }

    /// The panel's rightmost stop asks for no limit; it must survive `set_fps`
    /// unchanged (a clamp to `FPS_MIN` would silently make it 1 fps) and must
    /// not turn into a division by zero.
    #[test]
    fn zero_is_uncapped_not_a_rate() {
        let s = StreamSettings::from_config(&cfg());
        s.set_fps(0);
        assert!(s.uncapped());
        assert_eq!(s.fps(), 0);
        assert_eq!(s.spf(), Duration::ZERO);
        assert_eq!(s.sample_spf(), Duration::from_millis(1));
        // Budgeted as the ceiling, not as zero.
        assert_eq!(s.budget_fps(), 240);
        // And back to a rate.
        s.set_fps(144);
        assert!(!s.uncapped());
        assert_eq!(s.spf(), Duration::from_secs_f64(1.0 / 144.0));
        assert_eq!(s.budget_fps(), 144);
    }

    /// #68 regression guard: the transmitted size must stay independent of the
    /// client request, otherwise the pointer mapping divides by the wrong
    /// number and every click lands short (cursor offset).
    #[test]
    fn output_is_never_the_requested_resolution() {
        let s = StreamSettings::from_config(&cfg());
        s.set_resolution(3840, 2160);
        assert_eq!(s.output(), (0, 0), "unknown until the capture loop reports");
        s.set_output(1718, 926);
        assert_eq!(s.output(), (1718, 926));
        // A later client request must not move the transmitted geometry: only
        // the pipeline clamps + republishes it after reopening the encoder.
        s.set_resolution(2560, 1440);
        assert_eq!(s.output(), (1718, 926));
        assert_eq!((s.width(), s.height()), (2560, 1440));
    }

    /// #92: the rate-control pair. The defaults must be the pre-#92 behaviour
    /// (CBR), and a crf outside the quantizer range must clamp rather than be
    /// stored as-is — libvpx would otherwise take 1000 as a quantizer.
    #[test]
    fn rate_control_defaults_to_cbr_and_clamps_crf() {
        let s = StreamSettings::from_config(&cfg());
        assert_eq!(s.rate_control(), RateControl::Cbr);
        assert_eq!(s.crf(), CRF_DEFAULT);
        assert!(!s.dirty(), "seeded from the CLI, not a client request");

        s.set_rate_control(RateControl::Cq);
        s.set_crf(1000);
        assert_eq!(s.rate_control(), RateControl::Cq);
        assert_eq!(s.crf(), CRF_MAX);
        s.set_crf(0);
        assert_eq!(s.crf(), CRF_MIN);
        assert!(s.dirty());
    }

    /// The panel's wire vocabulary is `cbr` / `crf`; anything else must be
    /// refused instead of silently becoming CBR.
    #[test]
    fn wire_names_round_trip() {
        assert_eq!(RateControl::from_wire("crf"), Some(RateControl::Cq));
        assert_eq!(RateControl::from_wire("CBR"), Some(RateControl::Cbr));
        assert_eq!(RateControl::from_wire("cq"), Some(RateControl::Cq));
        assert_eq!(RateControl::from_wire("vbr"), None);
        assert_eq!(RateControl::Cq.as_str(), "crf");
        assert_eq!(RateControl::Cbr.as_str(), "cbr");
    }

    /// 快照要能原样复原到**另一个**程序上：这正是"切编码器后重连不再回落到
    /// VP8"的机制。
    #[test]
    fn prefs_restore_onto_a_fresh_program() {
        let old = StreamSettings::from_config(&cfg());
        old.set_codec(VideoCodec::Vp9);
        old.set_fps(45);
        old.set_resolution(1280, 720);
        old.set_rate_control(RateControl::Cq);
        old.set_crf(32);
        old.set_audio_bitrate_bps(24_000);
        old.set_audio_enabled(false);
        let prefs = old.prefs();

        let new = StreamSettings::from_config(&cfg());
        new.apply_prefs(prefs);

        assert_eq!(new.codec(), VideoCodec::Vp9);
        assert_eq!(new.fps(), 45);
        assert_eq!((new.width(), new.height()), (1280, 720));
        assert_eq!(new.rate_control(), RateControl::Cq);
        assert_eq!(new.crf(), 32);
        assert_eq!(new.audio_bitrate_bps(), 24_000);
        assert!(!new.audio_enabled());
        assert_eq!(new.prefs(), prefs, "单调往返必须稳定");
    }

    /// `StreamSettings::clone()` 是浅拷贝（字段都是 `Arc<Atomic*>`），所以快照
    /// 必须是**值**。这条测试盯着那个最容易踩的实现错误：拿 clone 顶替。
    #[test]
    fn prefs_are_a_snapshot_not_an_alias() {
        let s = StreamSettings::from_config(&cfg());
        s.set_codec(VideoCodec::Vp9);
        s.set_fps(45);
        let prefs = s.prefs();

        // 快照之后原程序继续被改（同一个会话里用户又调了参数）。
        s.set_codec(VideoCodec::Vp8);
        s.set_fps(15);

        assert_eq!(prefs.codec, VideoCodec::Vp9, "快照不能跟着原程序变");
        assert_eq!(prefs.fps, 45);

        // 反向也不行：套用快照到新程序，不该把旧程序的原子量带进来。
        let other = StreamSettings::from_config(&cfg());
        other.apply_prefs(prefs);
        other.set_fps(24);
        assert_eq!(s.fps(), 15, "两份设置之间没有共享的原子量");
    }
}
