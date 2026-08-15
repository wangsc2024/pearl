//! Lease lifecycle and worker-crash recovery — 系統開発需求書 §34, §61.
//!
//! The scenario these tests exist for: a worker claims a task, starts running it, and
//! dies without releasing anything. Nobody tells the system. The lease must lapse, the
//! reaper must notice, and the task must return to the queue.

use chrono::TimeDelta;
use pearl_core::{Clock, QualitySpec, TaskId, TaskState, TestClock, WorkerId};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_state::{StateStore, TaskSubmission};

fn task_id(s: &str) -> TaskId {
    TaskId::parse(s).unwrap()
}

fn worker(n: u32) -> WorkerId {
    WorkerId::from_host_pid("box", n)
}

/// A store with one task sitting in `READY`, ready to be claimed.
fn ready_store(clock: &TestClock) -> (StateStore, TaskId) {
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    let now = clock.now();

    store
        .create_task(
            TaskSubmission {
                task_id: id.clone(),
                task_type: "digest".into(),
                precision_class: None,
                quality: QualitySpec::mechanical(),
            },
            now,
        )
        .unwrap();
    for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
        store.transition(&id, state, None, None, now).unwrap();
    }
    (store, id)
}

#[test]
fn claim_moves_task_to_leased() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    assert_eq!(store.get_task(&id).unwrap().unwrap().state, TaskState::Leased);
    assert_eq!(lease.worker_id, worker(1));
    assert_eq!(lease.leased_until, clock.now() + TimeDelta::try_seconds(60).unwrap());
}

#[test]
fn a_task_cannot_be_claimed_twice() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let first = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    // Two workers running the same task would duplicate its side effects. The state
    // check fires first here (the task is no longer READY), which is a sufficient guard.
    let err = mgr.claim(&mut store, &id, &worker(2)).unwrap_err();
    assert!(err.to_string().contains("not claimable"), "got: {err}");

    // The original holder keeps the lease.
    let held = store.active_lease_for_task(&id).unwrap().unwrap();
    assert_eq!(held.lease_id, first.lease_id);
    assert_eq!(held.worker_id, worker(1));
}

#[test]
fn a_stale_lease_row_does_not_block_a_legitimate_reclaim() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    mgr.claim(&mut store, &id, &worker(1)).unwrap();

    // Worker 1 dies; the reaper returns the task to READY.
    clock.advance_secs(61);
    mgr.reap(&mut store).unwrap();

    // Worker 2 must be able to claim even though worker 1's lease row still exists.
    assert!(mgr.claim(&mut store, &id, &worker(2)).is_ok());
}

#[test]
fn only_ready_tasks_are_claimable() {
    let clock = TestClock::new();
    let mut store = StateStore::open_in_memory().unwrap();
    let id = task_id("t1");
    store
        .create_task(
            TaskSubmission {
                task_id: id.clone(),
                task_type: "digest".into(),
                precision_class: None,
                quality: QualitySpec::mechanical(),
            },
            clock.now(),
        )
        .unwrap();

    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let err = mgr.claim(&mut store, &id, &worker(1)).unwrap_err();
    assert!(err.to_string().contains("not claimable"), "got: {err}");
}

#[test]
fn heartbeat_extends_the_deadline() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    clock.advance_secs(30);
    let extended = mgr.heartbeat(&mut store, lease.lease_id).unwrap();

    assert!(extended > lease.leased_until, "heartbeat must push the deadline out");
    assert_eq!(extended, clock.now() + TimeDelta::try_seconds(60).unwrap());
}

#[test]
fn a_heartbeating_worker_is_never_reaped() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();
    store.transition(&id, TaskState::Running, None, None, clock.now()).unwrap();

    // Simulate ten minutes of healthy work: beat every 20s.
    for _ in 0..30 {
        clock.advance_secs(20);
        mgr.heartbeat(&mut store, lease.lease_id).unwrap();
        assert!(
            mgr.reap(&mut store).unwrap().is_empty(),
            "a live worker must never be reclaimed"
        );
    }

    assert_eq!(store.get_task(&id).unwrap().unwrap().state, TaskState::Running);
}

