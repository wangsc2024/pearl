//! # pearl-lease
//!
//! Worker leases — 系統開發需求書 §34.
//!
//! A worker that crashes cannot report its own death. Leases are how the system notices
//! anyway: a claim carries a deadline, the holder must keep extending it, and anything
//! that stops extending is reclaimed. Without this a dead worker's task stays `RUNNING`
//! forever and the queue silently loses capacity.
//!
//! ```text
//! claim → heartbeat → heartbeat → ... → release        (healthy)
//! claim → heartbeat → ✗ (worker dies) → expiry → READY (reclaimed)
//! ```

use chrono::{DateTime, TimeDelta, Utc};
use pearl_core::{Clock, LeaseId, TaskId, TaskState, WorkerId};
use pearl_events::{EventEnvelope, PearlEvent};
use pearl_state::{LeaseRecord, StateError, StateStore};

/// How leases behave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseConfig {
    /// How long a claim is valid before it must be renewed.
    pub duration: TimeDelta,
    /// How often the holder is expected to renew.
    pub heartbeat_interval: TimeDelta,
}

impl LeaseConfig {
    /// Builds a config, rejecting combinations that cannot work.
    pub fn new(duration: TimeDelta, heartbeat_interval: TimeDelta) -> Result<Self, LeaseError> {
        if duration <= TimeDelta::zero() || heartbeat_interval <= TimeDelta::zero() {
            return Err(LeaseError::InvalidConfig {
                detail: "duration and heartbeat interval must be positive".into(),
            });
        }
        // The interval must leave room for at least one missed beat, otherwise a single
        // scheduling hiccup on a healthy worker would look like death and its task would
        // be reclaimed while it is still running it.
        if heartbeat_interval * 2 > duration {
            return Err(LeaseError::InvalidConfig {
                detail: format!(
                    "heartbeat interval {}s must be at most half the lease duration {}s so a single missed beat is survivable",
                    heartbeat_interval.num_seconds(),
                    duration.num_seconds()
                ),
            });
        }
        Ok(Self { duration, heartbeat_interval })
    }

    /// 60s lease, 20s heartbeat: tolerates two missed beats.
    pub fn default_config() -> Self {
        Self {
            duration: TimeDelta::try_seconds(60).expect("valid"),
            heartbeat_interval: TimeDelta::try_seconds(20).expect("valid"),
        }
    }
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self::default_config()
    }
}

/// Issues, renews and reclaims leases.
pub struct LeaseManager<C: Clock> {
    config: LeaseConfig,
    clock: C,
}

impl<C: Clock> LeaseManager<C> {
    pub fn new(config: LeaseConfig, clock: C) -> Self {
        Self { config, clock }
    }

    pub fn config(&self) -> LeaseConfig {
        self.config
    }

    /// Claims a `READY` task for a worker, moving it to `LEASED`.
    ///
    /// The task transition and the lease row are written through `StateStore`, so both
    /// land in the ledger. A claim that is not in the ledger could not be reclaimed after
    /// a restart, which would defeat the purpose.
    pub fn claim(
        &self,
        store: &mut StateStore,
        task_id: &TaskId,
        worker_id: &WorkerId,
    ) -> Result<LeaseRecord, LeaseError> {
        let now = self.clock.now();
        let task = store
            .get_task(task_id)?
            .ok_or_else(|| LeaseError::TaskNotFound { task_id: task_id.to_string() })?;

        if !task.state.is_claimable() {
            return Err(LeaseError::NotClaimable { task_id: task_id.to_string(), state: task.state });
        }
        if let Some(existing) = store.active_lease_for_task(task_id)? {
            if existing.is_active(now) {
                return Err(LeaseError::AlreadyLeased {
                    task_id: task_id.to_string(),
                    worker_id: existing.worker_id.to_string(),
                });
            }
        }

        let lease = LeaseRecord {
            lease_id: LeaseId::new(),
            task_id: task_id.clone(),
            worker_id: worker_id.clone(),
            acquired_at: now,
            leased_until: now + self.config.duration,
            last_heartbeat: now,
            released_at: None,
        };

        let envelope = EventEnvelope::new(
            task.trace_id,
            now,
            PearlEvent::LeaseAcquired {
                task_id: task_id.clone(),
                lease_id: lease.lease_id,
                worker_id: worker_id.clone(),
                leased_until: lease.leased_until,
            },
        )
        .with_worker(worker_id.clone());

        store.ledger().append(&envelope)?;
        store.insert_lease(&lease)?;
        store.transition(task_id, TaskState::Leased, None, None, now)?;

        Ok(lease)
    }

