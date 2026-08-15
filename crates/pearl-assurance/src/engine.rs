//! Assurance engine: runs verification checks after execution.

use serde::{Deserialize, Serialize};

/// The kind of assurance check to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckKind {
    /// Validates output against a declared schema name.
    SchemaValidation { schema: String },
    /// Runs a verification script by path.
    ScriptVerifier { script_path: String },
    /// Executes a test command.
    TestCommand { command: String },
}

/// A single assurance check specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssuranceCheck {
    /// Human-readable name for this check.
    pub name: String,
    /// The kind of check to run.
    pub kind: CheckKind,
    /// Whether evidence must be recorded for this check.
    pub evidence_required: bool,
    /// Extra parameters merged into the verifier's input.
    ///
    /// This is what lets one general verifier be reused: `verifier.task-result` can be told
    /// which keys to require rather than needing a bespoke script per task. Ignored by
    /// schema checks, which have nothing to parameterise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

impl AssuranceCheck {
    /// A check with no extra parameters.
    pub fn new(name: impl Into<String>, kind: CheckKind, evidence_required: bool) -> Self {
        Self {
            name: name.into(),
            kind,
            evidence_required,
            input: None,
        }
    }

    /// Attaches verifier parameters.
    pub fn with_input(mut self, input: serde_json::Value) -> Self {
        self.input = Some(input);
        self
    }
}

/// A collection of assurance checks to run for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssuranceSpec {
    /// The checks to run.
    pub checks: Vec<AssuranceCheck>,
}

impl AssuranceSpec {
    /// Creates a new assurance spec with the given checks.
    pub fn new(checks: Vec<AssuranceCheck>) -> Self {
        Self { checks }
    }

    /// Creates an empty spec (no checks required).
    pub fn empty() -> Self {
        Self { checks: Vec::new() }
    }
}

/// Outcome of a single check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckOutcome {
    /// The check ran and the property held.
    Passed,
    /// The check ran and the property did not hold. This is a verdict.
    Failed { reason: String },
    /// The check could not run, so there is no verdict.
    ///
    /// Distinct from `Failed` because Article 2 treats them differently: a failure is
    /// information, whereas the absence of a verdict means nothing has been verified and
    /// success must not be claimed. Collapsing the two would let a broken verifier read as
    /// a legitimately failing one — or worse, be retried until it "passed".
    Errored { reason: String },
}

impl CheckOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, CheckOutcome::Passed)
    }

    /// Whether a verdict was actually reached, either way.
    pub fn is_verdict(&self) -> bool {
        !matches!(self, CheckOutcome::Errored { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            CheckOutcome::Passed => None,
            CheckOutcome::Failed { reason } | CheckOutcome::Errored { reason } => Some(reason),
        }
    }
}

/// Detail about one check's execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDetail {
    /// The name of the check that was run.
    pub name: String,
    /// What kind of check it was.
    ///
    /// Carried through so the caller can classify the resulting evidence (§52) without
    /// re-deriving it by zipping against the spec, which would break silently if the
    /// engine ever reordered or skipped a check.
    pub kind: CheckKind,
    /// The outcome of running this check.
    pub outcome: CheckOutcome,
    /// Whether this check produced a machine artifact to point at.
    pub evidence_provided: bool,
}

/// The overall assurance result after running all checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssuranceResult {
    /// Whether ALL checks passed and all required evidence was provided.
    pub passed: bool,
    /// Details of each individual check.
    pub details: Vec<CheckDetail>,
}

impl AssuranceResult {
    /// Returns the number of passed checks.
    pub fn passed_count(&self) -> usize {
        self.details.iter().filter(|d| d.outcome.passed()).count()
    }

    /// Returns the number of failed checks.
    pub fn failed_count(&self) -> usize {
        self.details
            .iter()
            .filter(|d| matches!(d.outcome, CheckOutcome::Failed { .. }))
            .count()
    }

    /// Returns the number of checks that could not run.
    pub fn errored_count(&self) -> usize {
        self.details
            .iter()
            .filter(|d| matches!(d.outcome, CheckOutcome::Errored { .. }))
            .count()
    }

