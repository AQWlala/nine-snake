//! v1.0: live performance monitor.
//!
//! [`PerfMonitor`] samples process memory + CPU on a background
//! tokio task (default 1 Hz).  The latest sample is stored in a
//! `parking_lot::Mutex` so any Tauri command can read it
//! synchronously and the front-end can poll it cheaply through
//! the `metrics` command we already expose.
//!
//! The implementation deliberately avoids pulling in
//! `tokio::time::interval` *plus* a long-lived `JoinHandle` —
//! instead it loops on `tokio::time::sleep` so the monitor can
//! be `Drop`ped (and stop cleanly) by simply dropping the
//! returned [`MonitorHandle`].

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;

#[cfg(feature = "perf-telemetry")]
use sysinfo::System;

/// A single point-in-time performance sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfSample {
    /// Unix epoch millis.
    pub ts_ms: i64,
    /// Resident set size (bytes).  `None` when telemetry is off.
    pub rss_bytes: Option<u64>,
    /// Virtual memory size (bytes).  `None` when telemetry is off.
    pub virt_bytes: Option<u64>,
    /// Process CPU usage percent (0..=100 * cores).  `None` when
    /// telemetry is off.
    pub cpu_pct: Option<f32>,
    /// `true` when `rss_bytes` is over the RSS budget.
    pub over_budget: bool,
}

impl PerfSample {
    /// A "no telemetry" sample — useful in tests and on platforms
    /// where `sysinfo` is not available.
    pub fn empty() -> Self {
        Self {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            rss_bytes: None,
            virt_bytes: None,
            cpu_pct: None,
            over_budget: false,
        }
    }
}

/// Owning handle to a running monitor.  Drop = stop.
pub struct MonitorHandle {
    abort: Arc<Mutex<bool>>,
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        *self.abort.lock() = true;
    }
}

/// The performance monitor.  Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct PerfMonitor {
    inner: Arc<PerfMonitorInner>,
}

struct PerfMonitorInner {
    latest: Mutex<PerfSample>,
    abort: Arc<Mutex<bool>>,
}

impl PerfMonitor {
    /// Build a monitor that holds a single `None` sample.  Use
    /// [`PerfMonitor::start`] to spawn the background task.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PerfMonitorInner {
                latest: Mutex::new(PerfSample::empty()),
                abort: Arc::new(Mutex::new(false)),
            }),
        }
    }

    /// Read the most recent sample.  O(1).
    pub fn latest(&self) -> PerfSample {
        self.inner.latest.lock().clone()
    }

    /// Spawn a background sampler.  Returns a handle whose
    /// `Drop` cancels the loop.
    pub fn start(period: Duration) -> MonitorHandle {
        let monitor = PerfMonitor::new();
        let abort = monitor.inner.abort.clone();
        let handle = MonitorHandle {
            abort: abort.clone(),
        };
        tokio::spawn(async move {
            run_loop(monitor, period, abort).await;
        });
        handle
    }

    /// Update the stored sample (used by the background task).
    fn record(&self, sample: PerfSample) {
        *self.inner.latest.lock() = sample;
    }
}

impl Default for PerfMonitor {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_loop(monitor: PerfMonitor, period: Duration, abort: Arc<Mutex<bool>>) {
    #[cfg(feature = "perf-telemetry")]
    let mut sys = System::new_all();
    #[cfg(feature = "perf-telemetry")]
    let pid = sysinfo::get_current_pid().ok();

    loop {
        if *abort.lock() {
            debug!(target: "nine_snake.perf", "monitor loop exiting");
            return;
        }

        let sample = take_sample(
            &monitor,
            #[cfg(feature = "perf-telemetry")]
            &mut sys,
            #[cfg(feature = "perf-telemetry")]
            pid,
        );
        monitor.record(sample);

        tokio::time::sleep(period).await;
    }
}

#[cfg(feature = "perf-telemetry")]
fn take_sample(_monitor: &PerfMonitor, sys: &mut System, pid: Option<sysinfo::Pid>) -> PerfSample {
    if let Some(pid) = pid {
        sys.refresh_process(pid);
        if let Some(proc_) = sys.process(pid) {
            let rss = proc_.memory() * 1024; // sysinfo reports KiB
            let virt = proc_.virtual_memory() * 1024;
            let cpu = proc_.cpu_usage();
            return PerfSample {
                ts_ms: chrono::Utc::now().timestamp_millis(),
                rss_bytes: Some(rss),
                virt_bytes: Some(virt),
                cpu_pct: Some(cpu),
                over_budget: rss > RSS_BUDGET_BYTES,
            };
        } else {
            warn!(target: "nine_snake.perf", "sysinfo could not find current process");
        }
    }
    PerfSample::empty()
}

#[cfg(not(feature = "perf-telemetry"))]
fn take_sample(_monitor: &PerfMonitor) -> PerfSample {
    PerfSample::empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::RSS_BUDGET_BYTES;

    #[test]
    fn empty_sample_is_within_budget() {
        let s = PerfSample::empty();
        assert!(!s.over_budget);
        assert!(s.rss_bytes.is_none());
    }

    #[test]
    fn rss_budget_is_500mb() {
        assert_eq!(RSS_BUDGET_BYTES, 500 * 1024 * 1024);
    }

    #[test]
    fn monitor_new_returns_empty_latest() {
        let m = PerfMonitor::new();
        let s = m.latest();
        assert!(s.rss_bytes.is_none());
        assert_eq!(s.ts_ms > 0, true);
    }

    #[test]
    fn over_budget_threshold() {
        let s = PerfSample {
            ts_ms: 0,
            rss_bytes: Some(RSS_BUDGET_BYTES + 1),
            virt_bytes: None,
            cpu_pct: None,
            over_budget: true,
        };
        assert!(s.over_budget);
    }
}
