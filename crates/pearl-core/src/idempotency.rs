//! Idempotency keys — Constitution Article 5.
//!
//! Every external side effect carries a key so that runtime retry cannot produce a
//! duplicate effect. The key is the deduplication identity; the effect ledger is
//! consulted before an effect is committed.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A key that uniquely identifies one intended side effect.
///
/// Shape: `{effect}:{target}[:{scope}...]`, e.g.
/// `todoist:complete:task_123:run_456`, `ntfy:daily_digest:2026-08-15`.
///
/// `:` is the separator, so no segment may contain one — that restriction is what makes
/// the key parseable back into its parts for auditing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Builds a key from ordered segments.
    pub fn new<I, S>(segments: I) -> Result<Self, IdempotencyKeyError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let parts: Vec<String> = segments
            .into_iter()
            .map(|s| s.as_ref().to_string())
            .collect();

        if parts.len() < 2 {
            return Err(IdempotencyKeyError::TooFewSegments(parts.len()));
        }
        for part in &parts {
            if part.is_empty() {
                return Err(IdempotencyKeyError::EmptySegment);
            }
            if part.contains(':') {
                return Err(IdempotencyKeyError::SeparatorInSegment(part.clone()));
            }
        }
        Ok(Self(parts.join(":")))
    }

    /// Parses an existing key string.
    pub fn parse(raw: &str) -> Result<Self, IdempotencyKeyError> {
        Self::new(raw.split(':'))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The leading segment, naming the kind of effect.
    pub fn effect(&self) -> &str {
        self.0.split(':').next().unwrap_or_default()
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split(':')
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a key was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdempotencyKeyError {
    #[error("idempotency key needs at least 2 segments, got {0}")]
    TooFewSegments(usize),
    #[error("idempotency key segments must not be empty")]
    EmptySegment,
    #[error("segment '{0}' contains the ':' separator")]
    SeparatorInSegment(String),
    #[error("template placeholder '{0}' was not supplied")]
    UnresolvedPlaceholder(String),
}

/// A key template declared on a capability, e.g. `todoist:complete:{task_id}:{run_id}`.
///
/// Templates are declared in the manifest rather than built ad hoc at the call site so
/// that the CI gate can verify a side-effecting capability has *some* key before it ever
/// runs, rather than discovering the omission after a duplicate notification is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyTemplate(String);

impl IdempotencyTemplate {
    pub fn new(template: impl Into<String>) -> Self {
        Self(template.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The placeholder names this template expects.
    pub fn placeholders(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = self.0.as_str();
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close) => {
                    found.push(after[..close].to_string());
                    rest = &after[close + 1..];
                }
                None => break,
            }
        }
        found
    }

    /// Substitutes placeholders and validates the result.
    ///
    /// An unsupplied placeholder is an error rather than being left literal: a key
    /// containing `{run_id}` would silently deduplicate *every* run against every other,
    /// which is the opposite of what Article 5 asks for.
    pub fn render(&self, bindings: &[(&str, &str)]) -> Result<IdempotencyKey, IdempotencyKeyError> {
        let mut rendered = self.0.clone();
        for (name, value) in bindings {
            rendered = rendered.replace(&format!("{{{name}}}"), value);
        }
        if let Some(open) = rendered.find('{') {
            let tail = &rendered[open + 1..];
            let name = tail.split('}').next().unwrap_or_default();
            return Err(IdempotencyKeyError::UnresolvedPlaceholder(name.to_string()));
        }
        IdempotencyKey::parse(&rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_key_from_segments() {
        let key = IdempotencyKey::new(["todoist", "complete", "task_123", "run_456"]).unwrap();
        assert_eq!(key.as_str(), "todoist:complete:task_123:run_456");
        assert_eq!(key.effect(), "todoist");
    }

    #[test]
    fn rejects_single_segment_keys() {
        assert_eq!(
            IdempotencyKey::new(["todoist"]),
            Err(IdempotencyKeyError::TooFewSegments(1))
        );
    }

    #[test]
    fn rejects_separator_inside_a_segment() {
        // Otherwise "a:b" + "c" and "a" + "b:c" would collide.
        assert_eq!(
            IdempotencyKey::new(["ntfy", "a:b"]),
            Err(IdempotencyKeyError::SeparatorInSegment("a:b".into()))
        );
    }

    #[test]
    fn rejects_empty_segments() {
        assert_eq!(
            IdempotencyKey::parse("ntfy::digest"),
            Err(IdempotencyKeyError::EmptySegment)
        );
    }

    #[test]
    fn key_round_trips_through_parse() {
        let raw = "ntfy:daily_digest:2026-08-15";
        assert_eq!(IdempotencyKey::parse(raw).unwrap().as_str(), raw);
    }

    #[test]
    fn equal_keys_deduplicate_and_differing_keys_do_not() {
        let a = IdempotencyKey::parse("ntfy:digest:2026-08-15").unwrap();
        let b = IdempotencyKey::parse("ntfy:digest:2026-08-15").unwrap();
        let c = IdempotencyKey::parse("ntfy:digest:2026-08-16").unwrap();

        assert_eq!(a, b, "same effect on same day must be one effect");
        assert_ne!(a, c, "a different day is a different effect");
    }

    #[test]
    fn template_reports_its_placeholders() {
        let t = IdempotencyTemplate::new("todoist:complete:{task_id}:{run_id}");
        assert_eq!(t.placeholders(), vec!["task_id", "run_id"]);
    }

    #[test]
    fn template_renders_with_bindings() {
        let t = IdempotencyTemplate::new("todoist:complete:{task_id}:{run_id}");
        let key = t
            .render(&[("task_id", "task_123"), ("run_id", "run_456")])
            .unwrap();
        assert_eq!(key.as_str(), "todoist:complete:task_123:run_456");
    }

    #[test]
    fn unresolved_placeholder_is_an_error_not_a_literal() {
        let t = IdempotencyTemplate::new("todoist:complete:{task_id}:{run_id}");
        let err = t.render(&[("task_id", "task_123")]).unwrap_err();
        assert_eq!(
            err,
            IdempotencyKeyError::UnresolvedPlaceholder("run_id".into())
        );
    }

    #[test]
    fn same_bindings_render_the_same_key() {
        let t = IdempotencyTemplate::new("ntfy:{channel}:{date}");
        let bindings = [("channel", "digest"), ("date", "2026-08-15")];
        assert_eq!(t.render(&bindings).unwrap(), t.render(&bindings).unwrap());
    }
}
