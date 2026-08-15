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
//! quota.
//!
//! Each platform has exactly one mechanism that actually contains a tree, and this crate
//! uses it rather than approximating it:
//!
//! | Platform | Scope            | Ask to stop  | Insist              | Liveness             |
//! |----------|------------------|--------------|---------------------|----------------------|
//! | Unix     | process group    | `SIGTERM`    | `SIGKILL`           | `killpg(pid, 0)`     |
//! | Windows  | job object       | Ctrl+Break   | `TerminateJobObject`| job active processes |
//!
//! Both escalate the same way: ask, wait out a bounded grace window, then insist. Both
//! report liveness for the whole scope, not just the direct child.
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
    /// The job object holding this process and its descendants.
    ///
    /// Windows has no process groups with signal semantics, so whole-tree cancellation
    /// goes through a job object instead. Unix carries no equivalent handle because the
    /// process group *is* the pid.
    #[cfg(windows)]
    job: Option<windows_impl::JobHandle>,
}

impl SupervisedProcess {
    /// Builds a handle to a process whose execution scope is already established.
    ///
    /// Platform-specific scope handles default to absent; the platform supervisor attaches
    /// them during `spawn`. Having one constructor keeps the cfg-dependent fields from
    /// leaking into every construction site.
    fn new(
        pid: u32,
        started_at: DateTime<Utc>,
        deadline: Option<DateTime<Utc>>,
        child: Option<Child>,
    ) -> Self {
        Self {
            pid,
            started_at,
            deadline,
            child,
            #[cfg(windows)]
            job: None,
        }
    }

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

