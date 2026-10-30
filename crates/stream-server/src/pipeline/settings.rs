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

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;

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

#[derive(Clone, Default)]
pub struct StreamSettings {
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
}

impl StreamSettings {
    /// Seed from the CLI config; `dirty=false` until the client asks.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            fps: Arc::new(AtomicU32::new(cfg.fps)),
            width: Arc::new(AtomicU32::new(cfg.width)),
            height: Arc::new(AtomicU32::new(cfg.height)),
            out_width: Arc::new(AtomicU32::new(0)),
            out_height: Arc::new(AtomicU32::new(0)),
            rc: Arc::new(AtomicU32::new(RateControl::Cbr.as_u32())),
            crf: Arc::new(AtomicU32::new(CRF_DEFAULT)),
            dirty: Arc::new(AtomicBool::new(false)),
        }
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
}
