//! The routing decision engine.

use pearl_capabilities::CapabilityRegistry;
use pearl_core::precision::PrecisionClass;
use pearl_governance::manifest::Runtime;
use serde::{Deserialize, Serialize};

/// The outcome of a routing decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RoutingDecision {
    /// Route to a mechanical script runtime.
    ScriptRoute {
        /// The capability id that will handle this task.
        capability_id: String,
        /// The runtime to use for execution.
        runtime: Runtime,
        /// The entrypoint script/binary path (from the manifest id convention).
        entrypoint: String,
    },
    /// Route to an agent runtime.
    AgentRoute {
        /// The precision class that determined agent routing.
        precision: PrecisionClass,
        /// Why agent routing was chosen.
        reason: String,
    },
    /// Rejected: no suitable execution path exists.
    Rejected {
        /// Why the task was rejected.
        reason: String,
    },
}

/// What the router needs to know about a task to make a routing decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRequirements {
    /// The type/category of task (matched against capability ids).
    pub task_type: String,
    /// Capabilities required by this task (capability ids to search for).
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// The quality specification driving precision classification.
    pub quality_spec: pearl_core::precision::QualitySpec,
    /// An explicit precision class override, if the caller already knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision_override: Option<PrecisionClass>,
}

/// Errors from the routing process.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// No capability matched the requirements.
    #[error("no capability matched task_type '{task_type}'")]
    NoMatch { task_type: String },
}

/// The script-first router.
///
/// Stateless: all state lives in the [`CapabilityRegistry`] and the task requirements.
#[derive(Debug, Clone, Default)]
pub struct Router;

impl Router {
    /// Create a new router instance.
    pub fn new() -> Self {
        Self
    }

    /// Route a task to its execution target.
    ///
    /// Decision logic:
    /// 1. Determine the effective precision class (override or inferred from quality_spec).
    /// 2. If P0, look for a mechanical capability in the registry.
    ///    - Found: return ScriptRoute.
    ///    - Not found: return Rejected (Article 1 forbids agent fallback for P0).
    /// 3. For P1/P2/P3, first try to find a mechanical capability (script-first preference).
    ///    - Found: return ScriptRoute.
    ///    - Not found: return AgentRoute.
    pub fn route(
        &self,
        requirements: &TaskRequirements,
        registry: &CapabilityRegistry,
    ) -> RoutingDecision {
        let precision = self.effective_precision(requirements);

        // Script-first: look for a matching mechanical (P0) capability.
        if let Some(decision) = self.try_script_route(requirements, registry) {
            return decision;
        }

        // No mechanical capability found.
        match precision {
            PrecisionClass::P0 => {
                // Article 1: P0 tasks MUST route to script. No fallback.
                RoutingDecision::Rejected {
                    reason: format!(
                        "P0 task '{}' requires a mechanical capability but none was found in the registry",
                        requirements.task_type
                    ),
                }
            }
            _ => {
                // P1/P2/P3 may route to agents.
                RoutingDecision::AgentRoute {
                    precision,
                    reason: format!(
                        "No mechanical capability for '{}'; routing to agent at precision {}",
                        requirements.task_type,
                        precision.as_str()
                    ),
                }
            }
        }
    }

    /// Determine the effective precision class for routing.
    ///
    /// If the caller provided an override, use it. Otherwise infer from quality_spec.
    fn effective_precision(&self, requirements: &TaskRequirements) -> PrecisionClass {
        if let Some(override_class) = requirements.precision_override {
            return override_class;
        }

        // Infer from quality_spec using the same logic as the Precision Decision Engine:
        // mechanical quality => P0, verifiable => P1, exact but unverifiable => P2, else P3.
        let qs = &requirements.quality_spec;
        if qs.deterministic_generation && qs.deterministic_verification {
            PrecisionClass::P0
        } else if !qs.deterministic_generation && qs.deterministic_verification {
            PrecisionClass::P1
        } else if qs.exactness_required && !qs.deterministic_verification {
            PrecisionClass::P2
        } else {
            PrecisionClass::P3
        }
    }

