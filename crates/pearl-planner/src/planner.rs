//! Planner core: produces typed execution plans.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use pearl_core::PrecisionClass;
use serde::{Deserialize, Serialize};

/// A reference to a predecessor step's output — the wiring that makes a plan a pipeline.
///
/// Written as `steps.<step-id>.output`, optionally followed by a path into the JSON that step
/// printed: `steps.collect.output.items`, `steps.score.output.breakdown.confidence`.
///
/// The `steps.…​.output` prefix is mandatory rather than decorative. It is what allows a
/// mistyped reference to be *rejected* instead of quietly becoming a literal string: there is
/// exactly one thing a reference can name, so anything else is an error the compiler can
/// report. Literal values have their own field, so nothing is lost.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StepRef {
    /// The step whose output is wanted.
    pub step: String,
    /// The path into that step's JSON output. Empty means the whole output.
    pub path: Vec<String>,
}

impl StepRef {
    /// A reference to the whole of a step's output.
    pub fn whole(step: impl Into<String>) -> Self {
        Self {
            step: step.into(),
            path: Vec::new(),
        }
    }

    /// A reference to one path within a step's output.
    pub fn field(
        step: impl Into<String>,
        path: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            step: step.into(),
            path: path.into_iter().map(Into::into).collect(),
        }
    }
}

/// Why a reference expression could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("'{text}' is not a step output reference: {detail}. Expected steps.<step-id>.output or steps.<step-id>.output.<path>")]
pub struct BadStepRef {
    pub text: String,
    pub detail: String,
}

impl FromStr for StepRef {
    type Err = BadStepRef;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = |detail: &str| BadStepRef {
            text: text.to_string(),
            detail: detail.to_string(),
        };
        let parts: Vec<&str> = text.split('.').collect();
        // steps . <id> . output  — three segments before any path.
        if parts.len() < 3 {
            return Err(bad("too few segments"));
        }
        if parts[0] != "steps" {
            return Err(bad("it does not start with 'steps'"));
        }
        if parts[2] != "output" {
            return Err(bad(
                "the only readable part of a step is its output, so segment three must be 'output'",
            ));
        }
        if parts[1].is_empty() {
            return Err(bad("the step id is empty"));
        }
        if parts[3..].iter().any(|s| s.is_empty()) {
            return Err(bad("the path has an empty segment"));
        }
        Ok(Self {
            step: parts[1].to_string(),
            path: parts[3..].iter().map(|s| s.to_string()).collect(),
        })
    }
}

impl fmt::Display for StepRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "steps.{}.output", self.step)?;
        for segment in &self.path {
            write!(f, ".{segment}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for StepRef {
    type Error = BadStepRef;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<StepRef> for String {
    fn from(value: StepRef) -> Self {
        value.to_string()
    }
}

/// What the executor should do with a step's output — §40.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepRole {
    /// The output is a result. Keep it for the steps that read it.
    #[default]
    Execute,
    /// The output is a plan. Compile it and run it — §40's dynamic form.
    ///
    /// A separate role rather than a convention about which capabilities happen to return
    /// plans, because the difference decides whether output is *data* or *instructions*, and
    /// nothing should have to infer that from a capability id.
    Plan,
}

/// A single step in an execution plan.
///
/// Not `Eq`: `input` holds arbitrary JSON, and `serde_json::Value` is only `PartialEq` because
/// floats are. Structural comparison is still available where it is wanted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Unique identifier for this step within the plan.
    pub id: String,
    /// The capability to invoke (must exist in the registry).
    pub capability: String,
    /// Step ids this step depends on (must complete before this step runs).
    pub depends_on: Vec<String>,
    /// The precision class assigned to this step.
    pub precision_class: PrecisionClass,
    /// Maximum time allowed for this step.
    pub timeout: Duration,
    /// Whether this step's result must be exact — §22.
    ///
    /// This, not the precision class, is what obliges a verifier. The two are independent: a
    /// P0 step can be a best-effort probe whose result nobody depends on, and a P2 step can be
    /// the one thing that must be right. Deriving the obligation from the class made every
    /// mechanical step require a verifier, which is stricter than §30 says and made ordinary
    /// workflows impossible to compile.
    #[serde(default)]
    pub exactness_required: bool,
    /// Literal values merged into this step's payload.
    ///
    /// What a general capability needs in order to be reusable: `verifier.task-result` checks
    /// whichever keys it is told to check, so the step says which, rather than the repository
    /// carrying one bespoke verifier per caller.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<String, serde_json::Value>,
    /// Payload values taken from a predecessor's output — the data flow between steps.
    ///
    /// Kept separate from `input` rather than distinguished by the shape of the value. One map
    /// holding both would have to guess whether the string `steps.collect.output` is a
    /// reference or a literal, and guessing wrong in the safe direction (treat it as a
    /// literal) turns a typo into a step that runs on the wrong data and succeeds.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_from: BTreeMap<String, StepRef>,
    /// Whether this step's output is a result or a plan to run — §40.
    #[serde(default)]
    pub role: StepRole,
}

