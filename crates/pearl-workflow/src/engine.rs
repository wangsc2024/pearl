//! Workflow engine: parses YAML workflow definitions and converts them to plans.

use std::collections::BTreeMap;
use std::time::Duration;

use pearl_core::PrecisionClass;
use pearl_plan_compiler::{CompiledPlan, CompilerConfig, PlanCompiler};
use pearl_planner::{PlanBudget, PlanStep, Planner, StepRef, StepRole};
use serde::{Deserialize, Serialize};

/// The type of a workflow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepType {
    /// A sequential run step.
    Run,
    /// A step that can run in parallel with siblings.
    Parallel,
    /// A verification step.
    Verify,
    /// A side-effecting step.
    Effect,
    /// A step whose output is a plan to run, not a result to keep — §40's dynamic form.
    ///
    /// This is the seam between the two workflow forms the specification requires. Everything
    /// above is declarative: a human wrote it and a reviewer read it. A `plan` step hands that
    /// job to something reasoning at runtime, and the plan it returns goes through the same
    /// compiler as the one in the file — so the dynamic form is constrained by exactly the
    /// rules the declarative form is, rather than by a second, laxer path.
    Plan,
}

/// A single step in a workflow definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Unique id for this step.
    pub id: String,
    /// The capability to invoke.
    pub capability: String,
    /// Step type (run, parallel, verify, effect).
    pub step_type: StepType,
    /// Steps this step depends on (by id).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Group id for parallel steps (steps in the same group run concurrently).
    #[serde(default)]
    pub parallel_group: Option<String>,
    /// Whether this step's result must be exact, and therefore verified — §22, §30.
    ///
    /// Declared per step rather than inferred, because only the workflow's author knows which
    /// results are load-bearing. A step that declares it and has no `verify` step depending on
    /// it will not compile.
    #[serde(default)]
    pub exactness_required: bool,
    /// Constants this step is configured with, merged into its payload.
    ///
    /// ```yaml
    /// input:
    ///   require_keys: [score, breakdown]
    ///   types: { score: number }
    /// ```
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<String, serde_json::Value>,
    /// Payload keys wired to a predecessor's output — the data flow between steps.
    ///
    /// ```yaml
    /// input_from:
    ///   result: steps.score.output
    ///   just_the_score: steps.score.output.score
    /// ```
    ///
    /// A second field rather than a convention inside `input`, so that no value has to be
    /// inspected to learn whether it is a reference. `steps.score.output` as a literal string
    /// is a legitimate thing to pass a capability; if one map held both, PEARL would have to
    /// guess which was meant, and the safe-looking guess turns a typo into a step that runs
    /// on the wrong data and reports success.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_from: BTreeMap<String, StepRef>,
}

fn default_timeout_secs() -> u64 {
    60
}

/// A declarative workflow definition parsed from YAML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowDef {
    /// Workflow name.
    pub name: String,
    /// Workflow description.
    #[serde(default)]
    pub description: String,
    /// The steps in this workflow.
    pub steps: Vec<WorkflowStep>,
    /// Budget constraints.
    #[serde(default)]
    pub budget: Option<PlanBudget>,
}

impl WorkflowDef {
    /// Parse a WorkflowDef from YAML text.
    pub fn from_yaml(yaml: &str) -> Result<Self, WorkflowError> {
        serde_yaml::from_str(yaml).map_err(|e| WorkflowError::ParseError {
            detail: e.to_string(),
        })
    }

    /// Convert this workflow into plan steps.
    fn to_plan_steps(&self) -> Vec<PlanStep> {
        self.steps
            .iter()
            .map(|ws| {
                let precision_class = match ws.step_type {
                    StepType::Run => PrecisionClass::P0,
                    StepType::Parallel => PrecisionClass::P0,
                    StepType::Verify => PrecisionClass::P0,
                    // Planning is reasoning, so it is classified as such — which also makes it
                    // count against the plan's LLM budget rather than being free.
                    StepType::Plan => PrecisionClass::P1,
                    StepType::Effect => PrecisionClass::P2,
                };
                let role = match ws.step_type {
                    StepType::Plan => StepRole::Plan,
                    _ => StepRole::Execute,
                };
                PlanStep {
                    id: ws.id.clone(),
                    capability: ws.capability.clone(),
                    depends_on: ws.depends_on.clone(),
                    precision_class,
                    timeout: Duration::from_secs(ws.timeout_secs),
                    exactness_required: ws.exactness_required,
                    input: ws.input.clone(),
                    input_from: ws.input_from.clone(),
                    role,
                }
            })
            .collect()
    }
}

/// Errors from the workflow engine.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// Failed to parse the YAML workflow definition.
    #[error("failed to parse workflow YAML: {detail}")]
    ParseError { detail: String },

    /// Failed to build a plan from the workflow.
    #[error("failed to build plan: {detail}")]
    PlanError { detail: String },

    /// Failed to compile the plan.
    #[error("failed to compile plan: {errors:?}")]
    CompileError { errors: Vec<String> },
}

