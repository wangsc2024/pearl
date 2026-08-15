//! §40's dynamic form, end to end: a step returns a plan and the plan runs.
//!
//! The unit tests in `replan` prove the compiler gate. These prove the *loop* — that expanded
//! steps actually execute, in order, after the step that proposed them and before whatever was
//! waiting on it, and that a refused proposal stops the run rather than being skipped past.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pearl_core::PrecisionClass;
use pearl_executor::{
    DynamicPlanning, Executor, ExecutorConfig, StepOutcome, StepOutput, StepOutputs,
};
use pearl_plan_compiler::{CompiledPlan, CompilerConfig, PlanCompiler};
use pearl_planner::{ExecutionPlan, PlanBudget, PlanStep};

fn caps(ids: &[&str]) -> CompilerConfig {
    CompilerConfig {
        known_capabilities: ids.iter().map(|s| s.to_string()).collect(),
        ..CompilerConfig::default()
    }
}

fn compile(steps: Vec<PlanStep>, budget: PlanBudget) -> CompiledPlan {
    let plan = ExecutionPlan { steps, budget };
    PlanCompiler::default().compile(&plan).unwrap()
}

fn budget(max_steps: usize) -> PlanBudget {
    PlanBudget {
        max_steps,
        max_llm_calls: 8,
    }
}

fn step(id: &str, capability: &str) -> PlanStep {
    PlanStep::new(id, capability, PrecisionClass::P0, Duration::from_secs(5))
}

/// A step executor that returns whatever the fixture said each step should return, and records
/// the order it was asked in.
struct Fixture {
    order: Arc<Mutex<Vec<String>>>,
    seen_inputs: Arc<Mutex<BTreeMap<String, StepOutputs>>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            order: Arc::new(Mutex::new(Vec::new())),
            seen_inputs: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn executor(&self, outputs: BTreeMap<String, serde_json::Value>) -> Executor {
        self.executor_with(outputs, ExecutorConfig::default())
    }

    fn executor_with(
        &self,
        outputs: BTreeMap<String, serde_json::Value>,
        config: ExecutorConfig,
    ) -> Executor {
        let order = self.order.clone();
        let seen = self.seen_inputs.clone();
        Executor::new(
            config,
            Box::new(move |step, upstream| {
                order.lock().unwrap().push(step.id.clone());
                seen.lock()
                    .unwrap()
                    .insert(step.id.clone(), upstream.clone());
                match outputs.get(&step.id) {
                    Some(value) => StepOutcome::Success {
                        output: value.to_string(),
                        structured: Some(value.clone()),
                    },
                    None => StepOutcome::succeeded(format!("ran {}", step.id)),
                }
            }),
        )
    }

    fn order(&self) -> Vec<String> {
        self.order.lock().unwrap().clone()
    }
}

fn proposal(steps: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "steps": steps })
}

#[test]
fn a_planning_step_produces_steps_that_then_run() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![step("decide", "agent.plan").proposing_a_plan()],
        budget(8),
    );

    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.a", "script.b"])));
    let executor = fixture.executor_with(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([
                { "id": "first", "capability": "script.a" },
                { "id": "second", "capability": "script.b", "depends_on": ["first"] }
            ])),
        )]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(result.success, "{:?}", result.records);
    assert_eq!(
        fixture.order(),
        vec!["decide", "decide/first", "decide/second"],
        "the sub-plan runs after the step that proposed it, in compiled order"
    );
}

#[test]
fn a_step_waiting_on_the_planner_runs_after_the_whole_sub_plan() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![
            step("decide", "agent.plan").proposing_a_plan(),
            step("report", "script.report").after(["decide"]),
        ],
        budget(8),
    );

    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.a"])));
    let executor = fixture.executor_with(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([
                { "id": "work", "capability": "script.a" }
            ])),
        )]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(result.success, "{:?}", result.records);
    // `report` depended on `decide`, and what `decide` meant was "run this plan" — so it must
    // wait for the plan, not merely for the planner.
    assert_eq!(fixture.order(), vec!["decide", "decide/work", "report"]);
}

#[test]
fn data_flows_from_a_parent_step_into_a_dynamically_planned_one() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![
            step("collect", "script.collect"),
            step("decide", "agent.plan")
                .after(["collect"])
                .proposing_a_plan(),
        ],
        budget(8),
    );

    let mut compiler = caps(&["script.summarize"]);
    compiler.completed_steps = ["collect".to_string()].into_iter().collect();
    let config = ExecutorConfig::default().with_dynamic_planning(DynamicPlanning::within(compiler));

    let executor = fixture.executor_with(
        [
            (
                "collect".to_string(),
                serde_json::json!({ "items": ["a", "b"] }),
            ),
            (
                "decide".to_string(),
                proposal(serde_json::json!([
                    { "id": "summarize", "capability": "script.summarize",
                      "input_from": { "items": "steps.collect.output.items" } }
                ])),
            ),
        ]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);
    assert!(result.success, "{:?}", result.records);

    // The dynamically planned step could see the parent step's output, which is the point of
    // exempting already-finished steps from the dependency rule.
    let seen = fixture.seen_inputs.lock().unwrap();
    let upstream = seen.get("decide/summarize").expect("it ran");
    assert_eq!(
        upstream
            .get("collect")
            .unwrap()
            .resolve(&["items".to_string()])
            .unwrap(),
        serde_json::json!(["a", "b"])
    );
}

