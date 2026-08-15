//! # pearl-process-supervisor
//!
//! Process supervision — 系統開發需求書 §36, Constitution Article 9.
//!
//! Article 9 requires every runtime to provide `spawn`, `status`, `cancel`, `timeout` and
//! `cleanup`, and requires cancellation to reclaim the *entire* execution scope:
//!
//! ```text
//! worker
//!  └─ child
//!      └─ grandchild
//! ```
//!
//! Killing only the direct child is the classic leak: a shell that spawned a Python
//! process that spawned a subprocess leaves two survivors holding file locks and API
//! quota. The fix is to make the child a process-group leader at spawn time and signal
//! the whole group.
//!
//! A backend that cannot do this must not be registered as a runtime.

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::Clock;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Environment for the child.
    ///
    /// A `BTreeMap` rather than inheriting the parent's environment wholesale: Article 60
    /// (Security) requires environment filtering, and an explicit map makes the filtered
    /// set visible and testable.
    pub env: BTreeMap<String, String>,
    /// Whether to clear the inherited environment before applying `env`.
    pub clear_env: bool,
    pub timeout: Option<TimeDelta>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            clear_env: false,
            timeout: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Starts from an empty environment, passing only what is explicitly set.
    pub fn with_clean_env(mut self) -> Self {
        self.clear_env = true;
        self
    }

    pub fn timeout(mut self, timeout: TimeDelta) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// A running process under supervision.
#[derive(Debug)]
pub struct SupervisedProcess {
    pub pid: u32,
    started_at: DateTime<Utc>,
    deadline: Option<DateTime<Utc>>,
    child: Option<Child>,
}

impl SupervisedProcess {
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    pub fn deadline(&self) -> Option<DateTime<Utc>> {
        self.deadline
    }

    /// Whether the deadline has passed as of `now`.
    pub fn is_overdue(&self, now: DateTime<Utc>) -> bool {
        self.deadline.is_some_and(|d| now > d)
    }
}

/// How a supervised process finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Exited {
        code: i32,
    },
    /// Terminated by signal (Unix).
    Signalled {
        signal: i32,
    },
    /// Killed by the supervisor because it outlived its deadline.
    TimedOut,
    /// Killed by the supervisor on request.
    Cancelled,
}

impl ExitStatus {
    /// Whether this counts as success.
    ///
    /// Only a zero exit code. A timeout is not success even if the process happened to
    /// finish its work first — Article 4 means we could not observe that it did.
    pub fn is_success(&self) -> bool {
        matches!(self, ExitStatus::Exited { code: 0 })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ExitStatus::Exited { .. } => "exited",
            ExitStatus::Signalled { .. } => "signalled",
            ExitStatus::TimedOut => "timed_out",
            ExitStatus::Cancelled => "cancelled",
        }
    }
}

/// The Article 9 runtime contract.
///
/// Every execution runtime must satisfy this. The trait exists so that the contract can
/// be tested once and applied to every backend.
pub trait ProcessSupervisor {
    /// Starts a process in its own execution scope.
    fn spawn(&self, spec: &CommandSpec) -> Result<SupervisedProcess, SupervisorError>;

    /// Non-blocking status check.
    fn status(&self, proc: &mut SupervisedProcess) -> Result<Option<ExitStatus>, SupervisorError>;