/// The result of a workflow execution.
#[derive(Debug, Clone)]
pub struct WorkflowResult {
    /// Whether the workflow completed successfully.
    pub success: bool,
    /// The compiled plan that was produced.
    pub compiled_plan: CompiledPlan,
}

/// The workflow engine converts YAML workflow definitions into compiled plans.
pub struct WorkflowEngine {
    compiler_config: CompilerConfig,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowEngine {
    /// Creates a new workflow engine with default compiler config.
    pub fn new() -> Self {
        Self {
            compiler_config: CompilerConfig::default(),
        }
    }

    /// Creates a workflow engine with a specific compiler configuration.
    pub fn with_config(config: CompilerConfig) -> Self {
        Self {
            compiler_config: config,
        }
    }

    /// Converts a workflow definition into a compiled plan.
    ///
    /// Steps:
    /// 1. Convert WorkflowDef steps into PlanSteps
    /// 2. Build an ExecutionPlan via the Planner
    /// 3. Compile the plan via the PlanCompiler
    pub fn compile_workflow(&self, def: &WorkflowDef) -> Result<CompiledPlan, WorkflowError> {
        let plan_steps = def.to_plan_steps();
        let budget = def.budget.clone().unwrap_or_default();
        let planner = Planner::new(budget);

        let plan = planner
            .build_plan(plan_steps)
            .map_err(|e| WorkflowError::PlanError {
                detail: e.to_string(),
            })?;

        let compiler = PlanCompiler::new(self.compiler_config.clone());
        let compiled = compiler
            .compile(&plan)
            .map_err(|errors| WorkflowError::CompileError {
                errors: errors.iter().map(|e| e.to_string()).collect(),
            })?;

        Ok(compiled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_WORKFLOW: &str = r#"
name: daily-digest
description: Produces the daily digest report.
steps:
  - id: fetch
    capability: script.fetch-data
    step_type: run
    timeout_secs: 30
  - id: process
    capability: script.process
    step_type: run
    depends_on: [fetch]
    timeout_secs: 60
  - id: verify
    capability: script.verify-output
    step_type: verify
    depends_on: [process]
    timeout_secs: 30
"#;

    const PARALLEL_WORKFLOW: &str = r#"
name: parallel-fetch
description: Fetches from multiple sources in parallel.
steps:
  - id: fetch-news
    capability: script.fetch-news
    step_type: parallel
    parallel_group: fetch
    timeout_secs: 30
  - id: fetch-weather
    capability: script.fetch-weather
    step_type: parallel
    parallel_group: fetch
    timeout_secs: 30
  - id: combine
    capability: script.combine
    step_type: run
    depends_on: [fetch-news, fetch-weather]
    timeout_secs: 60
"#;

    const PIPED_WORKFLOW: &str = r#"
name: piped
steps:
  - id: collect
    capability: script.collect
    step_type: run
  - id: summarize
    capability: script.summarize
    step_type: run
    depends_on: [collect]
    input:
      style: terse
    input_from:
      items: steps.collect.output.items
      whole: steps.collect.output
"#;

    #[test]
    fn wiring_parses_and_reaches_the_plan_step() {
        let def = WorkflowDef::from_yaml(PIPED_WORKFLOW).unwrap();
        let summarize = &def.steps[1];
        assert_eq!(summarize.input["style"], serde_json::json!("terse"));
        assert_eq!(
            summarize.input_from["items"],
            StepRef::field("collect", ["items"])
        );
        assert_eq!(summarize.input_from["whole"], StepRef::whole("collect"));

        // And survives the hop into the plan, which is all the executor ever sees.
        let compiled = WorkflowEngine::new().compile_workflow(&def).unwrap();
        let step = compiled
            .execution_order
            .iter()
            .find(|s| s.id == "summarize")
            .unwrap();
        assert_eq!(step.input_from.len(), 2);
        assert_eq!(step.input["style"], serde_json::json!("terse"));
    }

    #[test]
    fn a_malformed_reference_is_refused_at_parse_time() {
        // `stdout` is not a readable part of a step, so this cannot be quietly kept as a
        // literal string to be discovered by a confused capability later.
        let yaml = PIPED_WORKFLOW.replace("steps.collect.output.items", "steps.collect.stdout");
        let err = WorkflowDef::from_yaml(&yaml).unwrap_err();
        assert!(
            err.to_string().contains("step output reference"),
            "got {err}"
        );
    }

    #[test]
    fn a_workflow_reading_a_step_it_does_not_depend_on_does_not_compile() {
        let yaml = PIPED_WORKFLOW.replace("    depends_on: [collect]\n", "");
        let def = WorkflowDef::from_yaml(&yaml).unwrap();
        let err = WorkflowEngine::new().compile_workflow(&def).unwrap_err();
        match err {
            WorkflowError::CompileError { errors } => assert!(
                errors.iter().any(|e| e.contains("depends_on")),
                "got {errors:?}"
            ),
            other => panic!("expected a compile error, got {other}"),
        }
    }

    #[test]
    fn parses_simple_workflow() {
        let def = WorkflowDef::from_yaml(SIMPLE_WORKFLOW).unwrap();
        assert_eq!(def.name, "daily-digest");
        assert_eq!(def.steps.len(), 3);
        assert_eq!(def.steps[0].id, "fetch");
        assert_eq!(def.steps[0].step_type, StepType::Run);
        assert_eq!(def.steps[1].depends_on, vec!["fetch"]);
        assert_eq!(def.steps[2].step_type, StepType::Verify);
    }

    #[test]
    fn parses_parallel_workflow() {
        let def = WorkflowDef::from_yaml(PARALLEL_WORKFLOW).unwrap();
        assert_eq!(def.name, "parallel-fetch");
        assert_eq!(def.steps.len(), 3);
        assert_eq!(def.steps[0].step_type, StepType::Parallel);
        assert_eq!(def.steps[0].parallel_group, Some("fetch".to_string()));
        assert_eq!(def.steps[2].depends_on, vec!["fetch-news", "fetch-weather"]);
    }

    #[test]
    fn compiles_simple_workflow() {
        let def = WorkflowDef::from_yaml(SIMPLE_WORKFLOW).unwrap();
        // Use default config (no capability/verifier checks).
        let engine = WorkflowEngine::new();
        let compiled = engine.compile_workflow(&def).unwrap();
        assert_eq!(compiled.execution_order.len(), 3);
        // Topological order: fetch, process, verify.
        assert_eq!(compiled.execution_order[0].id, "fetch");
        assert_eq!(compiled.execution_order[1].id, "process");
        assert_eq!(compiled.execution_order[2].id, "verify");
    }

    #[test]
    fn compiles_parallel_workflow() {
        let def = WorkflowDef::from_yaml(PARALLEL_WORKFLOW).unwrap();
        let engine = WorkflowEngine::new();
        let compiled = engine.compile_workflow(&def).unwrap();
        assert_eq!(compiled.execution_order.len(), 3);
        // combine must come after both fetch steps.
        assert_eq!(compiled.execution_order[2].id, "combine");
    }

    #[test]
    fn rejects_invalid_yaml() {
        let err = WorkflowDef::from_yaml("invalid: [unclosed").unwrap_err();
        assert!(matches!(err, WorkflowError::ParseError { .. }));
    }

    #[test]
    fn rejects_workflow_with_cycle() {
        let yaml = r#"
name: cyclic
description: Has a cycle.
steps:
  - id: a
    capability: cap.a
    step_type: run
    depends_on: [b]
    timeout_secs: 30
  - id: b
    capability: cap.b
    step_type: run
    depends_on: [a]
    timeout_secs: 30
"#;
        let def = WorkflowDef::from_yaml(yaml).unwrap();
        let engine = WorkflowEngine::new();
        let err = engine.compile_workflow(&def).unwrap_err();
        assert!(matches!(err, WorkflowError::CompileError { .. }));
    }

    #[test]
    fn handles_effect_steps() {
        let yaml = r#"
name: with-effect
description: Has an effect step.
steps:
  - id: compute
    capability: script.compute
    step_type: run
    timeout_secs: 30
  - id: notify
    capability: tool.send-notification
    step_type: effect
    depends_on: [compute]
    timeout_secs: 30
"#;
        let def = WorkflowDef::from_yaml(yaml).unwrap();
        assert_eq!(def.steps[1].step_type, StepType::Effect);
        let engine = WorkflowEngine::new();
        let compiled = engine.compile_workflow(&def).unwrap();
        assert_eq!(compiled.execution_order.len(), 2);
    }

    #[test]
    fn default_timeout_is_60() {
        let yaml = r#"
name: default-timeout
description: Uses default timeout.
steps:
  - id: a
    capability: cap.a
    step_type: run
"#;
        let def = WorkflowDef::from_yaml(yaml).unwrap();
        assert_eq!(def.steps[0].timeout_secs, 60);
    }

    #[test]
    fn budget_from_workflow_def() {
        let yaml = r#"
name: budgeted
description: Has a budget.
budget:
  max_steps: 4
  max_llm_calls: 2
steps:
  - id: a
    capability: cap.a
    step_type: run
    timeout_secs: 30
"#;
        let def = WorkflowDef::from_yaml(yaml).unwrap();
        let budget = def.budget.unwrap();
        assert_eq!(budget.max_steps, 4);
        assert_eq!(budget.max_llm_calls, 2);
    }
}
