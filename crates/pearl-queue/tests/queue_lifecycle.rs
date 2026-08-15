//! Queue behaviour: claiming, retry with backoff, and dead-lettering.

use chrono::TimeDelta;
use pearl_core::{Clock, QualitySpec, RuntimeProfile, TaskId, TaskState, TestClock, WorkerId};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_queue::{FailureVerdict, RetryPolicy, WorkQueue};
use pearl_state::{StateStore, TaskSubmission};

fn task_id(s: &str) -> TaskId {
    TaskId::parse(s).unwrap()
}

fn worker(n: u32) -> WorkerId {
    WorkerId::from_host_pid("box", n)
}

/// Creates `n` tasks already sitting in `READY`.
fn seeded_store(clock: &TestClock, n: usize) -> (StateStore, Vec<TaskId>) {
    let mut store = StateStore::open_in_memory().unwrap();
    let now = clock.now();
    let mut ids = Vec::new();

    for i in 0..n {
        let id = task_id(&format!("t{i}"));
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
        ids.push(id);
    }
    (store, ids)
}

#[test]
fn depth_reflects_ready_tasks() {
    let clock = TestClock::new();
    let (store, _) = seeded_store(&clock, 4);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock);

    assert_eq!(queue.depth(&store).unwrap(), 4);
}

#[test]
fn claim_next_hands_out_the_oldest_task_first() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 3);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let claim = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();

    // Fairness: oldest first, so a busy stream of new work cannot starve older work.
    assert_eq!(claim.task.task_id, ids[0]);
    assert_eq!(claim.lease.worker_id, worker(1));
    assert_eq!(queue.depth(&store).unwrap(), 2);
}

#[test]
fn each_worker_gets_a_different_task() {
    let clock = TestClock::new();
    let (mut store, _) = seeded_store(&clock, 3);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let a = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    let b = queue.claim_next(&mut store, &leases, &worker(2)).unwrap().unwrap();
    let c = queue.claim_next(&mut store, &leases, &worker(3)).unwrap().unwrap();

    let mut seen = vec![
        a.task.task_id.to_string(),
        b.task.task_id.to_string(),
        c.task.task_id.to_string(),
    ];
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "no task may be handed to two workers");
    assert_eq!(queue.depth(&store).unwrap(), 0);
}

#[test]
fn empty_queue_yields_none() {
    let clock = TestClock::new();
    let (mut store, _) = seeded_store(&clock, 0);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());

    assert!(queue.claim_next(&mut store, &leases, &worker(1)).unwrap().is_none());
}

#[test]
fn peek_does_not_claim() {
    let clock = TestClock::new();
    let (store, _) = seeded_store(&clock, 5);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock);

    assert_eq!(queue.peek(&store, 2).unwrap().len(), 2);
    assert_eq!(
        queue.depth(&store).unwrap(),
        5,
        "peeking must not remove anything"
    );
}

#[test]
fn failure_within_budget_schedules_a_retry() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 1);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());

    queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    store.transition(&ids[0], TaskState::Running, None, None, clock.now()).unwrap();

    let verdict = queue.record_failure(&mut store, &ids[0], "script exited 1").unwrap();

    assert!(verdict.will_retry());
    assert_eq!(store.get_task(&ids[0]).unwrap().unwrap().state, TaskState::RetryWait);
}

#[test]
fn retry_becomes_claimable_only_after_the_backoff() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 1);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());

    let claim = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    store.transition(&ids[0], TaskState::Running, None, None, clock.now()).unwrap();
    let run = store.start_run(&ids[0], "rev", "hash", clock.now()).unwrap();
    store.start_attempt(run.run_id, 1, clock.now()).unwrap();
    leases.release(&mut store, claim.lease.lease_id).unwrap();
    queue.record_failure(&mut store, &ids[0], "boom").unwrap();

    // Base backoff is 30s. Before it elapses, nothing is promoted.
    clock.advance_secs(29);
    assert!(queue.promote_ready_retries(&mut store).unwrap().is_empty());
    assert_eq!(queue.depth(&store).unwrap(), 0);

    clock.advance_secs(2);
    let promoted = queue.promote_ready_retries(&mut store).unwrap();
    assert_eq!(promoted, vec![ids[0].clone()]);
    assert_eq!(queue.depth(&store).unwrap(), 1);
}

