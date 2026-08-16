//! Task spec loading — mirrors `schemas/task-spec-v1.json`.
//!
//! Lives here rather than in the CLI because a spec is not a CLI concept: the scheduler
//! submits tasks from spec files too, and a type only the CLI could parse would have forced
//! the daemon to reimplement it — which is how two parsers disagree about the Article 2 gate.

use crate::TaskSubmission;
use pearl_core::{AssuranceStep, PrecisionClass, QualitySpec, TaskId, TaskPlan};
use serde::{Deserialize, Serialize};

/// A submitted task spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub version: u32,
    pub task_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision_class: Option<PrecisionClass>,
    pub quality: QualitySpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assurance: Vec<AssuranceStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// What to hand the capability on `PEARL_INPUT`.
    ///
    /// Declared in `schemas/task-spec-v1.json` from the start and dropped by this parser
    /// until now, so a spec that set it was ignored without complaint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl TaskSpec {
    /// Parses a spec from YAML or JSON.
    ///
    /// YAML is attempted first because `serde_yaml` also accepts JSON, so one path covers
    /// both formats without asking the caller to declare which they wrote.
    pub fn parse(source: &str) -> Result<Self, SpecError> {
        serde_yaml::from_str(source).map_err(|e| SpecError::Parse {
            detail: e.to_string(),
        })
    }

    /// Loads a spec from a file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, SpecError> {
        let path = path.as_ref();
        let source = std::fs::read_to_string(path).map_err(|e| SpecError::Parse {
            detail: format!("{}: {e}", path.display()),
        })?;
        Self::parse(&source)
    }

    /// Converts into a submission under a different id.
    ///
    /// Used by the scheduler: a recurring schedule submits a new task each time it fires, and
    /// reusing the spec's id would collide with the previous occurrence. The id is the only
    /// thing that changes — the plan and quality contract are the spec's, so a scheduled run
    /// is verified exactly as a manual one would be.
    pub fn into_submission_as(mut self, task_id: &str) -> Result<TaskSubmission, SpecError> {
        self.id = task_id.to_string();
        self.into_submission()
    }

    /// Validates and converts into a submission.
    ///
    /// The Article 2 check happens here rather than at execution time so the operator is
    /// told at submission that the task could never succeed, instead of discovering it
    /// after the work has run.
    pub fn into_submission(self) -> Result<TaskSubmission, SpecError> {
        if self.version == 0 {
            return Err(SpecError::Invalid {
                detail: "version must be at least 1".into(),
            });
        }

        let task_id = TaskId::parse(self.id.clone()).map_err(|e| SpecError::Invalid {
            detail: e.to_string(),
        })?;

        if self.quality.gate().blocks() && self.assurance.is_empty() {
            return Err(SpecError::ConstitutionViolation {
                article: 2,
                detail: format!(
                    "task '{}' requires exactness but declares neither deterministic_verification nor any assurance step, so it could never reach VERIFIED_SUCCESS",
                    self.id
                ),
            });
        }

        if let Some(class) = self.precision_class {
            if class == PrecisionClass::P0 && !self.quality.deterministic_generation {
                return Err(SpecError::ConstitutionViolation {
                    article: 1,
                    detail: format!(
                        "task '{}' is classified p0 (deterministic) but declares deterministic_generation: false",
                        self.id
                    ),
                });
            }
        }

        // A declared step that names nothing verifies nothing, so it must not be able to
        // satisfy the Article 2 gate above by merely existing.
        if let Some(index) = self.assurance.iter().position(AssuranceStep::is_empty) {
            return Err(SpecError::Invalid {
                detail: format!(
                    "assurance step {} declares none of schema, script or test, so it would verify nothing",
                    index + 1
                ),
            });
        }

        Ok(
            TaskSubmission::new(task_id, self.task_type, self.precision_class, self.quality)
                // Carried into the ledger rather than discarded here: a worker that cannot read the
                // declared plan cannot honour it, and the Article 2 gate above would then have
                // approved a promise nothing kept.
                .with_plan(TaskPlan {
                    capability: self.capability,
                    assurance: self.assurance,
                    timeout_seconds: self.timeout_seconds,
                    payload: self.payload,
                }),
        )
    }
}

