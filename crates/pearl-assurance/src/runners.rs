//! Check runners that actually perform verification — §32, Articles 2, 4 and 8.
//!
//! [`crate::AssuranceEngine`] orchestrates checks but does not interpret them; this module
//! is what makes a [`CheckKind`] mean something. Without it the engine could report
//! "3/3 checks passed" while never having validated a schema or run a verifier, which is
//! the exact self-certification Article 8 forbids — just laundered through a struct.
//!
//! Three kinds, three real mechanisms:
//!
//! | Kind               | Mechanism                                    | Verdict from        |
//! |--------------------|----------------------------------------------|---------------------|
//! | `SchemaValidation` | JSON Schema draft 2020-12, local files only  | validation errors   |
//! | `ScriptVerifier`   | spawn under the process supervisor           | exit code           |
//! | `TestCommand`      | spawn under the process supervisor           | exit code           |
//!
//! Every mechanism can also *fail to run*, which produces [`CheckOutcome::Errored`] rather
//! than a verdict.

use std::path::{Path, PathBuf};

use chrono::TimeDelta;
use pearl_core::Clock;
use pearl_governance::manifest::Runtime;
use pearl_process_supervisor::ProcessSupervisor;
use pearl_runtime::{RuntimeAdapter, ScriptRuntimeAdapter, ScriptSpec};

use crate::engine::{AssuranceCheck, CheckKind, CheckOutcome};

/// Default ceiling for a single check.
///
/// A verifier with no deadline cannot be cancelled on one (Article 9), and a hung verifier
/// would hold the task in `VERIFYING` indefinitely.
pub const DEFAULT_CHECK_TIMEOUT_SECONDS: i64 = 120;

/// Everything a check needs in order to be performed.
///
/// Two documents, not one, because schema checks and verifier scripts are asking different
/// questions. A schema describes the shape of the *capability's output*, so validating an
/// envelope around it would always fail on the envelope's own keys. A verifier needs the
/// context — which capability ran, what it exited with — so handing it the bare output would
/// hide the very facts it exists to check.
#[derive(Debug, Clone)]
pub struct CheckContext {
    /// The capability's own output. What `SchemaValidation` validates.
    pub subject: serde_json::Value,
    /// The envelope handed to verifier scripts on `PEARL_INPUT`.
    ///
    /// Defaults to `subject` when not set, so a caller with nothing to add need not build one.
    pub verifier_input: Option<serde_json::Value>,
    /// Directory holding the JSON Schema files named by `SchemaValidation` checks.
    pub schema_dir: PathBuf,
    /// Working directory for spawned verifiers and test commands.
    pub working_dir: Option<PathBuf>,
    /// Per-check ceiling.
    pub timeout: TimeDelta,
}

impl CheckContext {
    /// A context for verifying `subject`, with schemas resolved under `schema_dir`.
    pub fn new(subject: serde_json::Value, schema_dir: impl Into<PathBuf>) -> Self {
        Self {
            subject,
            verifier_input: None,
            schema_dir: schema_dir.into(),
            working_dir: None,
            timeout: TimeDelta::try_seconds(DEFAULT_CHECK_TIMEOUT_SECONDS).expect("valid"),
        }
    }

    /// Sets the envelope verifier scripts receive.
    pub fn with_verifier_input(mut self, input: serde_json::Value) -> Self {
        self.verifier_input = Some(input);
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn with_timeout(mut self, timeout: TimeDelta) -> Self {
        self.timeout = timeout;
        self
    }

    /// The envelope a verifier receives, with a check's own parameters merged in.
    ///
    /// Check parameters win over envelope keys except for `result`, which is what actually
    /// ran and therefore is not the check's to redefine.
    fn input_for(&self, check: &AssuranceCheck) -> serde_json::Value {
        let base = self
            .verifier_input
            .clone()
            .unwrap_or_else(|| self.subject.clone());

        let Some(extra) = check.input.as_ref().and_then(|v| v.as_object()) else {
            return base;
        };
        let mut merged = match base {
            serde_json::Value::Object(map) => map,
            other => {
                // A non-object envelope cannot be merged into, so preserve it under `result`
                // rather than discarding either side.
                let mut map = serde_json::Map::new();
                map.insert("result".to_string(), other);
                map
            }
        };
        let authoritative = merged.get("result").cloned();
        for (key, value) in extra {
            merged.insert(key.clone(), value.clone());
        }
        if let Some(result) = authoritative {
            merged.insert("result".to_string(), result);
        }
        serde_json::Value::Object(merged)
    }
}

/// Performs assurance checks for real.
pub struct RuntimeCheckRunner<S: ProcessSupervisor, C: Clock> {
    adapter: ScriptRuntimeAdapter<S>,
    clock: C,
    context: CheckContext,
}

impl<S: ProcessSupervisor, C: Clock> RuntimeCheckRunner<S, C> {
    pub fn new(supervisor: S, clock: C, context: CheckContext) -> Self {
        Self {
            adapter: ScriptRuntimeAdapter::new(supervisor),
            clock,
            context,
        }
    }

