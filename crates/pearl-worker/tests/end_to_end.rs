//! The P0 acceptance scenario and its constitutional edge cases — §69, §70.
//!
//! §70 asks for one concrete proof that the framework works end to end: a task is
//! persisted, routed mechanically, executed, verified by a machine, given evidence, and
//! recorded in the ledger. [`the_acceptance_scenario_reaches_verified_success`] is that
//! proof, and it runs against the *shipped* capabilities rather than fixtures, so it fails
//! if the repository's own manifests or scripts stop working.
//!
//! The remaining tests cover the paths that are easy to get wrong in a way that looks like
//! success: a verifier that could not decide, exactness with nothing to establish it, a
//! capability that is not permitted, and a worker that dies mid-task.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use chrono::TimeDelta;
use pearl_core::{
    AssuranceStep, Clock, PrecisionClass, QualitySpec, SystemClock, TaskId, TaskPlan, TaskState,
    TestClock, WorkerId,
};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_queue::RetryPolicy;
use pearl_state::{StateStore, TaskSubmission};
use pearl_worker::{Verdict, Worker, WorkerConfig};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn python_available() -> bool {
    pearl_runtime::programs::is_available(&pearl_runtime::programs::python())
}

/// A worker configured against the repository's real capabilities.
fn shipped_config(dir: &TempDir) -> WorkerConfig {
    WorkerConfig {
        worker_id: WorkerId::new("worker:test"),
        poll_interval: std::time::Duration::from_millis(1),
        capabilities_dir: workspace_root().join("capabilities"),
        schema_dir: workspace_root().join("schemas"),
        permissions_path: Some(allow_all_permissions(dir)),
        working_dir: Some(dir.path().to_path_buf()),
        profile: pearl_core::RuntimeProfile::Normal,
        retry_policy: RetryPolicy::new(
            2,
            TimeDelta::try_seconds(0).unwrap(),
            TimeDelta::try_seconds(0).unwrap(),
        )
        .unwrap(),
    }
}

/// A worker configured against fixture capabilities written into `dir`.
fn fixture_config(dir: &TempDir) -> WorkerConfig {
    WorkerConfig {
        capabilities_dir: dir.path().join("capabilities"),
        ..shipped_config(dir)
    }
}

fn allow_all_permissions(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("permissions.yaml");
    std::fs::write(&path, "rules:\n  - capability: all\n    effect: allow\n").unwrap();
    path
}

/// Writes a capability manifest and its script into the fixture directory.
fn write_capability(dir: &TempDir, id: &str, script_body: &str, extra_manifest: &str) {
    let caps = dir.path().join("capabilities");
    std::fs::create_dir_all(&caps).unwrap();
    let script_name = format!("{}.py", id.replace('.', "_"));
    std::fs::write(caps.join(&script_name), script_body).unwrap();
    std::fs::write(
        caps.join(format!("{id}.yaml")),
        format!(
            "id: {id}\n\
             version: 1\n\
             type: script\n\
             description: fixture\n\
             execution:\n\
             \x20 kind: script\n\
             \x20 runtime: python\n\
             \x20 entrypoint:\n\
             \x20   script: {script_name}\n\
             quality:\n\
             \x20 deterministic: true\n\
             risk:\n\
             \x20 side_effect: false\n\
             platform:\n\
             \x20 windows: true\n\
             \x20 linux: true\n\
             timeout_seconds: 10\n\
             {extra_manifest}"
        ),
    )
    .unwrap();
}

fn store(dir: &TempDir) -> StateStore {
    StateStore::open(dir.path().join("pearl.db")).unwrap()
}

/// Submits a task and admits it to the queue.
///
/// The full path is walked rather than short-cut, because `CREATED → READY` is not a legal
/// transition and a test that skipped it would be testing a state the system cannot reach.
fn enqueue(
    store: &mut StateStore,
    clock: &dyn Clock,
    id: &str,
    quality: QualitySpec,
    precision: Option<PrecisionClass>,
    plan: TaskPlan,
) -> TaskId {
    let task_id = TaskId::parse(id).unwrap();
    let submission =
        TaskSubmission::new(task_id.clone(), "fixture", precision, quality).with_plan(plan);
    store.create_task(submission, clock.now()).unwrap();
    for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
        store
            .transition(&task_id, state, None, None, clock.now())
            .unwrap();
    }
    task_id
}

// ---------------------------------------------------------------------------
// The acceptance scenario
// ---------------------------------------------------------------------------

