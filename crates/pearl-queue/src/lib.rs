//! # pearl-queue
//!
//! The durable work queue — 系統開發需求書 §33, §60.
//!
//! The queue is not a separate data structure. It is a *view* over the task table:
//! "the queue" is every task in `READY`, ordered oldest-first. Keeping it a view rather
//! than a list is what makes it durable for free — there is no queue state that could
//! disagree with task state after a crash.
//!
//! Responsibilities:
//!
//! - hand the next claimable task to a worker (via `pearl-lease`),
//! - promote `RETRY_WAIT` tasks back to `READY` once their backoff has elapsed,
//! - dead-letter tasks that have exhausted their attempts.

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::{Clock, RuntimeProfile, TaskId, TaskState, WorkerId};
use pearl_lease::{LeaseError, LeaseManager};
use pearl_state::{LeaseRecord, StateError, StateStore, TaskRecord};

/// Retry policy — Article 3 puts this in code, never in a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// Delay before the first retry.
    pub base_backoff: TimeDelta,
    /// Ceiling, so exponential growth cannot park a task for a week.
    pub max_backoff: TimeDelta,
}

impl RetryPolicy {
    pub fn new(
        max_attempts: u32,
        base_backoff: TimeDelta,
        max_backoff: TimeDelta,
    ) -> Result<Self, QueueError> {
        if max_attempts == 0 {
            return Err(QueueError::InvalidPolicy {
                detail: "max_attempts must be at least 1".into(),
            });
        }
        if base_backoff < TimeDelta::zero() || max_backoff < base_backoff {
            return Err(QueueError::InvalidPolicy {
                detail: "backoff must be non-negative and max_backoff >= base_backoff".into(),
            });
        }
        Ok(Self {
            max_attempts,
            base_backoff,
            max_backoff,
        })
    }

    /// 3 attempts, 30s base, 5min ceiling.
    pub fn default_policy() -> Self {
        Self {
            max_attempts: 3,
            base_backoff: TimeDelta::try_seconds(30).expect("valid"),
            max_backoff: TimeDelta::try_seconds(300).expect("valid"),
        }
    }

    /// Backoff before attempt `attempt` (1-based), doubling and clamped.
    ///
    /// Deterministic — no jitter. Article 1: a test must be able to predict exactly when
    /// a task becomes eligible again. Jitter belongs to a scheduler with many workers,
    /// and can be added there without making this function unpredictable.
    pub fn backoff_for(&self, attempt: u32) -> TimeDelta {
        if attempt <= 1 {
            return self.base_backoff;
        }
        let shift = (attempt - 1).min(16);
        let multiplier = 1_i64 << shift;
        let scaled = self
            .base_backoff
            .num_milliseconds()
            .saturating_mul(multiplier);
        let candidate = TimeDelta::try_milliseconds(scaled).unwrap_or(self.max_backoff);
        candidate.min(self.max_backoff)
    }