    /// Read all captured stdout and stderr from the child process.
    ///
    /// This should be called after `wait()` has returned (the process has exited).
    /// The handles are consumed so this can only succeed once. Returns `(stdout, stderr)`.
    /// If the child or its handles are not available, returns empty strings.
    pub fn take_output(&mut self) -> (String, String) {
        use std::io::Read;

        let Some(child) = self.child.as_mut() else {
            return (String::new(), String::new());
        };

        let stdout = child
            .stdout
            .take()
            .map(|mut h| {
                let mut buf = String::new();
                let _ = h.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();

        let stderr = child
            .stderr
            .take()
            .map(|mut h| {
                let mut buf = String::new();
                let _ = h.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();

        (stdout, stderr)
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
    ///
    /// Takes the clock because the deadline is computed here and enforced in [`Self::wait`].
    /// Reading `Utc::now()` for one and the injected clock for the other made the timeout
    /// depend on the difference between two clocks: under a test clock, work either timed
    /// out instantly or never, depending on which side of the fixed instant real time
    /// happened to fall.
    fn spawn(
        &self,
        spec: &CommandSpec,
        clock: &dyn Clock,
    ) -> Result<SupervisedProcess, SupervisorError>;

    /// Starts a process against the real clock.
    ///
    /// For callers that have no injected clock. Production code should prefer
    /// [`Self::spawn`] so that the deadline and its enforcement share one time source.
    fn spawn_now(&self, spec: &CommandSpec) -> Result<SupervisedProcess, SupervisorError> {
        self.spawn(spec, &pearl_core::SystemClock)
    }

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

/// A borrowed supervisor supervises.
///
/// Lets a component that owns a supervisor lend it to something generic over
/// `S: ProcessSupervisor` without cloning it or giving up ownership. Every method takes
/// `&self`, so the forwarding is mechanical.
impl<T: ProcessSupervisor + ?Sized> ProcessSupervisor for &T {
    fn spawn(
        &self,
        spec: &CommandSpec,
        clock: &dyn Clock,
    ) -> Result<SupervisedProcess, SupervisorError> {
        (**self).spawn(spec, clock)
    }

    fn status(&self, proc: &mut SupervisedProcess) -> Result<Option<ExitStatus>, SupervisorError> {
        (**self).status(proc)
    }

    fn cancel(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
        (**self).cancel(proc)
    }

    fn kill_tree(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
        (**self).kill_tree(proc)
    }

    fn is_alive(&self, proc: &SupervisedProcess) -> bool {
        (**self).is_alive(proc)
    }

    fn wait(
        &self,
        proc: &mut SupervisedProcess,
        clock: &dyn Clock,
    ) -> Result<ExitStatus, SupervisorError> {
        (**self).wait(proc, clock)
    }

    fn cleanup(&self, proc: &mut SupervisedProcess) -> Result<(), SupervisorError> {
        (**self).cleanup(proc)
    }
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
        fn spawn(
            &self,
            spec: &CommandSpec,
            clock: &dyn Clock,
        ) -> Result<SupervisedProcess, SupervisorError> {
            let started_at = clock.now();
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

            Ok(SupervisedProcess::new(
                child.id(),
                started_at,
                spec.timeout.map(|t| started_at + t),
                Some(child),
            ))
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

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

    /// Exit code reported for a tree the supervisor terminated.
    const JOB_KILL_EXIT_CODE: u32 = 1;

    /// An owned job object handle.
    ///
    /// The job is created with `KILL_ON_JOB_CLOSE`, so the tree cannot outlive this
    /// handle. That makes the leak-free path the *default* one: even a panic that unwinds
    /// past `cleanup` still takes the descendants with it.
    #[derive(Debug)]
    pub struct JobHandle(HANDLE);

    // SAFETY: a job object handle is a kernel handle owned by the process, not by the
    // thread that created it. Every win32 call used here is thread-agnostic, and the
    // handle is closed exactly once, in `Drop`.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    impl JobHandle {
        /// Creates an anonymous job object that kills its members when closed.
        fn create() -> Result<Self, SupervisorError> {
            // SAFETY: both arguments are the documented "no attributes, no name" nulls.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(SupervisorError::Job {
                    detail: format!(
                        "CreateJobObjectW failed: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            let job = Self(handle);

            // SAFETY: `info` is fully zeroed and its size is passed as declared, which is
            // what SetInformationJobObject requires for this information class.
            let ok = unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(SupervisorError::Job {
                    detail: format!(
                        "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            Ok(job)
        }

        fn raw(&self) -> HANDLE {
            self.0
        }

        /// How many processes of this job are still running.
        ///
        /// This is the Windows counterpart of `killpg(pid, 0)`: it answers for the whole
        /// tree rather than for the direct child alone.
        fn active_processes(&self) -> u32 {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: the out-pointer and its declared size match the information class.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return 0;
            }
            info.ActiveProcesses
        }

        /// Terminates every process in the job.
        fn terminate(&self) -> Result<(), SupervisorError> {
            // SAFETY: `self.0` is a live job handle for as long as `self` exists.
            let ok = unsafe { TerminateJobObject(self.0, JOB_KILL_EXIT_CODE) };
            if ok == 0 {
                return Err(SupervisorError::Job {
                    detail: format!(
                        "TerminateJobObject failed: {}",
                        std::io::Error::last_os_error()
                    ),
                });
            }
            Ok(())
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: closed exactly once; `JobHandle` is not `Clone`.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// Supervises processes using Windows job objects.
    ///
    /// Windows has no process groups that carry signals, so the Unix approach does not
    /// transfer: `TerminateProcess` on the child leaves grandchildren running. A job
    /// object is the platform mechanism that actually contains a tree — descendants join
    /// automatically and cannot escape.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct WindowsProcessSupervisor;

    impl WindowsProcessSupervisor {
        pub fn new() -> Self {
            Self
        }

        /// How many processes of this execution scope are alive.
        ///
        /// Exposed for diagnostics and for the tree-containment test, which has to observe
        /// descendants rather than take their death on faith.
        pub fn active_process_count(&self, proc: &SupervisedProcess) -> u32 {
            proc.job.as_ref().map_or(0, |j| j.active_processes())
        }

        /// Asks the tree to stop the way a console user would: Ctrl+Break.
        ///
        /// Returns whether the request was delivered. It fails for processes with no
        /// console (a GUI or detached child), which is why callers must not treat this as
        /// sufficient — `wait` always escalates to the job kill.
        fn request_break(pid: u32) -> bool {
            // Group 0 means "every process sharing this console", which would include the
            // supervisor itself. Refusing it is not paranoia: it is the difference between
            // cancelling a task and killing PEARL.
            if pid == 0 {
                return false;
            }
            // SAFETY: the child was created with CREATE_NEW_PROCESS_GROUP, so its group id
            // equals its pid and the event reaches only that group.
            unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
        }
    }

    impl ProcessSupervisor for WindowsProcessSupervisor {
        fn spawn(
            &self,
            spec: &CommandSpec,
            clock: &dyn Clock,
        ) -> Result<SupervisedProcess, SupervisorError> {
            let started_at = clock.now();
            let job = JobHandle::create()?;

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

            // A new process group is what makes a targeted Ctrl+Break possible at all, and
            // it also stops a console Ctrl+C from reaching supervised work by accident.
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);

            let mut child = cmd.spawn().map_err(|e| SupervisorError::Spawn {
                program: spec.program.clone(),
                detail: e.to_string(),
            })?;

            // SAFETY: the child is alive and owned here, so its handle is valid.
            let assigned =
                unsafe { AssignProcessToJobObject(job.raw(), child.as_raw_handle() as HANDLE) };
            if assigned == 0 {
                let detail = format!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
                // A child that could not be put in a job is exactly the unsupervisable
                // process Article 9 forbids. Kill it rather than return a handle whose
                // cancel would silently do nothing.
                let _ = child.kill();
                let _ = child.wait();
                return Err(SupervisorError::Spawn {
                    program: spec.program.clone(),
                    detail,
                });
            }

            let mut proc = SupervisedProcess::new(
                child.id(),
                started_at,
                spec.timeout.map(|t| started_at + t),
                Some(child),
            );
            proc.job = Some(job);
            Ok(proc)
        }

        fn status(
            &self,
            proc: &mut SupervisedProcess,
        ) -> Result<Option<ExitStatus>, SupervisorError> {
            let pid = proc.pid;
            let Some(child) = proc.child.as_mut() else {
                return Ok(None);
            };
            match child.try_wait() {
                Ok(Some(status)) => Ok(Some(to_exit_status(status))),
                Ok(None) => Ok(None),
                Err(e) => Err(SupervisorError::Wait {
                    pid,
                    detail: e.to_string(),
                }),
            }
        }

        fn cancel(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
            // Graceful first. If the child has no console to receive it, fall through to
            // the job kill: an uncancellable runtime is not an acceptable outcome.
            if Self::request_break(proc.pid) {
                return Ok(());
            }
            self.kill_tree(proc)
        }

        fn kill_tree(&self, proc: &SupervisedProcess) -> Result<(), SupervisorError> {
            match proc.job.as_ref() {
                Some(job) => job.terminate(),
                // No job means nothing was ever spawned into one.
                None => Ok(()),
            }
        }

        fn is_alive(&self, proc: &SupervisedProcess) -> bool {
            self.active_process_count(proc) > 0
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
                    // Same escalation as Unix: ask, then insist. The grace window is
                    // bounded so a process that ignores Ctrl+Break cannot hold the worker.
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
                    if let Some(child) = proc.child.as_mut() {
                        let _ = child.wait();
                    }
                    return Ok(ExitStatus::TimedOut);
                }

                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
        }

        fn cleanup(&self, proc: &mut SupervisedProcess) -> Result<(), SupervisorError> {
            // Idempotent: terminating an already-empty job succeeds, and the job handle is
            // dropped only once, when the process handle is.
            self.kill_tree(proc)?;
            if let Some(mut child) = proc.child.take() {
                let _ = child.wait();
            }
            proc.job = None;
            Ok(())
        }
    }

    fn to_exit_status(status: std::process::ExitStatus) -> ExitStatus {
        match status.code() {
            Some(code) => ExitStatus::Exited { code },
            // Windows always reports a code; a missing one means the process was killed
            // by something outside our accounting, which is not success.
            None => ExitStatus::Cancelled,
        }
    }
}

#[cfg(unix)]
pub use unix::UnixProcessSupervisor;

#[cfg(windows)]
pub use windows_impl::WindowsProcessSupervisor;

/// The supervisor for the current platform.
#[cfg(unix)]
pub type PlatformSupervisor = UnixProcessSupervisor;

/// The supervisor for the current platform.
#[cfg(windows)]
pub type PlatformSupervisor = WindowsProcessSupervisor;

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
    /// A Windows job object operation failed.
    ///
    /// Distinct from `Signal` because the failure mode differs: a job error means the
    /// execution scope itself could not be established or torn down, so the process must
    /// not be treated as supervised at all.
    #[error("job object operation failed: {detail}")]
    Job { detail: String },
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
        let no_deadline = SupervisedProcess::new(1, Utc::now(), None, None);
        // A process with no timeout can never be overdue.
        assert!(!no_deadline.is_overdue(Utc::now() + TimeDelta::try_days(365).unwrap()));
    }

    #[test]
    fn deadline_is_derived_from_timeout() {
        let started = Utc::now();
        let proc = SupervisedProcess::new(
            1,
            started,
            Some(started + TimeDelta::try_seconds(10).unwrap()),
            None,
        );
        assert!(!proc.is_overdue(started + TimeDelta::try_seconds(9).unwrap()));
        assert!(proc.is_overdue(started + TimeDelta::try_seconds(11).unwrap()));
    }
}
