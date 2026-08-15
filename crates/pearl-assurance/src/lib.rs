//! # pearl-assurance
//!
//! The Assurance Engine runs verification steps after execution to confirm that
//! a task completed successfully. A task's completion requires ALL assurance checks
//! to pass.
//!
//! Check types, each with a real mechanism behind it in [`runners`]:
//! - **SchemaValidation**: validates the subject against a local JSON Schema
//! - **ScriptVerifier**: spawns a verification script and reads its exit code
//! - **TestCommand**: spawns a test command and reads its exit code
//!
//! Each check can optionally require evidence (`evidence_required` flag).
//!
//! Outcomes are three-valued, not two. A check that *could not run* returns
//! [`CheckOutcome::Errored`], which is neither a pass nor a failure: Article 2 requires
//! that the absence of a verdict be visible, because a broken verifier must not read as a
//! failing one, and must never be retried into a pass.

mod engine;
pub mod quality_metrics;
pub mod runners;

pub use engine::{
    AssuranceCheck, AssuranceEngine, AssuranceError, AssuranceResult, AssuranceSpec, CheckDetail,
    CheckKind, CheckOutcome, CheckRunner,
};
pub use quality_metrics::QualityMetrics;
pub use runners::{runner_fn, CheckContext, RuntimeCheckRunner, DEFAULT_CHECK_TIMEOUT_SECONDS};