/// Spec failures.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("failed to parse task spec: {detail}")]
    Parse { detail: String },
    #[error("invalid task spec: {detail}")]
    Invalid { detail: String },
    #[error("Constitution Article {article}: {detail}")]
    ConstitutionViolation { article: u8, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const MECHANICAL: &str = r#"
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

    #[test]
    fn parses_a_yaml_spec() {
        let spec = TaskSpec::parse(MECHANICAL).unwrap();
        assert_eq!(spec.id, "daily.digest");
        assert_eq!(spec.task_type, "digest");
        assert_eq!(spec.precision_class, Some(PrecisionClass::P1));
        assert!(spec.quality.deterministic_verification);
    }

    #[test]
    fn parses_json_too() {
        let json = r#"{
            "id": "t1", "version": 1, "task_type": "digest",
            "quality": {"exactness_required": false, "deterministic_verification": false}
        }"#;
        assert_eq!(TaskSpec::parse(json).unwrap().id, "t1");
    }

    #[test]
    fn converts_to_a_submission() {
        let submission = TaskSpec::parse(MECHANICAL)
            .unwrap()
            .into_submission()
            .unwrap();
        assert_eq!(submission.task_id.as_str(), "daily.digest");
        assert_eq!(submission.task_type, "digest");
    }

    #[test]
    fn rejects_version_zero() {
        let spec = TaskSpec::parse(&MECHANICAL.replace("version: 1", "version: 0")).unwrap();
        assert!(matches!(
            spec.into_submission(),
            Err(SpecError::Invalid { .. })
        ));
    }

    #[test]
    fn rejects_an_id_that_would_break_idempotency_keys() {
        let spec = TaskSpec::parse(&MECHANICAL.replace("daily.digest", "Daily:Digest")).unwrap();
        assert!(matches!(
            spec.into_submission(),
            Err(SpecError::Invalid { .. })
        ));
    }

    #[test]
    fn article_2_exactness_without_verification_or_assurance_is_rejected() {
        let yaml = r#"
id: t1
version: 1
task_type: research
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: false
"#;
        let err = TaskSpec::parse(yaml)
            .unwrap()
            .into_submission()
            .unwrap_err();
        match err {
            SpecError::ConstitutionViolation { article, .. } => assert_eq!(article, 2),
            other => panic!("expected an Article 2 violation, got {other:?}"),
        }
    }

    #[test]
    fn article_2_is_satisfied_by_declaring_assurance() {
        let yaml = r#"
id: t1
version: 1
task_type: research
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: false
assurance:
  - script: verifier.citations
    evidence_required: true
"#;
        assert!(TaskSpec::parse(yaml).unwrap().into_submission().is_ok());
    }

    #[test]
    fn the_declared_plan_survives_into_the_submission() {
        // The whole point: a worker later reads this back out of the ledger. If the plan
        // were dropped here, the Article 2 gate above would have approved a promise that
        // nothing was left to keep.
        let yaml = r#"
id: t1
version: 1
task_type: scoring
capability: script.task-score
timeout_seconds: 45
quality:
  exactness_required: true
  deterministic_generation: true
  deterministic_verification: true
assurance:
  - schema: verification-result-v1
  - script: verifier.task-result
    evidence_required: true
"#;
        let submission = TaskSpec::parse(yaml).unwrap().into_submission().unwrap();
        assert_eq!(
            submission.plan.capability.as_deref(),
            Some("script.task-score")
        );
        assert_eq!(submission.plan.timeout_seconds, Some(45));
        assert_eq!(submission.plan.assurance.len(), 2);
        assert!(submission.plan.has_assurance());
    }

    #[test]
    fn an_assurance_step_that_names_nothing_is_rejected() {
        // `assurance: [{}]` would otherwise pass the Article 2 gate while verifying nothing.
        let yaml = r#"
id: t1
version: 1
task_type: research
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: false
assurance:
  - evidence_required: true
"#;
        let err = TaskSpec::parse(yaml)
            .unwrap()
            .into_submission()
            .unwrap_err();
        assert!(matches!(err, SpecError::Invalid { .. }), "got: {err:?}");
    }

    #[test]
    fn article_1_p0_must_generate_deterministically() {
        let yaml = r#"
id: t1
version: 1
task_type: scoring
precision_class: p0
quality:
  exactness_required: true
  deterministic_generation: false
  deterministic_verification: true
"#;
        let err = TaskSpec::parse(yaml)
            .unwrap()
            .into_submission()
            .unwrap_err();
        match err {
            SpecError::ConstitutionViolation { article, .. } => assert_eq!(article, 1),
            other => panic!("expected an Article 1 violation, got {other:?}"),
        }
    }

    #[test]
    fn a_best_effort_task_needs_no_assurance() {
        let yaml = r#"
id: t1
version: 1
task_type: zen
precision_class: p3
quality:
  exactness_required: false
  deterministic_generation: false
  deterministic_verification: false
"#;
        assert!(TaskSpec::parse(yaml).unwrap().into_submission().is_ok());
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(TaskSpec::parse("id: [unclosed").is_err());
        assert!(TaskSpec::parse("not_a_task: true").is_err());
    }
}
