//! The classification engine -- core decision logic.

use pearl_core::precision::{PrecisionClass, QualitySpec};
use pearl_governance::{CapabilityManifest, CapabilityType, ExecutionKind, Quality, Runtime};
use serde::{Deserialize, Serialize};

/// Input to the Precision Decision Engine.
///
/// Captures all information needed to determine the precision class of a task step.
/// Can be constructed manually or via [`ClassificationInput::from_manifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationInput {
    /// What kind of capability this is.
    pub capability_type: CapabilityType,
    /// How the capability is executed (script, tool, agent, etc.).
    pub execution_kind: ExecutionKind,
    /// Which runtime executes it.
    pub runtime: Runtime,
    /// Whether the capability declares itself as deterministic.
    pub deterministic: bool,
    /// The quality specification from the task context.
    pub quality_spec: QualitySpec,
    /// Optional manual override with justification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_class: Option<ClassificationOverride>,
}

/// A manual override of the classification decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationOverride {
    /// The class to force.
    pub target_class: PrecisionClass,
    /// Why the override is justified (audit trail).
    pub reason: String,
}

/// The result of a precision classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassificationResult {
    /// The assigned precision class.
    pub class: PrecisionClass,
    /// Reasoning trail: which rules fired and in what order.
    pub reasoning: Vec<String>,
    /// Whether this result was produced by a manual override rather than the rules.
    pub overridden: bool,
}

impl ClassificationResult {
    /// Whether this classification permits LLM generation (delegates to PrecisionClass).
    pub fn permits_llm_generation(&self) -> bool {
        self.class.permits_llm_generation()
    }

    /// Whether a Machine Verifier is expected to decide correctness.
    pub fn expects_machine_verification(&self) -> bool {
        self.class.expects_machine_verification()
    }
}

/// Errors that can occur during classification.
#[derive(Debug, thiserror::Error)]
pub enum PrecisionError {
    /// The override targets a class that is less strict than the rules would assign,
    /// which is only permitted with explicit justification (caught by checks, not blocked).
    #[error("override relaxes classification from {from} to {to}: {reason}")]
    OverrideRelaxation {
        from: String,
        to: String,
        reason: String,
    },
}

/// The Precision Decision Engine.
///
/// Stateless classifier that applies the decision rules from the system specification
/// to determine which precision class a task step belongs to.
#[derive(Debug, Clone, Default)]
pub struct PrecisionDecisionEngine;

impl PrecisionDecisionEngine {
    /// Creates a new engine instance.
    pub fn new() -> Self {
        Self
    }

    /// Classify a task step based on its declared properties.
    ///
    /// Decision rules (applied in order):
    /// 1. If `deterministic == true` AND `execution_kind == Script` AND
    ///    `runtime.is_mechanical()` => P0 (fully deterministic script).
    /// 2. If `deterministic == false` AND `quality_spec.deterministic_verification == true`
    ///    => P1 (generative but verifiable).
    /// 3. If `deterministic == false` AND `quality_spec.deterministic_verification == false`
    ///    AND `quality_spec.exactness_required == true` => P2 (partially verifiable).
    /// 4. Otherwise => P3 (subjective/exploratory).
    ///
    /// A manual override, if present, replaces the rule-based decision but records both
    /// in the reasoning trail for audit purposes.
    pub fn classify(&self, input: &ClassificationInput) -> ClassificationResult {
        let mut reasoning = Vec::new();

        // Apply decision rules to determine the natural class.
        let natural_class = self.apply_rules(input, &mut reasoning);

        // Check for override.
        if let Some(ref override_spec) = input.override_class {
            reasoning.push(format!(
                "Manual override applied: {} -> {} (reason: {})",
                natural_class.as_str(),
                override_spec.target_class.as_str(),
                override_spec.reason
            ));

            return ClassificationResult {
                class: override_spec.target_class,
                reasoning,
                overridden: true,
            };
        }

        ClassificationResult {
            class: natural_class,
            reasoning,
            overridden: false,
        }
    }

    /// Convenience: classify directly from a [`CapabilityManifest`] and a [`QualitySpec`].
    pub fn classify_manifest(
        &self,
        manifest: &CapabilityManifest,
        quality_spec: QualitySpec,
    ) -> ClassificationResult {
        let input = ClassificationInput::from_manifest(manifest, quality_spec);
        self.classify(&input)
    }

