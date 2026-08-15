//! The event vocabulary — 系統開發需求書 §42.
//!
//! Events are the source of truth (ADR-0001). Two consequences shape this module:
//!
//! 1. The vocabulary is closed. An event is one of a fixed set, so a replay can be
//!    exhaustively matched and the compiler will flag any handler that forgets a case.
//! 2. History is never rewritten. A correction is expressed as a *new* event, which is
//!    why there is no `EventUpdate` type anywhere in this crate.

use chrono::{DateTime, Utc};
use pearl_core::{
    AttemptId, CheckpointId, EventId, IdempotencyKey, LeaseId, PrecisionClass, QualitySpec, RunId,
    TaskId, TaskPlan, TaskState, TraceId, WorkerId, EVENT_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

/// One thing that happened.
///
/// Internally tagged on `type` so the persisted payload is self-describing: the ledger
/// row can be deserialized without consulting the `event_type` column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PearlEvent {
    #[serde(rename = "task.created")]
    TaskCreated {
        task_id: TaskId,
        task_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        precision_class: Option<PrecisionClass>,
        /// The complete quality contract, not just `exactness_required`.
        ///
        /// ADR-0001 requires the ledger to be sufficient to rebuild state. Recording
        /// only the exactness flag made this event lossy: a replay could not tell
        /// whether verification was deterministic, so rebuilt tasks silently differed
        /// from live ones. The replay equivalence test caught it.
        quality: QualitySpec,
        /// What the submitter declared: which capability to run, and how to verify it.
        ///
        /// Same reasoning as `quality`. A plan that lived only in the submitted YAML could
        /// not be recovered by replay, and a worker reading a rebuilt task would run it
        /// with no assurance at all — silently turning a verified task into an unverified
        /// one. Defaults to empty so a v1 ledger still replays.
        #[serde(default, skip_serializing_if = "TaskPlan::is_empty")]
        plan: TaskPlan,
    },

    #[serde(rename = "task.planned")]
    TaskPlanned { task_id: TaskId, step_count: u32 },

    #[serde(rename = "task.state_changed")]
    TaskStateChanged {
        task_id: TaskId,
        from: TaskState,
        to: TaskState,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    #[serde(rename = "task.completed")]
    TaskCompleted {
        task_id: TaskId,
        final_state: TaskState,
    },

    #[serde(rename = "run.started")]
    RunStarted {
        task_id: TaskId,
        run_id: RunId,
        /// Article 10: without these a run is not reproducible.
        config_revision: String,
        config_hash: String,
    },

    #[serde(rename = "run.ended")]
    RunEnded {
        task_id: TaskId,
        run_id: RunId,
        outcome: RunOutcome,
    },

    /// An attempt within a run.
    ///
    /// Carries `task_id` as well as `run_id` even though the run determines the task: the
    /// ledger indexes on `task_id`, so an event without it cannot be found by the question
    /// operators actually ask — "what happened to this task?".
    #[serde(rename = "attempt.started")]
    AttemptStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        run_id: RunId,
        attempt_id: AttemptId,
        attempt_number: u32,
    },

    #[serde(rename = "attempt.ended")]
    AttemptEnded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        run_id: RunId,
        attempt_id: AttemptId,
        outcome: RunOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_reason: Option<String>,
    },

    #[serde(rename = "lease.acquired")]
    LeaseAcquired {
        task_id: TaskId,
        lease_id: LeaseId,
        worker_id: WorkerId,
        leased_until: DateTime<Utc>,
    },

    #[serde(rename = "lease.renewed")]
    LeaseRenewed {
        lease_id: LeaseId,
        leased_until: DateTime<Utc>,
    },

    /// Emitted by the reaper, not by the dead worker — a crashed worker cannot report
    /// its own death, which is the whole reason leases exist (§34).
    #[serde(rename = "lease.expired")]
    LeaseExpired {
        task_id: TaskId,
        lease_id: LeaseId,
        worker_id: WorkerId,
    },

    #[serde(rename = "lease.released")]
    LeaseReleased { task_id: TaskId, lease_id: LeaseId },

    #[serde(rename = "script.started")]
    ScriptStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        capability_id: String,
        runtime: String,
    },

    #[serde(rename = "script.completed")]
    ScriptCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        capability_id: String,
        /// `-1` when the process never reported a code of its own: killed, signalled or
        /// timed out. Recording that plainly beats inventing a plausible code.
        exit_code: i32,
        duration_ms: u64,
    },

    #[serde(rename = "verification.started")]
    VerificationStarted {
        task_id: TaskId,
        verifier_id: String,
    },

    #[serde(rename = "verification.passed")]
    VerificationPassed {
        task_id: TaskId,
        verifier_id: String,
        check_count: u32,
    },

    #[serde(rename = "verification.failed")]
    VerificationFailed {
        task_id: TaskId,
        verifier_id: String,
        reason: String,
    },

    #[serde(rename = "effect.requested")]
    EffectRequested {
        effect: String,
        idempotency_key: IdempotencyKey,
    },

    #[serde(rename = "effect.committed")]
    EffectCommitted {
        effect: String,
        idempotency_key: IdempotencyKey,
    },

    /// The retry-safety proof: a second request for the same key did *not* re-run.
    #[serde(rename = "effect.deduplicated")]
    EffectDeduplicated {
        effect: String,
        idempotency_key: IdempotencyKey,
    },

    #[serde(rename = "checkpoint.committed")]
    CheckpointCommitted {
        checkpoint_id: CheckpointId,
        step_id: String,
    },

    #[serde(rename = "evidence.stored")]
    EvidenceStored {
        task_id: TaskId,
        evidence_type: String,
        producer: String,
        passed: bool,
    },
}

