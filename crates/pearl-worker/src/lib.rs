//! # pearl-worker
//!
//! The component that actually does the work — §69, §70.
//!
//! Everything else in PEARL decides *what* should happen; this is where it happens. One
//! task at a time, the worker walks the path the Constitution requires and refuses to skip
//! a step:
//!
//! ```text
//! claim a lease            §34  nobody else may run this task
//!   → permit               §45  is this capability allowed to run at all
//!   → route                Art 1  mechanical if mechanical is possible
//!   → platform check       §39  the manifest says it runs here
//!   → open run + attempt   Art 10  with config_revision and config_hash
//!   → execute              Art 9  under a supervisor that can cancel it
//!   → verify               Art 2, 8  mechanically, or not at all
//!   → evidence             Art 4  success is a claim with something behind it
//!   → transition           the state machine decides what the outcome is called
//! ```
//!
//! Three properties are worth naming because they are easy to get wrong and expensive to
//! discover later:
//!
//! **The lease outlives the work.** A worker that executes synchronously cannot send
//! heartbeats mid-flight, so a fixed 60-second lease would expire during a five-minute
//! script and the reaper would hand the task to a second worker while the first was still
//! running it. The claim is therefore taken for at least twice the capability's timeout,
//! and the timeout is enforced by the supervisor — so the work can never outlive the lease.
//!
//! **A verifier that could not run is not a failure.** It is the absence of a verdict, and
//! Article 2 says the honest destination for that is `UNVERIFIED`, which an operator can
//! resolve, rather than `FAILED`, which invites a retry that will fail the same way.
//!
//! **Nothing reaches `VERIFIED_SUCCESS` without machine evidence.** The state store
//! enforces this independently (Gate 3), so a bug here produces a refused transition rather
//! than an unproven success.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use pearl_assurance::{
    AssuranceCheck, AssuranceEngine, AssuranceResult, AssuranceSpec, CheckContext, CheckKind,
    CheckOutcome, RuntimeCheckRunner,
};
use pearl_capabilities::{CapabilityRegistry, RegisteredCapability, RegistryError};
use pearl_core::{
    AssuranceStep, Clock, Evidence, EvidenceResult, EvidenceSet, EvidenceType, PrecisionClass,
    ResolvedConfig, RuntimeProfile, TaskPlan, TaskState, WorkerId,
};
use pearl_events::{EventEnvelope, PearlEvent, RunOutcome};
use pearl_governance::manifest::Runtime;
use pearl_lease::{LeaseConfig, LeaseError, LeaseManager};
use pearl_policy::{PermissionDecision, Permissions};
use pearl_process_supervisor::PlatformSupervisor;
use pearl_queue::{QueueError, RetryPolicy, WorkQueue};
use pearl_router::{Router, RoutingDecision, TaskRequirements};
use pearl_runtime::{
    family_of, AgentCliAdapter, ApiRuntimeAdapter, RuntimeAdapter, RuntimeFamily, RuntimeResult,
    ScriptRuntimeAdapter, ScriptSpec,
};
use pearl_state::{StateError, StateStore, TaskRecord};
use serde::{Deserialize, Serialize};

mod outcome;
pub use outcome::{Verdict, WorkResult};

/// Fallback ceiling for a capability whose manifest omits one.
///
/// The Constitution gate rejects such manifests, so this only applies to capabilities
/// registered programmatically. It is deliberately short: an unbounded default would let a
/// hung script hold a worker indefinitely, which is the failure Article 9 exists to prevent.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

/// How a worker is configured.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Unique identifier for this worker, recorded on every lease and event.
    pub worker_id: WorkerId,
    /// How long to wait between polls when the queue is empty.
    pub poll_interval: Duration,
    /// Directories of capability manifests, loaded into one registry.
    ///
    /// A list because an application's capabilities and the framework's shared verifiers and
    /// effects are separate trees. One directory forced either duplicating the shared ones
    /// into every application or flattening every application into one directory.
    pub capability_dirs: Vec<PathBuf>,
    /// Directory of JSON Schemas, for schema assurance steps.
    pub schema_dir: PathBuf,
    /// Capability permission rules. Absent means "permit nothing".
    pub permissions_path: Option<PathBuf>,
    /// Working directory handed to spawned scripts.
    pub working_dir: Option<PathBuf>,
    /// The active runtime profile (§48).
    pub profile: RuntimeProfile,
    /// Retry policy for failed attempts.
    pub retry_policy: RetryPolicy,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: WorkerId::new("worker:local"),
            poll_interval: Duration::from_millis(500),
            capability_dirs: vec![PathBuf::from("capabilities")],
            schema_dir: PathBuf::from("schemas"),
            permissions_path: Some(PathBuf::from("policies/permissions.yaml")),
            working_dir: None,
            profile: RuntimeProfile::Normal,
            retry_policy: RetryPolicy::default(),
        }
    }
}

/// Why a task could not be run at all, as opposed to having run and failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Refusal {
    /// No capability in the registry can perform this work.
    NoCapability { detail: String },
    /// The permission rules do not admit this capability.
    NotPermitted { capability: String, detail: String },
    /// The capability does not run on this platform.
    WrongPlatform { capability: String },
    /// The capability declares no runnable entrypoint.
    NotRunnable { capability: String, detail: String },
    /// The runtime the capability asks for is not available here.
    NoRuntime { capability: String, runtime: String },
    /// The profile forbids this work right now.
    ProfileForbids { detail: String },
}

