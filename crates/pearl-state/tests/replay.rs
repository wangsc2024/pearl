//! Replay tests — 系統開發需求書 §61, ADR-0001.
//!
//! ADR-0001 claims the ledger is the source of truth and the projections are a
//! droppable cache. That claim is only true if rebuilding from the ledger reproduces
//! exactly what incremental maintenance produced. These tests are that proof; without
//! them Article 6 is an aspiration.

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::{
    Evidence, EvidenceResult, EvidenceSet, EvidenceType, IdempotencyKey, PrecisionClass,
    QualitySpec, TaskId, TaskState,
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

fn evidence() -> EvidenceSet {
    [Evidence::new(
        EvidenceType::Test,
        "cargo test",
        EvidenceResult::Pass,
        t0(),
    )]
    .into_iter()
    .collect()
}

/// Builds a store containing a realistic multi-task, multi-run history.
fn populate(store: &mut StateStore) {
    // Task 1: full success path with a run, two attempts and evidence.
    let one = task_id("task.one");
    store
        .create_task(
            TaskSubmission {
                task_id: one.clone(),
                task_type: "digest".into(),
                precision_class: Some(PrecisionClass::P1),
                quality: QualitySpec::mechanical(),
            },
            t0(),
        )
        .unwrap();

    let run = store.start_run(&one, "system@builtin", "hash1", later(1)).unwrap();
    let a1 = store.start_attempt(run.run_id, 1, later(2)).unwrap();
    store
        .end_attempt(a1.attempt_id, RunOutcome::Failure, Some("timeout".into()), later(3))
        .unwrap();
    let a2 = store.start_attempt(run.run_id, 2, later(4)).unwrap();
    store
        .end_attempt(a2.attempt_id, RunOutcome::Success, None, later(5))
        .unwrap();
    store.end_run(run.run_id, RunOutcome::Success, later(6)).unwrap();

    for (i, state) in [
        TaskState::Planning,
        TaskState::Planned,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
        TaskState::Verifying,
    ]
    .iter()
    .enumerate()
    {
        store
            .transition(&one, *state, None, None, later(10 + i as i64))
            .unwrap();
    }
    store
        .transition(&one, TaskState::VerifiedSuccess, None, Some(&evidence()), later(20))
        .unwrap();

    // Task 2: blocked by the Exactness Gate, ending in UNVERIFIED.
    let two = task_id("task.two");
    store
        .create_task(
            TaskSubmission {
                task_id: two.clone(),
                task_type: "research".into(),
                precision_class: Some(PrecisionClass::P3),
                quality: QualitySpec::exact_but_unverifiable(),
            },
            later(30),
        )
        .unwrap();
    for (i, state) in [
        TaskState::Planning,
        TaskState::Planned,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
        TaskState::Verifying,
    ]
    .iter()
    .enumerate()
    {
        store
            .transition(&two, *state, None, None, later(31 + i as i64))
            .unwrap();
    }
    store
        .transition(
            &two,
            TaskState::Unverified,
            Some("no verifier".into()),
            None,
            later(40),
        )
        .unwrap();

    // Task 3: cancelled early.
    let three = task_id("task.three");
    store
        .create_task(
            TaskSubmission {
                task_id: three.clone(),
                task_type: "digest".into(),
                precision_class: None,
                quality: QualitySpec::best_effort(),
            },
            later(50),
        )
        .unwrap();
    store
        .transition(&three, TaskState::Cancelled, Some("operator".into()), None, later(51))
        .unwrap();

    // Effects, including a deduplicated repeat.
    let key = IdempotencyKey::parse("ntfy:digest:2026-08-15").unwrap();
    let trace = store.get_task(&one).unwrap().unwrap().trace_id;
    store.request_effect("ntfy", &key, trace, later(60)).unwrap();
    store.commit_effect("ntfy", &key, trace, later(61)).unwrap();
    store.request_effect("ntfy", &key, trace, later(62)).unwrap();
}

#[test]
fn replay_reproduces_task_state_exactly() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);

    let before = store.all_tasks().unwrap();
    store.rebuild_from_ledger().unwrap();
    let after = store.all_tasks().unwrap();

    assert_eq!(
        before, after,
        "rebuilding from the ledger must reproduce identical task state"
    );
}

