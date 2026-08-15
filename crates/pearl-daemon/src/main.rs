//! The `pearl-daemon` executable — §57.
//!
//! Runs the loops that keep the system moving, and nothing else: it never executes a task.
//! That separation means a scheduler bug cannot take execution down, and a worker crash cannot
//! stop schedules from firing.
//!
//! Also the operator surface for schedules, because a schedule that can only be registered
//! programmatically is a schedule nobody registers.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clap::{Parser, Subcommand};
use pearl_core::{Clock, SystemClock};
use pearl_daemon::{Daemon, DaemonConfig};
use pearl_state::{ScheduleRecord, StateStore, TaskSpec};

mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
}

#[derive(Parser)]
#[command(
    name = "pearl-daemon",
    about = "Fires schedules, reclaims leases, promotes retries, runs the governance loop.",
    version
)]
struct Cli {
    /// Path to the PEARL database.
    #[arg(long, default_value = "pearl.db", global = true)]
    db: PathBuf,

    /// Emit machine JSON on stdout.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,

    /// Seconds between scheduler ticks.
    #[arg(long, default_value_t = 10)]
    scheduler_interval: u64,

    /// Seconds between lease reaper passes.
    #[arg(long, default_value_t = 60)]
    reaper_interval: u64,

    /// Seconds between governance cycles.
    #[arg(long, default_value_t = 30)]
    ooda_interval: u64,

    /// Directory that relative spec paths are resolved from.
    #[arg(long)]
    working_dir: Option<PathBuf>,

    /// Run one pass of every loop and exit.
    ///
    /// What a cron job or a test wants: the loops are idempotent, so a single pass is a
    /// complete unit of work.
    #[arg(long)]
    once: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Schedule operations.
    #[command(subcommand)]
    Schedule(ScheduleCommand),
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// Register or update a schedule.
    Add {
        /// Schedule id, also the prefix of every task it submits.
        id: String,
        /// Path to the task spec each occurrence is submitted from.
        spec: PathBuf,
        /// 5-field cron expression, e.g. "0 7 * * *".
        #[arg(long, conflicts_with = "every")]
        cron: Option<String>,
        /// Interval in seconds.
        #[arg(long, conflicts_with = "cron")]
        every: Option<u64>,
        /// Misfire policy: skip, run_once or run_all.
        #[arg(long, default_value = "skip")]
        misfire: String,
    },
    /// List registered schedules.
    List,
    /// Enable a schedule.
    Enable { id: String },
    /// Disable a schedule without forgetting it.
    Disable { id: String },
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

    // stderr always: stdout carries machine output (§26).
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
    let mut store = StateStore::open(&cli.db)?;

    if let Some(Command::Schedule(cmd)) = &cli.command {
        return schedule(cli, &mut store, cmd);
    }

    let config = DaemonConfig {
        ooda_interval: Duration::from_secs(cli.ooda_interval),
        scheduler_interval: Duration::from_secs(cli.scheduler_interval),
        reaper_interval: Duration::from_secs(cli.reaper_interval),
        tick: Duration::from_secs(1),
        working_dir: cli.working_dir.clone(),
        ..DaemonConfig::default()
    };
    let mut daemon = Daemon::new(config, SystemClock);

    if cli.once {
        let report = daemon.tick_all(&mut store)?;
        if cli.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "triggered": report.triggered,
                    "reclaimed": report.reclaimed,
                    "promoted": report.promoted,
                    "failed": report.failed,
                }))?
            );
        } else {
            println!(
                "triggered {}, reclaimed {}, promoted {}, failed {}",
                report.triggered.len(),
                report.reclaimed,
                report.promoted,
                report.failed.len()
            );
            for (schedule, task) in &report.triggered {
                println!("  {schedule} -> {task}");
            }
            for (schedule, reason) in &report.failed {
                println!("  {schedule}: {reason}");
            }
        }
        return Ok(if report.failed.is_empty() {
            exit::OK
        } else {
            exit::ERROR
        });
    }

    let stop = daemon.stop_handle();
    // Cooperative: the loops finish their pass. A hard stop is a signal away, and the reaper
    // exists precisely to recover from that.
    let handler = stop.clone();
    if let Err(e) = ctrl_c(move || handler.store(true, Ordering::Relaxed)) {
        tracing::warn!(error = %e, "no Ctrl+C handler installed; stop with a signal");
    }

    let report = daemon.run(&mut store)?;
    println!(
        "daemon stopped after {} governance cycle(s), {}s uptime",
        report.cycles_completed,
        report.uptime.num_seconds()
    );
    Ok(exit::OK)
}

/// Installs a Ctrl+C handler without taking a dependency for it.
///
/// The standard library has no portable hook, so this is a best-effort no-op: the daemon is
/// stoppable by signal, and an unclean stop is recoverable by design (§34).
fn ctrl_c(_handler: impl FnMut() + Send + 'static) -> Result<(), String> {
    Err("not supported without a signal-handling dependency".to_string())
}

fn schedule(
    cli: &Cli,
    store: &mut StateStore,
    cmd: &ScheduleCommand,
) -> Result<u8, Box<dyn std::error::Error>> {
    match cmd {
        ScheduleCommand::Add {
            id,
            spec,
            cron,
            every,
            misfire,
        } => {
            // The spec is parsed now rather than at first fire: a schedule pointing at an
            // invalid spec should fail when it is registered, not at 07:00 in three weeks.
            let parsed = TaskSpec::load(spec)?;
            let task_type = parsed.task_type.clone();
            parsed.into_submission()?;

            let now = SystemClock.now();
            let record = match (cron, every) {
                (Some(expression), _) => {
                    ScheduleRecord::cron(id, &task_type, spec.to_string_lossy(), expression, now)
                }
                (None, Some(secs)) => {
                    ScheduleRecord::interval(id, &task_type, spec.to_string_lossy(), *secs, now)
                }
                (None, None) => {
                    return Err(
                        "give either --cron or --every, otherwise the schedule can never fire"
                            .into(),
                    )
                }
            }
            .with_misfire(misfire);

            store.upsert_schedule(&record)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!("registered schedule '{id}' for {}", spec.display());
            }
            Ok(exit::OK)
        }

        ScheduleCommand::List => {
            let schedules = store.list_schedules()?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&schedules)?);
            } else if schedules.is_empty() {
                println!("no schedules registered");
            } else {
                for s in &schedules {
                    let trigger = s
                        .cron_expr
                        .clone()
                        .or_else(|| s.interval_secs.map(|v| format!("every {v}s")))
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<24} {:<16} {:<20} {}",
                        s.schedule_id,
                        if s.enabled { "enabled" } else { "disabled" },
                        trigger,
                        s.spec_path
                    );
                }
            }
            Ok(exit::OK)
        }

        ScheduleCommand::Enable { id } => {
            set_enabled(cli, store, id, true)?;
            Ok(exit::OK)
        }
        ScheduleCommand::Disable { id } => {
            set_enabled(cli, store, id, false)?;
            Ok(exit::OK)
        }
    }
}

fn set_enabled(
    cli: &Cli,
    store: &mut StateStore,
    id: &str,
    enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if store.get_schedule(id)?.is_none() {
        return Err(format!("no schedule '{id}'").into());
    }
    store.set_schedule_enabled(id, enabled)?;
    if cli.json {
        println!(
            "{}",
            serde_json::json!({ "schedule_id": id, "enabled": enabled })
        );
    } else {
        println!(
            "schedule '{id}' {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }
    Ok(())
}
