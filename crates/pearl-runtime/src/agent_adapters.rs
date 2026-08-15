//! # Agent CLI Runtime Adapter Stubs
//!
//! Stub adapters for agent runtimes: claude_code, codex, cursor, llama_cpp.
//! These implement the RuntimeAdapter trait but return errors indicating "not configured"
//! until the actual binary/API is available in the environment.
//!
//! 系統開發需求書 §37: the system acknowledges these runtimes exist but refuses
//! to route to them until properly configured.

use crate::{RuntimeAdapter, RuntimeError, RuntimeResult, ScriptSpec};
use pearl_core::Clock;
use pearl_governance::manifest::Runtime;

/// The adapter for a non-mechanical runtime, if one exists.
///
/// The seam that keeps agent execution out of the worker's control flow: a caller asks for
/// the runtime a capability declared and either gets something that can run it or an honest
/// `None`. Mechanical runtimes are deliberately absent — those go through
/// [`crate::ScriptRuntimeAdapter`], which needs a process supervisor and therefore cannot
/// be produced from a runtime name alone.
pub fn agent_adapter_for(runtime: Runtime) -> Option<Box<dyn RuntimeAdapter>> {
    match runtime {
        Runtime::ClaudeCode => Some(Box::new(ClaudeCodeAdapter::new())),
        Runtime::Codex => Some(Box::new(CodexAdapter::new())),
        Runtime::Cursor => Some(Box::new(CursorAdapter::new())),
        Runtime::LlamaCpp => Some(Box::new(LlamaCppAdapter::new())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ClaudeCodeAdapter
// ---------------------------------------------------------------------------

/// Stub adapter for the Claude Code CLI agent runtime.
///
/// Returns `UnsupportedRuntime` until `claude` binary is configured in the environment.
pub struct ClaudeCodeAdapter;

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeAdapter for ClaudeCodeAdapter {
    fn execute(
        &self,
        _spec: &ScriptSpec,
        _clock: &dyn Clock,
    ) -> Result<RuntimeResult, RuntimeError> {
        Err(RuntimeError::UnsupportedRuntime {
            runtime: "claude_code: agent runtime not configured -- install `claude` CLI and set CLAUDE_API_KEY".to_string(),
        })
    }

    fn validate(&self, _spec: &ScriptSpec) -> Result<(), RuntimeError> {
        Err(RuntimeError::Validation {
            detail: "claude_code adapter is a stub; configure the claude CLI binary to enable"
                .to_string(),
        })
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        runtime == Runtime::ClaudeCode
    }
}

// ---------------------------------------------------------------------------
// CodexAdapter
// ---------------------------------------------------------------------------

/// Stub adapter for the OpenAI Codex CLI agent runtime.
///
/// Returns `UnsupportedRuntime` until `codex` binary is configured.
pub struct CodexAdapter;

impl CodexAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeAdapter for CodexAdapter {
    fn execute(
        &self,
        _spec: &ScriptSpec,
        _clock: &dyn Clock,
    ) -> Result<RuntimeResult, RuntimeError> {
        Err(RuntimeError::UnsupportedRuntime {
            runtime:
                "codex: agent runtime not configured -- install `codex` CLI and set OPENAI_API_KEY"
                    .to_string(),
        })
    }

    fn validate(&self, _spec: &ScriptSpec) -> Result<(), RuntimeError> {
        Err(RuntimeError::Validation {
            detail: "codex adapter is a stub; configure the codex CLI binary to enable".to_string(),
        })
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        runtime == Runtime::Codex
    }
}

// ---------------------------------------------------------------------------
// CursorAdapter
// ---------------------------------------------------------------------------

/// Stub adapter for the Cursor IDE agent runtime.
///
/// Returns `UnsupportedRuntime` until cursor is configured.
pub struct CursorAdapter;

impl CursorAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CursorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeAdapter for CursorAdapter {
    fn execute(
        &self,
        _spec: &ScriptSpec,
        _clock: &dyn Clock,
    ) -> Result<RuntimeResult, RuntimeError> {
        Err(RuntimeError::UnsupportedRuntime {
            runtime:
                "cursor: agent runtime not configured -- install Cursor and configure API access"
                    .to_string(),
        })
    }

    fn validate(&self, _spec: &ScriptSpec) -> Result<(), RuntimeError> {
        Err(RuntimeError::Validation {
            detail: "cursor adapter is a stub; configure Cursor IDE integration to enable"
                .to_string(),
        })
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        runtime == Runtime::Cursor
    }
}

// ---------------------------------------------------------------------------
// LlamaCppAdapter
// ---------------------------------------------------------------------------

/// Stub adapter for the llama.cpp local inference runtime.
///
/// Returns `UnsupportedRuntime` until a local model and llama-server are configured.
pub struct LlamaCppAdapter;

impl LlamaCppAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LlamaCppAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeAdapter for LlamaCppAdapter {
    fn execute(
        &self,
        _spec: &ScriptSpec,
        _clock: &dyn Clock,
    ) -> Result<RuntimeResult, RuntimeError> {
        Err(RuntimeError::UnsupportedRuntime {
            runtime: "llama_cpp: agent runtime not configured -- install llama-server and provide a model path".to_string(),
        })
    }

    fn validate(&self, _spec: &ScriptSpec) -> Result<(), RuntimeError> {
        Err(RuntimeError::Validation {
            detail: "llama_cpp adapter is a stub; configure llama-server and model to enable"
                .to_string(),
        })
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        runtime == Runtime::LlamaCpp
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_governance::manifest::Runtime;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn dummy_spec(runtime: Runtime) -> ScriptSpec {
        ScriptSpec {
            runtime,
            entrypoint: PathBuf::from("/tmp/agent"),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: chrono::TimeDelta::try_seconds(30).unwrap(),
            input_payload: None,
        }
    }

    #[test]
    fn claude_code_adapter_returns_not_configured() {
        let adapter = ClaudeCodeAdapter::new();
        assert!(adapter.supports_runtime(Runtime::ClaudeCode));
        assert!(!adapter.supports_runtime(Runtime::Python));

        let spec = dummy_spec(Runtime::ClaudeCode);
        let result = adapter.execute(&spec, &pearl_core::SystemClock);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RuntimeError::UnsupportedRuntime { .. }
        ));
    }

    #[test]
    fn codex_adapter_returns_not_configured() {
        let adapter = CodexAdapter::new();
        assert!(adapter.supports_runtime(Runtime::Codex));
        assert!(!adapter.supports_runtime(Runtime::Shell));

        let result = adapter.validate(&dummy_spec(Runtime::Codex));
        assert!(result.is_err());
    }

    #[test]
    fn cursor_adapter_returns_not_configured() {
        let adapter = CursorAdapter::new();
        assert!(adapter.supports_runtime(Runtime::Cursor));

        let result = adapter.execute(&dummy_spec(Runtime::Cursor), &pearl_core::SystemClock);
        assert!(result.is_err());
    }

    #[test]
    fn llama_cpp_adapter_returns_not_configured() {
        let adapter = LlamaCppAdapter::new();
        assert!(adapter.supports_runtime(Runtime::LlamaCpp));
        assert!(!adapter.supports_runtime(Runtime::Native));

        let result = adapter.validate(&dummy_spec(Runtime::LlamaCpp));
        assert!(result.is_err());
    }
}