    /// Extends a lease.
    pub fn heartbeat(
        &self,
        store: &mut StateStore,
        lease_id: LeaseId,
    ) -> Result<DateTime<Utc>, LeaseError> {
        let now = self.clock.now();
        let lease = store
            .get_lease(lease_id)?
            .ok_or_else(|| LeaseError::LeaseNotFound { lease_id: lease_id.to_string() })?;

        if lease.released_at.is_some() {
            return Err(LeaseError::AlreadyReleased { lease_id: lease_id.to_string() });
        }
        // A lapsed lease cannot be revived: the reaper may already have handed the task
        // to another worker, so extending it would create two owners.
        if lease.is_expired(now) {
            return Err(LeaseError::Expired {
                lease_id: lease_id.to_string(),
                expired_at: lease.leased_until,
            });
        }

        let leased_until = now + self.config.duration;
        store.renew_lease(lease_id, leased_until, now)?;
        Ok(leased_until)
    }

    /// Hands a lease back deliberately.
    pub fn release(&self, store: &mut StateStore, lease_id: LeaseId) -> Result<(), LeaseError> {
        let now = self.clock.now();
        store.release_lease(lease_id, now)?;
        Ok(())
    }

    /// Reclaims every lapsed lease, returning its task to `READY`.
    ///
    /// This is the mechanism that makes worker crashes survivable. It is idempotent: a
    /// second pass finds nothing, because reclamation releases the lease.
    pub fn reap(&self, store: &mut StateStore) -> Result<ReapReport, LeaseError> {
        let now = self.clock.now();
        let expired = store.expired_leases(now)?;

        let mut reclaimed = Vec::new();
        let mut skipped = Vec::new();

        for lease in expired {
            let task = match store.get_task(&lease.task_id)? {
                Some(t) => t,
                None => continue,
            };

            let envelope = EventEnvelope::new(
                task.trace_id,
                now,
                PearlEvent::LeaseExpired {
                    task_id: lease.task_id.clone(),
                    lease_id: lease.lease_id,
                    worker_id: lease.worker_id.clone(),
                },
            )
            .with_worker(lease.worker_id.clone());
            store.ledger().append(&envelope)?;

            // Release first: whatever happens to the task, this lease is finished. If the
            // task transition fails, leaving the lease open would make the reaper retry
            // the same dead lease on every pass.
            store.release_lease(lease.lease_id, now)?;

            // Only a task still held by this lease may be requeued. A task that has since
            // moved on (verified, cancelled, already reclaimed) must be left alone.
            if task.state.is_active() {
                // Where the task goes depends on how far it got, because that determines
                // whether any work — and therefore any side effect — already happened.
                let target = match task.state {
                    // Claimed but never started: nothing ran, so it is safe to offer the
                    // task to the next worker immediately.
                    TaskState::Leased => TaskState::Ready,
                    // Started, or finished and interrupted during verification. Work ran,
                    // so a retry must go through backoff accounting rather than jumping
                    // straight back into the claimable pool.
                    TaskState::Running | TaskState::Verifying => TaskState::RetryWait,
                    // is_active() covers exactly the three states above.
                    other => {
                        tracing::warn!(state = %other, "unexpected active state during reap");
                        skipped.push(lease.task_id.clone());
                        continue;
                    }
                };

                match store.transition(
                    &lease.task_id,
                    target,
                    Some(format!("lease {} expired", lease.lease_id)),
                    None,
                    now,
                ) {
                    Ok(_) => reclaimed.push(lease.task_id.clone()),
                    Err(_) => skipped.push(lease.task_id.clone()),
                }
            } else {
                skipped.push(lease.task_id.clone());
            }
        }

        Ok(ReapReport { reclaimed, skipped })
    }

