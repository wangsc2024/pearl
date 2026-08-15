//! The daemon's loops, end to end — §47, §34.
//!
//! The daemon's doc comment used to promise a scheduler tick and a lease reaper while the loop
//! only ran OODA cycles. These tests are what stops that from being true again: each one fails
//! if the corresponding loop stops doing anything.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::TimeDelta;
use pearl_core::{Clock, TaskState, TestClock, WorkerId};
use pearl_daemon::{Daemon, DaemonConfig};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_state::{ScheduleRecord, StateStore};
use tempfile::TempDir;

/// A task spec that names a capability and a verifier, so an occurrence carries a real plan.
const SPEC: &str = r#"
id: daily.digest
version: 1
task_type: digest
description: Assemble the daily digest
precision_class: p0
capability: script.task-score
timeout_seconds: 30
quality:
  exactness_required: true
  deterministic_generation: true
  deterministic_verification: true
assurance:
  - script: verifier.task-result
    evidence_required: true
"#;

fn fixture() -> (TempDir, StateStore, PathBuf) {
    let dir = TempDir::new().unwrap();
    let spec_path = dir.path().join("digest.yaml");
    std::fs::write(&spec_path, SPEC).unwrap();
    let store = StateStore::open(dir.path().join("pearl.db")).unwrap();
    (dir, store, spec_path)
}

fn daemon(clock: TestClock, working_dir: &Path) -> Daemon<TestClock> {
    Daemon::new(
        DaemonConfig {
            tick: Duration::from_millis(1),
            working_dir: Some(working_dir.to_path_buf()),
            ..DaemonConfig::default()
        },
        clock,
    )
}

#[test]
fn a_due_schedule_submits_an_occurrence_that_is_ready_to_run() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());

    store
        .upsert_schedule(&ScheduleRecord::interval(
            "daily.digest",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();

    let report = daemon.tick_scheduler(&mut store).unwrap();

    assert_eq!(report.triggered.len(), 1, "{report:?}");
    let (schedule_id, task_id) = &report.triggered[0];
    assert_eq!(schedule_id, "daily.digest");
    // A new task per occurrence, not a re-run of one id: each occurrence gets its own run,
    // attempts and evidence.
    assert!(task_id.starts_with("daily.digest-"), "got {task_id}");

    let task = store
        .get_task(&pearl_core::TaskId::parse(task_id.clone()).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        task.state,
        TaskState::Ready,
        "an occurrence must be claimable by a worker without further help"
    );
    assert_eq!(task.task_type, "digest");
    // The plan declared in the spec travels with the occurrence, so a scheduled run is
    // verified exactly as a manual one would be.
    assert_eq!(task.plan.capability.as_deref(), Some("script.task-score"));
    assert!(task.plan.has_assurance());
}

#[test]
fn the_history_shows_that_the_schedule_submitted_it() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "daily.digest",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();

    let report = daemon.tick_scheduler(&mut store).unwrap();
    let task_id = pearl_core::TaskId::parse(report.triggered[0].1.clone()).unwrap();

    let reasons: Vec<String> = store
        .ledger()
        .read_task(&task_id)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            pearl_events::PearlEvent::TaskStateChanged { reason, .. } => reason.clone(),
            _ => None,
        })
        .collect();
    assert!(
        reasons.iter().any(|r| r.contains("scheduled by")),
        "the ledger should say where this task came from: {reasons:?}"
    );
}

#[test]
fn an_interval_schedule_does_not_fire_twice_before_its_interval_elapses() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "every.minute",
            "digest",
            spec_path.to_string_lossy(),
            60,
            clock.now(),
        ))
        .unwrap();

    assert_eq!(
        daemon.tick_scheduler(&mut store).unwrap().triggered.len(),
        1
    );
    // Second pass, same instant: the last trigger was persisted, so nothing is due.
    assert_eq!(
        daemon.tick_scheduler(&mut store).unwrap().triggered.len(),
        0
    );

    clock.advance_secs(59);
    assert_eq!(
        daemon.tick_scheduler(&mut store).unwrap().triggered.len(),
        0
    );
    clock.advance_secs(1);
    assert_eq!(
        daemon.tick_scheduler(&mut store).unwrap().triggered.len(),
        1,
        "the interval elapsed, so it must fire again"
    );

    assert_eq!(store.count_by_state(TaskState::Ready).unwrap(), 2);
}

#[test]
fn the_last_trigger_survives_a_restart() {
    // The reason schedules are persisted at all: an in-memory scheduler restarting at 07:01
    // would either re-fire the 07:00 occurrence or skip it, depending on luck.
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let first_run = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "hourly",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();
    first_run.tick_scheduler(&mut store).unwrap();
    drop(store);

    // A fresh process, reading the same database.
    let mut reopened = StateStore::open(dir.path().join("pearl.db")).unwrap();
    let restarted = daemon(clock.clone(), dir.path());
    assert_eq!(
        restarted
            .tick_scheduler(&mut reopened)
            .unwrap()
            .triggered
            .len(),
        0,
        "a restart must not re-fire an occurrence that already happened"
    );
    assert!(reopened
        .get_schedule("hourly")
        .unwrap()
        .unwrap()
        .last_triggered_at
        .is_some());
}

