//! #83 A/V sync: pin the audio media clock to the wall clock.
//!
//! ## Why the obvious approach does not work here
//!
//! The first version of this module compared the two *media* clocks: audio
//! advances 20 ms per emitted frame, video advances `spf` per emitted frame, so
//! "audio ahead" meant dropping a frame. Deployed against a real desktop it read
//! `av=-16199ms/40fix` within a minute and kept dropping audio.
//!
//! The reasoning was wrong because this pipeline does **not** emit video at its
//! nominal rate. It sends on change: an idle desktop produces one or two frames
//! per second (the repeat path is throttled by `--repeat-every`), while audio
//! keeps a strict 50 frames/s from the WASAPI device. Video's media clock
//! therefore advances at whatever rate the desktop happens to change, and
//! `write_sample(data, spf)` stamps `spf` per frame regardless — RTP timestamps
//! here are decoder deltas, not a wall-clock timeline. Comparing the two
//! measures the desktop's activity, not drift.
//!
//! ## What is actually true
//!
//! Audio *is* produced at a fixed media rate: one 20 ms frame per loopback
//! period, 50 per second. The device clock is the only thing that can make that
//! deviate from real time (a 0.1 % slow endpoint is 60 ms after ten minutes and
//! half a second after an hour). Pinning audio's media clock to the wall clock
//! is therefore the whole fix: the receiver plays audio by media time and video
//! by arrival, so once audio's media time tracks real time the two stay
//! together, and a drifting audio device is caught instead of accumulating.
//!
//! Correction is deliberately slow: at most one 20 ms step every
//! [`FIX_MIN_INTERVAL`], and only past [`SKEW_LIMIT`]. A stream 30 ms out is
//! inaudible; chasing it would trade an inaudible error for an audible click.
//!
//! Why a process-wide singleton rather than a handle threaded through the media
//! session: there is exactly one audio thread per process (a second session
//! takes the first one over rather than running beside it), and that thread is
//! spawned from `on_track`, whose closure has no access to the session context.
//! Same shape as the audio counters in `pipeline::audio`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Beyond this the streams are visibly out of sync (one frame at 12.5 fps, well
/// under the ~100 ms where lip-sync reads as broken).
const SKEW_LIMIT: i64 = 80_000;
/// One step per this interval, so a correction is never a stutter.
const FIX_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// The first second is start-up: the loopback client delivers a burst of
/// buffered samples while the device settles, and measuring against it would
/// "correct" a startup artifact.
const WARMUP: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
struct State {
    /// Audio media time handed to the track, in microseconds.
    audio_us: i64,
    /// Wall clock at the first written frame — the zero of the comparison.
    started_at: Option<Instant>,
    last_fix_at: Option<Instant>,
    fixes: u64,
}

/// What the audio side should do to this period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    None,
    /// Emit one extra 20 ms silence frame: the audio media clock is behind real
    /// time (a slow capture endpoint).
    Advance,
    /// Drop one frame's samples: the audio media clock is ahead of real time.
    Retard,
}

pub struct AvClock {
    state: Mutex<State>,
}

impl AvClock {
    const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                audio_us: 0,
                started_at: None,
                last_fix_at: None,
                fixes: 0,
            }),
        }
    }

    /// Record one written audio frame of `dur` of media time. The first call
    /// starts the wall clock this is measured against.
    pub fn note_audio(&self, dur: Duration, now: Instant) {
        let mut st = self.lock();
        st.audio_us += dur.as_micros() as i64;
        if st.started_at.is_none() {
            st.started_at = Some(now);
        }
    }

    /// Decide whether this audio period should be stretched or squeezed.
    pub fn fix(&self, now: Instant) -> Fix {
        let mut st = self.lock();
        let Some(start) = st.started_at else {
            return Fix::None;
        };
        let wall = now.duration_since(start);
        if wall < WARMUP {
            return Fix::None;
        }
        if let Some(at) = st.last_fix_at {
            if now.duration_since(at) < FIX_MIN_INTERVAL {
                return Fix::None;
            }
        }
        let skew = st.audio_us - wall.as_micros() as i64;
        if skew < -SKEW_LIMIT {
            st.last_fix_at = Some(now);
            st.fixes += 1;
            Fix::Advance
        } else if skew > SKEW_LIMIT {
            st.last_fix_at = Some(now);
            st.fixes += 1;
            Fix::Retard
        } else {
            Fix::None
        }
    }

    /// Audio media time minus real time in milliseconds; positive means audio
    /// is running fast. `None` before the first frame.
    pub fn skew_ms(&self, now: Instant) -> Option<f32> {
        let st = self.lock();
        let start = st.started_at?;
        let wall = now.duration_since(start).as_micros() as i64;
        Some(((st.audio_us - wall) as f64 / 1000.0) as f32)
    }

    /// Steps applied since the session started, for the log line.
    pub fn fixes(&self) -> u64 {
        self.lock().fixes
    }

    /// New session: the media clock restarts at zero.
    pub fn reset(&self) {
        *self.lock() = State::default();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

static CLOCK: AvClock = AvClock::new();
/// Set once a session has written audio, so the 5 s pair log does not repeat the
/// same "not started" line forever.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// One written audio frame.
pub fn note_audio(dur: Duration) {
    ACTIVE.store(true, Ordering::Relaxed);
    CLOCK.note_audio(dur, Instant::now());
}

/// Correction for the audio period that is about to be emitted.
pub fn fix() -> Fix {
    CLOCK.fix(Instant::now())
}

/// `(skew_ms, fixes)` for the periodic log, `None` before audio starts.
pub fn report() -> Option<(f32, u64)> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    CLOCK.skew_ms(Instant::now()).map(|ms| (ms, CLOCK.fixes()))
}