impl Refusal {
    /// A single sentence for the task's `last_reason`.
    pub fn detail(&self) -> String {
        match self {
            Refusal::NoCapability { detail } => detail.clone(),
            Refusal::NotPermitted { capability, detail } => {
                format!("capability '{capability}' is not permitted: {detail}")
            }
            Refusal::WrongPlatform { capability } => {
                format!("capability '{capability}' does not declare support for this platform")
            }
            Refusal::NotRunnable { capability, detail } => {
                format!("capability '{capability}' cannot be executed: {detail}")
            }
            Refusal::NoRuntime {
                capability,
                runtime,
            } => format!(
                "capability '{capability}' needs runtime '{runtime}', which is not configured here"
            ),
            Refusal::ProfileForbids { detail } => detail.clone(),
        }
    }
}

/// Worker failures.
///
/// These are infrastructure faults. A task that runs and fails is not an error here — it is
/// a [`Verdict`], which is a normal return value.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("failed to load capabilities from {path}: {source}")]
    Registry {
        path: PathBuf,
        #[source]
        source: RegistryError,
    },
    #[error("failed to load permissions from {path}: {detail}")]
    Permissions { path: PathBuf, detail: String },
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error(transparent)]
    Queue(#[from] QueueError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Ledger(#[from] pearl_events::LedgerError),
}

/// A worker: claims tasks and sees them through to a recorded outcome.
///
/// The clock is `Send + Sync + 'static` because assurance checks are run through a boxed
/// runner that outlives the call, and because a worker is the kind of thing that ends up on
/// its own thread.
pub struct Worker<C: Clock + Clone + Send + Sync + 'static> {
    config: WorkerConfig,
    clock: C,
    registry: CapabilityRegistry,
    permissions: Permissions,
    router: Router,
    resolved_config: ResolvedConfig,
}

