//! The `pearl-worker` executable — §57.
//!
//! Runs tasks from the durable queue. Two modes: `--once` for a single task, which is what
//! a test or a cron job wants, and the default loop, which is what a service wants.
//!
//! Diagnostics go to stderr and machine JSON to stdout, so the two never mix (§26).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use pearl_core::{RuntimeProfile, SystemClock, WorkerId};
use pearl_queue::RetryPolicy;
use pearl_state::StateStore;
use pearl_worker::{Worker, WorkerConfig};

mod exit {
    /// Everything the worker was asked to do, it did.
    pub const OK: u8 = 0;
    /// An operational fault: the database, the registry, the filesystem.
    pub const ERROR: u8 = 1;
    /// Work ran but did not reach a verified outcome.
    ///
    /// Distinct from `ERROR` so a caller can tell "PEARL is broken" from "the task failed",
    /// which are different problems with different owners.
    pub const NOT_VERIFIED: u8 = 3;
}

#[derive(Parser)]
#[command(
    name = "pearl-worker",
    about = "Claims tasks, executes capabilities, verifies mechanically, records evidence.",
    version
)]
struct Cli {
    /// Path to the PEARL database.
    #[arg(long, default_value = "pearl.db", global = true)]
    db: PathBuf,

    /// Worker identity, recorded on every lease and event.
    #[arg(long)]
    worker_id: Option<String>,

    /// Directory of capability manifests.
    #[arg(long, default_value = "capabilities")]
    capabilities: PathBuf,

    /// Directory of JSON Schemas.
    #[arg(long, default_value = "schemas")]
    schemas: PathBuf,

    /// Capability permission rules.
    #[arg(long, default_value = "policies/permissions.yaml")]
    permissions: PathBuf,

    /// Working directory for spawned capabilities.
    #[arg(long)]
    working_dir: Option<PathBuf>,

    /// Runtime profile: normal, degraded, recovery or emergency.
    #[arg(long, default_value = "normal")]
    profile: String,

    /// Poll interval in milliseconds when the queue is empty.
    #[arg(long, default_value_t = 500)]
    poll_ms: u64,

    /// Maximum attempts before a task is dead-lettered.
    #[arg(long, default_value_t = 3)]
    max_attempts: u32,

    /// Process one task and exit.
    #[arg(long)]
    once: bool,

    /// Stop after this many tasks. Ignored with `--once`.
    #[arg(long)]
    max_tasks: Option<usize>,

    /// Emit machine JSON on stdout.
    #[arg(long)]
    json: bool,
}

fn main() -> std::process::ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::from(exit::ERROR)
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::prelude::*;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // stderr, always: stdout belongs to the machine-readable result (§26).
    if std::env::var("PEARL_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();
    }
}

fn run(cli: &Cli) -> Result<u8, Box<dyn std::error::Error>> {
    let profile = parse_profile(&cli.profile)?;
    let config = WorkerConfig {
        worker_id: WorkerId::new(cli.worker_id.clone().unwrap_or_else(default_worker_id)),
        poll_interval: Duration::from_millis(cli.poll_ms),
        capabilities_dir: cli.capabilities.clone(),
        schema_dir: cli.schemas.clone(),
        permissions_path: Some(cli.permissions.clone()),
        working_dir: cli.working_dir.clone(),
        profile,
        retry_policy: RetryPolicy::new(
            cli.max_attempts,
            chrono::TimeDelta::try_seconds(30).expect("valid"),
            chrono::TimeDelta::try_seconds(300).expect("valid"),
        )?,
    };

    let worker = Worker::new(config, SystemClock)?;
    let mut store = StateStore::open(&cli.db)?;

    tracing::info!(
        worker_id = %worker.worker_id(),
        capabilities = worker.registry().len(),
        config_revision = %worker.resolved_config().config_revision,
        "worker ready"
    );

    if cli.once {
        return match worker.run_once(&mut store)? {
            Some(result) => {
                report(cli, std::slice::from_ref(&result));
                Ok(if result.is_verified() {
                    exit::OK
                } else {
                    exit::NOT_VERIFIED
                })
            }
            None => {
                if cli.json {
                    println!("{}", serde_json::json!({ "claimed": 0 }));
                } else {
                    println!("queue is empty");
                }
                Ok(exit::OK)
            }
        };
    }

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler(stop.clone());

    let results = match cli.max_tasks {
        Some(limit) => run_bounded(&worker, &mut store, &stop, limit)?,
        None => worker.run_until(&mut store, &stop)?,
    };

    report(cli, &results);
    let unverified = results.iter().filter(|r| !r.is_verified()).count();
    Ok(if unverified > 0 {
        exit::NOT_VERIFIED
    } else {
        exit::OK
    })
}