impl PearlEvent {
    /// The stable wire name, used for the indexed `event_type` column.
    pub fn event_type(&self) -> &'static str {
        match self {
            PearlEvent::TaskCreated { .. } => "task.created",
            PearlEvent::TaskPlanned { .. } => "task.planned",
            PearlEvent::TaskStateChanged { .. } => "task.state_changed",
            PearlEvent::TaskCompleted { .. } => "task.completed",
            PearlEvent::RunStarted { .. } => "run.started",
            PearlEvent::RunEnded { .. } => "run.ended",
            PearlEvent::AttemptStarted { .. } => "attempt.started",
            PearlEvent::AttemptEnded { .. } => "attempt.ended",
            PearlEvent::LeaseAcquired { .. } => "lease.acquired",
            PearlEvent::LeaseRenewed { .. } => "lease.renewed",
            PearlEvent::LeaseExpired { .. } => "lease.expired",
            PearlEvent::LeaseReleased { .. } => "lease.released",
            PearlEvent::ScriptStarted { .. } => "script.started",
            PearlEvent::ScriptCompleted { .. } => "script.completed",
            PearlEvent::VerificationStarted { .. } => "verification.started",
            PearlEvent::VerificationPassed { .. } => "verification.passed",
            PearlEvent::VerificationFailed { .. } => "verification.failed",
            PearlEvent::EffectRequested { .. } => "effect.requested",
            PearlEvent::EffectCommitted { .. } => "effect.committed",
            PearlEvent::EffectDeduplicated { .. } => "effect.deduplicated",
            PearlEvent::CheckpointCommitted { .. } => "checkpoint.committed",
            PearlEvent::EvidenceStored { .. } => "evidence.stored",
        }
    }

    /// The task this event concerns, when it concerns one.
    ///
    /// Some events (script, checkpoint) are scoped to a run rather than a task, so this
    /// is genuinely optional; correlation for those goes through `trace_id`.
    pub fn task_id(&self) -> Option<&TaskId> {
        match self {
            PearlEvent::TaskCreated { task_id, .. }
            | PearlEvent::TaskPlanned { task_id, .. }
            | PearlEvent::TaskStateChanged { task_id, .. }
            | PearlEvent::TaskCompleted { task_id, .. }
            | PearlEvent::RunStarted { task_id, .. }
            | PearlEvent::RunEnded { task_id, .. }
            | PearlEvent::LeaseAcquired { task_id, .. }
            | PearlEvent::LeaseExpired { task_id, .. }
            | PearlEvent::LeaseReleased { task_id, .. }
            | PearlEvent::VerificationStarted { task_id, .. }
            | PearlEvent::VerificationPassed { task_id, .. }
            | PearlEvent::VerificationFailed { task_id, .. }
            | PearlEvent::EvidenceStored { task_id, .. } => Some(task_id),
            // Optional on these: they are scoped to a run or a capability, and the producer
            // may not know the task. When it does know, correlation should not be lost.
            PearlEvent::AttemptStarted { task_id, .. }
            | PearlEvent::AttemptEnded { task_id, .. }
            | PearlEvent::ScriptStarted { task_id, .. }
            | PearlEvent::ScriptCompleted { task_id, .. } => task_id.as_ref(),
            _ => None,
        }
    }

    pub fn run_id(&self) -> Option<&RunId> {
        match self {
            PearlEvent::RunStarted { run_id, .. }
            | PearlEvent::RunEnded { run_id, .. }
            | PearlEvent::AttemptStarted { run_id, .. }
            | PearlEvent::AttemptEnded { run_id, .. } => Some(run_id),
            _ => None,
        }
    }
}

