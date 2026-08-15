//! Executor: executes compiled plans step by step with checkpoint/resume.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use pearl_plan_compiler::CompiledPlan;
use pearl_planner::PlanStep;
use serde::{Deserialize, Serialize};

/// The outcome of executing a single step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepOutcome {
    /// Step completed successfully.
    Success { output: String },
    /// Step failed with an error.
    Failed { error: String },
    /// Step was skipped (e.g., dependency failed).
    Skipped { reason: String },
}

/// Record of a completed step execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        self.records.push(record);
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
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(60),
            skip_on_dep_failure: true,
        }
    }
}

/// A function that executes a single step and returns its outcome.
pub type StepExecutor = Box<dyn Fn(&PlanStep) -> StepOutcome + Send + Sync>;

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
            step_executor: Box::new(|step| StepOutcome::Success {
                output: format!("completed: {}", step.id),
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

        for step in &plan.execution_order {
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

            // Execute the step.
            let started_at = Utc::now();
            let outcome = (self.step_executor)(step);
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
        }

        let success = failed_steps.is_empty();
        ExecutionResult {
            success,
            records: ckpt.records,
            resumed,
        }
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
        PlanStep {
            id: id.to_string(),
            capability: cap.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            precision_class: class,
            timeout: Duration::from_secs(30),
        }
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
            Box::new(move |step| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::Success {
                    output: "ok".to_string(),
                }
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
            Box::new(move |step| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::Success {
                    output: "ok".to_string(),
                }
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
            outcome: StepOutcome::Success {
                output: "previously done".to_string(),
            },
            started_at: Utc::now(),
            completed_at: Utc::now(),
        });

        let execution_order = Arc::new(Mutex::new(Vec::new()));
        let order_clone = execution_order.clone();

        let executor = Executor::new(
            ExecutorConfig::default(),
            Box::new(move |step| {
                order_clone.lock().unwrap().push(step.id.clone());
                StepOutcome::Success {
                    output: "ok".to_string(),
                }
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
            Box::new(|step| {
                if step.id == "a" {
                    StepOutcome::Failed {
                        error: "crash".to_string(),
                    }
                } else {
                    StepOutcome::Success {
                        output: "ok".to_string(),
                    }
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
            Box::new(|step| {
                if step.id == "a" {
                    StepOutcome::Success {
                        output: "ok".to_string(),
                    }
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
}