    /// Try to find a mechanical capability matching the task requirements.
    ///
    /// Searches the registry for P0 capabilities whose id matches the task_type or
    /// one of the required_capabilities.
    fn try_script_route(
        &self,
        requirements: &TaskRequirements,
        registry: &CapabilityRegistry,
    ) -> Option<RoutingDecision> {
        let mechanical = registry.find_mechanical();

        // First: exact id match on task_type.
        if let Some(cap) = mechanical
            .iter()
            .find(|c| c.manifest.id == requirements.task_type)
        {
            return Some(RoutingDecision::ScriptRoute {
                capability_id: cap.manifest.id.clone(),
                runtime: cap.manifest.execution.runtime,
                entrypoint: cap.manifest.id.clone(),
            });
        }

        // Second: match against required_capabilities list.
        for req_cap in &requirements.required_capabilities {
            if let Some(cap) = mechanical.iter().find(|c| &c.manifest.id == req_cap) {
                return Some(RoutingDecision::ScriptRoute {
                    capability_id: cap.manifest.id.clone(),
                    runtime: cap.manifest.execution.runtime,
                    entrypoint: cap.manifest.id.clone(),
                });
            }
        }

        // Third: partial match -- task_type appears as a substring in capability id.
        if let Some(cap) = mechanical
            .iter()
            .find(|c| c.manifest.id.contains(&requirements.task_type))
        {
            return Some(RoutingDecision::ScriptRoute {
                capability_id: cap.manifest.id.clone(),
                runtime: cap.manifest.execution.runtime,
                entrypoint: cap.manifest.id.clone(),
            });
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::precision::QualitySpec;
    use pearl_governance::{
        CapabilityManifest, CapabilityType, Execution, ExecutionKind, Platform, Quality, Risk,
        Runtime, Schemas,
    };

    /// Build a deterministic script manifest (P0).
    fn mechanical_manifest(id: &str, runtime: Runtime) -> CapabilityManifest {
        CapabilityManifest {
            id: id.to_string(),
            version: 1,
            capability_type: CapabilityType::Script,
            description: Some("A mechanical script".to_string()),
            execution: Execution {
                kind: ExecutionKind::Script,
                runtime,
            },
            quality: Quality {
                deterministic: true,
            },
            risk: Risk {
                side_effect: false,
                idempotency: None,
            },
            platform: Platform {
                windows: true,
                linux: true,
            },
            schemas: Schemas::default(),
            timeout_seconds: Some(60),
            on_error: None,
        }
    }

    /// Build an agent manifest (non-deterministic, P3).
    fn agent_manifest(id: &str) -> CapabilityManifest {
        CapabilityManifest {
            id: id.to_string(),
            version: 1,
            capability_type: CapabilityType::Agent,
            description: Some("An agent capability".to_string()),
            execution: Execution {
                kind: ExecutionKind::Agent,
                runtime: Runtime::ClaudeCode,
            },
            quality: Quality {
                deterministic: false,
            },
            risk: Risk {
                side_effect: false,
                idempotency: None,
            },
            platform: Platform {
                windows: true,
                linux: true,
            },
            schemas: Schemas::default(),
            timeout_seconds: Some(120),
            on_error: None,
        }
    }

    fn registry_with_manifests(manifests: Vec<CapabilityManifest>) -> CapabilityRegistry {
        let mut registry = CapabilityRegistry::new();
        for m in manifests {
            registry.register(m, None);
        }
        registry
    }

    // -----------------------------------------------------------------------
    // Script routing tests (P0 tasks)
    // -----------------------------------------------------------------------

    #[test]
    fn routes_deterministic_task_to_script() {
        let registry = registry_with_manifests(vec![
            mechanical_manifest("script.task-score", Runtime::Rust),
            agent_manifest("agent.code-review"),
        ]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.task-score".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::ScriptRoute {
                capability_id,
                runtime,
                ..
            } => {
                assert_eq!(capability_id, "script.task-score");
                assert_eq!(runtime, Runtime::Rust);
            }
            other => panic!("expected ScriptRoute, got: {other:?}"),
        }
    }

    #[test]
    fn rejects_p0_task_with_no_mechanical_capability() {
        let registry = registry_with_manifests(vec![agent_manifest("agent.code-review")]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.nonexistent".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::Rejected { reason } => {
                assert!(reason.contains("P0"), "reason: {reason}");
                assert!(reason.contains("script.nonexistent"), "reason: {reason}");
            }
            other => panic!("expected Rejected, got: {other:?}"),
        }
    }

