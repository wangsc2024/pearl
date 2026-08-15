//! # pearl-daemon
//!
//! The long-running process that keeps the system moving — §47, §57.
//!
//! The daemon does the work nobody asked for but everything depends on. Its doc comment used
//! to promise a scheduler tick and a lease reaper; the loop only ran OODA cycles, so schedules
//! never fired and a crashed worker's task stayed claimed forever. Those loops are now real:
//!
//! ```text
//! every scheduler_interval  fire due schedules, submitting a task per occurrence
//! every reaper_interval     reclaim expired leases, promote elapsed backoffs
//! every ooda_interval       observe, orient, decide, act
//! ```
//!
//! Deliberately **not** in the daemon: executing tasks. That is the worker's job, in its own
//! process, so a scheduler bug cannot take execution down with it and a worker crash cannot
//! stop schedules from firing.
//!
//! Two properties worth naming:
//!
//! **A schedule points at a spec, not a task.** Each occurrence is submitted as a new task
//! with a timestamped id. A schedule that re-ran one task id would either collide with the
//! previous occurrence or overwrite its history.
//!
//! **Firing is recorded after submission, not before.** A crash between the two re-fires the
//! occurrence rather than skipping it. For a schedule, a duplicate is recoverable — the task
//! is idempotent by construction or its effects are keyed — and a silent miss is not.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::{Clock, TaskState};
use pearl_governance::ooda::{Observation, ObservationValue, OodaConfig, OodaLoop};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_queue::{RetryPolicy, WorkQueue};
use pearl_scheduler::{MisfirePolicy, Schedule, ScheduledTask, SchedulerEngine};
use pearl_state::{ScheduleRecord, StateStore, TaskSpec};

/// Configuration for the daemon process.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Interval between OODA governance cycles.
    pub ooda_interval: Duration,
    /// Interval between scheduler ticks.
    pub scheduler_interval: Duration,
    /// Interval between lease reaper runs.
    pub reaper_interval: Duration,
    /// How often the loop wakes up. The three intervals are multiples of this.
    pub tick: Duration,
    /// Where spec paths in schedules are resolved from.
    pub working_dir: Option<PathBuf>,
    /// Retry policy used when promoting backed-off tasks.
    pub retry_policy: RetryPolicy,
    /// Lease configuration used by the reaper.
    pub lease_config: LeaseConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            ooda_interval: Duration::from_secs(30),
            scheduler_interval: Duration::from_secs(10),
            reaper_interval: Duration::from_secs(60),
            tick: Duration::from_secs(1),
            working_dir: None,
            retry_policy: RetryPolicy::default(),
            lease_config: LeaseConfig::default(),
        }
    }
}

/// What one pass of the daemon's loops did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Schedules that fired, with the task id each occurrence was submitted as.
    pub triggered: Vec<(String, String)>,
    /// Leases reclaimed by the reaper.
    pub reclaimed: usize,
    /// Tasks promoted out of retry backoff.
    pub promoted: usize,
    /// Schedules that were due but could not be submitted, with the reason.
    pub failed: Vec<(String, String)>,
}

impl TickReport {
    pub fn is_empty(&self) -> bool {
        self.triggered.is_empty()
            && self.reclaimed == 0
            && self.promoted == 0
            && self.failed.is_empty()
    }
}

