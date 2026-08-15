//! Executor: executes compiled plans step by step with checkpoint/resume.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};
use pearl_plan_compiler::CompiledPlan;
use pearl_planner::PlanStep;
use serde::{Deserialize, Serialize};

use crate::replan::{self, DynamicPlanning, ReplanRequest};

/// What a step produced, kept for the steps that read it.
///
/// Two fields rather than one because the two are different claims. `text` is what the step
/// printed, which is always true. `structured` is what that text *meant*, which exists only
/// when the last line parsed as JSON. Collapsing them would make a step that printed the word
/// `null` indistinguishable from one that printed nothing, and would leave a downstream field
/// reference no way to say "that step did not emit an object".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepOutput {
    /// What the step printed.
    pub text: String,
    /// The last line of it, parsed, when it was JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
}

impl StepOutput {
    /// An output that is only text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
        }
    }

    /// An output that parsed as JSON.
    pub fn json(value: serde_json::Value) -> Self {
        Self {
            text: value.to_string(),
            structured: Some(value),
        }
    }

    /// The value a whole-output reference resolves to.
    ///
    /// The structured form when there is one, so a downstream step receives an object rather
    /// than a string it would have to parse itself.
    pub fn as_value(&self) -> serde_json::Value {
        match &self.structured {
            Some(value) => value.clone(),
            None => serde_json::Value::String(self.text.clone()),
        }
    }

    /// Walks a path into the structured output.
    ///
    /// `Err` carries what went wrong in the caller's terms, because "the step produced no
    /// JSON" and "the JSON has no such key" need different fixes.
    pub fn resolve(&self, path: &[String]) -> Result<serde_json::Value, String> {
        if path.is_empty() {
            return Ok(self.as_value());
        }
        let Some(root) = &self.structured else {
            return Err(format!(
                "it printed no JSON on its last line, so there is nothing to index into (it printed: {})",
                first_line(&self.text)
            ));
        };
        let mut current = root;
        for (depth, segment) in path.iter().enumerate() {
            current = match current.get(segment) {
                Some(next) => next,
                None => {
                    let so_far = path[..depth].join(".");
                    let where_ = if so_far.is_empty() {
                        "its output".to_string()
                    } else {
                        format!("its output.{so_far}")
                    };
                    return Err(format!("{where_} has no '{segment}'"));
                }
            };
        }
        Ok(current.clone())
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("nothing")
        .chars()
        .take(120)
        .collect()
}

/// Every completed step's output, by step id.
pub type StepOutputs = BTreeMap<String, StepOutput>;

/// The outcome of executing a single step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StepOutcome {
    /// Step completed successfully.
    Success {
        /// What the step printed.
        output: String,
        /// What it printed as JSON, when the last line parsed — what successors read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured: Option<serde_json::Value>,
    },
    /// Step failed with an error.
    Failed { error: String },
    /// Step was skipped (e.g., dependency failed).
    Skipped { reason: String },
}

impl StepOutcome {
    /// A success carrying only text.
    pub fn succeeded(output: impl Into<String>) -> Self {
        Self::Success {
            output: output.into(),
            structured: None,
        }
    }

    /// The output a successor could read, or `None` for a step that did not succeed.
    pub fn output(&self) -> Option<StepOutput> {
        match self {
            Self::Success { output, structured } => Some(StepOutput {
                text: output.clone(),
                structured: structured.clone(),
            }),
            _ => None,
        }
    }

    /// One line saying what happened, for a projection row a human will read.
    pub fn summary(&self) -> String {
        match self {
            Self::Success { output, .. } => first_line(output),
            Self::Failed { error } => first_line(error),
            Self::Skipped { reason } => first_line(reason),
        }
    }
}

/// Record of a completed step execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    /// The step id.
    pub step_id: String,
    /// The outcome of execution.
    pub outcome: StepOutcome,
    /// When the step started.
    pub started_at: DateTime<Utc>,
    /// When the step completed.
    pub completed_at: DateTime<Utc>,
}

