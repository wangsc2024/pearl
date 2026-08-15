//! # pearl-core
//!
//! Kernel primitives shared by every PEARL crate: identifiers, time, configuration
//! resolution, precision classification, evidence and idempotency.
//!
//! This crate deliberately contains **no I/O and no LLM coupling**. It is the layer the
//! Constitution's data-model articles live in, so that the articles can be enforced by
//! types rather than by review:
//!
//! | Article | Enforced here by |
//! |---|---|
//! | 4 — provable success | [`evidence::EvidenceSet::supports_verified_success`] |
//! | 5 — idempotent effects | [`idempotency::IdempotencyKey`] |
//! | 10 — single source of truth | [`config::ConfigResolver`] with `config_hash` |
//! | 2 — exactness needs a verifier | [`precision::QualitySpec::gate`] |
//! | 11 — autonomy from verifiability | [`precision::PrecisionClass`] ordering |

pub mod clock;
pub mod config;
pub mod evidence;
pub mod idempotency;
pub mod ids;
pub mod plan;
pub mod precision;
pub mod redactor;
pub mod task_state;

pub use clock::{Clock, SharedClock, SystemClock, TestClock};
pub use config::{
    ConfigError, ConfigResolver, ConfigSource, Layer, ResolvedConfig, RuntimeProfile,
};
pub use evidence::{Evidence, EvidenceRejection, EvidenceResult, EvidenceSet, EvidenceType};
pub use idempotency::{IdempotencyKey, IdempotencyKeyError, IdempotencyTemplate};
pub use ids::{
    AttemptId, CheckpointId, EventId, InvalidTaskId, LeaseId, RunId, TaskId, TraceId, WorkerId,
};
pub use plan::{AssuranceStep, TaskPlan};
pub use precision::{ExactnessGate, PrecisionClass, QualitySpec};
pub use redactor::SecretRedactor;
pub use task_state::{TaskState, TransitionError};

/// Event ledger schema version. Bumped when the event vocabulary changes shape;
/// replay compatibility is asserted per version (ADR-0001).
///
/// v2 added [`plan::TaskPlan`] to `task.created`. The field deserializes to an empty plan
/// when absent, so a v1 ledger still replays — the version is recorded to make the change
/// visible in the ledger, not to gate reading it.
pub const EVENT_SCHEMA_VERSION: u32 = 2;