/// Daemon failures.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    State(#[from] pearl_state::StateError),
    #[error(transparent)]
    Lease(#[from] pearl_lease::LeaseError),
    #[error(transparent)]
    Queue(#[from] pearl_queue::QueueError),
    #[error("schedule '{schedule_id}' is neither cron nor interval, so it can never fire")]
    UnschedulableSchedule { schedule_id: String },
}

/// The PEARL daemon.
pub struct Daemon<C: Clock + Clone> {
    config: DaemonConfig,
    clock: C,
    ooda: OodaLoop,
    stop: Arc<AtomicBool>,
}

impl<C: Clock + Clone> Daemon<C> {
    /// Creates a daemon.
    pub fn new(config: DaemonConfig, clock: C) -> Self {
        Self {
            config,
            clock,
            ooda: OodaLoop::new(Vec::new(), OodaConfig::default()),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle that asks the daemon to finish its current pass and stop.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn ooda_mut(&mut self) -> &mut OodaLoop {
        &mut self.ooda
    }

    /// Fires every due schedule, submitting one task per occurrence.
    pub fn tick_scheduler(&self, store: &mut StateStore) -> Result<TickReport, DaemonError> {
        let mut report = TickReport::default();
        let schedules = store.list_schedules()?;
        if schedules.is_empty() {
            return Ok(report);
        }

        let now = self.clock.now();
        for record in schedules {
            if !record.enabled {
                continue;
            }
            let scheduled = match to_scheduled_task(&record) {
                Ok(task) => task,
                Err(e) => {
                    report
                        .failed
                        .push((record.schedule_id.clone(), e.to_string()));
                    continue;
                }
            };
            if !SchedulerEngine::<C>::is_due(&scheduled, now) {
                continue;
            }

            match self.submit_occurrence(store, &record, now) {
                Ok(task_id) => {
                    // Recorded *after* submission: a crash in between re-fires rather than
                    // skips, and a missed occurrence is the unrecoverable direction.
                    store.mark_schedule_triggered(&record.schedule_id, now, None)?;
                    report.triggered.push((record.schedule_id.clone(), task_id));
                }
                Err(e) => {
                    tracing::warn!(
                        schedule = %record.schedule_id,
                        error = %e,
                        "schedule was due but its occurrence could not be submitted"
                    );
                    report
                        .failed
                        .push((record.schedule_id.clone(), e.to_string()));
                }
            }
        }
        Ok(report)
    }

    /// Submits one occurrence of a schedule, admitted straight to the queue.
    fn submit_occurrence(
        &self,
        store: &mut StateStore,
        record: &ScheduleRecord,
        now: DateTime<Utc>,
    ) -> Result<String, DaemonError> {
        let path = match &self.config.working_dir {
            Some(dir) if !PathBuf::from(&record.spec_path).is_absolute() => {
                dir.join(&record.spec_path)
            }
            _ => PathBuf::from(&record.spec_path),
        };

        let spec = TaskSpec::load(&path).map_err(|e| pearl_state::StateError::PlanEncoding {
            detail: format!("schedule '{}': {e}", record.schedule_id),
        })?;
        let task_id = occurrence_id(&record.schedule_id, now);
        let submission = spec.into_submission_as(&task_id).map_err(|e| {
            pearl_state::StateError::PlanEncoding {
                detail: format!("schedule '{}': {e}", record.schedule_id),
            }
        })?;

        store.create_task(submission, now)?;
        // Straight through PLANNING and PLANNED: a scheduled task's plan was declared in its
        // spec, so there is nothing left to plan. The states are still traversed because the
        // machine forbids skipping them, and the history should show what happened.
        for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
            store.transition(
                &pearl_core::TaskId::parse(task_id.clone()).map_err(|e| {
                    pearl_state::StateError::PlanEncoding {
                        detail: e.to_string(),
                    }
                })?,
                state,
                Some(format!("scheduled by '{}'", record.schedule_id)),
                None,
                now,
            )?;
        }
        Ok(task_id)
    }

    /// Reclaims expired leases and promotes tasks whose backoff has elapsed.
    pub fn tick_reaper(&self, store: &mut StateStore) -> Result<TickReport, DaemonError> {
        let leases = LeaseManager::new(self.config.lease_config, self.clock.clone());
        let reaped = leases.reap(store)?;

        let queue = WorkQueue::new(
            self.config.retry_policy,
            pearl_core::RuntimeProfile::Normal,
            self.clock.clone(),
        );
        let promoted = queue.promote_ready_retries(store)?;

        Ok(TickReport {
            reclaimed: reaped.reclaimed.len(),
            promoted: promoted.len(),
            ..TickReport::default()
        })
    }

    /// Records what the daemon can see about the system — §60.
    ///
    /// Machine collectors first (§53 Observe): counts and ages, not interpretation.
    pub fn observe(&self, store: &mut StateStore) -> Result<Vec<Observation>, DaemonError> {
        let now = self.clock.now();
        let ready = store.count_by_state(TaskState::Ready)?;
        let running = store.count_by_state(TaskState::Running)?;
        let unverified = store.count_by_state(TaskState::Unverified)?;
        let blocked = store.count_by_state(TaskState::Blocked)?;
        let expired = store.expired_leases(now)?.len() as u64;

        let observations = vec![
            observation("queue", "ready", ready as f64, now),
            observation("queue", "running", running as f64, now),
            observation("assurance", "unverified", unverified as f64, now),
            observation("queue", "blocked", blocked as f64, now),
            observation("lease", "expired", expired as f64, now),
        ];

        // A health row per subsystem, so `pearl doctor` and an operator dashboard read the
        // same numbers the OODA loop reasoned about.
        store.record_health(
            "queue",
            if ready > 0 || running > 0 {
                "active"
            } else {
                "idle"
            },
            Some(&format!(
                "ready={ready} running={running} blocked={blocked}"
            )),
            now,
        )?;
        store.record_health(
            "lease",
            if expired > 0 { "degraded" } else { "healthy" },
            Some(&format!("expired={expired}")),
            now,
        )?;
        store.record_health(
            "assurance",
            if unverified > 0 {
                "attention"
            } else {
                "healthy"
            },
            Some(&format!("unverified={unverified}")),
            now,
        )?;

        Ok(observations)
    }

    /// Runs one pass of every loop that is due.
    pub fn tick_all(&mut self, store: &mut StateStore) -> Result<TickReport, DaemonError> {
        let mut report = self.tick_scheduler(store)?;
        let reaper = self.tick_reaper(store)?;
        report.reclaimed = reaper.reclaimed;
        report.promoted = reaper.promoted;

        let observations = self.observe(store)?;
        let now = self.clock.now();
        self.ooda.run_cycle(observations, now);

        Ok(report)
    }

    /// Runs until stopped, waking on `tick` and running each loop on its own interval.
    ///
    /// The intervals are honoured rather than collapsed into one, because they have different
    /// costs: reaping scans open leases, the scheduler reads spec files, and OODA is the most
    /// expensive of the three.
    pub fn run(&mut self, store: &mut StateStore) -> Result<DaemonReport, DaemonError> {
        let mut cycles = 0u64;
        let mut totals = TickReport::default();
        let mut last_schedule = self.clock.now();
        let mut last_reap = self.clock.now();
        let mut last_ooda = self.clock.now();
        let started = self.clock.now();

        tracing::info!(
            scheduler_interval_s = self.config.scheduler_interval.as_secs(),
            reaper_interval_s = self.config.reaper_interval.as_secs(),
            ooda_interval_s = self.config.ooda_interval.as_secs(),
            "daemon started"
        );

        while !self.stop.load(Ordering::Relaxed) {
            let now = self.clock.now();

            if elapsed(now, last_schedule, self.config.scheduler_interval) {
                last_schedule = now;
                let report = self.tick_scheduler(store)?;
                for (schedule, task) in &report.triggered {
                    tracing::info!(schedule = %schedule, task = %task, "schedule fired");
                }
                totals.triggered.extend(report.triggered);
                totals.failed.extend(report.failed);
            }

            if elapsed(now, last_reap, self.config.reaper_interval) {
                last_reap = now;
                let report = self.tick_reaper(store)?;
                if !report.is_empty() {
                    tracing::info!(
                        reclaimed = report.reclaimed,
                        promoted = report.promoted,
                        "reaper pass"
                    );
                }
                totals.reclaimed += report.reclaimed;
                totals.promoted += report.promoted;
            }

            if elapsed(now, last_ooda, self.config.ooda_interval) {
                last_ooda = now;
                let observations = self.observe(store)?;
                self.ooda.run_cycle(observations, now);
                cycles += 1;
            }

            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(self.config.tick);
        }

        Ok(DaemonReport {
            cycles_completed: cycles,
            uptime: self.clock.now() - started,
            totals,
            stopped_cleanly: true,
        })
    }
}

/// Report from a daemon run.
#[derive(Debug, Clone)]
pub struct DaemonReport {
    /// How many governance cycles completed.
    pub cycles_completed: u64,
    pub uptime: TimeDelta,
    /// What the loops did in aggregate.
    pub totals: TickReport,
    /// Whether the daemon stopped via the stop signal rather than a crash.
    pub stopped_cleanly: bool,
}

/// Whether `interval` has passed since `last`.
fn elapsed(now: DateTime<Utc>, last: DateTime<Utc>, interval: Duration) -> bool {
    (now - last).num_milliseconds() >= interval.as_millis() as i64
}

/// The task id for one occurrence of a schedule.
///
/// Second resolution: two occurrences of the same schedule cannot legitimately land in the
/// same second, and a coarser stamp would collide while a finer one would be noise.
fn occurrence_id(schedule_id: &str, at: DateTime<Utc>) -> String {
    format!("{schedule_id}-{}", at.format("%Y%m%dt%H%M%Sz"))
}

/// Converts a stored schedule into the scheduler's in-memory form.
fn to_scheduled_task(record: &ScheduleRecord) -> Result<ScheduledTask, DaemonError> {
    let schedule = match (&record.cron_expr, record.interval_secs) {
        (Some(expression), _) => Schedule::Cron {
            expression: expression.clone(),
            timezone: record.timezone.clone().unwrap_or_else(|| "UTC".to_string()),
        },
        (None, Some(secs)) => Schedule::Interval {
            every: TimeDelta::try_seconds(secs as i64)
                .unwrap_or_else(|| TimeDelta::try_seconds(60).expect("valid")),
        },
        (None, None) => {
            return Err(DaemonError::UnschedulableSchedule {
                schedule_id: record.schedule_id.clone(),
            })
        }
    };

    let mut task = ScheduledTask::new(
        // The scheduler keys on TaskId; a schedule id is a valid one by the same rules.
        pearl_core::TaskId::parse(record.schedule_id.clone()).map_err(|_| {
            DaemonError::UnschedulableSchedule {
                schedule_id: record.schedule_id.clone(),
            }
        })?,
        schedule,
        misfire_from(&record.misfire_policy),
    );
    task.last_triggered_at = record.last_triggered_at;
    task.enabled = record.enabled;
    Ok(task)
}

fn misfire_from(policy: &str) -> MisfirePolicy {
    match policy.to_ascii_lowercase().as_str() {
        "run_once" => MisfirePolicy::RunOnce,
        "run_all" => MisfirePolicy::RunAll,
        // Skip is the safe default: firing everything missed during an outage can be a
        // thundering herd, and for most schedules yesterday's occurrence is worthless.
        _ => MisfirePolicy::Skip,
    }
}

fn observation(source: &str, metric: &str, value: f64, at: DateTime<Utc>) -> Observation {
    Observation {
        source: source.to_string(),
        metric: metric.to_string(),
        value: ObservationValue::Numeric(value),
        observed_at: at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::TestClock;

    #[test]
    fn an_occurrence_id_is_unique_per_second_and_derived_from_the_schedule() {
        let at = DateTime::from_timestamp(1_786_838_400, 0).unwrap();
        let id = occurrence_id("daily.digest", at);
        assert!(id.starts_with("daily.digest-"), "got {id}");
        // Must remain a valid TaskId, or the occurrence could not be submitted.
        assert!(pearl_core::TaskId::parse(id.clone()).is_ok(), "got {id}");
        assert_ne!(
            id,
            occurrence_id("daily.digest", at + TimeDelta::try_seconds(1).unwrap())
        );
    }

    #[test]
    fn a_cron_schedule_converts_to_the_scheduler_form() {
        let record = ScheduleRecord::cron(
            "daily.digest",
            "digest",
            "tasks/digest.yaml",
            "0 7 * * *",
            Utc::now(),
        );
        let task = to_scheduled_task(&record).unwrap();
        assert!(matches!(task.schedule, Schedule::Cron { .. }));
        assert_eq!(task.misfire_policy, MisfirePolicy::Skip);
        assert!(task.enabled);
    }

    #[test]
    fn an_interval_schedule_converts_to_the_scheduler_form() {
        let record =
            ScheduleRecord::interval("health", "probe", "tasks/probe.yaml", 300, Utc::now());
        let task = to_scheduled_task(&record).unwrap();
        match task.schedule {
            Schedule::Interval { every } => assert_eq!(every.num_seconds(), 300),
            other => panic!("expected an interval, got {other:?}"),
        }
    }

    #[test]
    fn a_schedule_with_neither_cron_nor_interval_is_rejected() {
        // Such a schedule can never fire, so silently keeping it would be a schedule that
        // looks configured and does nothing.
        let mut record = ScheduleRecord::interval("broken", "t", "tasks/t.yaml", 60, Utc::now());
        record.interval_secs = None;
        assert!(matches!(
            to_scheduled_task(&record),
            Err(DaemonError::UnschedulableSchedule { .. })
        ));
    }

    #[test]
    fn misfire_policies_parse_and_default_to_skip() {
        assert_eq!(misfire_from("run_once"), MisfirePolicy::RunOnce);
        assert_eq!(misfire_from("RUN_ALL"), MisfirePolicy::RunAll);
        assert_eq!(misfire_from("skip"), MisfirePolicy::Skip);
        // An unrecognised policy must not fire everything missed during an outage.
        assert_eq!(misfire_from("whatever"), MisfirePolicy::Skip);
    }

    #[test]
    fn intervals_are_measured_against_the_injected_clock() {
        let clock = TestClock::new();
        let start = clock.now();
        assert!(!elapsed(clock.now(), start, Duration::from_secs(10)));
        clock.advance_secs(10);
        assert!(elapsed(clock.now(), start, Duration::from_secs(10)));
    }

    #[test]
    fn a_stop_handle_ends_the_loop() {
        let clock = TestClock::new();
        let mut daemon = Daemon::new(
            DaemonConfig {
                tick: Duration::from_millis(1),
                ..DaemonConfig::default()
            },
            clock,
        );
        let stop = daemon.stop_handle();
        stop.store(true, Ordering::Relaxed);

        let mut store = StateStore::open_in_memory().unwrap();
        let report = daemon.run(&mut store).unwrap();
        assert!(report.stopped_cleanly);
        assert_eq!(report.cycles_completed, 0, "a stopped daemon does no work");
    }
}
