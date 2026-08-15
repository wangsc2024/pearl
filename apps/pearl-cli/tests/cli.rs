//! CLI integration tests — 系統開發需求書 §59.
//!
//! These invoke the built binary rather than calling library functions, because the
//! things worth testing here are exactly the things a library test cannot see: exit
//! codes, stream separation, and whether the JSON on stdout actually parses.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Locates the `pearl` binary next to the test executable.
fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe path");
    path.pop(); // deps/
    path.pop(); // debug/
                // `std::env::consts::EXE_SUFFIX` rather than a bare name: on Windows the binary is
                // `pearl.exe`, and hard-coding the Unix form made every CLI test fail there.
    path.push(format!("pearl{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "pearl binary not found at {}",
        path.display()
    );
    path
}

fn run(db: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .expect("failed to invoke pearl")
}

fn run_json(db: &Path, args: &[&str]) -> (Output, serde_json::Value) {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = Command::new(binary())
        .arg("--db")
        .arg(db)
        .args(&full)
        .output()
        .expect("failed to invoke pearl");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout was not valid JSON ({e}): {stdout}"));
    (output, json)
}

fn code(output: &Output) -> i32 {
    output.status.code().expect("process exited via signal")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

const MECHANICAL_SPEC: &str = r#"
id: daily.digest
version: 1
task_type: digest
description: Assemble the daily digest
precision_class: p1
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: true
timeout_seconds: 300
"#;

const UNVERIFIABLE_SPEC: &str = r#"
id: research.citations
version: 1
task_type: research
precision_class: p3
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: false
timeout_seconds: 600
"#;

/// Writes a spec file and returns its path.
fn spec_file(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn submit_then_inspect_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);

    let submit = run(&db, &["task", "submit", spec.to_str().unwrap()]);
    assert_eq!(code(&submit), 0, "stderr: {}", stderr(&submit));
    assert!(stdout(&submit).contains("daily.digest"));

    let inspect = run(&db, &["task", "inspect", "daily.digest"]);
    assert_eq!(code(&inspect), 0);
    let text = stdout(&inspect);
    assert!(text.contains("daily.digest"));
    assert!(text.contains("created"));
}

#[test]
fn article_2_unverifiable_task_is_refused_with_its_own_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "bad.yaml", UNVERIFIABLE_SPEC);

    let output = run(&db, &["task", "submit", spec.to_str().unwrap()]);

    // Exit 2 marks a Constitution violation, distinct from exit 1 for operational error.
    assert_eq!(code(&output), 2, "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("Article 2"),
        "stderr: {}",
        stderr(&output)
    );

    // And nothing was persisted.
    let list = run(&db, &["task", "list"]);
    assert!(stdout(&list).contains("no tasks"));
}

#[test]
fn duplicate_submission_is_an_error_not_a_silent_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);

    assert_eq!(
        code(&run(&db, &["task", "submit", spec.to_str().unwrap()])),
        0
    );
    let second = run(&db, &["task", "submit", spec.to_str().unwrap()]);
    assert_eq!(code(&second), 1);
    assert!(stderr(&second).contains("already exists"));
}

#[test]
fn json_mode_emits_only_json_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    // §26: stdout must be machine JSON only, never JSON mixed with prose.
    let (output, json) = run_json(&db, &["task", "inspect", "daily.digest"]);
    assert_eq!(code(&output), 0);
    assert_eq!(json["task"]["task_id"], "daily.digest");
    assert_eq!(json["task"]["state"], "created");
    assert!(json["runs"].is_array());
}

#[test]
fn missing_task_reports_on_stderr_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    run(&db, &["doctor"]); // create the db

    let output = run(&db, &["task", "inspect", "nope"]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("not found"));
    assert!(stdout(&output).is_empty(), "errors must not pollute stdout");
}

#[test]
fn cancel_moves_the_task_to_a_terminal_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    let cancel = run(
        &db,
        &[
            "task",
            "cancel",
            "daily.digest",
            "--reason",
            "operator stopped it",
        ],
    );
    assert_eq!(code(&cancel), 0, "stderr: {}", stderr(&cancel));

    let (_, json) = run_json(&db, &["task", "inspect", "daily.digest"]);
    assert_eq!(json["task"]["state"], "cancelled");
    assert_eq!(json["task"]["last_reason"], "operator stopped it");
}

