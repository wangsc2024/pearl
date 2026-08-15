//! Plan compiler: validates a plan before execution.

use std::collections::{HashMap, HashSet, VecDeque};

use pearl_planner::{ExecutionPlan, PlanStep};

/// A compiled plan that has passed all validation checks.
///
/// Only a `CompiledPlan` can be handed to the executor, ensuring that execution
/// never starts on an invalid plan.
#[derive(Debug, Clone)]
pub struct CompiledPlan {
    /// Steps in topological order (ready for sequential execution respecting deps).
    pub execution_order: Vec<PlanStep>,
    /// The original plan that was compiled.
    pub source_plan: ExecutionPlan,
}

/// Errors that can occur during plan compilation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    /// The dependency graph contains a cycle.
    #[error("cyclic dependency detected involving steps: {involved:?}")]
    CyclicDependency { involved: Vec<String> },

    /// A step references a capability not in the registry.
    #[error("step '{step}' references unknown capability '{capability}'")]
    MissingCapability { step: String, capability: String },

    /// An exactness (P0/P1) step lacks a verifier.
    #[error("step '{step}' with precision class {class} requires a verifier")]
    MissingVerifier { step: String, class: String },

    /// The plan exceeds the declared budget.
    #[error("plan exceeds budget: {detail}")]
    BudgetExceeded { detail: String },

    /// A step does not declare a timeout.
    #[error("step '{step}' is missing a timeout declaration")]
    MissingTimeout { step: String },

    /// A step reads a step it cannot read.
    ///
    /// Data flow is a dependency, so it is checked with the same seriousness as one. A step
    /// whose input comes from somewhere the ordering does not guarantee has run would receive
    /// nothing, and "nothing" is indistinguishable from a legitimately empty result.
    #[error("step '{step}' takes '{key}' from '{reference}', but {detail}")]
    UnreadableInput {
        step: String,
        key: String,
        reference: String,
        detail: String,
    },

    /// A payload key is declared as both a literal and a reference.
    #[error("step '{step}' declares '{key}' in both input and input_from; one of them would be silently discarded")]
    ConflictingInput { step: String, key: String },
}

/// A set of known capabilities (by id) for validation.
pub type CapabilitySet = HashSet<String>;

/// A set of step ids that have verifiers attached.
pub type VerifierSet = HashSet<String>;

/// Configuration for plan compilation.
#[derive(Debug, Clone, Default)]
pub struct CompilerConfig {
    /// Known capabilities in the registry.
    pub known_capabilities: CapabilitySet,
    /// Steps that have verifiers attached (for exactness checks).
    pub verified_steps: VerifierSet,
    /// Steps that already finished outside this plan, and whose output can therefore be read
    /// without a dependency edge — §40.
    ///
    /// A dynamic sub-plan is compiled while the run that asked for it is in progress, so the
    /// steps that produced its inputs are not in it. They still cannot be depended *on*: they
    /// are already done, so ordering is not in question. Everything else about them is checked
    /// exactly as usual.
    pub completed_steps: HashSet<String>,
}

/// The Plan Compiler validates a plan and produces a `CompiledPlan` on success.
#[derive(Debug, Clone)]
pub struct PlanCompiler {
    config: CompilerConfig,
}

impl Default for PlanCompiler {
    fn default() -> Self {
        Self::new(CompilerConfig::default())
    }
}

impl PlanCompiler {
    /// Creates a new compiler with the given configuration.
    pub fn new(config: CompilerConfig) -> Self {
        Self { config }
    }

