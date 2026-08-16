//! `pearl` — the operator command line — 系統開發需求書 §59.
//!
//! Every command is mechanical. Nothing here consults an LLM, which is the point: the
//! Phase 1 kernel must be fully operable without one.

use chrono::TimeDelta;
use clap::{Parser, Subcommand};
use pearl_assurance::{
    AssuranceCheck, AssuranceEngine, AssuranceSpec, CheckContext, CheckKind, CheckOutcome,
    RuntimeCheckRunner,
};
use pearl_capabilities::CapabilityRegistry;
use pearl_core::{Clock, RuntimeProfile, SystemClock, TaskId, TaskState, WorkerId};
use pearl_executor::{
    step_executor_fn, Checkpoint, CheckpointSink, Executor, ExecutorConfig, RuntimeStepExecutor,
};
use pearl_governance::{run_gate, CapabilityManifest};
use pearl_lease::{LeaseConfig, LeaseManager};
use pearl_process_supervisor::PlatformSupervisor;
use pearl_queue::{RetryPolicy, WorkQueue};
use pearl_runtime::{
    family_of, AgentCliAdapter, ApiRuntimeAdapter, RuntimeAdapter, RuntimeFamily,
    ScriptRuntimeAdapter, ScriptSpec,
};
use pearl_state::StateStore;
use pearl_state::{SpecError, TaskSpec};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Fallback timeout for a capability whose manifest omits one.
const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

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
    /// Capability registry operations.
    #[command(subcommand)]
    Capability(CapabilityCommand),
    /// Script execution operations.
    #[command(subcommand)]
    Script(ScriptCommand),
    /// Workflow operations.
    #[command(subcommand)]
    Workflow(WorkflowCommand),
    /// Verification: ask a machine verifier, or read what one already said.
    #[command(subcommand)]
    Verify(VerifyCommand),
    /// Report kernel health.
    Doctor,
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Submit a task spec.
    Submit {
        /// Path to a task spec (YAML or JSON).
        file: PathBuf,
        /// Leave the task in CREATED instead of admitting it to READY.
        ///
        /// Nothing will claim a held task. Useful to inspect what a spec produced before
        /// letting a worker act on it.
        #[arg(long)]
        hold: bool,
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
    /// Retry a failed or cancelled task.
    Retry { task_id: String },
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

#[derive(Subcommand)]
enum CapabilityCommand {
    /// List all registered capabilities.
    List {
        /// Directory containing capability manifests.
        #[arg(long, default_value = "capabilities")]
        path: PathBuf,
    },
    /// Inspect a specific capability by id.
    Inspect {
        /// The capability id to inspect.
        id: String,
        /// Directory containing capability manifests.
        #[arg(long, default_value = "capabilities")]
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ScriptCommand {
    /// Execute a script by capability id.
    Run {
        /// The capability id to execute.
        id: String,
        /// Optional JSON input payload.
        #[arg(long)]
        input: Option<String>,
        /// Directory containing capability manifests. Repeatable: pass it once per tree.
        #[arg(long, default_value = "capabilities")]
        capabilities_path: Vec<PathBuf>,
    },
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// Compile a workflow definition, reporting every problem found.
    Validate {
        /// Path to the workflow YAML file.
        file: PathBuf,
        /// Directory containing capability manifests. Repeatable: pass it once per tree.
        #[arg(long, default_value = "capabilities")]
        capabilities_path: Vec<PathBuf>,
    },
    /// Compile and execute a workflow.
    Run {
        /// Path to the workflow YAML file.
        file: PathBuf,
        /// Task id to record the run under. Defaults to a timestamped id.
        #[arg(long)]
        task_id: Option<String>,
        /// Directory containing capability manifests. Repeatable: pass it once per tree.
        #[arg(long, default_value = "capabilities")]
        capabilities_path: Vec<PathBuf>,
        /// Skip steps that already have a committed checkpoint.
        #[arg(long)]
        resume: bool,
    },
}

#[derive(Subcommand)]
enum VerifyCommand {
    /// Show what has been verified about a task.
    Task { task_id: String },
    /// Run a verifier or a schema check against a document.
    Run {
        /// Verifier capability id, or a path to a verifier script.
        #[arg(long)]
        verifier: Option<String>,
        /// JSON Schema name to validate against.
        #[arg(long)]
        schema: Option<String>,
        /// The document, inline.
        #[arg(long, conflicts_with = "input_file")]
        input: Option<String>,
        /// The document, from a file.
        #[arg(long)]
        input_file: Option<PathBuf>,
        /// Directory containing capability manifests. Repeatable: pass it once per tree.
        #[arg(long, default_value = "capabilities")]
        capabilities_path: Vec<PathBuf>,
        /// Directory containing JSON Schemas.
        #[arg(long, default_value = "schemas")]
        schemas_path: PathBuf,
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
        Command::Capability(cmd) => capability(cli, cmd),
        Command::Script(cmd) => script(cli, cmd),
        Command::Workflow(cmd) => workflow(cli, cmd),
        Command::Verify(cmd) => verify(cli, cmd),
        Command::Doctor => doctor(cli),
    }
}

fn task(cli: &Cli, cmd: &TaskCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        TaskCommand::Submit { file, hold } => {
            let hold = *hold;
            let source = std::fs::read_to_string(file)?;
            let parsed = TaskSpec::parse(&source)?;

            // A Constitution violation is not an ordinary error: it gets its own exit
            // code so CI can distinguish "the operator wrote an impossible task" from
            // "the disk is full".
            let submission = match parsed.into_submission() {
                Ok(s) => s,
                Err(SpecError::ConstitutionViolation { article, detail }) => {
                    eprintln!("Constitution Article {article}: {detail}");
                    return Ok(exit::CONSTITUTION_VIOLATION);
                }
                Err(e) => return Err(Box::new(e)),
            };

            let mut store = StateStore::open(&cli.db)?;
            let mut record = store.create_task(submission, SystemClock.now())?;

            // Straight through PLANNING and PLANNED to READY, exactly as the daemon does for
            // a scheduled occurrence and for the same reason: a spec-submitted task's plan
            // was declared in its spec, so there is nothing left to plan. The states are
            // still traversed because the machine forbids skipping them and the history
            // should show what happened.
            //
            // Without this a submitted task sat in CREATED, which no worker can claim, so
            // `task submit` produced something inert and the only way to run a task was to
            // schedule it. `--hold` keeps the old behaviour for anyone who wants to inspect
            // a task before it becomes claimable.
            if !hold {
                for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
                    store.transition(
                        &record.task_id,
                        state,
                        Some("submitted from a spec".into()),
                        None,
                        SystemClock.now(),
                    )?;
                }
                record = store
                    .get_task(&record.task_id)?
                    .expect("just transitioned it");
            }

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("submitted {} in state {}", record.task_id, record.state);
                println!("  trace_id: {}", record.trace_id);
                if hold {
                    println!("  held in {}; nothing will claim it", record.state);
                }
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

        TaskCommand::Retry { task_id } => {
            let mut store = StateStore::open(&cli.db)?;
            let id = TaskId::parse(task_id.clone())?;
            let task = store
                .get_task(&id)?
                .ok_or_else(|| format!("task '{task_id}' not found"))?;

            // Only failed, cancelled, or blocked tasks can be retried.
            let retriable = matches!(
                task.state,
                TaskState::Failed | TaskState::Cancelled | TaskState::Blocked
            );
            if !retriable {
                eprintln!(
                    "task '{}' is in state '{}' which cannot be retried",
                    task_id,
                    task.state.as_str()
                );
                return Ok(exit::ERROR);
            }

            let record = store.transition(
                &id,
                TaskState::Ready,
                Some("retried by operator".to_string()),
                None,
                SystemClock.now(),
            )?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("retried {} -> state={}", record.task_id, record.state);
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
    // One parser, shared with the registry. Two loaders with different ideas of what a manifest
    // file may contain is how `pearl constitution check` came to reject a directory the worker
    // loads happily: §57 puts workflows under `capabilities/`, and only the registry knew.
    if path.is_file() {
        let source = std::fs::read_to_string(path)?;
        return Ok(pearl_capabilities::parse_manifest_documents(&source, path)?);
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
        manifests.extend(
            pearl_capabilities::parse_manifest_documents(&source, &file)
                .map_err(|e| format!("{}: {e}", file.display()))?,
        );
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

fn capability(cli: &Cli, cmd: &CapabilityCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        CapabilityCommand::List { path } => {
            if !path.exists() {
                if cli.json {
                    println!("[]");
                } else {
                    println!("no capabilities directory found at '{}'", path.display());
                }
                return Ok(exit::OK);
            }
            let manifests = load_manifests(path)?;
            if cli.json {
                let items: Vec<_> = manifests
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id,
                            "version": m.version,
                            "type": m.capability_type.as_str(),
                            "runtime": m.execution.runtime.as_str(),
                            "deterministic": m.quality.deterministic,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if manifests.is_empty() {
                println!("no capabilities registered");
            } else {
                println!(
                    "{:<30} {:<10} {:<10} DETERMINISTIC",
                    "ID", "TYPE", "RUNTIME"
                );
                for m in &manifests {
                    println!(
                        "{:<30} {:<10} {:<10} {}",
                        m.id,
                        m.capability_type.as_str(),
                        m.execution.runtime.as_str(),
                        if m.quality.deterministic { "yes" } else { "no" }
                    );
                }
            }
            Ok(exit::OK)
        }

        CapabilityCommand::Inspect { id, path } => {
            if !path.exists() {
                eprintln!("capabilities directory '{}' not found", path.display());
                return Ok(exit::ERROR);
            }
            let manifests = load_manifests(path)?;
            let found = manifests.iter().find(|m| m.id == *id);
            match found {
                Some(manifest) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&manifest)?);
                    } else {
                        println!("id              {}", manifest.id);
                        println!("version         {}", manifest.version);
                        println!("type            {}", manifest.capability_type.as_str());
                        println!("runtime         {}", manifest.execution.runtime.as_str());
                        println!("deterministic   {}", manifest.quality.deterministic);
                        println!("side_effect     {}", manifest.risk.side_effect);
                        if let Some(timeout) = manifest.timeout_seconds {
                            println!("timeout         {}s", timeout);
                        }
                        if let Some(desc) = &manifest.description {
                            println!("description     {}", desc);
                        }
                    }
                    Ok(exit::OK)
                }
                None => {
                    eprintln!("capability '{}' not found", id);
                    Ok(exit::ERROR)
                }
            }
        }
    }
}

fn script(cli: &Cli, cmd: &ScriptCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        ScriptCommand::Run {
            id,
            input,
            capabilities_path,
        } => {
            let registry = CapabilityRegistry::load_directories(capabilities_path)?;
            let Some(capability) = registry.find_by_id(id) else {
                eprintln!(
                    "capability '{id}' is not in {}",
                    describe_paths(capabilities_path)
                );
                return Ok(exit::ERROR);
            };

            if !capability.manifest.runs_on_this_platform() {
                eprintln!("capability '{id}' does not declare support for this platform");
                return Ok(exit::ERROR);
            }

            let entrypoint = capability.resolve_entrypoint()?;
            let payload = match input {
                Some(text) => Some(serde_json::from_str::<serde_json::Value>(text)?),
                None => None,
            };
            let spec = ScriptSpec {
                runtime: capability.manifest.execution.runtime,
                entrypoint: entrypoint.target,
                args: entrypoint.args,
                env: Default::default(),
                cwd: None,
                timeout: TimeDelta::try_seconds(
                    capability.manifest.timeout_or(DEFAULT_TIMEOUT_SECONDS) as i64,
                )
                .unwrap_or_else(|| TimeDelta::try_seconds(60).expect("valid")),
                input_payload: payload,
            };

            // Really executed, under the same supervisor a worker would use. A dry run that
            // printed "would_execute" could not tell an operator whether the thing works,
            // which is the only reason to run one capability by hand.
            let result = match family_of(spec.runtime) {
                RuntimeFamily::Mechanical => {
                    ScriptRuntimeAdapter::new(PlatformSupervisor::default())
                        .execute(&spec, &SystemClock)?
                }
                RuntimeFamily::AgentCli(agent) => {
                    AgentCliAdapter::new(agent, PlatformSupervisor::default())
                        .execute(&spec, &SystemClock)?
                }
                RuntimeFamily::Api(provider) => {
                    ApiRuntimeAdapter::new(provider).execute(&spec, &SystemClock)?
                }
            };

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "capability_id": capability.manifest.id,
                        "runtime": spec.runtime.as_str(),
                        "exit_status": result.exit_status,
                        "duration_ms": result.duration.num_milliseconds(),
                        "output": result.structured_output,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                    }))?
                );
            } else {
                println!("capability   {}", capability.manifest.id);
                println!("runtime      {}", spec.runtime.as_str());
                println!("exit         {:?}", result.exit_status);
                println!("duration     {}ms", result.duration.num_milliseconds());
                if !result.stdout.trim().is_empty() {
                    println!("\n{}", result.stdout.trim());
                }
                if !result.stderr.trim().is_empty() {
                    eprintln!("{}", result.stderr.trim());
                }
            }

            // The capability's own verdict becomes the exit code, so a shell can act on it.
            Ok(if result.is_success() {
                exit::OK
            } else {
                exit::ERROR
            })
        }
    }
}

