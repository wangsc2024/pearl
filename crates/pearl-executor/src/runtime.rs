//! Step execution against real capabilities — §31, Article 9.
//!
//! The executor's own loop is about *ordering*: topological sequence, dependency skipping,
//! resume. This module is what makes a step actually happen. Without it the executor could
//! report a plan as executed while nothing ran, because its step function was a closure the
//! caller supplied — convenient for testing the ordering, useless as an execution engine.
//!
//! Two properties the injected-closure version could not have:
//!
//! - **The declared timeout is enforced.** `ExecutorConfig::default_timeout` existed but
//!   nothing read it, so a step's `timeout` was decoration. Here it becomes the deadline the
//!   process supervisor enforces.
//! - **A missing capability fails the step rather than the plan.** A plan that references a
//!   capability which has since been removed produces a failed step with an explanation, not
//!   a panic and not a silent success.

use std::path::PathBuf;

use chrono::TimeDelta;
use pearl_capabilities::CapabilityRegistry;
use pearl_core::Clock;
use pearl_planner::PlanStep;
use pearl_process_supervisor::ProcessSupervisor;
use pearl_runtime::{
    agent_adapter_for, RuntimeAdapter, RuntimeExitStatus, ScriptRuntimeAdapter, ScriptSpec,
};

use crate::executor::{StepExecutor, StepOutcome};

/// Executes plan steps by dispatching to the capability each one names.
pub struct RuntimeStepExecutor<S: ProcessSupervisor, C: Clock> {
    registry: CapabilityRegistry,
    supervisor: S,
    clock: C,
    working_dir: Option<PathBuf>,
    payload: serde_json::Value,
}

impl<S: ProcessSupervisor, C: Clock> RuntimeStepExecutor<S, C> {
    pub fn new(registry: CapabilityRegistry, supervisor: S, clock: C) -> Self {
        Self {
            registry,
            supervisor,
            clock,
            working_dir: None,
            payload: serde_json::Value::Null,
        }
    }

    /// Sets the working directory for spawned capabilities.
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Sets the JSON payload every step receives on `PEARL_INPUT`.
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Runs one step.
    pub fn execute_step(&self, step: &PlanStep) -> StepOutcome {
        let Some(capability) = self.registry.find_by_id(&step.capability) else {
            return StepOutcome::Failed {
                error: format!(
                    "step '{}' names capability '{}', which is not registered",
                    step.id, step.capability
                ),
            };
        };

        if !capability.manifest.runs_on_this_platform() {
            return StepOutcome::Failed {
                error: format!(
                    "capability '{}' does not declare support for this platform",
                    step.capability
                ),
            };
        }

        let entrypoint = match capability.resolve_entrypoint() {
            Ok(resolved) => resolved,
            Err(e) => {
                return StepOutcome::Failed {
                    error: format!("capability '{}' cannot be executed: {e}", step.capability),
                }
            }
        };

        let runtime = capability.manifest.execution.runtime;
        let spec = ScriptSpec {
            runtime,
            entrypoint: entrypoint.target,
            args: entrypoint.args,
            env: Default::default(),
            cwd: self.working_dir.clone(),
            // The step's declared timeout, not a global default: a plan that says a step
            // gets five seconds means it.
            timeout: TimeDelta::from_std(step.timeout)
                .unwrap_or_else(|_| TimeDelta::try_seconds(60).expect("valid")),
            input_payload: Some(self.step_payload(step)),
        };

        let executed = if runtime.is_mechanical() {
            ScriptRuntimeAdapter::new(&self.supervisor).execute(&spec, &self.clock)
        } else {
            match agent_adapter_for(runtime) {
                Some(adapter) => adapter.execute(&spec, &self.clock),
                None => {
                    return StepOutcome::Failed {
                        error: format!(
                            "capability '{}' needs runtime '{}', which is not configured here",
                            step.capability,
                            runtime.as_str()
                        ),
                    }
                }
            }
        };

        match executed {
            Ok(result) => match result.exit_status {
                RuntimeExitStatus::Exited { code: 0 } => StepOutcome::Success {
                    output: result
                        .structured_output
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| result.stdout.trim().to_string()),
                },
                RuntimeExitStatus::TimedOut => StepOutcome::Failed {
                    error: format!(
                        "step '{}' exceeded its {}s timeout",
                        step.id,
                        step.timeout.as_secs()
                    ),
                },
                other => StepOutcome::Failed {
                    error: format!(
                        "step '{}' ended {other:?}: {}",
                        step.id,
                        first_line(&result.stderr)
                    ),
                },
            },
            Err(e) => StepOutcome::Failed {
                error: format!("step '{}' could not run: {e}", step.id),
            },
        }
    }

    /// The payload a step receives: the plan-wide payload plus its own identity.
    fn step_payload(&self, step: &PlanStep) -> serde_json::Value {
        let mut map = match self.payload.clone() {
            serde_json::Value::Object(map) => map,
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("input".to_string(), other);
                map
            }
        };
        map.insert("step_id".to_string(), step.id.clone().into());
        map.insert("capability".to_string(), step.capability.clone().into());
        map.insert(
            "precision_class".to_string(),
            step.precision_class.as_str().into(),
        );
        serde_json::Value::Object(map)
    }
}