    /// The document being verified.
    pub fn subject(&self) -> &serde_json::Value {
        &self.context.subject
    }

    /// Runs one check.
    pub fn run(&self, check: &AssuranceCheck) -> CheckOutcome {
        match &check.kind {
            CheckKind::SchemaValidation { schema } => self.validate_schema(schema),
            CheckKind::ScriptVerifier { script_path } => {
                self.run_verifier(Path::new(script_path), self.context.input_for(check))
            }
            CheckKind::TestCommand { command } => self.run_test_command(command),
        }
    }

    /// Validates the subject against a named JSON Schema.
    fn validate_schema(&self, schema_name: &str) -> CheckOutcome {
        let path = resolve_schema_path(&self.context.schema_dir, schema_name);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                return CheckOutcome::Errored {
                    reason: format!(
                        "schema '{schema_name}' not readable at {}: {e}",
                        path.display()
                    ),
                }
            }
        };
        let schema: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                return CheckOutcome::Errored {
                    reason: format!("schema '{schema_name}' is not valid JSON: {e}"),
                }
            }
        };
        let validator = match build_validator(&schema, &self.context.schema_dir) {
            Ok(v) => v,
            Err(e) => {
                return CheckOutcome::Errored {
                    reason: format!("schema '{schema_name}' is not a usable JSON Schema: {e}"),
                }
            }
        };

        let errors: Vec<String> = validator
            .iter_errors(&self.context.subject)
            .map(|e| format!("{} at {}", e, e.instance_path()))
            .collect();

        if errors.is_empty() {
            CheckOutcome::Passed
        } else {
            // Bounded: a mismatched array can produce thousands of errors, and a reason
            // field that large is unreadable in a ledger and unhelpful in a CLI.
            let shown = errors.len().min(5);
            CheckOutcome::Failed {
                reason: format!(
                    "{} schema violation(s) against '{schema_name}': {}{}",
                    errors.len(),
                    errors[..shown].join("; "),
                    if errors.len() > shown { " ..." } else { "" }
                ),
            }
        }
    }

    /// Runs a verifier script, passing the envelope on `PEARL_INPUT`.
    ///
    /// Exit codes follow the Script I/O Contract (§26): `0` verified, `1` rejected, `2` could
    /// not decide. The third is the one that matters — a verifier handed input it cannot
    /// interpret has produced no verdict, and treating that as a rejection would make a
    /// broken pipeline look like failing work.
    fn run_verifier(&self, script: &Path, input: serde_json::Value) -> CheckOutcome {
        if !script.exists() {
            return CheckOutcome::Errored {
                reason: format!("verifier script does not exist: {}", script.display()),
            };
        }
        let Some(runtime) = runtime_for(script) else {
            return CheckOutcome::Errored {
                reason: format!(
                    "cannot tell which runtime should execute {}; expected .py, .ps1, .sh or an executable",
                    script.display()
                ),
            };
        };

        let spec = ScriptSpec {
            runtime,
            entrypoint: script.to_path_buf(),
            args: Vec::new(),
            env: Default::default(),
            cwd: self.context.working_dir.clone(),
            timeout: self.context.timeout,
            input_payload: Some(input),
        };

        let subject = script.display().to_string();
        match self.adapter.execute(&spec, &self.clock) {
            Ok(result) if result.is_success() && !declares_error(&result) => CheckOutcome::Passed,
            Ok(result) if declares_error(&result) => CheckOutcome::Errored {
                reason: describe_failure(subject, &result),
            },
            Ok(result) => CheckOutcome::Failed {
                reason: describe_failure(subject, &result),
            },
            // The verifier could not be run at all, so nothing was verified. Article 8:
            // this must not be laundered into a pass, and Article 2 says it must not be
            // laundered into "verified failure" either.
            Err(e) => CheckOutcome::Errored {
                reason: format!("verifier {subject} could not run: {e}"),
            },
        }
    }

    /// Runs a test command, e.g. `cargo test -p pearl-core`.
    fn run_test_command(&self, command: &str) -> CheckOutcome {
        let mut parts = match shell_split(command) {
            Ok(p) => p,
            Err(reason) => return CheckOutcome::Errored { reason },
        };
        if parts.is_empty() {
            return CheckOutcome::Errored {
                reason: "test command is empty".to_string(),
            };
        }
        let program = parts.remove(0);

        let spec = ScriptSpec {
            // Native: the command names its own program, so there is no interpreter to
            // insert. Going through a shell would also reintroduce the compound-command
            // hole that the guard exists to close.
            runtime: Runtime::Native,
            entrypoint: PathBuf::from(&program),
            args: parts,
            env: Default::default(),
            cwd: self.context.working_dir.clone(),
            timeout: self.context.timeout,
            input_payload: None,
        };

        match self.adapter.execute(&spec, &self.clock) {
            Ok(result) if result.is_success() => CheckOutcome::Passed,
            Ok(result) => CheckOutcome::Failed {
                reason: describe_failure(command.to_string(), &result),
            },
            Err(e) => CheckOutcome::Errored {
                reason: format!("test command '{command}' could not run: {e}"),
            },
        }
    }
}