    /// Leases whose holder is overdue for a heartbeat but which have not yet lapsed.
    ///
    /// Useful for a health monitor that wants to warn before work is actually reclaimed.
    pub fn stale_leases(&self, store: &StateStore) -> Result<Vec<LeaseRecord>, LeaseError> {
        let now = self.clock.now();
        let threshold = self.config.heartbeat_interval * 2;
        let mut stale = Vec::new();
        for lease in store.expired_leases(now + self.config.duration)? {
            if lease.released_at.is_none()
                && !lease.is_expired(now)
                && now - lease.last_heartbeat > threshold
            {
                stale.push(lease);
            }
        }
        Ok(stale)
    }
}

/// What a reap pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReapReport {
    /// Tasks returned to the queue.
    pub reclaimed: Vec<TaskId>,
    /// Leases closed without requeueing, because the task had already moved on.
    pub skipped: Vec<TaskId>,
}

impl ReapReport {
    pub fn is_empty(&self) -> bool {
        self.reclaimed.is_empty() && self.skipped.is_empty()
    }

    pub fn total(&self) -> usize {
        self.reclaimed.len() + self.skipped.len()
    }
}

/// Lease failures.
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("invalid lease configuration: {detail}")]
    InvalidConfig { detail: String },
    #[error("task '{task_id}' not found")]
    TaskNotFound { task_id: String },
    #[error("task '{task_id}' is {state}, not claimable")]
    NotClaimable { task_id: String, state: TaskState },
    #[error("task '{task_id}' is already leased by '{worker_id}'")]
    AlreadyLeased { task_id: String, worker_id: String },
    #[error("lease '{lease_id}' not found")]
    LeaseNotFound { lease_id: String },
    #[error("lease '{lease_id}' was already released")]
    AlreadyReleased { lease_id: String },
    #[error("lease '{lease_id}' expired at {expired_at}; it cannot be renewed")]
    Expired { lease_id: String, expired_at: DateTime<Utc> },
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Ledger(#[from] pearl_events::LedgerError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_positive_durations() {
        assert!(LeaseConfig::new(TimeDelta::zero(), TimeDelta::try_seconds(1).unwrap()).is_err());
        assert!(LeaseConfig::new(TimeDelta::try_seconds(10).unwrap(), TimeDelta::zero()).is_err());
    }

    #[test]
    fn rejects_heartbeat_too_close_to_expiry() {
        // 30s beat on a 40s lease leaves no room for a single missed beat.
        let err = LeaseConfig::new(
            TimeDelta::try_seconds(40).unwrap(),
            TimeDelta::try_seconds(30).unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("survivable"));
    }

    #[test]
    fn accepts_heartbeat_at_half_the_duration() {
        assert!(LeaseConfig::new(
            TimeDelta::try_seconds(60).unwrap(),
            TimeDelta::try_seconds(30).unwrap()
        )
        .is_ok());
    }

    #[test]
    fn default_config_tolerates_two_missed_beats() {
        let c = LeaseConfig::default();
        assert_eq!(c.duration.num_seconds(), 60);
        assert_eq!(c.heartbeat_interval.num_seconds(), 20);
        assert!(c.heartbeat_interval * 3 <= c.duration);
    }

    #[test]
    fn reap_report_arithmetic() {
        let mut r = ReapReport::default();
        assert!(r.is_empty());
        r.reclaimed.push(TaskId::parse("a").unwrap());
        r.skipped.push(TaskId::parse("b").unwrap());
        assert!(!r.is_empty());
        assert_eq!(r.total(), 2);
    }
}
