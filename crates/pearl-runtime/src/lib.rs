//! # pearl-runtime
//!
//! Script runtime adapter contracts and implementations -- Constitution Article 9
//! (spawn/status/cancel/timeout/cleanup) and Article 1 (script-first execution).
//!
//! Each adapter wraps [`pearl_process_supervisor::ProcessSupervisor`] to execute scripts
//! with the full Article 9 contract. The adapter layer adds:
//!
//! - **Structured output parsing**: a JSON stdout contract where the last line of stdout
//!   may be a JSON object, automatically extracted into `RuntimeResult::structured_output`.
//! - **Environment filtering**: builds an explicit `BTreeMap` so only declared variables
//!   reach the child (Article 60 security).
//! - **Timeout enforcement**: delegates deadline handling to the process supervisor.
//! - **Exit-code-to-outcome mapping**: zero is success, non-zero is failure, timeout and
//!   signal are distinct outcomes.
//!
//! ## Supported runtimes
//!
//! The mechanical runtimes from [`pearl_governance::manifest::Runtime`]. Interpreter names
//! are resolved per platform by [`programs`] rather than hard-coded, because `python3` and
//! `bash` do not mean the same thing — or exist — on Windows:
//!
//! | Runtime    | Program resolved                          | Override        |
//! |------------|-------------------------------------------|-----------------|
//! | Python     | `python3` / `python` / `py`               | `PEARL_PYTHON`  |
//! | PowerShell | `pwsh` / `powershell`                     | `PEARL_PWSH`    |
//! | Shell      | `bash` / `sh`                             | `PEARL_BASH`    |
//! | Rust       | entrypoint path                           | —               |
//! | Native     | entrypoint path                           | —               |

use chrono::TimeDelta;
use pearl_core::Clock;
use pearl_governance::manifest::Runtime;
use pearl_process_supervisor::{CommandSpec, ExitStatus, ProcessSupervisor, SupervisorError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub mod agent_adapters;
pub mod programs;

pub use agent_adapters::{
    agent_adapter_for, ClaudeCodeAdapter, CodexAdapter, CursorAdapter, LlamaCppAdapter,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced by the runtime adapter layer.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The requested runtime is not supported by this adapter.
    #[error("unsupported runtime: {runtime}")]
    UnsupportedRuntime { runtime: String },

    /// Pre-execution validation failed.
    #[error("validation failed: {detail}")]
    Validation { detail: String },

    /// The underlying process supervisor reported an error.
    #[error("supervisor error: {0}")]
    Supervisor(#[from] SupervisorError),

    /// Failed to parse structured output from stdout.
    #[error("output parse error: {detail}")]
    OutputParse { detail: String },
}

// ---------------------------------------------------------------------------
// ScriptSpec
// ---------------------------------------------------------------------------

/// Specification for a script to execute.
///
/// This is the adapter-layer input: it names the runtime, the script entrypoint, and
/// everything the adapter needs to build a [`CommandSpec`] for the process supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptSpec {
    /// Which runtime to use.
    pub runtime: Runtime,
    /// Path to the script or binary entrypoint.
    pub entrypoint: PathBuf,
    /// Arguments passed to the script.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Working directory for execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Maximum execution time.
    pub timeout: TimeDelta,
    /// Optional JSON payload to pass as the `PEARL_INPUT` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_payload: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// RuntimeResult
// ---------------------------------------------------------------------------

/// The outcome of a script execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeResult {
    /// How the process exited.
    pub exit_status: RuntimeExitStatus,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Wall-clock duration of execution.
    pub duration: TimeDelta,
    /// Structured output parsed from the last JSON line of stdout, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<serde_json::Value>,
}

impl RuntimeResult {
    /// Whether the execution succeeded (zero exit code).
    pub fn is_success(&self) -> bool {
        self.exit_status.is_success()
    }
}

/// Serializable mirror of [`ExitStatus`] from the process supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RuntimeExitStatus {
    Exited { code: i32 },
    Signalled { signal: i32 },
    TimedOut,
    Cancelled,
}

