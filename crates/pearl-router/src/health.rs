//! Health Monitor with profile degradation and auto-recovery.
//!
//! Tracks execution failures with time decay and drives runtime profile transitions:
//! - Normal -> Degraded when failure_count >= failure_threshold_degraded within decay_window
//! - Degraded -> Recovery when failure_count >= failure_threshold_recovery within decay_window
//! - Auto-recover to Normal when all failures have decayed (count drops to 0)
//!
//! Inspired by daily_rust health monitor pattern: track failures, decay old entries,
//! and mechanically degrade/recover the profile without agent involvement.

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::config::RuntimeProfile;
use pearl_core::{Clock, TaskId};
use serde::{Deserialize, Serialize};

/// Configuration for the health monitor thresholds and timing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthConfig {
    /// Number of failures within the decay window to trigger Normal -> Degraded.
    pub failure_threshold_degraded: u32,
    /// Number of failures within the decay window to trigger Degraded -> Recovery.
    pub failure_threshold_recovery: u32,
    /// How long a failure counts before it decays away.
    pub decay_window: TimeDelta,
    /// How often to check for profile transitions.
    pub check_interval: TimeDelta,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold_degraded: 3,
            failure_threshold_recovery: 5,
            decay_window: TimeDelta::try_minutes(5).expect("valid"),
            check_interval: TimeDelta::try_seconds(30).expect("valid"),
        }
    }
}

/// A recorded failure with timestamp for time-decay tracking.
#[derive(Debug, Clone)]
struct FailureRecord {
    /// Retained for diagnostics and future audit trail.
    #[allow(dead_code)]
    task_id: TaskId,
    at: DateTime<Utc>,
}

/// The Health Monitor tracks failures and manages profile degradation.
///
/// Profile transitions are purely mechanical: if the failure count within the decay
/// window exceeds a threshold, the profile degrades. When failures decay to zero, the
/// profile auto-recovers to Normal.
#[derive(Debug, Clone)]
pub struct HealthMonitor {
    config: HealthConfig,
    active_profile: RuntimeProfile,
    failures: Vec<FailureRecord>,
    last_check: DateTime<Utc>,
}

impl HealthMonitor {
    /// Create a new health monitor with the given config, starting in Normal profile.
    pub fn new(config: HealthConfig, clock: &dyn Clock) -> Self {
        Self {
            config,
            active_profile: RuntimeProfile::Normal,
            failures: Vec::new(),
            last_check: clock.now(),
        }
    }

    /// Create a health monitor with default configuration.
    pub fn with_defaults(clock: &dyn Clock) -> Self {
        Self::new(HealthConfig::default(), clock)
    }

    /// Record a task failure. This may trigger a profile degradation.
    pub fn record_failure(&mut self, task_id: TaskId, clock: &dyn Clock) {
        let now = clock.now();
        self.failures.push(FailureRecord { task_id, at: now });
        self.last_check = now;
    }

    /// Record a task success. Successes do not directly affect the failure count,
    /// but triggering a check can lead to recovery if failures have decayed.
    pub fn record_success(&mut self, _task_id: TaskId, clock: &dyn Clock) {
        self.last_check = clock.now();
    }

    /// Check whether a profile transition should occur.
    ///
    /// Returns `Some(new_profile)` if a transition happened, `None` if the profile
    /// remains the same.
    pub fn check_profile_transition(&mut self, clock: &dyn Clock) -> Option<RuntimeProfile> {
        let now = clock.now();
        self.last_check = now;

        // Decay: remove failures older than the decay window.
        self.prune_stale_failures(now);

        let failure_count = self.failures.len() as u32;
        let previous_profile = self.active_profile;

        // Determine target profile based on failure count.
        let target_profile = if failure_count >= self.config.failure_threshold_recovery {
            RuntimeProfile::Recovery
        } else if failure_count >= self.config.failure_threshold_degraded {
            RuntimeProfile::Degraded
        } else if failure_count == 0 {
            RuntimeProfile::Normal
        } else {
            // Between 0 and degraded threshold: stay at current or Normal.
            // If we're degraded/recovery and failures are below thresholds, start recovering.
            match self.active_profile {
                RuntimeProfile::Recovery => {
                    if failure_count < self.config.failure_threshold_recovery {
                        RuntimeProfile::Degraded
                    } else {
                        RuntimeProfile::Recovery
                    }
                }
                _ => self.active_profile,
            }
        };

        if target_profile != previous_profile {
            self.active_profile = target_profile;
            Some(target_profile)
        } else {
            None
        }
    }

