//! `pearl` — the operator command line — 系統開發需求書 §59.
//!
//! Every command is mechanical. Nothing here consults an LLM, which is the point: the
//! Phase 1 kernel must be fully operable without one.

mod spec;

use chrono::TimeDelta;
use clap::{Parser, Subcommand};
use pearl_core::{Clock, RuntimeProfile, SystemClock, TaskId, TaskState};
use pearl_governance::{run_gate, CapabilityManifest};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_queue::{RetryPolicy, WorkQueue};
use pearl_state::StateStore;
use spec::TaskSpec;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Exit codes. Distinct so CI can tell a Constitution violation from a crash.
mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const CONSTITUTION_VIOLATION: u8 = 2;
}

#[derive(Parser)]
#[command(
    name = "pearl",
    about = "PEARL — deterministic-first autonomous execution framework",
    version
)]
struct Cli {
    /// Path to the PEARL database.
    #[arg(long, global = true, default_value = "pearl.db")]
    db: PathBuf,

    /// Emit machine-readable JSON instead of human text.
    ///
    /// §26 forbids mixing the two on one stream, so this is a mode rather than a
    /// decoration.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Task operations.
    #[command(subcommand)]
    Task(TaskCommand),
    /// Run inspection.
    #[command(subcommand)]
    Run(RunCommand),
    /// Event ledger operations.
    #[command(subcommand)]
    Event(EventCommand),
    /// Queue operations.
    #[command(subcommand)]
    Queue(QueueCommand),
    /// Lease operations.
    #[command(subcommand)]
    Lease(LeaseCommand),
    /// Constitution gate.
    #[command(subcommand)]
    Constitution(ConstitutionCommand),
    /// Report kernel health.
    Doctor,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Submit a task spec.
    Submit {
        /// Path to a task spec (YAML or JSON).
        file: PathBuf,
    },
    /// Show a task with its runs and history.
    Inspect { task_id: String },
    /// List tasks, optionally filtered by state.
    List {
        #[arg(long)]
        state: Option<String>,
    },
    /// Cancel a task.
    Cancel {
        task_id: String,
        #[arg(long, default_value = "cancelled by operator")]
        reason: String,
    },
}

#[derive(Subcommand)]
enum RunCommand {
    /// Show a run and its attempts.
    Inspect { run_id: String },
}

#[derive(Subcommand)]
enum EventCommand {
    /// Print a task's event history.
    Log { task_id: String },
    /// Rebuild materialized state from the ledger.
    Replay,
}

#[derive(Subcommand)]
enum QueueCommand {
    /// Show queue depth and the head of the queue.
    Status {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Promote retry-waiting tasks whose backoff has elapsed.
    Promote,
}

#[derive(Subcommand)]
enum LeaseCommand {
    /// Reclaim expired leases.
    Reap,
    /// List active leases.
    List,
}

#[derive(Subcommand)]
enum ConstitutionCommand {
    /// Check capability manifests against the twelve articles.
    Check {
        /// Directory containing manifests, or a single manifest file.
        #[arg(default_value = "capabilities")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    init_tracing();

    let cli = Cli::parse();
    match dispatch(&cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // Diagnostics go to stderr so stdout stays machine-parseable (§26).
            eprintln!("error: {e}");
            ExitCode::from(exit::ERROR)
        }
    }
}

/// Initialize tracing subscriber.
///
/// When `PEARL_LOG_FORMAT=json` is set, outputs structured JSON logs to stderr.
/// Otherwise, outputs human-readable logs. This satisfies SS60 Observability.
fn init_tracing() {
    use tracing_subscriber::prelude::*;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));

    let log_format = std::env::var("PEARL_LOG_FORMAT").unwrap_or_default();

    if log_format == "json" {
        let json_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_thread_ids(true)
            .with_span_list(true);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(json_layer)
            .init();
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }
}

fn dispatch(cli: &Cli) -> Result<u8, Box<dyn std::error::Error>> {
    match &cli.command {
        Command::Task(cmd) => task(cli, cmd),
        Command::Run(cmd) => run(cli, cmd),
        Command::Event(cmd) => event(cli, cmd),
        Command::Queue(cmd) => queue(cli, cmd),
        Command::Lease(cmd) => lease(cli, cmd),
        Command::Constitution(cmd) => constitution(cli, cmd),
        Command::Doctor => doctor(cli),
    }
}