#[test]
fn the_acceptance_scenario_reaches_verified_success() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    let mut store = store(&dir);
    let clock = SystemClock;

    // A task that names the capability to run and the verifier that must approve it.
    let task_id = enqueue(
        &mut store,
        &clock,
        "acceptance.score",
        QualitySpec {
            exactness_required: true,
            deterministic_generation: true,
            deterministic_verification: true,
        },
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.task-score".into()),
            assurance: vec![AssuranceStep {
                input: Some(serde_json::json!({
                    "require_keys": ["score", "breakdown", "formula"],
                    "types": { "score": "number", "breakdown": "object" }
                })),
                ..AssuranceStep::script("verifier.task-result")
            }],
            timeout_seconds: Some(30),
        },
    );

    let worker = Worker::new(shipped_config(&dir), clock).unwrap();
    let result = worker
        .run_once(&mut store)
        .unwrap()
        .expect("a task was queued");

    // 1. The verdict is verified, not merely "finished".
    assert_eq!(result.verdict, Verdict::Verified, "{}", result.summary());
    assert_eq!(result.capability_id, "script.task-score");
    assert_eq!(result.exit_code, Some(0));

    // 2. The capability's machine output was captured, not just its exit code.
    let output = result.structured_output.expect("script emitted JSON");
    assert!(output["score"].is_number(), "got {output}");

    // 3. Verification actually ran, and reported a verdict.
    assert!(result.assurance.passed, "{}", result.assurance.summary());
    assert_eq!(result.assurance.errored_count(), 0);
    assert!(result
        .assurance
        .details
        .iter()
        .any(|d| d.name.contains("verifier.task-result")));

    // 4. The task reached VERIFIED_SUCCESS, which the store only permits with evidence.
    let task = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::VerifiedSuccess);

    // 5. Evidence was recorded (Article 4).
    let evidence = store.evidence_for_task(&task_id).unwrap();
    assert!(
        evidence.len() >= 2,
        "expected the execution and the verifier to both leave evidence, got {evidence:?}"
    );
    assert!(evidence.iter().all(|e| e.passed));

    // 6. The run and attempt were recorded with config provenance (Article 10).
    let runs = store.runs_for_task(&task_id).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].config_hash.is_empty());
    assert!(!runs[0].config_revision.is_empty());
    let attempts = store.attempts_for_run(runs[0].run_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome.as_deref(), Some("success"));

    // 7. The ledger tells the whole story (§42).
    let events: Vec<String> = store
        .ledger()
        .read_task(&task_id)
        .unwrap()
        .iter()
        .map(|e| e.event_type().to_string())
        .collect();
    for expected in [
        "task.created",
        "lease.acquired",
        "run.started",
        "attempt.started",
        "script.started",
        "script.completed",
        "verification.passed",
        "evidence.stored",
        "attempt.ended",
        "run.ended",
        "task.completed",
    ] {
        assert!(
            events.iter().any(|e| e == expected),
            "the ledger is missing '{expected}'; it has {events:?}"
        );
    }

    // 8. Replay reconstructs the same state from the ledger alone (ADR-0001).
    store.rebuild_from_ledger().unwrap();
    let rebuilt = store.get_task(&task_id).unwrap().unwrap();
    assert_eq!(rebuilt.state, TaskState::VerifiedSuccess);
    assert_eq!(
        rebuilt.plan.capability.as_deref(),
        Some("script.task-score"),
        "the declared plan must survive a rebuild, or a replayed task would run unverified"
    );
}

// ---------------------------------------------------------------------------
// Article 2: no verdict is not success
// ---------------------------------------------------------------------------

#[test]
fn a_task_demanding_exactness_with_no_verifier_becomes_unverified() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        "import json\nprint(json.dumps({\"ok\": True}))\n",
        "",
    );
    let mut store = store(&dir);
    let clock = SystemClock;

    let task_id = enqueue(
        &mut store,
        &clock,
        "exact.no-verifier",
        QualitySpec {
            exactness_required: true,
            deterministic_generation: false,
            deterministic_verification: false,
        },
        Some(PrecisionClass::P2),
        TaskPlan {
            capability: Some("script.plain".into()),
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    // It ran and exited zero, but nothing established the claim it makes about itself.
    assert_eq!(result.exit_code, Some(0));
    assert!(
        matches!(result.verdict, Verdict::Unverified { .. }),
        "expected UNVERIFIED, got {}",
        result.summary()
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Unverified
    );
}

#[test]
fn a_verifier_that_cannot_decide_leaves_the_task_unverified() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        "import json\nprint(json.dumps({\"ok\": True}))\n",
        "",
    );
    // Exit 2 is the Script I/O Contract's "I could not decide" (§26).
    write_capability(
        &dir,
        "verifier.confused",
        "import json, sys\nprint(json.dumps({\"status\": \"error\"}))\nsys.exit(2)\n",
        "",
    );
    let mut store = store(&dir);
    let clock = SystemClock;

    let task_id = enqueue(
        &mut store,
        &clock,
        "verifier.broken",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.plain".into()),
            assurance: vec![AssuranceStep::script("verifier.confused")],
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    // The distinction that matters: a verifier that could not decide has not rejected the
    // work, so retrying would prove nothing and FAILED would be a lie.
    assert_eq!(result.assurance.errored_count(), 1, "{}", result.summary());
    assert!(
        matches!(result.verdict, Verdict::Unverified { .. }),
        "got {}",
        result.summary()
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Unverified
    );
}