/// `pearl verify` — runs a verifier or a schema check by hand.
///
/// The operator counterpart to what the worker does automatically. Article 8 says only a
/// machine verifier may declare verification; this is how a human asks one, rather than
/// forming an opinion.
fn verify(cli: &Cli, cmd: &VerifyCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        VerifyCommand::Task { task_id } => {
            let store = StateStore::open(&cli.db)?;
            let id = TaskId::parse(task_id.clone())?;
            if store.get_task(&id)?.is_none() {
                eprintln!("task '{task_id}' not found");
                return Ok(exit::ERROR);
            }
            let results = store.verifications_for_task(&id)?;
            let evidence = store.evidence_for_task(&id)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": task_id,
                        "verifications": results,
                        "evidence": evidence,
                    }))?
                );
            } else if results.is_empty() {
                // Not the same as "verified": Article 2 makes the absence of a verdict a
                // reportable state rather than a silent pass.
                println!("no verification has been recorded for '{task_id}'");
            } else {
                for r in &results {
                    println!(
                        "{:<8} {:<40} {}",
                        if r.passed { "pass" } else { "fail" },
                        r.verifier_id,
                        r.detail.as_deref().unwrap_or("")
                    );
                }
                println!("\n{} evidence item(s)", evidence.len());
            }

            Ok(if results.iter().all(|r| r.passed) && !results.is_empty() {
                exit::OK
            } else {
                exit::ERROR
            })
        }

        VerifyCommand::Run {
            verifier,
            schema,
            input,
            input_file,
            capabilities_path,
            schemas_path,
        } => {
            let subject: serde_json::Value = match (input, input_file) {
                (Some(text), _) => serde_json::from_str(text)?,
                (None, Some(path)) => serde_json::from_str(&std::fs::read_to_string(path)?)?,
                (None, None) => {
                    eprintln!("give --input or --input-file: a verifier with nothing to inspect has nothing to say");
                    return Ok(exit::ERROR);
                }
            };

            let mut checks = Vec::new();
            if let Some(name) = schema {
                checks.push(AssuranceCheck::new(
                    format!("schema:{name}"),
                    CheckKind::SchemaValidation {
                        schema: name.clone(),
                    },
                    true,
                ));
            }
            if let Some(id) = verifier {
                // A capability id resolves through the registry; anything else is taken as a
                // path, so an ad-hoc script can be tried without registering it first.
                let path = CapabilityRegistry::load_directories(capabilities_path)
                    .ok()
                    .and_then(|registry| {
                        registry
                            .find_by_id(id)
                            .and_then(|cap| cap.resolve_entrypoint().ok())
                            .map(|resolved| resolved.target.to_string_lossy().to_string())
                    })
                    .unwrap_or_else(|| id.clone());
                checks.push(AssuranceCheck::new(
                    format!("verifier:{id}"),
                    CheckKind::ScriptVerifier { script_path: path },
                    true,
                ));
            }
            if checks.is_empty() {
                eprintln!("give --verifier or --schema");
                return Ok(exit::ERROR);
            }

            let context = CheckContext::new(subject.clone(), schemas_path)
                .with_verifier_input(serde_json::json!({ "result": subject }));
            let runner =
                RuntimeCheckRunner::new(PlatformSupervisor::default(), SystemClock, context);
            let result = AssuranceEngine::new(pearl_assurance::runner_fn(runner))
                .run(&AssuranceSpec::new(checks));

            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                for detail in &result.details {
                    println!(
                        "{:<8} {:<40} {}",
                        match &detail.outcome {
                            CheckOutcome::Passed => "pass",
                            CheckOutcome::Failed { .. } => "fail",
                            CheckOutcome::Errored { .. } => "error",
                        },
                        detail.name,
                        detail.outcome.reason().unwrap_or("")
                    );
                }
                println!("\n{}", result.summary());
            }

            // Three outcomes, three exit codes: a check that could not run is not a check
            // that failed, and a caller must be able to tell them apart (§26).
            Ok(if result.passed {
                exit::OK
            } else if result.errored_count() > 0 {
                exit::CONSTITUTION_VIOLATION
            } else {
                exit::ERROR
            })
        }
    }
}