/// How a run or attempt finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Failure,
    Timeout,
    Cancelled,
    /// The worker disappeared; the reaper concluded this.
    Abandoned,
}

impl RunOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunOutcome::Success => "success",
            RunOutcome::Failure => "failure",
            RunOutcome::Timeout => "timeout",
            RunOutcome::Cancelled => "cancelled",
            RunOutcome::Abandoned => "abandoned",
        }
    }
}

/// An event plus the correlation metadata every ledger record carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub schema_version: u32,
    pub occurred_at: DateTime<Utc>,
    /// Correlates every event of one logical task across runs and attempts.
    pub trace_id: TraceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<AttemptId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<WorkerId>,
    pub event: PearlEvent,
}

impl EventEnvelope {
    /// Seals an event for the ledger.
    ///
    /// The id is generated here rather than by the caller so that ledger order is always
    /// UUIDv7 generation order — a caller supplying its own id could break the ordering
    /// invariant ADR-0001 depends on.
    pub fn new(trace_id: TraceId, occurred_at: DateTime<Utc>, event: PearlEvent) -> Self {
        Self {
            id: EventId::new(),
            schema_version: EVENT_SCHEMA_VERSION,
            occurred_at,
            trace_id,
            attempt_id: None,
            worker_id: None,
            event,
        }
    }

    pub fn with_attempt(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }

    pub fn with_worker(mut self, worker_id: WorkerId) -> Self {
        self.worker_id = Some(worker_id);
        self
    }

    pub fn event_type(&self) -> &'static str {
        self.event.event_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> DateTime<Utc> {
        DateTime::from_timestamp(1_786_838_400, 0).unwrap()
    }

    fn task() -> TaskId {
        TaskId::parse("daily.digest").unwrap()
    }

    #[test]
    fn every_event_round_trips_through_json() {
        let events = vec![
            PearlEvent::TaskCreated {
                task_id: task(),
                task_type: "digest".into(),
                precision_class: Some(PrecisionClass::P1),
                quality: QualitySpec::mechanical(),
                plan: TaskPlan {
                    capability: Some("script.task-score".into()),
                    assurance: vec![pearl_core::AssuranceStep::script("verifier.task-result")],
                    timeout_seconds: Some(30),
                },
            },
            PearlEvent::TaskStateChanged {
                task_id: task(),
                from: TaskState::Ready,
                to: TaskState::Leased,
                reason: None,
            },
            PearlEvent::RunStarted {
                task_id: task(),
                run_id: RunId::new(),
                config_revision: "system@builtin".into(),
                config_hash: "abc".into(),
            },
            PearlEvent::EffectDeduplicated {
                effect: "ntfy".into(),
                idempotency_key: IdempotencyKey::parse("ntfy:digest:2026-08-15").unwrap(),
            },
            PearlEvent::LeaseExpired {
                task_id: task(),
                lease_id: LeaseId::new(),
                worker_id: WorkerId::new("box:1"),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let back: PearlEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, back, "round trip failed for {}", event.event_type());
        }
    }