    #[test]
    fn p0_inferred_from_quality_spec_routes_to_script() {
        let registry =
            registry_with_manifests(vec![mechanical_manifest("script.compute", Runtime::Python)]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.compute".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: None, // inferred as P0 from quality_spec
        };

        let decision = router.route(&requirements, &registry);
        assert!(matches!(decision, RoutingDecision::ScriptRoute { .. }));
    }

    #[test]
    fn p0_inferred_rejects_when_no_capability() {
        let registry = registry_with_manifests(vec![]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.missing".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: None,
        };

        let decision = router.route(&requirements, &registry);
        assert!(matches!(decision, RoutingDecision::Rejected { .. }));
    }

    // -----------------------------------------------------------------------
    // Agent routing tests (P1/P2/P3 tasks)
    // -----------------------------------------------------------------------

    #[test]
    fn routes_p3_task_to_agent_when_no_mechanical_match() {
        let registry =
            registry_with_manifests(vec![mechanical_manifest("script.other", Runtime::Shell)]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "code-review".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::best_effort(),
            precision_override: Some(PrecisionClass::P3),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::AgentRoute { precision, reason } => {
                assert_eq!(precision, PrecisionClass::P3);
                assert!(reason.contains("code-review"), "reason: {reason}");
            }
            other => panic!("expected AgentRoute, got: {other:?}"),
        }
    }

    #[test]
    fn p1_task_prefers_script_if_available() {
        let registry =
            registry_with_manifests(vec![mechanical_manifest("script.verify", Runtime::Python)]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.verify".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec {
                exactness_required: true,
                deterministic_generation: false,
                deterministic_verification: true,
            },
            precision_override: Some(PrecisionClass::P1),
        };