    /// Whether another attempt is permitted.
    pub fn permits_attempt(&self, attempts_so_far: u32) -> bool {
        attempts_so_far < self.max_attempts
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// A durable queue over the task table.
pub struct WorkQueue<C: Clock> {
    policy: RetryPolicy,
    profile: RuntimeProfile,
    clock: C,
}

impl<C: Clock + Clone> WorkQueue<C> {
    pub fn new(policy: RetryPolicy, profile: RuntimeProfile, clock: C) -> Self {
        Self {
            policy,
            profile,
            clock,
        }
    }

    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    pub fn profile(&self) -> RuntimeProfile {
        self.profile
    }

    /// Switches runtime profile, e.g. on health degradation.
    pub fn set_profile(&mut self, profile: RuntimeProfile) {
        self.profile = profile;
    }

    /// How deep the queue is right now.
    pub fn depth(&self, store: &StateStore) -> Result<u64, QueueError> {
        Ok(store.count_by_state(TaskState::Ready)?)
    }

    /// Tasks waiting to be claimed, oldest first.
    pub fn peek(&self, store: &StateStore, limit: usize) -> Result<Vec<TaskRecord>, QueueError> {
        let mut ready = store.list_by_state(TaskState::Ready)?;
        ready.truncate(limit);
        Ok(ready)
    }

    /// Claims the next available task for a worker.
    ///
    /// Returns `None` when the queue is empty. Claim contention is resolved by the lease
    /// layer: if another worker took the task between the read and the claim, this skips
    /// it and tries the next one rather than failing the whole call.
    pub fn claim_next(
        &self,
        store: &mut StateStore,
        leases: &LeaseManager<C>,
        worker_id: &WorkerId,
    ) -> Result<Option<Claim>, QueueError> {
        let candidates = store.list_by_state(TaskState::Ready)?;

        for candidate in candidates {
            match leases.claim(store, &candidate.task_id, worker_id) {
                Ok(lease) => {
                    let task = store.get_task(&candidate.task_id)?.ok_or_else(|| {
                        QueueError::TaskVanished {
                            task_id: candidate.task_id.to_string(),
                        }
                    })?;
                    return Ok(Some(Claim { task, lease }));
                }
                // Lost the race, or the task moved on. Both mean "try the next one".
                Err(LeaseError::AlreadyLeased { .. }) | Err(LeaseError::NotClaimable { .. }) => {
                    continue
                }
                Err(e) => return Err(QueueError::Lease(e)),
            }
        }
        Ok(None)
    }

    /// Records a failed attempt and decides what happens next.
    ///
    /// This is the decision Article 3 keeps out of prompts: whether to retry is a function
    /// of attempt count and policy, nothing else.
    pub fn record_failure(
        &self,
        store: &mut StateStore,
        task_id: &TaskId,
        reason: &str,
    ) -> Result<FailureVerdict, QueueError> {
        let now = self.clock.now();
        let task = store
            .get_task(task_id)?
            .ok_or_else(|| QueueError::TaskVanished {
                task_id: task_id.to_string(),
            })?;

        if self.policy.permits_attempt(task.attempt_count) {
            let backoff = self.policy.backoff_for(task.attempt_count.max(1));
            store.transition(
                task_id,
                TaskState::RetryWait,
                Some(format!("{reason} (retry after {}s)", backoff.num_seconds())),
                None,
                now,
            )?;
            Ok(FailureVerdict::WillRetry {
                after: now + backoff,
                attempt: task.attempt_count + 1,
            })
        } else {
            // Attempts exhausted. FAILED, not DEAD: the work ran and lost, which is a
            // different diagnosis from a worker vanishing.
            store.transition(
                task_id,
                TaskState::Failed,
                Some(format!(
                    "{reason} (exhausted {} attempts)",
                    self.policy.max_attempts
                )),
                None,
                now,
            )?;
            Ok(FailureVerdict::DeadLettered {
                attempts: task.attempt_count,
            })
        }
    }

    /// Promotes `RETRY_WAIT` tasks whose backoff has elapsed back to `READY`.
    ///
    /// Backoff is judged from `updated_at`, which the transition into `RETRY_WAIT` set.
    pub fn promote_ready_retries(&self, store: &mut StateStore) -> Result<Vec<TaskId>, QueueError> {
        let now = self.clock.now();
        let waiting = store.list_by_state(TaskState::RetryWait)?;
        let mut promoted = Vec::new();

        for task in waiting {
            let backoff = self.policy.backoff_for(task.attempt_count.max(1));
            if now - task.updated_at < backoff {
                continue;
            }
            store.transition(
                &task.task_id,
                TaskState::Ready,
                Some("backoff elapsed".into()),
                None,
                now,
            )?;
            promoted.push(task.task_id);
        }
        Ok(promoted)
    }

    /// Admits a `PLANNED` task into the queue.
    pub fn enqueue(&self, store: &mut StateStore, task_id: &TaskId) -> Result<(), QueueError> {
        store.transition(task_id, TaskState::Ready, None, None, self.clock.now())?;
        Ok(())
    }

    /// Whether the current profile permits starting more work.
    ///
    /// Article 11 in operational form: a degraded system narrows what it will attempt
    /// rather than trusting itself to cope.
    pub fn admits_more_work(&self, in_flight: u32) -> bool {
        in_flight < self.profile.concurrency_cap()
    }
}

/// A successful claim.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub task: TaskRecord,
    pub lease: LeaseRecord,
}

/// What the queue decided after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureVerdict {
    /// The task will be offered again after `after`.
    WillRetry { after: DateTime<Utc>, attempt: u32 },
    /// Attempts exhausted; the task is now `FAILED`.
    DeadLettered { attempts: u32 },
}

