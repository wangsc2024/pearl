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
    path.push("pearl");
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
    assert_eq!(json["findings"][0]["article"], 9);
    assert_eq!(json["findings"][0]["check"], "check_has_timeout");
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