#[test]
fn event_log_shows_the_task_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);
    run(&db, &["task", "cancel", "daily.digest"]);

    let output = run(&db, &["event", "log", "daily.digest"]);
    let text = stdout(&output);
    assert!(text.contains("task.created"));
    assert!(text.contains("task.state_changed"));
    // Cancellation is terminal, so a completion event is recorded too.
    assert!(text.contains("task.completed"));
}

#[test]
fn replay_rebuilds_state_without_changing_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    let (_, before) = run_json(&db, &["task", "inspect", "daily.digest"]);
    let (replay, summary) = run_json(&db, &["event", "replay"]);
    assert_eq!(code(&replay), 0);
    assert!(summary["total_events"].as_u64().unwrap() > 0);

    let (_, after) = run_json(&db, &["task", "inspect", "daily.digest"]);
    assert_eq!(before, after, "replay must not alter observable state");
}

#[test]
fn queue_status_reports_depth() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "digest.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    // A freshly created task is not yet READY, so the queue is empty.
    let (_, json) = run_json(&db, &["queue", "status"]);
    assert_eq!(json["depth"], 0);
    assert_eq!(json["retry_wait"], 0);
}

#[test]
fn doctor_reports_a_healthy_empty_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let (output, json) = run_json(&db, &["doctor"]);
    assert_eq!(code(&output), 0);
    assert_eq!(json["ledger_events"], 0);
    assert_eq!(json["healthy"], true);
    assert_eq!(json["expired_leases"], 0);
}

#[test]
fn doctor_warns_about_unverified_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(
        dir.path(),
        "assured.yaml",
        r#"
id: research.citations
version: 1
task_type: research
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: false
assurance:
  - script: verifier.citations
timeout_seconds: 60
"#,
    );
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    // Walk the task to UNVERIFIED, which is what Article 2 produces when exactness is
    // demanded and nothing can confirm it.
    let store_path = db.clone();
    {
        use pearl_core::{Clock, SystemClock, TaskId, TaskState};
        use pearl_state::StateStore;
        let mut store = StateStore::open(&store_path).unwrap();
        let id = TaskId::parse("research.citations").unwrap();
        for state in [
            TaskState::Planning,
            TaskState::Planned,
            TaskState::Ready,
            TaskState::Leased,
            TaskState::Running,
            TaskState::Verifying,
            TaskState::Unverified,
        ] {
            store
                .transition(&id, state, None, None, SystemClock.now())
                .unwrap();
        }
    }

    let (_, json) = run_json(&db, &["doctor"]);
    assert_eq!(json["healthy"], false);
    let warnings = json["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("UNVERIFIED")),
        "doctor should surface unverifiable work: {warnings:?}"
    );
}

#[test]
fn constitution_check_passes_on_the_repository_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let capabilities = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("capabilities");

    let output = run(
        &db,
        &["constitution", "check", capabilities.to_str().unwrap()],
    );
    assert_eq!(
        code(&output),
        0,
        "the repository's own capabilities must satisfy the Constitution.\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(stdout(&output).contains("PASSED"));
}

#[test]
fn constitution_check_fails_on_a_violating_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let manifests = dir.path().join("caps");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(
        manifests.join("bad.yaml"),
        r#"
id: bad.deterministic-agent
version: 1
type: script
description: Claims determinism but asks a model to decide.
execution:
  kind: agent
  runtime: claude_code
quality:
  deterministic: true
risk:
  side_effect: true
platform:
  windows: true
  linux: true
"#,
    )
    .unwrap();

    let output = run(&db, &["constitution", "check", manifests.to_str().unwrap()]);

    assert_eq!(code(&output), 2, "the gate must fail the build");
    let text = stdout(&output);
    assert!(text.contains("Article 1"), "{text}");
    assert!(text.contains("Article 5"), "{text}");
    assert!(text.contains("Article 9"), "{text}");
    assert!(text.contains("FAILED"));
}