impl PlanStep {
    /// A step with no exactness demand.
    pub fn new(
        id: impl Into<String>,
        capability: impl Into<String>,
        precision_class: PrecisionClass,
        timeout: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            capability: capability.into(),
            depends_on: Vec::new(),
            precision_class,
            timeout,
            exactness_required: false,
            input: BTreeMap::new(),
            input_from: BTreeMap::new(),
            role: StepRole::Execute,
        }
    }

    /// Declares that this step produces a plan rather than a result — §40.
    pub fn proposing_a_plan(mut self) -> Self {
        self.role = StepRole::Plan;
        self
    }

    /// Whether this step's output is a plan to run.
    pub fn proposes_a_plan(&self) -> bool {
        self.role == StepRole::Plan
    }

    /// Declares that this step's result must be exact, and therefore verified.
    pub fn requiring_exactness(mut self) -> Self {
        self.exactness_required = true;
        self
    }

    /// Declares dependencies.
    pub fn after(mut self, deps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on = deps.into_iter().map(Into::into).collect();
        self
    }

    /// Adds a literal payload value.
    pub fn with_input(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.input.insert(key.into(), value);
        self
    }

    /// Wires a payload key to a predecessor's output.
    pub fn taking(mut self, key: impl Into<String>, reference: StepRef) -> Self {
        self.input_from.insert(key.into(), reference);
        self
    }

    /// The steps whose output this step reads.
    pub fn referenced_steps(&self) -> impl Iterator<Item = &str> {
        self.input_from.values().map(|r| r.step.as_str())
    }
}

/// Resource budget for plan generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBudget {
    /// Maximum number of steps allowed in a plan.
    pub max_steps: usize,
    /// Maximum number of LLM calls allowed across all steps.
    pub max_llm_calls: usize,
}

impl Default for PlanBudget {
    fn default() -> Self {
        Self {
            max_steps: 16,
            max_llm_calls: 32,
        }
    }
}

/// A complete execution plan ready for compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// The ordered steps of the plan.
    pub steps: Vec<PlanStep>,
    /// The budget that was applied when creating this plan.
    pub budget: PlanBudget,
}

impl ExecutionPlan {
    /// Returns the set of all step ids in this plan.
    pub fn step_ids(&self) -> HashSet<&str> {
        self.steps.iter().map(|s| s.id.as_str()).collect()
    }

    /// Returns the set of all capabilities referenced by steps.
    pub fn capabilities_used(&self) -> HashSet<&str> {
        self.steps.iter().map(|s| s.capability.as_str()).collect()
    }

    /// Count how many steps are classified as needing LLM (P1 or higher).
    pub fn llm_step_count(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| {
                matches!(
                    s.precision_class,
                    PrecisionClass::P1 | PrecisionClass::P2 | PrecisionClass::P3
                )
            })
            .count()
    }
}

/// Errors that can occur during plan creation.
#[derive(Debug, thiserror::Error)]
pub enum PlannerError {
    /// The plan exceeds the maximum number of steps allowed by the budget.
    #[error("plan has {actual} steps, exceeding budget limit of {limit}")]
    BudgetExceededSteps { actual: usize, limit: usize },

    /// The plan requires more LLM calls than the budget allows.
    #[error("plan requires {actual} LLM steps, exceeding budget limit of {limit}")]
    BudgetExceededLlmCalls { actual: usize, limit: usize },

