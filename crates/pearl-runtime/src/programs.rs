//! Interpreter resolution — §24 (the four mechanical runtimes must work on both platforms).
//!
//! Hard-coding `python3` and `bash` is a Linux assumption wearing a portability label:
//! on a stock Windows machine `python3` is a Store stub and `bash` may not exist at all,
//! so a manifest declaring `platform: {windows: true}` would fail at spawn time with a
//! confusing "program not found".
//!
//! Resolution has three tiers, in order:
//!
//! 1. **Operator override** — `PEARL_PYTHON`, `PEARL_PWSH`, `PEARL_BASH`. Taken verbatim
//!    and never probed: this is the §13 Runtime Emergency Override layer, and an operator
//!    who names an interpreter explicitly is entitled to be believed.
//! 2. **Probe** — the platform's candidate list, first one that answers `--version`.
//!    Cached for the life of the process, so the cost is one spawn per interpreter.
//! 3. **Fallback** — the first candidate, so the resulting error names something an
//!    operator can act on rather than an empty string.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Environment variable that overrides the Python interpreter.
pub const PYTHON_OVERRIDE: &str = "PEARL_PYTHON";
/// Environment variable that overrides the PowerShell interpreter.
pub const PWSH_OVERRIDE: &str = "PEARL_PWSH";
/// Environment variable that overrides the POSIX shell.
pub const BASH_OVERRIDE: &str = "PEARL_BASH";

/// Python candidates, most-likely-correct first.
///
/// Windows leads with `python` because `python3.exe` on a stock install is the Microsoft
/// Store stub, which exits non-zero and opens a store page rather than running a script.
#[cfg(windows)]
const PYTHON_CANDIDATES: &[&str] = &["python", "py", "python3"];
#[cfg(not(windows))]
const PYTHON_CANDIDATES: &[&str] = &["python3", "python"];

/// PowerShell candidates. `pwsh` is cross-platform 7+; `powershell` is Windows 5.1.
#[cfg(windows)]
const PWSH_CANDIDATES: &[&str] = &["pwsh", "powershell"];
#[cfg(not(windows))]
const PWSH_CANDIDATES: &[&str] = &["pwsh"];

/// POSIX shell candidates. On Windows this is Git Bash or WSL, both of which are optional,
/// which is why a `shell` capability should declare `platform: {windows: false}` unless it
/// has been tested there.
#[cfg(windows)]
const BASH_CANDIDATES: &[&str] = &["bash", "sh"];
#[cfg(not(windows))]
const BASH_CANDIDATES: &[&str] = &["bash", "sh"];

/// The resolved Python interpreter.
pub fn python() -> String {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    resolve(&RESOLVED, PYTHON_OVERRIDE, PYTHON_CANDIDATES)
}

/// The resolved PowerShell interpreter.
pub fn powershell() -> String {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    resolve(&RESOLVED, PWSH_OVERRIDE, PWSH_CANDIDATES)
}

/// The resolved POSIX shell.
pub fn bash() -> String {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    resolve(&RESOLVED, BASH_OVERRIDE, BASH_CANDIDATES)
}

/// Whether a program can be executed on this machine.
///
/// Used by tests to skip interpreter-dependent cases honestly instead of failing on a
/// machine that simply does not have the interpreter.
pub fn is_available(program: &str) -> bool {
    responds_to_version(program)
}

fn resolve(cache: &'static OnceLock<String>, override_var: &str, candidates: &[&str]) -> String {
    // The override is read every call rather than cached: a cached override would make
    // the value depend on which code path ran first, which is exactly the kind of
    // order-dependent configuration Article 10 exists to prevent.
    if let Some(explicit) = non_empty_env(override_var) {
        return explicit;
    }
    cache
        .get_or_init(|| {
            candidates
                .iter()
                .find(|c| responds_to_version(c))
                .map(|c| c.to_string())
                .unwrap_or_else(|| candidates[0].to_string())
        })
        .clone()
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Whether `program --version` succeeds.
///
/// `--version` is the cheapest universal liveness probe: every candidate here supports it,
/// it has no side effects, and it exits immediately.
fn responds_to_version(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_is_taken_verbatim() {
        // A unique name per test avoids interference from parallel test threads sharing
        // the process environment.
        let var = "PEARL_TEST_OVERRIDE_VERBATIM";
        std::env::set_var(var, "/opt/custom/python3.13");
        static CACHE: OnceLock<String> = OnceLock::new();
        assert_eq!(
            resolve(&CACHE, var, &["python3"]),
            "/opt/custom/python3.13",
            "an explicit operator override must not be second-guessed by probing"
        );
        std::env::remove_var(var);
    }

    #[test]
    fn a_blank_override_is_ignored() {
        let var = "PEARL_TEST_OVERRIDE_BLANK";
        std::env::set_var(var, "   ");
        static CACHE: OnceLock<String> = OnceLock::new();
        // A blank value is an unset variable that went through a shell, not a request to
        // run the empty program.
        assert_eq!(
            resolve(&CACHE, var, &["definitely-not-a-real-program"]),
            "definitely-not-a-real-program"
        );
        std::env::remove_var(var);
    }

    #[test]
    fn unresolvable_candidates_fall_back_to_the_first() {
        static CACHE: OnceLock<String> = OnceLock::new();
        assert_eq!(
            resolve(
                &CACHE,
                "PEARL_TEST_UNSET_VAR_XYZ",
                &["no-such-program-a", "no-such-program-b"]
            ),
            "no-such-program-a",
            "the error message must name a candidate an operator can act on"
        );
    }

    #[test]
    fn probing_finds_a_later_candidate() {
        static CACHE: OnceLock<String> = OnceLock::new();
        // cargo is guaranteed present: these tests are running under it.
        assert_eq!(
            resolve(
                &CACHE,
                "PEARL_TEST_UNSET_VAR_ABC",
                &["no-such-program-a", "cargo"]
            ),
            "cargo"
        );
    }

    #[test]
    fn resolved_interpreters_are_non_empty() {
        assert!(!python().is_empty());
        assert!(!powershell().is_empty());
        assert!(!bash().is_empty());
    }

    #[test]
    fn platform_defaults_are_plausible() {
        // Not asserting the exact program: that depends on what is installed. Asserting
        // the candidate list is ordered for the platform, which is the decision under test.
        #[cfg(windows)]
        assert_eq!(PYTHON_CANDIDATES[0], "python");
        #[cfg(not(windows))]
        assert_eq!(PYTHON_CANDIDATES[0], "python3");
    }

    #[test]
    fn availability_probe_distinguishes_present_from_absent() {
        assert!(is_available("cargo"));
        assert!(!is_available("pearl-definitely-not-installed-xyz"));
    }
}
