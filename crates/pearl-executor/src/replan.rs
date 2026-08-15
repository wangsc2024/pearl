//! Dynamic Planner–Executor: a step that returns a plan — §40, §31.
//!
//! §40 asks for two workflow forms. The declarative one is a file: a human wrote the steps and
//! a reviewer read them. The dynamic one is `Planner → sub-plan → Compiler → Execution`, for
//! when the shape of the work is only knowable at runtime.
//!
//! The thing that makes the second form safe is the third arrow. §31 says the Executor may not
//! change policy, expand its tools, ignore a dependency or add a side effect — and a plan that
//! arrived as text from something reasoning is precisely an attempt to do all four. So it is
//! not executed. It is *compiled*, by the same [`PlanCompiler`] and against the same capability
//! set as the plan in the file, and only the result of that is run.
//!
//! Concretely, a sub-plan is subject to:
//!
//! - **the capability set** — it can only name capabilities the registry already has, so it
//!   cannot invent a tool (§30, §31);
//! - **the verifier rule** — a sub-step demanding exactness needs a `verify` step in the same
//!   sub-plan depending on it (§30, Article 8);
//! - **the parent's budget** — its steps are counted against the same `max_steps` and
//!   `max_llm_calls` the parent plan declared, so replanning cannot buy more work than the run
//!   was authorised to do;
//! - **a depth limit** — a sub-plan may ask for another only as deep as the caller allowed,
//!   because an unbounded chain of "let me think about it further" is a runaway;
//! - **the DAG and timeout checks** every plan gets.
//!
//! What is deliberately *not* delegated: the sub-plan cannot rename or shadow a parent step.
//! Its ids are prefixed with the planning step that produced them, so `summarize` proposed by
//! `decide` becomes `decide/summarize`. Without that, a proposal naming an id the parent
//! already used would overwrite that step's recorded output — and a proposal naming the *next*
//! parent step's id would make the executor skip it as already completed.

use std::collections::HashSet;

use pearl_plan_compiler::{CompileError, CompiledPlan, CompilerConfig, PlanCompiler};
use pearl_planner::{
    ExecutionPlan, PlanBudget, PlanProposal, PlanStep, Planner, PlannerError, ProposalError,
    StepRef,
};

use crate::executor::StepOutput;

/// What the caller allows a planning step to do — §40.
///
/// Absent from [`crate::ExecutorConfig`] by default, so dynamic planning is opt-in. A run that
/// never intended to replan should not gain the ability because a plan happened to contain a
/// `plan` step: the capability set a sub-plan may draw from is a decision, and something has
/// to make it deliberately.
#[derive(Debug, Clone)]
pub struct DynamicPlanning {
    /// The gate a sub-plan is compiled against — the registry it may draw from and the
    /// verification rules it must satisfy.
    pub compiler: CompilerConfig,
    /// How many levels of planning are allowed. `1` means a plan step may produce ordinary
    /// steps but none of them may plan again.
    pub max_depth: u32,
}

impl Default for DynamicPlanning {
    fn default() -> Self {
        Self {
            compiler: CompilerConfig::default(),
            // One level: enough for "decide what to do, then do it", short of a chain of
            // planners planning planners. Raise it deliberately.
            max_depth: 1,
        }
    }
}

impl DynamicPlanning {
    /// Planning limited to the given capability set.
    pub fn within(compiler: CompilerConfig) -> Self {
        Self {
            compiler,
            ..Self::default()
        }
    }

    /// Sets how deep planning may nest.
    pub fn to_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = max_depth;
        self
    }
}

/// The Executor asking the Planner for a plan — §31's `ReplanRequest`.
#[derive(Debug, Clone)]
pub struct ReplanRequest {
    /// The step whose output this is.
    pub origin: String,
    /// How many planning steps deep this is; the outermost is 1.
    pub depth: u32,
    /// What the planning step printed.
    pub output: StepOutput,
    /// Steps that have already finished, whose output a sub-plan may therefore read.
    pub completed: HashSet<String>,
    /// How many steps the run has left in its budget.
    pub steps_remaining: usize,
}

/// Why a proposed plan was not run.
#[derive(Debug, thiserror::Error)]
pub enum ReplanError {
    /// The caller did not enable dynamic planning.
    #[error("step '{origin}' returned a plan, but dynamic planning is not enabled for this run; nothing was run from it")]
    NotEnabled { origin: String },

