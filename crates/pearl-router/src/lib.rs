//! # pearl-router
//!
//! The script-first routing engine -- Constitution Article 1 enforcement.
//!
//! When a task needs execution, the router decides *how* to execute it. The decision
//! tree enforces Article 1 (deterministic work must be routed to scripts, never to an LLM):
//!
//! 1. Check the capability registry for a matching P0 mechanical capability.
//! 2. If found, route to the script runtime (ScriptRoute).
//! 3. If not found and the task is P0, *reject* -- there is no acceptable fallback.
//! 4. For P1/P2/P3, route to an agent (AgentRoute).
//!
//! The router also contains the Health Monitor, which tracks execution failures with
//! time decay and drives profile degradation (Normal -> Degraded -> Recovery) so the
//! system self-limits under stress and auto-recovers when failures clear.

pub mod health;
mod router;

pub use health::{HealthConfig, HealthMonitor};
pub use router::{Router, RouterError, RoutingDecision, TaskRequirements};
