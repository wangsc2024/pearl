//! # pearl-precision
//!
//! The Precision Decision Engine -- enforces Article 1 (determinism first) and
//! Article 11 (autonomy from verifiability) by classifying work before routing.
//!
//! Every task step must be assigned a [`PrecisionClass`] *before* it reaches a runtime
//! adapter.  The class determines which adapter may execute it:
//!
//! - **P0**: Fully deterministic script. An LLM must not participate (Article 1).
//! - **P1**: Generative but verifiable. An LLM may produce; a Machine Verifier decides.
//! - **P2**: Partially verifiable. Facts are mechanical, interpretation is agentic.
//! - **P3**: Subjective or exploratory. Agent plus recorded evidence.
//!
//! The decision rules inspect a task's declared [`ClassificationInput`] (derived from the
//! capability manifest and optional quality spec) and emit a [`ClassificationResult`]
//! containing the class, reasoning trail, and any override information.

mod engine;

pub use engine::{
    ClassificationInput, ClassificationOverride, ClassificationResult, PrecisionDecisionEngine,
    PrecisionError,
};
