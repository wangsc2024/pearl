//! Which mechanism runs which runtime — §37.
//!
//! Three families, because they need three different things from the caller:
//!
//! | Family      | Needs                | Cancellation                        |
//! |-------------|----------------------|-------------------------------------|
//! | `Mechanical`| a process supervisor | kill the process tree               |
//! | `AgentCli`  | a process supervisor | kill the process tree               |
//! | `Api`       | an HTTP client       | request timeout, then drop the call  |
//!
//! Making the family explicit keeps the distinction out of `if` chains scattered across
//! callers, and it is the distinction Article 1 cares most about: mechanical work must never
//! be routed to something that reasons.

use pearl_governance::manifest::Runtime;

/// An agent invoked as a command-line tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCli {
    ClaudeCode,
    Codex,
    Cursor,
}

impl AgentCli {
    /// The default program name.
    pub fn program(&self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "claude",
            AgentCli::Codex => "codex",
            // Cursor's headless agent is a separate binary from the editor.
            AgentCli::Cursor => "cursor-agent",
        }
    }

    /// The environment variable that overrides the program.
    pub fn program_override(&self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "PEARL_CLAUDE_CMD",
            AgentCli::Codex => "PEARL_CODEX_CMD",
            AgentCli::Cursor => "PEARL_CURSOR_CMD",
        }
    }

    /// The environment variable carrying credentials, when the CLI needs one.
    ///
    /// Absence is not always an error: `claude` and `codex` can be logged in interactively,
    /// in which case they hold their own credentials and PEARL never sees a key.
    pub fn key_var(&self) -> &'static str {
        match self {
            AgentCli::ClaudeCode => "ANTHROPIC_API_KEY",
            AgentCli::Codex => "OPENAI_API_KEY",
            AgentCli::Cursor => "CURSOR_API_KEY",
        }
    }

    /// Arguments that make the CLI run once, headless, and print a result.
    ///
    /// Each of these tools defaults to an interactive session, which would hang a worker
    /// forever — the flags are what make them usable as a runtime at all.
    pub fn headless_args(&self, prompt: &str) -> Vec<String> {
        match self {
            AgentCli::ClaudeCode => vec![
                "-p".to_string(),
                prompt.to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ],
            AgentCli::Codex => vec!["exec".to_string(), "--json".to_string(), prompt.to_string()],
            AgentCli::Cursor => vec![
                "-p".to_string(),
                prompt.to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ],
        }
    }
}

/// An OpenAI-compatible chat-completions endpoint.
///
/// One adapter serves all of these because they speak the same protocol; only the base URL,
/// the key and the default model differ. Pretending otherwise would mean four copies of the
/// same request builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiProvider {
    /// Any endpoint configured entirely by environment.
    OpenAiCompatible,
    Groq,
    Mistral,
    Nvidia,
    /// A local `llama-server`, which also speaks the OpenAI protocol.
    LlamaCpp,
}

impl ApiProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApiProvider::OpenAiCompatible => "openai_compatible",
            ApiProvider::Groq => "groq",
            ApiProvider::Mistral => "mistral",
            ApiProvider::Nvidia => "nvidia",
            ApiProvider::LlamaCpp => "llama_cpp",
        }
    }

    /// Environment variable holding the API key.
    pub fn key_var(&self) -> &'static str {
        match self {
            ApiProvider::OpenAiCompatible => "OPENAI_API_KEY",
            ApiProvider::Groq => "GROQ_API_KEY",
            ApiProvider::Mistral => "MISTRAL_API_KEY",
            // Spelled as the operator's environment spells it.
            ApiProvider::Nvidia => "NVIDIA_API_KEY",
            ApiProvider::LlamaCpp => "LLAMA_API_KEY",
        }
    }

    /// Environment variable overriding the base URL.
    pub fn base_var(&self) -> &'static str {
        match self {
            ApiProvider::OpenAiCompatible => "OPENAI_API_BASE",
            ApiProvider::Groq => "GROQ_API_BASE",
            ApiProvider::Mistral => "MISTRAL_API_BASE",
            ApiProvider::Nvidia => "NVIDIA_API_BASE",
            ApiProvider::LlamaCpp => "LLAMA_API_BASE",
        }
    }

    /// Environment variable overriding the model.
    pub fn model_var(&self) -> &'static str {
        match self {
            ApiProvider::OpenAiCompatible => "OPENAI_MODEL",
            ApiProvider::Groq => "GROQ_MODEL",
            ApiProvider::Mistral => "MISTRAL_MODEL",
            ApiProvider::Nvidia => "NVIDIA_MODEL",
            ApiProvider::LlamaCpp => "LLAMA_MODEL",
        }
    }

    /// The published base URL, used when the environment does not override it.
    ///
    /// `None` means there is no sensible default and the operator must say: a local
    /// llama-server has no canonical address, and a generic OpenAI-compatible endpoint is
    /// generic precisely because its address is unknown.
    pub fn default_base(&self) -> Option<&'static str> {
        match self {
            ApiProvider::Groq => Some("https://api.groq.com/openai/v1"),
            ApiProvider::Mistral => Some("https://api.mistral.ai/v1"),
            ApiProvider::Nvidia => Some("https://integrate.api.nvidia.com/v1"),
            ApiProvider::OpenAiCompatible | ApiProvider::LlamaCpp => None,
        }
    }

    /// Whether a key is required.
    ///
    /// A local model needs none, and demanding one would make the offline path impossible.
    pub fn requires_key(&self) -> bool {
        !matches!(self, ApiProvider::LlamaCpp)
    }
}

