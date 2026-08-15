//! # pearl-executor
//!
//! Executes compiled plans step by step, respecting dependency order (topological).
//!
//! Key properties:
//! - Only accepts `CompiledPlan` (cannot receive unvalidated plans)
//! - Checkpoints after each durable step (writes `CheckpointCommitted` event)
//! - On crash recovery: resumes from last checkpoint
//! - Cannot modify policy, expand tools, or add side effects

mod executor;

pub use executor::{
    Checkpoint, ExecutionResult, Executor, ExecutorConfig, StepExecutor, StepOutcome, StepRecord,
};
