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

        // Check verifiers for exactness steps (P0, P1).
        // Only check when a capability registry is provided (known_capabilities non-empty),
        // since verifier existence depends on registry context.
        if !self.config.known_capabilities.is_empty() {
            for step in &plan.steps {
                let needs_verifier = matches!(
                    step.precision_class,
                    pearl_core::PrecisionClass::P0 | pearl_core::PrecisionClass::P1
                );
                if needs_verifier && !self.config.verified_steps.contains(&step.id) {
                    errors.push(CompileError::MissingVerifier {
                        step: step.id.clone(),
                        class: format!("{:?}", step.precision_class),
                    });
                }
            }
        }

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
        PlanStep {
            id: id.to_string(),
            capability: cap.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            precision_class: class,
            timeout: Duration::from_secs(30),
        }
    }

    fn step_no_timeout(id: &str, cap: &str, deps: &[&str], class: PrecisionClass) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            capability: cap.to_string(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            precision_class: class,
            timeout: Duration::ZERO,
        }
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

    #[test]
    fn accepts_valid_linear_dag() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: all_verified(),
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
        });
        let p = plan(vec![step("a", "cap.a", &[], PrecisionClass::P0)]);
        let errs = compiler.compile(&p).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| matches!(e, CompileError::MissingVerifier { .. })));
    }

    #[test]
    fn p3_steps_do_not_require_verifier() {
        let compiler = PlanCompiler::new(CompilerConfig {
            known_capabilities: all_caps(),
            verified_steps: HashSet::new(), // No verifiers
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
