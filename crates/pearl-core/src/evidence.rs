//! Evidence model — Constitution Article 4.
//!
//! "Success = Result + Evidence + Verification". A result without evidence is a claim,
//! and a claim cannot become `VERIFIED_SUCCESS`. See `schemas/evidence-v1.json`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What kind of reason-to-believe this is — 系統開發需求書 §52.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    Source,
    ApiResponse,
    Test,
    Schema,
    Hash,
    ToolOutput,
    GitDiff,
    HumanApproval,
}

impl EvidenceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvidenceType::Source => "source",
            EvidenceType::ApiResponse => "api_response",
            EvidenceType::Test => "test",
            EvidenceType::Schema => "schema",
            EvidenceType::Hash => "hash",
            EvidenceType::ToolOutput => "tool_output",
            EvidenceType::GitDiff => "git_diff",
            EvidenceType::HumanApproval => "human_approval",
        }
    }

    /// Whether this evidence was produced by a machine rather than asserted by a human
    /// or an agent.
    ///
    /// Article 8 needs this distinction: only machine-produced evidence can discharge a
    /// verification obligation. Human approval is a *gate*, which is a different thing —
    /// it authorises proceeding without proof, it does not constitute proof.
    pub fn is_machine_produced(&self) -> bool {
        !matches!(self, EvidenceType::HumanApproval)
    }
}

/// Pass or fail, as decided by whatever produced the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceResult {
    Pass,
    Fail,
}

/// One reason to believe a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(rename = "type")]
    pub evidence_type: EvidenceType,
    /// What generated this, e.g. `pytest`, `cargo test`, `jsonschema`.
    pub producer: String,
    pub timestamp: DateTime<Utc>,
    /// Path or URI of the retained artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// SHA-256 of the artifact, so evidence cannot be silently swapped after the fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub result: EvidenceResult,
}

impl Evidence {
    pub fn new(
        evidence_type: EvidenceType,
        producer: impl Into<String>,
        result: EvidenceResult,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            evidence_type,
            producer: producer.into(),
            timestamp,
            artifact: None,
            digest: None,
            result,
        }
    }

    /// Attaches an artifact and its content digest.
    pub fn with_artifact(mut self, path: impl Into<String>, content: &[u8]) -> Self {
        self.artifact = Some(path.into());
        self.digest = Some(hex::encode(Sha256::digest(content)));
        self
    }

    pub fn passed(&self) -> bool {
        matches!(self.result, EvidenceResult::Pass)
    }
}

/// The evidence accumulated for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSet {
    items: Vec<Evidence>,
}

impl EvidenceSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, evidence: Evidence) {
        self.items.push(evidence);
    }

    pub fn items(&self) -> &[Evidence] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether this set can support a `VERIFIED_SUCCESS` claim.
    ///
    /// Three conditions, each earning its place:
    /// - non-empty, because Article 4 forbids evidence-free success;
    /// - no failing item, because a recorded failure contradicts the claim;
    /// - at least one machine-produced item, because Article 8 forbids an agent from
    ///   being the sole authority on its own correctness.
    pub fn supports_verified_success(&self) -> bool {
        !self.items.is_empty()
            && self.items.iter().all(Evidence::passed)
            && self
                .items
                .iter()
                .any(|e| e.evidence_type.is_machine_produced())
    }

    /// Why `supports_verified_success` returned false.
    pub fn rejection_reason(&self) -> Option<EvidenceRejection> {
        if self.items.is_empty() {
            return Some(EvidenceRejection::Empty);
        }
        if let Some(failed) = self.items.iter().find(|e| !e.passed()) {
            return Some(EvidenceRejection::ContainsFailure {
                producer: failed.producer.clone(),
            });
        }
        if !self
            .items
            .iter()
            .any(|e| e.evidence_type.is_machine_produced())
        {
            return Some(EvidenceRejection::NoMachineEvidence);
        }
        None
    }
}

impl FromIterator<Evidence> for EvidenceSet {
    fn from_iter<T: IntoIterator<Item = Evidence>>(iter: T) -> Self {
        Self {
            items: iter.into_iter().collect(),
        }
    }
}

/// Why an evidence set cannot support success.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceRejection {
    #[error("evidence set is empty; Article 4 forbids success without evidence")]
    Empty,
    #[error("evidence from '{producer}' records a failure")]
    ContainsFailure { producer: String },
    #[error(
        "evidence set contains no machine-produced item; Article 8 forbids self-certification"
    )]
    NoMachineEvidence,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_755_000_000, 0).unwrap()
    }

    fn passing(t: EvidenceType, producer: &str) -> Evidence {
        Evidence::new(t, producer, EvidenceResult::Pass, now())
    }

    #[test]
    fn empty_evidence_cannot_support_success() {
        let set = EvidenceSet::new();
        assert!(!set.supports_verified_success());
        assert_eq!(set.rejection_reason(), Some(EvidenceRejection::Empty));
    }

    #[test]
    fn passing_machine_evidence_supports_success() {
        let set: EvidenceSet = [passing(EvidenceType::Test, "cargo test")]
            .into_iter()
            .collect();
        assert!(set.supports_verified_success());
        assert_eq!(set.rejection_reason(), None);
    }

    #[test]
    fn a_single_failure_blocks_success() {
        let set: EvidenceSet = [
            passing(EvidenceType::Schema, "jsonschema"),
            Evidence::new(EvidenceType::Test, "pytest", EvidenceResult::Fail, now()),
        ]
        .into_iter()
        .collect();

        assert!(!set.supports_verified_success());
        assert_eq!(
            set.rejection_reason(),
            Some(EvidenceRejection::ContainsFailure {
                producer: "pytest".into()
            })
        );
    }

    #[test]
    fn human_approval_alone_cannot_certify_success() {
        // A human may authorise proceeding, but approval is not proof. Article 8.
        let set: EvidenceSet = [passing(EvidenceType::HumanApproval, "operator")]
            .into_iter()
            .collect();

        assert!(!set.supports_verified_success());
        assert_eq!(
            set.rejection_reason(),
            Some(EvidenceRejection::NoMachineEvidence)
        );
    }

    #[test]
    fn human_approval_alongside_machine_evidence_is_fine() {
        let set: EvidenceSet = [
            passing(EvidenceType::HumanApproval, "operator"),
            passing(EvidenceType::Test, "cargo test"),
        ]
        .into_iter()
        .collect();

        assert!(set.supports_verified_success());
    }

    #[test]
    fn artifact_digest_is_content_addressed() {
        let a = passing(EvidenceType::ToolOutput, "tool").with_artifact("out.json", b"same");
        let b = passing(EvidenceType::ToolOutput, "tool").with_artifact("other.json", b"same");
        let c = passing(EvidenceType::ToolOutput, "tool").with_artifact("out.json", b"different");

        assert_eq!(a.digest, b.digest, "same content must digest identically");
        assert_ne!(
            a.digest, c.digest,
            "different content must digest differently"
        );
        assert_eq!(a.digest.as_deref().map(str::len), Some(64));
    }

    #[test]
    fn only_human_approval_is_non_machine() {
        assert!(!EvidenceType::HumanApproval.is_machine_produced());
        for t in [
            EvidenceType::Source,
            EvidenceType::ApiResponse,
            EvidenceType::Test,
            EvidenceType::Schema,
            EvidenceType::Hash,
            EvidenceType::ToolOutput,
            EvidenceType::GitDiff,
        ] {
            assert!(t.is_machine_produced(), "{t:?} should be machine-produced");
        }
    }
}