#[test]
fn a_verifier_that_rejects_the_result_fails_the_task() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        // A result that declares its own failure: the shipped verifier must catch this.
        "import json\nprint(json.dumps({\"status\": \"failed\"}))\n",
        "",
    );
    let mut store = store(&dir);
    let clock = SystemClock;

    let verifier = workspace_root().join("capabilities/verifiers/verify_task_result.py");
    let task_id = enqueue(
        &mut store,
        &clock,
        "verifier.rejects",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.plain".into()),
            assurance: vec![AssuranceStep::script(verifier.to_string_lossy())],
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert!(
        matches!(result.verdict, Verdict::Failed { .. }),
        "got {}",
        result.summary()
    );
    // A rejected result is retryable, so it waits rather than dying.
    let state = store.get_task(&task_id).unwrap().unwrap().state;
    assert!(
        matches!(state, TaskState::RetryWait | TaskState::Failed),
        "got {state}"
    );
}

// ---------------------------------------------------------------------------
// Refusals: nothing ran
// ---------------------------------------------------------------------------

#[test]
fn a_capability_that_is_not_permitted_never_runs() {
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        "import json, sys\nsys.stderr.write('SHOULD NOT RUN')\nprint(json.dumps({}))\n",
        "",
    );
    // An allow-list that does not mention the capability admits nothing (§45).
    // A distinct filename: the shared fixture writes an allow-all `permissions.yaml`, and
    // reusing that name would have this test silently exercise the opposite of its point.
    let permissions = dir.path().join("restrictive-permissions.yaml");
    std::fs::write(
        &permissions,
        "rules:\n  - capability: script.something-else\n    effect: allow\n",
    )
    .unwrap();

    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "denied.task",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.plain".into()),
            ..TaskPlan::empty()
        },
    );

    let config = WorkerConfig {
        permissions_path: Some(permissions),
        ..fixture_config(&dir)
    };
    let worker = Worker::new(config, clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert!(
        matches!(result.verdict, Verdict::Refused { .. }),
        "got {}",
        result.summary()
    );
    assert!(!result.verdict.executed());
    // BLOCKED, not FAILED: no retry will change a permission rule.
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );
    // Nothing was executed, so no run was opened.
    assert!(store.runs_for_task(&task_id).unwrap().is_empty());
}

#[test]
fn a_task_naming_an_unknown_capability_is_blocked() {
    let dir = TempDir::new().unwrap();
    write_capability(&dir, "script.plain", "print('{}')\n", "");
    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "unknown.capability",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.does-not-exist".into()),
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert!(matches!(result.verdict, Verdict::Refused { .. }));
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );
}

#[test]
fn a_capability_for_another_platform_is_blocked_rather_than_attempted() {
    let dir = TempDir::new().unwrap();
    // Declared for whichever platform this is not, so the test means the same thing on both.
    let (windows, linux) = if cfg!(windows) {
        ("false", "true")
    } else {
        ("true", "false")
    };
    let caps = dir.path().join("capabilities");
    std::fs::create_dir_all(&caps).unwrap();
    std::fs::write(caps.join("elsewhere.py"), "print('{}')\n").unwrap();
    std::fs::write(
        caps.join("script.elsewhere.yaml"),
        format!(
            "id: script.elsewhere\nversion: 1\ntype: script\ndescription: fixture\n\
             execution:\n  kind: script\n  runtime: python\n  entrypoint:\n    script: elsewhere.py\n\
             quality:\n  deterministic: true\nrisk:\n  side_effect: false\n\
             platform:\n  windows: {windows}\n  linux: {linux}\ntimeout_seconds: 5\n"
        ),
    )
    .unwrap();

    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "wrong.platform",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.elsewhere".into()),
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert!(matches!(result.verdict, Verdict::Refused { .. }));
    assert!(
        result.verdict.reason().unwrap().contains("platform"),
        "got {}",
        result.summary()
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );
}