/// Builds the closure [`crate::AssuranceEngine`] expects.
pub fn runner_fn<S, C>(runner: RuntimeCheckRunner<S, C>) -> crate::engine::CheckRunner
where
    S: ProcessSupervisor + Send + Sync + 'static,
    C: Clock + Send + Sync + 'static,
{
    Box::new(move |check| runner.run(check))
}

/// The synthetic base every schema reference is resolved against.
///
/// Not `file://`: a real filesystem base would let a `$ref` walk out of the schema
/// directory, and not a URL scheme that could ever be fetched.
const SCHEMA_BASE_URI: &str = "pearl:///schemas/";

/// Resolves schema references to files in one directory, and nowhere else.
///
/// PEARL's schemas cross-reference each other (`verification-result-v1` uses
/// `evidence-v1`), so references must resolve. They must also resolve *hermetically*: a
/// verification result that depended on fetching a remote schema would depend on the
/// network, which is not a property a verdict is allowed to have.
struct LocalSchemaRetriever {
    schema_dir: PathBuf,
}

impl jsonschema::Retrieve for LocalSchemaRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        // Only the final segment is honoured, so `../../etc/passwd` cannot escape.
        let name = uri
            .as_str()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        if name.is_empty() {
            return Err(format!("schema reference '{uri}' names no document").into());
        }
        let path = self.schema_dir.join(&name);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("schema '{name}' not readable at {}: {e}", path.display()))?;
        Ok(serde_json::from_str(&text)
            .map_err(|e| format!("schema '{name}' is not valid JSON: {e}"))?)
    }
}

/// Builds a validator whose references resolve inside `schema_dir` only.
fn build_validator(
    schema: &serde_json::Value,
    schema_dir: &Path,
) -> Result<jsonschema::Validator, String> {
    jsonschema::options()
        .with_base_uri(SCHEMA_BASE_URI)
        .with_retriever(LocalSchemaRetriever {
            schema_dir: schema_dir.to_path_buf(),
        })
        .build(schema)
        .map_err(|e| e.to_string())
}

/// Maps a schema name to a file.
///
/// Accepts `verification-result-v1`, `verification-result-v1.json`, or a path. The bare
/// name is the form manifests use, and requiring the extension there would put a filesystem
/// detail into a capability declaration.
fn resolve_schema_path(schema_dir: &Path, schema_name: &str) -> PathBuf {
    let candidate = Path::new(schema_name);
    if candidate.is_absolute()
        || candidate
            .parent()
            .is_some_and(|p| !p.as_os_str().is_empty())
    {
        return candidate.to_path_buf();
    }
    if candidate.extension().is_some() {
        return schema_dir.join(candidate);
    }
    schema_dir.join(format!("{schema_name}.json"))
}

/// Infers the runtime from a script's extension.
fn runtime_for(script: &Path) -> Option<Runtime> {
    match script
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("py") => Some(Runtime::Python),
        Some("ps1") => Some(Runtime::Powershell),
        Some("sh" | "bash") => Some(Runtime::Shell),
        Some("exe") | None => Some(Runtime::Native),
        _ => None,
    }
}

/// Exit code a script uses to say "I could not decide" (§26).
const EXIT_CANNOT_DECIDE: i32 = 2;

/// Whether a verifier reported that it reached no verdict.
///
/// Two signals, either of which is enough: the reserved exit code, or `status: "error"` in
/// the verification-result document. Both exist because a script may fail to produce JSON at
/// all, and a script may produce JSON while exiting non-zero.
fn declares_error(result: &pearl_runtime::RuntimeResult) -> bool {
    if matches!(
        result.exit_status,
        pearl_runtime::RuntimeExitStatus::Exited {
            code: EXIT_CANNOT_DECIDE
        }
    ) {
        return true;
    }
    result
        .structured_output
        .as_ref()
        .and_then(|v| v.get("status"))
        .and_then(|s| s.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("error"))
}