        let decision = router.route(&requirements, &registry);
        // Even though it is P1, the script-first principle routes to script.
        assert!(matches!(decision, RoutingDecision::ScriptRoute { .. }));
    }

    #[test]
    fn p1_falls_back_to_agent_when_no_script() {
        let registry = registry_with_manifests(vec![]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "generate-code".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec {
                exactness_required: true,
                deterministic_generation: false,
                deterministic_verification: true,
            },
            precision_override: Some(PrecisionClass::P1),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::AgentRoute { precision, .. } => {
                assert_eq!(precision, PrecisionClass::P1);
            }
            other => panic!("expected AgentRoute, got: {other:?}"),
        }
    }

    #[test]
    fn p2_falls_back_to_agent() {
        let registry = registry_with_manifests(vec![]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "analyze-logs".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::exact_but_unverifiable(),
            precision_override: Some(PrecisionClass::P2),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::AgentRoute { precision, .. } => {
                assert_eq!(precision, PrecisionClass::P2);
            }
            other => panic!("expected AgentRoute, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // required_capabilities matching
    // -----------------------------------------------------------------------

    #[test]
    fn matches_via_required_capabilities() {
        let registry = registry_with_manifests(vec![
            mechanical_manifest("script.task-score", Runtime::Rust),
            mechanical_manifest("guard.shell-check", Runtime::Python),
        ]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "compute-priority".to_string(), // no direct match
            required_capabilities: vec!["script.task-score".to_string()],
            quality_spec: QualitySpec::mechanical(),
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::ScriptRoute { capability_id, .. } => {
                assert_eq!(capability_id, "script.task-score");
            }
            other => panic!("expected ScriptRoute via required_capabilities, got: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Partial match (substring) tests
    // -----------------------------------------------------------------------

    #[test]
    fn matches_via_substring_in_capability_id() {
        let registry = registry_with_manifests(vec![mechanical_manifest(
            "script.task-score",
            Runtime::Rust,
        )]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "task-score".to_string(), // substring of "script.task-score"
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);
        assert!(matches!(decision, RoutingDecision::ScriptRoute { .. }));
    }

    // -----------------------------------------------------------------------
    // Precision inference from quality_spec
    // -----------------------------------------------------------------------

    #[test]
    fn infers_p0_from_mechanical_quality_spec() {
        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "test".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(),
            precision_override: None,
        };
        // We test effective_precision indirectly via the routing result on empty registry.
        let registry = registry_with_manifests(vec![]);
        let decision = router.route(&requirements, &registry);
        // P0 with no capability => Rejected
        assert!(matches!(decision, RoutingDecision::Rejected { .. }));
    }

    #[test]
    fn infers_p3_from_best_effort_quality_spec() {
        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "explore".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::best_effort(),
            precision_override: None,
        };
        let registry = registry_with_manifests(vec![]);
        let decision = router.route(&requirements, &registry);
        match decision {
            RoutingDecision::AgentRoute { precision, .. } => {
                assert_eq!(precision, PrecisionClass::P3);
            }
            other => panic!("expected AgentRoute with P3, got: {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // §48/§51: Script/P0 failure MUST NOT fallback to Agent
    // -------------------------------------------------------------------

    #[test]
    fn p0_script_failure_does_not_fallback_to_agent() {
        // Given: a P0 task with no matching mechanical capability
        // Then: the router REJECTS rather than falling back to AgentRoute
        let registry = registry_with_manifests(vec![
            // Only agent capabilities present -- no scripts.
            agent_manifest("agent.code-review"),
            agent_manifest("agent.summarize"),
        ]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.deterministic-compute".to_string(),
            required_capabilities: vec![],
            quality_spec: QualitySpec::mechanical(), // P0
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);

        // Critical assertion: P0 MUST NOT route to an agent under any circumstance.
        assert!(
            !matches!(decision, RoutingDecision::AgentRoute { .. }),
            "VIOLATION: P0/Script task was routed to an agent! Decision: {decision:?}"
        );
        assert!(
            matches!(decision, RoutingDecision::Rejected { .. }),
            "P0 task without mechanical capability must be Rejected, got: {decision:?}"
        );
    }

    #[test]
    fn p0_with_unrelated_mechanical_capability_still_rejects() {
        // Even if mechanical capabilities exist, they must MATCH the task.
        // An unrelated mechanical capability does not satisfy the routing requirement.
        let registry = registry_with_manifests(vec![mechanical_manifest(
            "script.unrelated-tool",
            Runtime::Rust,
        )]);

        let router = Router::new();
        let requirements = TaskRequirements {
            task_type: "script.specific-compute".to_string(),
            required_capabilities: vec!["script.specific-compute".to_string()],
            quality_spec: QualitySpec::mechanical(),
            precision_override: Some(PrecisionClass::P0),
        };

        let decision = router.route(&requirements, &registry);

        // Must reject -- no fallback to agent allowed.
        assert!(
            !matches!(decision, RoutingDecision::AgentRoute { .. }),
            "VIOLATION: P0 task fell back to agent! Decision: {decision:?}"
        );
    }

    #[test]
    fn multiple_p0_tasks_all_reject_without_agent_fallback() {
        // Exhaustive: verify the invariant holds for various P0 task types.
        let registry = registry_with_manifests(vec![]);
        let router = Router::new();

        let task_types = [
            "script.cache-validate",
            "script.score-task",
            "script.verify-digest",
            "script.run-tests",
        ];

        for task_type in &task_types {
            let requirements = TaskRequirements {
                task_type: task_type.to_string(),
                required_capabilities: vec![],
                quality_spec: QualitySpec::mechanical(),
                precision_override: Some(PrecisionClass::P0),
            };

            let decision = router.route(&requirements, &registry);
            assert!(
                matches!(decision, RoutingDecision::Rejected { .. }),
                "P0 task '{task_type}' should be Rejected but got: {decision:?}"
            );
        }
    }
}