#[test]
fn exhausted_attempts_are_dead_lettered() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 1);
    let policy = RetryPolicy::new(
        2,
        TimeDelta::try_seconds(10).unwrap(),
        TimeDelta::try_seconds(60).unwrap(),
    )
    .unwrap();
    let queue = WorkQueue::new(policy, RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let id = &ids[0];

    let run = {
        let claim = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
        store.transition(id, TaskState::Running, None, None, clock.now()).unwrap();
        let run = store.start_run(id, "rev", "hash", clock.now()).unwrap();
        store.start_attempt(run.run_id, 1, clock.now()).unwrap();
        leases.release(&mut store, claim.lease.lease_id).unwrap();
        run
    };

    // Attempt 1 fails -> retry.
    assert!(queue.record_failure(&mut store, id, "fail 1").unwrap().will_retry());
    clock.advance_secs(11);
    queue.promote_ready_retries(&mut store).unwrap();

    // Attempt 2 fails -> budget exhausted.
    let claim = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    store.transition(id, TaskState::Running, None, None, clock.now()).unwrap();
    store.start_attempt(run.run_id, 2, clock.now()).unwrap();
    leases.release(&mut store, claim.lease.lease_id).unwrap();

    let verdict = queue.record_failure(&mut store, id, "fail 2").unwrap();
    assert!(matches!(verdict, FailureVerdict::DeadLettered { .. }));
    assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::Failed);

    // FAILED is terminal: the task must not silently reappear in the queue.
    assert_eq!(queue.depth(&store).unwrap(), 0);
    assert!(queue.promote_ready_retries(&mut store).unwrap().is_empty());
}

#[test]
fn dead_letter_reason_records_why() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 1);
    let policy =
        RetryPolicy::new(1, TimeDelta::zero(), TimeDelta::try_seconds(1).unwrap()).unwrap();
    let queue = WorkQueue::new(policy, RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let id = &ids[0];

    let claim = queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    store.transition(id, TaskState::Running, None, None, clock.now()).unwrap();
    let run = store.start_run(id, "rev", "hash", clock.now()).unwrap();
    store.start_attempt(run.run_id, 1, clock.now()).unwrap();
    leases.release(&mut store, claim.lease.lease_id).unwrap();

    queue.record_failure(&mut store, id, "verifier crashed").unwrap();

    let reason = store.get_task(id).unwrap().unwrap().last_reason.unwrap();
    assert!(reason.contains("verifier crashed"), "got: {reason}");
    assert!(reason.contains("exhausted"), "got: {reason}");
}

#[test]
fn crashed_worker_and_retry_policy_compose() {
    let clock = TestClock::new();
    let (mut store, ids) = seeded_store(&clock, 1);
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock.clone());
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());
    let id = &ids[0];

    // A worker claims, starts, and dies.
    queue.claim_next(&mut store, &leases, &worker(1)).unwrap().unwrap();
    store.transition(id, TaskState::Running, None, None, clock.now()).unwrap();
    clock.advance_secs(61);

    // The reaper puts it in RetryWait, and the queue promotes it once backoff elapses.
    leases.reap(&mut store).unwrap();
    assert_eq!(store.get_task(id).unwrap().unwrap().state, TaskState::RetryWait);

    clock.advance_secs(31);
    assert_eq!(queue.promote_ready_retries(&mut store).unwrap(), vec![id.clone()]);

    // And it can be claimed by a different worker.
    let second = queue.claim_next(&mut store, &leases, &worker(2)).unwrap().unwrap();
    assert_eq!(second.lease.worker_id, worker(2));
}

#[test]
fn profile_caps_concurrency() {
    let clock = TestClock::new();
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Degraded, clock);

    // Degraded allows 2 concurrent workers.
    assert!(queue.admits_more_work(0));
    assert!(queue.admits_more_work(1));
    assert!(!queue.admits_more_work(2));
}

#[test]
fn recovery_profile_serialises_work() {
    let clock = TestClock::new();
    let mut queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock);

    assert!(queue.admits_more_work(100));
    queue.set_profile(RuntimeProfile::Recovery);
    assert!(queue.admits_more_work(0));
    assert!(!queue.admits_more_work(1), "recovery runs one task at a time");
}

#[test]
fn queue_state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pearl.db");
    let clock = TestClock::new();
    let now = clock.now();
    let id = task_id("t1");

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
    }

    // A fresh process sees the same queue: there is no in-memory queue to lose.
    let store = StateStore::open(&path).unwrap();
    let queue = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, clock);
    assert_eq!(queue.depth(&store).unwrap(), 1);
    assert_eq!(queue.peek(&store, 10).unwrap()[0].task_id, id);
}
