//! #96: the ghost pointer's one server-side number.
//!
//! Game mode draws the pointer in the browser (the host half is
//! [`super::hostui::pin_pointer`], the drawing half is `input.js`). The server
//! runs no timer: it only decides how long an unmoved ghost stays on screen and
//! hands that number to every client through `server_settings`.
//!
//! It lives in a process-wide static for the same reason `pipeline::codec`
//! does — `server_settings` is built from `StreamSettings`, which is
//! per-session and knows nothing about the process's launch flags, while this
//! value belongs to the deployment.
//!
//! 5 s is the default because a game draws its own crosshair: a ghost that
//! never leaves sits on top of the action and claims a position the pointer may
//! no longer be at, and one that leaves too early is gone before the hand moves
//! again.

use std::sync::atomic::{AtomicU64, Ordering};

/// Seconds an unmoved ghost pointer stays visible.
pub const DEFAULT_IDLE_SECS: u64 = 5;

/// Below 1 s the ghost blinks off at every pause in the hand; above ten minutes
/// it may as well be permanent, which is the behavior this knob exists to avoid.
pub const MIN_IDLE_SECS: u64 = 1;
pub const MAX_IDLE_SECS: u64 = 600;

static IDLE_SECS: AtomicU64 = AtomicU64::new(DEFAULT_IDLE_SECS);

/// `secs` inside the range the announcement can express.
pub fn clamped(secs: u64) -> u64 {
    secs.clamp(MIN_IDLE_SECS, MAX_IDLE_SECS)
}

/// Record the process's choice. Called once at startup, from `--ghost-cursor-idle`.
pub fn set_idle_secs(secs: u64) {
    IDLE_SECS.store(clamped(secs), Ordering::Relaxed);
}

/// The number announced to clients, in seconds.
pub fn idle_secs() -> u64 {
    IDLE_SECS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is the documented five seconds, so a host that never passes
    /// the flag still announces a number (and not zero, which would read as
    /// "never fade" on the client).
    #[test]
    fn the_default_is_five_seconds() {
        assert_eq!(DEFAULT_IDLE_SECS, 5);
        assert_eq!(clamped(DEFAULT_IDLE_SECS), 5);
    }

    /// Both ends clamp instead of being trusted: 0 would make the ghost blink
    /// at every pause, and a very large value would never fade at all.
    #[test]
    fn the_idle_time_is_clamped_at_both_ends() {
        assert_eq!(clamped(0), MIN_IDLE_SECS);
        assert_eq!(clamped(99_999), MAX_IDLE_SECS);
        assert_eq!(clamped(7), 7);
    }
}