impl RuntimeExitStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, RuntimeExitStatus::Exited { code: 0 })
    }
}

impl From<ExitStatus> for RuntimeExitStatus {
    fn from(es: ExitStatus) -> Self {
        match es {
            ExitStatus::Exited { code } => RuntimeExitStatus::Exited { code },
            ExitStatus::Signalled { signal } => RuntimeExitStatus::Signalled { signal },
            ExitStatus::TimedOut => RuntimeExitStatus::TimedOut,
            ExitStatus::Cancelled => RuntimeExitStatus::Cancelled,
        }
    }
}

// ---------------------------------------------------------------------------
// RuntimeAdapter trait
// ---------------------------------------------------------------------------

/// The runtime adapter contract.
///
/// Implementations wrap [`ProcessSupervisor`] to provide a higher-level execution
/// interface with structured output parsing and environment filtering.
pub trait RuntimeAdapter {
    /// Execute a script according to its spec, blocking until completion or timeout.
    fn execute(&self, spec: &ScriptSpec, clock: &dyn Clock) -> Result<RuntimeResult, RuntimeError>;

    /// Pre-validate a spec (e.g., check that the entrypoint exists, runtime is supported).
    fn validate(&self, spec: &ScriptSpec) -> Result<(), RuntimeError>;

    /// Whether this adapter can handle the given runtime.
    fn supports_runtime(&self, runtime: Runtime) -> bool;
}

// ---------------------------------------------------------------------------
// ScriptRuntimeAdapter
// ---------------------------------------------------------------------------

/// The concrete adapter that wraps a [`ProcessSupervisor`] to execute mechanical scripts.
///
/// Supports: Python, PowerShell, Shell, Rust (compiled binary), Native (any binary).
pub struct ScriptRuntimeAdapter<S: ProcessSupervisor> {
    supervisor: S,
}

impl<S: ProcessSupervisor> ScriptRuntimeAdapter<S> {
    /// Create a new adapter wrapping the given supervisor.
    pub fn new(supervisor: S) -> Self {
        Self { supervisor }
    }

    /// Resolve the program name and adjust args based on the runtime.
    fn resolve_command(spec: &ScriptSpec) -> Result<(String, Vec<String>), RuntimeError> {
        let (program, mut args) = match spec.runtime {
            Runtime::Python => {
                let program = programs::python();
                let mut args = vec![spec.entrypoint.to_string_lossy().to_string()];
                args.extend(spec.args.iter().cloned());
                (program, args)
            }
            Runtime::Powershell => {
                let program = programs::powershell();
                let mut args = vec![
                    "-NoProfile".to_string(),
                    "-File".to_string(),
                    spec.entrypoint.to_string_lossy().to_string(),
                ];
                args.extend(spec.args.iter().cloned());
                (program, args)
            }
            Runtime::Shell => {
                let program = programs::bash();
                let mut args = vec![spec.entrypoint.to_string_lossy().to_string()];
                args.extend(spec.args.iter().cloned());
                (program, args)
            }
            Runtime::Rust | Runtime::Native => {
                let program = spec.entrypoint.to_string_lossy().to_string();
                let args = spec.args.clone();
                (program, args)
            }
            _ => {
                return Err(RuntimeError::UnsupportedRuntime {
                    runtime: spec.runtime.as_str().to_string(),
                });
            }
        };
        let _ = &mut args; // suppress unused_mut if needed
        Ok((program, args))
    }

