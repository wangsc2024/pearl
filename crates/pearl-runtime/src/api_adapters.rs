//! OpenAI-compatible API runtimes — §37.
//!
//! Groq, Mistral, NVIDIA, a generic endpoint and a local `llama-server` all speak the same
//! chat-completions protocol, so one adapter serves them; only the base URL, the key and the
//! default model differ. Four copies of the same request builder would be four places for a
//! header to go stale.
//!
//! Article 9 requires cancellability. An HTTP call cannot be signalled, so the deadline is
//! the request timeout: the call is bounded, and a bounded call is one the worker can survive.
//!
//! Two decisions that are easy to get wrong:
//!
//! **Nothing is spent by accident.** A provider with no key configured refuses before the
//! request is built. There is no fallback to another provider and no default key, because a
//! silent switch to a paid endpoint is exactly the surprise an operator must not get.
//!
//! **Failure is not laundered into an empty answer.** A non-2xx response, a refusal, or a
//! response with no content all produce a non-zero exit status, so the worker's verdict logic
//! sees a failure rather than a successful empty result.

use std::time::Duration;

use chrono::TimeDelta;
use pearl_core::Clock;
use pearl_governance::manifest::Runtime;

use crate::family::{family_of, ApiProvider, RuntimeFamily};
use crate::{RuntimeAdapter, RuntimeError, RuntimeExitStatus, RuntimeResult, ScriptSpec};

/// Exit code used when the endpoint answered, but not with a usable completion.
const EXIT_NO_COMPLETION: i32 = 1;
/// Exit code used when the endpoint rejected the request.
const EXIT_REJECTED: i32 = 2;

/// Calls an OpenAI-compatible chat-completions endpoint.
pub struct ApiRuntimeAdapter {
    provider: ApiProvider,
    /// Sampling temperature. Zero by default: a runtime whose output is verified downstream
    /// should be as reproducible as the provider allows.
    temperature: f64,
    max_tokens: Option<u32>,
}

impl ApiRuntimeAdapter {
    pub fn new(provider: ApiProvider) -> Self {
        Self {
            provider,
            temperature: 0.0,
            max_tokens: None,
        }
    }

    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = temperature;
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn provider(&self) -> ApiProvider {
        self.provider
    }

    /// The resolved endpoint, or an explanation of what is missing.
    pub fn endpoint(&self) -> Result<String, RuntimeError> {
        let base = non_empty_env(self.provider.base_var())
            .or_else(|| self.provider.default_base().map(str::to_string))
            .ok_or_else(|| RuntimeError::Validation {
                detail: format!(
                    "{} has no endpoint: set {}",
                    self.provider.as_str(),
                    self.provider.base_var()
                ),
            })?;
        Ok(format!("{}/chat/completions", base.trim_end_matches('/')))
    }

    /// The resolved model, or an explanation of what is missing.
    pub fn model(&self) -> Result<String, RuntimeError> {
        non_empty_env(self.provider.model_var()).ok_or_else(|| RuntimeError::Validation {
            detail: format!(
                "{} has no model: set {}",
                self.provider.as_str(),
                self.provider.model_var()
            ),
        })
    }

    /// The credential, when the provider needs one.
    fn key(&self) -> Result<Option<String>, RuntimeError> {
        match non_empty_env(self.provider.key_var()) {
            Some(key) => Ok(Some(key)),
            None if self.provider.requires_key() => Err(RuntimeError::Validation {
                detail: format!(
                    "{} has no credential: set {}",
                    self.provider.as_str(),
                    self.provider.key_var()
                ),
            }),
            None => Ok(None),
        }
    }

    /// Whether this provider is configured well enough to be used.
    ///
    /// Exposed so an operator surface can report readiness without making a call.
    pub fn is_configured(&self) -> bool {
        self.endpoint().is_ok() && self.model().is_ok() && self.key().is_ok()
    }

    /// The request body.
    fn body(&self, model: &str, prompt: &str) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": model,
            "temperature": self.temperature,
            "messages": [{ "role": "user", "content": prompt }],
        });
        if let Some(max) = self.max_tokens {
            body["max_tokens"] = max.into();
        }
        body
    }
}

