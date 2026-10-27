//! Stage2 injection interface; stage1 wires `NullInputSink`, which counts every
//! inbound input command and writes a debug log line but never touches enigo,
//! never maps keysym -> VK and never emits an input acknowledgement message.
//!
//! Method set follows design 10.1 verbatim. The dispatch layer depends only on
//! this trait, so swapping in the enigo implementation in stage2 changes no
//! routing structure.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Input injection sink. `&self` methods mirror the draft trait; counters live
/// behind atomics so an immutable reference can still tally dropped events.
pub trait InputSink: Send + Sync {
    // Mouse: absolute positioning (m) / relative movement (m2) / button (p) / wheel pulse.
    fn mouse_absolute(&self, x: f64, y: f64, mask: u32, magnitude: f64);
    fn mouse_relative(&self, dx: f64, dy: f64, mask: u32, magnitude: f64);
    fn mouse_button(&self, pressed: bool, mask: u32);
    fn mouse_wheel(&self, dx: f64, dy: f64, mask: u32);

    // Keyboard: single down / up / held-set sync / release-all (kr).
    fn key_down(&self, keysym: u32);
    fn key_up(&self, keysym: u32);
    fn keys_sync(&self, held: Vec<u32>);
    fn keys_release_all(&self);

    /// Clear held keys and pointer position on session drop / reconnect.
    fn reset(&self);

    /// Tally an unparsed input line in stage1; stage2 parses each C-group line
    /// and dispatches it to the matching concrete method above instead.
    fn note_stub(&self, line: &str);

    /// #54: full wire-message entry point used by the session for every
    /// C-group line. The default keeps the stage1 stub behavior; the real
    /// `WinInputSink` overrides it with the parser + injection.
    fn handle_line(&self, line: &str) {
        self.note_stub(line);
    }
}

/// Stage1 stand-in: zero Windows handles, atomics-only counters, debug logs
/// per event group. Stage2 replaces this with the enigo implementation.
#[derive(Default)]
pub struct NullInputSink {
    mouse_absolute: AtomicU32,
    mouse_relative: AtomicU32,
    mouse_button: AtomicU32,
    mouse_wheel: AtomicU32,
    key_down: AtomicU32,
    key_up: AtomicU32,
    keys_sync: AtomicU32,
    keys_release_all: AtomicU32,
    stub_lines: AtomicU32,
}

impl NullInputSink {
    /// Snapshot of dropped-event counters for a debug / health log line.
    /// Reserved for the stage2 enigo sink health report; not read in stage1.
    #[allow(dead_code)]
    pub fn counters(&self) -> InputCounters {
        InputCounters {
            mouse_absolute: self.mouse_absolute.load(Ordering::Relaxed),
            mouse_relative: self.mouse_relative.load(Ordering::Relaxed),
            mouse_button: self.mouse_button.load(Ordering::Relaxed),
            mouse_wheel: self.mouse_wheel.load(Ordering::Relaxed),
            key_down: self.key_down.load(Ordering::Relaxed),
            key_up: self.key_up.load(Ordering::Relaxed),
            keys_sync: self.keys_sync.load(Ordering::Relaxed),
            keys_release_all: self.keys_release_all.load(Ordering::Relaxed),
            stub_lines: self.stub_lines.load(Ordering::Relaxed),
        }
    }
}

/// Dropped-event counter snapshot.
#[derive(Debug, Default, Clone, Copy)]
pub struct InputCounters {
    pub mouse_absolute: u32,
    pub mouse_relative: u32,
    pub mouse_button: u32,
    pub mouse_wheel: u32,
    pub key_down: u32,
    pub key_up: u32,
    pub keys_sync: u32,
    pub keys_release_all: u32,
    /// C-group lines tallied without concrete stage2 parsing.
    pub stub_lines: u32,
}

impl InputSink for NullInputSink {
    fn mouse_absolute(&self, x: f64, y: f64, mask: u32, magnitude: f64) {
        let n = self.mouse_absolute.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!(
            "[stage1:input] mouse_absolute x={x:.1} y={y:.1} mask={mask:#x} mag={magnitude:.2} dropped n={n}"
        );
    }
    fn mouse_relative(&self, dx: f64, dy: f64, mask: u32, magnitude: f64) {
        let n = self.mouse_relative.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!(
            "[stage1:input] mouse_relative dx={dx:.1} dy={dy:.1} mask={mask:#x} mag={magnitude:.2} dropped n={n}"
        );
    }
    fn mouse_button(&self, pressed: bool, mask: u32) {
        let n = self.mouse_button.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] mouse_button pressed={pressed} mask={mask:#x} dropped n={n}");
    }
    fn mouse_wheel(&self, dx: f64, dy: f64, mask: u32) {
        let n = self.mouse_wheel.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] mouse_wheel dx={dx:.1} dy={dy:.1} mask={mask:#x} dropped n={n}");
    }
    fn key_down(&self, keysym: u32) {
        let n = self.key_down.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] key_down keysym={keysym:#x} dropped n={n}");
    }
    fn key_up(&self, keysym: u32) {
        let n = self.key_up.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] key_up keysym={keysym:#x} dropped n={n}");
    }
    fn keys_sync(&self, held: Vec<u32>) {
        let n = self.keys_sync.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] keys_sync held={held:?} dropped n={n}");
    }
    fn keys_release_all(&self) {
        let n = self.keys_release_all.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] keys_release_all dropped n={n}");
    }
    fn reset(&self) {
        let _ = self;
    }
    fn note_stub(&self, line: &str) {
        let n = self.stub_lines.fetch_add(1, Ordering::Relaxed) + 1;
        log::debug!("[stage1:input] unparsed C-group line dropped n={n}: {line}");
    }
}

/// Convenience alias for the stage1 sink wrapped in a shareable pointer.
pub type InputSinkRef = Arc<dyn InputSink>;