    /// Build a [`CommandSpec`] from a [`ScriptSpec`].
    pub fn build_command_spec(spec: &ScriptSpec) -> Result<CommandSpec, RuntimeError> {
        let (program, args) = Self::resolve_command(spec)?;
        let mut cmd = CommandSpec::new(program).args(args).timeout(spec.timeout);

        if let Some(cwd) = &spec.cwd {
            cmd = cmd.cwd(cwd);
        }

        // Apply environment filtering: only declared variables.
        for (k, v) in &spec.env {
            cmd = cmd.env(k, v);
        }

        // If an input payload is provided, serialize it and pass as PEARL_INPUT.
        if let Some(payload) = &spec.input_payload {
            let json_str =
                serde_json::to_string(payload).map_err(|e| RuntimeError::OutputParse {
                    detail: format!("failed to serialize input_payload: {e}"),
                })?;
            cmd = cmd.env("PEARL_INPUT", json_str);
        }

        Ok(cmd)
    }
}

impl<S: ProcessSupervisor> RuntimeAdapter for ScriptRuntimeAdapter<S> {
    fn execute(&self, spec: &ScriptSpec, clock: &dyn Clock) -> Result<RuntimeResult, RuntimeError> {
        let cmd = Self::build_command_spec(spec)?;
        let start = clock.now();
        // The same clock computes the deadline and enforces it, so a test clock behaves
        // predictably instead of depending on where real time happens to be.
        let mut proc = self.supervisor.spawn(&cmd, clock)?;
        let exit_status = self.supervisor.wait(&mut proc, clock)?;
        let duration = clock.now() - start;

        // Collect stdout/stderr from the child's captured pipes.
        let (stdout, stderr) = collect_output(&mut proc);

        // Attempt structured output parse from stdout.
        let structured_output = parse_structured_output(&stdout);

        self.supervisor.cleanup(&mut proc)?;

        Ok(RuntimeResult {
            exit_status: exit_status.into(),
            stdout,
            stderr,
            duration,
            structured_output,
        })
    }