    /// Compiles a plan, returning either a `CompiledPlan` or a list of errors.
    ///
    /// Checks performed:
    /// 1. DAG is acyclic (topological sort)
    /// 2. All capabilities exist in registry
    /// 3. Exactness tasks have verifiers
    /// 4. Budget not exceeded
    /// 5. Timeout declared on every step
    pub fn compile(&self, plan: &ExecutionPlan) -> Result<CompiledPlan, Vec<CompileError>> {
        let mut errors = Vec::new();

        // Check timeouts.
        for step in &plan.steps {
            if step.timeout.is_zero() {
                errors.push(CompileError::MissingTimeout {
                    step: step.id.clone(),
                });
            }
        }

        // Check capabilities exist.
        if !self.config.known_capabilities.is_empty() {
            for step in &plan.steps {
                if !self.config.known_capabilities.contains(&step.capability) {
                    errors.push(CompileError::MissingCapability {
                        step: step.id.clone(),
                        capability: step.capability.clone(),
                    });
                }
            }
        }

        // §30: a step that demands exactness must have a verifier. The trigger is the step's
        // own declaration, not its precision class — and it is checked unconditionally, because
        // an exactness demand with nothing to satisfy it is a violation whether or not the
        // caller supplied a registry.
        for step in &plan.steps {
            if step.exactness_required && !self.config.verified_steps.contains(&step.id) {
                errors.push(CompileError::MissingVerifier {
                    step: step.id.clone(),
                    class: format!("{:?}", step.precision_class),
                });
            }
        }

        // Data flow: every reference must name a step this one waits for, or one that has
        // already finished.
        errors.extend(check_input_wiring(
            &plan.steps,
            &self.config.completed_steps,
        ));

        // Check budget.
        if plan.steps.len() > plan.budget.max_steps {
            errors.push(CompileError::BudgetExceeded {
                detail: format!(
                    "{} steps exceed max_steps limit of {}",
                    plan.steps.len(),
                    plan.budget.max_steps
                ),
            });
        }
        let llm_count = plan.llm_step_count();
        if llm_count > plan.budget.max_llm_calls {
            errors.push(CompileError::BudgetExceeded {
                detail: format!(
                    "{} LLM steps exceed max_llm_calls limit of {}",
                    llm_count, plan.budget.max_llm_calls
                ),
            });
        }

        // Topological sort to detect cycles.
        match topological_sort(&plan.steps) {
            Some(order) => {
                if !errors.is_empty() {
                    return Err(errors);
                }
                Ok(CompiledPlan {
                    execution_order: order,
                    source_plan: plan.clone(),
                })
            }
            None => {
                // Find the involved nodes in the cycle.
                let involved = find_cycle_members(&plan.steps);
                errors.push(CompileError::CyclicDependency { involved });
                Err(errors)
            }
        }
    }
}

/// Checks that every step reads only from steps it declared a dependency on.
///
/// The dependency must be *declared*, not merely reachable. Transitive reachability would
/// order the steps correctly too, but it would leave `depends_on` an incomplete statement of
/// what a step needs: a reader of the step could not tell where its input comes from without
/// walking the whole graph. Requiring the edge costs one word and the compiler says which.
fn check_input_wiring(steps: &[PlanStep], completed: &HashSet<String>) -> Vec<CompileError> {
    let known: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    let mut errors = Vec::new();

    for step in steps {
        let declared: HashSet<&str> = step.depends_on.iter().map(String::as_str).collect();
        for (key, reference) in &step.input_from {
            if step.input.contains_key(key) {
                errors.push(CompileError::ConflictingInput {
                    step: step.id.clone(),
                    key: key.clone(),
                });
            }
            let detail = if completed.contains(&reference.step) {
                None
            } else if reference.step == step.id {
                Some("a step cannot read its own output".to_string())
            } else if !known.contains(reference.step.as_str()) {
                Some(format!(
                    "there is no step '{}' in this plan",
                    reference.step
                ))
            } else if !declared.contains(reference.step.as_str()) {
                Some(format!(
                    "'{}' is not among its dependencies; add it to depends_on",
                    reference.step
                ))
            } else {
                None
            };
            if let Some(detail) = detail {
                errors.push(CompileError::UnreadableInput {
                    step: step.id.clone(),
                    key: key.clone(),
                    reference: reference.to_string(),
                    detail,
                });
            }
        }
    }
    errors
}

