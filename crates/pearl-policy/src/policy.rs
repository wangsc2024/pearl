//! Policy engine implementation.

use serde::{Deserialize, Serialize};

/// The type of approval required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    /// Automatic approval (no human needed).
    Auto,
    /// Requires human approval before proceeding.
    Human,
}

/// Autonomy level derived from verification coverage (Article 11).
///
/// Higher verification coverage grants more autonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AutonomyLevel {
    /// No autonomy: all actions require human approval.
    None,
    /// Low autonomy: only P0 (deterministic) actions are autonomous.
    Low,
    /// Medium autonomy: P0 and verified P1 actions are autonomous.
    Medium,
    /// High autonomy: most actions are autonomous, only high-risk require approval.
    High,
}

impl AutonomyLevel {
    /// Derive autonomy level from verification coverage percentage (0.0 to 1.0).
    pub fn from_coverage(coverage: f64) -> Self {
        if coverage >= 0.9 {
            AutonomyLevel::High
        } else if coverage >= 0.7 {
            AutonomyLevel::Medium
        } else if coverage >= 0.4 {
            AutonomyLevel::Low
        } else {
            AutonomyLevel::None
        }
    }
}

/// A single policy rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Human-readable name for this rule.
    pub name: String,
    /// Conditions that must be met for this rule to apply.
    pub requires: Vec<String>,
    /// What approval is needed.
    pub approval: Approval,
    /// Whether idempotency is required for this action.
    pub idempotency_required: bool,
    /// Minimum autonomy level required to bypass this rule.
    #[serde(default)]
    pub min_autonomy: Option<AutonomyLevel>,
}

/// Context for a policy evaluation request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// The capability being invoked.
    pub capability: String,
    /// Whether the action has side effects.
    pub has_side_effect: bool,
    /// Whether an idempotency key is provided.
    pub has_idempotency_key: bool,
    /// Tags/labels that describe the request context.
    pub tags: Vec<String>,
    /// Current autonomy level.
    pub autonomy_level: AutonomyLevel,
}

/// The decision after evaluating policies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Action is allowed to proceed.
    Allow,
    /// Action is denied with a reason.
    Deny { reason: String },
    /// Action requires human approval before proceeding.
    RequiresApproval { rule: String },
}

/// Errors from the policy engine.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// Failed to parse a policy rule.
    #[error("failed to parse policy: {detail}")]
    ParseError { detail: String },
}

