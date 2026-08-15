//! Capability permissions — the allow-list layer of §45.
//!
//! Separate from [`crate::PolicyEngine`], which answers "what approval does this action
//! need". This module answers the prior question: "may this capability be invoked at
//! all". Keeping them apart matters because they fail in opposite directions — an
//! unknown capability is denied here, whereas an action with no matching policy rule is
//! allowed there.
//!
//! The file format is deliberately small, because a permission file that is hard to read
//! is a permission file nobody audits:
//!
//! ```yaml
//! rules:
//!   - capability: all
//!     effect: allow
//! ```

use serde::{Deserialize, Serialize};

/// What a matching rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
}

/// One permission rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// `all`, a `prefix.*` wildcard, or an exact capability id.
    pub capability: String,
    /// What happens when this rule matches.
    pub effect: Effect,
    /// Optional note explaining why the rule exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PermissionRule {
    /// Whether this rule's pattern covers `capability_id`.
    pub fn matches(&self, capability_id: &str) -> bool {
        match self.capability.as_str() {
            "all" | "*" => true,
            pattern => match pattern.strip_suffix('*') {
                Some(prefix) => capability_id.starts_with(prefix),
                None => pattern == capability_id,
            },
        }
    }
}

/// The permission decision for one capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// A rule allowed it.
    Allowed { rule: String },
    /// A rule denied it.
    Denied { rule: String },
    /// No rule matched. Denied, because the file is an allow-list.
    NotPermitted,
}

impl PermissionDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, PermissionDecision::Allowed { .. })
    }

    /// A short explanation suitable for an event payload or a CLI diagnostic.
    pub fn reason(&self) -> String {
        match self {
            PermissionDecision::Allowed { rule } => format!("allowed by rule '{rule}'"),
            PermissionDecision::Denied { rule } => format!("denied by rule '{rule}'"),
            PermissionDecision::NotPermitted => {
                "no permission rule matched; capability is not permitted".to_string()
            }
        }
    }
}

/// The loaded permission set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

impl Permissions {
    /// An empty set, which permits nothing.
    pub fn deny_all() -> Self {
        Self::default()
    }

    /// Parses a permissions document.
    pub fn from_yaml(yaml: &str) -> Result<Self, PermissionError> {
        serde_yaml::from_str(yaml).map_err(|e| PermissionError::Parse {
            detail: e.to_string(),
        })
    }

    /// Loads a permissions document from disk.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, PermissionError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| PermissionError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        Self::from_yaml(&text)
    }

    /// Evaluates a capability id against the rules, first match wins.
    pub fn evaluate(&self, capability_id: &str) -> PermissionDecision {
        for rule in &self.rules {
            if rule.matches(capability_id) {
                return match rule.effect {
                    Effect::Allow => PermissionDecision::Allowed {
                        rule: rule.capability.clone(),
                    },
                    Effect::Deny => PermissionDecision::Denied {
                        rule: rule.capability.clone(),
                    },
                };
            }
        }
        PermissionDecision::NotPermitted
    }

    /// Convenience predicate over [`Permissions::evaluate`].
    pub fn is_allowed(&self, capability_id: &str) -> bool {
        self.evaluate(capability_id).is_allowed()
    }

    /// Whether this set grants blanket access.
    ///
    /// Exposed so an operator surface can warn about it: a production profile running
    /// with `all: allow` has no capability gating at all.
    pub fn is_allow_all(&self) -> bool {
        self.rules.first().is_some_and(|r| {
            r.effect == Effect::Allow && matches!(r.capability.as_str(), "all" | "*")
        })
    }
}

/// Permission loading failures.
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("failed to read permissions file {path}: {detail}")]
    Io { path: String, detail: String },
    #[error("failed to parse permissions: {detail}")]
    Parse { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOW_ALL: &str = r#"
rules:
  - capability: all
    effect: allow
"#;

    #[test]
    fn allow_all_permits_every_capability() {
        let p = Permissions::from_yaml(ALLOW_ALL).unwrap();
        assert!(p.is_allowed("script.task-score"));
        assert!(p.is_allowed("effect.notify"));
        assert!(p.is_allowed("agent.groq.synthesize"));
        assert!(p.is_allow_all());
    }

    #[test]
    fn the_repository_permissions_file_parses_and_allows_all() {
        // Guards against the file drifting away from the loader that reads it.
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../policies/permissions.yaml"
        );
        let p = Permissions::load(path).expect("policies/permissions.yaml must parse");
        assert!(p.is_allow_all());
        assert!(p.is_allowed("script.anything"));
    }

    #[test]
    fn an_empty_set_permits_nothing() {
        let p = Permissions::deny_all();
        assert_eq!(p.evaluate("script.x"), PermissionDecision::NotPermitted);
        assert!(!p.is_allowed("script.x"));
        assert!(!p.is_allow_all());
    }

    #[test]
    fn first_match_wins() {
        let yaml = r#"
rules:
  - capability: effect.*
    effect: deny
    reason: side effects are gated in this profile
  - capability: all
    effect: allow
"#;
        let p = Permissions::from_yaml(yaml).unwrap();
        assert!(!p.is_allowed("effect.notify"));
        assert!(p.is_allowed("script.task-score"));
        // The deny rule is first, so this is not blanket allow.
        assert!(!p.is_allow_all());
    }

    #[test]
    fn prefix_wildcards_and_exact_ids_both_match() {
        let yaml = r#"
rules:
  - capability: script.task-score
    effect: allow
  - capability: verifier.*
    effect: allow
"#;
        let p = Permissions::from_yaml(yaml).unwrap();
        assert!(p.is_allowed("script.task-score"));
        assert!(p.is_allowed("verifier.task-result"));
        // Neither rule covers this one, so it falls through to not-permitted.
        assert!(!p.is_allowed("script.task-scorer"));
        assert!(!p.is_allowed("effect.notify"));
    }

    #[test]
    fn decisions_explain_themselves() {
        let p = Permissions::from_yaml(ALLOW_ALL).unwrap();
        assert!(p.evaluate("script.x").reason().contains("rule 'all'"));
        assert!(Permissions::deny_all()
            .evaluate("script.x")
            .reason()
            .contains("not permitted"));
    }

    #[test]
    fn malformed_documents_are_rejected() {
        assert!(Permissions::from_yaml("rules: [unclosed").is_err());
        // An unknown effect must not silently become "allow".
        assert!(
            Permissions::from_yaml("rules:\n  - capability: all\n    effect: maybe\n").is_err()
        );
    }

    #[test]
    fn round_trips_through_yaml() {
        let p = Permissions::from_yaml(ALLOW_ALL).unwrap();
        let text = serde_yaml::to_string(&p).unwrap();
        assert_eq!(Permissions::from_yaml(&text).unwrap(), p);
    }
}