    #[test]
    fn payload_is_self_describing() {
        // The persisted payload alone must be enough to reconstruct the event, so the
        // ledger never depends on its own event_type column being correct.
        let event = PearlEvent::TaskPlanned {
            task_id: task(),
            step_count: 3,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "task.planned");
    }

    #[test]
    fn event_type_matches_serialized_tag() {
        let event = PearlEvent::VerificationPassed {
            task_id: task(),
            verifier_id: "verifier.digest".into(),
            check_count: 4,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"].as_str(), Some(event.event_type()));
    }

    #[test]
    fn task_scoped_events_expose_their_task() {
        let event = PearlEvent::TaskCompleted {
            task_id: task(),
            final_state: TaskState::VerifiedSuccess,
        };
        assert_eq!(event.task_id(), Some(&task()));
    }

    #[test]
    fn a_run_scoped_event_always_has_a_run_and_may_have_a_task() {
        let run_id = RunId::new();
        let anonymous = PearlEvent::AttemptStarted {
            task_id: None,
            run_id,
            attempt_id: AttemptId::new(),
            attempt_number: 2,
        };
        assert_eq!(anonymous.task_id(), None);
        assert_eq!(anonymous.run_id(), Some(&run_id));

        // When the producer knows the task, correlation must not be lost: the ledger
        // indexes on task_id, and "what happened to this task?" is the question asked most.
        let correlated = PearlEvent::AttemptStarted {
            task_id: Some(task()),
            run_id,
            attempt_id: AttemptId::new(),
            attempt_number: 2,
        };
        assert_eq!(correlated.task_id(), Some(&task()));
        assert_eq!(correlated.run_id(), Some(&run_id));
    }

    #[test]
    fn script_events_carry_their_task_when_it_is_known() {
        let event = PearlEvent::ScriptCompleted {
            task_id: Some(task()),
            capability_id: "script.task-score".into(),
            exit_code: 0,
            duration_ms: 12,
        };
        assert_eq!(event.task_id(), Some(&task()));
        assert_eq!(event.event_type(), "script.completed");
    }

    #[test]
    fn envelope_stamps_the_current_schema_version() {
        let env = EventEnvelope::new(
            TraceId::new(),
            ts(),
            PearlEvent::TaskPlanned {
                task_id: task(),
                step_count: 1,
            },
        );
        assert_eq!(env.schema_version, EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn envelope_ids_are_unique_and_ordered() {
        let trace = TraceId::new();
        let mut ids = Vec::new();
        for _ in 0..20 {
            ids.push(
                EventEnvelope::new(
                    trace,
                    ts(),
                    PearlEvent::TaskPlanned {
                        task_id: task(),
                        step_count: 1,
                    },
                )
                .id,
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids must be unique");
        assert_eq!(ids, sorted, "ledger order must equal generation order");
    }

    #[test]
    fn optional_correlation_fields_are_omitted_when_absent() {
        let env = EventEnvelope::new(
            TraceId::new(),
            ts(),
            PearlEvent::TaskPlanned {
                task_id: task(),
                step_count: 1,
            },
        );
        let json = serde_json::to_value(&env).unwrap();
        assert!(json.get("attempt_id").is_none());
        assert!(json.get("worker_id").is_none());

        let with = env.with_worker(WorkerId::new("box:2"));
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["worker_id"], "box:2");
    }
}