/// Performs a topological sort on plan steps. Returns None if a cycle is detected.
fn topological_sort(steps: &[PlanStep]) -> Option<Vec<PlanStep>> {
    let id_to_step: HashMap<&str, &PlanStep> = steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    let id_set: HashSet<&str> = ids.iter().copied().collect();

    // Build in-degree and adjacency.
    let mut in_degree: HashMap<&str, usize> = ids.iter().map(|&id| (id, 0)).collect();
    let mut successors: HashMap<&str, Vec<&str>> = ids.iter().map(|&id| (id, Vec::new())).collect();

    for step in steps {
        for dep in &step.depends_on {
            if id_set.contains(dep.as_str()) {
                successors
                    .get_mut(dep.as_str())
                    .unwrap()
                    .push(step.id.as_str());
                *in_degree.get_mut(step.id.as_str()).unwrap() += 1;
            }
        }
    }

    // Kahn's algorithm.
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    // Sort for determinism.
    let mut sorted_queue: Vec<&str> = queue.drain(..).collect();
    sorted_queue.sort();
    queue.extend(sorted_queue);

    let mut order = Vec::new();

    while let Some(current) = queue.pop_front() {
        order.push(id_to_step[current].clone());
        let mut newly_ready: Vec<&str> = Vec::new();
        for &next in &successors[current] {
            let deg = in_degree.get_mut(next).unwrap();
            *deg -= 1;
            if *deg == 0 {
                newly_ready.push(next);
            }
        }
        newly_ready.sort();
        queue.extend(newly_ready);
    }

    if order.len() == steps.len() {
        Some(order)
    } else {
        None
    }
}

