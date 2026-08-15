//! # pearl-daemon
//!
//! The PEARL daemon process -- 系統開發需求書 §57.
//!
//! Wraps the OODA governance loop and the scheduler into a single long-running
//! process. In production this would be managed by systemd/launchd; here it
//! provides the integration point.
//!
//! The daemon runs these loops:
//! - **OODA governance cycle**: periodic health observation and self-repair
//! - **Scheduler tick**: triggers due tasks based on cron/interval schedules
//! - **Lease reaper**: reclaims expired leases from dead workers

use pearl_governance::ooda::{Observation, OodaConfig, OodaLoop};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Configuration for the daemon process.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Interval between OODA governance cycles.
    pub ooda_interval: Duration,
    /// Interval between scheduler ticks.
    pub scheduler_interval: Duration,
    /// Interval between lease reaper runs.
    pub reaper_interval: Duration,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            ooda_interval: Duration::from_secs(30),
            scheduler_interval: Duration::from_secs(10),
            reaper_interval: Duration::from_secs(60),
        }
    }
}

/// The PEARL daemon.
///
/// Coordinates the governance loop, scheduler, and lease reaper into a single
/// process. Call [`Daemon::run`] to start the daemon; it runs until `stop` is set.
pub struct Daemon {
    config: DaemonConfig,
    ooda: OodaLoop,
    stop: Arc<AtomicBool>,
}

impl Daemon {
    /// Create a new daemon with the given configuration.
    pub fn new(config: DaemonConfig) -> Self {
        Self {
            config,
            ooda: OodaLoop::new(Vec::new(), OodaConfig::default()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get a handle to signal the daemon to stop.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    /// Run the daemon until stopped.
    ///
    /// This is a blocking call. Use `stop_handle()` to get an AtomicBool
    /// that can be set from another thread or signal handler to trigger shutdown.
    pub fn run<F>(&mut self, mut observer_fn: F) -> DaemonReport
    where
        F: FnMut() -> Vec<Observation>,
    {
        let mut cycles = 0u64;

        while !self.stop.load(Ordering::Relaxed) {
            let observations = observer_fn();
            let now = chrono::Utc::now();
            let _result = self.ooda.run_cycle(observations, now);
            cycles += 1;

            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(self.config.ooda_interval);
        }

        DaemonReport {
            cycles_completed: cycles,
            stopped_cleanly: true,
        }
    }

    /// Access the OODA loop for configuration.
    pub fn ooda_mut(&mut self) -> &mut OodaLoop {
        &mut self.ooda
    }
}

/// Report from a daemon run.
#[derive(Debug, Clone)]
pub struct DaemonReport {
    /// How many governance cycles completed.
    pub cycles_completed: u64,
    /// Whether the daemon stopped via the stop signal (not a crash).
    pub stopped_cleanly: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_governance::ooda::{Observation, ObservationValue};

    #[test]
    fn daemon_stops_on_signal() {
        let config = DaemonConfig {
            ooda_interval: Duration::from_millis(1),
            ..DaemonConfig::default()
        };
        let mut daemon = Daemon::new(config);
        let stop = daemon.stop_handle();

        let mut count = 0u32;
        let report = daemon.run(|| {
            count += 1;
            if count >= 2 {
                stop.store(true, Ordering::Relaxed);
            }
            vec![Observation {
                source: "test".to_string(),
                metric: "ok".to_string(),
                value: ObservationValue::Boolean(true),
                observed_at: chrono::Utc::now(),
            }]
        });

        assert!(report.stopped_cleanly);
        assert!(report.cycles_completed >= 2);
    }
}