#[test]
fn a_side_effecting_capability_is_refused_under_the_emergency_profile() {
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "effect.fixture",
        "print('{}')\n",
        "risk_override: unused\n",
    );
    // Rewrite the manifest so it declares a side effect with a usable idempotency key.
    let caps = dir.path().join("capabilities");
    std::fs::write(
        caps.join("effect.fixture.yaml"),
        "id: effect.fixture\nversion: 1\ntype: tool\ndescription: fixture\n\
         execution:\n  kind: script\n  runtime: python\n  entrypoint:\n    script: effect_fixture.py\n\
         quality:\n  deterministic: false\n\
         risk:\n  side_effect: true\n  idempotency:\n    key_template: \"fixture:{target}\"\n\
         platform:\n  windows: true\n  linux: true\ntimeout_seconds: 5\n",
    )
    .unwrap();

    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "emergency.effect",
        QualitySpec::best_effort(),
        Some(PrecisionClass::P3),
        TaskPlan {
            capability: Some("effect.fixture".into()),
            ..TaskPlan::empty()
        },
    );

    let config = WorkerConfig {
        profile: pearl_core::RuntimeProfile::Emergency,
        ..fixture_config(&dir)
    };
    let worker = Worker::new(config, clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    // §48: a system that has lost confidence in its own judgement observes, it does not act.
    assert!(
        matches!(result.verdict, Verdict::Refused { .. }),
        "got {}",
        result.summary()
    );
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );
}

// ---------------------------------------------------------------------------
// Failure and retry
// ---------------------------------------------------------------------------

#[test]
fn a_failing_capability_is_retried_then_dead_lettered() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.broken",
        "import json, sys\nsys.stderr.write('deliberate failure\\n')\n\
         print(json.dumps({\"ok\": False}))\nsys.exit(1)\n",
        "",
    );
    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "failing.task",
        QualitySpec::best_effort(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.broken".into()),
            ..TaskPlan::empty()
        },
    );

    // max_attempts 2, zero backoff, so the whole retry lifecycle runs in one test.
    let worker = Worker::new(fixture_config(&dir), clock).unwrap();

    let first = worker.run_once(&mut store).unwrap().unwrap();
    assert!(matches!(first.verdict, Verdict::Failed { .. }));
    assert!(first.verdict.is_retryable());
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::RetryWait
    );

    // The queue promotes it once the (zero) backoff has elapsed, then it fails again and
    // is dead-lettered rather than retried forever.
    let stop = AtomicBool::new(false);
    let mut seen = 0;
    while seen < 4 {
        match worker.run_once(&mut store).unwrap() {
            Some(_) => seen += 1,
            None => {
                let promoted = pearl_queue::WorkQueue::new(
                    RetryPolicy::new(
                        2,
                        TimeDelta::try_seconds(0).unwrap(),
                        TimeDelta::try_seconds(0).unwrap(),
                    )
                    .unwrap(),
                    pearl_core::RuntimeProfile::Normal,
                    clock,
                )
                .promote_ready_retries(&mut store)
                .unwrap();
                if promoted.is_empty() {
                    break;
                }
            }
        }
    }
    let _ = stop;

    let final_state = store.get_task(&task_id).unwrap().unwrap().state;
    assert_eq!(
        final_state,
        TaskState::Failed,
        "attempts should be exhausted, not retried indefinitely"
    );
    let attempts_recorded = store.get_task(&task_id).unwrap().unwrap().attempt_count;
    assert!(attempts_recorded >= 2, "got {attempts_recorded}");
}

#[test]
fn a_capability_that_overruns_is_timed_out_not_left_running() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(&dir, "script.slow", "import time\ntime.sleep(120)\n", "");
    let mut store = store(&dir);
    // A real clock: the supervisor enforces the deadline by comparing against it.
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "slow.task",
        QualitySpec::best_effort(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.slow".into()),
            // One second, overriding the manifest: the task knows what it asked for.
            timeout_seconds: Some(1),
            ..TaskPlan::empty()
        },
    );

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert_eq!(result.verdict, Verdict::TimedOut, "{}", result.summary());
    assert!(result.exit_code.is_none(), "a timeout has no exit code");
    let state = store.get_task(&task_id).unwrap().unwrap().state;
    assert!(
        matches!(state, TaskState::RetryWait | TaskState::Failed),
        "got {state}"
    );
}