fn task(cli: &Cli, cmd: &TaskCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        TaskCommand::Submit { file } => {
            let source = std::fs::read_to_string(file)?;
            let parsed = TaskSpec::parse(&source)?;

            // A Constitution violation is not an ordinary error: it gets its own exit
            // code so CI can distinguish "the operator wrote an impossible task" from
            // "the disk is full".
            let submission = match parsed.into_submission() {
                Ok(s) => s,
                Err(spec::SpecError::ConstitutionViolation { article, detail }) => {
                    eprintln!("Constitution Article {article}: {detail}");
                    return Ok(exit::CONSTITUTION_VIOLATION);
                }
                Err(e) => return Err(Box::new(e)),
            };

            let mut store = StateStore::open(&cli.db)?;
            let record = store.create_task(submission, SystemClock.now())?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("submitted {} in state {}", record.task_id, record.state);
                println!("  trace_id: {}", record.trace_id);
            }
            Ok(exit::OK)
        }

        TaskCommand::Inspect { task_id } => {
            let store = StateStore::open(&cli.db)?;
            let id = TaskId::parse(task_id.clone())?;
            let Some(record) = store.get_task(&id)? else {
                eprintln!("task '{task_id}' not found");
                return Ok(exit::ERROR);
            };
            let runs = store.runs_for_task(&id)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task": record,
                        "runs": runs,
                    }))?
                );
            } else {
                println!("task        {}", record.task_id);
                println!("state       {}", record.state);
                println!("type        {}", record.task_type);
                println!("trace_id    {}", record.trace_id);
                println!("attempts    {}", record.attempt_count);
                println!(
                    "exactness   required={} verifiable={}",
                    record.quality.exactness_required, record.quality.deterministic_verification
                );
                if let Some(reason) = &record.last_reason {
                    println!("reason      {reason}");
                }
                if record.state == TaskState::Unverified {
                    println!();
                    println!("NOTE: UNVERIFIED means the work may be correct but nothing can");
                    println!("      confirm it. Build a verifier or use a human gate (Article 2).");
                }
                println!();
                println!("runs ({})", runs.len());
                for r in &runs {
                    println!(
                        "  {} started={} outcome={} config={}",
                        r.run_id,
                        r.started_at.to_rfc3339(),
                        r.outcome.as_deref().unwrap_or("-"),
                        r.config_revision
                    );
                }
            }
            Ok(exit::OK)
        }

        TaskCommand::List { state } => {
            let store = StateStore::open(&cli.db)?;
            let tasks = match state {
                Some(raw) => {
                    let parsed =
                        TaskState::parse(raw).ok_or_else(|| format!("unknown state '{raw}'"))?;
                    store.list_by_state(parsed)?
                }
                None => store.all_tasks()?,
            };

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("no tasks");
            } else {
                println!("{:<28} {:<18} {:<9} UPDATED", "TASK", "STATE", "ATTEMPTS");
                for t in &tasks {
                    // Rendered to String first: a custom Display impl only honours width
                    // if it calls Formatter::pad, which these newtypes do not.
                    println!(
                        "{:<28} {:<18} {:<9} {}",
                        t.task_id.to_string(),
                        t.state.as_str(),
                        t.attempt_count,
                        t.updated_at.to_rfc3339()
                    );
                }
            }
            Ok(exit::OK)
        }

        TaskCommand::Cancel { task_id, reason } => {
            let mut store = StateStore::open(&cli.db)?;
            let id = TaskId::parse(task_id.clone())?;
            let record = store.transition(
                &id,
                TaskState::Cancelled,
                Some(reason.clone()),
                None,
                SystemClock.now(),
            )?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("cancelled {}", record.task_id);
            }
            Ok(exit::OK)
        }
    }
}

fn run(cli: &Cli, cmd: &RunCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        RunCommand::Inspect { run_id } => {
            let store = StateStore::open(&cli.db)?;
            let id = pearl_core::RunId::parse(run_id)?;
            let Some(record) = store.get_run(id)? else {
                eprintln!("run '{run_id}' not found");
                return Ok(exit::ERROR);
            };
            let attempts = store.attempts_for_run(id)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "run": record,
                        "attempts": attempts,
                    }))?
                );
            } else {
                println!("run             {}", record.run_id);
                println!("task            {}", record.task_id);
                println!("started         {}", record.started_at.to_rfc3339());
                println!(
                    "outcome         {}",
                    record.outcome.as_deref().unwrap_or("-")
                );
                // Article 10: these are what make the run reproducible.
                println!("config_revision {}", record.config_revision);
                println!("config_hash     {}", record.config_hash);
                println!();
                println!("attempts ({})", attempts.len());
                for a in &attempts {
                    println!(
                        "  #{} outcome={} exit={}",
                        a.attempt_number,
                        a.outcome.as_deref().unwrap_or("-"),
                        a.exit_reason.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(exit::OK)
        }
    }
}

