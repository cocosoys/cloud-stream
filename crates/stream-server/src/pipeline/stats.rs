//! Pipeline counters and timing aggregates feeding `stream_stats` (design 4.3,
//! 5.4, 8.3). The capture loop owns this structure on one task; a snapshot is
//! taken once per 1 s stats tick to fill the outbound `StreamStats`.
//!
//! Counts mirror bench v2 exactly: `encoded_fps` uses the total (new + repeat)
//! frame count over the window; `encode_ms` averages new-frame encode only
//! (Ok / VpxNoOutput / Err all enter the new-frame group, matching bench
//! `new_encode`; repeat frames keep their own counters but no timing group);
//! `pipeline_ms` averages the successful new-frame capture -> YUV -> encode
//! end-to-end cost.
//!
//! #78 adds the transport half of the same picture, which the panel had no
//! source for: `bytes_sent` (the payload actually handed to the track, hence
//! `video_mbps`), `send_ms` (capture -> written, so the queue wait a congested
//! link introduces is included rather than hidden inside `pipeline_ms`), the
//! 250 ms peak, and the accumulated PLI/FIR count.

use std::time::{Duration, Instant};

/// Accumulated milliseconds for one timing group.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timing {
    sum_ms: f64,
    min_ms: f64,
    max_ms: f64,
    n: u32,
}

impl Timing {
    /// Add one measured sample in milliseconds. The first sample sets min/max.
    pub fn add(&mut self, ms: f64) {
        if self.n == 0 {
            self.min_ms = ms;
            self.max_ms = ms;
        } else {
            self.min_ms = self.min_ms.min(ms);
            self.max_ms = self.max_ms.max(ms);
        }
        self.sum_ms += ms;
        self.n += 1;
    }

    /// Mean cost over recorded samples; 0 when the group is empty.
    pub fn avg_ms(&self) -> f32 {
        if self.n > 0 {
            (self.sum_ms / self.n as f64) as f32
        } else {
            0.0
        }
    }

    /// Reset after a snapshot so the next window starts fresh.
    pub fn reset(&mut self) {
        *self = Timing::default();
    }

    /// Cheapest sample of the window; 0 when the group is empty (#78, the
    /// `send_min_ms` figure under the panel's "Capture to sent" tile).
    pub fn min_ms(&self) -> f32 {
        if self.n > 0 {
            self.min_ms as f32
        } else {
            0.0
        }
    }

    /// Most expensive sample of the window; 0 when the group is empty.
    pub fn max_ms(&self) -> f32 {
        if self.n > 0 {
            self.max_ms as f32
        } else {
            0.0
        }
    }
}

/// Widest 250 ms the window saw, in bits per second (#78 `peak_mbps`).
///
/// A one-second average hides the burst that actually broke a decoder's
/// reference chain: a keyframe followed by two full P-frames inside 250 ms
/// fits comfortably under a 4 Mbps/s average while tripling the instantaneous
/// rate on a 6 Mbit link. The panel draws that as "Peak, 250 ms", so the number
/// is measured over closed buckets rather than extrapolated from a partial one.
#[derive(Debug, Clone, Copy)]
pub struct PeakRate {
    bucket_start: Option<Instant>,
    bucket_bytes: u64,
    best_bps: f64,
}

/// One peak bucket. Long enough to span several frames at 30 fps (7-8), short
/// enough that a keyframe burst lands inside one.
const PEAK_BUCKET: Duration = Duration::from_millis(250);

impl Default for PeakRate {
    fn default() -> Self {
        Self {
            bucket_start: None,
            bucket_bytes: 0,
            best_bps: 0.0,
        }
    }
}

impl PeakRate {
    /// Add one written sample's payload at `now`.
    pub fn add(&mut self, bytes: u64, now: Instant) {
        let start = *self.bucket_start.get_or_insert(now);
        self.bucket_bytes += bytes;
        let elapsed = now.duration_since(start);
        if elapsed >= PEAK_BUCKET {
            self.close(elapsed);
            self.bucket_start = Some(now);
        }
    }

    /// Finish the open bucket and fold its rate into the best so far. A bucket
    /// shorter than 50 ms is dropped rather than extrapolated: one 60 KB
    /// keyframe over 8 ms would read as 60 Mbit and swamp the figure.
    fn close(&mut self, elapsed: Duration) {
        if elapsed >= Duration::from_millis(50) && self.bucket_bytes > 0 {
            let bps = self.bucket_bytes as f64 * 8.0 / elapsed.as_secs_f64();
            self.best_bps = self.best_bps.max(bps);
        }
        self.bucket_bytes = 0;
    }

    /// Best closed-bucket rate of the window in Mbps, and reset for the next
    /// one. The bucket in progress is closed first so a peak in the final
    /// quarter second is still reported.
    pub fn take_peak_mbps(&mut self, now: Instant) -> f32 {
        if let Some(start) = self.bucket_start {
            self.close(now.duration_since(start));
        }
        self.bucket_start = Some(now);
        let mbps = (self.best_bps / 1e6) as f32;
        self.best_bps = 0.0;
        if mbps > 0.0 {
            mbps
        } else {
            0.0
        }
    }
}