    fn validate(&self, spec: &ScriptSpec) -> Result<(), RuntimeError> {
        if !self.supports_runtime(spec.runtime) {
            return Err(RuntimeError::UnsupportedRuntime {
                runtime: spec.runtime.as_str().to_string(),
            });
        }

        // For Rust/Native, the entrypoint must exist and be a file.
        match spec.runtime {
            Runtime::Rust | Runtime::Native => {
                if !spec.entrypoint.exists() {
                    return Err(RuntimeError::Validation {
                        detail: format!("entrypoint does not exist: {}", spec.entrypoint.display()),
                    });
                }
            }
            _ => {
                // For interpreted runtimes, the script file should exist.
                if !spec.entrypoint.exists() {
                    return Err(RuntimeError::Validation {
                        detail: format!(
                            "script file does not exist: {}",
                            spec.entrypoint.display()
                        ),
                    });
                }
            }
        }

        if spec.timeout <= TimeDelta::zero() {
            return Err(RuntimeError::Validation {
                detail: "timeout must be positive".to_string(),
            });
        }

        Ok(())
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        matches!(
            runtime,
            Runtime::Python
                | Runtime::Powershell
                | Runtime::Shell
                | Runtime::Rust
                | Runtime::Native
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Collect stdout/stderr from a supervised process.
///
/// The process supervisor pipes stdout/stderr. After wait() completes, we read them
/// via [`SupervisedProcess::take_output`]. The handles are consumed on first call, so
/// subsequent calls return empty strings.
fn collect_output(proc: &mut pearl_process_supervisor::SupervisedProcess) -> (String, String) {
    proc.take_output()
}

/// Parse structured output from stdout.
///
/// Convention: the last non-empty line of stdout may be a JSON object. If it parses as
/// valid JSON, it becomes the structured output. This allows scripts to emit human-readable
/// logging followed by a machine-readable result on the final line.
pub fn parse_structured_output(stdout: &str) -> Option<serde_json::Value> {
    let last_line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let trimmed = last_line.trim();
    // Only attempt parse if it looks like a JSON object or array.
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed).ok()
    } else {
        None
    }
}

/// Resolve the program name for a given runtime.
///
/// Exposed for testing and introspection.
pub fn resolve_program(runtime: Runtime, entrypoint: &std::path::Path) -> Option<String> {
    match runtime {
        Runtime::Python => Some(programs::python()),
        Runtime::Powershell => Some(programs::powershell()),
        Runtime::Shell => Some(programs::bash()),
        Runtime::Rust | Runtime::Native => Some(entrypoint.to_string_lossy().to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // --- ScriptSpec construction ---

    fn sample_spec(runtime: Runtime) -> ScriptSpec {
        ScriptSpec {
            runtime,
            entrypoint: PathBuf::from("/usr/local/bin/my-script.py"),
            args: vec!["--verbose".to_string(), "input.json".to_string()],
            env: BTreeMap::from([
                ("HOME".to_string(), "/home/test".to_string()),
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ]),
            cwd: Some(PathBuf::from("/workspace")),
            timeout: TimeDelta::try_seconds(60).unwrap(),
            input_payload: None,
        }
    }

    // --- Command resolution tests ---

    #[test]
    fn python_runtime_resolves_to_the_platform_interpreter() {
        let spec = sample_spec(Runtime::Python);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        // The exact program depends on the platform and what is installed, so the
        // invariant under test is that resolution is delegated rather than hard-coded.
        assert_eq!(cmd.program, programs::python());
        assert!(cmd.program.to_lowercase().contains("py"));
        assert_eq!(cmd.args[0], "/usr/local/bin/my-script.py");
        assert_eq!(cmd.args[1], "--verbose");
        assert_eq!(cmd.args[2], "input.json");
    }

    #[test]
    fn powershell_runtime_resolves_to_a_powershell_with_profile_disabled() {
        let spec = sample_spec(Runtime::Powershell);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.program, programs::powershell());
        // -NoProfile is not cosmetic: a user profile could change the working directory,
        // set aliases, or write to stdout and break the §26 machine-JSON contract.
        assert_eq!(cmd.args[0], "-NoProfile");
        assert_eq!(cmd.args[1], "-File");
        assert_eq!(cmd.args[2], "/usr/local/bin/my-script.py");
        assert_eq!(cmd.args[3], "--verbose");
    }

    #[test]
    fn shell_runtime_resolves_to_a_posix_shell() {
        let spec = sample_spec(Runtime::Shell);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.program, programs::bash());
        assert_eq!(cmd.args[0], "/usr/local/bin/my-script.py");
    }

    #[test]
    fn rust_runtime_uses_entrypoint_directly() {
        let spec = ScriptSpec {
            runtime: Runtime::Rust,
            entrypoint: PathBuf::from("/opt/bin/my-tool"),
            args: vec!["run".to_string()],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: None,
        };
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.program, "/opt/bin/my-tool");
        assert_eq!(cmd.args, vec!["run"]);
    }

    #[test]
    fn native_runtime_uses_entrypoint_directly() {
        let spec = ScriptSpec {
            runtime: Runtime::Native,
            entrypoint: PathBuf::from("/usr/local/bin/native-app"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.program, "/usr/local/bin/native-app");
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn unsupported_runtime_returns_error() {
        let spec = ScriptSpec {
            runtime: Runtime::ClaudeCode,
            entrypoint: PathBuf::from("/some/path"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let result = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RuntimeError::UnsupportedRuntime { .. }));
    }

    // --- Environment filtering ---

    #[test]
    fn env_variables_are_passed_through() {
        let spec = sample_spec(Runtime::Python);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.env.get("HOME").map(String::as_str), Some("/home/test"));
        assert_eq!(cmd.env.get("LANG").map(String::as_str), Some("en_US.UTF-8"));
    }

    #[test]
    fn input_payload_becomes_pearl_input_env() {
        let spec = ScriptSpec {
            runtime: Runtime::Python,
            entrypoint: PathBuf::from("/script.py"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: Some(serde_json::json!({"task_id": "abc-123", "score": 42})),
        };
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        let pearl_input = cmd.env.get("PEARL_INPUT").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(pearl_input).unwrap();
        assert_eq!(parsed["task_id"], "abc-123");
        assert_eq!(parsed["score"], 42);
    }

    // --- Timeout ---

    #[test]
    fn timeout_is_passed_to_command_spec() {
        let spec = sample_spec(Runtime::Shell);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.timeout.unwrap().num_seconds(), 60);
    }

    // --- CWD ---

    #[test]
    fn cwd_is_passed_through() {
        let spec = sample_spec(Runtime::Python);
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.cwd, Some(PathBuf::from("/workspace")));
    }

