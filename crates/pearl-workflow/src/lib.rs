//! # pearl-workflow
//!
//! Declarative workflow engine: YAML-defined step sequences with `run`, `parallel`,
//! `verify`, and `effect` step types.
//!
//! A WorkflowDef is parsed from YAML and converted into an ExecutionPlan, which is
//! then compiled and executed. This provides checkpoint/resume support through the
//! standard plan execution pipeline.

mod engine;

pub use engine::{
    StepType, WorkflowDef, WorkflowEngine, WorkflowError, WorkflowResult, WorkflowStep,
};
