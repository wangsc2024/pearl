//! What a submitted task declares about how it should be run and verified — §22, §32.
//!
//! This exists because a declaration that is not persisted is a declaration that cannot be
//! honoured. A task spec naming `assurance: [{script: verifier.citations}]` passed the
//! Article 2 gate at submission and was then discarded, so the worker had no way to know a
//! verifier had been promised. The plan travels with the task instead: into the
//! `task.created` event, into the projection, and back out when a worker claims it.
//!
//! The distinction the two halves draw:
//!
//! - **plan** — what was declared, fixed at submission, never rewritten.
//! - **steps** — what actually ran, appended during execution.
//!
//! Keeping them apart is what allows "the task did not do what it said" to be a detectable
//! condition rather than an invisible one.

use serde::{Deserialize, Serialize};

/// One declared verification step.
///
/// Exactly one of `schema`, `script` or `test` is meaningful. The shape mirrors
/// `schemas/task-spec-v1.json`, where the three are alternatives rather than a tagged
/// union, because that reads better in YAML:
///
/// ```yaml
/// assurance:
///   - schema: verification-result-v1
///   - script: verifier.task-result
///     evidence_required: true
///   - test: cargo test -p pearl-core
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssuranceStep {
    /// Name of a JSON Schema the output must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// A verifier: either a capability id or a path to a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A test command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// Whether this step must produce evidence. Defaults to true.
    ///
    /// True by default because Article 4 is the norm and not the exception: a step that
    /// deliberately produces no evidence should have to say so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_required: Option<bool>,
    /// Parameters merged into the verifier's input.
    ///
    /// This is what makes one general verifier reusable: a task says which keys must be
    /// present rather than shipping a bespoke script per task.
    ///
    /// ```yaml
    /// assurance:
    ///   - script: verifier.task-result
    ///     input:
    ///       require_keys: [score, breakdown]
    ///       types: { score: number }
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
}

impl AssuranceStep {
    /// A schema step.
    pub fn schema(name: impl Into<String>) -> Self {
        Self {
            schema: Some(name.into()),
            ..Self::default()
        }
    }

    /// A verifier step.
    pub fn script(target: impl Into<String>) -> Self {
        Self {
            script: Some(target.into()),
            ..Self::default()
        }
    }

    /// A test-command step.
    pub fn test(command: impl Into<String>) -> Self {
        Self {
            test: Some(command.into()),
            ..Self::default()
        }
    }

    /// Whether this step names nothing to do.
    ///
    /// An empty step is a spec mistake worth catching: it would otherwise satisfy the
    /// Article 2 gate ("assurance is declared") while verifying nothing.
    pub fn is_empty(&self) -> bool {
        self.schema.is_none() && self.script.is_none() && self.test.is_none()
    }

    /// Whether evidence is required, applying the default.
    pub fn requires_evidence(&self) -> bool {
        self.evidence_required.unwrap_or(true)
    }

    /// A short label naming what this step does, for logs and events.
    pub fn label(&self) -> String {
        if let Some(schema) = &self.schema {
            return format!("schema:{schema}");
        }
        if let Some(script) = &self.script {
            return format!("verifier:{script}");
        }
        if let Some(test) = &self.test {
            return format!("test:{test}");
        }
        "empty".to_string()
    }
}

/// The execution and verification plan a task was submitted with.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPlan {
    /// The capability to execute, when the task names one.
    ///
    /// When absent the router falls back to matching on `task_type`, which is a guess.
    /// Naming the capability turns dispatch into a lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    /// Verification steps to run after execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assurance: Vec<AssuranceStep>,
    /// Task-level timeout, overriding the capability's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

impl TaskPlan {
    /// A plan that declares nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether any verification was declared.
    pub fn has_assurance(&self) -> bool {
        self.assurance.iter().any(|s| !s.is_empty())
    }

    /// The steps that name something to do.
    pub fn effective_assurance(&self) -> impl Iterator<Item = &AssuranceStep> {
        self.assurance.iter().filter(|s| !s.is_empty())
    }

    /// Whether the plan carries nothing at all, so it need not be persisted.
    pub fn is_empty(&self) -> bool {
        self.capability.is_none() && self.assurance.is_empty() && self.timeout_seconds.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_knows_what_it_declares() {
        assert_eq!(
            AssuranceStep::schema("evidence-v1").label(),
            "schema:evidence-v1"
        );
        assert_eq!(
            AssuranceStep::script("verifier.task-result").label(),
            "verifier:verifier.task-result"
        );
        assert_eq!(AssuranceStep::test("cargo test").label(), "test:cargo test");
        assert_eq!(AssuranceStep::default().label(), "empty");
    }

    #[test]
    fn an_empty_step_is_detectable() {
        assert!(AssuranceStep::default().is_empty());
        assert!(!AssuranceStep::schema("x").is_empty());
    }

    #[test]
    fn evidence_is_required_unless_waived() {
        assert!(AssuranceStep::schema("x").requires_evidence());
        let waived = AssuranceStep {
            evidence_required: Some(false),
            ..AssuranceStep::schema("x")
        };
        assert!(!waived.requires_evidence());
    }

    #[test]
    fn a_plan_of_only_empty_steps_declares_no_assurance() {
        // Otherwise `assurance: [{}]` would satisfy the Article 2 gate while verifying
        // nothing at all.
        let plan = TaskPlan {
            assurance: vec![AssuranceStep::default(), AssuranceStep::default()],
            ..TaskPlan::empty()
        };
        assert!(!plan.has_assurance());
        assert_eq!(plan.effective_assurance().count(), 0);
        assert!(!plan.is_empty(), "the steps are still recorded as declared");
    }

    #[test]
    fn plans_round_trip_through_json() {
        let plan = TaskPlan {
            capability: Some("script.task-score".into()),
            assurance: vec![
                AssuranceStep::schema("verification-result-v1"),
                AssuranceStep {
                    evidence_required: Some(true),
                    ..AssuranceStep::script("verifier.task-result")
                },
            ],
            timeout_seconds: Some(30),
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<TaskPlan>(&json).unwrap(), plan);
    }

    #[test]
    fn an_absent_plan_deserializes_to_empty() {
        // Ledgers written before the plan existed must still replay.
        assert_eq!(
            serde_json::from_str::<TaskPlan>("{}").unwrap(),
            TaskPlan::empty()
        );
        assert!(TaskPlan::empty().is_empty());
    }

    #[test]
    fn yaml_alternatives_parse_as_written_in_a_spec() {
        let yaml = r#"
capability: script.task-score
timeout_seconds: 30
assurance:
  - schema: verification-result-v1
  - script: verifier.task-result
    evidence_required: true
  - test: cargo test -p pearl-core
"#;
        let plan: TaskPlan = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(plan.capability.as_deref(), Some("script.task-score"));
        assert_eq!(plan.assurance.len(), 3);
        assert!(plan.has_assurance());
    }
}