    /// Apply the decision rules and return the natural (non-overridden) class.
    fn apply_rules(
        &self,
        input: &ClassificationInput,
        reasoning: &mut Vec<String>,
    ) -> PrecisionClass {
        // Rule 1: Deterministic + Script + Mechanical runtime => P0
        if input.deterministic
            && input.execution_kind == ExecutionKind::Script
            && input.runtime.is_mechanical()
        {
            reasoning.push(format!(
                "Rule 1 (P0): deterministic=true, execution_kind=script, runtime={} is mechanical",
                input.runtime.as_str()
            ));
            return PrecisionClass::P0;
        }

        // Rule 2: Non-deterministic + deterministic verification => P1
        if !input.deterministic && input.quality_spec.deterministic_verification {
            reasoning.push(
                "Rule 2 (P1): deterministic=false, deterministic_verification=true \
                 (generative but verifiable)"
                    .to_string(),
            );
            return PrecisionClass::P1;
        }

        // Rule 3: Non-deterministic + no deterministic verification + exactness required => P2
        if !input.deterministic
            && !input.quality_spec.deterministic_verification
            && input.quality_spec.exactness_required
        {
            reasoning.push(
                "Rule 3 (P2): deterministic=false, deterministic_verification=false, \
                 exactness_required=true (partially verifiable)"
                    .to_string(),
            );
            return PrecisionClass::P2;
        }

        // Rule 4: Everything else => P3
        reasoning.push(
            "Rule 4 (P3): no higher-precision rule matched (subjective/exploratory)".to_string(),
        );
        PrecisionClass::P3
    }
}

impl ClassificationInput {
    /// Construct a [`ClassificationInput`] from a governance [`CapabilityManifest`]
    /// and a task-level [`QualitySpec`].
    ///
    /// The manifest provides the capability-level declarations (type, execution, runtime,
    /// quality.deterministic). The quality_spec comes from the task context and captures
    /// what the *caller* requires.
    pub fn from_manifest(manifest: &CapabilityManifest, quality_spec: QualitySpec) -> Self {
        Self {
            capability_type: manifest.capability_type,
            execution_kind: manifest.execution.kind,
            runtime: manifest.execution.runtime,
            deterministic: manifest.quality.deterministic,
            quality_spec,
            override_class: None,
        }
    }

    /// Construct a [`ClassificationInput`] from a manifest, inferring the quality spec
    /// from the manifest's own declarations.
    ///
    /// This is a convenience for when the task has no separate quality spec and we rely
    /// entirely on what the capability declares about itself.
    pub fn from_manifest_inferred(manifest: &CapabilityManifest) -> Self {
        let quality_spec = infer_quality_spec(&manifest.quality, manifest.execution.runtime);
        Self::from_manifest(manifest, quality_spec)
    }
}