    /// A step has a duplicate id.
    #[error("duplicate step id: {0}")]
    DuplicateStepId(String),

    /// A step depends on a non-existent step.
    #[error("step '{step}' depends on unknown step '{dependency}'")]
    UnknownDependency { step: String, dependency: String },
}

/// The Planner produces execution plans from a set of steps and a budget.
///
/// It validates basic structural invariants (unique ids, valid dependencies,
/// budget limits) but does NOT validate against the capability registry or
/// check for cycles -- that is the job of `pearl-plan-compiler`.
#[derive(Debug, Clone)]
pub struct Planner {
    budget: PlanBudget,
}

impl Default for Planner {
    fn default() -> Self {
        Self::new(PlanBudget::default())
    }
}

impl Planner {
    /// Creates a new planner with the given budget.
    pub fn new(budget: PlanBudget) -> Self {
        Self { budget }
    }

    /// Returns the budget this planner enforces.
    pub fn budget(&self) -> &PlanBudget {
        &self.budget
    }

    /// Builds an execution plan from the given steps.
    ///
    /// Validates:
    /// - Step count within budget
    /// - LLM call count within budget
    /// - No duplicate step ids
    /// - All depends_on references point to existing step ids
    pub fn build_plan(&self, steps: Vec<PlanStep>) -> Result<ExecutionPlan, PlannerError> {
        // Check step count budget.
        if steps.len() > self.budget.max_steps {
            return Err(PlannerError::BudgetExceededSteps {
                actual: steps.len(),
                limit: self.budget.max_steps,
            });
        }

        // Collect all step ids and check for duplicates.
        let mut seen_ids = HashSet::new();
        for step in &steps {
            if !seen_ids.insert(step.id.as_str()) {
                return Err(PlannerError::DuplicateStepId(step.id.clone()));
            }
        }

        // Check that all dependencies reference existing steps.
        for step in &steps {
            for dep in &step.depends_on {
                if !seen_ids.contains(dep.as_str()) {
                    return Err(PlannerError::UnknownDependency {
                        step: step.id.clone(),
                        dependency: dep.clone(),
                    });
                }
            }
        }

        // Build plan and check LLM budget.
        let plan = ExecutionPlan {
            steps,
            budget: self.budget.clone(),
        };

        let llm_count = plan.llm_step_count();
        if llm_count > self.budget.max_llm_calls {
            return Err(PlannerError::BudgetExceededLlmCalls {
                actual: llm_count,
                limit: self.budget.max_llm_calls,
            });
        }

        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, capability: &str, depends_on: &[&str], class: PrecisionClass) -> PlanStep {
        PlanStep::new(id, capability, class, Duration::from_secs(30)).after(depends_on.to_vec())
    }

    #[test]
    fn a_reference_names_a_step_and_a_path_into_its_output() {
        let whole: StepRef = "steps.collect.output".parse().unwrap();
        assert_eq!(whole, StepRef::whole("collect"));
        assert!(whole.path.is_empty());

        let nested: StepRef = "steps.score.output.breakdown.confidence".parse().unwrap();
        assert_eq!(nested.step, "score");
        assert_eq!(nested.path, vec!["breakdown", "confidence"]);
    }

