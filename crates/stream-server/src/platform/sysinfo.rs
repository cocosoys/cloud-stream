//! CPU / memory sampler feeding `stream_stats` (design 9.2 #3).
//!
//! # CPU scope: the whole machine, not one core (read this before changing it)
//!
//! `cpu_percent` is the **host's** CPU utilisation: 0 is an idle machine, 100
//! is every logical core saturated. It is the number an operator recognises
//! from Task Manager, so the panel answers "how loaded is this box" without the
//! reader knowing the core count.
//!
//! It used to be `process(pid).cpu_usage()` — sysinfo's *percent of one core*,
//! which on a 4 vCPU host reaches 400 when the machine is saturated. The panel
//! clamps at 100, so "one core busy" and "machine on fire" printed the same
//! number, and the figure described this process rather than the device. That
//! mismatch (a status panel reporting the server's appetite instead of the
//! host's health) is why the scope was widened. The old figure survives as
//! `process_cpu_percent` for diagnostics; the panel meter reads `cpu_percent`.
//!
//! How it is computed: `global_cpu_info().cpu_usage()` of the sysinfo fork
//! (0.29.10, re-exported by hbb_common so a second sysinfo copy is never pulled
//! in). On Windows that counter is PDH `\Processor(_Total)\% Idle Time`
//! subtracted from 100 — averaged over all logical processors, i.e. exactly the
//! whole-machine share. `refresh_cpu_usage()` is used on purpose instead of
//! `refresh_cpu()`: the latter also polls CPU frequency via
//! `CallNtPowerInformation`, and nothing displays frequency.
//!
//! Cadence: refreshed once per 1 s stats tick, never inside the capture loop.
//! Both CPU figures are deltas since the previous refresh, so the first sample
//! after construction only seeds the baseline and may read 0.

use hbb_common::sysinfo::{Pid, System};

/// One host-scope sample for the `stream_stats` message.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostSample {
    /// Host-wide CPU utilisation, percent of **all** logical cores, 0..=100.
    /// This is what the panel shows as `CPU`.
    pub cpu_percent: f32,
    /// This process alone, percent of **one** core (sysinfo semantics: a
    /// multithreaded encoder legitimately exceeds 100 here). Never fed to the
    /// panel's meter — it answers "is the stream server the one eating the
    /// CPU", which is a different question from "how loaded is the host".
    pub process_cpu_percent: f32,
    /// Logical processors the host reports; context for the two figures above.
    pub cpu_cores: u32,
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
    /// Create the sampler: seed host memory, the CPU counters and the process
    /// table so the first CPU read already has a baseline delta to compare
    /// against (sysinfo reports 0 until two samples exist).
    pub fn new() -> anyhow::Result<Self> {
        let mut sys = System::new();
        sys.refresh_memory();
        // Seeds `\Processor(_Total)\% Idle Time` and one counter per logical
        // processor; this is what makes `global_cpu_info()` meaningful.
        sys.refresh_cpu_usage();
        sys.refresh_processes();
        let pid = Pid::from_u32(std::process::id());
        Ok(Self { sys, pid })
    }

    /// Refresh the whole-machine CPU counters, host memory, and only this
    /// process's CPU entry; walking every process each second would waste
    /// cycles the Minecraft instance could use.
    pub fn refresh(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let _ = self.sys.refresh_process(self.pid);
    }

    /// Return the most recent sample without refreshing.
    pub fn sample(&self) -> HostSample {
        // Whole machine (all logical processors averaged), clamped because a
        // PDH rounding wobble can return 100.4.
        let cpu_percent = self.sys.global_cpu_info().cpu_usage().clamp(0.0, 100.0);
        let process_cpu_percent = self
            .sys
            .process(self.pid)
            .map(|p| p.cpu_usage())
            .unwrap_or(0.0);
        HostSample {
            cpu_percent,
            process_cpu_percent,
            cpu_cores: self.sys.cpus().len() as u32,
            mem_used: self.sys.used_memory(),
            mem_total: self.sys.total_memory(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Smoke test for the widened CPU scope: the numbers must be sane after two
    /// refreshes (`global_cpu_info()` reports 0 until a baseline exists) and a
    /// busy CPU must move the host figure off zero — a PDH counter that failed
    /// to open would otherwise read 0 forever.
    #[test]
    fn samples_a_live_whole_machine_cpu() {
        let mut sampler = Sampler::new().expect("sampler");
        std::thread::sleep(Duration::from_millis(600));
        sampler.refresh();
        let idle = sampler.sample();
        assert!(idle.cpu_percent.is_finite());
        assert!(
            (0.0..=100.0).contains(&idle.cpu_percent),
            "cpu_percent {} must be a 0..=100 whole-machine share",
            idle.cpu_percent
        );
        assert!(idle.cpu_cores >= 1, "cpu_cores must be known");
        assert!(idle.process_cpu_percent >= 0.0);
        assert!(idle.mem_total > 0);

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers: Vec<_> = (0..idle.cpu_cores.max(1))
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(1200));
        sampler.refresh();
        let busy = sampler.sample();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in workers {
            let _ = w.join();
        }
        assert!(
            busy.cpu_percent > 30.0,
            "every logical core was spinning, so the host share must read high, got {}",
            busy.cpu_percent
        );
        assert!((0.0..=100.0).contains(&busy.cpu_percent));
    }
}
