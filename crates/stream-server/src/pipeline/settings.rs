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
const FPS_MIN: u32 = 1;
const FPS_MAX: u32 = 120;
const RES_MIN: u32 = 320;
const RES_MAX: u32 = 4080;

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
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn fps(&self) -> u32 {
        self.fps.load(Ordering::SeqCst)
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
        self.fps.store(fps.clamp(FPS_MIN, FPS_MAX), Ordering::SeqCst);
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
    pub fn spf(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps() as f64)
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
        assert_eq!(s.fps(), 120);
        s.set_resolution(10, 10);
        assert!(s.width() >= 320);
        s.set_resolution(99999, 99999);
        assert!(s.width() <= 4080);
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
}
