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
    /// The check passed.
    Passed,
    /// The check failed with a reason.
    Failed { reason: String },
}

/// Detail about one check's execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDetail {
    /// The name of the check that was run.
    pub name: String,
    /// The outcome of running this check.
    pub outcome: CheckOutcome,
    /// Whether evidence was provided (only relevant if evidence_required).
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
        self.details
            .iter()
            .filter(|d| d.outcome == CheckOutcome::Passed)
            .count()
    }

    /// Returns the number of failed checks.
    pub fn failed_count(&self) -> usize {
        self.details
            .iter()
            .filter(|d| matches!(d.outcome, CheckOutcome::Failed { .. }))
            .count()
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
            let evidence_provided = !check.evidence_required || outcome == CheckOutcome::Passed;

            if outcome != CheckOutcome::Passed {
                all_passed = false;
            }
            if check.evidence_required && !evidence_provided {
                all_passed = false;
            }

            details.push(CheckDetail {
                name: check.name.clone(),
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
        }
    }

    fn script_check(name: &str, evidence_required: bool) -> AssuranceCheck {
        AssuranceCheck {
            name: name.to_string(),
            kind: CheckKind::ScriptVerifier {
                script_path: "/checks/verify.sh".to_string(),
            },
            evidence_required,
        }
    }

    fn test_cmd_check(name: &str) -> AssuranceCheck {
        AssuranceCheck {
            name: name.to_string(),
            kind: CheckKind::TestCommand {
                command: "cargo test".to_string(),
            },
            evidence_required: true,
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
    fn missing_evidence_on_failure_blocks_success() {
        let engine = AssuranceEngine::always_fail("no evidence");
        let spec = AssuranceSpec::new(vec![script_check("needs-evidence", true)]);
        let result = engine.run(&spec);
        assert!(!result.passed);
        // Evidence not provided because the check failed.
        assert!(!result.details[0].evidence_provided);
    }

    #[test]
    fn check_kinds_serialize() {
        let check = schema_check("s");
        let yaml = serde_yaml::to_string(&check).unwrap();
        assert!(yaml.contains("SchemaValidation"));
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