#[test]
fn constitution_check_json_is_machine_readable() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let manifests = dir.path().join("caps");
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(
        manifests.join("bad.yaml"),
        r#"
id: bad.no-timeout
version: 1
type: script
description: No deadline.
execution:
  kind: script
  runtime: python
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
"#,
    )
    .unwrap();

    let (output, json) = run_json(&db, &["constitution", "check", manifests.to_str().unwrap()]);
    assert_eq!(code(&output), 2);
    assert_eq!(json["passed"], false);
    assert_eq!(json["inspected"], 1);
    assert_eq!(json["violations"], 1);
    // Located by content rather than by index: findings are sorted by article, so asserting on
    // position would break every time a lower-numbered check is added.
    let findings = json["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["article"] == 9 && f["check"] == "check_has_timeout"),
        "expected the Article 9 timeout violation, got {findings:?}"
    );
}

#[test]
fn constitution_check_on_a_missing_path_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let output = run(&db, &["constitution", "check", "/no/such/directory"]);
    // Exit 1, not 2: failing to look is different from finding a violation.
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("does not exist"));
}

#[test]
fn lease_reap_on_an_idle_kernel_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    run(&db, &["doctor"]);

    let (output, json) = run_json(&db, &["lease", "reap"]);
    assert_eq!(code(&output), 0);
    assert!(json["reclaimed"].as_array().unwrap().is_empty());
}

#[test]
fn unknown_state_filter_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    run(&db, &["doctor"]);

    let output = run(&db, &["task", "list", "--state", "not_a_state"]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("unknown state"));
}

#[test]
fn malformed_spec_is_rejected_before_touching_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "broken.yaml", "id: [unclosed");

    let output = run(&db, &["task", "submit", spec.to_str().unwrap()]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("parse"));
}

// ---------------------------------------------------------------------------
// script run, verify, workflow — §59
// ---------------------------------------------------------------------------

/// The repository's own directories, so these tests exercise what ships.
fn repo(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(sub)
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        || Command::new("python")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

#[test]
fn script_run_executes_the_capability_for_real() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let (output, json) = run_json(
        &db,
        &[
            "script",
            "run",
            "script.task-score",
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
            "--input",
            r#"{"priority":4,"time_proximity":"overdue"}"#,
        ],
    );

    // Really executed: the capability's own output is present, not a "would_execute" placeholder.
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(json["capability_id"], "script.task-score");
    assert!(
        json["output"]["score"].is_number(),
        "expected the script's own JSON output, got {json}"
    );
}

#[test]
fn script_run_reports_a_failing_capability_through_its_exit_code() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    // Out-of-domain input: the script exits 2, and a shell must be able to see that.
    let output = run(
        &db,
        &[
            "script",
            "run",
            "script.task-score",
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
            "--input",
            r#"{"priority":99}"#,
        ],
    );
    assert_ne!(code(&output), 0, "stdout: {}", stdout(&output));
}

#[test]
fn script_run_on_an_unknown_capability_fails_without_running_anything() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let output = run(
        &db,
        &[
            "script",
            "run",
            "script.does-not-exist",
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
        ],
    );
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("script.does-not-exist"));
}

#[test]
fn verify_run_validates_a_document_against_a_real_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let (output, json) = run_json(
        &db,
        &[
            "verify",
            "run",
            "--schema",
            "verification-result-v1",
            "--schemas-path",
            repo("schemas").to_str().unwrap(),
            "--input",
            r#"{"status":"pass","checks":[{"id":"c","status":"pass"}]}"#,
        ],
    );
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(json["passed"], true);
}

#[test]
fn verify_run_rejects_a_document_that_violates_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let output = run(
        &db,
        &[
            "verify",
            "run",
            "--schema",
            "verification-result-v1",
            "--schemas-path",
            repo("schemas").to_str().unwrap(),
            // `status` is constrained by the schema.
            "--input",
            r#"{"status":"probably-fine","checks":[]}"#,
        ],
    );
    // Exit 1: the check ran and rejected the document. That is a verdict.
    assert_eq!(code(&output), 1, "stdout: {}", stdout(&output));
}

#[test]
fn verify_run_distinguishes_a_check_that_could_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");

    let output = run(
        &db,
        &[
            "verify",
            "run",
            "--schema",
            "no-such-schema-v9",
            "--schemas-path",
            repo("schemas").to_str().unwrap(),
            "--input",
            "{}",
        ],
    );
    // Exit 2, not 1: nothing was verified, and a caller must be able to tell the difference
    // between "rejected" and "no verdict" (Article 2).
    assert_eq!(code(&output), 2, "stdout: {}", stdout(&output));
}

