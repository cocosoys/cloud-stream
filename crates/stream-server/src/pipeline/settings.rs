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
    dirty: Arc<AtomicBool>,
}

impl StreamSettings {
    /// Seed from the CLI config; `dirty=false` until the client asks.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            fps: Arc::new(AtomicU32::new(cfg.fps)),
            width: Arc::new(AtomicU32::new(cfg.width)),
            height: Arc::new(AtomicU32::new(cfg.height)),
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
}