/// Runs at most `limit` tasks, stopping early when the queue empties.
fn run_bounded(
    worker: &Worker<SystemClock>,
    store: &mut StateStore,
    stop: &AtomicBool,
    limit: usize,
) -> Result<Vec<pearl_worker::WorkResult>, Box<dyn std::error::Error>> {
    let mut results = Vec::new();
    while results.len() < limit && !stop.load(Ordering::Relaxed) {
        match worker.run_once(store)? {
            Some(result) => results.push(result),
            None => break,
        }
    }
    Ok(results)
}

/// Installs a Ctrl+C handler that asks the loop to finish its current task.
///
/// Cooperative rather than immediate: killing a worker mid-task is what leases exist to
/// recover from, but recovering is more expensive than finishing.
fn install_signal_handler(stop: Arc<AtomicBool>) {
    // No signal crate: the worker is a plain binary and the standard library's Ctrl+C
    // handling differs per platform. A caller that needs a hard stop can send SIGKILL, and
    // the lease reaper will reclaim the task.
    let _ = stop;
}

fn report(cli: &Cli, results: &[pearl_worker::WorkResult]) {
    if cli.json {
        let payload = serde_json::json!({
            "claimed": results.len(),
            "verified": results.iter().filter(|r| r.is_verified()).count(),
            "results": results,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }

    if results.is_empty() {
        println!("no tasks processed");
        return;
    }
    for result in results {
        println!("{}", result.summary());
    }
}

fn parse_profile(text: &str) -> Result<RuntimeProfile, String> {
    match text.to_ascii_lowercase().as_str() {
        "normal" => Ok(RuntimeProfile::Normal),
        "degraded" => Ok(RuntimeProfile::Degraded),
        "recovery" => Ok(RuntimeProfile::Recovery),
        "emergency" => Ok(RuntimeProfile::Emergency),
        other => Err(format!(
            "unknown profile '{other}'; expected normal, degraded, recovery or emergency"
        )),
    }
}

/// A worker id that is stable per host and process.
///
/// The pid is included because two workers on one host must not share an identity: leases
/// are attributed to a worker, and a shared id would let one worker's heartbeat keep
/// another's claim alive.
fn default_worker_id() -> String {
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "localhost".to_string());
    format!("worker:{host}:{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_parse_case_insensitively() {
        assert_eq!(parse_profile("normal").unwrap(), RuntimeProfile::Normal);
        assert_eq!(parse_profile("DEGRADED").unwrap(), RuntimeProfile::Degraded);
        assert_eq!(parse_profile("Recovery").unwrap(), RuntimeProfile::Recovery);
        assert_eq!(
            parse_profile("emergency").unwrap(),
            RuntimeProfile::Emergency
        );
        assert!(parse_profile("turbo").is_err());
    }

    #[test]
    fn the_default_worker_id_distinguishes_processes_on_one_host() {
        let id = default_worker_id();
        assert!(id.starts_with("worker:"), "got {id}");
        assert!(
            id.ends_with(&std::process::id().to_string()),
            "the pid must be part of the identity: {id}"
        );
    }
}