/// Infer a [`QualitySpec`] from the manifest's quality declarations and runtime.
///
/// This is a best-effort inference when no explicit quality spec is provided by the task:
/// - If the manifest says deterministic, we infer full mechanical quality.
/// - Otherwise, we infer best-effort (the caller should provide an explicit spec if they
///   need stronger guarantees).
fn infer_quality_spec(quality: &Quality, _runtime: Runtime) -> QualitySpec {
    if quality.deterministic {
        QualitySpec::mechanical()
    } else {
        QualitySpec::best_effort()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_governance::{
        CapabilityManifest, CapabilityType, Execution, ExecutionKind, Platform, Quality, Risk,
        Runtime, Schemas,
    };

    fn make_engine() -> PrecisionDecisionEngine {
        PrecisionDecisionEngine::new()
    }

    /// Helper: build a manifest with given parameters.
    fn make_manifest(
        cap_type: CapabilityType,
        kind: ExecutionKind,
        runtime: Runtime,
        deterministic: bool,
    ) -> CapabilityManifest {
        CapabilityManifest {
            id: "test.capability".to_string(),
            version: 1,
            capability_type: cap_type,
            description: None,
            execution: Execution { kind, runtime },
            quality: Quality { deterministic },
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

    // -----------------------------------------------------------------------
    // P0 tests: deterministic scripts on mechanical runtimes
    // -----------------------------------------------------------------------

    #[test]
    fn deterministic_script_on_python_is_p0() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Script,
            execution_kind: ExecutionKind::Script,
            runtime: Runtime::Python,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P0);
        assert!(!result.overridden);
        assert!(!result.permits_llm_generation());
        assert!(result.expects_machine_verification());
    }

    #[test]
    fn deterministic_script_on_rust_is_p0() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Script,
            execution_kind: ExecutionKind::Script,
            runtime: Runtime::Rust,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P0);
    }

    #[test]
    fn deterministic_script_on_shell_is_p0() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Script,
            execution_kind: ExecutionKind::Script,
            runtime: Runtime::Shell,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P0);
    }

    #[test]
    fn deterministic_script_on_non_mechanical_runtime_is_not_p0() {
        let engine = make_engine();
        // ClaudeCode is not a mechanical runtime
        let input = ClassificationInput {
            capability_type: CapabilityType::Script,
            execution_kind: ExecutionKind::Script,
            runtime: Runtime::ClaudeCode,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        // deterministic=true but runtime is not mechanical, so Rule 1 does not fire.
        // Falls through to Rule 4 (P3) since deterministic=true means Rules 2/3 skip.
        assert_ne!(result.class, PrecisionClass::P0);
    }

    #[test]
    fn deterministic_tool_execution_is_not_p0() {
        let engine = make_engine();
        // Execution kind is Tool, not Script
        let input = ClassificationInput {
            capability_type: CapabilityType::Tool,
            execution_kind: ExecutionKind::Tool,
            runtime: Runtime::Python,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        // Rule 1 requires execution_kind == Script
        assert_ne!(result.class, PrecisionClass::P0);
    }

    // -----------------------------------------------------------------------
    // P1 tests: generative but verifiable (LLM-generated-but-verifiable)
    // -----------------------------------------------------------------------

    #[test]
    fn non_deterministic_with_deterministic_verification_is_p1() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Agent,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec {
                exactness_required: true,
                deterministic_generation: false,
                deterministic_verification: true,
            },
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P1);
        assert!(!result.overridden);
        assert!(result.permits_llm_generation());
        assert!(result.expects_machine_verification());
    }

    #[test]
    fn llm_generated_code_with_test_verifier_is_p1() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Skill,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec {
                exactness_required: true,
                deterministic_generation: false,
                deterministic_verification: true,
            },
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P1);
    }

    // -----------------------------------------------------------------------
    // P2 tests: exact but unverifiable (partially verifiable)
    // -----------------------------------------------------------------------

    #[test]
    fn exact_but_unverifiable_is_p2() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Tool,
            execution_kind: ExecutionKind::Tool,
            runtime: Runtime::Python,
            deterministic: false,
            quality_spec: QualitySpec::exact_but_unverifiable(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P2);
        assert!(!result.overridden);
        assert!(result.permits_llm_generation());
        assert!(!result.expects_machine_verification());
    }

    // -----------------------------------------------------------------------
    // P3 tests: subjective or best-effort
    // -----------------------------------------------------------------------

    #[test]
    fn best_effort_non_deterministic_is_p3() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Agent,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec::best_effort(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P3);
        assert!(!result.overridden);
        assert!(result.permits_llm_generation());
        assert!(!result.expects_machine_verification());
    }

    #[test]
    fn subjective_exploration_is_p3() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Workflow,
            execution_kind: ExecutionKind::Workflow,
            runtime: Runtime::OpenaiCompatible,
            deterministic: false,
            quality_spec: QualitySpec {
                exactness_required: false,
                deterministic_generation: false,
                deterministic_verification: false,
            },
            override_class: None,
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P3);
    }

    // -----------------------------------------------------------------------
    // Override tests
    // -----------------------------------------------------------------------

    #[test]
    fn override_replaces_natural_classification() {
        let engine = make_engine();
        // This would naturally be P3 but we override to P1.
        let input = ClassificationInput {
            capability_type: CapabilityType::Agent,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec::best_effort(),
            override_class: Some(ClassificationOverride {
                target_class: PrecisionClass::P1,
                reason: "Custom verifier attached post-hoc".to_string(),
            }),
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P1);
        assert!(result.overridden);
        // The reasoning should mention both the natural rule and the override.
        assert!(result
            .reasoning
            .iter()
            .any(|r| r.contains("Manual override")));
        assert!(result
            .reasoning
            .iter()
            .any(|r| r.contains("Custom verifier")));
    }

    #[test]
    fn override_to_p0_is_recorded() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Tool,
            execution_kind: ExecutionKind::Tool,
            runtime: Runtime::Python,
            deterministic: false,
            quality_spec: QualitySpec::best_effort(),
            override_class: Some(ClassificationOverride {
                target_class: PrecisionClass::P0,
                reason: "Operator verified this tool is fully deterministic".to_string(),
            }),
        };
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P0);
        assert!(result.overridden);
        assert!(!result.permits_llm_generation());
    }

    // -----------------------------------------------------------------------
    // from_manifest tests
    // -----------------------------------------------------------------------

    #[test]
    fn from_manifest_deterministic_script_yields_p0() {
        let engine = make_engine();
        let manifest = make_manifest(
            CapabilityType::Script,
            ExecutionKind::Script,
            Runtime::Python,
            true,
        );
        let result = engine.classify_manifest(&manifest, QualitySpec::mechanical());
        assert_eq!(result.class, PrecisionClass::P0);
    }

    #[test]
    fn from_manifest_agent_with_verifier_yields_p1() {
        let engine = make_engine();
        let manifest = make_manifest(
            CapabilityType::Agent,
            ExecutionKind::Agent,
            Runtime::ClaudeCode,
            false,
        );
        let quality_spec = QualitySpec {
            exactness_required: true,
            deterministic_generation: false,
            deterministic_verification: true,
        };
        let result = engine.classify_manifest(&manifest, quality_spec);
        assert_eq!(result.class, PrecisionClass::P1);
    }

    #[test]
    fn from_manifest_inferred_deterministic_is_p0() {
        let engine = make_engine();
        let manifest = make_manifest(
            CapabilityType::Verifier,
            ExecutionKind::Script,
            Runtime::Python,
            true,
        );
        let input = ClassificationInput::from_manifest_inferred(&manifest);
        let result = engine.classify(&input);
        assert_eq!(result.class, PrecisionClass::P0);
    }

    #[test]
    fn from_manifest_inferred_non_deterministic_is_p3() {
        let engine = make_engine();
        let manifest = make_manifest(
            CapabilityType::Agent,
            ExecutionKind::Agent,
            Runtime::ClaudeCode,
            false,
        );
        let input = ClassificationInput::from_manifest_inferred(&manifest);
        let result = engine.classify(&input);
        // Non-deterministic with best_effort inferred => P3
        assert_eq!(result.class, PrecisionClass::P3);
    }

    // -----------------------------------------------------------------------
    // Article 1 alignment: P0 forbids LLM
    // -----------------------------------------------------------------------

    #[test]
    fn p0_classification_forbids_llm_per_article_1() {
        let engine = make_engine();
        let manifest = make_manifest(
            CapabilityType::Script,
            ExecutionKind::Script,
            Runtime::Shell,
            true,
        );
        let result = engine.classify_manifest(&manifest, QualitySpec::mechanical());
        assert_eq!(result.class, PrecisionClass::P0);
        assert!(
            !result.permits_llm_generation(),
            "Article 1: P0 must forbid LLM generation"
        );
    }

    #[test]
    fn p1_and_above_permit_llm_generation() {
        let engine = make_engine();

        // P1
        let input_p1 = ClassificationInput {
            capability_type: CapabilityType::Skill,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec {
                exactness_required: true,
                deterministic_generation: false,
                deterministic_verification: true,
            },
            override_class: None,
        };
        assert!(engine.classify(&input_p1).permits_llm_generation());

        // P2
        let input_p2 = ClassificationInput {
            capability_type: CapabilityType::Tool,
            execution_kind: ExecutionKind::Tool,
            runtime: Runtime::Python,
            deterministic: false,
            quality_spec: QualitySpec::exact_but_unverifiable(),
            override_class: None,
        };
        assert!(engine.classify(&input_p2).permits_llm_generation());

        // P3
        let input_p3 = ClassificationInput {
            capability_type: CapabilityType::Agent,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec::best_effort(),
            override_class: None,
        };
        assert!(engine.classify(&input_p3).permits_llm_generation());
    }

    // -----------------------------------------------------------------------
    // Reasoning trail tests
    // -----------------------------------------------------------------------

    #[test]
    fn reasoning_explains_p0_decision() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Script,
            execution_kind: ExecutionKind::Script,
            runtime: Runtime::Python,
            deterministic: true,
            quality_spec: QualitySpec::mechanical(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert!(!result.reasoning.is_empty());
        assert!(result.reasoning[0].contains("Rule 1"));
        assert!(result.reasoning[0].contains("P0"));
    }

    #[test]
    fn reasoning_explains_p3_fallthrough() {
        let engine = make_engine();
        let input = ClassificationInput {
            capability_type: CapabilityType::Agent,
            execution_kind: ExecutionKind::Agent,
            runtime: Runtime::ClaudeCode,
            deterministic: false,
            quality_spec: QualitySpec::best_effort(),
            override_class: None,
        };
        let result = engine.classify(&input);
        assert!(result
            .reasoning
            .iter()
            .any(|r| r.contains("Rule 4") && r.contains("P3")));
    }
}