impl<C: Clock + Clone + Send + Sync + 'static> Worker<C> {
    /// Builds a worker, loading its registry and permissions up front.
    ///
    /// Loading eagerly means a misconfigured worker fails at start-up rather than on the
    /// first task, when a lease is already held.
    pub fn new(config: WorkerConfig, clock: C) -> Result<Self, WorkerError> {
        let registry =
            CapabilityRegistry::load_directories(&config.capability_dirs).map_err(|e| {
                WorkerError::Registry {
                    path: config
                        .capability_dirs
                        .first()
                        .cloned()
                        .unwrap_or_else(|| PathBuf::from("<none>")),
                    source: e,
                }
            })?;

        let permissions = match &config.permissions_path {
            Some(path) => Permissions::load(path).map_err(|e| WorkerError::Permissions {
                path: path.clone(),
                detail: e.to_string(),
            })?,
            // No permission file is not "allow everything": §45 makes this an allow-list,
            // and an absent list admits nothing.
            None => Permissions::deny_all(),
        };

        let resolved_config = pearl_core::ConfigResolver::new()
            .with_source(
                pearl_core::ConfigSource::new(pearl_core::Layer::System, "worker-builtin")
                    .set("poll_interval_ms", config.poll_interval.as_millis() as i64)
                    .set("max_attempts", config.retry_policy.max_attempts as i64),
            )
            .with_source(
                pearl_core::ConfigSource::new(pearl_core::Layer::Profile, config.profile.as_str())
                    .set("concurrency_cap", config.profile.concurrency_cap() as i64)
                    .set("side_effects", config.profile.allows_side_effects()),
            )
            .resolve();

        Ok(Self {
            config,
            clock,
            registry,
            permissions,
            router: Router::new(),
            resolved_config,
        })
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.config.worker_id
    }

    pub fn registry(&self) -> &CapabilityRegistry {
        &self.registry
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    /// The configuration provenance recorded on every run (Article 10).
    pub fn resolved_config(&self) -> &ResolvedConfig {
        &self.resolved_config
    }

    /// Claims one task and runs it to a recorded outcome.
    ///
    /// Returns `Ok(None)` when the queue is empty.
    pub fn run_once(&self, store: &mut StateStore) -> Result<Option<WorkResult>, WorkerError> {
        let queue = WorkQueue::new(
            self.config.retry_policy,
            self.config.profile,
            self.clock.clone(),
        );

        // The claim is deliberately short-lived: it is immediately re-taken for the length
        // the chosen capability needs, once that capability is known.
        let leases = LeaseManager::new(LeaseConfig::default(), self.clock.clone());
        let Some(claim) = queue.claim_next(store, &leases, &self.config.worker_id)? else {
            return Ok(None);
        };

        let result = self.execute_claim(store, &queue, claim.task, claim.lease.lease_id);
        Ok(Some(result?))
    }

    /// Runs until `stop` is set, sleeping when there is nothing to do.
    pub fn run_until(
        &self,
        store: &mut StateStore,
        stop: &AtomicBool,
    ) -> Result<Vec<WorkResult>, WorkerError> {
        let mut results = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match self.run_once(store)? {
                Some(result) => results.push(result),
                None => {
                    // Promote anything whose backoff elapsed before sleeping, so a retry
                    // does not wait for an unrelated task to arrive.
                    let queue = WorkQueue::new(
                        self.config.retry_policy,
                        self.config.profile,
                        self.clock.clone(),
                    );
                    queue.promote_ready_retries(store)?;
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(self.config.poll_interval);
                }
            }
        }
        Ok(results)
    }

    /// The full pipeline for one claimed task.
    fn execute_claim(
        &self,
        store: &mut StateStore,
        queue: &WorkQueue<C>,
        task: TaskRecord,
        lease_id: pearl_core::LeaseId,
    ) -> Result<WorkResult, WorkerError> {
        let started_at = self.clock.now();
        let task_id = task.task_id.clone();

        // --- decide what to run, before opening a run ---
        let selection = match self.select_capability(&task) {
            Ok(cap) => cap,
            Err(refusal) => {
                return self.refuse(store, &task, lease_id, refusal, started_at);
            }
        };
        let capability_id = selection.capability_id.clone();
        let timeout = selection.timeout;

        // Ask the runtime whether it could run this, before anything is opened. A missing
        // credential or an unfillable prompt is knowable without executing, and discovering it
        // after `start_run` would leave a run recorded for work that never began — and, for a
        // paid endpoint, would be the wrong moment to find out.
        if let Err(refusal) = self.preflight(&selection, &task) {
            return self.refuse(store, &task, lease_id, refusal, started_at);
        }

        // Re-take the claim for long enough to cover the work. See the module note: a
        // synchronous worker cannot heartbeat, so the lease has to be sized up front.
        let leases = LeaseManager::new(lease_config_for(timeout), self.clock.clone());
        leases.heartbeat(store, lease_id)?;

        // --- open the run (Article 10) ---
        let run = store.start_run(
            &task_id,
            &self.resolved_config.config_revision,
            &self.resolved_config.config_hash,
            self.clock.now(),
        )?;
        store.transition(
            &task_id,
            TaskState::Running,
            Some(format!("executing {capability_id}")),
            None,
            self.clock.now(),
        )?;
        let attempt = store.start_attempt(run.run_id, task.attempt_count + 1, self.clock.now())?;

        // --- execute (Article 9) ---
        let mechanical = selection.runtime.is_mechanical();
        self.record(
            store,
            &task,
            // Article 1 makes this distinction the most important fact about an execution,
            // and §71 measures the ratio, so the ledger records which side it was on.
            if mechanical {
                PearlEvent::ScriptStarted {
                    task_id: Some(task_id.clone()),
                    capability_id: capability_id.clone(),
                    runtime: selection.runtime.as_str().to_string(),
                }
            } else {
                PearlEvent::AgentStarted {
                    task_id: Some(task_id.clone()),
                    capability_id: capability_id.clone(),
                    runtime: selection.runtime.as_str().to_string(),
                    model: None,
                }
            },
        )?;
        store.record_step(
            &pearl_state::StepRecord::new(
                run.run_id,
                1,
                &capability_id,
                format!("execute {capability_id}"),
                "running",
            )
            .started(started_at),
        )?;

        let execution = self.execute_capability(&selection, &task);
        let elapsed = self.clock.now() - started_at;

        match &execution {
            Ok(result) => {
                // -1 for a process that never reported a code of its own: killed, signalled
                // or timed out. The event records that it did not exit normally rather than
                // inventing a plausible code.
                let exit_code = exit_code_of(result).unwrap_or(-1);
                let duration_ms = result.duration.num_milliseconds().max(0) as u64;
                self.record(
                    store,
                    &task,
                    if mechanical {
                        PearlEvent::ScriptCompleted {
                            task_id: Some(task_id.clone()),
                            capability_id: capability_id.clone(),
                            exit_code,
                            duration_ms,
                        }
                    } else {
                        PearlEvent::AgentCompleted {
                            task_id: Some(task_id.clone()),
                            capability_id: capability_id.clone(),
                            exit_code,
                            duration_ms,
                            tokens: token_usage(result),
                        }
                    },
                )?;
                store.record_step(
                    &pearl_state::StepRecord::new(
                        run.run_id,
                        1,
                        &capability_id,
                        format!("execute {capability_id}"),
                        if result.is_success() {
                            "success"
                        } else {
                            "failed"
                        },
                    )
                    .started(started_at)
                    .completed(self.clock.now()),
                )?;
            }
            Err(refusal) => {
                // The capability could not be started at all. That is a refusal, not a
                // failed attempt: nothing ran, so there is nothing to retry differently.
                store.end_attempt(
                    attempt.attempt_id,
                    RunOutcome::Failure,
                    Some(refusal.detail()),
                    self.clock.now(),
                )?;
                store.end_run(run.run_id, RunOutcome::Failure, self.clock.now())?;
                return self.refuse(store, &task, lease_id, refusal.clone(), started_at);
            }
        }

        let result = execution.expect("checked above");

        // --- verify (Articles 2, 8) ---
        store.transition(
            &task_id,
            TaskState::Verifying,
            Some(format!("verifying {capability_id}")),
            None,
            self.clock.now(),
        )?;

        let subject = verification_subject(&result);
        let envelope = verification_envelope(&task, &capability_id, &result, &subject);
        let spec = self.assurance_spec(&task.plan, &selection);
        let assurance = self.verify(&spec, subject.clone(), envelope, timeout);

        for (index, check) in assurance.details.iter().enumerate() {
            self.record(
                store,
                &task,
                PearlEvent::VerificationStarted {
                    task_id: task_id.clone(),
                    verifier_id: check.name.clone(),
                },
            )?;
            let event = match &check.outcome {
                CheckOutcome::Passed => PearlEvent::VerificationPassed {
                    task_id: task_id.clone(),
                    verifier_id: check.name.clone(),
                    check_count: 1,
                },
                CheckOutcome::Failed { reason } | CheckOutcome::Errored { reason } => {
                    PearlEvent::VerificationFailed {
                        task_id: task_id.clone(),
                        verifier_id: check.name.clone(),
                        reason: reason.clone(),
                    }
                }
            };
            self.record(store, &task, event)?;

            // The verdict is queryable, not only replayable: "what verified this task?" is
            // the question an audit asks, and answering it should not require a replay.
            store.record_verification(
                &task_id,
                &check.name,
                check.outcome.passed(),
                check.outcome.reason(),
                self.clock.now(),
            )?;
            store.record_step(
                &pearl_state::StepRecord::new(
                    run.run_id,
                    (index + 2) as u32,
                    &check.name,
                    format!("verify {}", check.name),
                    match &check.outcome {
                        CheckOutcome::Passed => "success",
                        CheckOutcome::Failed { .. } => "failed",
                        // Skipped rather than failed: the check never reached a verdict.
                        CheckOutcome::Errored { .. } => "skipped",
                    },
                )
                .started(started_at)
                .completed(self.clock.now()),
            )?;
        }

        // Artifacts the capability declared it produced (§44). Recorded only when the file
        // exists and its digest can be taken: an index entry pointing at nothing would be
        // worse than no entry.
        for artifact in declared_artifacts(
            &result,
            &task_id,
            self.clock.now(),
            &self.config.working_dir,
        ) {
            store.record_artifact(&artifact)?;
        }

        // --- evidence (Article 4) ---
        let evidence = build_evidence(&capability_id, &result, &assurance, self.clock.now());

        // --- decide what the outcome is called ---
        let verdict = self.decide(&task, &result, &spec, &assurance);
        let work_result = WorkResult {
            task_id: task_id.clone(),
            capability_id: capability_id.clone(),
            verdict: verdict.clone(),
            assurance: assurance.clone(),
            exit_code: exit_code_of(&result),
            structured_output: result.structured_output.clone(),
            started_at,
            completed_at: self.clock.now(),
            duration_ms: elapsed.num_milliseconds().max(0) as u64,
        };

        let run_outcome = match &verdict {
            Verdict::Verified => RunOutcome::Success,
            Verdict::Unverified { .. } => RunOutcome::Success,
            Verdict::Failed { .. } => RunOutcome::Failure,
            Verdict::TimedOut => RunOutcome::Timeout,
            Verdict::Refused { .. } => RunOutcome::Failure,
        };

        store.end_attempt(
            attempt.attempt_id,
            run_outcome,
            verdict.reason(),
            self.clock.now(),
        )?;

        match &verdict {
            Verdict::Verified => {
                store.transition(
                    &task_id,
                    TaskState::VerifiedSuccess,
                    Some(assurance.summary()),
                    Some(&evidence),
                    self.clock.now(),
                )?;
            }
            Verdict::Unverified { reason } => {
                // Article 2, case B: exactness was demanded and nothing could establish it.
                // UNVERIFIED is resolvable — a verifier can be written, or a human can
                // approve — whereas FAILED would invite a retry that changes nothing.
                store.transition(
                    &task_id,
                    TaskState::Unverified,
                    Some(reason.clone()),
                    None,
                    self.clock.now(),
                )?;
            }
            Verdict::Failed { reason } | Verdict::Refused { reason } => {
                queue.record_failure(store, &task_id, reason)?;
            }
            Verdict::TimedOut => {
                queue.record_failure(
                    store,
                    &task_id,
                    &format!(
                        "{capability_id} exceeded its {}s timeout",
                        timeout.num_seconds()
                    ),
                )?;
            }
        }

        store.end_run(run.run_id, run_outcome, self.clock.now())?;
        leases.release(store, lease_id)?;

        Ok(work_result)
    }

    /// Chooses the capability to run, or explains why none can be.
    fn select_capability(&self, task: &TaskRecord) -> Result<Selection, Refusal> {
        let capability = self.find_capability(task)?;

        // Permission is checked after selection and before anything is opened: routing is a
        // pure lookup, so asking first would mean asking about a capability we might not use.
        let decision = self.permissions.evaluate(&capability.manifest.id);
        if !decision.is_allowed() {
            return Err(Refusal::NotPermitted {
                capability: capability.manifest.id.clone(),
                detail: match &decision {
                    PermissionDecision::Denied { rule } => format!("denied by rule '{rule}'"),
                    _ => decision.reason(),
                },
            });
        }

        if !capability.manifest.runs_on_this_platform() {
            return Err(Refusal::WrongPlatform {
                capability: capability.manifest.id.clone(),
            });
        }

        if capability.manifest.risk.side_effect && !self.config.profile.allows_side_effects() {
            return Err(Refusal::ProfileForbids {
                detail: format!(
                    "capability '{}' has side effects, which profile '{}' forbids",
                    capability.manifest.id,
                    self.config.profile.as_str()
                ),
            });
        }

        let entrypoint = capability
            .resolve_entrypoint()
            .map_err(|e| Refusal::NotRunnable {
                capability: capability.manifest.id.clone(),
                detail: e.to_string(),
            })?;

        // Task timeout wins over the capability's, because the task knows what it asked for.
        let seconds = task
            .plan
            .timeout_seconds
            .unwrap_or_else(|| capability.manifest.timeout_or(DEFAULT_TIMEOUT_SECONDS));

        Ok(Selection {
            capability_id: capability.manifest.id.clone(),
            runtime: capability.manifest.execution.runtime,
            entrypoint: entrypoint.target,
            args: entrypoint.args,
            output_schema: capability.manifest.schemas.output.clone(),
            timeout: TimeDelta::try_seconds(seconds as i64)
                .unwrap_or_else(|| TimeDelta::try_seconds(DEFAULT_TIMEOUT_SECONDS as i64).unwrap()),
        })
    }

    /// Finds the capability a task should run.
    ///
    /// A named capability is used verbatim. Only an unnamed task falls back to the router,
    /// whose matching is a heuristic; naming the capability turns dispatch into a lookup.
    fn find_capability(&self, task: &TaskRecord) -> Result<&RegisteredCapability, Refusal> {
        if let Some(named) = &task.plan.capability {
            return self
                .registry
                .find_by_id(named)
                .ok_or_else(|| Refusal::NoCapability {
                    detail: format!(
                        "task names capability '{named}', which is not in {}",
                        self.config
                            .capability_dirs
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
        }

        let requirements = TaskRequirements {
            task_type: task.task_type.clone(),
            required_capabilities: Vec::new(),
            quality_spec: task.quality,
            precision_override: task.precision_class,
        };

        match self.router.route(&requirements, &self.registry) {
            RoutingDecision::ScriptRoute { capability_id, .. } => self
                .registry
                .find_by_id(&capability_id)
                .ok_or_else(|| Refusal::NoCapability {
                    detail: format!(
                        "router chose '{capability_id}', which vanished from the registry"
                    ),
                }),
            RoutingDecision::AgentRoute { precision, reason } => {
                // Article 1 has been satisfied: no mechanical capability exists, so an agent
                // is permitted. Look for one that names this task type before giving up,
                // otherwise a perfectly configured agent capability would never be reachable
                // except by a task naming it explicitly.
                self.find_agent_capability(&task.task_type)
                    .ok_or_else(|| Refusal::NoCapability {
                        detail: format!(
                            "no capability for task_type '{}' at {}: {reason}",
                            task.task_type,
                            precision.as_str()
                        ),
                    })
            }
            RoutingDecision::Rejected { reason } => Err(Refusal::NoCapability { detail: reason }),
        }
    }

    /// An agent capability whose id mentions this task type.
    ///
    /// Exact match first, then a prefixed convention (`agent.<task_type>`), then any agent
    /// capability mentioning it. Ordered from most to least specific so a registry with
    /// several agents does not resolve by accident.
    fn find_agent_capability(&self, task_type: &str) -> Option<&RegisteredCapability> {
        let agents: Vec<&RegisteredCapability> = self
            .registry
            .iter()
            .filter(|c| !c.manifest.execution.runtime.is_mechanical())
            .collect();

        agents
            .iter()
            .find(|c| c.manifest.id == task_type)
            .or_else(|| {
                agents
                    .iter()
                    .find(|c| c.manifest.id == format!("agent.{task_type}"))
            })
            .or_else(|| agents.iter().find(|c| c.manifest.id.contains(task_type)))
            .copied()
    }

    /// The execution request for a selected capability.
    fn build_spec(&self, selection: &Selection, task: &TaskRecord) -> ScriptSpec {
        ScriptSpec {
            runtime: selection.runtime,
            entrypoint: selection.entrypoint.clone(),
            args: selection.args.clone(),
            env: Default::default(),
            cwd: self.config.working_dir.clone(),
            timeout: selection.timeout,
            input_payload: Some(task_payload(task)),
        }
    }

    /// Asks the runtime whether it is in a position to run this, without running it.
    ///
    /// Everything checked here is knowable in advance: whether the tool is installed, whether
    /// a credential exists, whether the prompt's placeholders can be filled. Checking it
    /// before a run is opened is what makes "an unconfigured provider costs nothing" true.
    fn preflight(&self, selection: &Selection, task: &TaskRecord) -> Result<(), Refusal> {
        let spec = self.build_spec(selection, task);
        let outcome = match family_of(selection.runtime) {
            RuntimeFamily::Mechanical => {
                ScriptRuntimeAdapter::new(PlatformSupervisor::default()).validate(&spec)
            }
            RuntimeFamily::AgentCli(cli) => {
                AgentCliAdapter::new(cli, PlatformSupervisor::default()).validate(&spec)
            }
            RuntimeFamily::Api(provider) => ApiRuntimeAdapter::new(provider).validate(&spec),
        };

        outcome.map_err(|e| Refusal::NoRuntime {
            capability: selection.capability_id.clone(),
            runtime: format!("{}: {e}", selection.runtime.as_str()),
        })
    }

    /// Runs the selected capability.
    fn execute_capability(
        &self,
        selection: &Selection,
        task: &TaskRecord,
    ) -> Result<RuntimeResult, Refusal> {
        let spec = self.build_spec(selection, task);

        // Dispatch by family rather than by a mechanical/other split: an agent CLI is a
        // supervised process and an API call is not, and the difference is not incidental.
        let outcome = match family_of(selection.runtime) {
            RuntimeFamily::Mechanical => {
                ScriptRuntimeAdapter::new(PlatformSupervisor::default()).execute(&spec, &self.clock)
            }
            RuntimeFamily::AgentCli(cli) => {
                AgentCliAdapter::new(cli, PlatformSupervisor::default()).execute(&spec, &self.clock)
            }
            RuntimeFamily::Api(provider) => {
                ApiRuntimeAdapter::new(provider).execute(&spec, &self.clock)
            }
        };

        outcome.map_err(|e| Refusal::NoRuntime {
            capability: selection.capability_id.clone(),
            runtime: format!("{}: {e}", selection.runtime.as_str()),
        })
    }

    /// Turns declared assurance into checks to run.
    ///
    /// Two sources, in order: what the task declared, then the capability's own output
    /// schema. The capability's schema is added even when the task said nothing, because a
    /// capability that declares an output shape has asked for that shape to be checked.
    fn assurance_spec(&self, plan: &TaskPlan, selection: &Selection) -> AssuranceSpec {
        let mut checks: Vec<AssuranceCheck> = Vec::new();

        for step in plan.effective_assurance() {
            if let Some(kind) = self.check_kind_for(step) {
                let mut check = AssuranceCheck::new(step.label(), kind, step.requires_evidence());
                if let Some(input) = step.input.clone() {
                    check = check.with_input(input);
                }
                checks.push(check);
            }
        }

        if let Some(schema) = &selection.output_schema {
            let name = format!("schema:{schema}");
            if !checks.iter().any(|c| c.name == name) {
                checks.push(AssuranceCheck::new(
                    name,
                    CheckKind::SchemaValidation {
                        schema: schema.clone(),
                    },
                    true,
                ));
            }
        }

        AssuranceSpec::new(checks)
    }

    /// Resolves one declared step into an executable check.
    ///
    /// A `script:` step may name a capability id or a path. Resolving the id through the
    /// registry is what lets a task say `verifier.task-result` without knowing where that
    /// verifier lives on disk.
    fn check_kind_for(&self, step: &AssuranceStep) -> Option<CheckKind> {
        if let Some(schema) = &step.schema {
            return Some(CheckKind::SchemaValidation {
                schema: schema.clone(),
            });
        }
        if let Some(script) = &step.script {
            let path = self
                .registry
                .find_by_id(script)
                .and_then(|cap| cap.resolve_entrypoint().ok())
                .map(|resolved| resolved.target.to_string_lossy().to_string())
                .unwrap_or_else(|| script.clone());
            return Some(CheckKind::ScriptVerifier { script_path: path });
        }
        if let Some(test) = &step.test {
            return Some(CheckKind::TestCommand {
                command: test.clone(),
            });
        }
        None
    }

    /// Runs the assurance checks.
    fn verify(
        &self,
        spec: &AssuranceSpec,
        subject: serde_json::Value,
        envelope: serde_json::Value,
        timeout: TimeDelta,
    ) -> AssuranceResult {
        let mut context = CheckContext::new(subject, &self.config.schema_dir)
            .with_verifier_input(envelope)
            .with_timeout(timeout);
        if let Some(dir) = &self.config.working_dir {
            context = context.with_working_dir(dir);
        }
        let runner =
            RuntimeCheckRunner::new(PlatformSupervisor::default(), self.clock.clone(), context);
        AssuranceEngine::new(pearl_assurance::runner_fn(runner)).run(spec)
    }

    /// Names the outcome, applying the Constitution's ordering of concerns.
    fn decide(
        &self,
        task: &TaskRecord,
        result: &RuntimeResult,
        spec: &AssuranceSpec,
        assurance: &AssuranceResult,
    ) -> Verdict {
        use pearl_runtime::RuntimeExitStatus;

        // Execution first: verification of a failed run is not interesting.
        match result.exit_status {
            RuntimeExitStatus::TimedOut => return Verdict::TimedOut,
            RuntimeExitStatus::Exited { code: 0 } => {}
            RuntimeExitStatus::Exited { code } => {
                return Verdict::Failed {
                    reason: format!("capability exited {code}: {}", first_line(&result.stderr)),
                }
            }
            RuntimeExitStatus::Signalled { signal } => {
                return Verdict::Failed {
                    reason: format!("capability was killed by signal {signal}"),
                }
            }
            RuntimeExitStatus::Cancelled => {
                return Verdict::Failed {
                    reason: "capability was cancelled".to_string(),
                }
            }
        }

        // A check that could not run leaves the claim unestablished, whatever the others said.
        if assurance.errored_count() > 0 {
            return Verdict::Unverified {
                reason: format!(
                    "verification could not be performed: {}",
                    assurance
                        .first_problem()
                        .unwrap_or_else(|| assurance.summary())
                ),
            };
        }

        if !assurance.passed {
            return Verdict::Failed {
                reason: assurance
                    .first_problem()
                    .unwrap_or_else(|| assurance.summary()),
            };
        }

        // Nothing was checked. Whether that is acceptable depends on what the task claimed
        // about itself: a task demanding exactness must not be called verified on the
        // strength of an exit code alone.
        if spec.checks.is_empty() {
            let exactness_needs_more =
                task.quality.exactness_required && task.precision_class != Some(PrecisionClass::P0);
            if exactness_needs_more {
                return Verdict::Unverified {
                    reason: "task requires exactness but no verifier was declared or derivable"
                        .to_string(),
                };
            }
        }

        Verdict::Verified
    }

    /// Records a refusal against the task and releases the claim.
    fn refuse(
        &self,
        store: &mut StateStore,
        task: &TaskRecord,
        lease_id: pearl_core::LeaseId,
        refusal: Refusal,
        started_at: DateTime<Utc>,
    ) -> Result<WorkResult, WorkerError> {
        let now = self.clock.now();
        let detail = refusal.detail();

        // The current state, not the snapshot taken at claim time: a refusal can happen after
        // the task has already moved to RUNNING (a runtime that turns out to be unconfigured
        // is only discovered once execution is attempted), and transitioning from a stale
        // state is how a legal path gets rejected as illegal.
        let current = store
            .get_task(&task.task_id)?
            .map(|t| t.state)
            .unwrap_or(task.state);

        // BLOCKED, not FAILED: nothing ran to completion, and nothing will until a human adds
        // the capability, the permission, the credential or the platform support. A retry
        // would only rediscover the same refusal.
        if current == TaskState::Leased {
            // LEASED cannot reach BLOCKED directly, and the queue edge is the one the state
            // machine provides for "claimed but not started".
            store.transition(
                &task.task_id,
                TaskState::Ready,
                Some(detail.clone()),
                None,
                now,
            )?;
        }
        store.transition(
            &task.task_id,
            TaskState::Blocked,
            Some(detail.clone()),
            None,
            now,
        )?;

        let leases = LeaseManager::new(LeaseConfig::default(), self.clock.clone());
        let _ = leases.release(store, lease_id);

        Ok(WorkResult {
            task_id: task.task_id.clone(),
            capability_id: match &refusal {
                Refusal::NotPermitted { capability, .. }
                | Refusal::WrongPlatform { capability }
                | Refusal::NotRunnable { capability, .. }
                | Refusal::NoRuntime { capability, .. } => capability.clone(),
                _ => String::new(),
            },
            verdict: Verdict::Refused { reason: detail },
            assurance: AssuranceResult {
                passed: false,
                details: Vec::new(),
            },
            exit_code: None,
            structured_output: None,
            started_at,
            completed_at: now,
            duration_ms: (now - started_at).num_milliseconds().max(0) as u64,
        })
    }

    /// Appends an event on the task's trace.
    fn record(
        &self,
        store: &mut StateStore,
        task: &TaskRecord,
        event: PearlEvent,
    ) -> Result<(), WorkerError> {
        let envelope = EventEnvelope::new(task.trace_id, self.clock.now(), event)
            .with_worker(self.config.worker_id.clone());
        store.ledger().append(&envelope)?;
        Ok(())
    }
}

/// What the worker decided to run and how.
#[derive(Debug, Clone)]
struct Selection {
    capability_id: String,
    runtime: Runtime,
    entrypoint: PathBuf,
    args: Vec<String>,
    output_schema: Option<String>,
    timeout: TimeDelta,
}

/// A lease long enough to outlive the work it covers.
///
/// Twice the timeout plus a margin: the supervisor kills the work at the timeout, so the
/// lease can only expire after the work has already been stopped. The heartbeat interval is
/// half the duration, which is the tightest value `LeaseConfig` accepts.
fn lease_config_for(timeout: TimeDelta) -> LeaseConfig {
    let seconds = (timeout.num_seconds() * 2 + 30).max(60);
    let duration = TimeDelta::try_seconds(seconds).expect("bounded");
    LeaseConfig::new(duration, duration / 2)
        .expect("duration is twice the heartbeat by construction")
}

/// The payload handed to a capability on `PEARL_INPUT`.
///
/// The task's declared payload first, then its identity. The order is the point: identity is
/// a fact about this invocation rather than a parameter, so a spec cannot dress a task up as
/// another one by declaring `task_id`. The workflow executor applies the same rule to steps,
/// and it is the same reason.
fn task_payload(task: &TaskRecord) -> serde_json::Value {
    let mut map = task.plan.payload_fields();
    map.insert("task_id".into(), task.task_id.as_str().into());
    map.insert("task_type".into(), task.task_type.clone().into());
    map.insert("trace_id".into(), task.trace_id.to_string().into());
    map.insert("attempt".into(), (task.attempt_count + 1).into());
    map.insert(
        "precision_class".into(),
        match task.precision_class {
            Some(class) => class.as_str().into(),
            None => serde_json::Value::Null,
        },
    );
    map.insert(
        "quality".into(),
        serde_json::to_value(task.quality).unwrap_or(serde_json::Value::Null),
    );
    serde_json::Value::Object(map)
}

/// What verification is performed against.
///
/// The script's structured output when it produced one, otherwise a wrapper carrying the
/// exit code and stderr. Verifiers therefore always receive an object, and a script that
/// emits no JSON can still be schema-checked for the fact that it produced nothing.
fn verification_subject(result: &RuntimeResult) -> serde_json::Value {
    result.structured_output.clone().unwrap_or_else(|| {
        serde_json::json!({
            "exit_code": exit_code_of(result),
            "stdout": result.stdout.trim(),
            "stderr": result.stderr.trim(),
        })
    })
}

/// The envelope a verifier script receives.
///
/// Carries the facts a verifier needs but cannot infer: which task and capability produced
/// the result, how the process exited, and what it said on stderr. The capability's own
/// output stays under `result`, which is the key the shipped verifier reads and the one a
/// check's parameters are not allowed to overwrite.
fn verification_envelope(
    task: &TaskRecord,
    capability_id: &str,
    result: &RuntimeResult,
    subject: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "task_id": task.task_id.as_str(),
        "task_type": task.task_type,
        "attempt": task.attempt_count + 1,
        "capability": capability_id,
        "exit_code": exit_code_of(result),
        "result": subject,
        "stderr": result.stderr.trim(),
    })
}

