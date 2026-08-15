//! Integration tests that execute real scripts through the full pearl-runtime pipeline.
//!
//! These tests exercise:
//! - The `ScriptRuntimeAdapter` with `PlatformSupervisor`
//! - Stdout/stderr collection (Gap 1 fix verification)
//! - Structured JSON output parsing from script stdout
//! - The PEARL_INPUT env var contract
//!
//! Requires a Python interpreter on PATH (see `pearl_runtime::programs`). Cases that need
//! an interpreter the machine does not have are skipped with a message rather than failed:
//! a missing POSIX shell on Windows is an environment fact, not a defect in the adapter.

use chrono::TimeDelta;
use pearl_process_supervisor::PlatformSupervisor;
use pearl_runtime::{
    programs, RuntimeAdapter, RuntimeExitStatus, ScriptRuntimeAdapter, ScriptSpec,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Whether the resolved Python interpreter can run.
fn python_usable() -> bool {
    programs::is_available(&programs::python())
}

/// Whether the `Shell` runtime is meaningfully testable here.
///
/// On Windows, `bash.exe` is usually the WSL launcher, which cannot open a Windows-path
/// script. Rather than pretend, the shell cases run only where a POSIX shell genuinely
/// understands the paths we hand it: any Unix, or a Windows machine where the operator has
/// pointed `PEARL_BASH` at a real one such as Git Bash.
fn shell_usable() -> bool {
    if cfg!(windows) && std::env::var("PEARL_BASH").is_err() {
        return false;
    }
    programs::is_available(&programs::bash())
}

/// Helper to get the workspace root (where Cargo.toml lives).
fn workspace_root() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    PathBuf::from(manifest_dir)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Verify that a real Python script can be executed and stdout is captured.
#[test]
fn execute_real_python_verify_json_valid_input() {
    if !python_usable() {
        eprintln!(
            "skipping: no usable Python interpreter ({})",
            programs::python()
        );
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    let script_path = workspace_root().join("capabilities/scripts/examples/verify_json.py");
    assert!(
        script_path.exists(),
        "verify_json.py not found at {script_path:?}"
    );

    let input = serde_json::json!({
        "data": {"name": "test", "value": 42},
        "schema": {
            "required_keys": ["name", "value"],
            "types": {"name": "string", "value": "number"}
        }
    });

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Python,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(30).unwrap(),
        input_payload: Some(input),
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    // Verify stdout was actually captured (not empty -- Gap 1 fix).
    assert!(
        !result.stdout.is_empty(),
        "stdout should not be empty -- collect_output should capture piped stdout"
    );

    // Verify structured output was parsed.
    assert!(result.structured_output.is_some());
    let output = result.structured_output.as_ref().unwrap();
    assert_eq!(output["valid"], true);

    // Verify exit code is 0 (valid input).
    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 0 });
    assert!(result.is_success());

    // Verify stderr has diagnostic output.
    assert!(
        !result.stderr.is_empty(),
        "stderr should contain diagnostic messages"
    );
    assert!(result.stderr.contains("Validation passed"));
}

/// Verify that schema validation failures are properly reported.
#[test]
fn execute_real_python_verify_json_invalid_input() {
    if !python_usable() {
        eprintln!(
            "skipping: no usable Python interpreter ({})",
            programs::python()
        );
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    let script_path = workspace_root().join("capabilities/scripts/examples/verify_json.py");

    let input = serde_json::json!({
        "data": {"name": 123},
        "schema": {
            "required_keys": ["name", "value"],
            "types": {"name": "string"}
        }
    });

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Python,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(30).unwrap(),
        input_payload: Some(input),
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    // Script should exit with code 1 (invalid).
    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 1 });
    assert!(!result.is_success());

    // Structured output should report validation errors.
    let output = result.structured_output.unwrap();
    assert_eq!(output["valid"], false);
    let errors = output["errors"].as_array().unwrap();
    assert!(!errors.is_empty());
}