impl RuntimeAdapter for ApiRuntimeAdapter {
    fn execute(&self, spec: &ScriptSpec, clock: &dyn Clock) -> Result<RuntimeResult, RuntimeError> {
        self.validate(spec)?;

        let endpoint = self.endpoint()?;
        let model = self.model()?;
        let key = self.key()?;
        let prompt = crate::prompt::render(spec)?;
        let started = clock.now();

        let agent = ureq::Agent::config_builder()
            // Article 9: an HTTP call cannot be signalled, so the deadline *is* the timeout.
            .timeout_global(Some(to_std_duration(spec.timeout)))
            .build()
            .new_agent();

        let mut request = agent
            .post(&endpoint)
            .header("content-type", "application/json");
        if let Some(key) = &key {
            request = request.header("authorization", &format!("Bearer {key}"));
        }

        let outcome = request.send_json(self.body(&model, &prompt));
        let duration = clock.now() - started;

        match outcome {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().map_err(|e| {
                    RuntimeError::OutputParse {
                        detail: format!("could not read the response body: {e}"),
                    }
                })?;
                Ok(self.interpret(status, &text, duration, &model))
            }
            Err(ureq::Error::StatusCode(code)) => Ok(RuntimeResult {
                exit_status: RuntimeExitStatus::Exited {
                    code: EXIT_REJECTED,
                },
                stdout: String::new(),
                stderr: format!(
                    "{} rejected the request with HTTP {code}",
                    self.provider.as_str()
                ),
                duration,
                structured_output: None,
            }),
            Err(ureq::Error::Timeout(_)) => Ok(RuntimeResult {
                exit_status: RuntimeExitStatus::TimedOut,
                stdout: String::new(),
                stderr: format!(
                    "{} did not answer within {}s",
                    self.provider.as_str(),
                    spec.timeout.num_seconds()
                ),
                duration,
                structured_output: None,
            }),
            // A transport failure is not a verdict about the work: the call never completed,
            // so it is an adapter error rather than a failed execution.
            Err(e) => Err(RuntimeError::Validation {
                detail: format!("{} could not be reached: {e}", self.provider.as_str()),
            }),
        }
    }

    fn validate(&self, spec: &ScriptSpec) -> Result<(), RuntimeError> {
        if !self.supports_runtime(spec.runtime) {
            return Err(RuntimeError::UnsupportedRuntime {
                runtime: spec.runtime.as_str().to_string(),
            });
        }
        if spec.timeout <= TimeDelta::zero() {
            return Err(RuntimeError::Validation {
                detail: "timeout must be positive".to_string(),
            });
        }
        // Configuration and prompt are checked before any request: discovering a bad prompt
        // by spending tokens on it is avoidable.
        self.endpoint()?;
        self.model()?;
        self.key()?;
        crate::prompt::validate(spec)
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        matches!(family_of(runtime), RuntimeFamily::Api(p) if p == self.provider)
    }
}

impl ApiRuntimeAdapter {
    /// Turns a response into a runtime result.
    ///
    /// The completion text becomes stdout; a completion that *is* JSON also becomes the
    /// structured output, so a capability whose prompt asks for JSON gets the same
    /// machine-readable contract a script would (§26).
    fn interpret(
        &self,
        status: u16,
        body: &str,
        duration: TimeDelta,
        model: &str,
    ) -> RuntimeResult {
        if !(200..300).contains(&status) {
            return RuntimeResult {
                exit_status: RuntimeExitStatus::Exited {
                    code: EXIT_REJECTED,
                },
                stdout: String::new(),
                stderr: format!("HTTP {status}: {}", truncate(body, 500)),
                duration,
                structured_output: None,
            };
        }

        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(e) => {
                return RuntimeResult {
                    exit_status: RuntimeExitStatus::Exited {
                        code: EXIT_NO_COMPLETION,
                    },
                    stdout: String::new(),
                    stderr: format!("response was not JSON: {e}"),
                    duration,
                    structured_output: None,
                }
            }
        };

        let content = parsed
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();

        if content.is_empty() {
            // An empty completion is a failure, not a successful empty answer. Reporting
            // success here would let a refusal or a truncated response pass verification as
            // "the model produced nothing, which conforms to nothing".
            let finish = parsed
                .pointer("/choices/0/finish_reason")
                .and_then(|f| f.as_str())
                .unwrap_or("unknown");
            return RuntimeResult {
                exit_status: RuntimeExitStatus::Exited {
                    code: EXIT_NO_COMPLETION,
                },
                stdout: String::new(),
                stderr: format!("{} returned no content (finish_reason: {finish})", model),
                duration,
                structured_output: usage_of(&parsed),
            };
        }

        // Models often fence JSON in ```json blocks; unwrapping is the difference between a
        // usable structured output and a verifier that fails on backticks.
        let structured = parse_json_payload(&content).or_else(|| usage_of(&parsed));