/// Tokens an agent runtime reported, if it reported any.
///
/// Read from the structured output rather than assumed, because only the runtime knows.
/// Accepts the OpenAI-compatible shape (`usage.total_tokens`) and a bare `tokens`.
fn token_usage(result: &RuntimeResult) -> Option<u64> {
    let output = result.structured_output.as_ref()?;
    output
        .get("usage")
        .and_then(|u| u.get("total_tokens"))
        .or_else(|| output.get("tokens"))
        .and_then(|t| t.as_u64())
}

/// Artifacts a capability declared in its output.
///
/// The contract is a top-level `artifacts` array of `{path, type}`. Each is digested here
/// rather than trusted: Article 4 evidence is only as good as the bytes it names, and a
/// declared path that does not exist is a claim, not an artifact.
fn declared_artifacts(
    result: &RuntimeResult,
    task_id: &pearl_core::TaskId,
    now: DateTime<Utc>,
    working_dir: &Option<PathBuf>,
) -> Vec<pearl_state::Artifact> {
    use sha2::{Digest, Sha256};

    let Some(declared) = result
        .structured_output
        .as_ref()
        .and_then(|v| v.get("artifacts"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut artifacts = Vec::new();
    for entry in declared {
        let Some(path) = entry.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        let resolved = match (Path::new(path).is_absolute(), working_dir) {
            (false, Some(dir)) => dir.join(path),
            _ => PathBuf::from(path),
        };
        let Ok(bytes) = std::fs::read(&resolved) else {
            tracing::warn!(
                path = %resolved.display(),
                "capability declared an artifact that does not exist; not recording it"
            );
            continue;
        };
        artifacts.push(pearl_state::Artifact {
            artifact_id: format!("{task_id}:{path}"),
            task_id: task_id.clone(),
            artifact_type: entry
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("output")
                .to_string(),
            path: resolved.to_string_lossy().to_string(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            size_bytes: bytes.len() as u64,
            created_at: now,
        });
    }
    artifacts
}

fn exit_code_of(result: &RuntimeResult) -> Option<i32> {
    match result.exit_status {
        pearl_runtime::RuntimeExitStatus::Exited { code } => Some(code),
        _ => None,
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no diagnostics")
        .chars()
        .take(200)
        .collect()
}

/// Builds the evidence set for a completed execution (Article 4, §52).
///
/// The execution itself contributes one item, digested over its stdout so the evidence
/// cannot be silently swapped later. Every assurance check that reached a verdict
/// contributes another, classified by what produced it.
fn build_evidence(
    capability_id: &str,
    result: &RuntimeResult,
    assurance: &AssuranceResult,
    now: DateTime<Utc>,
) -> EvidenceSet {
    let mut set = EvidenceSet::new();

    set.push(
        Evidence::new(
            EvidenceType::ToolOutput,
            capability_id,
            if result.is_success() {
                EvidenceResult::Pass
            } else {
                EvidenceResult::Fail
            },
            now,
        )
        .with_artifact(format!("{capability_id}:stdout"), result.stdout.as_bytes()),
    );

    for check in &assurance.details {
        if !check.outcome.is_verdict() {
            // Nothing to point at: a check that could not run produced no artifact.
            continue;
        }
        let evidence_type = match &check.kind {
            CheckKind::SchemaValidation { .. } => EvidenceType::Schema,
            CheckKind::ScriptVerifier { .. } => EvidenceType::ToolOutput,
            CheckKind::TestCommand { .. } => EvidenceType::Test,
        };
        set.push(Evidence::new(
            evidence_type,
            check.name.clone(),
            if check.outcome.passed() {
                EvidenceResult::Pass
            } else {
                EvidenceResult::Fail
            },
            now,
        ));
    }

    set
}

/// Convenience for callers that only have a path.
pub fn load_registry(dir: &Path) -> Result<CapabilityRegistry, WorkerError> {
    CapabilityRegistry::load_directory(dir).map_err(|e| WorkerError::Registry {
        path: dir.to_path_buf(),
        source: e,
    })
}