/// The Policy Engine evaluates rules against request contexts.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    rules: Vec<PolicyRule>,
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PolicyEngine {
    /// Creates a new empty policy engine.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Creates a policy engine from a list of rules.
    pub fn with_rules(rules: Vec<PolicyRule>) -> Self {
        Self { rules }
    }

    /// Loads rules from YAML text.
    pub fn from_yaml(yaml: &str) -> Result<Self, PolicyError> {
        let rules: Vec<PolicyRule> =
            serde_yaml::from_str(yaml).map_err(|e| PolicyError::ParseError {
                detail: e.to_string(),
            })?;
        Ok(Self { rules })
    }

    /// Adds a rule to the engine.
    pub fn add_rule(&mut self, rule: PolicyRule) {
        self.rules.push(rule);
    }

    /// Returns all rules.
    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    /// Evaluates all applicable rules against the given context.
    ///
    /// A rule applies if ALL of its `requires` conditions are met by the context's tags
    /// or built-in conditions. If any applicable rule denies or requires approval,
    /// that decision is returned.
    pub fn evaluate(&self, ctx: &RequestContext) -> PolicyDecision {
        for rule in &self.rules {
            if !self.rule_applies(rule, ctx) {
                continue;
            }

            // Check idempotency requirement.
            if rule.idempotency_required && ctx.has_side_effect && !ctx.has_idempotency_key {
                return PolicyDecision::Deny {
                    reason: format!(
                        "rule '{}' requires idempotency key for side-effecting action",
                        rule.name
                    ),
                };
            }

            // Check if autonomy level allows bypassing this rule.
            if let Some(min_autonomy) = &rule.min_autonomy {
                if ctx.autonomy_level >= *min_autonomy {
                    continue; // Autonomy level sufficient, skip this rule.
                }
            }

            // Check approval requirement.
            if rule.approval == Approval::Human {
                return PolicyDecision::RequiresApproval {
                    rule: rule.name.clone(),
                };
            }
        }

        PolicyDecision::Allow
    }

    /// Checks if a rule's conditions are met by the context.
    fn rule_applies(&self, rule: &PolicyRule, ctx: &RequestContext) -> bool {
        rule.requires
            .iter()
            .all(|condition| match condition.as_str() {
                "side_effect" => ctx.has_side_effect,
                "no_side_effect" => !ctx.has_side_effect,
                _ => ctx.tags.contains(condition),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side_effect_rule() -> PolicyRule {
        PolicyRule {
            name: "require-idempotency-for-effects".to_string(),
            requires: vec!["side_effect".to_string()],
            approval: Approval::Auto,
            idempotency_required: true,
            min_autonomy: None,
        }
    }

    fn human_approval_rule() -> PolicyRule {
        PolicyRule {
            name: "high-risk-needs-human".to_string(),
            requires: vec!["high_risk".to_string()],
            approval: Approval::Human,
            idempotency_required: false,
            min_autonomy: None,
        }
    }

    fn autonomy_bypass_rule() -> PolicyRule {
        PolicyRule {
            name: "effect-approval-if-low-autonomy".to_string(),
            requires: vec!["side_effect".to_string()],
            approval: Approval::Human,
            idempotency_required: false,
            min_autonomy: Some(AutonomyLevel::Medium),
        }
    }

    fn ctx_safe() -> RequestContext {
        RequestContext {
            capability: "script.compute".to_string(),
            has_side_effect: false,
            has_idempotency_key: false,
            tags: vec![],
            autonomy_level: AutonomyLevel::Low,
        }
    }

    fn ctx_effect_no_key() -> RequestContext {
        RequestContext {
            capability: "tool.send-email".to_string(),
            has_side_effect: true,
            has_idempotency_key: false,
            tags: vec![],
            autonomy_level: AutonomyLevel::Low,
        }
    }

    fn ctx_effect_with_key() -> RequestContext {
        RequestContext {
            capability: "tool.send-email".to_string(),
            has_side_effect: true,
            has_idempotency_key: true,
            tags: vec![],
            autonomy_level: AutonomyLevel::Low,
        }
    }

    fn ctx_high_risk() -> RequestContext {
        RequestContext {
            capability: "tool.delete-data".to_string(),
            has_side_effect: true,
            has_idempotency_key: true,
            tags: vec!["high_risk".to_string()],
            autonomy_level: AutonomyLevel::Low,
        }
    }

    #[test]
    fn allows_safe_action() {
        let engine = PolicyEngine::with_rules(vec![side_effect_rule()]);
        let decision = engine.evaluate(&ctx_safe());
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn denies_side_effect_without_idempotency_key() {
        let engine = PolicyEngine::with_rules(vec![side_effect_rule()]);
        let decision = engine.evaluate(&ctx_effect_no_key());
        assert!(matches!(decision, PolicyDecision::Deny { .. }));
    }

    #[test]
    fn allows_side_effect_with_idempotency_key() {
        let engine = PolicyEngine::with_rules(vec![side_effect_rule()]);
        let decision = engine.evaluate(&ctx_effect_with_key());
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn requires_human_approval_for_high_risk() {
        let engine = PolicyEngine::with_rules(vec![human_approval_rule()]);
        let decision = engine.evaluate(&ctx_high_risk());
        assert!(matches!(decision, PolicyDecision::RequiresApproval { .. }));
    }

    #[test]
    fn high_risk_without_tag_is_allowed() {
        let engine = PolicyEngine::with_rules(vec![human_approval_rule()]);
        // No high_risk tag.
        let decision = engine.evaluate(&ctx_effect_with_key());
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn autonomy_level_bypasses_rule() {
        let engine = PolicyEngine::with_rules(vec![autonomy_bypass_rule()]);

        // Low autonomy: rule applies, requires approval.
        let mut ctx = ctx_effect_with_key();
        ctx.autonomy_level = AutonomyLevel::Low;
        let decision = engine.evaluate(&ctx);
        assert!(matches!(decision, PolicyDecision::RequiresApproval { .. }));

        // Medium autonomy: rule bypassed.
        ctx.autonomy_level = AutonomyLevel::Medium;
        let decision = engine.evaluate(&ctx);
        assert_eq!(decision, PolicyDecision::Allow);

        // High autonomy: rule bypassed.
        ctx.autonomy_level = AutonomyLevel::High;
        let decision = engine.evaluate(&ctx);
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn autonomy_level_from_coverage() {
        assert_eq!(AutonomyLevel::from_coverage(0.0), AutonomyLevel::None);
        assert_eq!(AutonomyLevel::from_coverage(0.3), AutonomyLevel::None);
        assert_eq!(AutonomyLevel::from_coverage(0.4), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_coverage(0.5), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_coverage(0.7), AutonomyLevel::Medium);
        assert_eq!(AutonomyLevel::from_coverage(0.8), AutonomyLevel::Medium);
        assert_eq!(AutonomyLevel::from_coverage(0.9), AutonomyLevel::High);
        assert_eq!(AutonomyLevel::from_coverage(1.0), AutonomyLevel::High);
    }

    #[test]
    fn empty_engine_allows_all() {
        let engine = PolicyEngine::new();
        let decision = engine.evaluate(&ctx_high_risk());
        assert_eq!(decision, PolicyDecision::Allow);
    }

    #[test]
    fn loads_from_yaml() {
        let yaml = r#"
- name: effect-guard
  requires: [side_effect]
  approval: auto
  idempotency_required: true
- name: risky-approval
  requires: [high_risk]
  approval: human
  idempotency_required: false
"#;
        let engine = PolicyEngine::from_yaml(yaml).unwrap();
        assert_eq!(engine.rules().len(), 2);
        assert_eq!(engine.rules()[0].name, "effect-guard");
        assert_eq!(engine.rules()[1].approval, Approval::Human);
    }

    #[test]
    fn add_rule_works() {
        let mut engine = PolicyEngine::new();
        engine.add_rule(side_effect_rule());
        assert_eq!(engine.rules().len(), 1);
    }

    #[test]
    fn multiple_conditions_must_all_match() {
        let rule = PolicyRule {
            name: "multi-condition".to_string(),
            requires: vec!["side_effect".to_string(), "critical".to_string()],
            approval: Approval::Human,
            idempotency_required: false,
            min_autonomy: None,
        };
        let engine = PolicyEngine::with_rules(vec![rule]);

        // Only side_effect, no "critical" tag.
        let decision = engine.evaluate(&ctx_effect_with_key());
        assert_eq!(decision, PolicyDecision::Allow);

        // Both conditions met.
        let mut ctx = ctx_effect_with_key();
        ctx.tags.push("critical".to_string());
        let decision = engine.evaluate(&ctx);
        assert!(matches!(decision, PolicyDecision::RequiresApproval { .. }));
    }
}