/// Per-window frame counters for the main capture loop.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameCounters {
    /// New frames acquired from the capturer this window.
    pub new_frames: u64,
    /// Re-encoded previous-YUV frames after WouldBlock, within repeat cap.
    pub repeat_frames: u64,
    /// WouldBlock timeouts that still produced a repeat or skipped one.
    pub would_block: u64,
    /// Frames reported invalid by the capturer.
    pub invalid: u64,
    /// Fatal capturer read errors this window.
    pub capture_err: u64,
    /// New frames encoded successfully.
    pub encoded_ok: u64,
    /// New-frame encoder VpxNoOutput (produced no packet this round).
    pub encode_no_output: u64,
    /// New-frame encoder errors outside VpxNoOutput.
    pub encode_err: u64,
    /// Repeat frames encoded successfully.
    pub repeat_ok: u64,
    /// Repeat frames skipped because repeat cap was reached or YUV was empty.
    pub repeat_skipped: u64,
    /// `track.write_sample` consecutive/total failures this window.
    pub write_sample_err: u64,
}

/// Everything the loop accumulates between stats snapshots.
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    pub counters: FrameCounters,
    /// New-frame encode_to_message cost.
    pub encode_timing: Timing,
    /// Capture + YUV conversion + encode end-to-end cost, new frames only.
    pub pipeline_timing: Timing,
    /// #78: capture beat start -> `write_sample` returned, new frames only.
    /// `pipeline_timing` stops at the encoder; the difference is the hand-off
    /// queue, which is exactly what grows when the link cannot keep up.
    pub send_timing: Timing,
    /// #78: payload bytes successfully queued for the track this window. The
    /// window mean becomes `video_mbps`; RTP/DTLS overhead is excluded because
    /// the browser counts its own received bytes for its side of the panel.
    pub bytes_sent: u64,
    /// #78: widest 250 ms of the window, for `peak_mbps`.
    pub peak: PeakRate,
    /// #78: encoder target in kbps as last applied (client `vb,` override or
    /// the fps-scaled `--bitrate`), so the panel can show "rate / target".
    pub target_kbps: u32,
    /// #78: PLI + FIR received from the browser since the session started.
    /// Deliberately not windowed: the panel reads it as "how many times the
    /// decoder lost its anchor", which is a session fact, not a rate.
    pub pli_requests: u64,
    /// Encoder warm-up / first-keyframe flag (design 8.3).
    pub warmed_up: bool,
    pub first_frame_seen: bool,
}

impl PipelineStats {
    /// Build the outbound sample for a `window` seconds slice. `total_fps`
    /// covers new + repeat frames, matching bench v2 `fps(total)`.
    pub fn snapshot(&self, window: Duration) -> PipelineSnapshot {
        let wall = window.as_secs_f64().max(1e-6);
        let total_frames = self.counters.new_frames + self.counters.repeat_ok;
        PipelineSnapshot {
            encoded_fps: (total_frames as f64 / wall) as f32,
            encode_ms: self.encode_timing.avg_ms(),
            pipeline_ms: self.pipeline_timing.avg_ms(),
            send_ms: self.send_timing.avg_ms(),
            send_min_ms: self.send_timing.min_ms(),
            send_max_ms: self.send_timing.max_ms(),
            video_mbps: (self.bytes_sent as f64 * 8.0 / wall / 1e6) as f32,
            target_mbps: self.target_kbps as f32 / 1000.0,
            peak_mbps: 0.0,
            pli_requests: self.pli_requests,
            total_frames,
            warm: self.warmed_up,
        }
    }

    /// Zero the counters and timing groups after a snapshot has been sent.
    /// `pli_requests` survives: it is a session total.
    pub fn reset_window(&mut self) {
        self.counters = FrameCounters::default();
        self.encode_timing.reset();
        self.pipeline_timing.reset();
        self.send_timing.reset();
        self.bytes_sent = 0;
    }
}

/// Lightweight snapshot handed to the stats timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineSnapshot {
    pub encoded_fps: f32,
    pub encode_ms: f32,
    pub pipeline_ms: f32,
    /// #78: capture -> written, mean / min / max over the window.
    pub send_ms: f32,
    pub send_min_ms: f32,
    pub send_max_ms: f32,
    /// #78: payload rate actually produced, and the encoder target it was
    /// produced against.
    pub video_mbps: f32,
    pub target_mbps: f32,
    /// Filled by the caller from `PeakRate::take_peak_mbps`, which needs `&mut`.
    pub peak_mbps: f32,
    /// #78: session PLI/FIR total.
    pub pli_requests: u64,
    pub total_frames: u64,
    pub warm: bool,
}