/// A checkpoint representing progress through plan execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Step ids that have been completed.
    pub completed_steps: HashSet<String>,
    /// Records of completed steps in order.
    pub records: Vec<StepRecord>,
    /// What each completed step produced, for the steps that read it.
    ///
    /// Part of the checkpoint rather than a separate accumulator because §41 says only a
    /// committed checkpoint licenses the next step — and if the next step consumes its
    /// predecessor's output, then that output is part of what has to have been committed.
    /// A resumed run that had the step ids but not the outputs could order the work
    /// correctly and still feed it nothing.
    #[serde(default)]
    pub outputs: StepOutputs,
}

impl Checkpoint {
    /// Creates a new empty checkpoint.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a step has already been completed (for resume).
    pub fn is_completed(&self, step_id: &str) -> bool {
        self.completed_steps.contains(step_id)
    }

    /// Record a completed step.
    pub fn record(&mut self, record: StepRecord) {
        self.completed_steps.insert(record.step_id.clone());
        if let Some(output) = record.outcome.output() {
            self.outputs.insert(record.step_id.clone(), output);
        }
        self.records.push(record);
    }

    /// Restores what a step produced, when resuming from durable storage.
    pub fn restore_output(&mut self, step_id: impl Into<String>, output: StepOutput) {
        self.outputs.insert(step_id.into(), output);
    }
}

/// The result of executing a full plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Whether all steps completed successfully.
    pub success: bool,
    /// Records of all executed steps.
    pub records: Vec<StepRecord>,
    /// Whether this was a resumed execution.
    pub resumed: bool,
}

impl ExecutionResult {
    /// Returns the number of successful steps.
    pub fn success_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| matches!(r.outcome, StepOutcome::Success { .. }))
            .count()
    }

    /// Returns the number of failed steps.
    pub fn failed_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| matches!(r.outcome, StepOutcome::Failed { .. }))
            .count()
    }
}

/// Configuration for the executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Default timeout for steps that do not declare one.
    pub default_timeout: Duration,
    /// Whether to skip steps whose dependencies failed.
    pub skip_on_dep_failure: bool,
    /// What a `plan`-role step is allowed to produce, or `None` to refuse them — §40.
    ///
    /// `None` by default. A run that did not ask for dynamic planning does not acquire it
    /// because a plan contained a planning step: which capabilities a sub-plan may draw from
    /// is a decision, and it has to be made rather than inherited.
    pub dynamic: Option<DynamicPlanning>,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            skip_on_dep_failure: true,
            dynamic: None,
        }
    }
}

impl ExecutorConfig {
    /// Allows planning steps, within the given limits — §40.
    pub fn with_dynamic_planning(mut self, planning: DynamicPlanning) -> Self {
        self.dynamic = Some(planning);
        self
    }
}

/// A function that executes a single step and returns its outcome.
///
/// Takes the outputs of the steps that already ran, because a step's input can come from its
/// predecessor. The earlier signature was `Fn(&PlanStep)`, which made step-to-step data flow
/// not merely unimplemented but unrepresentable: the closure had no way to see what came
/// before it, so every step could only ever receive the plan-wide payload.
pub type StepExecutor = Box<dyn Fn(&PlanStep, &StepOutputs) -> StepOutcome + Send + Sync>;

/// The Executor runs compiled plans step by step.
pub struct Executor {
    config: ExecutorConfig,
    step_executor: StepExecutor,
}

impl Executor {
    /// Creates a new executor with the given configuration and step executor.
    pub fn new(config: ExecutorConfig, step_executor: StepExecutor) -> Self {
        Self {
            config,
            step_executor,
        }
    }

    /// Creates an executor that succeeds all steps (for testing).
    pub fn always_succeed() -> Self {
        Self {
            config: ExecutorConfig::default(),
            step_executor: Box::new(|step, _upstream| {
                StepOutcome::succeeded(format!("completed: {}", step.id))
            }),
        }
    }