/// Verify that a simple shell script can be executed and output captured.
#[test]
fn execute_real_shell_echo() {
    if !shell_usable() {
        eprintln!("skipping: no usable POSIX shell (set PEARL_BASH to opt in on Windows)");
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    // Create a temporary shell script.
    let tmp_dir = tempfile::tempdir().unwrap();
    let script_path = tmp_dir.path().join("echo_test.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/bash
echo "diagnostic output" >&2
echo '{"result": "hello", "count": 3}'
"#,
    )
    .unwrap();

    // Make executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Shell,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(10).unwrap(),
        input_payload: None,
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    assert!(result.is_success());
    assert!(result.stdout.contains(r#""result": "hello""#));
    assert!(result.stderr.contains("diagnostic output"));

    // Structured output should be parsed from the JSON line.
    let output = result.structured_output.unwrap();
    assert_eq!(output["result"], "hello");
    assert_eq!(output["count"], 3);
}

/// Verify timeout enforcement with a real long-running script.
#[test]
fn execute_real_script_timeout() {
    if !shell_usable() {
        eprintln!("skipping: no usable POSIX shell (see execute_real_python_timeout)");
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    // Create a script that sleeps longer than the timeout.
    let tmp_dir = tempfile::tempdir().unwrap();
    let script_path = tmp_dir.path().join("slow.sh");
    std::fs::write(
        &script_path,
        "#!/bin/bash\nsleep 60\necho '{\"done\": true}'\n",
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Shell,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(1).unwrap(),
        input_payload: None,
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    assert_eq!(result.exit_status, RuntimeExitStatus::TimedOut);
    assert!(!result.is_success());
}

/// Verify timeout enforcement on every platform, using the interpreter that is always
/// required rather than the optional shell.
#[test]
fn execute_real_python_timeout() {
    if !python_usable() {
        eprintln!(
            "skipping: no usable Python interpreter ({})",
            programs::python()
        );
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    let tmp_dir = tempfile::tempdir().unwrap();
    let script_path = tmp_dir.path().join("slow.py");
    std::fs::write(
        &script_path,
        "import time\ntime.sleep(60)\nprint('{\"done\": true}')\n",
    )
    .unwrap();

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Python,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(1).unwrap(),
        input_payload: None,
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    assert_eq!(result.exit_status, RuntimeExitStatus::TimedOut);
    assert!(!result.is_success(), "a timeout is never success");
}

/// Verify that PEARL_INPUT env var is correctly passed to scripts.
#[test]
fn execute_real_python_reads_pearl_input() {
    if !python_usable() {
        eprintln!(
            "skipping: no usable Python interpreter ({})",
            programs::python()
        );
        return;
    }
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let clock = pearl_core::SystemClock;

    // Create a script that echoes PEARL_INPUT back.
    let tmp_dir = tempfile::tempdir().unwrap();
    let script_path = tmp_dir.path().join("echo_input.py");
    std::fs::write(
        &script_path,
        r#"#!/usr/bin/env python3
import os, json
payload = os.environ.get("PEARL_INPUT", "{}")
data = json.loads(payload)
print(json.dumps({"received": data}))
"#,
    )
    .unwrap();

    let input = serde_json::json!({"task_id": "test-001", "score": 95});

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Python,
        entrypoint: script_path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(10).unwrap(),
        input_payload: Some(input.clone()),
    };

    let result = adapter.execute(&spec, &clock).unwrap();

    assert!(result.is_success());
    let output = result.structured_output.unwrap();
    assert_eq!(output["received"]["task_id"], "test-001");
    assert_eq!(output["received"]["score"], 95);
}

// ---------------------------------------------------------------- effect.notify
//
// `notify.py` publishes to an AgentFlow-Notify hub. These cases deliberately never reach
// one: every path below is decidable from the input and the environment alone, which is
// the property that lets an unconfigured or malformed notification cost nothing.
//
// The one thing not tested here is a successful publish, because that needs a running hub
// and would either be a network-dependent test or a mock asserting that our own mock was
// called. What a hub confirms -- that the topic, priority and tags arrive as sent -- was
// verified by hand against a local `agentflow-notify` instance and recorded in ADR-0006.

/// Runs `notify.py` with the given payload and environment, returning the result.
fn run_notify(payload: serde_json::Value, env: &[(&str, &str)]) -> pearl_runtime::RuntimeResult {
    let adapter = ScriptRuntimeAdapter::new(PlatformSupervisor::default());
    let script_path = workspace_root().join("capabilities/scripts/notify.py");
    assert!(
        script_path.exists(),
        "notify.py not found at {script_path:?}"
    );

    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::Python,
        entrypoint: script_path,
        args: vec![],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
        timeout: TimeDelta::try_seconds(30).unwrap(),
        input_payload: Some(payload),
    };
    adapter.execute(&spec, &pearl_core::SystemClock).unwrap()
}

fn a_notification() -> serde_json::Value {
    serde_json::json!({
        "title": "subject",
        "message": "body",
        "idempotency_key": "notify:PEARL_kiro:selftest:2026-08-15",
    })
}

/// An environment pointing at nothing, with the credentials explicitly cleared.
///
/// Port 9 is discard, so no test here can reach a hub even if the developer happens to have
/// one running locally — an earlier version of these tests passed for that reason, which
/// made them assertions about the machine rather than about the script. Clearing the token
/// variables matters for the same reason: `env` here is merged onto the parent process's,
/// so an ambient `AGENTFLOW_NOTIFY_TOKEN` would otherwise decide the outcome.
///
/// The consequence worth keeping in mind: a validation bug now shows up as exit 1
/// ("unreachable") instead of exit 0, which is still distinguishable from the expected 2.
fn unreachable_hub() -> Vec<(&'static str, &'static str)> {
    vec![
        ("AGENTFLOW_NOTIFY_URL", "http://127.0.0.1:9"),
        ("AGENTFLOW_NOTIFY_ALLOW_ANON", "1"),
        ("AGENTFLOW_NOTIFY_TOKEN", ""),
        ("AGENTFLOW_NOTIFY_TIMEOUT_SECONDS", "5"),
    ]
}

/// Exit 2 is "could not even try", and it must be distinguishable from exit 1, "tried and
/// failed". Article 2's three-valued reasoning applied to a side effect: a configuration
/// gap is not a delivery failure, and retrying it would change nothing.
#[test]
fn notify_refuses_to_run_without_a_hub_configured() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    let result = run_notify(
        a_notification(),
        &[("AGENTFLOW_NOTIFY_URL", ""), ("AGENTFLOW_NOTIFY_TOKEN", "")],
    );

    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 2 });
    let output = result.structured_output.as_ref().expect("machine-readable");
    assert_eq!(output["accepted"], false);
    assert!(
        output["error"]
            .as_str()
            .unwrap()
            .contains("AGENTFLOW_NOTIFY_URL"),
        "the error must name the variable to set: {output}"
    );
}

#[test]
fn notify_refuses_to_publish_without_a_token_unless_anonymous_is_explicit() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    let result = run_notify(
        a_notification(),
        &[
            ("AGENTFLOW_NOTIFY_URL", "http://127.0.0.1:9"),
            ("AGENTFLOW_NOTIFY_TOKEN", ""),
            ("AGENTFLOW_NOTIFY_ALLOW_ANON", ""),
        ],
    );

    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 2 });
    let error = result.structured_output.as_ref().unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(error.contains("AGENTFLOW_NOTIFY_TOKEN"), "got {error}");
    // And it says how to opt out, because a local hub legitimately runs without auth.
    assert!(error.contains("ALLOW_ANON"), "got {error}");
}

