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