impl FailureVerdict {
    pub fn will_retry(&self) -> bool {
        matches!(self, FailureVerdict::WillRetry { .. })
    }
}

/// Queue failures.
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("invalid retry policy: {detail}")]
    InvalidPolicy { detail: String },
    #[error("task '{task_id}' disappeared mid-operation")]
    TaskVanished { task_id: String },
    #[error(transparent)]
    Lease(#[from] LeaseError),
    #[error(transparent)]
    State(#[from] StateError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::default()
    }

    #[test]
    fn rejects_zero_max_attempts() {
        let err = RetryPolicy::new(
            0,
            TimeDelta::try_seconds(1).unwrap(),
            TimeDelta::try_seconds(2).unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least 1"));
    }

    #[test]
    fn rejects_max_below_base_backoff() {
        assert!(RetryPolicy::new(
            3,
            TimeDelta::try_seconds(60).unwrap(),
            TimeDelta::try_seconds(30).unwrap()
        )
        .is_err());
    }

    #[test]
    fn backoff_doubles_then_clamps() {
        let p = policy();
        assert_eq!(p.backoff_for(1).num_seconds(), 30);
        assert_eq!(p.backoff_for(2).num_seconds(), 60);
        assert_eq!(p.backoff_for(3).num_seconds(), 120);
        assert_eq!(p.backoff_for(4).num_seconds(), 240);
        // Clamped at max_backoff rather than growing without bound.
        assert_eq!(p.backoff_for(5).num_seconds(), 300);
        assert_eq!(p.backoff_for(50).num_seconds(), 300);
    }

    #[test]
    fn backoff_is_deterministic() {
        // Article 1: predictable, so a test can assert exactly when work resumes.
        let p = policy();
        for attempt in 1..=10 {
            assert_eq!(p.backoff_for(attempt), p.backoff_for(attempt));
        }
    }

    #[test]
    fn backoff_never_overflows() {
        let p = RetryPolicy::new(
            u32::MAX,
            TimeDelta::try_seconds(3600).unwrap(),
            TimeDelta::try_seconds(7200).unwrap(),
        )
        .unwrap();
        assert_eq!(p.backoff_for(u32::MAX).num_seconds(), 7200);
    }

    #[test]
    fn attempts_are_bounded() {
        let p = policy();
        assert!(p.permits_attempt(0));
        assert!(p.permits_attempt(2));
        assert!(!p.permits_attempt(3), "3 attempts means 3, not 4");
        assert!(!p.permits_attempt(99));
    }

    #[test]
    fn zero_backoff_is_allowed() {
        let p = RetryPolicy::new(2, TimeDelta::zero(), TimeDelta::zero()).unwrap();
        assert_eq!(p.backoff_for(1), TimeDelta::zero());
        assert_eq!(p.backoff_for(5), TimeDelta::zero());
    }

    #[test]
    fn failure_verdict_reports_intent() {
        let retry = FailureVerdict::WillRetry {
            after: DateTime::from_timestamp(0, 0).unwrap(),
            attempt: 2,
        };
        assert!(retry.will_retry());
        assert!(!FailureVerdict::DeadLettered { attempts: 3 }.will_retry());
    }
}
