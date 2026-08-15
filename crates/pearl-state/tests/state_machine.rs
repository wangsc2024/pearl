//! Task lifecycle and Constitution gate enforcement.

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::{
    Evidence, EvidenceResult, EvidenceSet, EvidenceType, PrecisionClass, QualitySpec, TaskId,
    TaskState,
};
use pearl_events::RunOutcome;
use pearl_state::{StateStore, TaskSubmission};

fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_786_838_400, 0).unwrap()
}

fn later(secs: i64) -> DateTime<Utc> {
    t0() + TimeDelta::try_seconds(secs).unwrap()
}

fn task_id(s: &str) -> TaskId {
    TaskId::parse(s).unwrap()
}

fn submission(id: &str, quality: QualitySpec) -> TaskSubmission {
    TaskSubmission {
        task_id: task_id(id),
        task_type: "digest".into(),
        precision_class: Some(PrecisionClass::P1),
        quality,
    }
}

fn machine_evidence() -> EvidenceSet {
    [Evidence::new(
        EvidenceType::Test,
        "cargo test",
        EvidenceResult::Pass,
        t0(),
    )]
    .into_iter()
    .collect()
}

/// Drives a task to the given state through the legal path.
fn advance_to(store: &mut StateStore, id: &TaskId, target: TaskState) {
    let path = [
        TaskState::Planning,
        TaskState::Planned,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
        TaskState::Verifying,
    ];
    for (i, state) in path.iter().enumerate() {
        store
            .transition(id, *state, None, None, later(i as i64 + 1))
            .unwrap();
        if *state == target {
            return;
        }
    }
}

#[test]
fn task_is_created_in_created_state() {
    let mut store = StateStore::open_in_memory().unwrap();
    let record = store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();

    assert_eq!(record.state, TaskState::Created);
    assert_eq!(record.attempt_count, 0);
    assert_eq!(store.get_task(&task_id("t1")).unwrap().unwrap(), record);
}

#[test]
fn duplicate_task_id_is_rejected() {
    let mut store = StateStore::open_in_memory().unwrap();
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();

    let err = store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap_err();
    assert!(err.to_string().contains("already exists"));
}

#[test]
fn full_happy_path_reaches_verified_success() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);

    let final_record = store
        .transition(
            &id,
            TaskState::VerifiedSuccess,
            None,
            Some(&machine_evidence()),
            later(10),
        )
        .unwrap();

    assert_eq!(final_record.state, TaskState::VerifiedSuccess);
}

#[test]
fn illegal_transition_is_refused() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();

    // Created -> Running skips planning, claiming and the lease.
    let err = store
        .transition(&id, TaskState::Running, None, None, later(1))
        .unwrap_err();
    assert!(err.to_string().contains("not permitted"));

    // State is unchanged after a refused transition.
    assert_eq!(
        store.get_task(&id).unwrap().unwrap().state,
        TaskState::Created
    );
}

#[test]
fn terminal_state_cannot_be_left() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    store
        .transition(&id, TaskState::Cancelled, None, None, later(1))
        .unwrap();

    let err = store
        .transition(&id, TaskState::Ready, None, None, later(2))
        .unwrap_err();
    assert!(err.to_string().contains("terminal"));
}

#[test]
fn article_4_success_without_evidence_is_refused() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);

    let err = store
        .transition(&id, TaskState::VerifiedSuccess, None, None, later(10))
        .unwrap_err();
    assert!(
        err.to_string().contains("no evidence supplied"),
        "got: {err}"
    );
    assert_eq!(
        store.get_task(&id).unwrap().unwrap().state,
        TaskState::Verifying,
        "a refused success must not move the task"
    );
}

#[test]
fn article_4_empty_evidence_set_is_refused() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);

    let empty = EvidenceSet::new();
    let err = store
        .transition(&id, TaskState::VerifiedSuccess, None, Some(&empty), later(10))
        .unwrap_err();
    assert!(err.to_string().contains("empty"), "got: {err}");
}

#[test]
fn article_8_human_approval_alone_cannot_certify_success() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);

    let approval: EvidenceSet = [Evidence::new(
        EvidenceType::HumanApproval,
        "operator",
        EvidenceResult::Pass,
        t0(),
    )]
    .into_iter()
    .collect();

    let err = store
        .transition(
            &id,
            TaskState::VerifiedSuccess,
            None,
            Some(&approval),
            later(10),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("machine-produced"),
        "got: {err}"
    );
}