    #[test]
    fn a_reference_round_trips_through_its_written_form() {
        for text in [
            "steps.collect.output",
            "steps.score.output.score",
            "steps.a.output.b.c.d",
        ] {
            let parsed: StepRef = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
            // And through serde, which is how it arrives from YAML.
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<StepRef>(&json).unwrap(), parsed);
        }
    }

    /// A mistyped reference must be an error rather than a literal: the alternative is a step
    /// that runs on the string "step.collect.output" and reports success.
    #[test]
    fn anything_that_is_not_a_reference_is_refused() {
        for text in [
            "collect",
            "step.collect.output",
            "steps.collect",
            "steps.collect.stdout",
            "steps..output",
            "steps.collect.output.",
            "",
        ] {
            assert!(
                text.parse::<StepRef>().is_err(),
                "'{text}' should not parse as a reference"
            );
        }
    }

    #[test]
    fn a_step_knows_which_steps_it_reads() {
        let s = step(
            "summarize",
            "agent.summarize",
            &["collect"],
            PrecisionClass::P1,
        )
        .taking("items", StepRef::field("collect", ["items"]))
        .taking("count", StepRef::field("collect", ["count"]))
        .with_input("style", serde_json::json!("terse"));
        let read: Vec<&str> = s.referenced_steps().collect();
        assert_eq!(read, vec!["collect", "collect"]);
        assert_eq!(s.input["style"], serde_json::json!("terse"));
    }

    #[test]
    fn creates_a_simple_plan() {
        let planner = Planner::default();
        let steps = vec![
            step("fetch", "script.fetch-data", &[], PrecisionClass::P0),
            step("process", "script.process", &["fetch"], PrecisionClass::P0),
        ];
        let plan = planner.build_plan(steps).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.llm_step_count(), 0);
    }

    #[test]
    fn creates_plan_with_llm_steps() {
        let planner = Planner::default();
        let steps = vec![
            step("fetch", "script.fetch-data", &[], PrecisionClass::P0),
            step(
                "summarize",
                "agent.summarize",
                &["fetch"],
                PrecisionClass::P1,
            ),
            step("review", "agent.review", &["summarize"], PrecisionClass::P3),
        ];
        let plan = planner.build_plan(steps).unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.llm_step_count(), 2);
    }

    #[test]
    fn enforces_step_count_budget() {
        let budget = PlanBudget {
            max_steps: 2,
            max_llm_calls: 10,
        };
        let planner = Planner::new(budget);
        let steps = vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &[], PrecisionClass::P0),
            step("c", "cap.c", &[], PrecisionClass::P0),
        ];
        let err = planner.build_plan(steps).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::BudgetExceededSteps {
                actual: 3,
                limit: 2
            }
        ));
    }

    #[test]
    fn enforces_llm_call_budget() {
        let budget = PlanBudget {
            max_steps: 10,
            max_llm_calls: 1,
        };
        let planner = Planner::new(budget);
        let steps = vec![
            step("a", "agent.a", &[], PrecisionClass::P1),
            step("b", "agent.b", &["a"], PrecisionClass::P2),
        ];
        let err = planner.build_plan(steps).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::BudgetExceededLlmCalls {
                actual: 2,
                limit: 1
            }
        ));
    }

    #[test]
    fn rejects_duplicate_step_ids() {
        let planner = Planner::default();
        let steps = vec![
            step("dup", "cap.a", &[], PrecisionClass::P0),
            step("dup", "cap.b", &[], PrecisionClass::P0),
        ];
        let err = planner.build_plan(steps).unwrap_err();
        assert!(matches!(err, PlannerError::DuplicateStepId(ref id) if id == "dup"));
    }

    #[test]
    fn rejects_unknown_dependency() {
        let planner = Planner::default();
        let steps = vec![step("a", "cap.a", &["nonexistent"], PrecisionClass::P0)];
        let err = planner.build_plan(steps).unwrap_err();
        assert!(matches!(err, PlannerError::UnknownDependency { .. }));
    }

    #[test]
    fn dependency_declaration_works() {
        let planner = Planner::default();
        let steps = vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0),
            step("c", "cap.c", &["a", "b"], PrecisionClass::P0),
        ];
        let plan = planner.build_plan(steps).unwrap();
        assert_eq!(plan.steps[2].depends_on, vec!["a", "b"]);
    }

    #[test]
    fn step_ids_and_capabilities_used() {
        let planner = Planner::default();
        let steps = vec![
            step("a", "cap.x", &[], PrecisionClass::P0),
            step("b", "cap.y", &["a"], PrecisionClass::P1),
        ];
        let plan = planner.build_plan(steps).unwrap();
        let ids = plan.step_ids();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        let caps = plan.capabilities_used();
        assert!(caps.contains("cap.x"));
        assert!(caps.contains("cap.y"));
    }

    #[test]
    fn empty_plan_is_valid() {
        let planner = Planner::default();
        let plan = planner.build_plan(vec![]).unwrap();
        assert_eq!(plan.steps.len(), 0);
        assert_eq!(plan.llm_step_count(), 0);
    }

    #[test]
    fn budget_default_values() {
        let budget = PlanBudget::default();
        assert_eq!(budget.max_steps, 16);
        assert_eq!(budget.max_llm_calls, 32);
    }
}
