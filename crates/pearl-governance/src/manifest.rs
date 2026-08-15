//! Capability manifest parsing — mirrors `schemas/capability-manifest-v1.json`.
//!
//! The manifest is the declaration a capability makes about itself. The Constitution
//! checks in [`crate::checks`] read these declarations, which is why the type is
//! permissive about *shape* but the checks are strict about *content*: a manifest that
//! omitted a required field would otherwise fail to parse and never reach the check that
//! explains what is wrong.

use pearl_core::IdempotencyTemplate;
use serde::{Deserialize, Serialize};

/// What kind of capability this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityType {
    Script,
    Tool,
    Verifier,
    Skill,
    Agent,
    Workflow,
    Runtime,
    Guard,
}

impl CapabilityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityType::Script => "script",
            CapabilityType::Tool => "tool",
            CapabilityType::Verifier => "verifier",
            CapabilityType::Skill => "skill",
            CapabilityType::Agent => "agent",
            CapabilityType::Workflow => "workflow",
            CapabilityType::Runtime => "runtime",
            CapabilityType::Guard => "guard",
        }
    }
}

/// How a capability is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Script,
    Tool,
    Agent,
    Workflow,
    HumanGate,
}

impl ExecutionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionKind::Script => "script",
            ExecutionKind::Tool => "tool",
            ExecutionKind::Agent => "agent",
            ExecutionKind::Workflow => "workflow",
            ExecutionKind::HumanGate => "human_gate",
        }
    }

    /// Whether this kind involves an LLM.
    ///
    /// Article 1 turns on this distinction: deterministic work must not be routed to
    /// anything that reasons.
    pub fn involves_llm(&self) -> bool {
        matches!(self, ExecutionKind::Agent)
    }
}

/// Which runtime executes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Rust,
    Python,
    Powershell,
    Shell,
    ClaudeCode,
    Codex,
    Cursor,
    OpenaiCompatible,
    /// Groq's OpenAI-compatible endpoint.
    ///
    /// Named explicitly rather than folded into `openai_compatible` so that a manifest, a
    /// permission rule and a routing decision can all speak about one provider. They differ
    /// in cost, rate limit and available models, which are exactly the things policy cares
    /// about.
    Groq,
    Mistral,
    Nvidia,
    LlamaCpp,
    Native,
}

impl Runtime {
    pub fn as_str(&self) -> &'static str {
        match self {
            Runtime::Rust => "rust",
            Runtime::Python => "python",
            Runtime::Powershell => "powershell",
            Runtime::Shell => "shell",
            Runtime::ClaudeCode => "claude_code",
            Runtime::Codex => "codex",
            Runtime::Cursor => "cursor",
            Runtime::OpenaiCompatible => "openai_compatible",
            Runtime::Groq => "groq",
            Runtime::Mistral => "mistral",
            Runtime::Nvidia => "nvidia",
            Runtime::LlamaCpp => "llama_cpp",
            Runtime::Native => "native",
        }
    }

    /// Whether this runtime is a mechanical script runtime (§24).
    pub fn is_mechanical(&self) -> bool {
        matches!(
            self,
            Runtime::Rust
                | Runtime::Python
                | Runtime::Powershell
                | Runtime::Shell
                | Runtime::Native
        )
    }
}

/// Functional classification within the registry — §27.
///
/// Separate from [`CapabilityType`] because the two answer different questions: the type
/// says how the registry treats it, this says what it does to the world. A `script` that
/// is an `effect` needs an idempotency key; a `script` that is a `collector` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionalKind {
    Collector,
    Transformer,
    Validator,
    Verifier,
    Guard,
    Effect,
    Repair,
    Migration,
    Diagnostic,
}

impl FunctionalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FunctionalKind::Collector => "collector",
            FunctionalKind::Transformer => "transformer",
            FunctionalKind::Validator => "validator",
            FunctionalKind::Verifier => "verifier",
            FunctionalKind::Guard => "guard",
            FunctionalKind::Effect => "effect",
            FunctionalKind::Repair => "repair",
            FunctionalKind::Migration => "migration",
            FunctionalKind::Diagnostic => "diagnostic",
        }
    }
}

