//! What a worker concluded about one task.

use chrono::{DateTime, Utc};
use pearl_assurance::AssuranceResult;
use pearl_core::TaskId;
use serde::{Deserialize, Serialize};

/// The outcome of one task, in the vocabulary the Constitution cares about.
///
/// Five outcomes rather than success/failure, because the differences drive different
/// operator actions:
///
/// - `Verified` — proven, so it may be relied on.
/// - `Unverified` — it ran, but nothing established the claim. Needs a verifier or a gate.
/// - `Failed` — it ran and lost. A retry might succeed.
/// - `TimedOut` — it did not finish, so nothing is known about what it did.
/// - `Refused` — it never started. A retry changes nothing until a human changes something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum Verdict {
    Verified,
    Unverified { reason: String },
    Failed { reason: String },
    TimedOut,
    Refused { reason: String },
}

impl Verdict {
    /// Whether the work may be relied on.
    ///
    /// Only `Verified`. `Unverified` deliberately does not count: that is the whole point of
    /// keeping it distinct from success.
    pub fn is_verified(&self) -> bool {
        matches!(self, Verdict::Verified)
    }

    /// Whether the capability actually ran.
    pub fn executed(&self) -> bool {
        !matches!(self, Verdict::Refused { .. })
    }

    /// Whether retrying could plausibly reach a different outcome.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Verdict::Failed { .. } | Verdict::TimedOut)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Verified => "verified",
            Verdict::Unverified { .. } => "unverified",
            Verdict::Failed { .. } => "failed",
            Verdict::TimedOut => "timed_out",
            Verdict::Refused { .. } => "refused",
        }
    }

    /// The explanation, when there is one.
    pub fn reason(&self) -> Option<String> {
        match self {
            Verdict::Verified => None,
            Verdict::TimedOut => Some("timed out".to_string()),
            Verdict::Unverified { reason }
            | Verdict::Failed { reason }
            | Verdict::Refused { reason } => Some(reason.clone()),
        }
    }
}

/// The full record of one task's processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResult {
    pub task_id: TaskId,
    /// The capability that ran, or empty when none was chosen.
    pub capability_id: String,
    pub verdict: Verdict,
    /// What verification found.
    pub assurance: AssuranceResult,
    /// Exit code, when the process exited normally.
    pub exit_code: Option<i32>,
    /// The machine JSON the capability emitted, when it emitted any.
    pub structured_output: Option<serde_json::Value>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
}

impl WorkResult {
    pub fn is_verified(&self) -> bool {
        self.verdict.is_verified()
    }

    /// A one-line summary for an operator.
    pub fn summary(&self) -> String {
        let reason = self
            .verdict
            .reason()
            .map(|r| format!(" — {r}"))
            .unwrap_or_default();
        format!(
            "{} {} in {}ms ({}){reason}",
            self.task_id,
            self.verdict.as_str(),
            self.duration_ms,
            self.assurance.summary()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_counts_as_verified() {
        assert!(Verdict::Verified.is_verified());
        assert!(!Verdict::Unverified {
            reason: "no verifier".into()
        }
        .is_verified());
        assert!(!Verdict::Failed {
            reason: "exit 1".into()
        }
        .is_verified());
    }

    #[test]
    fn a_refusal_did_not_execute_and_is_not_retryable() {
        let refused = Verdict::Refused {
            reason: "not permitted".into(),
        };
        assert!(!refused.executed());
        assert!(!refused.is_retryable());
    }

    #[test]
    fn unverified_is_not_retryable_because_a_retry_changes_nothing() {
        // The missing piece is a verifier, which no number of retries will supply.
        let unverified = Verdict::Unverified {
            reason: "no verifier declared".into(),
        };
        assert!(unverified.executed());
        assert!(!unverified.is_retryable());
    }

    #[test]
    fn failures_and_timeouts_are_retryable() {
        assert!(Verdict::Failed {
            reason: "exit 1".into()
        }
        .is_retryable());
        assert!(Verdict::TimedOut.is_retryable());
    }

    #[test]
    fn verdicts_round_trip_through_json() {
        for verdict in [
            Verdict::Verified,
            Verdict::TimedOut,
            Verdict::Unverified { reason: "r".into() },
            Verdict::Failed { reason: "r".into() },
            Verdict::Refused { reason: "r".into() },
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            assert_eq!(serde_json::from_str::<Verdict>(&json).unwrap(), verdict);
        }
    }
}