#[test]
fn worker_that_dies_before_starting_returns_the_task_to_ready() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    // Claimed but never started: no work happened, so no side effect can have happened.
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    // The worker dies here. No release, no heartbeat, no notification.
    clock.advance_secs(61);

    let report = mgr.reap(&mut store).unwrap();

    assert_eq!(report.reclaimed, vec![id.clone()]);
    assert_eq!(
        store.get_task(&id).unwrap().unwrap().state,
        TaskState::Ready,
        "nothing ran, so the task is immediately claimable again"
    );
    assert!(
        store.get_lease(lease.lease_id).unwrap().unwrap().released_at.is_some(),
        "the dead lease must be closed"
    );
}

#[test]
fn worker_that_dies_mid_run_goes_to_retry_wait_not_straight_to_ready() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();
    store.transition(&id, TaskState::Running, None, None, clock.now()).unwrap();

    // The worker dies mid-execution. Work already started, so side effects may already
    // have landed; re-offering the task immediately would risk repeating them without
    // any retry accounting.
    clock.advance_secs(61);
    let report = mgr.reap(&mut store).unwrap();

    assert_eq!(report.reclaimed, vec![id.clone()]);
    assert_eq!(
        store.get_task(&id).unwrap().unwrap().state,
        TaskState::RetryWait,
        "a started run must be reclaimed through retry, not straight back to the pool"
    );
    assert!(store.get_lease(lease.lease_id).unwrap().unwrap().released_at.is_some());

    // RetryWait is not a dead end: it leads back to Ready.
    store
        .transition(&id, TaskState::Ready, Some("backoff elapsed".into()), None, clock.now())
        .unwrap();
    assert!(mgr.claim(&mut store, &id, &worker(2)).is_ok());
}

#[test]
fn expiry_is_recorded_in_the_ledger() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    mgr.claim(&mut store, &id, &worker(7)).unwrap();

    clock.advance_secs(61);
    mgr.reap(&mut store).unwrap();

    // The reaper concluded the death; the dead worker could not have reported it.
    let expired = store.ledger().read_by_type("lease.expired").unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].worker_id.as_ref(), Some(&worker(7)));
}

#[test]
fn reclaimed_task_can_be_picked_up_by_another_worker() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    mgr.claim(&mut store, &id, &worker(1)).unwrap();
    clock.advance_secs(61);
    mgr.reap(&mut store).unwrap();

    // The whole point: work survives the worker.
    let second = mgr.claim(&mut store, &id, &worker(2)).unwrap();
    assert_eq!(second.worker_id, worker(2));
    assert_eq!(store.get_task(&id).unwrap().unwrap().state, TaskState::Leased);
}

#[test]
fn reclaimed_run_is_recorded_as_a_distinct_attempt() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());

    // First worker claims, opens a run, then dies mid-attempt.
    mgr.claim(&mut store, &id, &worker(1)).unwrap();
    store.transition(&id, TaskState::Running, None, None, clock.now()).unwrap();
    let run = store.start_run(&id, "system@builtin", "hash", clock.now()).unwrap();
    store.start_attempt(run.run_id, 1, clock.now()).unwrap();

    clock.advance_secs(61);
    mgr.reap(&mut store).unwrap();

    // Second worker retries: a new attempt, not a mutation of the first.
    store.transition(&id, TaskState::Ready, None, None, clock.now()).unwrap();
    mgr.claim(&mut store, &id, &worker(2)).unwrap();
    store.start_attempt(run.run_id, 2, clock.now()).unwrap();

    let attempts = store.attempts_for_run(run.run_id).unwrap();
    assert_eq!(attempts.len(), 2, "each try is its own durable record");
    assert_eq!(attempts[0].attempt_number, 1);
    assert_eq!(attempts[1].attempt_number, 2);
    assert_eq!(store.get_task(&id).unwrap().unwrap().attempt_count, 2);
}

#[test]
fn an_expired_lease_cannot_be_revived_by_heartbeat() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    clock.advance_secs(61);

    // A worker that wakes from a long pause must not reclaim ownership: the reaper may
    // already have given the task away, and two owners would double the side effects.
    let err = mgr.heartbeat(&mut store, lease.lease_id).unwrap_err();
    assert!(err.to_string().contains("expired"), "got: {err}");
}

#[test]
fn released_lease_cannot_be_heartbeat() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    mgr.release(&mut store, lease.lease_id).unwrap();
    let err = mgr.heartbeat(&mut store, lease.lease_id).unwrap_err();
    assert!(err.to_string().contains("already released"), "got: {err}");
}

