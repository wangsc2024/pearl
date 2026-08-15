//! # pearl-plan-compiler
//!
//! Validates a plan before execution. The compiler checks:
//!
//! 1. **DAG acyclicity** -- topological sort detects cycles
//! 2. **Capability existence** -- all referenced capabilities must be in the registry
//! 3. **Verifier presence** -- exactness (P0/P1) tasks must have verifiers
//! 4. **Budget compliance** -- step/LLM-call counts within limits
//! 5. **Timeout presence** -- every step must declare a timeout
//!
//! Only a successfully compiled plan (`CompiledPlan`) can be handed to the executor.

mod compiler;

pub use compiler::{
    CapabilitySet, CompileError, CompiledPlan, CompilerConfig, PlanCompiler, VerifierSet,
};