    /// Executes a compiled plan, optionally resuming from a checkpoint.
    ///
    /// Steps are executed in topological order. If `skip_on_dep_failure` is true,
    /// a step whose dependency failed will be skipped. After each step, the
    /// checkpoint is updated.
    pub fn execute(&self, plan: &CompiledPlan, checkpoint: Option<Checkpoint>) -> ExecutionResult {
        self.execute_with_sink(plan, checkpoint, &mut NullSink)
    }

    /// Executes a plan, committing each step's checkpoint through `sink` before continuing.
    ///
    /// §41: only a *committed* checkpoint licenses the next step. An in-memory checkpoint
    /// satisfies the shape of the requirement while losing everything on the crash it exists
    /// to survive, so durability is the caller's to provide and the executor's to demand.
    ///
    /// A sink that fails stops the plan. Continuing would mean executing a step whose
    /// predecessor's completion was not recorded — on resume that step would run twice.
    pub fn execute_with_sink(
        &self,
        plan: &CompiledPlan,
        checkpoint: Option<Checkpoint>,
        sink: &mut dyn CheckpointSink,
    ) -> ExecutionResult {
        let mut ckpt = checkpoint.unwrap_or_default();
        let resumed = !ckpt.completed_steps.is_empty();
        let mut failed_steps: HashSet<String> = HashSet::new();

        // Identify steps already failed from checkpoint.
        for record in &ckpt.records {
            if matches!(record.outcome, StepOutcome::Failed { .. }) {
                failed_steps.insert(record.step_id.clone());
            }
        }

        // A queue rather than a `for` over `execution_order`, because a planning step inserts
        // work immediately after itself (§40). Its depth travels with each step so a sub-plan
        // knows how deeply nested it is.
        let mut pending: VecDeque<(PlanStep, u32)> = plan
            .execution_order
            .iter()
            .cloned()
            .map(|step| (step, 0))
            .collect();
        let budget = plan.source_plan.budget.clone();
        // Steps that exist because a planner asked for them count against the same budget as
        // the ones in the file: replanning must not be a way to buy more work.
        let mut step_allowance = budget.max_steps.saturating_sub(plan.execution_order.len());

        while let Some((step, depth)) = pending.pop_front() {
            // Skip already completed steps (resume support).
            if ckpt.is_completed(&step.id) {
                continue;
            }

            // Check if any dependency failed.
            if self.config.skip_on_dep_failure {
                let dep_failed = step.depends_on.iter().any(|dep| failed_steps.contains(dep));
                if dep_failed {
                    let record = StepRecord {
                        step_id: step.id.clone(),
                        outcome: StepOutcome::Skipped {
                            reason: "dependency failed".to_string(),
                        },
                        started_at: Utc::now(),
                        completed_at: Utc::now(),
                    };
                    failed_steps.insert(step.id.clone());
                    ckpt.record(record.clone());
                    if let Err(detail) = sink.commit(&record, &ckpt) {
                        return Self::halted(ckpt, resumed, &record.step_id, &detail);
                    }
                    continue;
                }
            }

            // Execute the step, giving it what its predecessors produced.
            let started_at = Utc::now();
            let outcome = (self.step_executor)(&step, &ckpt.outputs);
            let completed_at = Utc::now();

            if matches!(outcome, StepOutcome::Failed { .. }) {
                failed_steps.insert(step.id.clone());
            }

            let record = StepRecord {
                step_id: step.id.clone(),
                outcome,
                started_at,
                completed_at,
            };
            ckpt.record(record.clone());
            if let Err(detail) = sink.commit(&record, &ckpt) {
                return Self::halted(ckpt, resumed, &record.step_id, &detail);
            }

            // §40: a planning step's output is a plan, so compile it and run what comes out.
            if step.proposes_a_plan() {
                if let Some(output) = record.outcome.output() {
                    match self.expand(&step, depth, output, &ckpt, step_allowance, &budget) {
                        Ok(sub_steps) => {
                            step_allowance = step_allowance.saturating_sub(sub_steps.len());
                            // Pushed to the front, in reverse, so they run in compiled order
                            // and before any parent step that waited on the planning step.
                            for sub in sub_steps.into_iter().rev() {
                                pending.push_front((sub, depth + 1));
                            }
                        }
                        Err(detail) => {
                            // The step ran and printed something; what it printed is not a
                            // plan this run may execute. Recorded as its own failure so the
                            // history distinguishes "the planner crashed" from "the planner
                            // produced something we refused", and the planning step is marked
                            // failed so nothing that depended on it proceeds.
                            let refusal = StepRecord {
                                step_id: format!("{}:replan", step.id),
                                outcome: StepOutcome::Failed { error: detail },
                                started_at: Utc::now(),
                                completed_at: Utc::now(),
                            };
                            failed_steps.insert(step.id.clone());
                            ckpt.record(refusal.clone());
                            if let Err(detail) = sink.commit(&refusal, &ckpt) {
                                return Self::halted(ckpt, resumed, &refusal.step_id, &detail);
                            }
                        }
                    }
                }
            }
        }

        let success = failed_steps.is_empty();
        ExecutionResult {
            success,
            records: ckpt.records,
            resumed,
        }
    }