#[test]
fn a_refused_plan_fails_the_run_and_says_why() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![
            step("decide", "agent.plan").proposing_a_plan(),
            step("report", "script.report").after(["decide"]),
        ],
        budget(8),
    );

    // The registry has `script.a`; the planner asks for something else.
    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.a"])));
    let executor = fixture.executor_with(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([
                { "id": "sneak", "capability": "script.not-registered" }
            ])),
        )]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(!result.success);
    // Nothing from the proposal ran.
    assert_eq!(fixture.order(), vec!["decide"]);
    let refusal = result
        .records
        .iter()
        .find(|r| r.step_id == "decide:replan")
        .expect("the refusal is recorded as its own step");
    match &refusal.outcome {
        StepOutcome::Failed { error } => {
            assert!(error.contains("did not compile"), "got {error}");
            assert!(error.contains("script.not-registered"), "got {error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    // And the step that waited on the planner was skipped rather than run on nothing.
    let report = result
        .records
        .iter()
        .find(|r| r.step_id == "report")
        .expect("report is accounted for");
    assert!(matches!(report.outcome, StepOutcome::Skipped { .. }));
}

#[test]
fn a_planning_step_in_a_run_that_did_not_enable_planning_fails_rather_than_being_ignored() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![step("decide", "agent.plan").proposing_a_plan()],
        budget(8),
    );

    // Default config: dynamic planning off.
    let executor = fixture.executor(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([{ "id": "a", "capability": "script.a" }])),
        )]
        .into_iter()
        .collect(),
    );

    let result = executor.execute(&plan, None);

    assert!(
        !result.success,
        "a plan nobody would run is not a successful run"
    );
    let refusal = result
        .records
        .iter()
        .find(|r| r.step_id == "decide:replan")
        .expect("recorded");
    assert!(refusal.outcome.summary().contains("not enabled"));
}

#[test]
fn a_sub_plan_cannot_exceed_what_the_parent_budget_left() {
    let fixture = Fixture::new();
    // Budget of 3, one step already in the plan, so two remain.
    let plan = compile(
        vec![step("decide", "agent.plan").proposing_a_plan()],
        budget(3),
    );

    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.a"])));
    let executor = fixture.executor_with(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([
                { "id": "one", "capability": "script.a" },
                { "id": "two", "capability": "script.a" },
                { "id": "three", "capability": "script.a" }
            ])),
        )]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(!result.success);
    assert_eq!(fixture.order(), vec!["decide"]);
    let refusal = result
        .records
        .iter()
        .find(|r| r.step_id == "decide:replan")
        .unwrap();
    assert!(refusal.outcome.summary().contains("budget"));
}

#[test]
fn nesting_stops_at_the_declared_depth() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![step("outer", "agent.plan").proposing_a_plan()],
        budget(8),
    );

    // Depth 1: the outer planning step may plan, but what it plans may not plan again.
    let config = ExecutorConfig::default().with_dynamic_planning(
        DynamicPlanning::within(caps(&["agent.plan", "script.a"])).to_depth(1),
    );
    let executor = fixture.executor_with(
        [
            (
                "outer".to_string(),
                proposal(serde_json::json!([
                    { "id": "inner", "capability": "agent.plan", "kind": "plan" }
                ])),
            ),
            (
                "outer/inner".to_string(),
                proposal(serde_json::json!([
                    { "id": "deeper", "capability": "script.a" }
                ])),
            ),
        ]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(!result.success);
    // The inner planning step ran, but its plan was refused for depth.
    assert_eq!(fixture.order(), vec!["outer", "outer/inner"]);
    let refusal = result
        .records
        .iter()
        .find(|r| r.step_id == "outer/inner:replan")
        .expect("the depth refusal is recorded against the inner step");
    assert!(refusal.outcome.summary().contains("beyond the limit"));
}

#[test]
fn an_ordinary_step_returning_something_plan_shaped_is_left_alone() {
    let fixture = Fixture::new();
    // Same output, but the step does not have the planning role.
    let plan = compile(vec![step("decide", "script.a")], budget(8));

    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.b"])));
    let executor = fixture.executor_with(
        [(
            "decide".to_string(),
            proposal(serde_json::json!([{ "id": "x", "capability": "script.b" }])),
        )]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);

    assert!(result.success);
    assert_eq!(
        fixture.order(),
        vec!["decide"],
        "output is only instructions when the step declared that it would be"
    );
}

#[test]
fn a_sub_plan_step_can_read_another_sub_plan_step() {
    let fixture = Fixture::new();
    let plan = compile(
        vec![step("decide", "agent.plan").proposing_a_plan()],
        budget(8),
    );

    let config = ExecutorConfig::default()
        .with_dynamic_planning(DynamicPlanning::within(caps(&["script.a", "script.b"])));
    let executor = fixture.executor_with(
        [
            (
                "decide".to_string(),
                proposal(serde_json::json!([
                    { "id": "produce", "capability": "script.a" },
                    { "id": "consume", "capability": "script.b", "depends_on": ["produce"],
                      "input_from": { "n": "steps.produce.output.value" } }
                ])),
            ),
            (
                "decide/produce".to_string(),
                serde_json::json!({ "value": 7 }),
            ),
        ]
        .into_iter()
        .collect(),
        config,
    );

    let result = executor.execute(&plan, None);
    assert!(result.success, "{:?}", result.records);

    let seen = fixture.seen_inputs.lock().unwrap();
    let upstream = seen.get("decide/consume").expect("it ran");
    assert_eq!(
        upstream.get("decide/produce"),
        Some(&StepOutput::json(serde_json::json!({ "value": 7 }))),
        "the reference was renamed alongside the step it names"
    );
}