    /// Whether any check was actually performed.
    ///
    /// An empty result is not a pass. A caller that treated it as one would be claiming
    /// verification it never attempted.
    pub fn any_verdict(&self) -> bool {
        self.details.iter().any(|d| d.outcome.is_verdict())
    }

    /// A one-line summary for a state-transition reason.
    pub fn summary(&self) -> String {
        format!(
            "{}/{} checks passed, {} failed, {} could not run",
            self.passed_count(),
            self.details.len(),
            self.failed_count(),
            self.errored_count()
        )
    }

    /// The first reason a check gave for not passing.
    pub fn first_problem(&self) -> Option<String> {
        self.details
            .iter()
            .find(|d| !d.outcome.passed())
            .and_then(|d| d.outcome.reason().map(|r| format!("{}: {r}", d.name)))
    }
}

/// Errors from the assurance engine.
#[derive(Debug, thiserror::Error)]
pub enum AssuranceError {
    /// A check could not be run (infrastructure failure, not a check failure).
    #[error("failed to run check '{name}': {detail}")]
    CheckExecutionFailed { name: String, detail: String },
}

/// A check runner function: given a check, returns the outcome.
///
/// This trait allows injection of different verification backends (real scripts,
/// mocks for testing, etc.).
pub type CheckRunner = Box<dyn Fn(&AssuranceCheck) -> CheckOutcome + Send + Sync>;

/// The Assurance Engine orchestrates running all checks in an AssuranceSpec.
pub struct AssuranceEngine {
    runner: CheckRunner,
}

impl AssuranceEngine {
    /// Creates a new engine with the given check runner.
    pub fn new(runner: CheckRunner) -> Self {
        Self { runner }
    }

    /// Creates an engine that passes all checks (for testing).
    pub fn always_pass() -> Self {
        Self {
            runner: Box::new(|_| CheckOutcome::Passed),
        }
    }

    /// Creates an engine that fails all checks (for testing).
    pub fn always_fail(reason: &str) -> Self {
        let reason = reason.to_string();
        Self {
            runner: Box::new(move |_| CheckOutcome::Failed {
                reason: reason.clone(),
            }),
        }
    }