    /// Compiles what a planning step returned into steps this run may execute — §40.
    fn expand(
        &self,
        step: &PlanStep,
        depth: u32,
        output: StepOutput,
        ckpt: &Checkpoint,
        steps_remaining: usize,
        budget: &pearl_planner::PlanBudget,
    ) -> Result<Vec<PlanStep>, String> {
        let request = ReplanRequest {
            origin: step.id.clone(),
            depth: depth + 1,
            output,
            completed: ckpt.completed_steps.clone(),
            steps_remaining,
        };
        replan::expand(&request, self.config.dynamic.as_ref(), budget).map_err(|e| e.to_string())
    }

    /// Stops the plan because progress could not be recorded.
    fn halted(mut ckpt: Checkpoint, resumed: bool, step_id: &str, detail: &str) -> ExecutionResult {
        ckpt.records.push(StepRecord {
            step_id: format!("{step_id}:checkpoint"),
            outcome: StepOutcome::Failed {
                error: format!("checkpoint could not be committed: {detail}"),
            },
            started_at: Utc::now(),
            completed_at: Utc::now(),
        });
        ExecutionResult {
            success: false,
            records: ckpt.records,
            resumed,
        }
    }
}

/// Somewhere durable to record progress after each step — §41.
pub trait CheckpointSink {
    /// Commits one step's completion. Returning `Err` halts the plan.
    fn commit(&mut self, record: &StepRecord, checkpoint: &Checkpoint) -> Result<(), String>;
}

/// A sink that records nothing.
///
/// The default, so that a caller with no store still gets ordering and resume semantics —
/// just not across a crash.
pub struct NullSink;

impl CheckpointSink for NullSink {
    fn commit(&mut self, _record: &StepRecord, _checkpoint: &Checkpoint) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::PrecisionClass;
    use pearl_plan_compiler::PlanCompiler;
    use pearl_planner::{ExecutionPlan, PlanBudget, PlanStep};
    use std::sync::{Arc, Mutex};

    fn step(id: &str, cap: &str, deps: &[&str], class: PrecisionClass) -> PlanStep {
        PlanStep::new(id, cap, class, Duration::from_secs(30)).after(deps.to_vec())
    }

    fn compile_plan(steps: Vec<PlanStep>) -> CompiledPlan {
        let plan = ExecutionPlan {
            steps,
            budget: PlanBudget {
                max_steps: 16,
                max_llm_calls: 32,
            },
        };
        let compiler = PlanCompiler::default();
        compiler.compile(&plan).unwrap()
    }