#[test]
fn article_2_exactness_gate_blocks_unverifiable_success() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(
            submission("t1", QualitySpec::exact_but_unverifiable()),
            t0(),
        )
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);

    // Even with good machine evidence, exactness with no deterministic verification
    // must not auto-complete.
    let err = store
        .transition(
            &id,
            TaskState::VerifiedSuccess,
            None,
            Some(&machine_evidence()),
            later(10),
        )
        .unwrap_err();
    assert!(err.to_string().contains("Article 2"), "got: {err}");

    // UNVERIFIED is the honest destination, and it is reachable.
    let record = store
        .transition(
            &id,
            TaskState::Unverified,
            Some("no verifier exists".into()),
            None,
            later(11),
        )
        .unwrap();
    assert_eq!(record.state, TaskState::Unverified);
    assert_eq!(record.last_reason.as_deref(), Some("no verifier exists"));
}

#[test]
fn unverified_can_be_resolved_once_a_verifier_exists() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(
            submission("t1", QualitySpec::exact_but_unverifiable()),
            t0(),
        )
        .unwrap();
    advance_to(&mut store, &id, TaskState::Verifying);
    store
        .transition(&id, TaskState::Unverified, None, None, later(10))
        .unwrap();

    // Unverified is not a dead end: it can re-enter verification.
    let record = store
        .transition(&id, TaskState::Verifying, None, None, later(11))
        .unwrap();
    assert_eq!(record.state, TaskState::Verifying);
}

#[test]
fn article_10_run_requires_config_revision_and_hash() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();

    assert!(store.start_run(&id, "", "hash", later(1)).is_err());
    assert!(store.start_run(&id, "rev", "", later(1)).is_err());

    let run = store
        .start_run(&id, "system@builtin", "deadbeef", later(1))
        .unwrap();
    assert_eq!(run.config_revision, "system@builtin");
    assert_eq!(run.config_hash, "deadbeef");
}

#[test]
fn attempts_accumulate_on_the_task() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    let run = store.start_run(&id, "rev", "hash", later(1)).unwrap();

    for n in 1..=3 {
        let attempt = store.start_attempt(run.run_id, n, later(n as i64 + 1)).unwrap();
        store
            .end_attempt(
                attempt.attempt_id,
                RunOutcome::Failure,
                Some("timeout".into()),
                later(n as i64 + 2),
            )
            .unwrap();
    }

    assert_eq!(store.get_task(&id).unwrap().unwrap().attempt_count, 3);
    let attempts = store.attempts_for_run(run.run_id).unwrap();
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[0].attempt_number, 1);
    assert_eq!(attempts[2].outcome.as_deref(), Some("failure"));
    assert_eq!(attempts[2].exit_reason.as_deref(), Some("timeout"));
}

#[test]
fn run_outcome_is_recorded() {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(submission("t1", QualitySpec::mechanical()), t0())
        .unwrap();
    let run = store.start_run(&id, "rev", "hash", later(1)).unwrap();
    store.end_run(run.run_id, RunOutcome::Success, later(5)).unwrap();

    let stored = store.get_run(run.run_id).unwrap().unwrap();
    assert_eq!(stored.outcome.as_deref(), Some("success"));
    assert_eq!(stored.ended_at, Some(later(5)));
}

#[test]
fn list_by_state_is_oldest_first() {
    let mut store = StateStore::open_in_memory().unwrap();
    for (i, id) in ["t3", "t1", "t2"].iter().enumerate() {
        store
            .create_task(submission(id, QualitySpec::mechanical()), later(i as i64))
            .unwrap();
    }

    let created = store.list_by_state(TaskState::Created).unwrap();
    let ids: Vec<_> = created.iter().map(|t| t.task_id.to_string()).collect();
    assert_eq!(ids, vec!["t3", "t1", "t2"], "creation order, not alphabetical");
    assert_eq!(store.count_by_state(TaskState::Created).unwrap(), 3);
}

#[test]
fn state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pearl.db");
    let id = task_id("t1");

    {
        let mut store = StateStore::open(&path).unwrap();
        store
            .create_task(submission("t1", QualitySpec::mechanical()), t0())
            .unwrap();
        store
            .transition(&id, TaskState::Planning, None, None, later(1))
            .unwrap();
    }

    // Article 6: after a restart the system knows what happened and where it got to.
    let store = StateStore::open(&path).unwrap();
    let record = store.get_task(&id).unwrap().unwrap();
    assert_eq!(record.state, TaskState::Planning);
    assert_eq!(store.ledger().read_task(&id).unwrap().len(), 2);
}