/// Find nodes involved in cycles (those not in the topological order).
fn find_cycle_members(steps: &[PlanStep]) -> Vec<String> {
    let id_set: HashSet<&str> = steps.iter().map(|s| s.id.as_str()).collect();
    let mut in_degree: HashMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    let mut successors: HashMap<&str, Vec<&str>> =
        steps.iter().map(|s| (s.id.as_str(), Vec::new())).collect();

    for step in steps {
        for dep in &step.depends_on {
            if id_set.contains(dep.as_str()) {
                successors
                    .get_mut(dep.as_str())
                    .unwrap()
                    .push(step.id.as_str());
                *in_degree.get_mut(step.id.as_str()).unwrap() += 1;
            }
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut visited = HashSet::new();
    while let Some(current) = queue.pop_front() {
        visited.insert(current);
        for &next in &successors[current] {
            let deg = in_degree.get_mut(next).unwrap();
            *deg -= 1;
            if *deg == 0 {
                queue.push_back(next);
            }
        }
    }

    steps
        .iter()
        .filter(|s| !visited.contains(s.id.as_str()))
        .map(|s| s.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::PrecisionClass;
    use pearl_planner::{PlanBudget, PlanStep};
    use std::time::Duration;

    fn step(id: &str, cap: &str, deps: &[&str], class: PrecisionClass) -> PlanStep {
        PlanStep::new(id, cap, class, Duration::from_secs(30)).after(deps.to_vec())
    }

    fn step_no_timeout(id: &str, cap: &str, deps: &[&str], class: PrecisionClass) -> PlanStep {
        PlanStep::new(id, cap, class, Duration::ZERO).after(deps.to_vec())
    }

    fn plan(steps: Vec<PlanStep>) -> ExecutionPlan {
        ExecutionPlan {
            steps,
            budget: PlanBudget {
                max_steps: 16,
                max_llm_calls: 32,
            },
        }
    }

    fn all_caps() -> CapabilitySet {
        ["cap.a", "cap.b", "cap.c", "cap.d"]
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn all_verified() -> VerifierSet {
        ["a", "b", "c", "d"]
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // ------------------------------------------------------- data flow wiring

    use pearl_planner::StepRef;

    #[test]
    fn a_step_may_read_a_step_it_depends_on() {
        let compiler = PlanCompiler::default();
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0)
                .taking("items", StepRef::field("a", ["items"])),
        ]);
        let compiled = compiler.compile(&p).unwrap();
        assert_eq!(compiled.execution_order[1].input_from.len(), 1);
    }

    #[test]
    fn reading_a_step_that_is_not_a_dependency_does_not_compile() {
        let compiler = PlanCompiler::default();
        // `b` runs after `a` only by accident of ordering — nothing declares it must.
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &[], PrecisionClass::P0).taking("items", StepRef::whole("a")),
        ]);
        let errors = compiler.compile(&p).unwrap_err();
        let problem = errors
            .iter()
            .find_map(|e| match e {
                CompileError::UnreadableInput { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected an unreadable-input error, got {errors:?}"));
        assert!(problem.contains("depends_on"), "got {problem}");
    }

    #[test]
    fn reading_a_step_that_does_not_exist_does_not_compile() {
        let compiler = PlanCompiler::default();
        let p =
            plan(vec![step("a", "cap.a", &[], PrecisionClass::P0)
                .taking("x", StepRef::whole("imaginary"))]);
        let errors = compiler.compile(&p).unwrap_err();
        assert!(
            errors.iter().any(|e| matches!(
                e,
                CompileError::UnreadableInput { detail, .. } if detail.contains("no step 'imaginary'")
            )),
            "got {errors:?}"
        );
    }

    #[test]
    fn a_step_cannot_read_its_own_output() {
        let compiler = PlanCompiler::default();
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0).taking("x", StepRef::whole("a"))
        ]);
        let errors = compiler.compile(&p).unwrap_err();
        assert!(
            errors.iter().any(|e| matches!(
                e,
                CompileError::UnreadableInput { detail, .. } if detail.contains("its own output")
            )),
            "got {errors:?}"
        );
    }

    #[test]
    fn a_step_may_read_a_step_that_already_finished_elsewhere() {
        // How a dynamic sub-plan reads the run that produced it: the step it names is not in
        // this plan and cannot be, because it has already run.
        let compiler = PlanCompiler::new(CompilerConfig {
            completed_steps: ["earlier".to_string()].into_iter().collect(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![step("a", "cap.a", &[], PrecisionClass::P0)
            .taking("seed", StepRef::field("earlier", ["items"]))]);
        assert!(compiler.compile(&p).is_ok());

        // And without that declaration it is still an error, so the exemption is opt-in.
        assert!(PlanCompiler::default().compile(&p).is_err());
    }

    #[test]
    fn a_key_declared_as_both_a_literal_and_a_reference_does_not_compile() {
        let compiler = PlanCompiler::default();
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0)
                .with_input("items", serde_json::json!([1, 2]))
                .taking("items", StepRef::whole("a")),
        ]);
        let errors = compiler.compile(&p).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, CompileError::ConflictingInput { key, .. } if key == "items")),
            "got {errors:?}"
        );
    }

    #[test]
    fn accepts_valid_linear_dag() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0),
            step("c", "cap.c", &["b"], PrecisionClass::P0),
        ]);
        let compiled = compiler.compile(&p).unwrap();
        assert_eq!(compiled.execution_order.len(), 3);
        assert_eq!(compiled.execution_order[0].id, "a");
        assert_eq!(compiled.execution_order[1].id, "b");
        assert_eq!(compiled.execution_order[2].id, "c");
    }

    #[test]
    fn accepts_valid_diamond_dag() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![
            step("a", "cap.a", &[], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0),
            step("c", "cap.c", &["a"], PrecisionClass::P0),
            step("d", "cap.d", &["b", "c"], PrecisionClass::P0),
        ]);
        let compiled = compiler.compile(&p).unwrap();
        assert_eq!(compiled.execution_order.len(), 4);
        // a must come first, d must come last.
        assert_eq!(compiled.execution_order[0].id, "a");
        assert_eq!(compiled.execution_order[3].id, "d");
    }

    #[test]
    fn rejects_cyclic_dependency() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![
            step("a", "cap.a", &["b"], PrecisionClass::P0),
            step("b", "cap.b", &["a"], PrecisionClass::P0),
        ]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::CyclicDependency { .. })));
    }

    #[test]
    fn rejects_self_cycle() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![step("a", "cap.a", &["a"], PrecisionClass::P0)]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::CyclicDependency { .. })));
    }

    #[test]
    fn rejects_missing_capability() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: HashSet::from(["cap.a".to_string()]),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![step("a", "cap.unknown", &[], PrecisionClass::P0)]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::MissingCapability { .. })));
    }

    #[test]
    fn rejects_missing_verifier_for_exactness_step() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: HashSet::new(), // No verifiers
            ..CompilerConfig::default()
        });
        // §30 keys the obligation on the step's own exactness demand, not on its precision
        // class: a mechanical step can be a best-effort probe, and demanding a verifier for
        // every one of them made ordinary plans impossible to compile.
        let mut declared = step("a", "cap.a", &[], PrecisionClass::P0);
        declared.exactness_required = true;
        let p = plan(vec![declared]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::MissingVerifier { .. })));
    }

    #[test]
    fn an_exactness_step_with_a_verifier_compiles() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: HashSet::from(["a".to_string()]),
            ..CompilerConfig::default()
        });
        let mut declared = step("a", "cap.a", &[], PrecisionClass::P0);
        declared.exactness_required = true;
        assert!(compiler.compile(&plan(vec![declared])).is_ok());
    }

    #[test]
    fn an_exactness_demand_is_checked_even_with_no_registry() {
        // A demand with nothing to satisfy it is a violation whether or not the caller
        // supplied a capability set; the previous behaviour skipped the check entirely when
        // the registry was empty, which is the default.
        let compiler = PlanCompiler::default();
        let mut declared = step("a", "cap.a", &[], PrecisionClass::P0);
        declared.exactness_required = true;
        let errs = compiler.compile(&plan(vec![declared])).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::MissingVerifier { .. })));
    }

    #[test]
    fn a_step_making_no_exactness_demand_needs_no_verifier() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: HashSet::new(), // No verifiers
            ..CompilerConfig::default()
        });
        let p = plan(vec![step("a", "cap.a", &[], PrecisionClass::P3)]);
        let compiled = compiler.compile(&p).unwrap();
        assert_eq!(compiled.execution_order.len(), 1);
    }

    #[test]
    fn rejects_budget_exceeded() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = ExecutionPlan {
            steps: vec![
                step("a", "cap.a", &[], PrecisionClass::P0),
                step("b", "cap.b", &[], PrecisionClass::P0),
            ],
            budget: PlanBudget {
                max_steps: 1,
                max_llm_calls: 32,
            },
        };
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::BudgetExceeded { .. })));
    }

    #[test]
    fn rejects_missing_timeout() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
            ..CompilerConfig::default()
        });
        let p = plan(vec![step_no_timeout("a", "cap.a", &[], PrecisionClass::P0)]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::MissingTimeout { .. })));
    }

    #[test]
    fn empty_plan_compiles() {
        let compiler = PlanCompiler::default();
        let p = plan(vec![]);
        let compiled = compiler.compile(&p).unwrap();
        assert!(compiled.execution_order.is_empty());
    }

    #[test]
    fn skips_capability_check_when_registry_empty() {
        // When known_capabilities is empty, skip the check (no registry provided).
        let compiler = PlanCompiler::default();
        let p = plan(vec![step("a", "any.cap", &[], PrecisionClass::P3)]);
        let compiled = compiler.compile(&p).unwrap();
        assert_eq!(compiled.execution_order.len(), 1);
    }
}
