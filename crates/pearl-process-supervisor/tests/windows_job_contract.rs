//! Windows Runtime Adapter Contract tests — Constitution Article 9, 系統開發需求書 §61.
//!
//! The Unix suite in `runtime_contract.rs` proves the same contract through process
//! groups. This file proves it through job objects, because that is the only Windows
//! mechanism that actually contains a tree: `TerminateProcess` on the direct child leaves
//! grandchildren running, holding file locks and API quota.
//!
//! The load-bearing test is [`kill_tree_reclaims_the_entire_process_tree`]. It spawns
//! three generations, confirms every one of them is alive using win32 directly rather than
//! trusting the supervisor, and then requires all three to be gone.

#![cfg(windows)]

use chrono::TimeDelta;
use pearl_core::SystemClock;
use pearl_process_supervisor::{CommandSpec, ExitStatus, PlatformSupervisor, ProcessSupervisor};
use std::path::Path;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// `STATUS_PENDING`: the exit code Windows reports for a process that has not exited.
const STILL_ACTIVE: u32 = 259;

fn supervisor() -> PlatformSupervisor {
    PlatformSupervisor::new()
}

/// The command interpreter, taken from the environment rather than assumed.
fn cmd_exe() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string())
}

/// A PowerShell that exists on this machine.
///
/// `pwsh` is what the script runtime targets; `powershell` is the one Windows always
/// ships. Trying both keeps the test meaningful on either.
fn powershell() -> &'static str {
    fn works(program: &str) -> bool {
        std::process::Command::new(program)
            .args(["-NoProfile", "-Command", "exit 0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    if works("pwsh") {
        "pwsh"
    } else {
        "powershell"
    }
}

/// Blocks until `f` is true or the budget expires.
fn wait_until(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    f()
}

/// Whether a pid is a running process, asked of the OS directly.
///
/// A pid whose handle can still be opened may already have exited — the supervisor holds
/// a handle to its direct child, which keeps the pid resolvable after death. So liveness
/// is decided by the exit code, not by whether the open succeeded.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: a failed open returns null, which is checked before use.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code: u32 = 0;
    // SAFETY: `handle` is a live process handle and `code` is a valid out-pointer.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    // SAFETY: closed exactly once, immediately after the last use of the handle.
    unsafe {
        CloseHandle(handle);
    }
    ok != 0 && code == STILL_ACTIVE
}

/// A long-running child that needs no shell features.
fn sleeper() -> CommandSpec {
    // `ping` to loopback is the classic console-app sleep: no window, predictable, and
    // present on every Windows install.
    CommandSpec::new(cmd_exe())
        .arg("/C")
        .arg("ping -n 200 127.0.0.1 > NUL")
}

#[test]
fn spawn_and_collect_a_successful_exit() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new(cmd_exe()).arg("/C").arg("exit 0"))
        .unwrap();

    let status = sup.wait(&mut proc, &SystemClock).unwrap();
    assert_eq!(status, ExitStatus::Exited { code: 0 });
    assert!(status.is_success());
}

#[test]
fn nonzero_exit_is_reported_faithfully() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new(cmd_exe()).arg("/C").arg("exit 42"))
        .unwrap();

    let status = sup.wait(&mut proc, &SystemClock).unwrap();
    assert_eq!(status, ExitStatus::Exited { code: 42 });
    assert!(!status.is_success());
}

#[test]
fn spawn_failure_is_an_error_not_a_panic() {
    let sup = supervisor();
    let err = sup
        .spawn_now(&CommandSpec::new(r"C:\definitely\not\a\real\binary.exe"))
        .unwrap_err();
    assert!(err.to_string().contains("failed to spawn"), "got: {err}");
}

#[test]
fn status_is_non_blocking() {
    let sup = supervisor();
    let mut proc = sup.spawn_now(&sleeper()).unwrap();

    let start = Instant::now();
    let status = sup.status(&mut proc).unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(status.is_none());

    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn cancel_terminates_the_process() {
    let sup = supervisor();
    let mut proc = sup.spawn_now(&sleeper()).unwrap();
    let pid = proc.pid;
    assert!(
        wait_until(Duration::from_secs(5), || pid_alive(pid)),
        "the child should be running before cancellation"
    );

    sup.cancel(&proc).unwrap();

    // Whether Ctrl+Break was deliverable or the job kill did it, the observable
    // requirement is the same: the process is gone.
    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(pid)),
        "a cancelled process must not survive"
    );
    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn kill_tree_reclaims_the_entire_process_tree() {
    // The Article 9 test that matters. Three generations, each recording its own pid;
    // all three must be gone afterwards. Asserting only on the direct child would pass
    // while leaking the grandchildren, which is the real production failure.
    let shell = powershell();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("pids.txt");
    std::fs::write(&log, "").unwrap();

    // Separate script files rather than nested inline quoting: three levels of PowerShell
    // escaping is a test that fails for reasons unrelated to what it measures.
    let gen3 = dir.path().join("gen3.ps1");
    let gen2 = dir.path().join("gen2.ps1");
    let gen1 = dir.path().join("gen1.ps1");

    std::fs::write(
        &gen3,
        "Add-Content -Path $env:PEARL_PID_LOG -Value $PID\nStart-Sleep -Seconds 300\n",
    )
    .unwrap();
    std::fs::write(
        &gen2,
        format!(
            "Add-Content -Path $env:PEARL_PID_LOG -Value $PID\n\
             Start-Process -NoNewWindow -FilePath '{shell}' -ArgumentList '-NoProfile','-File','{gen3}'\n\
             Start-Sleep -Seconds 300\n",
            gen3 = gen3.display()
        ),
    )
    .unwrap();
    std::fs::write(
        &gen1,
        format!(
            "Add-Content -Path $env:PEARL_PID_LOG -Value $PID\n\
             Start-Process -NoNewWindow -FilePath '{shell}' -ArgumentList '-NoProfile','-File','{gen2}'\n\
             Start-Sleep -Seconds 300\n",
            gen2 = gen2.display()
        ),
    )
    .unwrap();

    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new(shell)
                .args(["-NoProfile", "-File"])
                .arg(gen1.to_string_lossy().to_string())
                .env("PEARL_PID_LOG", log.to_string_lossy().to_string()),
        )
        .unwrap();

    let pids_recorded = || -> Vec<u32> {
        std::fs::read_to_string(&log)
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    };

    // PowerShell start-up is slow; three generations need a generous budget.
    assert!(
        wait_until(Duration::from_secs(60), || pids_recorded().len() >= 3),
        "expected 3 generations to start; recorded: {:?}",
        pids_recorded()
    );

    let pids = pids_recorded();
    assert!(
        pids.iter().all(|p| pid_alive(*p)),
        "all generations should be alive before the kill: {pids:?}"
    );
    // Descendants must have joined the job, otherwise the kill below would be a no-op
    // for them and this test would be proving nothing.
    assert!(
        sup.active_process_count(&proc) >= 3,
        "descendants did not join the job object; scope containment is broken"
    );

    sup.kill_tree(&proc).unwrap();
    sup.cleanup(&mut proc).unwrap();

    for pid in &pids {
        assert!(
            wait_until(Duration::from_secs(10), || !pid_alive(*pid)),
            "pid {pid} survived kill_tree; the execution scope leaked"
        );
    }
}

