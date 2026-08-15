//! The projections that were declared but never written — §43.
//!
//! Nine of the fifteen tables in the schema had no writer: `steps`, `checkpoints`,
//! `verification_results`, `artifacts`, `policy_decisions`, `config_revisions` and
//! `runtime_health` existed as DDL and nothing else. A query surface that cannot answer
//! "which steps ran?" or "what verified this?" is not a query surface, so these tests exist
//! to keep each one connected to something that writes it.

use chrono::{TimeDelta, Utc};
use pearl_core::{Clock, PrecisionClass, QualitySpec, SystemClock, TaskId, TaskState, TestClock};
use pearl_events::RunOutcome;
use pearl_state::{Artifact, ConfigRevision, StateStore, StepRecord, TaskSubmission};

fn store() -> StateStore {
    StateStore::open_in_memory().unwrap()
}

fn task_id(id: &str) -> TaskId {
    TaskId::parse(id).unwrap()
}

/// A task in `RUNNING` with an open run, which is the state most projections belong to.
fn running_task(store: &mut StateStore, id: &str) -> (TaskId, pearl_core::RunId) {
    let clock = SystemClock;
    let task_id = task_id(id);
    store
        .create_task(
            TaskSubmission::new(
                task_id.clone(),
                "fixture",
                Some(PrecisionClass::P0),
                QualitySpec::mechanical(),
            ),
            clock.now(),
        )
        .unwrap();
    for state in [
        TaskState::Planning,
        TaskState::Planned,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
    ] {
        store
            .transition(&task_id, state, None, None, clock.now())
            .unwrap();
    }
    let run = store
        .start_run(&task_id, "system@test", "hash", clock.now())
        .unwrap();
    (task_id, run.run_id)
}

#[test]
fn steps_record_what_actually_ran_in_order() {
    let mut store = store();
    let (_task_id, run_id) = running_task(&mut store, "steps.task");
    let now = Utc::now();

    store
        .record_step(
            &StepRecord::new(run_id, 1, "collect", "collect sources", "success")
                .started(now)
                .completed(now),
        )
        .unwrap();
    store
        .record_step(
            &StepRecord::new(run_id, 2, "verify", "verify digest", "failed")
                .started(now)
                .completed(now),
        )
        .unwrap();

    let steps = store.steps_for_run(run_id).unwrap();
    assert_eq!(steps.len(), 2);
    // Order is execution order, not insertion order by id: a reader reconstructs the run.
    assert_eq!(steps[0].step_number, 1);
    assert_eq!(steps[0].status, "success");
    assert_eq!(steps[1].description, "verify digest");
    assert_eq!(steps[1].status, "failed");
}

#[test]
fn a_step_is_replaced_rather_than_duplicated_when_its_status_changes() {
    // A step moves from running to success; that is one step with two states, not two steps.
    let mut store = store();
    let (_task_id, run_id) = running_task(&mut store, "steps.update");
    let now = Utc::now();

    store
        .record_step(&StepRecord::new(run_id, 1, "collect", "collect", "running").started(now))
        .unwrap();
    store
        .record_step(
            &StepRecord::new(run_id, 1, "collect", "collect", "success")
                .started(now)
                .completed(now),
        )
        .unwrap();

    let steps = store.steps_for_run(run_id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].status, "success");
    assert!(steps[0].completed_at.is_some());
}

#[test]
fn a_checkpoint_is_committed_with_its_event_and_survives_a_rebuild() {
    let mut store = store();
    let (task_id, _run_id) = running_task(&mut store, "checkpoint.task");

    let id = store
        .commit_checkpoint(&task_id, "collect", Some(r#"{"cursor":42}"#), Utc::now())
        .unwrap();

    let checkpoints = store.checkpoints_for_task(&task_id).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].checkpoint_id, id.to_string());
    assert_eq!(checkpoints[0].label, "collect");
    assert_eq!(checkpoints[0].payload.as_deref(), Some(r#"{"cursor":42}"#));

    // The event is what makes it durable. §41: resume reads the latest checkpoint, so a
    // rebuild that dropped them would silently restart completed work.
    let events: Vec<String> = store
        .ledger()
        .read_trace(store.get_task(&task_id).unwrap().unwrap().trace_id)
        .unwrap()
        .iter()
        .map(|e| e.event_type().to_string())
        .collect();
    assert!(
        events.iter().any(|e| e == "checkpoint.committed"),
        "{events:?}"
    );

    store.rebuild_from_ledger().unwrap();
    let rebuilt = store.checkpoints_for_task(&task_id).unwrap();
    assert_eq!(rebuilt.len(), 1, "the checkpoint must survive replay");
    assert_eq!(rebuilt[0].label, "collect");
}

#[test]
fn the_latest_checkpoint_is_the_resume_point() {
    let mut store = store();
    let (task_id, _run_id) = running_task(&mut store, "checkpoint.latest");
    let clock = TestClock::new();

    store
        .commit_checkpoint(&task_id, "first", None, clock.now())
        .unwrap();
    clock.advance_secs(10);
    store
        .commit_checkpoint(&task_id, "second", None, clock.now())
        .unwrap();

    assert_eq!(
        store.latest_checkpoint(&task_id).unwrap().unwrap().label,
        "second"
    );
}

#[test]
fn verification_results_are_queryable_per_task() {
    let mut store = store();
    let (task_id, _run_id) = running_task(&mut store, "verify.task");
    let now = Utc::now();

    store
        .record_verification(&task_id, "schema:verification-result-v1", true, None, now)
        .unwrap();
    store
        .record_verification(
            &task_id,
            "verifier.task-result",
            false,
            Some("score is missing"),
            now,
        )
        .unwrap();

    let results = store.verifications_for_task(&task_id).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].passed);
    assert!(!results[1].passed);
    assert_eq!(results[1].detail.as_deref(), Some("score is missing"));
}

