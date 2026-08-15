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

use crate::records::{AttemptRecord, EffectRecord, LeaseRecord, RunRecord, TaskRecord};
use chrono::{DateTime, Utc};
use pearl_core::{
    AttemptId, EvidenceSet, ExactnessGate, IdempotencyKey, LeaseId, PrecisionClass, QualitySpec,
    RunId, TaskId, TaskState, TraceId, TransitionError, WorkerId,
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
    last_reason     TEXT
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
const PROJECTION_TABLES: [&str; 14] = [
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
];

/// A new task to persist.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSubmission {
    pub task_id: TaskId,
    pub task_type: String,
    pub precision_class: Option<PrecisionClass>,
    pub quality: QualitySpec,
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
                        created_at, updated_at, attempt_count, last_reason
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
                    created_at, updated_at, attempt_count, last_reason
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
                    created_at, updated_at, attempt_count, last_reason
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
        let (_, trace_id) = self.run_owner(run_id)?;

        let envelope = EventEnvelope::new(
            trace_id,
            now,
            PearlEvent::AttemptEnded {
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
             created_at, updated_at, attempt_count, last_reason)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,NULL)",
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
        ],
    )?;
    Ok(())
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
        } => {
            // The event carries the complete quality contract, so replay reconstructs it
            // exactly rather than guessing. See ADR-0001 and the replay equivalence test.
            tx.execute(
                "INSERT OR REPLACE INTO tasks
                    (task_id, trace_id, task_type, state, precision_class,
                     exactness_required, deterministic_generation, deterministic_verification,
                     created_at, updated_at, attempt_count, last_reason)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9,0,NULL)",
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
        // Inert by design: audit and metrics only.
        PearlEvent::TaskPlanned { .. }
        | PearlEvent::TaskCompleted { .. }
        | PearlEvent::ScriptStarted { .. }
        | PearlEvent::ScriptCompleted { .. }
        | PearlEvent::VerificationStarted { .. }
        | PearlEvent::VerificationPassed { .. }
        | PearlEvent::VerificationFailed { .. }
        | PearlEvent::EffectDeduplicated { .. }
        | PearlEvent::CheckpointCommitted { .. } => Ok(false),
    }
}

// ------------------------------------------------------------- row mappers

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    let precision: Option<String> = row.get(4)?;
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