fn workflow(cli: &Cli, cmd: &WorkflowCommand) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        WorkflowCommand::Validate {
            file,
            capabilities_path,
        } => {
            let (definition, compiled) = match compile(file, capabilities_path) {
                Ok(pair) => pair,
                Err(report) => {
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        eprintln!("workflow {} is not valid:", file.display());
                        for problem in report["problems"].as_array().into_iter().flatten() {
                            eprintln!("  {}", problem.as_str().unwrap_or_default());
                        }
                    }
                    return Ok(exit::ERROR);
                }
            };

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "file": file.display().to_string(),
                        "workflow": definition.name,
                        "steps": compiled.execution_order.len(),
                        "execution_order": compiled
                            .execution_order
                            .iter()
                            .map(|s| s.id.clone())
                            .collect::<Vec<_>>(),
                        "status": "valid",
                    }))?
                );
            } else {
                println!("workflow     {}", definition.name);
                println!("steps        {}", compiled.execution_order.len());
                println!(
                    "order        {}",
                    compiled
                        .execution_order
                        .iter()
                        .map(|s| s.id.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                );
                println!("status       valid");
            }
            Ok(exit::OK)
        }

        WorkflowCommand::Run {
            file,
            task_id,
            capabilities_path,
            resume,
        } => {
            let (definition, compiled) = match compile(file, capabilities_path) {
                Ok(pair) => pair,
                Err(report) => {
                    // Compilation is a gate, not advice: an invalid plan is never executed
                    // (§30), so a caller cannot run a workflow that failed to compile.
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        eprintln!(
                            "workflow {} did not compile; nothing was run",
                            file.display()
                        );
                        for problem in report["problems"].as_array().into_iter().flatten() {
                            eprintln!("  {}", problem.as_str().unwrap_or_default());
                        }
                    }
                    return Ok(exit::CONSTITUTION_VIOLATION);
                }
            };

            let registry = CapabilityRegistry::load_directories(capabilities_path)?;
            let mut store = StateStore::open(&cli.db)?;
            let now = SystemClock.now();

            // A workflow run is a task, so it gets the same durable record as any other work:
            // a run, checkpoints, and a history that says what happened.
            let id = TaskId::parse(task_id.clone().unwrap_or_else(|| {
                format!("{}-{}", definition.name, now.format("%Y%m%dt%H%M%Sz"))
            }))?;
            if store.get_task(&id)?.is_none() {
                store.create_task(
                    pearl_state::TaskSubmission::new(
                        id.clone(),
                        definition.name.clone(),
                        Some(pearl_core::PrecisionClass::P0),
                        pearl_core::QualitySpec::mechanical(),
                    ),
                    now,
                )?;
                for state in [TaskState::Planning, TaskState::Planned, TaskState::Ready] {
                    store.transition(&id, state, Some("workflow run".into()), None, now)?;
                }
            }

            // A task can only be claimed from READY. Rather than let the lease layer's error
            // surface as "not claimable", say what state it is actually in and what that means:
            // an interrupted run is resumable, a finished one is not.
            let state = store
                .get_task(&id)?
                .map(|t| t.state)
                .unwrap_or(TaskState::Ready);
            match state {
                TaskState::Ready => {}
                TaskState::Blocked | TaskState::RetryWait => {
                    store.transition(
                        &id,
                        TaskState::Ready,
                        Some("re-admitted for a workflow run".into()),
                        None,
                        SystemClock.now(),
                    )?;
                }
                other => {
                    eprintln!(
                        "task '{id}' is {other} and cannot be run again; give a new --task-id to run this workflow afresh"
                    );
                    if *resume {
                        let done = store.checkpoints_for_task(&id)?.len();
                        eprintln!("{done} step(s) already have a committed checkpoint");
                    }
                    return Ok(exit::ERROR);
                }
            }

            let leases = LeaseManager::new(LeaseConfig::default(), SystemClock);
            let lease = leases.claim(&mut store, &id, &WorkerId::new("cli:workflow"))?;
            store.transition(&id, TaskState::Running, None, None, SystemClock.now())?;
            let run = store.start_run(
                &id,
                &format!("workflow@{}", definition.name),
                &compiled_hash(&compiled),
                SystemClock.now(),
            )?;

            // Resume reads the committed checkpoints: §41 says only a committed checkpoint
            // licenses the next step, and this is the other half of that promise.
            let checkpoint = if *resume {
                resume_from(&store, &id)?
            } else {
                None
            };

            let runner =
                RuntimeStepExecutor::new(registry, PlatformSupervisor::default(), SystemClock)
                    .with_payload(serde_json::json!({
                        "task_id": id.as_str(),
                        "workflow": definition.name,
                    }));
            let executor = Executor::new(ExecutorConfig::default(), step_executor_fn(runner));
            let mut sink = StoreSink {
                store: &mut store,
                task_id: id.clone(),
                run_id: run.run_id,
                step_number: 0,
            };
            let result = executor.execute_with_sink(&compiled, checkpoint, &mut sink);

            let outcome = if result.success {
                pearl_events::RunOutcome::Success
            } else {
                pearl_events::RunOutcome::Failure
            };
            store.end_run(run.run_id, outcome, SystemClock.now())?;
            // VERIFYING, then a verdict: a workflow that merely finished has not been verified,
            // and this command does not pretend otherwise.
            store.transition(&id, TaskState::Verifying, None, None, SystemClock.now())?;
            store.transition(
                &id,
                if result.success {
                    // No assurance was declared for an ad-hoc workflow run, so the honest
                    // destination is UNVERIFIED even when every step succeeded (Article 2).
                    TaskState::Unverified
                } else {
                    TaskState::Failed
                },
                Some(format!(
                    "{} of {} step(s) succeeded",
                    result.success_count(),
                    result.records.len()
                )),
                None,
                SystemClock.now(),
            )?;
            leases.release(&mut store, lease.lease_id)?;

            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": id.as_str(),
                        "run_id": run.run_id.to_string(),
                        "success": result.success,
                        "resumed": result.resumed,
                        "steps": result.records,
                    }))?
                );
            } else {
                println!("task         {id}");
                println!("run          {}", run.run_id);
                if result.resumed {
                    println!("resumed      from a committed checkpoint");
                }
                for record in &result.records {
                    // A summary, not the whole outcome: a step's structured output can be
                    // kilobytes, and `--json` is there for whoever wants all of it.
                    let status = match &record.outcome {
                        pearl_executor::StepOutcome::Success { .. } => "ok",
                        pearl_executor::StepOutcome::Failed { .. } => "failed",
                        pearl_executor::StepOutcome::Skipped { .. } => "skipped",
                    };
                    println!(
                        "  {:<24} {:<8} {}",
                        record.step_id,
                        status,
                        record.outcome.summary()
                    );
                }
                println!(
                    "\n{} of {} step(s) succeeded",
                    result.success_count(),
                    result.records.len()
                );
            }

            Ok(if result.success {
                exit::OK
            } else {
                exit::ERROR
            })
        }
    }
}