fn event(cli: &Cli, cmd: &EventCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        EventCommand::Log { task_id } => {
            let store = StateStore::open(&cli.db)?;
            let id = TaskId::parse(task_id.clone())?;
            let events = store.ledger().read_task(&id)?;

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else if events.is_empty() {
                println!("no events for '{task_id}'");
            } else {
                for e in &events {
                    println!("{}  {}", e.occurred_at.to_rfc3339(), e.event_type());
                }
            }
            Ok(exit::OK)
        }

        EventCommand::Replay => {
            let mut store = StateStore::open(&cli.db)?;
            let summary = store.rebuild_from_ledger()?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "total_events": summary.total_events,
                        "applied": summary.applied,
                        "skipped": summary.skipped,
                    }))?
                );
            } else {
                println!(
                    "replayed {} events ({} applied, {} inert)",
                    summary.total_events, summary.applied, summary.skipped
                );
            }
            Ok(exit::OK)
        }
    }
}

fn queue(cli: &Cli, cmd: &QueueCommand) -> Result<u8, Box<dyn std::error::Error>> {
    let q = WorkQueue::new(RetryPolicy::default(), RuntimeProfile::Normal, SystemClock);
    match cmd {
        QueueCommand::Status { limit } => {
            let store = StateStore::open(&cli.db)?;
            let depth = q.depth(&store)?;
            let head = q.peek(&store, *limit)?;
            let retrying = store.count_by_state(TaskState::RetryWait)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "depth": depth,
                        "retry_wait": retrying,
                        "head": head,
                    }))?
                );
            } else {
                println!("ready       {depth}");
                println!("retry_wait  {retrying}");
                for t in &head {
                    println!("  {} (created {})", t.task_id, t.created_at.to_rfc3339());
                }
            }
            Ok(exit::OK)
        }

        QueueCommand::Promote => {
            let mut store = StateStore::open(&cli.db)?;
            let promoted = q.promote_ready_retries(&mut store)?;
            if cli.json {
                let ids: Vec<String> = promoted.iter().map(ToString::to_string).collect();
                println!("{}", serde_json::to_string_pretty(&ids)?);
            } else {
                println!("promoted {} task(s)", promoted.len());
                for id in &promoted {
                    println!("  {id}");
                }
            }
            Ok(exit::OK)
        }
    }
}

fn lease(cli: &Cli, cmd: &LeaseCommand) -> Result<u8, Box<dyn std::error::Error>> {
    let mgr = LeaseManager::new(LeaseConfig::default(), SystemClock);
    match cmd {
        LeaseCommand::Reap => {
            let mut store = StateStore::open(&cli.db)?;
            let report = mgr.reap(&mut store)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "reclaimed": report.reclaimed.iter().map(ToString::to_string).collect::<Vec<_>>(),
                        "skipped": report.skipped.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("reclaimed {} task(s)", report.reclaimed.len());
                for id in &report.reclaimed {
                    println!("  {id}");
                }
                if !report.skipped.is_empty() {
                    println!("closed without requeue: {}", report.skipped.len());
                }
            }
            Ok(exit::OK)
        }

        LeaseCommand::List => {
            let store = StateStore::open(&cli.db)?;
            // Anything not yet past a full lease duration into the future is open.
            let horizon = SystemClock.now() + TimeDelta::try_days(365).expect("valid");
            let leases = store.expired_leases(horizon)?;
            let open: Vec<_> = leases.iter().filter(|l| l.released_at.is_none()).collect();

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&open)?);
            } else if open.is_empty() {
                println!("no open leases");
            } else {
                println!("{:<28} {:<18} LEASED UNTIL", "TASK", "WORKER");
                for l in &open {
                    println!(
                        "{:<28} {:<18} {}",
                        l.task_id.to_string(),
                        l.worker_id.to_string(),
                        l.leased_until.to_rfc3339()
                    );
                }
            }
            Ok(exit::OK)
        }
    }
}

