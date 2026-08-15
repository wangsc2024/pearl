//! # pearl-policy
//!
//! The Policy Engine: rules that are NOT in prompts.
//!
//! Policies are declarative rules that control what actions are permitted,
//! what approval is required, and what autonomy level is granted based on
//! verification coverage (Article 11).
//!
//! Key concepts:
//! - **PolicyRule**: requires (conditions), approval (human/auto), idempotency_required
//! - **AutonomyLevel**: derived from verification coverage
//! - **PolicyEngine**: evaluates rules against a request context

mod policy;

pub use policy::{
    Approval, AutonomyLevel, PolicyDecision, PolicyEngine, PolicyError, PolicyRule, RequestContext,
};