#[test]
fn reap_is_idempotent() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    mgr.claim(&mut store, &id, &worker(1)).unwrap();

    clock.advance_secs(61);

    let first = mgr.reap(&mut store).unwrap();
    assert_eq!(first.reclaimed.len(), 1);

    // A second pass has nothing to do, because reclamation released the lease.
    let second = mgr.reap(&mut store).unwrap();
    assert!(second.is_empty(), "second reap should find nothing: {second:?}");
}

#[test]
fn reap_does_not_disturb_a_task_that_already_moved_on() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let lease = mgr.claim(&mut store, &id, &worker(1)).unwrap();

    // The task is cancelled while the lease is still outstanding.
    store
        .transition(&id, TaskState::Cancelled, Some("operator".into()), None, clock.now())
        .unwrap();
    clock.advance_secs(61);

    let report = mgr.reap(&mut store).unwrap();

    assert!(report.reclaimed.is_empty(), "a cancelled task must not be requeued");
    assert_eq!(report.skipped, vec![id.clone()]);
    assert_eq!(store.get_task(&id).unwrap().unwrap().state, TaskState::Cancelled);
    assert!(store.get_lease(lease.lease_id).unwrap().unwrap().released_at.is_some());
}

#[test]
fn interrupted_verification_retries_rather_than_requeueing_directly() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    mgr.claim(&mut store, &id, &worker(1)).unwrap();
    store.transition(&id, TaskState::Running, None, None, clock.now()).unwrap();
    store.transition(&id, TaskState::Verifying, None, None, clock.now()).unwrap();

    clock.advance_secs(61);
    let report = mgr.reap(&mut store).unwrap();

    // The run already happened, so going straight back to Ready would redo work whose
    // side effects may already have landed. RetryWait is the honest state.
    assert_eq!(report.reclaimed, vec![id.clone()]);
    assert_eq!(
        store.get_task(&id).unwrap().unwrap().state,
        TaskState::RetryWait
    );
}

#[test]
fn several_dead_workers_are_all_reclaimed() {
    let clock = TestClock::new();
    let mut store = StateStore::open_in_memory().unwrap();
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let now = clock.now();

    let ids: Vec<_> = (0..5).map(|i| task_id(&format!("t{i}"))).collect();
    for (i, id) in ids.iter().enumerate() {
        store
            .create_task(
                TaskSubmission {
                    task_id: id.clone(),
                    task_type: "digest".into(),
                    precision_class: None,
                    quality: QualitySpec::mechanical(),
                },
                now,
            )
            .unwrap();
        for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
            store.transition(id, state, None, None, now).unwrap();
        }
        mgr.claim(&mut store, id, &worker(i as u32)).unwrap();
    }

    clock.advance_secs(61);
    let report = mgr.reap(&mut store).unwrap();

    assert_eq!(report.reclaimed.len(), 5);
    assert_eq!(store.count_by_state(TaskState::Ready).unwrap(), 5);
}

#[test]
fn recovery_survives_a_process_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pearl.db");
    let clock = TestClock::new();
    let id = task_id("t1");
    let now = clock.now();

    // Process 1: claim a task, then vanish.
    {
        let mut store = StateStore::open(&path).unwrap();
        store
            .create_task(
                TaskSubmission {
                    task_id: id.clone(),
                    task_type: "digest".into(),
                    precision_class: None,
                    quality: QualitySpec::mechanical(),
                },
                now,
            )
            .unwrap();
        for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
            store.transition(&id, state, None, None, now).unwrap();
        }
        let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
        mgr.claim(&mut store, &id, &worker(1)).unwrap();
    }

    clock.advance_secs(61);

    // Process 2: a fresh process reclaims work it never issued.
    let mut store = StateStore::open(&path).unwrap();
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let report = mgr.reap(&mut store).unwrap();

    assert_eq!(report.reclaimed, vec![id.clone()]);
    assert_eq!(store.get_task(&id).unwrap().unwrap().state, TaskState::Ready);
}

#[test]
fn reclamation_is_visible_after_replay() {
    let clock = TestClock::new();
    let (mut store, id) = ready_store(&clock);
    let mgr = LeaseManager::new(LeaseConfig::default(), clock.clone());
    mgr.claim(&mut store, &id, &worker(1)).unwrap();
    clock.advance_secs(61);
    mgr.reap(&mut store).unwrap();

    let before = store.get_task(&id).unwrap().unwrap();
    store.rebuild_from_ledger().unwrap();
    let after = store.get_task(&id).unwrap().unwrap();

    assert_eq!(before, after, "reclamation must be reconstructible from events");
    assert_eq!(after.state, TaskState::Ready);
}
