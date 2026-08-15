//! Task spec loading — mirrors `schemas/task-spec-v1.json`.

use pearl_core::{PrecisionClass, QualitySpec, TaskId};
use pearl_state::TaskSubmission;
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
}

/// One verification step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssuranceStep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_required: Option<bool>,
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

        Ok(TaskSubmission {
            task_id,
            task_type: self.task_type,
            precision_class: self.precision_class,
            quality: self.quality,
        })
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