/// What kind of thing executes a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeFamily {
    /// A script or binary, run under a process supervisor (Article 1).
    Mechanical,
    /// An agent command-line tool, run under a process supervisor.
    AgentCli(AgentCli),
    /// An HTTP chat-completions endpoint.
    Api(ApiProvider),
}

/// Classifies a runtime.
pub fn family_of(runtime: Runtime) -> RuntimeFamily {
    match runtime {
        Runtime::Rust
        | Runtime::Python
        | Runtime::Powershell
        | Runtime::Shell
        | Runtime::Native => RuntimeFamily::Mechanical,
        Runtime::ClaudeCode => RuntimeFamily::AgentCli(AgentCli::ClaudeCode),
        Runtime::Codex => RuntimeFamily::AgentCli(AgentCli::Codex),
        Runtime::Cursor => RuntimeFamily::AgentCli(AgentCli::Cursor),
        Runtime::OpenaiCompatible => RuntimeFamily::Api(ApiProvider::OpenAiCompatible),
        Runtime::Groq => RuntimeFamily::Api(ApiProvider::Groq),
        Runtime::Mistral => RuntimeFamily::Api(ApiProvider::Mistral),
        Runtime::Nvidia => RuntimeFamily::Api(ApiProvider::Nvidia),
        Runtime::LlamaCpp => RuntimeFamily::Api(ApiProvider::LlamaCpp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runtime_has_exactly_one_family() {
        // A runtime with no family would be unroutable; one with two would be ambiguous.
        for runtime in [
            Runtime::Rust,
            Runtime::Python,
            Runtime::Powershell,
            Runtime::Shell,
            Runtime::Native,
            Runtime::ClaudeCode,
            Runtime::Codex,
            Runtime::Cursor,
            Runtime::OpenaiCompatible,
            Runtime::Groq,
            Runtime::Mistral,
            Runtime::Nvidia,
            Runtime::LlamaCpp,
        ] {
            let family = family_of(runtime);
            assert_eq!(
                matches!(family, RuntimeFamily::Mechanical),
                runtime.is_mechanical(),
                "{runtime:?} disagrees with is_mechanical()"
            );
        }
    }

    #[test]
    fn mechanical_runtimes_are_never_agents() {
        // Article 1 in the type system: nothing mechanical may resolve to a reasoning family.
        for runtime in [
            Runtime::Rust,
            Runtime::Python,
            Runtime::Powershell,
            Runtime::Shell,
        ] {
            assert!(matches!(family_of(runtime), RuntimeFamily::Mechanical));
        }
    }

    #[test]
    fn cloud_providers_have_a_default_base_and_local_ones_do_not() {
        for provider in [ApiProvider::Groq, ApiProvider::Mistral, ApiProvider::Nvidia] {
            assert!(provider.default_base().is_some(), "{provider:?}");
            assert!(provider.requires_key(), "{provider:?}");
        }
        assert!(ApiProvider::LlamaCpp.default_base().is_none());
        assert!(
            !ApiProvider::LlamaCpp.requires_key(),
            "a local model must be usable offline and unauthenticated"
        );
        assert!(ApiProvider::OpenAiCompatible.default_base().is_none());
    }

    #[test]
    fn provider_environment_variables_are_distinct() {
        let providers = [
            ApiProvider::OpenAiCompatible,
            ApiProvider::Groq,
            ApiProvider::Mistral,
            ApiProvider::Nvidia,
            ApiProvider::LlamaCpp,
        ];
        let mut keys: Vec<&str> = providers.iter().map(|p| p.key_var()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(
            keys.len(),
            providers.len(),
            "two providers share a key variable"
        );
    }

    #[test]
    fn agent_clis_run_headless() {
        // The default for all three is an interactive session, which would hang a worker.
        for cli in [AgentCli::ClaudeCode, AgentCli::Codex, AgentCli::Cursor] {
            let args = cli.headless_args("say hello");
            assert!(
                args.iter().any(|a| a == "say hello"),
                "{cli:?} does not pass the prompt"
            );
            assert!(
                args.iter().any(|a| a.contains("json")),
                "{cli:?} does not ask for machine-readable output"
            );
        }
    }

    #[test]
    fn agent_clis_have_distinct_overrides() {
        let clis = [AgentCli::ClaudeCode, AgentCli::Codex, AgentCli::Cursor];
        let mut overrides: Vec<&str> = clis.iter().map(|c| c.program_override()).collect();
        overrides.sort_unstable();
        overrides.dedup();
        assert_eq!(overrides.len(), clis.len());
    }
}