/// Loads and compiles a workflow, with the registry as the capability set.
///
/// Compilation is where §30 is enforced, so this is deliberately the only path to a runnable
/// plan: passing the registry means a workflow naming a capability that does not exist fails
/// here rather than at the step that needed it.
type CompiledWorkflow = (
    pearl_workflow::WorkflowDef,
    pearl_plan_compiler::CompiledPlan,
);

/// Names the directories a lookup covered, for an error a reader can act on.
fn describe_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn compile(
    file: &Path,
    capabilities_path: &[PathBuf],
) -> Result<CompiledWorkflow, serde_json::Value> {
    let problem = |detail: String| {
        serde_json::json!({
            "file": file.display().to_string(),
            "status": "invalid",
            "problems": [detail],
        })
    };

    let source = std::fs::read_to_string(file)
        .map_err(|e| problem(format!("{} could not be read: {e}", file.display())))?;
    let definition =
        pearl_workflow::WorkflowDef::from_yaml(&source).map_err(|e| problem(e.to_string()))?;

    let known: std::collections::HashSet<String> =
        CapabilityRegistry::load_directories(capabilities_path)
            .map(|registry| registry.iter().map(|c| c.manifest.id.clone()).collect())
            .unwrap_or_default();
    // A step is verified when a `verify` step depends on it. Only verify steps count: taking
    // any dependency as verification would let one ordinary step "verify" another simply by
    // running after it.
    let verified: std::collections::HashSet<String> = definition
        .steps
        .iter()
        .filter(|s| s.step_type == pearl_workflow::StepType::Verify)
        .flat_map(|s| s.depends_on.clone())
        .collect();

    let engine = pearl_workflow::WorkflowEngine::with_config(pearl_plan_compiler::CompilerConfig {
        known_capabilities: known,
        verified_steps: verified,
        // A workflow file is compiled before anything runs, so no step has finished yet: every
        // reference in it must resolve within the file.
        completed_steps: Default::default(),
    });

    match engine.compile_workflow(&definition) {
        Ok(compiled) => Ok((definition, compiled)),
        Err(pearl_workflow::WorkflowError::CompileError { errors }) => Err(serde_json::json!({
            "file": file.display().to_string(),
            "status": "invalid",
            "problems": errors,
        })),
        Err(e) => Err(problem(e.to_string())),
    }
}