    /// Asks the process tree to stop (SIGTERM to the group).
    fn cancel(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError>;

    /// Forcibly removes the whole process tree (SIGKILL to the group).
    fn kill_tree(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError>;

    /// Whether any member of the tree is still alive.
    fn is_alive(&self, proc: &SupervisedProcess) -> bool;

    /// Blocks until exit, enforcing the deadline.
    fn wait(
        &self,
        proc: &mut SupervisedProcess,
        clock: &dyn Clock,
    ) -> Result<ExitStatus, SupervisorError>;

    /// Releases supervisor resources. Must be safe to call twice.
    fn cleanup(&self, proc: &mut SupervisedProcess) -> Result<(), SupervisorError>;
}

/// How long a cancelled process may take to exit before it is killed.
///
/// Graceful first, forceful second: a script that writes an artifact should get a chance
/// to finish the write, but not an unbounded one.
pub const GRACEFUL_STOP_GRACE: i64 = 5;

/// Polling interval while waiting.
const POLL_INTERVAL_MS: u64 = 20;

#[cfg(unix)]
mod unix {
    use super::*;
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    use std::os::unix::process::CommandExt;

    /// Supervises processes using Unix process groups.
    ///
    /// Each child becomes a group leader via `setsid`, so its descendants inherit the
    /// group and one `killpg` reaches all of them.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct UnixProcessSupervisor;

    impl UnixProcessSupervisor {
        pub fn new() -> Self {
            Self
        }

        fn signal_group(&self, pid: u32, signal: Signal) -> Result<(), SupervisorError> {
            match killpg(Pid::from_raw(pid as i32), signal) {
                Ok(()) => Ok(()),
                // ESRCH: the group is already gone, which is the state we wanted.
                Err(nix::errno::Errno::ESRCH) => Ok(()),
                Err(e) => Err(SupervisorError::Signal {
                    pid,
                    signal: signal as i32,
                    detail: e.to_string(),
                }),
            }
        }
    }

    impl ProcessSupervisor for UnixProcessSupervisor {
        fn spawn(&self, spec: &CommandSpec) -> Result<SupervisedProcess, SupervisorError> {
            let started_at = Utc::now();
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if let Some(dir) = &spec.cwd {
                cmd.current_dir(dir);
            }
            if spec.clear_env {
                cmd.env_clear();
            }
            for (k, v) in &spec.env {
                cmd.env(k, v);
            }

            // SAFETY: `setsid` is async-signal-safe and is the documented way to detach
            // the child into a new session and process group. Running it in the
            // pre-exec hook means every descendant inherits the group, which is what
            // makes whole-tree cancellation possible.
            unsafe {
                cmd.pre_exec(|| {
                    nix::unistd::setsid().map_err(std::io::Error::from)?;
                    Ok(())
                });
            }

            let child = cmd.spawn().map_err(|e| SupervisorError::Spawn {
                program: spec.program.clone(),
                detail: e.to_string(),
            })?;

            Ok(SupervisedProcess {
                pid: child.id(),
                started_at,
                deadline: spec.timeout.map(|t| started_at + t),
                child: Some(child),
            })
        }

        fn status(
            &self,
            proc: &mut SupervisedProcess,
        ) -> Result<Option<ExitStatus>, SupervisorError> {
            let Some(child) = proc.child.as_mut() else {
                return Ok(None);
            };
            match child.try_wait() {
                Ok(Some(status)) => Ok(Some(to_exit_status(status))),
                Ok(None) => Ok(None),
                Err(e) => Err(SupervisorError::Wait {
                    pid: proc.pid,
                    detail: e.to_string(),
                }),
            }
        }

        fn cancel(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
            self.signal_group(proc.pid, Signal::SIGTERM)
        }

        fn kill_tree(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
            self.signal_group(proc.pid, Signal::SIGKILL)
        }

        fn is_alive(&self, proc: &SupervisedProcess) -> bool {
            // Signal 0 probes for existence without delivering anything.
            killpg(Pid::from_raw(proc.pid as i32), None).is_ok()
        }

        fn wait(
            &self,
            proc: &mut SupervisedProcess,
            clock: &dyn Clock,
        ) -> Result<ExitStatus, SupervisorError> {
            loop {
                if let Some(status) = self.status(proc)? {
                    return Ok(status);
                }

                if proc.is_overdue(clock.now()) {
                    // Graceful, then forceful. The grace window is bounded so a process
                    // that ignores SIGTERM cannot hold the worker forever.
                    self.cancel(proc)?;
                    let grace_end =
                        clock.now() + TimeDelta::try_seconds(GRACEFUL_STOP_GRACE).expect("valid");
                    while clock.now() < grace_end {
                        if self.status(proc)?.is_some() {
                            return Ok(ExitStatus::TimedOut);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
                    }
                    self.kill_tree(proc)?;
                    let _ = self.status(proc)?;
                    // Reap the direct child so it does not linger as a zombie.
                    if let Some(child) = proc.child.as_mut() {
                        let _ = child.wait();
                    }
                    return Ok(ExitStatus::TimedOut);
                }

                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
        }

        fn cleanup(&self, proc: &mut SupervisedProcess) -> Result<(), SupervisorError> {
            // Idempotent: if the tree is gone, killpg returns ESRCH which we map to Ok.
            self.kill_tree(proc)?;
            if let Some(mut child) = proc.child.take() {
                let _ = child.wait();
            }
            Ok(())
        }
    }

    fn to_exit_status(status: std::process::ExitStatus) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        match (status.code(), status.signal()) {
            (Some(code), _) => ExitStatus::Exited { code },
            (None, Some(signal)) => ExitStatus::Signalled { signal },
            (None, None) => ExitStatus::Signalled { signal: 0 },
        }
    }
}

#[cfg(unix)]
pub use unix::UnixProcessSupervisor;

/// The supervisor for the current platform.
#[cfg(unix)]
pub type PlatformSupervisor = UnixProcessSupervisor;

/// Supervision failures.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("failed to spawn '{program}': {detail}")]
    Spawn { program: String, detail: String },
    #[error("failed to signal {signal} to process group {pid}: {detail}")]
    Signal {
        pid: u32,
        signal: i32,
        detail: String,
    },
    #[error("failed to wait on process {pid}: {detail}")]
    Wait { pid: u32, detail: String },
    #[error("process supervision is not implemented for this platform")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_builds_incrementally() {
        let spec = CommandSpec::new("echo")
            .arg("hello")
            .args(["a", "b"])
            .env("KEY", "value")
            .timeout(TimeDelta::try_seconds(30).unwrap());

        assert_eq!(spec.program, "echo");
        assert_eq!(spec.args, vec!["hello", "a", "b"]);
        assert_eq!(spec.env.get("KEY").map(String::as_str), Some("value"));
        assert_eq!(spec.timeout.unwrap().num_seconds(), 30);
    }

    #[test]
    fn only_zero_exit_is_success() {
        assert!(ExitStatus::Exited { code: 0 }.is_success());
        assert!(!ExitStatus::Exited { code: 1 }.is_success());
        assert!(!ExitStatus::Signalled { signal: 9 }.is_success());
        // A timeout is never success: we could not observe completion.
        assert!(!ExitStatus::TimedOut.is_success());
        assert!(!ExitStatus::Cancelled.is_success());
    }

    #[test]
    fn overdue_requires_a_deadline() {
        let no_deadline = SupervisedProcess {
            pid: 1,
            started_at: Utc::now(),
            deadline: None,
            child: None,
        };
        // A process with no timeout can never be overdue.
        assert!(!no_deadline.is_overdue(Utc::now() + TimeDelta::try_days(365).unwrap()));
    }

    #[test]
    fn deadline_is_derived_from_timeout() {
        let started = Utc::now();
        let proc = SupervisedProcess {
            pid: 1,
            started_at: started,
            deadline: Some(started + TimeDelta::try_seconds(10).unwrap()),
            child: None,
        };
        assert!(!proc.is_overdue(started + TimeDelta::try_seconds(9).unwrap()));
        assert!(proc.is_overdue(started + TimeDelta::try_seconds(11).unwrap()));
    }
}
