//! # pearl-assurance
//!
//! The Assurance Engine runs verification steps after execution to confirm that
//! a task completed successfully. A task's completion requires ALL assurance checks
//! to pass.
//!
//! Check types:
//! - **SchemaValidation**: validates output against a declared schema
//! - **ScriptVerifier**: runs a verification script
//! - **TestCommand**: executes a test command and checks exit code
//!
//! Each check can optionally require evidence (`evidence_required` flag).

mod engine;

pub use engine::{
    AssuranceCheck, AssuranceEngine, AssuranceError, AssuranceResult, AssuranceSpec, CheckDetail,
    CheckKind, CheckOutcome,
};
