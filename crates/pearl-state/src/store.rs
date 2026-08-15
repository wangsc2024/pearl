//! The materialized state store.
//!
//! ADR-0001: the ledger is truth, these tables are a derived cache. Two rules follow and
//! this module exists to enforce them:
//!
//! 1. **Every mutation is one transaction containing both writes.** Appending the event
//!    and updating the projection must be atomic, otherwise a crash between them leaves
//!    the cache disagreeing with truth and replay would "fix" state that was never real.
//! 2. **Nothing writes a projection without a corresponding event.** The only way to
//!    change state is through a method here, each of which records why.

use crate::records::{
    Artifact, AttemptRecord, CheckpointRecord, ConfigRevision, EffectRecord, EvidenceRecord,
    LeaseRecord, PolicyDecision, RunRecord, RuntimeHealth, StepRecord, TaskRecord,
    VerificationResult,
};
use chrono::{DateTime, Utc};
use pearl_core::{
    AttemptId, CheckpointId, EvidenceSet, ExactnessGate, IdempotencyKey, LeaseId, PrecisionClass,
    QualitySpec, RunId, TaskId, TaskPlan, TaskState, TraceId, TransitionError, WorkerId,
};
use pearl_events::{append_in_tx, EventEnvelope, EventLedger, LedgerError, PearlEvent, RunOutcome};
use rusqlite::{params, OptionalExtension, Transaction};
use std::path::Path;

/// DDL for the projections.
///
/// Deliberately free of the append-only triggers that guard `events`: these tables are
/// *meant* to be mutable and droppable, because they are a cache.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    task_id         TEXT PRIMARY KEY,
    trace_id        TEXT    NOT NULL,
    task_type       TEXT    NOT NULL,
    state           TEXT    NOT NULL,
    precision_class TEXT,
    exactness_required          INTEGER NOT NULL,
    deterministic_generation    INTEGER NOT NULL,
    deterministic_verification  INTEGER NOT NULL,
    created_at      TEXT    NOT NULL,
    updated_at      TEXT    NOT NULL,
    attempt_count   INTEGER NOT NULL DEFAULT 0,
    last_reason     TEXT,
    -- The declared execution and verification plan, as submitted (§22, §32). JSON rather
    -- than normalised columns because it is read whole, written once, and never queried
    -- by field.
    plan            TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks (state);

CREATE TABLE IF NOT EXISTS runs (
    run_id          TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    trace_id        TEXT    NOT NULL,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    config_revision TEXT    NOT NULL,
    config_hash     TEXT    NOT NULL,
    outcome         TEXT
);
CREATE INDEX IF NOT EXISTS idx_runs_task ON runs (task_id);

CREATE TABLE IF NOT EXISTS attempts (
    attempt_id      TEXT PRIMARY KEY,
    run_id          TEXT    NOT NULL,
    attempt_number  INTEGER NOT NULL,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    outcome         TEXT,
    exit_reason     TEXT
);
CREATE INDEX IF NOT EXISTS idx_attempts_run ON attempts (run_id);

CREATE TABLE IF NOT EXISTS leases (
    lease_id        TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    worker_id       TEXT    NOT NULL,
    acquired_at     TEXT    NOT NULL,
    leased_until    TEXT    NOT NULL,
    last_heartbeat  TEXT    NOT NULL,
    released_at     TEXT
);
CREATE INDEX IF NOT EXISTS idx_leases_task ON leases (task_id);
-- Partial index: the reaper only ever scans unreleased leases.
CREATE INDEX IF NOT EXISTS idx_leases_open ON leases (leased_until) WHERE released_at IS NULL;

CREATE TABLE IF NOT EXISTS effects (
    idempotency_key TEXT PRIMARY KEY,
    effect          TEXT    NOT NULL,
    requested_at    TEXT    NOT NULL,
    committed_at    TEXT
);