#[test]
fn artifacts_are_indexed_by_content_digest() {
    let mut store = store();
    let (task_id, _run_id) = running_task(&mut store, "artifact.task");

    let artifact = Artifact {
        artifact_id: "digest-2026-08-15".into(),
        task_id: task_id.clone(),
        artifact_type: "report".into(),
        path: "artifacts/digest.md".into(),
        sha256: "a".repeat(64),
        size_bytes: 4096,
        created_at: Utc::now(),
    };
    store.record_artifact(&artifact).unwrap();

    let found = store.artifacts_for_task(&task_id).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].sha256, "a".repeat(64));
    assert_eq!(found[0].size_bytes, 4096);

    // Recording the same artifact again is not a duplicate: the id is its identity.
    store.record_artifact(&artifact).unwrap();
    assert_eq!(store.artifacts_for_task(&task_id).unwrap().len(), 1);
}

#[test]
fn health_reports_the_latest_observation_per_subsystem() {
    let mut store = store();
    let clock = TestClock::new();

    store
        .record_health("worker", "healthy", None, clock.now())
        .unwrap();
    store
        .record_health("scheduler", "healthy", None, clock.now())
        .unwrap();
    clock.advance_secs(60);
    store
        .record_health("worker", "degraded", Some("3 failures"), clock.now())
        .unwrap();

    let health = store.latest_health().unwrap();
    assert_eq!(health.len(), 2, "one row per subsystem, not one per report");
    let worker = health.iter().find(|h| h.subsystem == "worker").unwrap();
    assert_eq!(worker.status, "degraded");
    assert_eq!(worker.detail.as_deref(), Some("3 failures"));
}

#[test]
fn policy_decisions_are_recorded_against_the_task_they_concern() {
    let mut store = store();
    let (task_id, _run_id) = running_task(&mut store, "policy.task");
    let now = Utc::now();

    store
        .record_policy_decision(
            Some(&task_id),
            "permission",
            "denied",
            Some("no rule matched 'script.x'"),
            now,
        )
        .unwrap();
    // A decision with no task is legitimate: a profile change concerns the system.
    store
        .record_policy_decision(None, "profile", "degraded", None, now)
        .unwrap();

    let decisions = store.policy_decisions_for_task(&task_id).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].outcome, "denied");
    assert!(decisions[0].reason.as_deref().unwrap().contains("script.x"));
}

#[test]
fn config_revisions_are_retrievable_by_id() {
    let mut store = store();
    let revision = ConfigRevision {
        revision_id: "system@builtin+profile@normal".into(),
        config_hash: "b".repeat(64),
        source: "worker".into(),
        applied_at: Utc::now(),
        payload: Some(r#"{"concurrency_cap":2}"#.into()),
    };
    store.record_config_revision(&revision).unwrap();

    // Article 10: a run records which revision it used, so the revision must be resolvable
    // afterwards or the record is a dangling reference.
    let found = store
        .get_config_revision("system@builtin+profile@normal")
        .unwrap()
        .unwrap();
    assert_eq!(found.config_hash, "b".repeat(64));
    assert_eq!(found.payload.as_deref(), Some(r#"{"concurrency_cap":2}"#));
    assert!(store
        .get_config_revision("never-applied")
        .unwrap()
        .is_none());
}

#[test]
fn a_rebuild_clears_every_projection_including_health() {
    // `runtime_health` was missing from the projection list, so a rebuild left stale rows
    // behind while reporting that it had reconstructed everything.
    let mut store = store();
    let (task_id, run_id) = running_task(&mut store, "rebuild.task");
    let now = Utc::now();

    store
        .record_health("worker", "degraded", None, now)
        .unwrap();
    store
        .record_step(
            &StepRecord::new(run_id, 1, "s", "step", "success")
                .started(now)
                .completed(now),
        )
        .unwrap();
    store
        .record_verification(&task_id, "verifier.x", true, None, now)
        .unwrap();
    store.end_run(run_id, RunOutcome::Success, now).unwrap();

    store.rebuild_from_ledger().unwrap();

    assert!(
        store.latest_health().unwrap().is_empty(),
        "health is an observation, not a projection of the ledger; a rebuild must not keep it"
    );
    // Steps and verifications are likewise not reconstructible from the current event
    // vocabulary, so a rebuild empties them rather than presenting stale rows as current.
    assert!(store.steps_for_run(run_id).unwrap().is_empty());
    assert!(store.verifications_for_task(&task_id).unwrap().is_empty());
    // What the ledger does carry is fully restored.
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Running
    );
    assert!(store.get_run(run_id).unwrap().is_some());
}

#[test]
fn projection_writes_do_not_disturb_the_state_machine() {
    // Recording a step or a verdict is bookkeeping; it must not move the task.
    let mut store = store();
    let (task_id, run_id) = running_task(&mut store, "isolation.task");
    let before = store.get_task(&task_id).unwrap().unwrap();

    let now = before.updated_at + TimeDelta::try_seconds(1).unwrap();
    store
        .record_step(
            &StepRecord::new(run_id, 1, "s", "step", "success")
                .started(now)
                .completed(now),
        )
        .unwrap();
    store
        .record_verification(&task_id, "verifier.x", true, None, now)
        .unwrap();
    store.commit_checkpoint(&task_id, "s", None, now).unwrap();

    let after = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.attempt_count, before.attempt_count);
}
