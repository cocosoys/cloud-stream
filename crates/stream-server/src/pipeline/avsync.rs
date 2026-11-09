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
//!
//! ## #118: the clock only runs while audio runs
//!
//! The first version of *this* module still measured "wall time since the first
//! frame" against "media time written". On a desktop nobody is playing sound on,
//! WASAPI loopback delivers nothing (`Loopback::drain` returns after its 500 ms
//! timeout with an empty buffer), so no frame is ever written and the media
//! clock stands still while the wall clock keeps running. The skew then doubles
//! as fast as real time passes, and the corrector fires an `Advance` every
//! [`FIX_MIN_INTERVAL`] forever — measured on the deployed host as
//! `av=-140687920ms/70963fix`, i.e. it had been "correcting" a 39-hour offset
//! for two days and 19 hours, one 20 ms silence frame at a time.
//!
//! That is not drift, it is silence on both ends of the link: the receiver is
//! not playing anything either, so there is no lip-sync to preserve. The fix is
//! to advance the wall clock **only across the gaps between consecutive audio
//! frames**: a gap longer than [`MAX_FRAME_GAP`] is a stall (idle desktop, device
//! restart, or a scheduling hiccup), and on resume both sides are re-aligned
//! instead of pretending the missing seconds were media time. A real slow
//! device never gaps — it keeps delivering 20 ms frames slightly late — so its
//! drift is still measured, just without the idle noise.

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
/// "correct" a startup artifact. Measured in *audio* time, not wall time: a
/// session that only starts producing sound after ten minutes has still had no
/// chance to drift during those ten minutes.
const WARMUP: Duration = Duration::from_secs(1);
/// A gap between two written frames longer than this is a stall, not a slow
/// device (#118). 250 ms is twelve frames — far beyond any jitter a working
/// loopback produces, and far below the seconds an idle desktop can last.
const MAX_FRAME_GAP: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
struct State {
    /// Audio media time handed to the track, in microseconds.
    audio_us: i64,
    /// Wall time that passed **while audio was flowing**, in microseconds.
    /// Not `now - started_at`: idle stretches are excluded (#118).
    wall_us: i64,
    /// When the last frame was written; `None` before the first one.
    last_note_at: Option<Instant>,
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
                wall_us: 0,
                last_note_at: None,
                last_fix_at: None,
                fixes: 0,
            }),
        }
    }

    /// Record one written audio frame of `dur` of media time.
    ///
    /// The wall clock advances by the gap since the previous frame — **unless**
    /// that gap is longer than [`MAX_FRAME_GAP`], in which case the two clocks
    /// are re-aligned instead. Re-aligning rather than accumulating is what
    /// makes both stall shapes harmless: silence (audio did not advance, wall
    /// did) and a catch-up burst (audio jumped ahead by however many frames the
    /// device buffered) both end with `skew = 0` instead of a pile of
    /// corrections that would take minutes to unwind.
    pub fn note_audio(&self, dur: Duration, now: Instant) {
        let mut st = self.lock();
        match st.last_note_at {
            None => {
                // First frame: no interval to measure yet. Media time starts at
                // one frame, wall time at zero, so the very first reading is
                // +20 ms — an order of magnitude below `SKEW_LIMIT`.
            }
            Some(prev) => {
                let gap = now.saturating_duration_since(prev);
                if gap > MAX_FRAME_GAP {
                    st.wall_us = st.audio_us;
                } else {
                    st.wall_us += gap.as_micros() as i64;
                }
            }
        }
        st.last_note_at = Some(now);
        st.audio_us += dur.as_micros() as i64;
    }

    /// Decide whether this audio period should be stretched or squeezed.
    pub fn fix(&self, now: Instant) -> Fix {
        let mut st = self.lock();
        let Some(last) = st.last_note_at else {
            return Fix::None;
        };
        // Nothing has been written for a while: the receiver is not playing
        // anything, so there is no drift to correct and no timeline to move.
        if now.saturating_duration_since(last) > MAX_FRAME_GAP {
            return Fix::None;
        }
        // Warm-up is measured in audio time: a session that has only produced
        // half a second of sound has not had a chance to drift.
        if st.audio_us < WARMUP.as_micros() as i64 {
            return Fix::None;
        }
        if let Some(at) = st.last_fix_at {
            if now.duration_since(at) < FIX_MIN_INTERVAL {
                return Fix::None;
            }
        }
        let skew = st.audio_us - st.wall_us;
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

    /// Audio media time minus the time audio actually ran, in milliseconds;
    /// positive means audio is running fast. `None` before the first frame.
    pub fn skew_ms(&self, _now: Instant) -> Option<f32> {
        let st = self.lock();
        st.last_note_at?;
        Some(((st.audio_us - st.wall_us) as f64 / 1000.0) as f32)
    }

    /// How long the audio track has been silent, if it has.
    ///
    /// Reported next to the skew so `av=+12ms` on an idle desktop is not
    /// mistaken for a healthy reading: it is a frozen clock, not a synced one.
    pub fn idle_for(&self, now: Instant) -> Option<Duration> {
        let st = self.lock();
        let last = st.last_note_at?;
        let idle = now.saturating_duration_since(last);
        (idle > MAX_FRAME_GAP).then_some(idle)
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

/// One reading for the periodic log.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    /// Media time minus the time audio actually ran; positive = audio fast.
    pub skew_ms: f32,
    /// Correction steps applied since the session started.
    pub fixes: u64,
    /// How long the track has been silent; `None` while audio is flowing.
    pub idle_ms: Option<u64>,
}

/// `(skew, fixes, idle)` for the periodic log, `None` before audio starts.
pub fn report() -> Option<Report> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    let now = Instant::now();
    CLOCK.skew_ms(now).map(|skew_ms| Report {
        skew_ms,
        fixes: CLOCK.fixes(),
        idle_ms: CLOCK.idle_for(now).map(|d| d.as_millis() as u64),
    })
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
    /// client's first second is a buffered burst, not a clock sample. Warm-up is
    /// counted in *audio* time, so a burst of thirty frames inside 100 ms of wall
    /// time is still exempt.
    #[test]
    fn warmup_is_exempt() {
        let c = AvClock::new();
        let t0 = Instant::now();
        for i in 0..30 {
            let at = t0 + Duration::from_millis(3 * i);
            c.note_audio(Duration::from_millis(20), at);
            // 600 ms of media inside 87 ms of wall is 513 ms "ahead".
            assert_eq!(c.fix(at), Fix::None, "warm-up must stay exempt");
        }
        assert_eq!(c.fixes(), 0);
    }

    /// Correction cannot run faster than `FIX_MIN_INTERVAL`; without the gate a
    /// large offset would be "fixed" with a burst of silence.
    ///
    /// The drift here is produced by a device that runs at a fifth of real time
    /// *without* ever gapping long enough to look like a stall — one 20 ms frame
    /// every 100 ms. An idle gap would be re-aligned instead (#118).
    #[test]
    fn corrections_are_rate_limited() {
        let c = AvClock::new();
        let mut at = Instant::now();
        for _ in 0..60 {
            c.note_audio(Duration::from_millis(20), at);
            at += Duration::from_millis(100);
        }
        // 1.2 s of media over 5.9 s of wall: 4.7 s behind.
        assert_eq!(c.fix(at), Fix::Advance);
        assert_eq!(
            c.fix(at + Duration::from_millis(100)),
            Fix::None,
            "second step inside FIX_MIN_INTERVAL"
        );
        // Keep producing (so the idle guard stays out of the way) until the
        // interval expires, then the same skew must buy another step.
        for _ in 0..20 {
            c.note_audio(Duration::from_millis(20), at + Duration::from_millis(100));
            at += Duration::from_millis(100);
        }
        assert_eq!(c.fix(at), Fix::Advance);
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
        let mut at = Instant::now();
        for _ in 0..60 {
            c.note_audio(Duration::from_millis(20), at);
            at += Duration::from_millis(100);
        }
        assert_eq!(c.fix(at), Fix::Advance);
        c.reset();
        assert_eq!(c.fixes(), 0);
        assert!(c.skew_ms(at).is_none());
    }

    /// #118, the regression for the deployed host: a desktop with nothing
    /// playing produces no loopback frames at all, and that used to read as
    /// drift — `av=-140687920ms/70963fix`, one silence frame injected every two
    /// seconds for two days. Silence on both ends is not a lip-sync error.
    #[test]
    fn an_idle_gap_is_not_drift() {
        let c = AvClock::new();
        let mut at = Instant::now();
        // Ten minutes of sound.
        for _ in 0..50 * 60 * 10 {
            c.note_audio(Duration::from_millis(20), at);
            at += Duration::from_millis(20);
        }
        let before = c.fixes();
        // Eight hours of nothing, then sound again.
        at += Duration::from_secs(8 * 3600);
        for _ in 0..300 {
            c.note_audio(Duration::from_millis(20), at);
            at += Duration::from_millis(20);
        }
        assert_eq!(c.fixes(), before, "silence must not be corrected");
        assert_eq!(c.fix(at), Fix::None);
        let skew = c.skew_ms(at).expect("audio has been written");
        assert!(skew.abs() < 80.0, "skew after an idle gap: {skew}");
    }

    /// While the track is idle there is nothing to correct, and the log must say
    /// so rather than print a frozen skew that looks healthy.
    #[test]
    fn idle_is_reported_instead_of_corrected() {
        let c = AvClock::new();
        let t0 = Instant::now();
        c.note_audio(Duration::from_millis(20), t0);
        assert!(c.idle_for(t0 + Duration::from_millis(100)).is_none());
        let idle = c.idle_for(t0 + Duration::from_secs(30)).expect("idle");
        assert_eq!(idle, Duration::from_secs(30));
        assert_eq!(c.fix(t0 + Duration::from_secs(30)), Fix::None);
        assert_eq!(c.fixes(), 0);
    }

    /// A stall that hands over the buffered backlog in one burst must stay
    /// bounded by the endpoint buffer (`BUFFER_HNS` = 200 ms), not by the length
    /// of the stall: without the re-align this read as "3 s behind".
    #[test]
    fn a_burst_after_a_stall_stays_bounded() {
        let c = AvClock::new();
        let mut at = Instant::now();
        for _ in 0..100 {
            c.note_audio(Duration::from_millis(20), at);
            at += Duration::from_millis(20);
        }
        // 3 s of silence, then the device dumps its 200 ms buffer at once.
        at += Duration::from_secs(3);
        for _ in 0..10 {
            c.note_audio(Duration::from_millis(20), at);
        }
        let skew = c.skew_ms(at).expect("audio has been written");
        assert!(skew > 0.0 && skew <= 220.0, "burst skew: {skew}");
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