    /// The current active runtime profile.
    pub fn active_profile(&self) -> RuntimeProfile {
        self.active_profile
    }

    /// The number of active (non-decayed) failures.
    pub fn active_failure_count(&self, clock: &dyn Clock) -> u32 {
        let now = clock.now();
        self.failures
            .iter()
            .filter(|f| now - f.at < self.config.decay_window)
            .count() as u32
    }

    /// When the last check occurred.
    pub fn last_check(&self) -> DateTime<Utc> {
        self.last_check
    }

    /// The health configuration.
    pub fn config(&self) -> &HealthConfig {
        &self.config
    }

    /// Remove failures that are older than the decay window.
    fn prune_stale_failures(&mut self, now: DateTime<Utc>) {
        self.failures
            .retain(|f| now - f.at < self.config.decay_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::TestClock;

    fn test_config() -> HealthConfig {
        HealthConfig {
            failure_threshold_degraded: 3,
            failure_threshold_recovery: 5,
            decay_window: TimeDelta::try_minutes(5).expect("valid"),
            check_interval: TimeDelta::try_seconds(30).expect("valid"),
        }
    }

    fn make_task_id() -> TaskId {
        TaskId::parse("test-task-1").expect("valid test id")
    }

    // -----------------------------------------------------------------------
    // Basic construction
    // -----------------------------------------------------------------------

    #[test]
    fn starts_in_normal_profile() {
        let clock = TestClock::new();
        let monitor = HealthMonitor::new(test_config(), &clock);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Normal);
    }

    #[test]
    fn with_defaults_uses_default_config() {
        let clock = TestClock::new();
        let monitor = HealthMonitor::with_defaults(&clock);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Normal);
        assert_eq!(monitor.config().failure_threshold_degraded, 3);
    }

    // -----------------------------------------------------------------------
    // Degradation on repeated failures
    // -----------------------------------------------------------------------

