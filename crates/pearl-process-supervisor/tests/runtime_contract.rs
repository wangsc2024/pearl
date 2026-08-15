//! Runtime Adapter Contract tests — Constitution Article 9, 系統開發需求書 §61.
//!
//! Article 9 says a backend that cannot be reliably cancelled must not be a runtime.
//! These tests are what "reliably" means in practice: after cancellation, no descendant
//! of the spawned process survives. Asserting only that the direct child died would pass
//! while leaking grandchildren, which is the actual failure mode in production.

#![cfg(unix)]

use chrono::TimeDelta;
use pearl_core::SystemClock;
use pearl_process_supervisor::{CommandSpec, ExitStatus, PlatformSupervisor, ProcessSupervisor};
use std::time::{Duration, Instant};

fn supervisor() -> PlatformSupervisor {
    PlatformSupervisor::new()
}

/// Blocks until `f` is true or the budget expires.
fn wait_until(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

/// Whether a pid exists, read straight from /proc so the test does not depend on the
/// code under test to answer the question.
fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn spawn_and_collect_a_successful_exit() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("exit 0"))
        .unwrap();

    let status = sup.wait(&mut proc, &SystemClock).unwrap();
    assert_eq!(status, ExitStatus::Exited { code: 0 });
    assert!(status.is_success());
}

#[test]
fn nonzero_exit_is_reported_faithfully() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("exit 42"))
        .unwrap();

    let status = sup.wait(&mut proc, &SystemClock).unwrap();
    assert_eq!(status, ExitStatus::Exited { code: 42 });
    assert!(!status.is_success());
}

#[test]
fn spawn_failure_is_an_error_not_a_panic() {
    let sup = supervisor();
    let err = sup
        .spawn_now(&CommandSpec::new("/definitely/not/a/real/binary"))
        .unwrap_err();
    assert!(err.to_string().contains("failed to spawn"), "got: {err}");
}

#[test]
fn status_is_non_blocking() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 5"))
        .unwrap();

    // Must return immediately with "still running" rather than waiting 5 seconds.
    let start = Instant::now();
    let status = sup.status(&mut proc).unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(status.is_none());

    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn cancel_terminates_the_process() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 60"))
        .unwrap();
    let pid = proc.pid;
    assert!(pid_alive(pid));

    sup.cancel(&proc).unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || sup
            .status(&mut proc)
            .unwrap()
            .is_some()),
        "cancelled process should exit"
    );
    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn cancel_reclaims_the_entire_process_tree() {
    // The Article 9 test that matters. A shell spawns a child shell which spawns a
    // grandchild; all three must be gone after cancellation.
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids");

    let script = format!(
        r#"
        echo $$ >> {path}
        /bin/sh -c '
            echo $$ >> {path}
            /bin/sh -c "echo \$\$ >> {path}; sleep 120" &
            sleep 120
        ' &
        sleep 120
        "#,
        path = pid_file.display()
    );

    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg(&script))
        .unwrap();

    // Wait for all three generations to register themselves.
    assert!(
        wait_until(Duration::from_secs(10), || {
            std::fs::read_to_string(&pid_file)
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() >= 3)
                .unwrap_or(false)
        }),
        "expected 3 generations to start; got: {:?}",
        std::fs::read_to_string(&pid_file)
    );

    let pids: Vec<u32> = std::fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert!(pids.len() >= 3);
    assert!(
        pids.iter().all(|p| pid_alive(*p)),
        "all generations should be alive before cancellation"
    );

    sup.kill_tree(&proc).unwrap();
    sup.cleanup(&mut proc).unwrap();

    // Every generation must be gone, not just the direct child.
    for pid in &pids {
        assert!(
            wait_until(Duration::from_secs(5), || !pid_alive(*pid)),
            "pid {pid} survived kill_tree; the execution scope leaked"
        );
    }
}