#[test]
fn verify_run_needs_something_to_inspect() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let output = run(&db, &["verify", "run", "--schema", "evidence-v1"]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("--input"));
}

#[test]
fn verify_task_reports_that_nothing_has_been_verified() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let spec = spec_file(dir.path(), "task.yaml", MECHANICAL_SPEC);
    run(&db, &["task", "submit", spec.to_str().unwrap()]);

    let output = run(&db, &["verify", "task", "daily.digest"]);
    // Silence is not a pass: an unverified task exits non-zero and says so.
    assert_eq!(code(&output), 1);
    assert!(
        stdout(&output).contains("no verification"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn workflow_validate_compiles_the_shipped_example() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let workflow = repo("capabilities/workflows/example.score-twice.yaml");

    let (output, json) = run_json(
        &db,
        &[
            "workflow",
            "validate",
            workflow.to_str().unwrap(),
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
        ],
    );
    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(json["status"], "valid");
    // Ordering is what compilation establishes, so it is what the output reports.
    assert_eq!(json["execution_order"][0], "score");
    assert_eq!(json["execution_order"][1], "score-again");
}

#[test]
fn workflow_validate_reports_every_problem_it_found() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    // A step demanding exactness with no verify step depending on it (§30), and an unknown
    // capability. Both must be reported, not just the first.
    let workflow = dir.path().join("bad.yaml");
    std::fs::write(
        &workflow,
        "name: bad\nsteps:\n  - id: a\n    capability: script.nope\n    step_type: run\n    timeout_secs: 5\n    exactness_required: true\n",
    )
    .unwrap();

    let (output, json) = run_json(
        &db,
        &[
            "workflow",
            "validate",
            workflow.to_str().unwrap(),
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
        ],
    );
    assert_eq!(code(&output), 1);
    let problems = json["problems"].as_array().unwrap();
    assert!(
        problems.len() >= 2,
        "expected both problems to be reported, got {problems:?}"
    );
}

#[test]
fn workflow_run_executes_the_steps_and_records_them() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let workflow = repo("capabilities/workflows/example.score-twice.yaml");

    let (output, json) = run_json(
        &db,
        &[
            "workflow",
            "run",
            workflow.to_str().unwrap(),
            "--task-id",
            "wf.test",
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
        ],
    );

    assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
    assert_eq!(json["success"], true);
    assert_eq!(json["steps"].as_array().unwrap().len(), 2);

    // A workflow run is a durable task, not a detached script: it has a run, and its outcome
    // is UNVERIFIED rather than success, because no assurance was declared (Article 2).
    let (_, task) = run_json(&db, &["task", "inspect", "wf.test"]);
    assert_eq!(task["task"]["state"], "unverified");
}

#[test]
fn workflow_run_refuses_to_run_a_plan_that_did_not_compile() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let workflow = dir.path().join("bad.yaml");
    std::fs::write(
        &workflow,
        "name: bad\nsteps:\n  - id: a\n    capability: script.nope\n    step_type: run\n    timeout_secs: 5\n",
    )
    .unwrap();

    let output = run(
        &db,
        &[
            "workflow",
            "run",
            workflow.to_str().unwrap(),
            "--capabilities-path",
            repo("capabilities").to_str().unwrap(),
        ],
    );
    // Exit 2: compilation is a gate (§30), so nothing ran.
    assert_eq!(code(&output), 2, "stdout: {}", stdout(&output));
    assert!(stderr(&output).contains("nothing was run"));
}

#[test]
fn a_completed_workflow_task_cannot_be_run_again_by_accident() {
    if !python_available() {
        eprintln!("skipping: no Python interpreter");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("pearl.db");
    let workflow = repo("capabilities/workflows/example.score-twice.yaml");
    let capabilities = repo("capabilities");
    let args = [
        "workflow",
        "run",
        workflow.to_str().unwrap(),
        "--task-id",
        "wf.once",
        "--capabilities-path",
        capabilities.to_str().unwrap(),
    ];

    assert_eq!(code(&run(&db, &args)), 0);

    // Second attempt on the same id: the state machine forbids it, and the message says what
    // to do instead rather than surfacing "not claimable".
    let output = run(&db, &args);
    assert_eq!(code(&output), 1);
    assert!(
        stderr(&output).contains("new --task-id"),
        "got: {}",
        stderr(&output)
    );
}
