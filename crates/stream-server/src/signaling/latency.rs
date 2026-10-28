//! #78: round-trip measurement over the control data channel.
//!
//! The direction matters and used to be inverted. `lib/webrtc.js:775-777`
//! answers a *server* `{"type":"ping"}` with a bare text line `pong,<epoch_s>`
//! on the same ordered channel; it never originates a ping. The server used to
//! sit on the other end of that contract — `classify("pong,...")` fell through
//! to `Action::Unknown` (dropped at debug), while `Action::Pong` echoed a pong
//! back to a client that had asked nothing — so no RTT was ever measured and
//! `stream_stats.rtt_ms` was omitted on purpose.
//!
//! This module owns the probe: one ping per second, the arrival time of the
//! outstanding one, an EMA over the samples, and the `latency_measurement`
//! line the frontend turns into its WebRTC RTT fallback
//! (`selkies-wr-core.js:3687-3689`, used when `currentRoundTripTime` is null).
//!
//! The RTT is computed from the local `Instant` of the send, never from the
//! payload: `<epoch_s>` comes from the browser's clock, so subtracting it from
//! ours would measure the clock skew, not the round trip.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hbb_common::serde_json::json;

/// How often a ping goes out. One second matches the `stream_stats` cadence so
/// every sample carries a fresh figure instead of a stale one.
pub const PING_INTERVAL: Duration = Duration::from_secs(1);

/// A ping outstanding for longer than this is counted as lost. Five intervals
/// is long enough to ride out a jitter spike and short enough that a dead
/// channel does not keep reporting the last good RTT forever.
const LOST_AFTER: Duration = Duration::from_secs(5);

/// Weight of the newest sample in the exponential average. 0.3 settles in a
/// few seconds while still showing a real change (a Wi-Fi retransmit burst
/// moves the number by more than one sample of noise would).
const EMA_ALPHA: f32 = 0.3;

#[derive(Debug, Default)]
struct Inner {
    /// Send time of the ping waiting for its reply; `None` between sending and
    /// the next interval, and after a reply was consumed.
    outstanding: Option<Instant>,
    /// Smoothed RTT in milliseconds; 0 until the first reply.
    rtt_ms: f32,
    /// Replies folded into `rtt_ms`.
    samples: u64,
    /// Pings that aged out without a reply.
    lost: u64,
}

/// Shared round-trip state for one session. Written by the ping task and the
/// control-channel hook, read by the stats timer.
#[derive(Debug, Default)]
pub struct LatencyProbe {
    inner: Mutex<Inner>,
}

impl LatencyProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the ping line and arm the timeout for its reply. Called once per
    /// `PING_INTERVAL` by the session's ping task; the payload carries the
    /// server clock only as a convenience, the measurement ignores it.
    pub fn ping_line(&self) -> String {
        let now = Instant::now();
        if let Ok(mut guard) = self.inner.lock() {
            // A ping still outstanding means the previous one is late: keep it
            // armed (a reply to it is still a valid sample) and let the timeout
            // below decide whether it counts as lost.
            if guard
                .outstanding
                .is_some_and(|sent| now.duration_since(sent) > LOST_AFTER)
            {
                guard.lost += 1;
                guard.outstanding = None;
            }
            if guard.outstanding.is_none() {
                guard.outstanding = Some(now);
            }
        }
        json!({
            "type": "ping",
            "data": { "t": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0) }
        })
        .to_string()
    }

    /// Disarm the outstanding ping after a send that never reached the wire
    /// (channel not open yet, or closing). Without this the probe would count
    /// a transport that was never asked as a lost round trip.
    pub fn cancel_pending(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.outstanding = None;
        }
    }

    /// Fold one `pong` into the average. Returns the smoothed RTT that should
    /// be announced, or `None` when the reply answered no ping (a duplicate or
    /// a stale line) and there is nothing new to report.
    pub fn on_reply(&self) -> Option<f32> {
        let now = Instant::now();
        let mut guard = self.inner.lock().ok()?;
        let sent = guard.outstanding.take()?;
        // A reply that took longer than the timeout belongs to a ping already
        // written off; taking it would fold one round trip into the average
        // twice.
        if now.duration_since(sent) > LOST_AFTER {
            guard.lost += 1;
            return None;
        }
        let sample = now.duration_since(sent).as_secs_f32() * 1000.0;
        guard.rtt_ms = if guard.samples == 0 {
            sample
        } else {
            guard.rtt_ms + EMA_ALPHA * (sample - guard.rtt_ms)
        };
        guard.samples += 1;
        // One line on the first reply settles "is the probe alive at all"
        // without a per-second log; the figure itself lives in `stream_stats`.
        if guard.samples == 1 {
            log::info!("rtt probe: first reply {sample:.1} ms over the control channel");
        }
        Some(guard.rtt_ms)
    }

    /// Smoothed RTT for `stream_stats.rtt_ms`; `None` until the first reply, so
    /// the field is omitted rather than reported as a fake 0 ms.
    pub fn rtt_ms(&self) -> Option<f32> {
        let guard = self.inner.lock().ok()?;
        (guard.samples > 0).then_some(guard.rtt_ms)
    }

    /// `(samples, lost)` for the session's one-line summary at teardown.
    pub fn counters(&self) -> (u64, u64) {
        match self.inner.lock() {
            Ok(guard) => (guard.samples, guard.lost),
            Err(_) => (0, 0),
        }
    }
}

/// `latency_measurement` downstream: the frontend's RTT fallback for engines
/// that report no `currentRoundTripTime` (`lib/webrtc.js:778-781`).
pub fn latency_measurement(rtt_ms: f32) -> String {
    json!({
        "type": "latency_measurement",
        "data": { "latency_ms": (rtt_ms * 10.0).round() / 10.0 }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_without_ping_is_ignored() {
        let probe = LatencyProbe::new();
        assert!(probe.on_reply().is_none());
        assert!(probe.rtt_ms().is_none());
    }

    #[test]
    fn first_reply_sets_the_average_and_ema_follows() {
        let probe = LatencyProbe::new();
        let _ = probe.ping_line();
        // No sleep: the round trip is ~0 ms here, so the assertion is about the
        // state machine (a sample was taken), not about the value.
        assert!(probe.on_reply().is_some());
        let first = probe.rtt_ms().expect("one sample folded");
        assert!(first < 1.0, "first sample read {first} ms for a local reply");
        let _ = probe.ping_line();
        assert!(probe.on_reply().is_some());
        let second = probe.rtt_ms().expect("two samples folded");
        // Both samples are ~0 ms, so the average stays near zero rather than
        // drifting; this pins that the EMA does not accumulate a bias.
        assert!(second < 1.0, "average drifted to {second} from {first}");
        assert_eq!(probe.counters(), (2, 0));
    }

    #[test]
    fn ping_line_is_the_contract_the_frontend_answers() {
        let probe = LatencyProbe::new();
        let line = probe.ping_line();
        assert!(line.contains("\"type\":\"ping\""), "unexpected line: {line}");
    }

    #[test]
    fn measurement_line_keeps_one_decimal() {
        let line = latency_measurement(23.456);
        assert!(line.contains("\"latency_ms\":23.5"), "unexpected: {line}");
    }
}