/// Where the executable actually is — §25.
///
/// Without this the registry can name a capability but not run it, which is how a
/// capability id ends up standing in for a script path and the router hands the executor
/// something it cannot execute.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entrypoint {
    /// An importable module path, for runtimes that take one (`python -m`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// A compiled binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// A script file, relative to the manifest that declares it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Fixed arguments always passed to the entrypoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl Entrypoint {
    /// The declared target, whichever form it took.
    ///
    /// Precedence is script, then binary, then module: the most specific declaration wins,
    /// so a manifest that names both is not silently resolved to the vaguer one.
    pub fn target(&self) -> Option<&str> {
        self.script
            .as_deref()
            .or(self.binary.as_deref())
            .or(self.module.as_deref())
    }

    /// Whether the target is a module rather than a filesystem path.
    pub fn is_module(&self) -> bool {
        self.script.is_none() && self.binary.is_none() && self.module.is_some()
    }
}

/// Retry allowance — §25.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retry {
    pub max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub kind: ExecutionKind,
    pub runtime: Runtime,
    /// Where the code lives. Optional because agent and human-gate capabilities have no
    /// file to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Entrypoint>,
}

impl Execution {
    /// An execution declaration with no entrypoint, for capabilities that are not files.
    pub fn new(kind: ExecutionKind, runtime: Runtime) -> Self {
        Self {
            kind,
            runtime,
            entrypoint: None,
        }
    }

