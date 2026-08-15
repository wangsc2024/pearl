//! The durable work state machine — 系統開發需求書 §33.
//!
//! ```text
//! CREATED → PLANNING → PLANNED → READY → LEASED → RUNNING → VERIFYING → VERIFIED_SUCCESS
//! ```
//!
//! The enum and its transition rules live in `pearl-core` rather than `pearl-state`
//! because the event ledger must be able to record a transition without depending on
//! the store that applies it.
//!
//! Two states deserve explanation:
//!
//! - `UNVERIFIED` exists because of Article 2 Case B. When the business demands exactness
//!   but no Machine Verifier exists yet, the honest outcome is neither success nor
//!   failure — the work may well be correct, but nothing can confirm it. Collapsing this
//!   into `VERIFIED_SUCCESS` is the exact dishonesty the Constitution forbids.
//! - `DEAD` is distinct from `FAILED`. `FAILED` means the work ran and did not succeed;
//!   `DEAD` means the worker vanished and the system gave up reclaiming it. They demand
//!   different operator responses.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A durable task state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Planning,
    Planned,
    Ready,
    Leased,
    Running,
    Verifying,
    VerifiedSuccess,
    /// Exactness demanded, no verifier available (Article 2 Case B).
    Unverified,
    RetryWait,
    Blocked,
    Failed,
    Cancelled,
    /// Worker lost and not reclaimable.
    Dead,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Created => "created",
            TaskState::Planning => "planning",
            TaskState::Planned => "planned",
            TaskState::Ready => "ready",
            TaskState::Leased => "leased",
            TaskState::Running => "running",
            TaskState::Verifying => "verifying",
            TaskState::VerifiedSuccess => "verified_success",
            TaskState::Unverified => "unverified",
            TaskState::RetryWait => "retry_wait",
            TaskState::Blocked => "blocked",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
            TaskState::Dead => "dead",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "created" => TaskState::Created,
            "planning" => TaskState::Planning,
            "planned" => TaskState::Planned,
            "ready" => TaskState::Ready,
            "leased" => TaskState::Leased,
            "running" => TaskState::Running,
            "verifying" => TaskState::Verifying,
            "verified_success" => TaskState::VerifiedSuccess,
            "unverified" => TaskState::Unverified,
            "retry_wait" => TaskState::RetryWait,
            "blocked" => TaskState::Blocked,
            "failed" => TaskState::Failed,
            "cancelled" => TaskState::Cancelled,
            "dead" => TaskState::Dead,
            _ => return None,
        })
    }

    /// States from which no further transition is permitted.
    ///
    /// `Unverified` is deliberately *not* terminal: building a verifier later must be
    /// able to resolve it, otherwise Article 2 would turn every unverifiable task into
    /// permanent garbage.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::VerifiedSuccess | TaskState::Failed | TaskState::Cancelled | TaskState::Dead
        )
    }

    /// Whether this state means a worker currently holds the task.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskState::Leased | TaskState::Running | TaskState::Verifying
        )
    }

    /// Whether the task is waiting to be picked up.
    pub fn is_claimable(&self) -> bool {
        matches!(self, TaskState::Ready)
    }

    /// The states reachable in one step.
    pub fn allowed_next(&self) -> &'static [TaskState] {
        use TaskState::*;
        match self {
            Created => &[Planning, Cancelled],
            Planning => &[Planned, Blocked, Failed, Cancelled],
            Planned => &[Ready, Blocked, Cancelled],
            // Ready → Leased is the claim. Nothing else may move it forward.
            Ready => &[Leased, Blocked, Cancelled],
            // Leased → Ready is lease expiry: the reaper returns unclaimed work to the
            // queue. This edge is what prevents a task sticking in an active state
            // forever when a worker dies (§34).
            Leased => &[Running, Ready, Cancelled, Dead],
            Running => &[Verifying, RetryWait, Blocked, Failed, Cancelled, Dead],
            // Verifying → Unverified is the Exactness Gate refusing to auto-complete.
            Verifying => &[VerifiedSuccess, Unverified, RetryWait, Blocked, Failed],
            Unverified => &[Verifying, VerifiedSuccess, Blocked, Failed, Cancelled],
            RetryWait => &[Ready, Failed, Cancelled, Dead],
            Blocked => &[Ready, Failed, Cancelled],
            VerifiedSuccess | Failed | Cancelled | Dead => &[],
        }
    }

    /// Whether `next` is reachable from here in one step.
    pub fn can_transition_to(&self, next: TaskState) -> bool {
        self.allowed_next().contains(&next)
    }

    /// Validates a transition.
    pub fn validate_transition(&self, next: TaskState) -> Result<(), TransitionError> {
        if self.can_transition_to(next) {
            return Ok(());
        }
        Err(if self.is_terminal() {
            TransitionError::FromTerminal {
                from: *self,
                to: next,
            }
        } else {
            TransitionError::NotAllowed {
                from: *self,
                to: next,
            }
        })
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    #[error("transition {from} → {to} is not permitted by the state machine")]
    NotAllowed { from: TaskState, to: TaskState },
    #[error("{from} is terminal; it cannot transition to {to}")]
    FromTerminal { from: TaskState, to: TaskState },
    #[error("cannot enter verified_success: {reason}")]
    EvidenceInsufficient { reason: String },
}