#[test]
fn timeout_kills_an_overrunning_process() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new("/bin/sh")
                .arg("-c")
                .arg("sleep 120")
                .timeout(TimeDelta::try_milliseconds(200).unwrap()),
        )
        .unwrap();
    let pid = proc.pid;

    let status = sup.wait(&mut proc, &SystemClock).unwrap();

    assert_eq!(status, ExitStatus::TimedOut);
    assert!(!status.is_success(), "a timeout is never success");
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(pid)),
        "a timed-out process must not survive"
    );
}

#[test]
fn a_process_ignoring_sigterm_is_still_killed() {
    // Graceful stop is an offer, not a negotiation. A process that traps SIGTERM must
    // not be able to hold the worker indefinitely.
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new("/bin/sh")
                .arg("-c")
                .arg("trap '' TERM; sleep 120")
                .timeout(TimeDelta::try_milliseconds(200).unwrap()),
        )
        .unwrap();
    let pid = proc.pid;

    let status = sup.wait(&mut proc, &SystemClock).unwrap();
    assert_eq!(status, ExitStatus::TimedOut);
    assert!(
        wait_until(Duration::from_secs(10), || !pid_alive(pid)),
        "SIGKILL must follow an ignored SIGTERM"
    );
}

#[test]
fn a_process_finishing_within_its_timeout_is_not_disturbed() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new("/bin/sh")
                .arg("-c")
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
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 60"))
        .unwrap();

    // Article 9 requires cleanup to be safe to call twice: a supervisor cannot know
    // whether an earlier cleanup attempt completed before it crashed.
    sup.cleanup(&mut proc).unwrap();
    sup.cleanup(&mut proc).unwrap();
    sup.cleanup(&mut proc).unwrap();
}

#[test]
fn cancelling_an_already_dead_process_is_not_an_error() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("exit 0"))
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    // The desired end state is "not running", which is already true.
    sup.cancel(&proc).unwrap();
    sup.kill_tree(&proc).unwrap();
}

#[test]
fn is_alive_tracks_the_process() {
    let sup = supervisor();
    let mut proc = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 30"))
        .unwrap();
    assert!(sup.is_alive(&proc));

    sup.cleanup(&mut proc).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || !sup.is_alive(&proc)),
        "is_alive should report false after cleanup"
    );
}

#[test]
fn environment_is_passed_to_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");

    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf '%s' \"$PEARL_TEST\" > {}", out.display()))
                .env("PEARL_TEST", "visible"),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    assert_eq!(std::fs::read_to_string(&out).unwrap(), "visible");
}

#[test]
fn clean_env_withholds_inherited_variables() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    std::env::set_var("PEARL_SECRET_FOR_TEST", "leaked");

    let sup = supervisor();
    let mut proc = sup
        .spawn_now(
            &CommandSpec::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "printf '%s' \"$PEARL_SECRET_FOR_TEST\" > {}",
                    out.display()
                ))
                .with_clean_env(),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    // Environment filtering is a security requirement (§60), not a convenience.
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "",
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
            &CommandSpec::new("/bin/sh")
                .arg("-c")
                .arg("pwd > pwd.txt")
                .cwd(dir.path()),
        )
        .unwrap();
    sup.wait(&mut proc, &SystemClock).unwrap();

    let recorded = std::fs::read_to_string(dir.path().join("pwd.txt")).unwrap();
    let expected = dir.path().canonicalize().unwrap();
    assert_eq!(
        std::path::Path::new(recorded.trim())
            .canonicalize()
            .unwrap(),
        expected
    );
}

#[test]
fn concurrent_processes_are_isolated_from_each_other() {
    // Each spawn gets its own process group, so cancelling one must not affect another.
    let sup = supervisor();
    let mut keep = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 30"))
        .unwrap();
    let mut kill = sup
        .spawn_now(&CommandSpec::new("/bin/sh").arg("-c").arg("sleep 30"))
        .unwrap();

    assert_ne!(keep.pid, kill.pid);
    sup.cleanup(&mut kill).unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(kill.pid)),
        "the targeted process should die"
    );
    assert!(
        sup.is_alive(&keep),
        "an unrelated process must survive its neighbour's cancellation"
    );

    sup.cleanup(&mut keep).unwrap();
}