/// Wraps a [`RuntimeStepExecutor`] in the closure [`crate::Executor`] expects.
pub fn step_executor_fn<S, C>(runner: RuntimeStepExecutor<S, C>) -> StepExecutor
where
    S: ProcessSupervisor + Send + Sync + 'static,
    C: Clock + Send + Sync + 'static,
{
    Box::new(move |step| runner.execute_step(step))
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no diagnostics")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::{PrecisionClass, SystemClock};
    use pearl_process_supervisor::PlatformSupervisor;
    use std::time::Duration;
    use tempfile::TempDir;

    fn python_available() -> bool {
        pearl_runtime::programs::is_available(&pearl_runtime::programs::python())
    }

    /// Writes a capability whose script body is given, and returns its registry.
    fn registry_with(id: &str, body: &str) -> (TempDir, CapabilityRegistry) {
        let dir = TempDir::new().unwrap();
        let script = format!("{}.py", id.replace('.', "_"));
        std::fs::write(dir.path().join(&script), body).unwrap();
        std::fs::write(
            dir.path().join(format!("{id}.yaml")),
            format!(
                "id: {id}\nversion: 1\ntype: script\ndescription: fixture\n\
                 execution:\n  kind: script\n  runtime: python\n  entrypoint:\n    script: {script}\n\
                 quality:\n  deterministic: true\nrisk:\n  side_effect: false\n\
                 platform:\n  windows: true\n  linux: true\ntimeout_seconds: 30\n"
            ),
        )
        .unwrap();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();
        (dir, registry)
    }

    fn step(id: &str, capability: &str, timeout: Duration) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            capability: capability.to_string(),
            depends_on: Vec::new(),
            precision_class: PrecisionClass::P0,
            timeout,
        }
    }

    #[test]
    fn a_step_runs_its_capability_and_captures_the_output() {
        if !python_available() {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let (_dir, registry) = registry_with(
            "script.echo",
            "import json, os\npayload = json.loads(os.environ['PEARL_INPUT'])\n\
             print(json.dumps({\"saw_step\": payload['step_id']}))\n",
        );
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock);

        let outcome = runner.execute_step(&step("first", "script.echo", Duration::from_secs(30)));
        match outcome {
            StepOutcome::Success { output } => {
                // The step's own identity reaches the capability, so a shared script can
                // tell which step invoked it.
                assert!(output.contains("first"), "got {output}");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn a_nonzero_exit_fails_the_step_with_its_diagnostics() {
        if !python_available() {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let (_dir, registry) = registry_with(
            "script.fails",
            "import sys\nsys.stderr.write('deliberate\\n')\nsys.exit(3)\n",
        );
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock);

        match runner.execute_step(&step("s", "script.fails", Duration::from_secs(30))) {
            StepOutcome::Failed { error } => assert!(error.contains("deliberate"), "got {error}"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn the_steps_declared_timeout_is_enforced() {
        if !python_available() {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let (_dir, registry) = registry_with("script.slow", "import time\ntime.sleep(60)\n");
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock);

        // One second, from the step rather than from the manifest's 30.
        match runner.execute_step(&step("s", "script.slow", Duration::from_secs(1))) {
            StepOutcome::Failed { error } => {
                assert!(error.contains("timeout"), "got {error}");
            }
            other => panic!("expected a timeout failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unregistered_capability_fails_the_step_rather_than_the_plan() {
        let (_dir, registry) = registry_with("script.present", "print('{}')\n");
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock);

        match runner.execute_step(&step("s", "script.absent", Duration::from_secs(5))) {
            StepOutcome::Failed { error } => {
                assert!(error.contains("not registered"), "got {error}")
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn a_capability_with_no_entrypoint_cannot_be_executed() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("script.declared-only.yaml"),
            "id: script.declared-only\nversion: 1\ntype: script\ndescription: fixture\n\
             execution:\n  kind: script\n  runtime: python\n\
             quality:\n  deterministic: true\nrisk:\n  side_effect: false\n\
             platform:\n  windows: true\n  linux: true\ntimeout_seconds: 5\n",
        )
        .unwrap();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock);

        match runner.execute_step(&step("s", "script.declared-only", Duration::from_secs(5))) {
            StepOutcome::Failed { error } => {
                assert!(error.contains("cannot be executed"), "got {error}")
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn the_plan_payload_is_merged_with_the_step_identity() {
        let (_dir, registry) = registry_with("script.x", "print('{}')\n");
        let runner = RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock)
            .with_payload(serde_json::json!({ "task_id": "t1", "step_id": "ignored" }));

        let payload = runner.step_payload(&step("real", "script.x", Duration::from_secs(1)));
        assert_eq!(payload["task_id"], "t1");
        // The step's own identity wins: it is a fact about this invocation, not a parameter.
        assert_eq!(payload["step_id"], "real");
        assert_eq!(payload["capability"], "script.x");
        assert_eq!(payload["precision_class"], "p0");
    }
}
