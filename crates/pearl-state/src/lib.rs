//! # pearl-state
//!
//! Materialized projections over the event ledger — 系統開發需求書 §33, §43, §58.
//!
//! The store owns the task state machine's *enforcement*: `pearl-core` defines which
//! transitions are legal, and this crate refuses the illegal ones while recording every
//! legal one to the ledger in the same transaction.
//!
//! Three Constitution gates are applied on the way to `VERIFIED_SUCCESS`:
//!
//! 1. the state machine (no skipping verification),
//! 2. the Exactness Gate (Article 2 — exactness with no verifier cannot auto-complete),
//! 3. the evidence check (Article 4 — success must be provable).

pub mod migrations;
pub mod records;
pub mod spec;
pub mod store;

pub use records::{
    Artifact, AttemptRecord, CheckpointRecord, ConfigRevision, EffectRecord, EvidenceRecord,
    LeaseRecord, PolicyDecision, RunRecord, RuntimeHealth, ScheduleRecord, StepRecord, TaskRecord,
    VerificationResult,
};
pub use records::{ArtifactData, CacheData, EvidenceData, MemoryData, StateData};
pub use spec::{SpecError, TaskSpec};
pub use store::{EffectDecision, ReplaySummary, StateError, StateStore, TaskSubmission};
