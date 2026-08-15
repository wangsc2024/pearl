//! Planner core: produces typed execution plans.

use std::collections::HashSet;
use std::time::Duration;

use pearl_core::PrecisionClass;
use serde::{Deserialize, Serialize};

/// A single step in an execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        PlanStep {
            id: id.to_string(),
            capability: capability.to_string(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            precision_class: class,
            timeout: Duration::from_secs(30),
            exactness_required: false,
        }
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