fn constitution(cli: &Cli, cmd: &ConstitutionCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        ConstitutionCommand::Check { path } => {
            let manifests = load_manifests(path)?;
            let refs: Vec<&CapabilityManifest> = manifests.iter().collect();
            let report = run_gate(refs);

            if cli.json {
                let findings: Vec<_> = report
                    .findings
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "article": f.article.0,
                            "check": f.check,
                            "severity": if f.is_violation() { "violation" } else { "warning" },
                            "subject": f.subject,
                            "detail": f.detail,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "inspected": report.inspected,
                        "violations": report.violation_count(),
                        "passed": report.passed(),
                        "findings": findings,
                    }))?
                );
            } else {
                println!("inspected {} manifest(s)", report.inspected);
                for finding in &report.findings {
                    println!("{finding}");
                }
                println!();
                if report.passed() {
                    println!("constitution check PASSED");
                } else {
                    println!(
                        "constitution check FAILED: {} violation(s)",
                        report.violation_count()
                    );
                }
            }

            // The gate's whole purpose is to fail the build.
            Ok(if report.passed() {
                exit::OK
            } else {
                exit::CONSTITUTION_VIOLATION
            })
        }
    }
}

/// Loads manifests from a file or a directory of `.yaml`/`.yml` files.
fn load_manifests(path: &Path) -> Result<Vec<CapabilityManifest>, Box<dyn std::error::Error>> {
    if path.is_file() {
        let source = std::fs::read_to_string(path)?;
        return Ok(vec![CapabilityManifest::from_yaml(&source)?]);
    }
    if !path.exists() {
        return Err(format!("path '{}' does not exist", path.display()).into());
    }

    let mut manifests = Vec::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    collect_yaml(path, &mut paths)?;
    // Sorted so gate output does not depend on filesystem iteration order.
    paths.sort();

    for file in paths {
        let source = std::fs::read_to_string(&file)?;
        let manifest = CapabilityManifest::from_yaml(&source)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        manifests.push(manifest);
    }
    Ok(manifests)
}

fn collect_yaml(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml(&path, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "yaml" || e == "yml")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn doctor(cli: &Cli) -> Result<u8, Box<dyn std::error::Error>> {
    let store = StateStore::open(&cli.db)?;
    let now = SystemClock.now();

    let ledger_events = store.ledger().count()?;
    let mut by_state = Vec::new();
    for state in [
        TaskState::Created,
        TaskState::Ready,
        TaskState::Leased,
        TaskState::Running,
        TaskState::Verifying,
        TaskState::VerifiedSuccess,
        TaskState::Unverified,
        TaskState::RetryWait,
        TaskState::Blocked,
        TaskState::Failed,
        TaskState::Cancelled,
        TaskState::Dead,
    ] {
        let n = store.count_by_state(state)?;
        if n > 0 {
            by_state.push((state, n));
        }
    }
    let expired = store.expired_leases(now)?.len();
    let unverified = store.count_by_state(TaskState::Unverified)?;

    // Stuck work is the symptom Article 6 exists to make visible.
    let mut warnings = Vec::new();
    if expired > 0 {
        warnings.push(format!(
            "{expired} expired lease(s) awaiting reclamation; run `pearl lease reap`"
        ));
    }
    if unverified > 0 {
        warnings.push(format!(
            "{unverified} task(s) in UNVERIFIED; they need a verifier or a human gate (Article 2)"
        ));
    }

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "database": cli.db.display().to_string(),
                "ledger_events": ledger_events,
                "tasks_by_state": by_state
                    .iter()
                    .map(|(s, n)| (s.as_str().to_string(), *n))
                    .collect::<std::collections::BTreeMap<_, _>>(),
                "expired_leases": expired,
                "warnings": warnings,
                "healthy": warnings.is_empty(),
            }))?
        );
    } else {
        println!("database       {}", cli.db.display());
        println!("ledger events  {ledger_events}");
        println!();
        if by_state.is_empty() {
            println!("no tasks");
        } else {
            for (state, n) in &by_state {
                println!("  {:<18} {}", state.as_str(), n);
            }
        }
        println!();
        if warnings.is_empty() {
            println!("doctor: no problems found");
        } else {
            for w in &warnings {
                println!("warning: {w}");
            }
        }
    }
    Ok(exit::OK)
}