CREATE TABLE IF NOT EXISTS evidence (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id       TEXT    NOT NULL,
    evidence_type TEXT    NOT NULL,
    producer      TEXT    NOT NULL,
    passed        INTEGER NOT NULL,
    recorded_at   TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_evidence_task ON evidence (task_id);

CREATE TABLE IF NOT EXISTS steps (
    step_id         TEXT PRIMARY KEY,
    run_id          TEXT    NOT NULL,
    step_number     INTEGER NOT NULL,
    description     TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'pending',
    started_at      TEXT,
    completed_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_steps_run ON steps (run_id);

CREATE TABLE IF NOT EXISTS checkpoints (
    checkpoint_id   TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    label           TEXT    NOT NULL,
    payload         TEXT,
    created_at      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_task ON checkpoints (task_id);

CREATE TABLE IF NOT EXISTS schedules (
    schedule_id     TEXT PRIMARY KEY,
    task_type       TEXT    NOT NULL,
    cron_expr       TEXT,
    interval_secs   INTEGER,
    next_run_at     TEXT,
    enabled         INTEGER NOT NULL DEFAULT 1,
    created_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS capabilities (
    capability_id   TEXT PRIMARY KEY,
    version         INTEGER NOT NULL,
    capability_type TEXT    NOT NULL,
    runtime         TEXT    NOT NULL,
    deterministic   INTEGER NOT NULL,
    registered_at   TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS artifacts (
    artifact_id     TEXT PRIMARY KEY,
    task_id         TEXT    NOT NULL,
    artifact_type   TEXT    NOT NULL,
    path            TEXT    NOT NULL,
    sha256          TEXT    NOT NULL,
    size_bytes      INTEGER NOT NULL,
    created_at      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_task ON artifacts (task_id);

CREATE TABLE IF NOT EXISTS verification_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT    NOT NULL,
    verifier_id     TEXT    NOT NULL,
    passed          INTEGER NOT NULL,
    detail          TEXT,
    verified_at     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_verification_task ON verification_results (task_id);

CREATE TABLE IF NOT EXISTS policy_decisions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id         TEXT,
    decision_type   TEXT    NOT NULL,
    outcome         TEXT    NOT NULL,
    reason          TEXT,
    decided_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS config_revisions (
    revision_id     TEXT PRIMARY KEY,
    config_hash     TEXT    NOT NULL,
    source          TEXT    NOT NULL,
    applied_at      TEXT    NOT NULL,
    payload         TEXT
);

CREATE TABLE IF NOT EXISTS runtime_health (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    subsystem       TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    detail          TEXT,
    recorded_at     TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_runtime_health_subsystem ON runtime_health (subsystem);
"#;

/// Tables that `rebuild_from_ledger` clears before replaying.
const PROJECTION_TABLES: [&str; 15] = [
    "tasks",
    "runs",
    "attempts",
    "leases",
    "effects",
    "evidence",
    "steps",
    "checkpoints",
    "schedules",
    "capabilities",
    "artifacts",
    "verification_results",
    "policy_decisions",
    "config_revisions",
    // Was missing, so a rebuild left stale health rows behind while claiming to have
    // reconstructed every projection.
    "runtime_health",
];

/// A new task to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSubmission {
    pub task_id: TaskId,
    pub task_type: String,
    pub precision_class: Option<PrecisionClass>,
    pub quality: QualitySpec,
    /// What the submitter declared about execution and verification.
    ///
    /// Persisted rather than validated-and-discarded: a worker that cannot read the
    /// declared assurance cannot honour it.
    pub plan: TaskPlan,
}

impl TaskSubmission {
    /// A submission with no execution plan, for callers that only carry a quality contract.
    pub fn new(
        task_id: TaskId,
        task_type: impl Into<String>,
        precision_class: Option<PrecisionClass>,
        quality: QualitySpec,
    ) -> Self {
        Self {
            task_id,
            task_type: task_type.into(),
            precision_class,
            quality,
            plan: TaskPlan::empty(),
        }
    }

    /// Attaches a declared plan.
    pub fn with_plan(mut self, plan: TaskPlan) -> Self {
        self.plan = plan;
        self
    }
}

/// Materialized state over an event ledger.
pub struct StateStore {
    ledger: EventLedger,
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        Self::init(EventLedger::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, StateError> {
        Self::init(EventLedger::open_in_memory()?)
    }

    fn init(ledger: EventLedger) -> Result<Self, StateError> {
        ledger.connection().execute_batch(SCHEMA)?;
        Ok(Self { ledger })
    }

    pub fn ledger(&self) -> &EventLedger {
        &self.ledger
    }

    // ---------------------------------------------------------------- tasks

    /// Creates a task in `CREATED`.
    pub fn create_task(
        &mut self,
        submission: TaskSubmission,
        now: DateTime<Utc>,
    ) -> Result<TaskRecord, StateError> {
        if self.get_task(&submission.task_id)?.is_some() {
            return Err(StateError::TaskAlreadyExists {
                task_id: submission.task_id.to_string(),
            });
        }

        let trace_id = TraceId::new();
        let event = PearlEvent::TaskCreated {
            task_id: submission.task_id.clone(),
            task_type: submission.task_type.clone(),
            precision_class: submission.precision_class,
            quality: submission.quality,
            plan: submission.plan.clone(),
        };
        let envelope = EventEnvelope::new(trace_id, now, event);

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        insert_task(&tx, &submission, trace_id, now)?;
        tx.commit()?;

        Ok(TaskRecord {
            task_id: submission.task_id,
            trace_id,
            task_type: submission.task_type,
            state: TaskState::Created,
            precision_class: submission.precision_class,
            quality: submission.quality,
            plan: submission.plan,
            created_at: now,
            updated_at: now,
            attempt_count: 0,
            last_reason: None,
        })
    }

    /// Moves a task to a new state.
    ///
    /// Three gates apply in order, and the order matters: an illegal transition is
    /// rejected before evidence is even considered, so a caller cannot smuggle a bad
    /// transition through by attaching good evidence.
    pub fn transition(
        &mut self,
        task_id: &TaskId,
        to: TaskState,
        reason: Option<String>,
        evidence: Option<&EvidenceSet>,
        now: DateTime<Utc>,
    ) -> Result<TaskRecord, StateError> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| StateError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;

        // Gate 1 — the state machine.
        task.state.validate_transition(to)?;

        // Gate 2 — the Exactness Gate (Article 2). Exactness demanded with no mechanical
        // verification cannot reach success; the honest destination is UNVERIFIED.
        if to == TaskState::VerifiedSuccess
            && task.quality.gate() == ExactnessGate::BlockAutoComplete
        {
            return Err(StateError::ExactnessGateBlocked {
                task_id: task_id.to_string(),
            });
        }

        // Gate 3 — evidence (Article 4). Success must be provable.
        if to == TaskState::VerifiedSuccess {
            match evidence {
                None => {
                    return Err(StateError::Transition(
                        TransitionError::EvidenceInsufficient {
                            reason: "no evidence supplied".into(),
                        },
                    ))
                }
                Some(set) => {
                    if let Some(rejection) = set.rejection_reason() {
                        return Err(StateError::Transition(
                            TransitionError::EvidenceInsufficient {
                                reason: rejection.to_string(),
                            },
                        ));
                    }
                }
            }
        }

        let envelope = EventEnvelope::new(
            task.trace_id,
            now,
            PearlEvent::TaskStateChanged {
                task_id: task_id.clone(),
                from: task.state,
                to,
                reason: reason.clone(),
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        apply_state_change(&tx, task_id, to, reason.as_deref(), now)?;

        if let Some(set) = evidence {
            for item in set.items() {
                let ev = EventEnvelope::new(
                    task.trace_id,
                    now,
                    PearlEvent::EvidenceStored {
                        task_id: task_id.clone(),
                        evidence_type: item.evidence_type.as_str().to_string(),
                        producer: item.producer.clone(),
                        passed: item.passed(),
                    },
                );
                append_in_tx(&tx, &ev)?;
                insert_evidence(
                    &tx,
                    task_id,
                    item.evidence_type.as_str(),
                    &item.producer,
                    item.passed(),
                    now,
                )?;
            }
        }

        if to.is_terminal() {
            let ev = EventEnvelope::new(
                task.trace_id,
                now,
                PearlEvent::TaskCompleted {
                    task_id: task_id.clone(),
                    final_state: to,
                },
            );
            append_in_tx(&tx, &ev)?;
        }

        tx.commit()?;

        self.get_task(task_id)?
            .ok_or_else(|| StateError::TaskNotFound {
                task_id: task_id.to_string(),
            })
    }

    pub fn get_task(&self, task_id: &TaskId) -> Result<Option<TaskRecord>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT task_id, trace_id, task_type, state, precision_class,
                        exactness_required, deterministic_generation, deterministic_verification,
                        created_at, updated_at, attempt_count, last_reason, plan
                 FROM tasks WHERE task_id = ?1",
                params![task_id.as_str()],
                row_to_task,
            )
            .optional()?)
    }

    /// Tasks in a given state, oldest first.
    ///
    /// Oldest-first is the fairness rule: it prevents a steady stream of new work from
    /// starving a task that has been waiting.
    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<TaskRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT task_id, trace_id, task_type, state, precision_class,
                    exactness_required, deterministic_generation, deterministic_verification,
                    created_at, updated_at, attempt_count, last_reason, plan
             FROM tasks WHERE state = ?1 ORDER BY created_at ASC, task_id ASC",
        )?;
        let rows = stmt.query_map(params![state.as_str()], row_to_task)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn count_by_state(&self, state: TaskState) -> Result<u64, StateError> {
        let n: i64 = self.ledger.connection().query_row(
            "SELECT COUNT(*) FROM tasks WHERE state = ?1",
            params![state.as_str()],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn all_tasks(&self) -> Result<Vec<TaskRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT task_id, trace_id, task_type, state, precision_class,
                    exactness_required, deterministic_generation, deterministic_verification,
                    created_at, updated_at, attempt_count, last_reason, plan
             FROM tasks ORDER BY task_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_task)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ----------------------------------------------------------------- runs

    /// Opens a run.
    ///
    /// `config_revision` and `config_hash` are required parameters rather than optional
    /// metadata because Article 10 makes an unrecorded configuration a defect, and a
    /// required parameter is the cheapest place to enforce that.
    pub fn start_run(
        &mut self,
        task_id: &TaskId,
        config_revision: &str,
        config_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<RunRecord, StateError> {
        if config_revision.is_empty() || config_hash.is_empty() {
            return Err(StateError::MissingConfigRevision);
        }
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| StateError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;

        let run_id = RunId::new();
        let envelope = EventEnvelope::new(
            task.trace_id,
            now,
            PearlEvent::RunStarted {
                task_id: task_id.clone(),
                run_id,
                config_revision: config_revision.to_string(),
                config_hash: config_hash.to_string(),
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        insert_run(
            &tx,
            run_id,
            task_id,
            task.trace_id,
            config_revision,
            config_hash,
            now,
        )?;
        tx.commit()?;

        Ok(RunRecord {
            run_id,
            task_id: task_id.clone(),
            trace_id: task.trace_id,
            started_at: now,
            ended_at: None,
            config_revision: config_revision.to_string(),
            config_hash: config_hash.to_string(),
            outcome: None,
        })
    }

    pub fn end_run(
        &mut self,
        run_id: RunId,
        outcome: RunOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let (task_id, trace_id) = self.run_owner(run_id)?;
        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::RunEnded {
                task_id,
                run_id,
                outcome,
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        finish_run(&tx, run_id, outcome, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_run(&self, run_id: RunId) -> Result<Option<RunRecord>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT run_id, task_id, trace_id, started_at, ended_at,
                        config_revision, config_hash, outcome
                 FROM runs WHERE run_id = ?1",
                params![run_id.to_string()],
                row_to_run,
            )
            .optional()?)
    }

    pub fn runs_for_task(&self, task_id: &TaskId) -> Result<Vec<RunRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT run_id, task_id, trace_id, started_at, ended_at,
                    config_revision, config_hash, outcome
             FROM runs WHERE task_id = ?1 ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_run)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ------------------------------------------------------------- attempts

    /// Opens an attempt and increments the task's attempt counter.
    pub fn start_attempt(
        &mut self,
        run_id: RunId,
        attempt_number: u32,
        now: DateTime<Utc>,
    ) -> Result<AttemptRecord, StateError> {
        let (task_id, trace_id) = self.run_owner(run_id)?;
        let attempt_id = AttemptId::new();
        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::AttemptStarted {
                // The run determines the task, but the ledger indexes on task_id, so an
                // attempt without it would not appear in that task's history.
                task_id: Some(task_id.clone()),
                run_id,
                attempt_id,
                attempt_number,
            },
        )
        .with_attempt(attempt_id);

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        insert_attempt(&tx, attempt_id, run_id, attempt_number, now)?;
        bump_attempt_count(&tx, &task_id, now)?;
        tx.commit()?;

        Ok(AttemptRecord {
            attempt_id,
            run_id,
            attempt_number,
            started_at: now,
            ended_at: None,
            outcome: None,
            exit_reason: None,
        })
    }

    pub fn end_attempt(
        &mut self,
        attempt_id: AttemptId,
        outcome: RunOutcome,
        exit_reason: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let run_id: String = self
            .ledger
            .connection()
            .query_row(
                "SELECT run_id FROM attempts WHERE attempt_id = ?1",
                params![attempt_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StateError::AttemptNotFound {
                attempt_id: attempt_id.to_string(),
            })?;
        let run_id = RunId::parse(&run_id).map_err(|_| StateError::AttemptNotFound {
            attempt_id: attempt_id.to_string(),
        })?;
        let (owner, trace_id) = self.run_owner(run_id)?;

        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::AttemptEnded {
                task_id: Some(owner),
                run_id,
                attempt_id,
                outcome,
                exit_reason: exit_reason.clone(),
            },
        )
        .with_attempt(attempt_id);

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        finish_attempt(&tx, attempt_id, outcome, exit_reason.as_deref(), now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn attempts_for_run(&self, run_id: RunId) -> Result<Vec<AttemptRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT attempt_id, run_id, attempt_number, started_at, ended_at, outcome, exit_reason
             FROM attempts WHERE run_id = ?1 ORDER BY attempt_number ASC",
        )?;
        let rows = stmt.query_map(params![run_id.to_string()], row_to_attempt)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // --------------------------------------------------------------- effects

    /// Registers intent to perform a side effect, or reports that it already happened.
    ///
    /// This is the Article 5 mechanism. The caller asks *before* acting; a
    /// `Deduplicated` answer means the effect must not be performed again.
    pub fn request_effect(
        &mut self,
        effect: &str,
        key: &IdempotencyKey,
        trace_id: TraceId,
        now: DateTime<Utc>,
    ) -> Result<EffectDecision, StateError> {
        if let Some(existing) = self.get_effect(key)? {
            let envelope = EventEnvelope::new(
                trace_id,
                now,
                PearlEvent::EffectDeduplicated {
                    effect: effect.to_string(),
                    idempotency_key: key.clone(),
                },
            );
            self.ledger.append(&envelope)?;
            return Ok(EffectDecision::Deduplicated(existing));
        }

        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::EffectRequested {
                effect: effect.to_string(),
                idempotency_key: key.clone(),
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        insert_effect(&tx, key, effect, now)?;
        tx.commit()?;

        Ok(EffectDecision::Proceed)
    }

    /// Marks a requested effect as actually performed.
    pub fn commit_effect(
        &mut self,
        effect: &str,
        key: &IdempotencyKey,
        trace_id: TraceId,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::EffectCommitted {
                effect: effect.to_string(),
                idempotency_key: key.clone(),
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        mark_effect_committed(&tx, key, now)?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- steps

    /// Records a step of a run — §43.
    ///
    /// Steps are what actually ran, as distinct from the plan, which is what was declared.
    /// Keeping both makes "the run did not follow its plan" a detectable condition.
    ///
    /// Idempotent on `step_id`: a step that moves from running to success is one step with
    /// two observations, not two steps.
    pub fn record_step(&mut self, step: &StepRecord) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO steps
                (step_id, run_id, step_number, description, status, started_at, completed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                step.step_id,
                step.run_id.to_string(),
                step.step_number,
                step.description,
                step.status,
                step.started_at.map(|t| t.to_rfc3339()),
                step.completed_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The steps of a run, in execution order.
    pub fn steps_for_run(&self, run_id: RunId) -> Result<Vec<StepRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT step_id, run_id, step_number, description, status, started_at, completed_at
             FROM steps WHERE run_id = ?1 ORDER BY step_number ASC",
        )?;
        let rows = stmt.query_map(params![run_id.to_string()], row_to_step)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ----------------------------------------------------------- checkpoints

    /// Commits a checkpoint — §41.
    ///
    /// Appends the event and writes the projection in one transaction, so a checkpoint that
    /// is visible is a checkpoint that was recorded. Resume reads the latest one; a
    /// checkpoint written outside the ledger could not survive a rebuild.
    pub fn commit_checkpoint(
        &mut self,
        task_id: &TaskId,
        step_id: &str,
        payload: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<CheckpointId, StateError> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| StateError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;
        let checkpoint_id = CheckpointId::new();
        let envelope = EventEnvelope::new(
            task.trace_id,
            now,
            PearlEvent::CheckpointCommitted {
                checkpoint_id,
                step_id: step_id.to_string(),
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        insert_checkpoint(&tx, checkpoint_id, task_id, step_id, payload, now)?;
        tx.commit()?;
        Ok(checkpoint_id)
    }

    /// Every checkpoint for a task, oldest first.
    pub fn checkpoints_for_task(
        &self,
        task_id: &TaskId,
    ) -> Result<Vec<CheckpointRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT checkpoint_id, task_id, label, payload, created_at
             FROM checkpoints WHERE task_id = ?1 ORDER BY created_at ASC, checkpoint_id ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_checkpoint)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The most recent checkpoint, which is where a resumed run continues from.
    pub fn latest_checkpoint(
        &self,
        task_id: &TaskId,
    ) -> Result<Option<CheckpointRecord>, StateError> {
        Ok(self.checkpoints_for_task(task_id)?.pop())
    }

    // -------------------------------------------------- verification results

    /// Records a verifier's verdict — Article 8.
    pub fn record_verification(
        &mut self,
        task_id: &TaskId,
        verifier_id: &str,
        passed: bool,
        detail: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT INTO verification_results (task_id, verifier_id, passed, detail, verified_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                task_id.as_str(),
                verifier_id,
                passed as i32,
                detail,
                now.to_rfc3339()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every verdict recorded for a task.
    pub fn verifications_for_task(
        &self,
        task_id: &TaskId,
    ) -> Result<Vec<VerificationResult>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT task_id, verifier_id, passed, detail, verified_at
             FROM verification_results WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_verification)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ------------------------------------------------------------- artifacts

    /// Records an artifact a task produced — §44.
    ///
    /// Content-addressed by SHA-256 so the index cannot silently point at different bytes
    /// than the ones that were produced.
    pub fn record_artifact(&mut self, artifact: &Artifact) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO artifacts
                (artifact_id, task_id, artifact_type, path, sha256, size_bytes, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                artifact.artifact_id,
                artifact.task_id.as_str(),
                artifact.artifact_type,
                artifact.path,
                artifact.sha256,
                artifact.size_bytes as i64,
                artifact.created_at.to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every artifact a task produced.
    pub fn artifacts_for_task(&self, task_id: &TaskId) -> Result<Vec<Artifact>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT artifact_id, task_id, artifact_type, path, sha256, size_bytes, created_at
             FROM artifacts WHERE task_id = ?1 ORDER BY created_at ASC, artifact_id ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_artifact)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // -------------------------------------------------------- runtime health

    /// Records a health observation — §60.
    pub fn record_health(
        &mut self,
        subsystem: &str,
        status: &str,
        detail: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT INTO runtime_health (subsystem, status, detail, recorded_at)
             VALUES (?1,?2,?3,?4)",
            params![subsystem, status, detail, now.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The most recent observation per subsystem.
    pub fn latest_health(&self) -> Result<Vec<RuntimeHealth>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT subsystem, status, detail, recorded_at FROM runtime_health
             WHERE id IN (SELECT MAX(id) FROM runtime_health GROUP BY subsystem)
             ORDER BY subsystem ASC",
        )?;
        let rows = stmt.query_map([], row_to_health)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ------------------------------------------------------ policy decisions

    /// Records a policy or permission decision — §45.
    pub fn record_policy_decision(
        &mut self,
        task_id: Option<&TaskId>,
        decision_type: &str,
        outcome: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT INTO policy_decisions (task_id, decision_type, outcome, reason, decided_at)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                task_id.map(|t| t.as_str().to_string()),
                decision_type,
                outcome,
                reason,
                now.to_rfc3339()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every decision recorded for a task.
    pub fn policy_decisions_for_task(
        &self,
        task_id: &TaskId,
    ) -> Result<Vec<PolicyDecision>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT task_id, decision_type, outcome, reason, decided_at
             FROM policy_decisions WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_policy_decision)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ------------------------------------------------------ config revisions

    /// Records a configuration revision — Article 10.
    pub fn record_config_revision(&mut self, revision: &ConfigRevision) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO config_revisions
                (revision_id, config_hash, source, applied_at, payload)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                revision.revision_id,
                revision.config_hash,
                revision.source,
                revision.applied_at.to_rfc3339(),
                revision.payload,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// A configuration revision by id.
    pub fn get_config_revision(
        &self,
        revision_id: &str,
    ) -> Result<Option<ConfigRevision>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT revision_id, config_hash, source, applied_at, payload
                 FROM config_revisions WHERE revision_id = ?1",
                params![revision_id],
                row_to_config_revision,
            )
            .optional()?)
    }

    /// Every recorded reason to believe this task's result — Article 4.
    ///
    /// Ordered oldest-first so the sequence reads as the argument it is: the execution's own
    /// output, then each verification that examined it.
    pub fn evidence_for_task(&self, task_id: &TaskId) -> Result<Vec<EvidenceRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT task_id, evidence_type, producer, passed, recorded_at
             FROM evidence WHERE task_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![task_id.as_str()], row_to_evidence)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_effect(&self, key: &IdempotencyKey) -> Result<Option<EffectRecord>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT idempotency_key, effect, requested_at, committed_at
                 FROM effects WHERE idempotency_key = ?1",
                params![key.as_str()],
                row_to_effect,
            )
            .optional()?)
    }

    // ---------------------------------------------------------------- leases

    pub fn insert_lease(&mut self, lease: &LeaseRecord) -> Result<(), StateError> {
        let tx = self.ledger.connection_mut().transaction()?;
        write_lease(&tx, lease)?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_lease(&self, lease_id: LeaseId) -> Result<Option<LeaseRecord>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT lease_id, task_id, worker_id, acquired_at, leased_until,
                        last_heartbeat, released_at
                 FROM leases WHERE lease_id = ?1",
                params![lease_id.to_string()],
                row_to_lease,
            )
            .optional()?)
    }

    /// Open leases whose deadline has passed.
    pub fn expired_leases(&self, now: DateTime<Utc>) -> Result<Vec<LeaseRecord>, StateError> {
        let conn = self.ledger.connection();
        let mut stmt = conn.prepare(
            "SELECT lease_id, task_id, worker_id, acquired_at, leased_until,
                    last_heartbeat, released_at
             FROM leases
             WHERE released_at IS NULL AND leased_until < ?1
             ORDER BY leased_until ASC",
        )?;
        let rows = stmt.query_map(params![now.to_rfc3339()], row_to_lease)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn active_lease_for_task(
        &self,
        task_id: &TaskId,
    ) -> Result<Option<LeaseRecord>, StateError> {
        Ok(self
            .ledger
            .connection()
            .query_row(
                "SELECT lease_id, task_id, worker_id, acquired_at, leased_until,
                        last_heartbeat, released_at
                 FROM leases WHERE task_id = ?1 AND released_at IS NULL
                 ORDER BY acquired_at DESC LIMIT 1",
                params![task_id.as_str()],
                row_to_lease,
            )
            .optional()?)
    }

    pub fn renew_lease(
        &mut self,
        lease_id: LeaseId,
        leased_until: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let lease = self
            .get_lease(lease_id)?
            .ok_or_else(|| StateError::LeaseNotFound {
                lease_id: lease_id.to_string(),
            })?;
        if lease.released_at.is_some() {
            return Err(StateError::LeaseAlreadyReleased {
                lease_id: lease_id.to_string(),
            });
        }

        let trace_id = self.trace_for_task(&lease.task_id)?;
        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::LeaseRenewed {
                lease_id,
                leased_until,
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        tx.execute(
            "UPDATE leases SET leased_until = ?1, last_heartbeat = ?2 WHERE lease_id = ?3",
            params![
                leased_until.to_rfc3339(),
                now.to_rfc3339(),
                lease_id.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn release_lease(
        &mut self,
        lease_id: LeaseId,
        now: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let lease = self
            .get_lease(lease_id)?
            .ok_or_else(|| StateError::LeaseNotFound {
                lease_id: lease_id.to_string(),
            })?;
        let trace_id = self.trace_for_task(&lease.task_id)?;
        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::LeaseReleased {
                task_id: lease.task_id.clone(),
                lease_id,
            },
        );

        let tx = self.ledger.connection_mut().transaction()?;
        append_in_tx(&tx, &envelope)?;
        tx.execute(
            "UPDATE leases SET released_at = ?1 WHERE lease_id = ?2",
            params![now.to_rfc3339(), lease_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---------------------------------------------------------------- replay

    /// Rebuilds every projection from the ledger.
    ///
    /// This is both the recovery path and the correctness check: if replayed state ever
    /// differs from incrementally maintained state, one of the two is wrong and ADR-0001's
    /// central claim is broken. The replay test asserts they agree.
    pub fn rebuild_from_ledger(&mut self) -> Result<ReplaySummary, StateError> {
        let events = self.ledger.read_all()?;
        let tx = self.ledger.connection_mut().transaction()?;

        for table in PROJECTION_TABLES {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }

        let mut applied = 0u64;
        let mut skipped = 0u64;
        for envelope in &events {
            if project(&tx, envelope)? {
                applied += 1;
            } else {
                skipped += 1;
            }
        }
        tx.commit()?;

        Ok(ReplaySummary {
            total_events: events.len() as u64,
            applied,
            skipped,
        })
    }

    // --------------------------------------------------------------- helpers

    fn run_owner(&self, run_id: RunId) -> Result<(TaskId, TraceId), StateError> {
        let row: Option<(String, String)> = self
            .ledger
            .connection()
            .query_row(
                "SELECT task_id, trace_id FROM runs WHERE run_id = ?1",
                params![run_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (task_id, trace_id) = row.ok_or_else(|| StateError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
        Ok((
            TaskId::parse(task_id)?,
            TraceId::parse(&trace_id).map_err(|_| StateError::RunNotFound {
                run_id: run_id.to_string(),
            })?,
        ))
    }

    fn trace_for_task(&self, task_id: &TaskId) -> Result<TraceId, StateError> {
        self.get_task(task_id)?
            .map(|t| t.trace_id)
            .ok_or_else(|| StateError::TaskNotFound {
                task_id: task_id.to_string(),
            })
    }
}

/// Whether a side effect should be performed.
#[derive(Debug, Clone, PartialEq)]
pub enum EffectDecision {
    /// Not seen before — perform it.
    Proceed,
    /// Already recorded under this key — do not perform it again.
    Deduplicated(EffectRecord),
}

impl EffectDecision {
    pub fn should_proceed(&self) -> bool {
        matches!(self, EffectDecision::Proceed)
    }
}

/// Outcome of a ledger replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaySummary {
    pub total_events: u64,
    /// Events that changed a projection.
    pub applied: u64,
    /// Events with no projection effect (script/verification telemetry).
    pub skipped: u64,
}

// ------------------------------------------------------------ SQL primitives

fn insert_task(
    tx: &Transaction<'_>,
    s: &TaskSubmission,
    trace_id: TraceId,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO tasks
            (task_id, trace_id, task_type, state, precision_class,
             exactness_required, deterministic_generation, deterministic_verification,
             created_at, updated_at, attempt_count, last_reason, plan)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,NULL,?11)",
        params![
            s.task_id.as_str(),
            trace_id.to_string(),
            s.task_type,
            TaskState::Created.as_str(),
            s.precision_class.map(|p| p.as_str().to_string()),
            s.quality.exactness_required as i32,
            s.quality.deterministic_generation as i32,
            s.quality.deterministic_verification as i32,
            now.to_rfc3339(),
            now.to_rfc3339(),
            encode_plan(&s.plan)?,
        ],
    )?;
    Ok(())
}

/// Serialises a plan for storage, or `None` when it declares nothing.
///
/// Storing `NULL` for an empty plan keeps the column honest: a row with a plan really did
/// have one declared.
fn encode_plan(plan: &TaskPlan) -> Result<Option<String>, StateError> {
    if plan.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(plan).map_err(|e| {
        StateError::PlanEncoding {
            detail: e.to_string(),
        }
    })?))
}

fn apply_state_change(
    tx: &Transaction<'_>,
    task_id: &TaskId,
    to: TaskState,
    reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE tasks SET state = ?1, updated_at = ?2, last_reason = ?3 WHERE task_id = ?4",
        params![to.as_str(), now.to_rfc3339(), reason, task_id.as_str()],
    )?;
    Ok(())
}

fn bump_attempt_count(
    tx: &Transaction<'_>,
    task_id: &TaskId,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE tasks SET attempt_count = attempt_count + 1, updated_at = ?1 WHERE task_id = ?2",
        params![now.to_rfc3339(), task_id.as_str()],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_run(
    tx: &Transaction<'_>,
    run_id: RunId,
    task_id: &TaskId,
    trace_id: TraceId,
    config_revision: &str,
    config_hash: &str,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO runs (run_id, task_id, trace_id, started_at, ended_at, config_revision, config_hash, outcome)
         VALUES (?1,?2,?3,?4,NULL,?5,?6,NULL)",
        params![
            run_id.to_string(),
            task_id.as_str(),
            trace_id.to_string(),
            now.to_rfc3339(),
            config_revision,
            config_hash,
        ],
    )?;
    Ok(())
}

fn finish_run(
    tx: &Transaction<'_>,
    run_id: RunId,
    outcome: RunOutcome,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE runs SET ended_at = ?1, outcome = ?2 WHERE run_id = ?3",
        params![now.to_rfc3339(), outcome.as_str(), run_id.to_string()],
    )?;
    Ok(())
}

fn insert_attempt(
    tx: &Transaction<'_>,
    attempt_id: AttemptId,
    run_id: RunId,
    attempt_number: u32,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO attempts (attempt_id, run_id, attempt_number, started_at, ended_at, outcome, exit_reason)
         VALUES (?1,?2,?3,?4,NULL,NULL,NULL)",
        params![
            attempt_id.to_string(),
            run_id.to_string(),
            attempt_number,
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn finish_attempt(
    tx: &Transaction<'_>,
    attempt_id: AttemptId,
    outcome: RunOutcome,
    exit_reason: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE attempts SET ended_at = ?1, outcome = ?2, exit_reason = ?3 WHERE attempt_id = ?4",
        params![
            now.to_rfc3339(),
            outcome.as_str(),
            exit_reason,
            attempt_id.to_string()
        ],
    )?;
    Ok(())
}

fn insert_effect(
    tx: &Transaction<'_>,
    key: &IdempotencyKey,
    effect: &str,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO effects (idempotency_key, effect, requested_at, committed_at)
         VALUES (?1,?2,?3,NULL)",
        params![key.as_str(), effect, now.to_rfc3339()],
    )?;
    Ok(())
}

fn mark_effect_committed(
    tx: &Transaction<'_>,
    key: &IdempotencyKey,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE effects SET committed_at = ?1 WHERE idempotency_key = ?2",
        params![now.to_rfc3339(), key.as_str()],
    )?;
    Ok(())
}

fn write_lease(tx: &Transaction<'_>, lease: &LeaseRecord) -> Result<(), StateError> {
    tx.execute(
        "INSERT OR REPLACE INTO leases
            (lease_id, task_id, worker_id, acquired_at, leased_until, last_heartbeat, released_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            lease.lease_id.to_string(),
            lease.task_id.as_str(),
            lease.worker_id.as_str(),
            lease.acquired_at.to_rfc3339(),
            lease.leased_until.to_rfc3339(),
            lease.last_heartbeat.to_rfc3339(),
            lease.released_at.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

fn insert_evidence(
    tx: &Transaction<'_>,
    task_id: &TaskId,
    evidence_type: &str,
    producer: &str,
    passed: bool,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO evidence (task_id, evidence_type, producer, passed, recorded_at)
         VALUES (?1,?2,?3,?4,?5)",
        params![
            task_id.as_str(),
            evidence_type,
            producer,
            passed as i32,
            now.to_rfc3339()
        ],
    )?;
    Ok(())
}

/// Applies one event to the projections during replay.
///
/// Returns whether the event changed anything. Events that carry no projection (script
/// timing, verification progress) are legitimately inert here — they exist for audit and
/// metrics, not for current state.
fn project(tx: &Transaction<'_>, envelope: &EventEnvelope) -> Result<bool, StateError> {
    match &envelope.event {
        PearlEvent::TaskCreated {
            task_id,
            task_type,
            precision_class,
            quality,
            plan,
        } => {
            // The event carries the complete quality contract and the declared plan, so
            // replay reconstructs both exactly rather than guessing. See ADR-0001, ADR-0002
            // and the replay equivalence test.
            tx.execute(
                "INSERT OR REPLACE INTO tasks
                    (task_id, trace_id, task_type, state, precision_class,
                     exactness_required, deterministic_generation, deterministic_verification,
                     created_at, updated_at, attempt_count, last_reason, plan)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,0,NULL,?10)",
                params![
                    task_id.as_str(),
                    envelope.trace_id.to_string(),
                    task_type,
                    TaskState::Created.as_str(),
                    precision_class.map(|p| p.as_str().to_string()),
                    quality.exactness_required as i32,
                    quality.deterministic_generation as i32,
                    quality.deterministic_verification as i32,
                    envelope.occurred_at.to_rfc3339(),
                    encode_plan(plan)?,
                ],
            )?;
            Ok(true)
        }
        PearlEvent::TaskStateChanged {
            task_id,
            to,
            reason,
            ..
        } => {
            tx.execute(
                "UPDATE tasks SET state = ?1, updated_at = ?2, last_reason = ?3 WHERE task_id = ?4",
                params![
                    to.as_str(),
                    envelope.occurred_at.to_rfc3339(),
                    reason.as_deref(),
                    task_id.as_str()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::RunStarted {
            task_id,
            run_id,
            config_revision,
            config_hash,
        } => {
            tx.execute(
                "INSERT OR REPLACE INTO runs
                    (run_id, task_id, trace_id, started_at, ended_at, config_revision, config_hash, outcome)
                 VALUES (?1,?2,?3,?4,NULL,?5,?6,NULL)",
                params![
                    run_id.to_string(),
                    task_id.as_str(),
                    envelope.trace_id.to_string(),
                    envelope.occurred_at.to_rfc3339(),
                    config_revision,
                    config_hash,
                ],
            )?;
            Ok(true)
        }
        PearlEvent::RunEnded {
            run_id, outcome, ..
        } => {
            tx.execute(
                "UPDATE runs SET ended_at = ?1, outcome = ?2 WHERE run_id = ?3",
                params![
                    envelope.occurred_at.to_rfc3339(),
                    outcome.as_str(),
                    run_id.to_string()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::AttemptStarted {
            run_id,
            attempt_id,
            attempt_number,
            ..
        } => {
            tx.execute(
                "INSERT OR REPLACE INTO attempts
                    (attempt_id, run_id, attempt_number, started_at, ended_at, outcome, exit_reason)
                 VALUES (?1,?2,?3,?4,NULL,NULL,NULL)",
                params![
                    attempt_id.to_string(),
                    run_id.to_string(),
                    attempt_number,
                    envelope.occurred_at.to_rfc3339(),
                ],
            )?;
            // The counter is derived from attempt.started events, so replay reproduces it
            // rather than trusting a stored number.
            tx.execute(
                "UPDATE tasks SET attempt_count = attempt_count + 1, updated_at = ?1
                 WHERE task_id = (SELECT task_id FROM runs WHERE run_id = ?2)",
                params![envelope.occurred_at.to_rfc3339(), run_id.to_string()],
            )?;
            Ok(true)
        }
        PearlEvent::AttemptEnded {
            attempt_id,
            outcome,
            exit_reason,
            ..
        } => {
            tx.execute(
                "UPDATE attempts SET ended_at = ?1, outcome = ?2, exit_reason = ?3
                 WHERE attempt_id = ?4",
                params![
                    envelope.occurred_at.to_rfc3339(),
                    outcome.as_str(),
                    exit_reason.as_deref(),
                    attempt_id.to_string()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::LeaseAcquired {
            task_id,
            lease_id,
            worker_id,
            leased_until,
        } => {
            tx.execute(
                "INSERT OR REPLACE INTO leases
                    (lease_id, task_id, worker_id, acquired_at, leased_until, last_heartbeat, released_at)
                 VALUES (?1,?2,?3,?4,?5,?4,NULL)",
                params![
                    lease_id.to_string(),
                    task_id.as_str(),
                    worker_id.as_str(),
                    envelope.occurred_at.to_rfc3339(),
                    leased_until.to_rfc3339(),
                ],
            )?;
            Ok(true)
        }
        PearlEvent::LeaseRenewed {
            lease_id,
            leased_until,
        } => {
            tx.execute(
                "UPDATE leases SET leased_until = ?1, last_heartbeat = ?2 WHERE lease_id = ?3",
                params![
                    leased_until.to_rfc3339(),
                    envelope.occurred_at.to_rfc3339(),
                    lease_id.to_string()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::LeaseExpired { lease_id, .. } | PearlEvent::LeaseReleased { lease_id, .. } => {
            tx.execute(
                "UPDATE leases SET released_at = ?1 WHERE lease_id = ?2 AND released_at IS NULL",
                params![envelope.occurred_at.to_rfc3339(), lease_id.to_string()],
            )?;
            Ok(true)
        }
        PearlEvent::EffectRequested {
            effect,
            idempotency_key,
        } => {
            tx.execute(
                "INSERT OR IGNORE INTO effects (idempotency_key, effect, requested_at, committed_at)
                 VALUES (?1,?2,?3,NULL)",
                params![
                    idempotency_key.as_str(),
                    effect,
                    envelope.occurred_at.to_rfc3339()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::EffectCommitted {
            idempotency_key, ..
        } => {
            tx.execute(
                "UPDATE effects SET committed_at = ?1 WHERE idempotency_key = ?2",
                params![envelope.occurred_at.to_rfc3339(), idempotency_key.as_str()],
            )?;
            Ok(true)
        }
        PearlEvent::EvidenceStored {
            task_id,
            evidence_type,
            producer,
            passed,
        } => {
            tx.execute(
                "INSERT INTO evidence (task_id, evidence_type, producer, passed, recorded_at)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    task_id.as_str(),
                    evidence_type,
                    producer,
                    *passed as i32,
                    envelope.occurred_at.to_rfc3339()
                ],
            )?;
            Ok(true)
        }
        PearlEvent::CheckpointCommitted {
            checkpoint_id,
            step_id,
        } => {
            // §41: resume reads the latest checkpoint, so a rebuild that dropped them would
            // silently restart completed work. The payload is not in the event, so a replayed
            // checkpoint records that the step completed but not its resume state — enough to
            // skip it, which is what resume needs.
            let Some(task_id) = envelope
                .event
                .task_id()
                .cloned()
                .or_else(|| task_id_for_trace(tx, envelope.trace_id).ok().flatten())
            else {
                return Ok(false);
            };
            insert_checkpoint(
                tx,
                *checkpoint_id,
                &task_id,
                step_id,
                None,
                envelope.occurred_at,
            )?;
            Ok(true)
        }
        // Inert by design: audit and metrics only.
        PearlEvent::TaskPlanned { .. }
        | PearlEvent::TaskCompleted { .. }
        | PearlEvent::ScriptStarted { .. }
        | PearlEvent::ScriptCompleted { .. }
        | PearlEvent::VerificationStarted { .. }
        | PearlEvent::VerificationPassed { .. }
        | PearlEvent::VerificationFailed { .. }
        | PearlEvent::EffectDeduplicated { .. } => Ok(false),
    }
}

// ------------------------------------------------------------- row mappers

/// The task a trace belongs to.
///
/// Some events are correlated only by `trace_id` — a checkpoint knows which step it follows,
/// not which task owns it. Replay resolves the owner from the already-projected tasks table,
/// which is safe because `task.created` is always the first event on a trace.
fn task_id_for_trace(
    tx: &Transaction<'_>,
    trace_id: TraceId,
) -> Result<Option<TaskId>, StateError> {
    let found: Option<String> = tx
        .query_row(
            "SELECT task_id FROM tasks WHERE trace_id = ?1 LIMIT 1",
            params![trace_id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match found {
        Some(id) => Some(TaskId::parse(id).map_err(|e| StateError::Projection {
            detail: e.to_string(),
        })?),
        None => None,
    })
}

fn insert_checkpoint(
    tx: &Transaction<'_>,
    checkpoint_id: CheckpointId,
    task_id: &TaskId,
    step_id: &str,
    payload: Option<&str>,
    now: DateTime<Utc>,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT OR REPLACE INTO checkpoints (checkpoint_id, task_id, label, payload, created_at)
         VALUES (?1,?2,?3,?4,?5)",
        params![
            checkpoint_id.to_string(),
            task_id.as_str(),
            step_id,
            payload,
            now.to_rfc3339()
        ],
    )?;
    Ok(())
}

fn row_to_step(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepRecord> {
    Ok(StepRecord {
        step_id: row.get(0)?,
        run_id: RunId::parse(&row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        step_number: row.get::<_, i64>(2)? as u32,
        description: row.get(3)?,
        status: row.get(4)?,
        started_at: parse_time_opt(row, 5)?,
        completed_at: parse_time_opt(row, 6)?,
    })
}

fn row_to_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckpointRecord> {
    Ok(CheckpointRecord {
        checkpoint_id: row.get(0)?,
        task_id: TaskId::parse(row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        label: row.get(2)?,
        payload: row.get(3)?,
        created_at: parse_time(row, 4)?,
    })
}

fn row_to_verification(row: &rusqlite::Row<'_>) -> rusqlite::Result<VerificationResult> {
    Ok(VerificationResult {
        task_id: TaskId::parse(row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        verifier_id: row.get(1)?,
        passed: row.get::<_, i32>(2)? != 0,
        detail: row.get(3)?,
        verified_at: parse_time(row, 4)?,
    })
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Artifact> {
    Ok(Artifact {
        artifact_id: row.get(0)?,
        task_id: TaskId::parse(row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        artifact_type: row.get(2)?,
        path: row.get(3)?,
        sha256: row.get(4)?,
        size_bytes: row.get::<_, i64>(5)? as u64,
        created_at: parse_time(row, 6)?,
    })
}

fn row_to_health(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuntimeHealth> {
    Ok(RuntimeHealth {
        subsystem: row.get(0)?,
        status: row.get(1)?,
        detail: row.get(2)?,
        recorded_at: parse_time(row, 3)?,
    })
}

fn row_to_policy_decision(row: &rusqlite::Row<'_>) -> rusqlite::Result<PolicyDecision> {
    let task_id: Option<String> = row.get(0)?;
    Ok(PolicyDecision {
        task_id: match task_id {
            Some(id) => Some(TaskId::parse(id).map_err(to_sqlite_err)?),
            None => None,
        },
        decision_type: row.get(1)?,
        outcome: row.get(2)?,
        reason: row.get(3)?,
        decided_at: parse_time(row, 4)?,
    })
}

fn row_to_config_revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConfigRevision> {
    Ok(ConfigRevision {
        revision_id: row.get(0)?,
        config_hash: row.get(1)?,
        source: row.get(2)?,
        applied_at: parse_time(row, 3)?,
        payload: row.get(4)?,
    })
}

fn row_to_evidence(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvidenceRecord> {
    Ok(EvidenceRecord {
        task_id: TaskId::parse(row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        evidence_type: row.get(1)?,
        producer: row.get(2)?,
        passed: row.get::<_, i32>(3)? != 0,
        recorded_at: parse_time(row, 4)?,
    })
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let precision: Option<String> = row.get(4)?;
    let plan: Option<String> = row.get(12)?;
    let plan = match plan {
        Some(json) => serde_json::from_str(&json).map_err(to_sqlite_err)?,
        None => TaskPlan::empty(),
    };
    Ok(TaskRecord {
        task_id: TaskId::parse(row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        trace_id: TraceId::parse(&row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        task_type: row.get(2)?,
        state: TaskState::parse(&row.get::<_, String>(3)?)
            .ok_or_else(|| to_sqlite_err("unknown task state"))?,
        precision_class: precision.and_then(|p| match p.as_str() {
            "p0" => Some(PrecisionClass::P0),
            "p1" => Some(PrecisionClass::P1),
            "p2" => Some(PrecisionClass::P2),
            "p3" => Some(PrecisionClass::P3),
            _ => None,
        }),
        plan,
        quality: QualitySpec {
            exactness_required: row.get::<_, i32>(5)? != 0,
            deterministic_generation: row.get::<_, i32>(6)? != 0,
            deterministic_verification: row.get::<_, i32>(7)? != 0,
        },
        created_at: parse_time(row, 8)?,
        updated_at: parse_time(row, 9)?,
        attempt_count: row.get::<_, i64>(10)? as u32,
        last_reason: row.get(11)?,
    })
}

fn row_to_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    Ok(RunRecord {
        run_id: RunId::parse(&row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        task_id: TaskId::parse(row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        trace_id: TraceId::parse(&row.get::<_, String>(2)?).map_err(to_sqlite_err)?,
        started_at: parse_time(row, 3)?,
        ended_at: parse_time_opt(row, 4)?,
        config_revision: row.get(5)?,
        config_hash: row.get(6)?,
        outcome: row.get(7)?,
    })
}

fn row_to_attempt(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptRecord> {
    Ok(AttemptRecord {
        attempt_id: AttemptId::parse(&row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        run_id: RunId::parse(&row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        attempt_number: row.get::<_, i64>(2)? as u32,
        started_at: parse_time(row, 3)?,
        ended_at: parse_time_opt(row, 4)?,
        outcome: row.get(5)?,
        exit_reason: row.get(6)?,
    })
}

fn row_to_lease(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRecord> {
    Ok(LeaseRecord {
        lease_id: LeaseId::parse(&row.get::<_, String>(0)?).map_err(to_sqlite_err)?,
        task_id: TaskId::parse(row.get::<_, String>(1)?).map_err(to_sqlite_err)?,
        worker_id: WorkerId::new(row.get::<_, String>(2)?),
        acquired_at: parse_time(row, 3)?,
        leased_until: parse_time(row, 4)?,
        last_heartbeat: parse_time(row, 5)?,
        released_at: parse_time_opt(row, 6)?,
    })
}

fn row_to_effect(row: &rusqlite::Row<'_>) -> rusqlite::Result<EffectRecord> {
    Ok(EffectRecord {
        idempotency_key: row.get(0)?,
        effect: row.get(1)?,
        requested_at: parse_time(row, 2)?,
        committed_at: parse_time_opt(row, 3)?,
    })
}

fn parse_time(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let raw: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(to_sqlite_err)
}

fn parse_time_opt(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = row.get(idx)?;
    match raw {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(to_sqlite_err),
    }
}

fn to_sqlite_err<E: std::fmt::Display>(e: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.to_string().into())
}

/// State store failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("task '{task_id}' already exists")]
    TaskAlreadyExists { task_id: String },
    /// The declared plan could not be serialised for storage.
    #[error("failed to encode the task plan: {detail}")]
    PlanEncoding { detail: String },
    /// A stored value could not be projected back into its type.
    #[error("failed to project a stored value: {detail}")]
    Projection { detail: String },
    #[error("task '{task_id}' not found")]
    TaskNotFound { task_id: String },
    #[error("run '{run_id}' not found")]
    RunNotFound { run_id: String },
    #[error("attempt '{attempt_id}' not found")]
    AttemptNotFound { attempt_id: String },
    #[error("lease '{lease_id}' not found")]
    LeaseNotFound { lease_id: String },
    #[error("lease '{lease_id}' was already released")]
    LeaseAlreadyReleased { lease_id: String },
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("task '{task_id}' demands exactness but has no deterministic verification; Article 2 forbids auto-completion")]
    ExactnessGateBlocked { task_id: String },
    #[error("Article 10: a run must record config_revision and config_hash")]
    MissingConfigRevision,
    #[error("invalid task id: {0}")]
    InvalidTaskId(#[from] pearl_core::InvalidTaskId),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