/// Reset for a new session.
pub fn reset() {
    ACTIVE.store(false, Ordering::Relaxed);
    CLOCK.reset();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake audio device + the correction loop that consumes it.
    ///
    /// `rate` is how many 20 ms frames the device delivers per 20 ms of wall
    /// time: 1.0 keeps real time, 0.997 is a slow endpoint, 1.003 a fast one.
    struct Rig {
        clock: AvClock,
        now: Instant,
        rate: f64,
        pending: f64,
        /// Set by a `Retard` answer: the next frame the device produces is
        /// dropped instead of sent, which is what removes media time.
        skip_next: bool,
    }

    impl Rig {
        fn new(rate: f64) -> Self {
            Self {
                clock: AvClock::new(),
                now: Instant::now(),
                rate,
                pending: 0.0,
                skip_next: false,
            }
        }

        fn run(&mut self, seconds: u32) -> (u32, u32) {
            let (mut advanced, mut retarded) = (0, 0);
            for _ in 0..seconds * 50 {
                self.now += Duration::from_millis(20);
                // Whatever the device produced since the last period.
                self.pending += self.rate;
                while self.pending >= 1.0 {
                    self.pending -= 1.0;
                    if self.skip_next {
                        self.skip_next = false;
                        continue;
                    }
                    self.clock.note_audio(Duration::from_millis(20), self.now);
                }
                match self.clock.fix(self.now) {
                    Fix::Advance => {
                        // The extra silence frame the audio thread emits.
                        self.clock.note_audio(Duration::from_millis(20), self.now);
                        advanced += 1;
                    }
                    Fix::Retard => {
                        self.skip_next = true;
                        retarded += 1;
                    }
                    Fix::None => {}
                }
            }
            (advanced, retarded)
        }
    }

    /// A device that keeps real time needs no correction: the common case must
    /// not produce a stutter every two seconds.
    #[test]
    fn a_device_at_real_time_is_left_alone() {
        let mut rig = Rig::new(1.0);
        let (advanced, retarded) = rig.run(120);
        assert_eq!((advanced, retarded), (0, 0));
    }

    /// A slow device (0.3 % — two orders of magnitude worse than a real
    /// crystal) falls behind and gets nudged back with silence frames, and the
    /// corrections keep up rather than piling up.
    #[test]
    fn a_slow_device_is_caught_up() {
        let mut rig = Rig::new(0.997);
        let (advanced, retarded) = rig.run(400);
        assert_eq!(retarded, 0, "slow audio must never be retarded");
        assert!(advanced > 0, "no correction applied");
        // 400 s of wall time at one step per 2 s is 200 at most.
        assert!(advanced <= 210, "corrected too often: {advanced}");
    }

    /// A fast device is squeezed instead of stretched.
    #[test]
    fn a_fast_device_is_retarded() {
        let mut rig = Rig::new(1.003);
        let (advanced, retarded) = rig.run(200);
        assert_eq!(advanced, 0, "fast audio must never be advanced");
        assert!(retarded > 0);
    }

    /// Nothing happens during warm-up, however the device behaves: the loopback
    /// client's first second is a buffered burst, not a clock sample.
    #[test]
    fn warmup_is_exempt() {
        let c = AvClock::new();
        let t0 = Instant::now();
        c.note_audio(Duration::from_millis(20), t0);
        // One frame after 900 ms of wall time is 880 ms "behind" — warm-up.
        assert_eq!(c.fix(t0 + Duration::from_millis(900)), Fix::None);
    }

    /// Correction cannot run faster than `FIX_MIN_INTERVAL`; without the gate a
    /// large offset would be "fixed" with a burst of silence.
    #[test]
    fn corrections_are_rate_limited() {
        let c = AvClock::new();
        let t0 = Instant::now();
        c.note_audio(Duration::from_millis(20), t0);
        // 5 s of wall time with only 20 ms of media sent: audio is way behind.
        assert_eq!(c.fix(t0 + Duration::from_secs(5)), Fix::Advance);
        assert_eq!(
            c.fix(t0 + Duration::from_secs(5) + Duration::from_millis(100)),
            Fix::None
        );
        assert_eq!(
            c.fix(t0 + Duration::from_secs(5) + FIX_MIN_INTERVAL),
            Fix::Advance
        );
    }

    /// The regression for the first deploy: comparing against the video media
    /// clock read a paused desktop as 16 s of drift. The wall clock cannot do
    /// that, and this pins it down — the decision must be independent of whether
    /// any video frame has ever been written.
    #[test]
    fn only_the_wall_clock_matters() {
        let c = AvClock::new();
        let t0 = Instant::now();
        for i in 0..=250 {
            let at = t0 + Duration::from_millis(20 * i);
            c.note_audio(Duration::from_millis(20), at);
            assert_eq!(c.fix(at), Fix::None, "correction without any video at all");
        }
        assert_eq!(c.fixes(), 0);
    }

    /// `reset` clears both the media clock and the baseline, so a reconnect does
    /// not inherit the skew of the session that just ended.
    #[test]
    fn reset_forgets_the_previous_session() {
        let c = AvClock::new();
        let t0 = Instant::now();
        c.note_audio(Duration::from_millis(20), t0);
        assert_eq!(c.fix(t0 + Duration::from_secs(5)), Fix::Advance);
        c.reset();
        assert_eq!(c.fixes(), 0);
        assert!(c.skew_ms(t0).is_none());
    }

    /// The process-wide clock behind `report()` only speaks once audio exists.
    #[test]
    fn process_clock_reports_after_first_frame() {
        reset();
        assert!(report().is_none());
        note_audio(Duration::from_millis(20));
        assert!(report().is_some());
        reset();
    }
}