    /// Runs all checks in the spec and returns the overall result.
    ///
    /// A task's completion requires ALL checks to pass. If any check fails
    /// or required evidence is missing, the overall result is `passed: false`.
    pub fn run(&self, spec: &AssuranceSpec) -> AssuranceResult {
        let mut details = Vec::new();
        let mut all_passed = true;

        for check in &spec.checks {
            let outcome = (self.runner)(check);
            // Evidence exists when the check actually ran, whatever it concluded. A check
            // that could not run has nothing to point at, which is precisely why it cannot
            // discharge an evidence requirement.
            let evidence_provided = outcome.is_verdict();

            if !outcome.passed() {
                all_passed = false;
            }
            if check.evidence_required && !evidence_provided {
                all_passed = false;
            }

            details.push(CheckDetail {
                name: check.name.clone(),
                kind: check.kind.clone(),
                outcome,
                evidence_provided,
            });
        }

        AssuranceResult {
            passed: all_passed,
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_check(name: &str) -> AssuranceCheck {
        AssuranceCheck {
            name: name.to_string(),
            kind: CheckKind::SchemaValidation {
                schema: "output-v1".to_string(),
            },
            evidence_required: false,
            input: None,
        }
    }

    fn script_check(name: &str, evidence_required: bool) -> AssuranceCheck {
        AssuranceCheck {
            name: name.to_string(),
            kind: CheckKind::ScriptVerifier {
                script_path: "/checks/verify.sh".to_string(),
            },
            evidence_required,
            input: None,
        }
    }

    fn test_cmd_check(name: &str) -> AssuranceCheck {
        AssuranceCheck {
            name: name.to_string(),
            kind: CheckKind::TestCommand {
                command: "cargo test".to_string(),
            },
            evidence_required: true,
            input: None,
        }
    }

    #[test]
    fn all_pass_yields_success() {
        let engine = AssuranceEngine::always_pass();
        let spec = AssuranceSpec::new(vec![
            schema_check("schema"),
            script_check("script", false),
            test_cmd_check("tests"),
        ]);
        let result = engine.run(&spec);
        assert!(result.passed);
        assert_eq!(result.passed_count(), 3);
        assert_eq!(result.failed_count(), 0);
    }

    #[test]
    fn any_failure_blocks_success() {
        // Engine that fails only the second check.
        let engine = AssuranceEngine::new(Box::new(|check| {
            if check.name == "bad" {
                CheckOutcome::Failed {
                    reason: "validation failed".to_string(),
                }
            } else {
                CheckOutcome::Passed
            }
        }));
        let spec = AssuranceSpec::new(vec![schema_check("good"), schema_check("bad")]);
        let result = engine.run(&spec);
        assert!(!result.passed);
        assert_eq!(result.passed_count(), 1);
        assert_eq!(result.failed_count(), 1);
    }

    #[test]
    fn all_fail_yields_failure() {
        let engine = AssuranceEngine::always_fail("broken");
        let spec = AssuranceSpec::new(vec![schema_check("a"), schema_check("b")]);
        let result = engine.run(&spec);
        assert!(!result.passed);
        assert_eq!(result.failed_count(), 2);
    }

    #[test]
    fn empty_spec_passes() {
        let engine = AssuranceEngine::always_pass();
        let spec = AssuranceSpec::empty();
        let result = engine.run(&spec);
        assert!(result.passed);
        assert_eq!(result.details.len(), 0);
    }

    #[test]
    fn evidence_required_flag_tracked() {
        let engine = AssuranceEngine::always_pass();
        let spec = AssuranceSpec::new(vec![script_check("with-evidence", true)]);
        let result = engine.run(&spec);
        assert!(result.passed);
        assert!(result.details[0].evidence_provided);
    }

    #[test]
    fn a_failing_check_still_produced_evidence() {
        // A check that ran and said no has something to point at: its own output. What
        // does *not* produce evidence is a check that never ran at all.
        let engine = AssuranceEngine::always_fail("the property does not hold");
        let spec = AssuranceSpec::new(vec![script_check("needs-evidence", true)]);
        let result = engine.run(&spec);
        assert!(!result.passed);
        assert!(result.details[0].evidence_provided);
        assert!(result.any_verdict());
    }

    #[test]
    fn a_check_that_could_not_run_provides_no_evidence_and_no_verdict() {
        let engine = AssuranceEngine::new(Box::new(|_| CheckOutcome::Errored {
            reason: "verifier binary is missing".into(),
        }));
        let spec = AssuranceSpec::new(vec![script_check("needs-evidence", true)]);
        let result = engine.run(&spec);

        assert!(!result.passed, "no verdict cannot be a pass");
        assert!(!result.details[0].evidence_provided);
        assert!(!result.any_verdict());
        assert_eq!(result.errored_count(), 1);
        // The distinction has to survive into the summary an operator reads.
        assert!(
            result.summary().contains("1 could not run"),
            "{}",
            result.summary()
        );
    }

    #[test]
    fn check_kinds_serialize() {
        let check = schema_check("s");
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("SchemaValidation"), "got: {json}");
    }

    #[test]
    fn the_first_problem_is_reported_with_its_check_name() {
        let engine = AssuranceEngine::new(Box::new(|check| match check.name.as_str() {
            "ok" => CheckOutcome::Passed,
            _ => CheckOutcome::Failed {
                reason: "mismatch".into(),
            },
        }));
        let spec = AssuranceSpec::new(vec![schema_check("ok"), schema_check("broken")]);
        let problem = engine.run(&spec).first_problem().unwrap();
        assert!(problem.contains("broken"), "got: {problem}");
        assert!(problem.contains("mismatch"), "got: {problem}");
    }

    #[test]
    fn partial_failure_details() {
        let engine = AssuranceEngine::new(Box::new(|check| match check.name.as_str() {
            "pass1" | "pass2" => CheckOutcome::Passed,
            _ => CheckOutcome::Failed {
                reason: "nope".to_string(),
            },
        }));
        let spec = AssuranceSpec::new(vec![
            schema_check("pass1"),
            schema_check("fail1"),
            schema_check("pass2"),
        ]);
        let result = engine.run(&spec);
        assert!(!result.passed);
        assert_eq!(result.passed_count(), 2);
        assert_eq!(result.failed_count(), 1);
    }
}
