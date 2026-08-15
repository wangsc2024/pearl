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
//! - **Permissions**: the capability allow-list (`policies/permissions.yaml`)
//!
//! The two layers answer different questions and fail in opposite directions:
//! [`Permissions`] decides whether a capability may be invoked at all and denies anything
//! it does not recognise; [`PolicyEngine`] decides what approval an already-permitted
//! action needs and allows anything no rule speaks to.

mod permissions;
mod policy;

pub use permissions::{Effect, PermissionDecision, PermissionError, PermissionRule, Permissions};
pub use policy::{
    Approval, AutonomyLevel, PolicyDecision, PolicyEngine, PolicyError, PolicyRule, RequestContext,
};