#[cfg(test)]
mod tests {
    use super::TaskState::*;
    use super::*;

    #[test]
    fn happy_path_walks_end_to_end() {
        let path = [
            Created,
            Planning,
            Planned,
            Ready,
            Leased,
            Running,
            Verifying,
            VerifiedSuccess,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{} → {} must be allowed",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn terminal_states_are_final() {
        for terminal in [VerifiedSuccess, Failed, Cancelled, Dead] {
            assert!(terminal.is_terminal());
            assert!(terminal.allowed_next().is_empty());
            assert!(matches!(
                terminal.validate_transition(Ready),
                Err(TransitionError::FromTerminal { .. })
            ));
        }
    }

    #[test]
    fn unverified_is_resolvable_not_terminal() {
        // Article 2: building a verifier later must be able to rescue the task.
        assert!(!Unverified.is_terminal());
        assert!(Unverified.can_transition_to(Verifying));
        assert!(Unverified.can_transition_to(VerifiedSuccess));
    }

    #[test]
    fn lease_expiry_returns_work_to_the_queue() {
        // §34: the edge that stops a task sticking in an active state when a worker dies.
        assert!(Leased.can_transition_to(Ready));
    }

    #[test]
    fn retry_returns_through_ready_not_directly_to_running() {
        assert!(RetryWait.can_transition_to(Ready));
        assert!(
            !RetryWait.can_transition_to(Running),
            "a retry must be re-claimed, so the lease is re-established"
        );
    }

    #[test]
    fn work_cannot_skip_verification() {
        assert!(
            !Running.can_transition_to(VerifiedSuccess),
            "Article 4: success must pass through verification"
        );
        assert!(Running.can_transition_to(Verifying));
    }

    #[test]
    fn work_cannot_start_without_being_claimed() {
        assert!(
            !Ready.can_transition_to(Running),
            "running requires a lease"
        );
        assert!(Ready.can_transition_to(Leased));
    }

    #[test]
    fn planning_cannot_be_skipped_from_created() {
        assert!(!Created.can_transition_to(Ready));
        assert!(!Created.can_transition_to(Running));
        assert!(Created.can_transition_to(Planning));
    }

    #[test]
    fn cancellation_is_reachable_from_every_non_terminal_state() {
        for state in [
            Created, Planning, Planned, Ready, Leased, Running, Unverified, RetryWait, Blocked,
        ] {
            assert!(
                state.can_transition_to(Cancelled),
                "{state} must be cancellable"
            );
        }
    }

    #[test]
    fn active_states_are_exactly_those_holding_a_worker() {
        for s in [Leased, Running, Verifying] {
            assert!(s.is_active(), "{s} should be active");
        }
        for s in [
            Created, Planning, Planned, Ready, RetryWait, Blocked, Unverified,
        ] {
            assert!(!s.is_active(), "{s} should not be active");
        }
    }

    #[test]
    fn only_ready_is_claimable() {
        assert!(Ready.is_claimable());
        for s in [
            Created, Planning, Planned, Leased, Running, RetryWait, Blocked,
        ] {
            assert!(!s.is_claimable(), "{s} must not be claimable");
        }
    }

    #[test]
    fn state_names_round_trip() {
        let all = [
            Created,
            Planning,
            Planned,
            Ready,
            Leased,
            Running,
            Verifying,
            VerifiedSuccess,
            Unverified,
            RetryWait,
            Blocked,
            Failed,
            Cancelled,
            Dead,
        ];
        for state in all {
            assert_eq!(TaskState::parse(state.as_str()), Some(state));
        }
        assert_eq!(TaskState::parse("not_a_state"), None);
    }

    #[test]
    fn no_transition_targets_created() {
        // Created is an entry point only; nothing may re-enter it.
        let all = [
            Created,
            Planning,
            Planned,
            Ready,
            Leased,
            Running,
            Verifying,
            VerifiedSuccess,
            Unverified,
            RetryWait,
            Blocked,
            Failed,
            Cancelled,
            Dead,
        ];
        for state in all {
            assert!(
                !state.can_transition_to(Created),
                "{state} must not transition back to created"
            );
        }
    }
}