/// Summarises why an execution counted as a failure.
///
/// Prefers the machine JSON a script emitted, then stderr, then the bare exit status:
/// a verifier that explained itself should have that explanation preserved.
fn describe_failure(subject: String, result: &pearl_runtime::RuntimeResult) -> String {
    let detail = result
        .structured_output
        .as_ref()
        .map(|v| v.to_string())
        .or_else(|| {
            let stderr = result.stderr.trim();
            (!stderr.is_empty()).then(|| truncate(stderr, 400))
        })
        .unwrap_or_else(|| "no output".to_string());

    format!(
        "{subject} reported failure ({:?}): {detail}",
        result.exit_status
    )
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

/// Splits a command line into program and arguments, honouring double quotes.
///
/// Deliberately not a shell: no globbing, no variable expansion, no `&&`. A test command
/// that needs those should be a script, which is inspectable and can be guarded.
fn shell_split(command: &str) -> Result<Vec<String>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut any = false;

    for ch in command.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                any = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if any {
                    parts.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            c => {
                current.push(c);
                any = true;
            }
        }
    }
    if in_quotes {
        return Err(format!("unbalanced quote in test command: {command}"));
    }
    if any {
        parts.push(current);
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AssuranceCheck;
    use pearl_core::SystemClock;
    use pearl_process_supervisor::PlatformSupervisor;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn runner(subject: serde_json::Value) -> RuntimeCheckRunner<PlatformSupervisor, SystemClock> {
        RuntimeCheckRunner::new(
            PlatformSupervisor::default(),
            SystemClock,
            CheckContext::new(subject, workspace_root().join("schemas"))
                .with_timeout(TimeDelta::try_seconds(60).unwrap()),
        )
    }

    fn schema_check(schema: &str) -> AssuranceCheck {
        AssuranceCheck {
            name: format!("schema:{schema}"),
            kind: CheckKind::SchemaValidation {
                schema: schema.to_string(),
            },
            evidence_required: true,
            input: None,
        }
    }

    fn python_available() -> bool {
        pearl_runtime::programs::is_available(&pearl_runtime::programs::python())
    }

    // --- schema validation ---

    #[test]
    fn a_conforming_document_passes_the_real_schema() {
        let subject = serde_json::json!({
            "status": "pass",
            "verifier": "verifier.task-result",
            "checks": [{ "id": "schema", "status": "pass" }],
            "duration_ms": 12
        });
        let outcome = runner(subject).run(&schema_check("verification-result-v1"));
        assert_eq!(outcome, CheckOutcome::Passed, "got {outcome:?}");
    }

    #[test]
    fn a_cross_referenced_schema_resolves_without_the_network() {
        // verification-result-v1 $refs evidence-v1. If references did not resolve locally,
        // this document could never be validated at all.
        let subject = serde_json::json!({
            "status": "pass",
            "checks": [{ "id": "c", "status": "pass" }],
            "evidence": [{
                "type": "test",
                "producer": "pytest",
                "timestamp": "2026-08-15T00:00:00Z",
                "result": "pass"
            }]
        });
        let outcome = runner(subject).run(&schema_check("verification-result-v1"));
        assert_eq!(outcome, CheckOutcome::Passed, "got {outcome:?}");
    }

    #[test]
    fn a_violating_document_fails_with_the_violation_named() {
        // `status` is constrained by the schema, so an invented value must be caught.
        let subject = serde_json::json!({
            "status": "probably-fine",
            "checks": []
        });
        let outcome = runner(subject).run(&schema_check("verification-result-v1"));
        match outcome {
            CheckOutcome::Failed { reason } => {
                assert!(reason.contains("schema violation"), "got: {reason}");
            }
            other => panic!("expected a failure verdict, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_schema_is_an_error_not_a_failure() {
        // The difference matters: nothing was verified, so this must not read as a
        // verified failure.
        let outcome = runner(serde_json::json!({})).run(&schema_check("no-such-schema-v9"));
        assert!(
            matches!(outcome, CheckOutcome::Errored { .. }),
            "got {outcome:?}"
        );
        assert!(!outcome.is_verdict());
    }

    #[test]
    fn schema_names_resolve_with_or_without_the_extension() {
        let dir = Path::new("/schemas");
        assert_eq!(
            resolve_schema_path(dir, "evidence-v1"),
            dir.join("evidence-v1.json")
        );
        assert_eq!(
            resolve_schema_path(dir, "evidence-v1.json"),
            dir.join("evidence-v1.json")
        );
    }

    // --- script verifiers ---

    #[test]
    fn the_shipped_verifier_passes_a_good_result() {
        if !python_available() {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let script = workspace_root().join("capabilities/verifiers/verify_task_result.py");
        let subject = serde_json::json!({
            "result": { "status": "success", "items": [1, 2, 3] },
            "require_keys": ["status", "items"],
            "non_empty": ["items"],
            "expect": { "status": "success" }
        });
        let outcome = runner(subject).run(&AssuranceCheck {
            name: "verifier.task-result".into(),
            kind: CheckKind::ScriptVerifier {
                script_path: script.to_string_lossy().to_string(),
            },
            evidence_required: true,
            input: None,
        });
        assert_eq!(outcome, CheckOutcome::Passed, "got {outcome:?}");
    }

    #[test]
    fn the_shipped_verifier_rejects_a_result_that_claims_its_own_failure() {
        if !python_available() {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let script = workspace_root().join("capabilities/verifiers/verify_task_result.py");
        let subject = serde_json::json!({
            "result": { "status": "failed" },
            "require_keys": ["status"]
        });
        let outcome = runner(subject).run(&AssuranceCheck {
            name: "verifier.task-result".into(),
            kind: CheckKind::ScriptVerifier {
                script_path: script.to_string_lossy().to_string(),
            },
            evidence_required: true,
            input: None,
        });
        match outcome {
            CheckOutcome::Failed { reason } => {
                assert!(
                    reason.contains("verifier.task-result") || reason.contains("status"),
                    "the verifier's own explanation should survive: {reason}"
                );
            }
            other => panic!("expected a failure verdict, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_verifier_script_is_an_error() {
        let outcome = runner(serde_json::json!({})).run(&AssuranceCheck {
            name: "absent".into(),
            kind: CheckKind::ScriptVerifier {
                script_path: "/definitely/not/here/verify.py".into(),
            },
            evidence_required: true,
            input: None,
        });
        assert!(
            matches!(outcome, CheckOutcome::Errored { .. }),
            "got {outcome:?}"
        );
    }

    // --- test commands ---

    #[test]
    fn a_passing_command_passes() {
        let outcome = runner(serde_json::json!({})).run(&AssuranceCheck {
            name: "toolchain".into(),
            // cargo is guaranteed present: these tests run under it.
            kind: CheckKind::TestCommand {
                command: "cargo --version".into(),
            },
            evidence_required: false,
            input: None,
        });
        assert_eq!(outcome, CheckOutcome::Passed, "got {outcome:?}");
    }

    #[test]
    fn a_failing_command_is_a_verdict() {
        let outcome = runner(serde_json::json!({})).run(&AssuranceCheck {
            name: "bad-subcommand".into(),
            kind: CheckKind::TestCommand {
                command: "cargo definitely-not-a-subcommand".into(),
            },
            evidence_required: false,
            input: None,
        });
        assert!(
            matches!(outcome, CheckOutcome::Failed { .. }),
            "got {outcome:?}"
        );
        assert!(outcome.is_verdict());
    }

    #[test]
    fn a_command_that_cannot_start_is_an_error() {
        let outcome = runner(serde_json::json!({})).run(&AssuranceCheck {
            name: "absent-program".into(),
            kind: CheckKind::TestCommand {
                command: "pearl-no-such-program-xyz --run".into(),
            },
            evidence_required: false,
            input: None,
        });
        assert!(
            matches!(outcome, CheckOutcome::Errored { .. }),
            "got {outcome:?}"
        );
    }

    #[test]
    fn command_splitting_keeps_quoted_arguments_together() {
        assert_eq!(
            shell_split(r#"cargo test --test "my integration test""#).unwrap(),
            vec!["cargo", "test", "--test", "my integration test"]
        );
        assert_eq!(shell_split("   ").unwrap(), Vec::<String>::new());
        assert!(shell_split(r#"cargo "unbalanced"#).is_err());
    }

    #[test]
    fn runtime_inference_covers_the_script_extensions() {
        assert_eq!(runtime_for(Path::new("v.py")), Some(Runtime::Python));
        assert_eq!(runtime_for(Path::new("v.ps1")), Some(Runtime::Powershell));
        assert_eq!(runtime_for(Path::new("v.sh")), Some(Runtime::Shell));
        assert_eq!(runtime_for(Path::new("verify")), Some(Runtime::Native));
        assert_eq!(runtime_for(Path::new("v.rb")), None);
    }
}