#[test]
fn a_disabled_schedule_is_remembered_but_does_not_fire() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "paused",
            "digest",
            spec_path.to_string_lossy(),
            60,
            clock.now(),
        ))
        .unwrap();
    store.set_schedule_enabled("paused", false).unwrap();

    assert!(daemon
        .tick_scheduler(&mut store)
        .unwrap()
        .triggered
        .is_empty());
    // Disabled, not deleted: re-enabling must not require re-registering.
    assert!(store.get_schedule("paused").unwrap().is_some());

    store.set_schedule_enabled("paused", true).unwrap();
    assert_eq!(
        daemon.tick_scheduler(&mut store).unwrap().triggered.len(),
        1
    );
}

#[test]
fn a_schedule_with_a_missing_spec_is_reported_without_stopping_the_loop() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());

    store
        .upsert_schedule(&ScheduleRecord::interval(
            "broken",
            "digest",
            "no-such-spec.yaml",
            60,
            clock.now(),
        ))
        .unwrap();
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "healthy",
            "digest",
            spec_path.to_string_lossy(),
            60,
            clock.now(),
        ))
        .unwrap();

    let report = daemon.tick_scheduler(&mut store).unwrap();

    // One bad schedule must not prevent the others from firing: a daemon that stopped at the
    // first problem would turn one typo into a total outage.
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, "broken");
    assert_eq!(report.triggered.len(), 1);
    assert_eq!(report.triggered[0].0, "healthy");
}

#[test]
fn the_reaper_reclaims_a_lease_from_a_worker_that_disappeared() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "work",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();
    let task_id = pearl_core::TaskId::parse(
        daemon.tick_scheduler(&mut store).unwrap().triggered[0]
            .1
            .clone(),
    )
    .unwrap();

    // A worker claims it and vanishes.
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());
    leases
        .claim(&mut store, &task_id, &WorkerId::new("worker:doomed"))
        .unwrap();
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Leased
    );

    clock.advance_secs(3600);
    let report = daemon.tick_reaper(&mut store).unwrap();

    assert_eq!(report.reclaimed, 1, "{report:?}");
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Ready,
        "nothing ran, so the task is safe to offer again immediately"
    );
}

#[test]
fn the_reaper_promotes_a_task_whose_backoff_has_elapsed() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "work",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();
    let task_id = pearl_core::TaskId::parse(
        daemon.tick_scheduler(&mut store).unwrap().triggered[0]
            .1
            .clone(),
    )
    .unwrap();

    // Walk it into RETRY_WAIT the way a failed attempt would.
    for state in [TaskState::Leased, TaskState::Running, TaskState::RetryWait] {
        store
            .transition(&task_id, state, None, None, clock.now())
            .unwrap();
    }
    assert_eq!(daemon.tick_reaper(&mut store).unwrap().promoted, 0);

    // Default policy: 30s base backoff.
    clock.advance(TimeDelta::try_seconds(31).unwrap());
    assert_eq!(daemon.tick_reaper(&mut store).unwrap().promoted, 1);
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Ready
    );
}

#[test]
fn observation_records_health_per_subsystem() {
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "work",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();
    daemon.tick_scheduler(&mut store).unwrap();

    let observations = daemon.observe(&mut store).unwrap();
    assert!(
        observations.iter().any(|o| o.metric == "ready"),
        "{observations:?}"
    );

    // §60: the numbers the governance loop reasoned about are the numbers an operator sees.
    let health = store.latest_health().unwrap();
    let subsystems: Vec<&str> = health.iter().map(|h| h.subsystem.as_str()).collect();
    assert!(subsystems.contains(&"queue"), "{subsystems:?}");
    assert!(subsystems.contains(&"lease"), "{subsystems:?}");
    assert!(subsystems.contains(&"assurance"), "{subsystems:?}");
    let queue = health.iter().find(|h| h.subsystem == "queue").unwrap();
    assert!(queue.detail.as_deref().unwrap().contains("ready=1"));
}

#[test]
fn one_pass_of_every_loop_is_a_complete_unit_of_work() {
    // What `--once` runs, and what a cron-driven deployment relies on.
    let (dir, mut store, spec_path) = fixture();
    let clock = TestClock::new();
    let mut daemon = daemon(clock.clone(), dir.path());
    store
        .upsert_schedule(&ScheduleRecord::interval(
            "work",
            "digest",
            spec_path.to_string_lossy(),
            3600,
            clock.now(),
        ))
        .unwrap();

    let report = daemon.tick_all(&mut store).unwrap();
    assert_eq!(report.triggered.len(), 1);
    assert!(report.failed.is_empty());
    assert_eq!(store.count_by_state(TaskState::Ready).unwrap(), 1);
    assert!(!store.latest_health().unwrap().is_empty());
}

#[test]
fn a_cron_schedule_fires_on_a_matching_minute() {
    let (dir, mut store, spec_path) = fixture();
    // The test clock starts at 2026-08-15T00:00:00Z, so a schedule for minute 0 of hour 0 is
    // due immediately and one for hour 7 is not.
    let clock = TestClock::new();
    let daemon = daemon(clock.clone(), dir.path());

    store
        .upsert_schedule(&ScheduleRecord::cron(
            "at.seven",
            "digest",
            spec_path.to_string_lossy(),
            "0 7 * * *",
            clock.now(),
        ))
        .unwrap();
    store
        .upsert_schedule(&ScheduleRecord::cron(
            "at.midnight",
            "digest",
            spec_path.to_string_lossy(),
            "0 0 * * *",
            clock.now(),
        ))
        .unwrap();

    let report = daemon.tick_scheduler(&mut store).unwrap();
    let fired: Vec<&String> = report.triggered.iter().map(|(id, _)| id).collect();
    assert_eq!(fired, vec!["at.midnight"], "{report:?}");
}
