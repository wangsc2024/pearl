//! Agent CLI runtimes — §37, Article 9.
//!
//! These were stubs that returned "not configured" unconditionally, which meant §37's
//! required runtimes existed in the enum and nowhere else. They now spawn the real tool
//! under the process supervisor, which is what makes them cancellable and therefore
//! admissible as runtimes at all (Article 9).
//!
//! Three properties worth naming:
//!
//! **The prompt comes from a file.** A capability's entrypoint is a prompt template, and the
//! task payload is rendered into it. Building prompts in Rust string literals would put
//! Prompt content in code, which Article 3 exists to prevent — and it would mean a prompt
//! change required a rebuild.
//!
//! **Absence of a key is not always an error.** `claude` and `codex` can hold interactive
//! credentials, so PEARL never needs to see one. The adapter only refuses when the tool
//! itself is missing.
//!
//! **Nothing runs unless the operator installed the tool.** The refusal is explicit and
//! names the program and the override variable, because "not configured" without saying what
//! to configure is a dead end.

use std::collections::BTreeMap;

use pearl_core::Clock;
use pearl_governance::manifest::Runtime;
use pearl_process_supervisor::ProcessSupervisor;

use crate::family::{family_of, AgentCli, RuntimeFamily};
use crate::{
    programs, RuntimeAdapter, RuntimeError, RuntimeResult, ScriptRuntimeAdapter, ScriptSpec,
};

/// Runs an agent command-line tool.
pub struct AgentCliAdapter<S: ProcessSupervisor> {
    cli: AgentCli,
    supervisor: S,
}

impl<S: ProcessSupervisor> AgentCliAdapter<S> {
    pub fn new(cli: AgentCli, supervisor: S) -> Self {
        Self { cli, supervisor }
    }

    /// Which tool this adapter drives.
    pub fn cli(&self) -> AgentCli {
        self.cli
    }

    /// The program to invoke, honouring the operator's override.
    pub fn program(&self) -> String {
        std::env::var(self.cli.program_override())
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.cli.program().to_string())
    }

    /// Whether the tool is present on this machine.
    pub fn is_available(&self) -> bool {
        programs::is_available(&self.program())
    }
}

impl<S: ProcessSupervisor> RuntimeAdapter for AgentCliAdapter<S> {
    fn execute(&self, spec: &ScriptSpec, clock: &dyn Clock) -> Result<RuntimeResult, RuntimeError> {
        self.validate(spec)?;

        let prompt = crate::prompt::render(spec)?;
        let program = self.program();
        let mut args = self.cli.headless_args(&prompt);
        args.extend(spec.args.iter().cloned());

        // The key is forwarded only if the environment has one: an empty value would look
        // like a credential to the tool and fail more confusingly than its absence.
        let mut env = spec.env.clone();
        if let Ok(key) = std::env::var(self.cli.key_var()) {
            if !key.trim().is_empty() {
                env.insert(self.cli.key_var().to_string(), key);
            }
        }

        let native = ScriptSpec {
            runtime: Runtime::Native,
            entrypoint: std::path::PathBuf::from(&program),
            args,
            env,
            cwd: spec.cwd.clone(),
            timeout: spec.timeout,
            // The prompt already carries the payload; PEARL_INPUT is set as well so a wrapper
            // script used as `PEARL_CLAUDE_CMD` can read it structurally.
            input_payload: spec.input_payload.clone(),
        };

        // Reuse the script adapter: the supervision, timeout and output contract are
        // identical, and an agent process is not special enough to deserve its own copy.
        ScriptRuntimeAdapter::new(&self.supervisor).execute(&native, clock)
    }

