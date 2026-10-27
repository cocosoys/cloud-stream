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

use std::time::Duration;

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
            total_frames,
            warm: self.warmed_up,
        }
    }

    /// Zero the counters and timing groups after a snapshot has been sent.
    pub fn reset_window(&mut self) {
        self.counters = FrameCounters::default();
        self.encode_timing.reset();
        self.pipeline_timing.reset();
    }
}

/// Lightweight snapshot handed to the stats timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct PipelineSnapshot {
    pub encoded_fps: f32,
    pub encode_ms: f32,
    pub pipeline_ms: f32,
    pub total_frames: u64,
    pub warm: bool,
}
