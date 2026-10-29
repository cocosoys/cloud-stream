//! #84 QoS: back the encoder off when the *link* is the bottleneck, and put it
//! back when it is not.
//!
//! Before this the bitrate had exactly one source: the client's `vb,N` slider
//! or the fps-scaled `--bitrate`. A congested link therefore did not change the
//! rate at all — the picture turned to mosaic, the decoder sent PLI after PLI,
//! and it stayed that way until a human dragged the slider. `stream_stats`
//! published `throttled: false` for the same reason: there was nothing to
//! publish.
//!
//! Two observations drive the decision, and both are already measured:
//!
//! * **backpressure** — `send_ms - pipeline_ms`, the time a frame spends waiting
//!   in the hand-off queue (added in #78). `pipeline_ms` stops at the encoder
//!   exit, so the difference is exactly what the transport could not absorb.
//! * **PLI rate** — the decoder lost its reference chain, which on a LAN means
//!   the link dropped packets rather than the encoder being slow.
//!
//! Packet loss *is* available from the browser (`stream_stats` on its side), but
//! this server does not consume it yet: two server-side signals already answer
//! "is the link behind", and a wrong loss sample would degrade the picture for
//! nothing. Recorded here so the next person knows it is a choice, not an
//! oversight.
//!
//! The controller is deliberately conservative:
//! * one decision per [`QOS_INTERVAL`], never per frame;
//! * one step of -25 % / +10 %, so a burst cannot collapse the picture;
//! * `Recovering` climbs only after [`RECOVER_WINDOWS`] calm windows in a row,
//!   so the frame that caused the drop cannot immediately undo it;
//! * a floor at 20 % of the base (and 300 kbps), because a rate low enough to
//!   keep the link idle is also low enough to be useless.

use std::time::Duration;

/// How often a decision may be made. 2 s is long enough to see a queue build
/// and short enough to react inside one buffer's worth of stutter.
pub const QOS_INTERVAL: Duration = Duration::from_secs(2);
/// Queue wait that means "the transport is not keeping up". A healthy session
/// sits under 10 ms (`send_ms` ≈ `pipeline_ms`).
const DEGRADE_BACKLOG_MS: f32 = 120.0;
/// PLI/s above this is a link dropping packets, not a slow encoder.
const DEGRADE_PLI_PER_SEC: f32 = 2.0;
/// Back to normal: queue drained and the decoder stopped asking for keyframes.
const RECOVER_BACKLOG_MS: f32 = 40.0;
const RECOVER_PLI_PER_SEC: f32 = 0.5;
/// Calm windows required before climbing out of `Degraded`.
const RECOVER_WINDOWS: u8 = 2;
/// One step down is a quarter of the current target...
const STEP_DOWN: f32 = 0.75;
/// ...and one step up a tenth, so recovery is gradual and does not re-trigger.
const STEP_UP: f32 = 1.10;
/// Never go below this share of the configured base, nor below an absolute
/// floor where VP8 has nothing left to spend.
const FLOOR_FRACTION: f32 = 0.2;
const FLOOR_KBPS: u32 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosState {
    /// At the configured target.
    Normal,
    /// Backed off, still under pressure.
    Degraded,
    /// Backed off, climbing back toward the base.
    Recovering,
}

impl QosState {
    /// Value published as `throttled` in `stream_stats`.
    pub fn throttled(self) -> bool {
        !matches!(self, QosState::Normal)
    }

    pub fn label(self) -> &'static str {
        match self {
            QosState::Normal => "normal",
            QosState::Degraded => "degraded",
            QosState::Recovering => "recovering",
        }
    }
}

/// One window's evidence.
#[derive(Debug, Clone, Copy)]
pub struct QosSample {
    /// `send_ms - pipeline_ms` over the window, in milliseconds.
    pub backlog_ms: f32,
    /// Session-total PLI/FIR count; the controller differences it.
    pub pli_total: u64,
    /// Length of the window the figures were measured over.
    pub window: Duration,
}

/// What the loop should do with this window.
#[derive(Debug, Clone, Copy)]
pub struct QosStep {
    pub target_kbps: u32,
    pub state: QosState,
    /// True when `target_kbps` differs from the previous window, i.e. the
    /// encoder must be re-aimed.
    pub changed: bool,
}