/// The hub's `Topic` type accepts 1-64 characters of `A-Za-z0-9_-`. Checking it here means
/// a bad topic costs no network round trip and the message names the actual rule rather
/// than returning the hub's 400.
#[test]
fn notify_rejects_a_topic_the_hub_would_refuse() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    for bad in ["has space", "a/b", &"x".repeat(65)] {
        let mut payload = a_notification();
        payload["topic"] = bad.into();
        let result = run_notify(payload, &unreachable_hub());
        assert_eq!(
            result.exit_status,
            RuntimeExitStatus::Exited { code: 2 },
            "topic {bad:?} should have been refused"
        );
    }
}

/// An absent topic uses the default; a topic that is present and empty is a caller bug.
/// Treating the two the same would publish to `PEARL_kiro` on behalf of a task that had
/// tried to name something else and got it wrong.
#[test]
fn notify_distinguishes_an_absent_topic_from_an_empty_one() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    let mut payload = a_notification();
    payload["topic"] = "".into();
    let result = run_notify(payload, &unreachable_hub());
    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 2 });
    assert!(
        result.structured_output.as_ref().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("omit it"),
        "the error should say what to do instead"
    );

    // Absent, by contrast, gets as far as the network — which is the observable difference.
    let result = run_notify(a_notification(), &unreachable_hub());
    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 1 });
}

/// Priorities are named on the PEARL side and numeric on the hub's. A task spec should not
/// have to encode another system's numbering, so passing the number is an error that says
/// what to write instead.
#[test]
fn notify_takes_named_priorities_not_the_hubs_numbers() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    let mut payload = a_notification();
    payload["priority"] = 4.into();
    let result = run_notify(payload, &unreachable_hub());

    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 2 });
    let error = result.structured_output.as_ref().unwrap()["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        error.contains("urgent"),
        "the names should be listed: {error}"
    );
}