    /// The chain of planning steps is as deep as it is allowed to get.
    #[error("step '{origin}' asked to plan at depth {depth}, beyond the limit of {max_depth}")]
    TooDeep {
        origin: String,
        depth: u32,
        max_depth: u32,
    },

    /// The output was not a plan.
    #[error("step '{origin}' was asked for a plan: {source}")]
    NotAPlan {
        origin: String,
        #[source]
        source: ProposalError,
    },

    /// The sub-plan would exceed what the run was authorised to do.
    #[error("step '{origin}' proposed {proposed} step(s) but only {remaining} remain in the plan's budget")]
    OverBudget {
        origin: String,
        proposed: usize,
        remaining: usize,
    },

    /// The proposal was structurally invalid.
    #[error("the plan from step '{origin}' is not well formed: {source}")]
    Unplannable {
        origin: String,
        #[source]
        source: PlannerError,
    },

    /// The proposal did not survive the compiler — §30.
    #[error(
        "the plan from step '{origin}' did not compile, so none of it ran: {}",
        format_errors(problems)
    )]
    DidNotCompile {
        origin: String,
        problems: Vec<CompileError>,
    },
}

fn format_errors(problems: &[CompileError]) -> String {
    problems
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Turns what a planning step printed into steps that are safe to run — §40.
///
/// The whole path in one function so there is exactly one way a dynamic plan can reach the
/// executor: read the proposal, refuse anything that is not one, plan it, compile it against
/// the caller's capability set, then namespace it.
pub fn expand(
    request: &ReplanRequest,
    planning: Option<&DynamicPlanning>,
    budget: &PlanBudget,
) -> Result<Vec<PlanStep>, ReplanError> {
    let origin = request.origin.clone();

    let Some(planning) = planning else {
        return Err(ReplanError::NotEnabled { origin });
    };
    if request.depth > planning.max_depth {
        return Err(ReplanError::TooDeep {
            origin,
            depth: request.depth,
            max_depth: planning.max_depth,
        });
    }

    let value = request.output.as_value();
    let proposal = PlanProposal::from_value(&value).map_err(|source| ReplanError::NotAPlan {
        origin: origin.clone(),
        source,
    })?;

    if proposal.steps.len() > request.steps_remaining {
        return Err(ReplanError::OverBudget {
            origin,
            proposed: proposal.steps.len(),
            remaining: request.steps_remaining,
        });
    }

    // The Planner, not the Executor, turns steps into a plan: duplicate ids, unknown
    // dependencies and the budget are its checks, and they apply the same way whether a human
    // or a model wrote the steps.
    let plan = Planner::new(budget.clone())
        .build_plan(proposal.to_plan_steps())
        .map_err(|source| ReplanError::Unplannable {
            origin: origin.clone(),
            source,
        })?;

    // Then the Compiler, which is where §30 lives. `verified_steps` comes from the proposal
    // itself — a sub-step demanding exactness must be verified inside the sub-plan, because
    // the parent workflow could not have known the step would exist.
    let config = CompilerConfig {
        known_capabilities: planning.compiler.known_capabilities.clone(),
        verified_steps: proposal
            .verify_targets()
            .union(&planning.compiler.verified_steps)
            .cloned()
            .collect(),
        completed_steps: request.completed.clone(),
    };
    let compiled = PlanCompiler::new(config)
        .compile(&plan)
        .map_err(|problems| ReplanError::DidNotCompile {
            origin: origin.clone(),
            problems,
        })?;

    Ok(namespaced(&origin, compiled))
}

/// Prefixes a sub-plan's step ids with the step that produced it.
///
/// Applied after compilation so the proposal is validated in its own namespace and the rename
/// is a mechanical, total substitution. References to steps *outside* the sub-plan are left
/// alone: those name parent steps that already finished, and prefixing them would turn a valid
/// reference into a dangling one.
fn namespaced(origin: &str, compiled: CompiledPlan) -> Vec<PlanStep> {
    let owned: HashSet<String> = compiled
        .execution_order
        .iter()
        .map(|s| s.id.clone())
        .collect();
    let rename = |id: &str| format!("{origin}/{id}");

    compiled
        .execution_order
        .into_iter()
        .map(|mut step| {
            step.id = rename(&step.id);
            step.depends_on = step
                .depends_on
                .iter()
                .map(|dep| {
                    if owned.contains(dep) {
                        rename(dep)
                    } else {
                        dep.clone()
                    }
                })
                .collect();
            step.input_from = step
                .input_from
                .into_iter()
                .map(|(key, reference)| {
                    let reference = if owned.contains(&reference.step) {
                        StepRef {
                            step: rename(&reference.step),
                            path: reference.path,
                        }
                    } else {
                        reference
                    };
                    (key, reference)
                })
                .collect();
            step
        })
        .collect()
}

/// The plan a dynamic expansion would run, without running it.
///
/// Exposed for `pearl workflow` style tooling: showing an operator what a planner proposed,
/// and whether it would compile, without committing to it.
pub fn dry_run(
    request: &ReplanRequest,
    planning: Option<&DynamicPlanning>,
    budget: &PlanBudget,
) -> Result<ExecutionPlan, ReplanError> {
    let steps = expand(request, planning, budget)?;
    Ok(ExecutionPlan {
        steps,
        budget: budget.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(ids: &[&str]) -> CompilerConfig {
        CompilerConfig {
            known_capabilities: ids.iter().map(|s| s.to_string()).collect(),
            ..CompilerConfig::default()
        }
    }

    fn request(output: serde_json::Value) -> ReplanRequest {
        ReplanRequest {
            origin: "decide".to_string(),
            depth: 1,
            output: StepOutput::json(output),
            completed: HashSet::new(),
            steps_remaining: 8,
        }
    }

    fn budget() -> PlanBudget {
        PlanBudget {
            max_steps: 16,
            max_llm_calls: 4,
        }
    }

    fn proposal_of(steps: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "steps": steps })
    }

    #[test]
    fn a_proposal_becomes_steps_namespaced_by_the_step_that_asked() {
        let planning = DynamicPlanning::within(caps(&["script.a", "script.b"]));
        let req = request(proposal_of(serde_json::json!([
            { "id": "first", "capability": "script.a" },
            { "id": "second", "capability": "script.b", "depends_on": ["first"],
              "input_from": { "seed": "steps.first.output.value" } }
        ])));

        let steps = expand(&req, Some(&planning), &budget()).unwrap();

        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "decide/first");
        assert_eq!(steps[1].id, "decide/second");
        // Dependencies and references are renamed with them, or the sub-plan would point at
        // ids that no longer exist.
        assert_eq!(steps[1].depends_on, vec!["decide/first"]);
        assert_eq!(steps[1].input_from["seed"].step, "decide/first");
        assert_eq!(steps[1].input_from["seed"].path, vec!["value"]);
    }

    #[test]
    fn a_sub_plan_may_read_a_parent_step_that_already_finished() {
        let planning = DynamicPlanning::within(caps(&["script.a"]));
        let mut req = request(proposal_of(serde_json::json!([
            { "id": "use-it", "capability": "script.a",
              "input_from": { "items": "steps.collect.output.items" } }
        ])));
        req.completed.insert("collect".to_string());

        let steps = expand(&req, Some(&planning), &budget()).unwrap();

        // Left un-prefixed: `collect` is a parent step, not one of the sub-plan's.
        assert_eq!(steps[0].input_from["items"].step, "collect");
    }

    /// §31: the Executor may not expand its own tools. A proposal naming a capability the
    /// registry does not have is refused by the compiler, and nothing runs.
    #[test]
    fn a_proposal_naming_an_unknown_capability_does_not_compile() {
        let planning = DynamicPlanning::within(caps(&["script.a"]));
        let req = request(proposal_of(serde_json::json!([
            { "id": "sneaky", "capability": "script.not-in-the-registry" }
        ])));

        let err = expand(&req, Some(&planning), &budget()).unwrap_err();
        assert!(
            matches!(err, ReplanError::DidNotCompile { .. }),
            "got {err}"
        );
        assert!(err.to_string().contains("unknown capability"), "got {err}");
    }

    /// Article 8 / §30, applied to a plan nobody reviewed: a sub-step that demands exactness
    /// must be verified inside the sub-plan.
    #[test]
    fn a_sub_step_demanding_exactness_needs_a_verifier_in_the_sub_plan() {
        let planning = DynamicPlanning::within(caps(&["script.a", "verifier.x"]));

        let unverified = request(proposal_of(serde_json::json!([
            { "id": "load-bearing", "capability": "script.a", "exactness_required": true }
        ])));
        let err = expand(&unverified, Some(&planning), &budget()).unwrap_err();
        assert!(err.to_string().contains("requires a verifier"), "got {err}");

        // With a verify step depending on it, the same proposal compiles.
        let verified = request(proposal_of(serde_json::json!([
            { "id": "load-bearing", "capability": "script.a", "exactness_required": true },
            { "id": "check", "capability": "verifier.x", "kind": "verify",
              "depends_on": ["load-bearing"] }
        ])));
        assert!(expand(&verified, Some(&planning), &budget()).is_ok());
    }

    #[test]
    fn a_sub_plan_cannot_buy_more_work_than_the_budget_allows() {
        let planning = DynamicPlanning::within(caps(&["script.a"]));
        let mut req = request(proposal_of(serde_json::json!([
            { "id": "one", "capability": "script.a" },
            { "id": "two", "capability": "script.a" },
            { "id": "three", "capability": "script.a" }
        ])));
        req.steps_remaining = 2;

        let err = expand(&req, Some(&planning), &budget()).unwrap_err();
        assert!(
            matches!(
                err,
                ReplanError::OverBudget {
                    proposed: 3,
                    remaining: 2,
                    ..
                }
            ),
            "got {err}"
        );
    }

    #[test]
    fn planning_steps_count_against_the_llm_budget() {
        let planning = DynamicPlanning::within(caps(&["agent.p"])).to_depth(3);
        let req = request(proposal_of(serde_json::json!([
            { "id": "a", "capability": "agent.p", "kind": "plan" },
            { "id": "b", "capability": "agent.p", "kind": "plan" }
        ])));

        let tight = PlanBudget {
            max_steps: 16,
            max_llm_calls: 1,
        };
        let err = expand(&req, Some(&planning), &tight).unwrap_err();
        assert!(err.to_string().contains("LLM"), "got {err}");
    }

    #[test]
    fn planning_beyond_the_depth_limit_is_refused() {
        let planning = DynamicPlanning::within(caps(&["script.a"])).to_depth(1);
        let mut req = request(proposal_of(serde_json::json!([
            { "id": "a", "capability": "script.a" }
        ])));
        req.depth = 2;

        let err = expand(&req, Some(&planning), &budget()).unwrap_err();
        assert!(
            matches!(err, ReplanError::TooDeep { max_depth: 1, .. }),
            "got {err}"
        );
    }

    #[test]
    fn dynamic_planning_is_off_unless_the_caller_enabled_it() {
        let req = request(proposal_of(serde_json::json!([
            { "id": "a", "capability": "script.a" }
        ])));
        let err = expand(&req, None, &budget()).unwrap_err();
        assert!(matches!(err, ReplanError::NotEnabled { .. }), "got {err}");
        assert!(
            err.to_string().contains("nothing was run"),
            "the message should say what did not happen: {err}"
        );
    }

    #[test]
    fn output_that_is_not_a_plan_is_refused_with_what_it_was() {
        let planning = DynamicPlanning::within(caps(&["script.a"]));
        for output in [
            StepOutput::text("I think we should probably collect the news first"),
            StepOutput::json(serde_json::json!({ "steps": [] })),
            StepOutput::json(serde_json::json!({ "steps": [], "shell": "rm -rf /" })),
        ] {
            let req = ReplanRequest {
                output,
                ..request(serde_json::json!(null))
            };
            let err = expand(&req, Some(&planning), &budget()).unwrap_err();
            assert!(matches!(err, ReplanError::NotAPlan { .. }), "got {err}");
        }
    }

    #[test]
    fn a_dry_run_returns_the_plan_without_a_compiler_of_its_own() {
        let planning = DynamicPlanning::within(caps(&["script.a"]));
        let req = request(proposal_of(serde_json::json!([
            { "id": "a", "capability": "script.a" }
        ])));
        let plan = dry_run(&req, Some(&planning), &budget()).unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "decide/a");
    }
}
