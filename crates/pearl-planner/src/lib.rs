//! # pearl-planner
//!
//! The Planner produces typed execution plans (Vec of PlanStep) without executing
//! anything. It is a pure declaration layer that separates intent from action.
//!
//! Key concepts:
//! - **PlanStep**: a single step with id, capability, dependencies, precision class, timeout
//! - **PlanBudget**: resource limits (max_steps, max_llm_calls) to bound plan size
//! - **ExecutionPlan**: a validated collection of steps ready for compilation
//!
//! The Planner does NOT execute -- it only produces plans. Execution is handled by
//! `pearl-executor` after the plan passes through `pearl-plan-compiler`.

mod planner;

pub use planner::{ExecutionPlan, PlanBudget, PlanStep, Planner, PlannerError};