/// A stable digest of a compiled plan, recorded as the run's config hash (Article 10).
fn compiled_hash(compiled: &pearl_plan_compiler::CompiledPlan) -> String {
    use sha2::{Digest, Sha256};
    let joined = compiled
        .execution_order
        .iter()
        .map(|s| format!("{}:{}", s.id, s.capability))
        .collect::<Vec<_>>()
        .join("|");
    hex::encode(Sha256::digest(joined.as_bytes()))
}

/// Rebuilds an executor checkpoint from what was committed.
///
/// Restores the outputs as well as the step ids. §41 says a committed checkpoint licenses the
/// next step; when that step reads its predecessor's output, the output is part of what was
/// licensed. Restoring ids alone would resume in the right order and feed the work nothing,
/// which is the failure mode a crash-resume is supposed to prevent.
fn resume_from(
    store: &StateStore,
    task_id: &TaskId,
) -> Result<Option<Checkpoint>, Box<dyn std::error::Error>> {
    let committed = store.checkpoints_for_task(task_id)?;
    if committed.is_empty() {
        return Ok(None);
    }
    let mut checkpoint = Checkpoint::new();
    for record in committed {
        // A payload that will not parse is treated as an absent output rather than a fatal
        // error: the step still counts as done, and a successor that needed its output will
        // say precisely that instead of the resume failing wholesale.
        if let Some(output) = record
            .payload
            .as_deref()
            .and_then(|p| serde_json::from_str::<pearl_executor::StepOutput>(p).ok())
        {
            checkpoint.restore_output(record.label.clone(), output);
        }
        checkpoint.completed_steps.insert(record.label);
    }
    Ok(Some(checkpoint))
}