    #[test]
    fn cwd_none_is_none() {
        let spec = ScriptSpec {
            runtime: Runtime::Shell,
            entrypoint: PathBuf::from("/script.sh"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let cmd = ScriptRuntimeAdapter::<pearl_process_supervisor::PlatformSupervisor>::build_command_spec(&spec).unwrap();
        assert_eq!(cmd.cwd, None);
    }

    // --- Structured output parsing ---

    #[test]
    fn parses_json_from_last_stdout_line() {
        let stdout = "Starting task...\nProcessing...\n{\"status\": \"ok\", \"count\": 5}\n";
        let result = parse_structured_output(stdout);
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val["status"], "ok");
        assert_eq!(val["count"], 5);
    }

    #[test]
    fn parses_json_array_from_stdout() {
        let stdout = "log line 1\n[1, 2, 3]\n";
        let result = parse_structured_output(stdout);
        assert!(result.is_some());
        let val = result.unwrap();
        assert_eq!(val, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn non_json_stdout_returns_none() {
        let stdout = "just a plain log message\nanother line\n";
        let result = parse_structured_output(stdout);
        assert!(result.is_none());
    }

    #[test]
    fn empty_stdout_returns_none() {
        assert!(parse_structured_output("").is_none());
        assert!(parse_structured_output("   \n  \n").is_none());
    }

    #[test]
    fn invalid_json_returns_none() {
        let stdout = "{not valid json\n";
        let result = parse_structured_output(stdout);
        assert!(result.is_none());
    }

    #[test]
    fn skips_trailing_empty_lines() {
        let stdout = "{\"result\": true}\n\n\n";
        let result = parse_structured_output(stdout);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["result"], true);
    }

    // --- RuntimeExitStatus mapping ---

    #[test]
    fn exit_status_conversion() {
        assert_eq!(
            RuntimeExitStatus::from(ExitStatus::Exited { code: 0 }),
            RuntimeExitStatus::Exited { code: 0 }
        );
        assert_eq!(
            RuntimeExitStatus::from(ExitStatus::Exited { code: 1 }),
            RuntimeExitStatus::Exited { code: 1 }
        );
        assert_eq!(
            RuntimeExitStatus::from(ExitStatus::Signalled { signal: 9 }),
            RuntimeExitStatus::Signalled { signal: 9 }
        );
        assert_eq!(
            RuntimeExitStatus::from(ExitStatus::TimedOut),
            RuntimeExitStatus::TimedOut
        );
        assert_eq!(
            RuntimeExitStatus::from(ExitStatus::Cancelled),
            RuntimeExitStatus::Cancelled
        );
    }

    #[test]
    fn runtime_exit_status_success() {
        assert!(RuntimeExitStatus::Exited { code: 0 }.is_success());
        assert!(!RuntimeExitStatus::Exited { code: 1 }.is_success());
        assert!(!RuntimeExitStatus::TimedOut.is_success());
        assert!(!RuntimeExitStatus::Cancelled.is_success());
        assert!(!RuntimeExitStatus::Signalled { signal: 15 }.is_success());
    }

    // --- supports_runtime ---

    #[test]
    fn supports_mechanical_runtimes() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        assert!(adapter.supports_runtime(Runtime::Python));
        assert!(adapter.supports_runtime(Runtime::Powershell));
        assert!(adapter.supports_runtime(Runtime::Shell));
        assert!(adapter.supports_runtime(Runtime::Rust));
        assert!(adapter.supports_runtime(Runtime::Native));
    }

    #[test]
    fn does_not_support_llm_runtimes() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        assert!(!adapter.supports_runtime(Runtime::ClaudeCode));
        assert!(!adapter.supports_runtime(Runtime::Codex));
        assert!(!adapter.supports_runtime(Runtime::Cursor));
        assert!(!adapter.supports_runtime(Runtime::OpenaiCompatible));
        assert!(!adapter.supports_runtime(Runtime::LlamaCpp));
    }