    #[test]
    fn degrades_to_degraded_on_threshold_failures() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 3 failures (= threshold_degraded).
        for _ in 0..3 {
            monitor.record_failure(make_task_id(), &clock);
        }

        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Degraded));
        assert_eq!(monitor.active_profile(), RuntimeProfile::Degraded);
    }

    #[test]
    fn degrades_to_recovery_on_higher_threshold() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 5 failures (= threshold_recovery).
        for _ in 0..5 {
            monitor.record_failure(make_task_id(), &clock);
        }

        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Recovery));
        assert_eq!(monitor.active_profile(), RuntimeProfile::Recovery);
    }

    #[test]
    fn stays_normal_below_threshold() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 2 failures (below threshold_degraded of 3).
        for _ in 0..2 {
            monitor.record_failure(make_task_id(), &clock);
        }

        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, None);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Normal);
    }

    // -----------------------------------------------------------------------
    // Auto-recovery when failures clear
    // -----------------------------------------------------------------------

    #[test]
    fn recovers_to_normal_when_failures_decay() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 3 failures to enter Degraded.
        for _ in 0..3 {
            monitor.record_failure(make_task_id(), &clock);
        }
        monitor.check_profile_transition(&clock);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Degraded);

        // Advance time past the decay window (5 minutes).
        clock.advance(TimeDelta::try_minutes(6).unwrap());

        // Check again: failures have decayed, should recover.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Normal));
        assert_eq!(monitor.active_profile(), RuntimeProfile::Normal);
    }

    #[test]
    fn recovers_from_recovery_through_degraded_to_normal() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 5 failures to enter Recovery.
        for _ in 0..5 {
            monitor.record_failure(make_task_id(), &clock);
        }
        monitor.check_profile_transition(&clock);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Recovery);

        // Advance past decay window.
        clock.advance(TimeDelta::try_minutes(6).unwrap());

        // All failures decayed -> Normal.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Normal));
        assert_eq!(monitor.active_profile(), RuntimeProfile::Normal);
    }

    // -----------------------------------------------------------------------
    // Time decay behavior
    // -----------------------------------------------------------------------

    #[test]
    fn profile_degradation_is_time_decayed() {
        let clock = TestClock::new();
        let config = HealthConfig {
            failure_threshold_degraded: 3,
            failure_threshold_recovery: 5,
            decay_window: TimeDelta::try_minutes(2).expect("valid"),
            check_interval: TimeDelta::try_seconds(30).expect("valid"),
        };
        let mut monitor = HealthMonitor::new(config, &clock);

        // Record 2 failures at t=0.
        monitor.record_failure(make_task_id(), &clock);
        monitor.record_failure(make_task_id(), &clock);

        // Advance 1.5 minutes (still within 2-min decay window).
        clock.advance(TimeDelta::try_seconds(90).unwrap());

        // Record 1 more failure at t=1.5m.
        monitor.record_failure(make_task_id(), &clock);

        // All 3 failures are within the decay window => degrade.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Degraded));

        // Now advance to t=2.5m. The first 2 failures from t=0 are now >2 min old.
        clock.advance(TimeDelta::try_seconds(60).unwrap());

        // Only 1 failure remains within the window (the one from t=1.5m).
        // Below threshold, profile should stay Degraded (count != 0).
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, None);

        // Advance past the last failure's decay: t=4m.
        clock.advance(TimeDelta::try_seconds(90).unwrap());

        // Now all failures have decayed.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Normal));
    }

    #[test]
    fn active_failure_count_respects_decay() {
        let clock = TestClock::new();
        let config = HealthConfig {
            failure_threshold_degraded: 3,
            failure_threshold_recovery: 5,
            decay_window: TimeDelta::try_minutes(1).expect("valid"),
            check_interval: TimeDelta::try_seconds(10).expect("valid"),
        };
        let mut monitor = HealthMonitor::new(config, &clock);

        // Record 3 failures.
        for _ in 0..3 {
            monitor.record_failure(make_task_id(), &clock);
        }
        assert_eq!(monitor.active_failure_count(&clock), 3);

        // Advance past decay window.
        clock.advance(TimeDelta::try_minutes(2).unwrap());
        assert_eq!(monitor.active_failure_count(&clock), 0);
    }

    // -----------------------------------------------------------------------
    // record_success does not affect failure count
    // -----------------------------------------------------------------------

    #[test]
    fn success_does_not_reduce_failure_count() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        monitor.record_failure(make_task_id(), &clock);
        monitor.record_failure(make_task_id(), &clock);
        monitor.record_success(make_task_id(), &clock);

        assert_eq!(monitor.active_failure_count(&clock), 2);
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn no_transition_when_already_at_correct_profile() {
        let clock = TestClock::new();
        let mut monitor = HealthMonitor::new(test_config(), &clock);

        // Record 3 failures, transition to Degraded.
        for _ in 0..3 {
            monitor.record_failure(make_task_id(), &clock);
        }
        let first = monitor.check_profile_transition(&clock);
        assert_eq!(first, Some(RuntimeProfile::Degraded));

        // Check again without new failures: no transition.
        let second = monitor.check_profile_transition(&clock);
        assert_eq!(second, None);
    }

    #[test]
    fn recovery_to_degraded_when_some_failures_decay() {
        let clock = TestClock::new();
        let config = HealthConfig {
            failure_threshold_degraded: 2,
            failure_threshold_recovery: 4,
            decay_window: TimeDelta::try_minutes(3).expect("valid"),
            check_interval: TimeDelta::try_seconds(10).expect("valid"),
        };
        let mut monitor = HealthMonitor::new(config, &clock);

        // Record 4 failures at t=0 -> Recovery.
        for _ in 0..4 {
            monitor.record_failure(make_task_id(), &clock);
        }
        monitor.check_profile_transition(&clock);
        assert_eq!(monitor.active_profile(), RuntimeProfile::Recovery);

        // Advance 2 minutes, add 1 more failure (total: 1 within window from t=2m,
        // plus 4 from t=0 which are still within 3-min window).
        clock.advance(TimeDelta::try_minutes(2).unwrap());
        monitor.record_failure(make_task_id(), &clock);

        // At t=2m, all 5 failures are still within the 3-min window -> stay Recovery.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, None);

        // Advance to t=3.5m. The 4 failures from t=0 have decayed (>3min).
        // Only 1 failure (from t=2m) remains.
        clock.advance(TimeDelta::try_seconds(90).unwrap());

        // 1 failure < threshold_recovery (4) and < threshold_degraded (2) => Degraded
        // Actually 1 < 2 so we go back further.
        // From Recovery: failure_count(1) < threshold_recovery(4), so transition to Degraded.
        let transition = monitor.check_profile_transition(&clock);
        assert_eq!(transition, Some(RuntimeProfile::Degraded));
    }
}