/// Commits each step's completion to the store before the next one starts — §41.
struct StoreSink<'a> {
    store: &'a mut StateStore,
    task_id: TaskId,
    run_id: pearl_core::RunId,
    step_number: u32,
}

impl CheckpointSink for StoreSink<'_> {
    fn commit(
        &mut self,
        record: &pearl_executor::StepRecord,
        _checkpoint: &Checkpoint,
    ) -> Result<(), String> {
        self.step_number += 1;
        let status = match &record.outcome {
            pearl_executor::StepOutcome::Success { .. } => "success",
            pearl_executor::StepOutcome::Failed { .. } => "failed",
            pearl_executor::StepOutcome::Skipped { .. } => "skipped",
        };
        self.store
            .record_step(
                &pearl_state::StepRecord::new(
                    self.run_id,
                    self.step_number,
                    &record.step_id,
                    // One line for a human reading the projection. The full output lives in
                    // the checkpoint payload, where resume can find it.
                    record.outcome.summary(),
                    status,
                )
                .started(record.started_at)
                .completed(record.completed_at),
            )
            .map_err(|e| e.to_string())?;

        // Only a successful step is a resume point. Committing a failed one would make resume
        // skip the step that needs redoing.
        if let Some(output) = record.outcome.output() {
            let payload = serde_json::to_string(&output).map_err(|e| e.to_string())?;
            self.store
                .commit_checkpoint(
                    &self.task_id,
                    &record.step_id,
                    Some(&payload),
                    record.completed_at,
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

fn doctor(cli: &Cli) -> Result<u8, Box<dyn std::error::Error>> {
    let store = StateStore::open(&cli.db)?;
    let now = SystemClock.now();

    let ledger_events = store.ledger().count()?;
    // Which schema a database is on is the first question when a query fails on a machine you
    // cannot see, so doctor answers it before anything else.
    let ledger_schema = store.ledger().schema_version()?;
    let projection_schema = store.schema_version()?;
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
                "schema": {
                    "ledger": ledger_schema,
                    "projections": projection_schema,
                },
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
        println!("schema         ledger v{ledger_schema}, projections v{projection_schema}");
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