    /// An execution declaration naming a script file.
    pub fn script(runtime: Runtime, script: impl Into<String>) -> Self {
        Self {
            kind: ExecutionKind::Script,
            runtime,
            entrypoint: Some(Entrypoint {
                script: Some(script.into()),
                ..Entrypoint::default()
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quality {
    pub deterministic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Idempotency {
    pub key_template: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Risk {
    pub side_effect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<Idempotency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub windows: bool,
    pub linux: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schemas {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Guard failure behaviour — Article 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// Fail-closed: deny the operation.
    Deny,
    /// Fail-open: allow the operation to proceed.
    Allow,
}

/// A capability's self-declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub id: String,
    pub version: u32,
    #[serde(rename = "type")]
    pub capability_type: CapabilityType,
    /// Functional classification (§27). Optional so existing manifests keep parsing.
    #[serde(rename = "kind", default, skip_serializing_if = "Option::is_none")]
    pub functional_kind: Option<FunctionalKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub execution: Execution,
    pub quality: Quality,
    pub risk: Risk,
    pub platform: Platform,
    #[serde(default)]
    pub schemas: Schemas,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<Retry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<OnError>,
}

impl CapabilityManifest {
    /// Parses a manifest from YAML.
    pub fn from_yaml(yaml: &str) -> Result<Self, ManifestError> {
        serde_yaml::from_str(yaml).map_err(|e| ManifestError::Parse {
            detail: e.to_string(),
        })
    }

    /// The declared idempotency template, if any.
    pub fn idempotency_template(&self) -> Option<IdempotencyTemplate> {
        self.risk
            .idempotency
            .as_ref()
            .map(|i| IdempotencyTemplate::new(&i.key_template))
    }

    /// Whether the capability runs on at least one platform.
    ///
    /// A manifest claiming neither is not "portable", it is unrunnable — almost certainly
    /// a copy-paste mistake worth catching.
    pub fn runs_anywhere(&self) -> bool {
        self.platform.windows || self.platform.linux
    }

    /// Whether this capability runs on the platform executing this code.
    ///
    /// Checked before dispatch: routing a Linux-only script to a Windows worker produces a
    /// confusing spawn failure instead of an honest "not supported here".
    pub fn runs_on_this_platform(&self) -> bool {
        if cfg!(windows) {
            self.platform.windows
        } else {
            self.platform.linux
        }
    }

    /// The declared entrypoint, if any.
    pub fn entrypoint(&self) -> Option<&Entrypoint> {
        self.execution.entrypoint.as_ref()
    }

    /// The retry allowance, defaulting to a single attempt.
    ///
    /// One attempt is the safe default: a capability that has not said it is retry-safe
    /// must not be retried by assumption.
    pub fn max_attempts(&self) -> u32 {
        self.retry.map_or(1, |r| r.max_attempts.max(1))
    }

    /// The declared timeout, or `fallback` when the manifest omits one.
    pub fn timeout_or(&self, fallback: u64) -> u64 {
        self.timeout_seconds.filter(|s| *s > 0).unwrap_or(fallback)
    }
}

/// Manifest failures.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("failed to parse capability manifest: {detail}")]
    Parse { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERIFIER: &str = r#"
id: verifier.config-consistency
version: 3
type: verifier
execution:
  kind: script
  runtime: python
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
schemas:
  input: config-check-input-v1
  output: verification-result-v1
timeout_seconds: 60
"#;

    #[test]
    fn parses_a_verifier_manifest() {
        let m = CapabilityManifest::from_yaml(VERIFIER).unwrap();
        assert_eq!(m.id, "verifier.config-consistency");
        assert_eq!(m.version, 3);
        assert_eq!(m.capability_type, CapabilityType::Verifier);
        assert_eq!(m.execution.kind, ExecutionKind::Script);
        assert_eq!(m.execution.runtime, Runtime::Python);
        assert!(m.quality.deterministic);
        assert!(!m.risk.side_effect);
        assert_eq!(m.timeout_seconds, Some(60));
    }

    #[test]
    fn parses_an_effect_manifest_with_idempotency() {
        let yaml = r#"
id: effect.ntfy-send
version: 1
type: tool
execution:
  kind: script
  runtime: python
quality:
  deterministic: false
risk:
  side_effect: true
  idempotency:
    key_template: "ntfy:{channel}:{date}"
platform:
  windows: true
  linux: true
"#;
        let m = CapabilityManifest::from_yaml(yaml).unwrap();
        assert!(m.risk.side_effect);
        let template = m.idempotency_template().unwrap();
        assert_eq!(template.placeholders(), vec!["channel", "date"]);
    }

    #[test]
    fn rejects_malformed_yaml() {
        assert!(CapabilityManifest::from_yaml("id: [unclosed").is_err());
    }

    #[test]
    fn rejects_a_manifest_missing_required_fields() {
        assert!(CapabilityManifest::from_yaml("id: x\nversion: 1\n").is_err());
    }

    #[test]
    fn only_agent_execution_involves_an_llm() {
        assert!(ExecutionKind::Agent.involves_llm());
        for k in [
            ExecutionKind::Script,
            ExecutionKind::Tool,
            ExecutionKind::Workflow,
            ExecutionKind::HumanGate,
        ] {
            assert!(!k.involves_llm(), "{k:?} should not involve an LLM");
        }
    }

    #[test]
    fn mechanical_runtimes_are_identified() {
        for r in [
            Runtime::Rust,
            Runtime::Python,
            Runtime::Powershell,
            Runtime::Shell,
        ] {
            assert!(r.is_mechanical(), "{r:?} should be mechanical");
        }
        for r in [
            Runtime::ClaudeCode,
            Runtime::Codex,
            Runtime::OpenaiCompatible,
        ] {
            assert!(!r.is_mechanical(), "{r:?} should not be mechanical");
        }
    }

    #[test]
    fn a_manifest_targeting_no_platform_is_detectable() {
        let yaml = VERIFIER
            .replace("windows: true", "windows: false")
            .replace("linux: true", "linux: false");
        let m = CapabilityManifest::from_yaml(&yaml).unwrap();
        assert!(!m.runs_anywhere());
    }

    #[test]
    fn manifest_round_trips_through_yaml() {
        let m = CapabilityManifest::from_yaml(VERIFIER).unwrap();
        let yaml = serde_yaml::to_string(&m).unwrap();
        assert_eq!(CapabilityManifest::from_yaml(&yaml).unwrap(), m);
    }
}