pub struct QosController {
    /// The budget a healthy link should get back (`--bitrate` × fps scale, or
    /// the last `vb,N` the client asked for).
    base_kbps: u32,
    target_kbps: u32,
    state: QosState,
    pli_seen: u64,
    good_windows: u8,
}

impl QosController {
    pub fn new(base_kbps: u32) -> Self {
        let base = base_kbps.max(FLOOR_KBPS);
        Self {
            base_kbps: base,
            target_kbps: base,
            state: QosState::Normal,
            pli_seen: 0,
            good_windows: 0,
        }
    }

    pub fn state(&self) -> QosState {
        self.state
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    pub fn base_kbps(&self) -> u32 {
        self.base_kbps
    }

    /// The configured budget changed (fps change, `--bitrate`, `vb,N`). A new
    /// base also means a new ceiling: whatever the controller had backed off to
    /// is meaningless against a different target, so it restarts from `base`.
    pub fn set_base(&mut self, base_kbps: u32) {
        self.base_kbps = base_kbps.max(FLOOR_KBPS);
        self.target_kbps = self.base_kbps;
        self.state = QosState::Normal;
        self.good_windows = 0;
    }

    fn floor_kbps(&self) -> u32 {
        ((self.base_kbps as f32 * FLOOR_FRACTION) as u32).max(FLOOR_KBPS)
    }

    /// Fold one window in and return the rate to apply.
    pub fn evaluate(&mut self, sample: QosSample) -> QosStep {
        let window_secs = sample.window.as_secs_f32().max(1e-3);
        let pli_delta = sample.pli_total.saturating_sub(self.pli_seen);
        self.pli_seen = sample.pli_total;
        let pli_rate = pli_delta as f32 / window_secs;

        let pressure = sample.backlog_ms >= DEGRADE_BACKLOG_MS || pli_rate >= DEGRADE_PLI_PER_SEC;
        let calm = sample.backlog_ms <= RECOVER_BACKLOG_MS && pli_rate < RECOVER_PLI_PER_SEC;

        let before = self.target_kbps;
        match self.state {
            QosState::Normal | QosState::Recovering => {
                if pressure {
                    self.target_kbps = self.step_down();
                    self.state = QosState::Degraded;
                    self.good_windows = 0;
                } else if self.state == QosState::Recovering {
                    if calm {
                        self.good_windows += 1;
                    } else {
                        // Neither calm enough to climb nor under pressure: hold
                        // and restart the count rather than creep upward.
                        self.good_windows = 0;
                    }
                    if self.good_windows >= RECOVER_WINDOWS {
                        self.good_windows = 0;
                        self.target_kbps = ((self.target_kbps as f32 * STEP_UP) as u32)
                            .min(self.base_kbps);
                        if self.target_kbps >= self.base_kbps {
                            self.target_kbps = self.base_kbps;
                            self.state = QosState::Normal;
                        }
                    }
                }
            }
            QosState::Degraded => {
                if pressure {
                    self.target_kbps = self.step_down();
                } else if calm {
                    self.state = QosState::Recovering;
                    // This calm window is the first of the required run; without
                    // counting it, "two calm windows" would silently mean three.
                    self.good_windows = 1;
                }
            }
        }

        QosStep {
            target_kbps: self.target_kbps,
            state: self.state,
            changed: self.target_kbps != before,
        }
    }

    fn step_down(&self) -> u32 {
        let next = (self.target_kbps as f32 * STEP_DOWN) as u32;
        next.max(self.floor_kbps()).min(self.target_kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(backlog_ms: f32, pli_total: u64) -> QosSample {
        QosSample {
            backlog_ms,
            pli_total,
            window: QOS_INTERVAL,
        }
    }

    #[test]
    fn healthy_link_is_left_alone() {
        let mut q = QosController::new(4000);
        for _ in 0..50 {
            let step = q.evaluate(sample(8.0, 0));
            assert_eq!(step.target_kbps, 4000);
            assert!(!step.changed);
            assert_eq!(step.state, QosState::Normal);
        }
    }

    #[test]
    fn backpressure_drops_one_step_and_flags_throttled() {
        let mut q = QosController::new(4000);
        let step = q.evaluate(sample(200.0, 0));
        assert_eq!(step.target_kbps, 3000, "one 25 % step");
        assert!(step.changed);
        assert!(step.state.throttled());
    }

    /// Sustained pressure keeps stepping down, but never past the floor — a
    /// rate that keeps the link idle is also too low to be watchable.
    #[test]
    fn sustained_pressure_stops_at_the_floor() {
        let mut q = QosController::new(4000);
        let mut last = 4000;
        for _ in 0..40 {
            last = q.evaluate(sample(300.0, 0)).target_kbps;
        }
        assert_eq!(last, 800, "floor is 20 % of base");
        assert!(last <= 4000 && last >= FLOOR_KBPS);
    }

    /// A single calm window is not enough: the frame that caused the drop must
    /// not be able to undo it immediately.
    #[test]
    fn recovery_needs_two_calm_windows() {
        let mut q = QosController::new(4000);
        q.evaluate(sample(200.0, 0)); // -> 3000, Degraded
        let a = q.evaluate(sample(10.0, 0));
        assert_eq!(a.state, QosState::Recovering);
        assert_eq!(a.target_kbps, 3000, "no climb on the first calm window");
        let b = q.evaluate(sample(10.0, 0));
        assert_eq!(b.target_kbps, 3300, "+10 % after the second");
        assert!(b.changed);
    }

    /// Climbing is gradual and lands exactly on the base, then stays there.
    #[test]
    fn recovery_climbs_back_to_base_and_stops() {
        let mut q = QosController::new(4000);
        q.evaluate(sample(200.0, 0));
        let mut last = 0;
        for _ in 0..100 {
            last = q.evaluate(sample(5.0, 0)).target_kbps;
        }
        assert_eq!(last, 4000);
        assert_eq!(q.state(), QosState::Normal);
        assert!(!q.state().throttled());
    }

    /// A PLI storm is a link symptom even with an empty queue: the queue only
    /// fills when the transport *stalls*, while packet loss shows up as the
    /// decoder re-requesting anchors.
    #[test]
    fn pli_storm_degrades_without_backpressure() {
        let mut q = QosController::new(4000);
        // 4 PLI in one 2 s window = 2/s.
        let step = q.evaluate(sample(5.0, 4));
        assert_eq!(step.state, QosState::Degraded);
        assert_eq!(step.target_kbps, 3000);
    }

    /// The controller differences the session-total PLI counter, so a session
    /// that lost a frame an hour ago must not stay degraded because of it.
    #[test]
    fn old_pli_does_not_degrade_a_quiet_link() {
        let mut q = QosController::new(4000);
        let first = q.evaluate(sample(5.0, 40));
        assert_eq!(first.state, QosState::Degraded, "40 PLI in one window");
        // Same total next window: no new loss.
        let mut q = QosController::new(4000);
        q.evaluate(sample(5.0, 40));
        let quiet = q.evaluate(sample(5.0, 40));
        assert!(!quiet.changed);
        assert_eq!(quiet.state, QosState::Recovering);
    }

    /// A new base (fps change, `vb,N`) resets the ceiling; a stale reduced
    /// target would otherwise cap a session the user just asked to be bigger.
    #[test]
    fn new_base_resets_the_ceiling() {
        let mut q = QosController::new(4000);
        q.evaluate(sample(200.0, 0));
        assert_eq!(q.target_kbps(), 3000);
        q.set_base(8000);
        assert_eq!(q.target_kbps(), 8000);
        assert_eq!(q.state(), QosState::Normal);
    }

    /// Tiny bases must not produce a floor above the base itself.
    #[test]
    fn floor_never_exceeds_a_small_base() {
        let mut q = QosController::new(500);
        let mut last = 0;
        for _ in 0..10 {
            last = q.evaluate(sample(500.0, 0)).target_kbps;
        }
        assert_eq!(last, 300, "absolute floor for a 500 kbps base");
        assert!(last <= 500);
    }
}