#[test]
fn replay_reproduces_runs_and_attempts() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);
    let one = task_id("task.one");

    let runs_before = store.runs_for_task(&one).unwrap();
    let attempts_before = store.attempts_for_run(runs_before[0].run_id).unwrap();

    store.rebuild_from_ledger().unwrap();

    let runs_after = store.runs_for_task(&one).unwrap();
    let attempts_after = store.attempts_for_run(runs_after[0].run_id).unwrap();

    assert_eq!(runs_before, runs_after);
    assert_eq!(attempts_before, attempts_after);
}

#[test]
fn replay_reproduces_effect_deduplication_state() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);
    let key = IdempotencyKey::parse("ntfy:digest:2026-08-15").unwrap();

    let before = store.get_effect(&key).unwrap().unwrap();
    store.rebuild_from_ledger().unwrap();
    let after = store.get_effect(&key).unwrap().unwrap();

    assert_eq!(before, after);
    assert!(
        after.is_committed(),
        "a committed effect must stay committed across replay, or retry would resend it"
    );
}

#[test]
fn replay_is_idempotent() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);

    store.rebuild_from_ledger().unwrap();
    let once = store.all_tasks().unwrap();
    store.rebuild_from_ledger().unwrap();
    let twice = store.all_tasks().unwrap();
    store.rebuild_from_ledger().unwrap();
    let thrice = store.all_tasks().unwrap();

    assert_eq!(once, twice);
    assert_eq!(twice, thrice);
}

#[test]
fn replay_does_not_append_to_the_ledger() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);

    let count_before = store.ledger().count().unwrap();
    store.rebuild_from_ledger().unwrap();
    let count_after = store.ledger().count().unwrap();

    assert_eq!(
        count_before, count_after,
        "replay reads history; it must not write it"
    );
}

#[test]
fn replay_summary_accounts_for_every_event() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);

    let summary = store.rebuild_from_ledger().unwrap();
    assert_eq!(summary.total_events, store.ledger().count().unwrap());
    assert_eq!(
        summary.applied + summary.skipped,
        summary.total_events,
        "every event is either projected or explicitly inert"
    );
    assert!(summary.applied > 0);
}

#[test]
fn projections_can_be_dropped_and_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pearl.db");

    let expected = {
        let mut store = StateStore::open(&path).unwrap();
        populate(&mut store);
        store.all_tasks().unwrap()
    };

    // Simulate cache corruption: wipe the projections but keep the ledger, which the
    // append-only triggers protect anyway.
    {
        let store = StateStore::open(&path).unwrap();
        for table in ["tasks", "runs", "attempts", "effects", "evidence", "leases"] {
            store
                .ledger()
                .connection()
                .execute(&format!("DELETE FROM {table}"), [])
                .unwrap();
        }
    }

    let mut store = StateStore::open(&path).unwrap();
    assert!(
        store.all_tasks().unwrap().is_empty(),
        "projections should be empty before rebuild"
    );

    store.rebuild_from_ledger().unwrap();
    assert_eq!(
        store.all_tasks().unwrap(),
        expected,
        "the ledger alone must be sufficient to restore full state"
    );
}

#[test]
fn replay_of_empty_ledger_yields_empty_state() {
    let mut store = StateStore::open_in_memory().unwrap();
    let summary = store.rebuild_from_ledger().unwrap();

    assert_eq!(summary.total_events, 0);
    assert_eq!(summary.applied, 0);
    assert!(store.all_tasks().unwrap().is_empty());
}

#[test]
fn deduplicated_effect_did_not_create_a_second_row() {
    let mut store = StateStore::open_in_memory().unwrap();
    populate(&mut store);

    // populate() requests the same key twice; Article 5 means one effect row.
    let rows: i64 = store
        .ledger()
        .connection()
        .query_row("SELECT COUNT(*) FROM effects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);

    // And the ledger recorded the deduplication as evidence that retry was safe.
    assert_eq!(
        store.ledger().read_by_type("effect.deduplicated").unwrap().len(),
        1
    );
}