    // --- resolve_program helper ---

    #[test]
    fn resolve_program_for_known_runtimes() {
        // Interpreted runtimes go through platform resolution; compiled ones are the
        // entrypoint itself.
        assert_eq!(
            resolve_program(Runtime::Python, &PathBuf::from("/x")),
            Some(programs::python())
        );
        assert_eq!(
            resolve_program(Runtime::Powershell, &PathBuf::from("/x")),
            Some(programs::powershell())
        );
        assert_eq!(
            resolve_program(Runtime::Shell, &PathBuf::from("/x")),
            Some(programs::bash())
        );
        assert_eq!(
            resolve_program(Runtime::Rust, &PathBuf::from("/my-bin")),
            Some("/my-bin".to_string())
        );
        assert_eq!(
            resolve_program(Runtime::Native, &PathBuf::from("/app")),
            Some("/app".to_string())
        );
    }

    #[test]
    fn resolve_program_returns_none_for_unsupported() {
        assert_eq!(
            resolve_program(Runtime::ClaudeCode, &PathBuf::from("/x")),
            None
        );
        assert_eq!(resolve_program(Runtime::Codex, &PathBuf::from("/x")), None);
    }

    // --- Validation ---

    #[test]
    fn validate_rejects_unsupported_runtime() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        let spec = ScriptSpec {
            runtime: Runtime::Codex,
            entrypoint: PathBuf::from("/some/path"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let result = adapter.validate(&spec);
        assert!(result.is_err());
    }

    #[test]
    fn validate_rejects_zero_timeout() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        let spec = ScriptSpec {
            runtime: Runtime::Python,
            entrypoint: PathBuf::from("/bin/true"), // exists on most systems
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::zero(),
            input_payload: None,
        };
        let result = adapter.validate(&spec);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RuntimeError::Validation { .. }));
    }

    #[test]
    fn validate_rejects_missing_entrypoint() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        let spec = ScriptSpec {
            runtime: Runtime::Python,
            entrypoint: PathBuf::from("/nonexistent/path/to/script.py"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let result = adapter.validate(&spec);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, RuntimeError::Validation { .. }));
    }

    #[test]
    fn validate_accepts_existing_entrypoint() {
        let adapter =
            ScriptRuntimeAdapter::new(pearl_process_supervisor::PlatformSupervisor::default());
        // A file the test creates, rather than a platform-specific one like /bin/true:
        // validation checks existence, and the assertion should not depend on the OS.
        let dir = tempfile::tempdir().unwrap();
        let entrypoint = dir.path().join("tool");
        std::fs::write(&entrypoint, b"").unwrap();
        let spec = ScriptSpec {
            runtime: Runtime::Native,
            entrypoint,
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(10).unwrap(),
            input_payload: None,
        };
        let result = adapter.validate(&spec);
        assert!(result.is_ok());
    }

    // --- RuntimeResult ---

    #[test]
    fn runtime_result_success_check() {
        let result = RuntimeResult {
            exit_status: RuntimeExitStatus::Exited { code: 0 },
            stdout: String::new(),
            stderr: String::new(),
            duration: TimeDelta::try_seconds(1).unwrap(),
            structured_output: None,
        };
        assert!(result.is_success());

        let failed = RuntimeResult {
            exit_status: RuntimeExitStatus::Exited { code: 1 },
            stdout: String::new(),
            stderr: String::new(),
            duration: TimeDelta::try_seconds(1).unwrap(),
            structured_output: None,
        };
        assert!(!failed.is_success());
    }
}