// ---------------------------------------------------------------------------
// Crash recovery
// ---------------------------------------------------------------------------

#[test]
fn a_task_abandoned_by_a_dead_worker_is_reclaimed_and_completed_by_another() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        "import json\nprint(json.dumps({\"ok\": True}))\n",
        "",
    );
    let mut store = store(&dir);
    let clock = TestClock::new();

    let task_id = enqueue(
        &mut store,
        &clock,
        "crashed.task",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.plain".into()),
            ..TaskPlan::empty()
        },
    );

    // Worker A claims the task and then disappears without releasing anything.
    let leases = LeaseManager::new(LeaseConfig::default(), clock.clone());
    leases
        .claim(&mut store, &task_id, &WorkerId::new("worker:doomed"))
        .unwrap();
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Leased
    );

    // Time passes; the lease lapses; the reaper returns the task to the queue.
    clock.advance_secs(3600);
    let report = leases.reap(&mut store).unwrap();
    assert_eq!(report.reclaimed, vec![task_id.clone()]);
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Ready,
        "a claimed-but-unstarted task is safe to offer again immediately"
    );

    // Worker B picks it up and carries it to a verified outcome.
    let config = WorkerConfig {
        worker_id: WorkerId::new("worker:survivor"),
        ..fixture_config(&dir)
    };
    let worker = Worker::new(config, clock.clone()).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert_eq!(result.verdict, Verdict::Verified, "{}", result.summary());
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::VerifiedSuccess
    );
    // The abandonment is in the history, not papered over.
    let events: Vec<String> = store
        .ledger()
        .read_task(&task_id)
        .unwrap()
        .iter()
        .map(|e| e.event_type().to_string())
        .collect();
    assert!(events.iter().any(|e| e == "lease.expired"), "{events:?}");
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

#[test]
fn an_empty_queue_yields_nothing_rather_than_blocking() {
    let dir = TempDir::new().unwrap();
    write_capability(&dir, "script.plain", "print('{}')\n", "");
    let mut store = store(&dir);
    let worker = Worker::new(fixture_config(&dir), SystemClock).unwrap();
    assert!(worker.run_once(&mut store).unwrap().is_none());
}

#[test]
fn the_worker_drains_a_queue_of_several_tasks() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = TempDir::new().unwrap();
    write_capability(
        &dir,
        "script.plain",
        "import json\nprint(json.dumps({\"ok\": True}))\n",
        "",
    );
    let mut store = store(&dir);
    let clock = SystemClock;

    for i in 0..3 {
        enqueue(
            &mut store,
            &clock,
            &format!("batch.task-{i}"),
            QualitySpec::mechanical(),
            Some(PrecisionClass::P0),
            TaskPlan {
                capability: Some("script.plain".into()),
                ..TaskPlan::empty()
            },
        );
    }

    let worker = Worker::new(fixture_config(&dir), clock).unwrap();
    let mut verified = 0;
    while let Some(result) = worker.run_once(&mut store).unwrap() {
        if result.is_verified() {
            verified += 1;
        }
    }

    assert_eq!(verified, 3);
    assert_eq!(store.count_by_state(TaskState::VerifiedSuccess).unwrap(), 3);
    assert_eq!(store.count_by_state(TaskState::Ready).unwrap(), 0);
}

#[test]
fn a_worker_with_no_permission_file_admits_nothing() {
    // Not an error at construction, but nothing will be permitted to run: §45 makes the
    // file an allow-list, and an absent list admits nothing.
    let dir = TempDir::new().unwrap();
    write_capability(&dir, "script.plain", "print('{}')\n", "");
    let mut store = store(&dir);
    let clock = SystemClock;
    let task_id = enqueue(
        &mut store,
        &clock,
        "no.permissions",
        QualitySpec::mechanical(),
        Some(PrecisionClass::P0),
        TaskPlan {
            capability: Some("script.plain".into()),
            ..TaskPlan::empty()
        },
    );

    let config = WorkerConfig {
        permissions_path: None,
        ..fixture_config(&dir)
    };
    let worker = Worker::new(config, clock).unwrap();
    let result = worker.run_once(&mut store).unwrap().unwrap();

    assert!(matches!(result.verdict, Verdict::Refused { .. }));
    assert_eq!(
        store.get_task(&task_id).unwrap().unwrap().state,
        TaskState::Blocked
    );
}
