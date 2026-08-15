//! Materialized row types — 系統開発需求書 §43, §58.
//!
//! These are projections, not truth. Every field here is derivable from the event
//! ledger; the tables exist so that "which tasks are ready?" is an indexed query rather
//! than a full replay.

use chrono::{DateTime, Utc};
use pearl_core::{
    AttemptId, LeaseId, PrecisionClass, QualitySpec, RunId, TaskId, TaskState, TraceId, WorkerId,
};
use serde::{Deserialize, Serialize};

/// A durable unit of work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub trace_id: TraceId,
    pub task_type: String,
    pub state: TaskState,
    pub precision_class: Option<PrecisionClass>,
    pub quality: QualitySpec,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// How many times this task has been attempted across all runs.
    pub attempt_count: u32,
    /// Set when the task is `Blocked`, `Failed` or `Unverified`, so an operator does not
    /// have to replay the ledger to learn why.
    pub last_reason: Option<String>,
}

/// One execution of a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub trace_id: TraceId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// Article 10: a run without these is not reproducible.
    pub config_revision: String,
    pub config_hash: String,
    pub outcome: Option<String>,
}

/// One try within a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: AttemptId,
    pub run_id: RunId,
    pub attempt_number: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
    pub exit_reason: Option<String>,
}

/// A worker's claim on a task — §34.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub lease_id: LeaseId,
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    pub acquired_at: DateTime<Utc>,
    pub leased_until: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

impl LeaseRecord {
    /// Whether this lease has lapsed as of `now`.
    ///
    /// Expiry is judged on `leased_until` rather than on heartbeat age: the heartbeat
    /// extends the deadline, so a single comparison covers both "worker died" and
    /// "worker hung without renewing".
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.released_at.is_none() && now > self.leased_until
    }

    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.released_at.is_none() && now <= self.leased_until
    }
}

/// A recorded external effect, keyed for deduplication — Article 5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub idempotency_key: String,
    pub effect: String,
    pub requested_at: DateTime<Utc>,
    pub committed_at: Option<DateTime<Utc>>,
}

impl EffectRecord {
    pub fn is_committed(&self) -> bool {
        self.committed_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn lease(until: i64, released: Option<i64>) -> LeaseRecord {
        LeaseRecord {
            lease_id: LeaseId::new(),
            task_id: TaskId::parse("t").unwrap(),
            worker_id: WorkerId::new("box:1"),
            acquired_at: at(1000),
            leased_until: at(until),
            last_heartbeat: at(1000),
            released_at: released.map(at),
        }
    }

    #[test]
    fn lease_is_active_before_its_deadline() {
        let l = lease(2000, None);
        assert!(l.is_active(at(1999)));
        assert!(!l.is_expired(at(1999)));
    }

    #[test]
    fn lease_expires_strictly_after_the_deadline() {
        let l = lease(2000, None);
        // At exactly the deadline the lease still holds; the holder gets the full window.
        assert!(l.is_active(at(2000)));
        assert!(!l.is_expired(at(2000)));
        assert!(l.is_expired(at(2001)));
        assert!(!l.is_active(at(2001)));
    }

    #[test]
    fn released_lease_never_expires() {
        // A released lease was handed back deliberately; the reaper must not also
        // "reclaim" it, or the same task could be queued twice.
        let l = lease(2000, Some(1500));
        assert!(!l.is_expired(at(9999)));
        assert!(!l.is_active(at(1600)));
    }

    #[test]
    fn effect_commitment_is_observable() {
        let mut e = EffectRecord {
            idempotency_key: "ntfy:digest:2026-08-15".into(),
            effect: "ntfy".into(),
            requested_at: at(1000),
            committed_at: None,
        };
        assert!(!e.is_committed());
        e.committed_at = Some(at(1001));
        assert!(e.is_committed());
    }
}