        RuntimeResult {
            exit_status: RuntimeExitStatus::Exited { code: 0 },
            stdout: content,
            stderr: String::new(),
            duration,
            structured_output: structured.map(|mut value| {
                if let (Some(map), Some(usage)) = (value.as_object_mut(), usage_of(&parsed)) {
                    // Budget accounting needs the usage, and it cannot be recovered later.
                    map.entry("usage").or_insert_with(|| usage["usage"].clone());
                }
                value
            }),
        }
    }
}

/// The `usage` block, wrapped so it can stand alone as structured output.
fn usage_of(response: &serde_json::Value) -> Option<serde_json::Value> {
    let usage = response.get("usage")?.clone();
    Some(serde_json::json!({ "usage": usage }))
}

/// Parses a completion as JSON, unwrapping a fenced code block if present.
fn parse_json_payload(content: &str) -> Option<serde_json::Value> {
    let trimmed = content.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("```") {
        // ```json\n{...}\n``` or ```\n{...}\n```
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        rest.trim_start_matches(['\r', '\n'])
            .trim_end()
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    if !(candidate.starts_with('{') || candidate.starts_with('[')) {
        return None;
    }
    serde_json::from_str(candidate).ok()
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn to_std_duration(delta: TimeDelta) -> Duration {
    Duration::from_millis(delta.num_milliseconds().max(1) as u64)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// Sets provider environment for the duration of a closure, then restores it.
    ///
    /// Environment is process-global, so tests that touch it must put it back or they break
    /// their neighbours.
    fn with_env<T>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let saved: Vec<(String, Option<String>)> = pairs
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let result = f();
        for (key, value) in saved {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
        result
    }

    fn spec(prompt: &Path, runtime: Runtime) -> ScriptSpec {
        ScriptSpec {
            runtime,
            entrypoint: prompt.to_path_buf(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: Some(serde_json::json!({ "task_id": "t-1" })),
        }
    }

    fn template() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.md");
        std::fs::write(&path, "Summarise {{task_id}}").unwrap();
        (dir, path)
    }

    #[test]
    fn each_adapter_supports_only_its_own_provider() {
        let groq = ApiRuntimeAdapter::new(ApiProvider::Groq);
        assert!(groq.supports_runtime(Runtime::Groq));
        assert!(!groq.supports_runtime(Runtime::Mistral));
        assert!(!groq.supports_runtime(Runtime::Python));
        assert!(!groq.supports_runtime(Runtime::ClaudeCode));
    }

    #[test]
    fn a_cloud_provider_resolves_its_published_endpoint() {
        with_env(&[("GROQ_API_BASE", None)], || {
            let endpoint = ApiRuntimeAdapter::new(ApiProvider::Groq)
                .endpoint()
                .unwrap();
            assert_eq!(endpoint, "https://api.groq.com/openai/v1/chat/completions");
        });
    }

    #[test]
    fn the_endpoint_is_overridable_and_trailing_slashes_do_not_double_up() {
        with_env(
            &[("GROQ_API_BASE", Some("http://localhost:8080/v1/"))],
            || {
                assert_eq!(
                    ApiRuntimeAdapter::new(ApiProvider::Groq)
                        .endpoint()
                        .unwrap(),
                    "http://localhost:8080/v1/chat/completions"
                );
            },
        );
    }

    #[test]
    fn a_provider_with_no_endpoint_says_which_variable_to_set() {
        with_env(&[("OPENAI_API_BASE", None)], || {
            let err = ApiRuntimeAdapter::new(ApiProvider::OpenAiCompatible)
                .endpoint()
                .unwrap_err();
            assert!(err.to_string().contains("OPENAI_API_BASE"), "got {err}");
        });
    }

    #[test]
    fn a_missing_credential_is_refused_before_any_request_is_built() {
        // The property that matters: no key, no call, no cost.
        let (_dir, prompt) = template();
        with_env(
            &[
                ("MISTRAL_API_KEY", None),
                ("MISTRAL_MODEL", Some("mistral-small-latest")),
            ],
            || {
                let err = ApiRuntimeAdapter::new(ApiProvider::Mistral)
                    .validate(&spec(&prompt, Runtime::Mistral))
                    .unwrap_err();
                assert!(err.to_string().contains("MISTRAL_API_KEY"), "got {err}");
            },
        );
    }

    #[test]
    fn a_local_model_needs_no_credential() {
        let (_dir, prompt) = template();
        with_env(
            &[
                ("LLAMA_API_KEY", None),
                ("LLAMA_API_BASE", Some("http://localhost:8080/v1")),
                ("LLAMA_MODEL", Some("qwen2.5")),
            ],
            || {
                // Offline use must remain possible; demanding a key would make it impossible.
                assert!(ApiRuntimeAdapter::new(ApiProvider::LlamaCpp)
                    .validate(&spec(&prompt, Runtime::LlamaCpp))
                    .is_ok());
            },
        );
    }

    #[test]
    fn a_missing_model_is_refused() {
        let (_dir, prompt) = template();
        with_env(
            &[("NVIDIA_API_KEY", Some("k")), ("NVIDIA_MODEL", None)],
            || {
                let err = ApiRuntimeAdapter::new(ApiProvider::Nvidia)
                    .validate(&spec(&prompt, Runtime::Nvidia))
                    .unwrap_err();
                assert!(err.to_string().contains("NVIDIA_MODEL"), "got {err}");
            },
        );
    }

    #[test]
    fn readiness_is_reportable_without_making_a_call() {
        with_env(
            &[
                ("GROQ_API_KEY", Some("k")),
                ("GROQ_MODEL", Some("llama-3.3-70b-versatile")),
            ],
            || assert!(ApiRuntimeAdapter::new(ApiProvider::Groq).is_configured()),
        );
        with_env(&[("GROQ_API_KEY", None)], || {
            assert!(!ApiRuntimeAdapter::new(ApiProvider::Groq).is_configured());
        });
    }

    #[test]
    fn the_request_body_is_deterministic_by_default() {
        let adapter = ApiRuntimeAdapter::new(ApiProvider::Groq);
        let body = adapter.body("m", "hello");
        // Temperature zero: output that will be machine-verified should be as reproducible
        // as the provider allows.
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("max_tokens").is_none());

        let capped = ApiRuntimeAdapter::new(ApiProvider::Groq).with_max_tokens(256);
        assert_eq!(capped.body("m", "hi")["max_tokens"], 256);
    }

    // --- response interpretation, without a network ---

    fn interpret(body: &str, status: u16) -> RuntimeResult {
        ApiRuntimeAdapter::new(ApiProvider::Groq).interpret(
            status,
            body,
            TimeDelta::try_seconds(1).unwrap(),
            "m",
        )
    }

    #[test]
    fn a_text_completion_becomes_stdout() {
        let result = interpret(
            r#"{"choices":[{"message":{"content":"a summary"}}],"usage":{"total_tokens":42}}"#,
            200,
        );
        assert!(result.is_success());
        assert_eq!(result.stdout, "a summary");
        // Usage survives even for a text answer, because budget accounting needs it.
        assert_eq!(
            result.structured_output.unwrap()["usage"]["total_tokens"],
            42
        );
    }

    #[test]
    fn a_json_completion_becomes_structured_output() {
        let result = interpret(
            r#"{"choices":[{"message":{"content":"{\"score\": 7}"}}],"usage":{"total_tokens":9}}"#,
            200,
        );
        assert!(result.is_success());
        let output = result.structured_output.unwrap();
        assert_eq!(output["score"], 7);
        assert_eq!(output["usage"]["total_tokens"], 9);
    }

    #[test]
    fn a_fenced_json_block_is_unwrapped() {
        // Models fence JSON constantly; a verifier that failed on backticks would be
        // rejecting well-formed answers.
        let result = interpret(
            r#"{"choices":[{"message":{"content":"```json\n{\"ok\": true}\n```"}}]}"#,
            200,
        );
        assert!(result.is_success());
        assert_eq!(result.structured_output.unwrap()["ok"], true);
    }

    #[test]
    fn an_empty_completion_is_a_failure_not_an_empty_success() {
        let result = interpret(
            r#"{"choices":[{"message":{"content":""},"finish_reason":"length"}]}"#,
            200,
        );
        assert!(!result.is_success());
        assert!(result.stderr.contains("length"), "got {}", result.stderr);
    }

    #[test]
    fn a_rejection_is_reported_with_its_status() {
        let result = interpret(r#"{"error":{"message":"invalid model"}}"#, 400);
        assert!(!result.is_success());
        assert!(result.stderr.contains("400"), "got {}", result.stderr);
        assert!(
            result.stderr.contains("invalid model"),
            "got {}",
            result.stderr
        );
    }

    #[test]
    fn a_non_json_body_is_a_failure_rather_than_a_panic() {
        let result = interpret("<html>gateway timeout</html>", 200);
        assert!(!result.is_success());
        assert!(result.stderr.contains("not JSON"), "got {}", result.stderr);
    }

    #[test]
    fn json_payload_parsing_is_conservative() {
        assert!(parse_json_payload("just prose").is_none());
        assert!(parse_json_payload("{not json}").is_none());
        assert_eq!(parse_json_payload(r#"{"a":1}"#).unwrap()["a"], 1);
        assert_eq!(parse_json_payload("```\n[1,2]\n```").unwrap()[1], 2);
    }
}