    #[test]
    fn executes_steps_in_order() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
            step("c", "cap.c", &["b"], PrecisionClass::P3),
        ]);

        let execution_order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = execution_order.clone();

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |step, _| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::succeeded("ok")
            }),
        );

        let result = executor.execute(&compiled, None);
        assert!(result.success);
        assert_eq!(result.success_count(), 3);
        let order = execution_order.lock().unwrap();
        assert_eq!(*order, vec!["a", "b", "c"]);
    }

    #[test]
    fn respects_dependencies_in_diamond() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
            step("c", "cap.c", &["a"], PrecisionClass::P3),
            step("d", "cap.d", &["b", "c"], PrecisionClass::P3),
        ]);

        let execution_order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = execution_order.clone();

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |step, _| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::succeeded("ok")
            }),
        );

        let result = executor.execute(&compiled, None);
        assert!(result.success);
        let order = execution_order.lock().unwrap();
        // a must come before b and c; d must come after both.
        let pos_a = order.iter().position(|s| s == "a").unwrap();
        let pos_b = order.iter().position(|s| s == "b").unwrap();
        let pos_c = order.iter().position(|s| s == "c").unwrap();
        let pos_d = order.iter().position(|s| s == "d").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_a < pos_c);
        assert!(pos_b < pos_d);
        assert!(pos_c < pos_d);
    }

    #[test]
    fn checkpoint_resume_skips_completed_steps() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
            step("c", "cap.c", &["b"], PrecisionClass::P3),
        ]);

        // Simulate a checkpoint where step "a" is already done.
        let mut ckpt = Checkpoint::new();
        ckpt.record(StepRecord {
            step_id: "a".to_string(),
            outcome: StepOutcome::succeeded("previously done"),
            started_at: Utc::now(),
            completed_at: Utc::now(),
        });

        let execution_order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = execution_order.clone();

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |step, _| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::succeeded("ok")
            }),
        );

        let result = executor.execute(&compiled, Some(ckpt));
        assert!(result.success);
        assert!(result.resumed);
        let order = execution_order.lock().unwrap();
        // "a" should NOT be re-executed.
        assert!(!order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
        assert!(order.contains(&"c".to_string()));
    }

    #[test]
    fn skips_steps_on_dependency_failure() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
        ]);

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(|step, _| {
                if step.id == "a" {
                    StepOutcome::Failed {
                        error: "crash".to_string(),
                    }
                } else {
                    StepOutcome::succeeded("ok")
                }
            }),
        );

        let result = executor.execute(&compiled, None);
        assert!(!result.success);
        assert_eq!(result.records.len(), 2);
        assert!(matches!(
            result.records[0].outcome,
            StepOutcome::Failed { .. }
        ));
        assert!(matches!(
            result.records[1].outcome,
            StepOutcome::Skipped { .. }
        ));
    }

    #[test]
    fn empty_plan_succeeds() {
        let compiled = compile_plan(vec![]);
        let executor = Executor::always_succeed();
        let result = executor.execute(&compiled, None);
        assert!(result.success);
        assert_eq!(result.records.len(), 0);
    }

    #[test]
    fn always_succeed_executor() {
        let compiled = compile_plan(vec![step("x", "cap.x", &[], PrecisionClass::P3)]);
        let executor = Executor::always_succeed();
        let result = executor.execute(&compiled, None);
        assert!(result.success);
        assert_eq!(result.success_count(), 1);
    }

    #[test]
    fn execution_result_counts() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &[], PrecisionClass::P3),
        ]);

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(|step, _| {
                if step.id == "a" {
                    StepOutcome::succeeded("ok")
                } else {
                    StepOutcome::Failed {
                        error: "bad".to_string(),
                    }
                }
            }),
        );

        let result = executor.execute(&compiled, None);
        assert!(!result.success);
        assert_eq!(result.success_count(), 1);
        assert_eq!(result.failed_count(), 1);
    }

    // ---------------------------------------------------------------- outputs

    #[test]
    fn a_step_sees_what_its_predecessors_produced() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
        ]);

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |step, upstream| {
                seen_clone.lock().unwrap().push((
                    step.id.clone(),
                    upstream.keys().cloned().collect::<Vec<_>>(),
                ));
                StepOutcome::Success {
                    output: format!("{{\"from\":\"{}\"}}", step.id),
                    structured: Some(serde_json::json!({ "from": step.id })),
                }
            }),
        );

        let result = executor.execute(&compiled, None);
        assert!(result.success);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], ("a".to_string(), vec![]));
        assert_eq!(seen[1], ("b".to_string(), vec!["a".to_string()]));
    }

    #[test]
    fn only_successful_steps_contribute_an_output() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &[], PrecisionClass::P3),
        ]);
        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(|step, _| {
                if step.id == "a" {
                    StepOutcome::Failed {
                        error: "no".to_string(),
                    }
                } else {
                    StepOutcome::succeeded("yes")
                }
            }),
        );
        executor.execute(&compiled, None);

        // Verified through the checkpoint, which is what a successor would read from.
        let mut ckpt = Checkpoint::new();
        ckpt.record(StepRecord {
            step_id: "a".to_string(),
            outcome: StepOutcome::Failed {
                error: "no".to_string(),
            },
            started_at: Utc::now(),
            completed_at: Utc::now(),
        });
        assert!(
            !ckpt.outputs.contains_key("a"),
            "a failed step has no output to offer"
        );
        assert!(ckpt.is_completed("a"), "but it is still accounted for");
    }

    #[test]
    fn a_restored_output_is_visible_to_a_resumed_step() {
        let compiled = compile_plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P3),
            step("b", "cap.b", &["a"], PrecisionClass::P3),
        ]);

        // What the CLI does on `--resume`: the step ids *and* what they produced.
        let mut ckpt = Checkpoint::new();
        ckpt.record(StepRecord {
            step_id: "a".to_string(),
            outcome: StepOutcome::succeeded("ignored"),
            started_at: Utc::now(),
            completed_at: Utc::now(),
        });
        ckpt.restore_output(
            "a",
            StepOutput::json(serde_json::json!({ "items": [1, 2] })),
        );

        let seen = Arc::new(Mutex::new(None));
        let seen_clone = seen.clone();
        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |_step, upstream| {
                *seen_clone.lock().unwrap() = upstream.get("a").cloned();
                StepOutcome::succeeded("ok")
            }),
        );
        executor.execute(&compiled, Some(ckpt));

        let restored = seen.lock().unwrap().clone().expect("b ran");
        assert_eq!(
            restored.resolve(&["items".to_string()]).unwrap(),
            serde_json::json!([1, 2]),
            "a resumed run must feed the next step, not just order it"
        );
    }

    #[test]
    fn a_whole_output_reference_prefers_the_structured_form() {
        let json = StepOutput::json(serde_json::json!({ "score": 8.28 }));
        assert_eq!(
            json.resolve(&[]).unwrap(),
            serde_json::json!({"score": 8.28})
        );

        // Plain text is still a value, just a string one.
        let text = StepOutput::text("all good");
        assert_eq!(text.resolve(&[]).unwrap(), serde_json::json!("all good"));
    }

    #[test]
    fn a_path_into_a_step_that_printed_no_json_says_so() {
        let text = StepOutput::text("all good");
        let err = text.resolve(&["score".to_string()]).unwrap_err();
        assert!(err.contains("printed no JSON"), "got {err}");
        assert!(
            err.contains("all good"),
            "it should quote what it did print"
        );
    }

    #[test]
    fn a_missing_field_names_the_path_that_failed() {
        let output = StepOutput::json(serde_json::json!({ "breakdown": { "confidence": 1.0 } }));
        assert_eq!(
            output
                .resolve(&["breakdown".to_string(), "confidence".to_string()])
                .unwrap(),
            serde_json::json!(1.0)
        );
        let err = output
            .resolve(&["breakdown".to_string(), "absent".to_string()])
            .unwrap_err();
        assert!(
            err.contains("output.breakdown has no 'absent'"),
            "got {err}"
        );
    }
}