#[test]
fn timeout_kills_an_overrunning_process() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&sleeper().timeout(TimeDelta::try_milliseconds(200).unwrap()))
        .unwrap();
    let pid = proc.pid;

    let status = sup.wait(&mut proc, &SystemClock).unwrap();

    assert_eq!(status, ExitStatus::TimedOut);
    assert!(!status.is_success(), "a timeout is never success");
    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(pid)),
        "a timed-out process must not survive"
    );
}

#[test]
fn a_process_finishing_within_its_timeout_is_not_disturbed() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new(cmd_exe())
                .arg("/C")
                .arg("exit 7")
                .timeout(TimeDelta::try_seconds(30).unwrap()),
        )
        .unwrap();

    assert_eq!(
        sup.wait(&mut proc, &SystemClock).unwrap(),
        ExitStatus::Exited { code: 7 }
    );
}

#[test]
fn cleanup_is_idempotent() {
    let sup = supervisor();
    let mut proc = sup.spawn_now(&sleeper()).unwrap();

    // A supervisor cannot know whether an earlier cleanup completed before it crashed,
    // so calling it again must be safe.
    sup.cleanup(&mut proc).unwrap();
    sup.cleanup(&mut proc).unwrap();
    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn cancelling_an_already_dead_process_is_not_an_error() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new(cmd_exe()).arg("/C").arg("exit 0"))
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    // The desired end state is "not running", which already holds.
    sup.cancel(&proc).unwrap();
    sup.kill_tree(&proc).unwrap();
}

#[test]
fn is_alive_tracks_the_whole_scope() {
    let sup = supervisor();
    let mut proc = sup.spawn_now(&sleeper()).unwrap();
    assert!(sup.is_alive(&proc));

    sup.cleanup(&mut proc).unwrap();
    assert!(
        !sup.is_alive(&proc),
        "is_alive must report false once the scope is gone"
    );
}

#[test]
fn environment_is_passed_to_the_child() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new(cmd_exe())
                .arg("/C")
                .arg("echo %PEARL_TEST%")
                .env("PEARL_TEST", "visible"),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    let (stdout, _) = proc.take_output();
    assert_eq!(stdout.trim(), "visible");
}

#[test]
fn clean_env_withholds_inherited_variables() {
    std::env::set_var("PEARL_SECRET_FOR_TEST", "leaked");

    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new(cmd_exe())
                .arg("/C")
                .arg("echo %PEARL_SECRET_FOR_TEST%")
                .with_clean_env(),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    let (stdout, _) = proc.take_output();
    // cmd leaves an undefined variable unexpanded, so the literal token is the proof
    // that nothing was inherited. Environment filtering is a security requirement (§60).
    assert_eq!(
        stdout.trim(),
        "%PEARL_SECRET_FOR_TEST%",
        "a clean environment must not leak inherited variables"
    );
    std::env::remove_var("PEARL_SECRET_FOR_TEST");
}

#[test]
fn working_directory_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new(cmd_exe())
                .arg("/C")
                .arg("cd")
                .cwd(dir.path()),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    let (stdout, _) = proc.take_output();
    // Temp paths can be reported in short (8.3) form, so compare canonical forms.
    assert_eq!(
        Path::new(stdout.trim()).canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[test]
fn concurrent_processes_are_isolated_from_each_other() {
    // Each spawn gets its own job, so killing one must not touch another.
    let sup = supervisor();
    let mut keep = sup.spawn_now(&sleeper()).unwrap();
    let mut kill = sup.spawn_now(&sleeper()).unwrap();
    let kill_pid = kill.pid;

    assert_ne!(keep.pid, kill.pid);
    sup.cleanup(&mut kill).unwrap();

    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(kill_pid)),
        "the targeted process should die"
    );
    assert!(
        sup.is_alive(&keep),
        "an unrelated process must survive its neighbour's cancellation"
    );

    sup.cleanup(&mut keep).unwrap();
}
