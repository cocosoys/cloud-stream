//! CPU / memory sampler feeding `stream_stats` (design 9.2 #3).
//!
//! Stage1 decision: `cpu_percent` is this process's own CPU occupancy (percent
//! of one core, matching the 0e bench single-core column); `mem_used` /
//! `mem_total` are host-scope figures. The sysinfo rlim_max fork 0.29.10 comes
//! through hbb_common's re-export so a second sysinfo copy is never pulled in.
//!
//! Cadence: refreshed once per 1 s stats tick, never inside the capture loop.
//! sysinfo `cpu_usage()` returns a delta since the previous refresh, so the
//! first `refresh()` after construction only seeds the baseline and reports 0;
//! the second onward carries a comparable value.

use hbb_common::sysinfo::{Pid, System};

/// One host-scope sample for the `stream_stats` message.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSample {
    /// This process's own CPU occupancy, percent of one core.
    pub cpu_percent: f32,
    /// Host used memory, bytes.
    pub mem_used: u64,
    /// Host total memory, bytes.
    pub mem_total: u64,
}

/// Owns the sysinfo handles between periodic samples.
pub struct Sampler {
    sys: System,
    pid: Pid,
}

impl Sampler {
    /// Create the sampler: seed the host memory and the process table so the
    /// first CPU read already has a baseline delta to compare against.
    pub fn new() -> anyhow::Result<Self> {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_processes();
        let pid = Pid::from_u32(std::process::id());
        Ok(Self { sys, pid })
    }

    /// Refresh host memory and only this process's CPU entry; walking every
    /// process each second would waste cycles the Minecraft instance could use.
    pub fn refresh(&mut self) {
        self.sys.refresh_memory();
        let _ = self.sys.refresh_process(self.pid);
    }

    /// Return the most recent sample without refreshing.
    pub fn sample(&self) -> HostSample {
        let cpu_percent = self
            .sys
            .process(self.pid)
            .map(|p| p.cpu_usage())
            .unwrap_or(0.0);
        HostSample {
            cpu_percent,
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
        }
    }
}
