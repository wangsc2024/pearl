//! Prompt rendering — Article 3.
//!
//! Article 3 keeps infrastructure out of prompts. The mirror of that rule is keeping prompts
//! out of code: a prompt embedded in a Rust string literal cannot be reviewed as content,
//! cannot be changed without a rebuild, and cannot be diffed as a prompt revision. So an
//! agent capability's entrypoint is a template file, and the task payload is rendered into it.
//!
//! The syntax is deliberately the smallest thing that works:
//!
//! ```text
//! Score task {{task_id}} of type {{task_type}}.
//! Payload: {{payload}}
//! ```
//!
//! - `{{key}}` is replaced by the payload's top-level value for `key`.
//! - `{{payload}}` is the whole payload as pretty JSON.
//! - An unknown placeholder is an error, not an empty string: a prompt that silently lost a
//!   variable would produce a plausible-looking result built on missing information.

use crate::{RuntimeError, ScriptSpec};

/// The placeholder that expands to the entire payload.
const WHOLE_PAYLOAD: &str = "payload";

/// Checks that the prompt template exists and every placeholder can be filled.
pub fn validate(spec: &ScriptSpec) -> Result<(), RuntimeError> {
    let template = read_template(spec)?;
    let payload = spec
        .input_payload
        .clone()
        .unwrap_or(serde_json::Value::Null);
    for name in placeholders(&template) {
        resolve(&name, &payload).ok_or_else(|| RuntimeError::Validation {
            detail: format!(
                "prompt {} references {{{{{name}}}}}, which the task payload does not provide",
                spec.entrypoint.display()
            ),
        })?;
    }
    Ok(())
}

/// Renders the prompt template named by the spec's entrypoint.
pub fn render(spec: &ScriptSpec) -> Result<String, RuntimeError> {
    let template = read_template(spec)?;
    let payload = spec
        .input_payload
        .clone()
        .unwrap_or(serde_json::Value::Null);

    let mut rendered = template.clone();
    for name in placeholders(&template) {
        let value = resolve(&name, &payload).ok_or_else(|| RuntimeError::Validation {
            detail: format!(
                "prompt {} references {{{{{name}}}}}, which the task payload does not provide",
                spec.entrypoint.display()
            ),
        })?;
        rendered = rendered.replace(&format!("{{{{{name}}}}}"), &value);
    }

    if rendered.trim().is_empty() {
        return Err(RuntimeError::Validation {
            detail: format!("prompt {} is empty", spec.entrypoint.display()),
        });
    }
    Ok(rendered)
}

fn read_template(spec: &ScriptSpec) -> Result<String, RuntimeError> {
    std::fs::read_to_string(&spec.entrypoint).map_err(|e| RuntimeError::Validation {
        detail: format!(
            "prompt template {} could not be read: {e}",
            spec.entrypoint.display()
        ),
    })
}

/// Every `{{name}}` in the template, in order, deduplicated.
fn placeholders(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim().to_string();
        if !name.is_empty() && !found.contains(&name) {
            found.push(name);
        }
        rest = &after[end + 2..];
    }
    found
}

/// The replacement for one placeholder, or `None` when the payload has no such key.
fn resolve(name: &str, payload: &serde_json::Value) -> Option<String> {
    if name == WHOLE_PAYLOAD {
        return Some(serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string()));
    }
    let value = payload.get(name)?;
    Some(match value {
        // A string interpolates as itself: quoting it would put JSON syntax into prose.
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use pearl_governance::manifest::Runtime;
    use std::collections::BTreeMap;
    use std::path::Path;

    fn spec(prompt: &Path, payload: Option<serde_json::Value>) -> ScriptSpec {
        ScriptSpec {
            runtime: Runtime::Groq,
            entrypoint: prompt.to_path_buf(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: payload,
        }
    }

    fn with_template(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn placeholders_are_filled_from_the_payload() {
        let (_dir, path) = with_template("Score {{task_id}} of type {{task_type}}.");
        let rendered = render(&spec(
            &path,
            Some(serde_json::json!({ "task_id": "t-1", "task_type": "digest" })),
        ))
        .unwrap();
        assert_eq!(rendered, "Score t-1 of type digest.");
    }

    #[test]
    fn a_string_interpolates_without_quotes_and_a_number_as_itself() {
        let (_dir, path) = with_template("{{name}} scored {{score}}");
        let rendered = render(&spec(
            &path,
            Some(serde_json::json!({ "name": "alpha", "score": 8.28 })),
        ))
        .unwrap();
        assert_eq!(rendered, "alpha scored 8.28");
    }

    #[test]
    fn the_whole_payload_is_available() {
        let (_dir, path) = with_template("Data:\n{{payload}}");
        let rendered = render(&spec(&path, Some(serde_json::json!({ "a": 1 })))).unwrap();
        assert!(rendered.contains("\"a\": 1"), "got {rendered}");
    }

    #[test]
    fn a_repeated_placeholder_is_filled_everywhere() {
        let (_dir, path) = with_template("{{id}} and again {{id}}");
        let rendered = render(&spec(&path, Some(serde_json::json!({ "id": "x" })))).unwrap();
        assert_eq!(rendered, "x and again x");
    }

    #[test]
    fn an_unknown_placeholder_is_an_error_rather_than_an_empty_string() {
        // Silently dropping it would produce a plausible answer built on missing input.
        let (_dir, path) = with_template("Score {{missing_key}}");
        let err = render(&spec(&path, Some(serde_json::json!({ "task_id": "t" })))).unwrap_err();
        assert!(err.to_string().contains("missing_key"), "got {err}");
        assert!(validate(&spec(&path, Some(serde_json::json!({})))).is_err());
    }

    #[test]
    fn validation_catches_a_bad_prompt_before_the_call_is_made() {
        // The point of validating separately: no tokens are spent discovering this.
        let (_dir, path) = with_template("{{absent}}");
        assert!(validate(&spec(&path, None)).is_err());
    }

    #[test]
    fn a_missing_template_is_reported_with_its_path() {
        let err = render(&spec(Path::new("/no/such/prompt.md"), None)).unwrap_err();
        assert!(err.to_string().contains("prompt.md"), "got {err}");
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        let (_dir, path) = with_template("   \n  ");
        assert!(render(&spec(&path, None)).is_err());
    }

    #[test]
    fn a_template_with_no_placeholders_renders_verbatim() {
        let (_dir, path) = with_template("Summarise the attached data.");
        assert_eq!(
            render(&spec(&path, None)).unwrap(),
            "Summarise the attached data."
        );
    }

    #[test]
    fn unbalanced_braces_do_not_panic() {
        let (_dir, path) = with_template("{{unterminated and {{id}}");
        // The parser stops at the first unterminated placeholder rather than looping.
        let result = render(&spec(&path, Some(serde_json::json!({ "id": "x" }))));
        assert!(result.is_err() || result.unwrap().contains("x"));
    }
}