/// Every field the ledger needs is required. A notification with no idempotency key could
/// not be deduplicated, and Article 5 does not allow a side effect that cannot be.
#[test]
fn notify_requires_a_title_a_message_and_an_idempotency_key() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    for missing in ["title", "message", "idempotency_key"] {
        let mut payload = a_notification();
        payload.as_object_mut().unwrap().remove(missing);
        let result = run_notify(payload, &unreachable_hub());
        assert_eq!(
            result.exit_status,
            RuntimeExitStatus::Exited { code: 2 },
            "a notification with no {missing} should be refused"
        );
        let error = result.structured_output.as_ref().unwrap()["error"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(error.contains(missing), "the error should name it: {error}");
    }
}

/// An unreachable hub is exit 1, not exit 2: the notification was well formed and the
/// attempt was real, so a retry is worth making.
#[test]
fn notify_reports_an_unreachable_hub_as_a_failed_attempt() {
    if !python_usable() {
        eprintln!("skipping: no usable Python interpreter");
        return;
    }
    let result = run_notify(a_notification(), &unreachable_hub());

    assert_eq!(result.exit_status, RuntimeExitStatus::Exited { code: 1 });
    let output = result.structured_output.as_ref().unwrap();
    assert_eq!(output["accepted"], false);
    assert!(
        output["error"].as_str().unwrap().contains("unreachable"),
        "got {output}"
    );
}

// ------------------------------------------------------------- shipped prompts
//
// An agent capability's entrypoint is a prompt template, and `prompt::validate` is the check
// that runs before any model is contacted: an unfillable prompt costs nothing. That makes it
// exactly the property worth testing here, because it is decidable without a model — and
// because the failure it catches is silent. A prompt referencing `{{subject}}` when the
// workflow supplies `topic` renders fine right up until it is asked to, and then fails inside
// a run that has already claimed a lease.

/// Every `{{placeholder}}` in a shipped prompt must be fillable from the payload its workflow
/// actually provides. The payloads below mirror the `input_from` blocks in
/// `capabilities/workflows/` and `applications/ddp/workflows/`.
#[test]
fn shipped_prompts_render_from_the_payloads_their_workflows_supply() {
    let cases: &[(&str, serde_json::Value)] = &[
        (
            "applications/ddp/prompts/zen_koan_compose.md",
            // ddp.zen-koan's `compose` step takes exactly these from `select`.
            serde_json::json!({ "topic": "趙州狗子", "source": "禪宗公案選集" }),
        ),
        (
            "capabilities/agents/prompts/propose_plan.md",
            // A planning step gets task identity plus whatever the run is about.
            serde_json::json!({
                "task_id": "t.1",
                "task_type": "digest",
                "context": { "items": [1, 2] },
            }),
        ),
        (
            "capabilities/agents/prompts/synthesize.md",
            serde_json::json!({ "task_id": "t.1", "task_type": "digest", "facts": [] }),
        ),
    ];

    for (relative, payload) in cases {
        let path = workspace_root().join(relative);
        assert!(path.exists(), "{relative} is missing");

        let spec = ScriptSpec {
            runtime: pearl_governance::manifest::Runtime::LlamaCpp,
            entrypoint: path,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: Some(payload.clone()),
        };

        pearl_runtime::prompt::validate(&spec)
            .unwrap_or_else(|e| panic!("{relative} cannot be rendered from its payload: {e}"));

        let rendered = pearl_runtime::prompt::render(&spec)
            .unwrap_or_else(|e| panic!("{relative} failed to render: {e}"));
        assert!(
            !rendered.contains("{{"),
            "{relative} still has an unrendered placeholder"
        );
    }
}

/// The converse, so the test above is known to be capable of failing: a payload missing a key
/// the prompt needs is refused, and the error names the key.
#[test]
fn a_prompt_whose_payload_lacks_a_key_is_refused_before_any_model_is_called() {
    let path = workspace_root().join("applications/ddp/prompts/zen_koan_compose.md");
    let spec = ScriptSpec {
        runtime: pearl_governance::manifest::Runtime::LlamaCpp,
        entrypoint: path,
        args: vec![],
        env: BTreeMap::new(),
        cwd: None,
        timeout: TimeDelta::try_seconds(30).unwrap(),
        // `source` withheld: this is what a workflow that forgot an `input_from` line looks like.
        input_payload: Some(serde_json::json!({ "topic": "趙州狗子" })),
    };

    let err = pearl_runtime::prompt::validate(&spec).unwrap_err();
    assert!(err.to_string().contains("source"), "got {err}");
}