    fn validate(&self, spec: &ScriptSpec) -> Result<(), RuntimeError> {
        if !self.supports_runtime(spec.runtime) {
            return Err(RuntimeError::UnsupportedRuntime {
                runtime: spec.runtime.as_str().to_string(),
            });
        }
        if spec.timeout <= chrono::TimeDelta::zero() {
            return Err(RuntimeError::Validation {
                detail: "timeout must be positive".to_string(),
            });
        }
        if !self.is_available() {
            return Err(RuntimeError::UnsupportedRuntime {
                runtime: format!(
                    "{}: '{}' is not on PATH; install it or set {}",
                    spec.runtime.as_str(),
                    self.program(),
                    self.cli.program_override()
                ),
            });
        }
        crate::prompt::validate(spec)
    }

    fn supports_runtime(&self, runtime: Runtime) -> bool {
        matches!(family_of(runtime), RuntimeFamily::AgentCli(cli) if cli == self.cli)
    }
}

/// An adapter for a non-mechanical runtime, when one can be built without a supervisor.
///
/// Only API runtimes qualify: an agent CLI is a process and therefore needs supervision, so
/// callers construct [`AgentCliAdapter`] with a supervisor of their own. Kept as the seam the
/// worker asks first, so adding a provider does not change the worker.
pub fn agent_adapter_for(runtime: Runtime) -> Option<Box<dyn RuntimeAdapter>> {
    match family_of(runtime) {
        RuntimeFamily::Api(provider) => Some(Box::new(
            crate::api_adapters::ApiRuntimeAdapter::new(provider),
        )),
        RuntimeFamily::AgentCli(_) | RuntimeFamily::Mechanical => None,
    }
}

/// Environment variables an adapter may forward, for diagnostics.
pub fn credential_vars() -> BTreeMap<&'static str, bool> {
    let mut present = BTreeMap::new();
    for cli in [AgentCli::ClaudeCode, AgentCli::Codex, AgentCli::Cursor] {
        present.insert(
            cli.key_var(),
            std::env::var(cli.key_var()).is_ok_and(|v| !v.trim().is_empty()),
        );
    }
    present
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use pearl_process_supervisor::PlatformSupervisor;
    use std::path::PathBuf;

    fn spec_with(runtime: Runtime, prompt: &std::path::Path) -> ScriptSpec {
        ScriptSpec {
            runtime,
            entrypoint: prompt.to_path_buf(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            timeout: TimeDelta::try_seconds(30).unwrap(),
            input_payload: None,
        }
    }

    fn adapter(cli: AgentCli) -> AgentCliAdapter<PlatformSupervisor> {
        AgentCliAdapter::new(cli, PlatformSupervisor::default())
    }

    #[test]
    fn each_adapter_supports_only_its_own_runtime() {
        let claude = adapter(AgentCli::ClaudeCode);
        assert!(claude.supports_runtime(Runtime::ClaudeCode));
        assert!(!claude.supports_runtime(Runtime::Codex));
        assert!(!claude.supports_runtime(Runtime::Python));
        assert!(!claude.supports_runtime(Runtime::Groq));

        let codex = adapter(AgentCli::Codex);
        assert!(codex.supports_runtime(Runtime::Codex));
        assert!(!codex.supports_runtime(Runtime::Cursor));
    }

    #[test]
    fn the_program_is_overridable() {
        let var = AgentCli::Cursor.program_override();
        let restore = std::env::var(var).ok();
        std::env::set_var(var, "my-cursor-wrapper");
        assert_eq!(adapter(AgentCli::Cursor).program(), "my-cursor-wrapper");

        // A blank override is an unset variable that went through a shell.
        std::env::set_var(var, "  ");
        assert_eq!(adapter(AgentCli::Cursor).program(), "cursor-agent");

        match restore {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn a_missing_tool_is_refused_with_something_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = dir.path().join("p.md");
        std::fs::write(&prompt, "say hello").unwrap();

        let var = AgentCli::ClaudeCode.program_override();
        let restore = std::env::var(var).ok();
        std::env::set_var(var, "pearl-no-such-agent-xyz");

        let err = adapter(AgentCli::ClaudeCode)
            .validate(&spec_with(Runtime::ClaudeCode, &prompt))
            .unwrap_err();
        let message = err.to_string();
        // Both the program and the way to change it, because "not configured" alone is a
        // dead end for whoever has to fix it.
        assert!(
            message.contains("pearl-no-such-agent-xyz"),
            "got: {message}"
        );
        assert!(message.contains(var), "got: {message}");

        match restore {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn a_missing_prompt_file_is_a_validation_error() {
        let err = adapter(AgentCli::Codex)
            .validate(&spec_with(
                Runtime::Codex,
                &PathBuf::from("/no/such/prompt.md"),
            ))
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::Validation { .. } | RuntimeError::UnsupportedRuntime { .. }
        ));
    }

    #[test]
    fn a_wrapper_program_receives_the_rendered_prompt() {
        // Uses python as a stand-in for an agent CLI, which is exactly what the override
        // exists for: it lets an operator put anything runnable behind the runtime.
        if !programs::is_available(&programs::python()) {
            eprintln!("skipping: no Python interpreter");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("wrapper.py");
        std::fs::write(
            &wrapper,
            "import json, sys\nprint(json.dumps({\"argv\": sys.argv[1:]}))\n",
        )
        .unwrap();
        let prompt = dir.path().join("p.md");
        std::fs::write(&prompt, "score {{task_id}}").unwrap();

        // A launcher script so the wrapper is invoked as a program.
        let launcher = dir
            .path()
            .join(if cfg!(windows) { "run.cmd" } else { "run.sh" });
        let python = programs::python();
        if cfg!(windows) {
            std::fs::write(
                &launcher,
                format!("@echo off\r\n{python} \"{}\" %*\r\n", wrapper.display()),
            )
            .unwrap();
        } else {
            std::fs::write(
                &launcher,
                format!("#!/bin/sh\n{python} \"{}\" \"$@\"\n", wrapper.display()),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }

        let var = AgentCli::Cursor.program_override();
        let restore = std::env::var(var).ok();
        std::env::set_var(var, launcher.to_string_lossy().to_string());

        let mut spec = spec_with(Runtime::Cursor, &prompt);
        spec.input_payload = Some(serde_json::json!({ "task_id": "t-42" }));
        let result = adapter(AgentCli::Cursor).execute(&spec, &pearl_core::SystemClock);

        match restore {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }

        let result = result.expect("wrapper should run");
        let output = result.structured_output.expect("wrapper emits JSON");
        let argv = output["argv"].as_array().unwrap();
        // The placeholder was rendered from the payload before the tool ever saw it.
        assert!(
            argv.iter().any(|a| a.as_str() == Some("score t-42")),
            "got {argv:?}"
        );
    }

    #[test]
    fn credential_presence_is_reportable_without_revealing_anything() {
        let vars = credential_vars();
        assert!(vars.contains_key("ANTHROPIC_API_KEY"));
        assert!(vars.contains_key("OPENAI_API_KEY"));
        assert!(vars.contains_key("CURSOR_API_KEY"));
        // Booleans only: `pearl doctor` needs to say whether a key is configured without
        // printing it.
        for value in vars.values() {
            let _: &bool = value;
        }
    }

    #[test]
    fn api_runtimes_resolve_to_an_adapter_and_cli_runtimes_do_not() {
        // A CLI agent is a process and needs a supervisor, so it cannot be produced from a
        // runtime name alone.
        for runtime in [
            Runtime::Groq,
            Runtime::Mistral,
            Runtime::Nvidia,
            Runtime::OpenaiCompatible,
            Runtime::LlamaCpp,
        ] {
            assert!(agent_adapter_for(runtime).is_some(), "{runtime:?}");
        }
        for runtime in [Runtime::ClaudeCode, Runtime::Codex, Runtime::Cursor] {
            assert!(agent_adapter_for(runtime).is_none(), "{runtime:?}");
        }
        assert!(agent_adapter_for(Runtime::Python).is_none());
    }
}
