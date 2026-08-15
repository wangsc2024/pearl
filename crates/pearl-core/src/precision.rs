//! Precision classification and the Exactness Gate.
//!
//! 系統開發需求書 §17–§22. This module carries the *data model* and the gate decision.
//! The full Precision Decision Engine that infers a class from a task body is Phase 2;
//! Phase 1 needs the classification and the gate because the task state machine must be
//! able to refuse an unverifiable auto-completion (Article 2).

use serde::{Deserialize, Serialize};

/// How certain the correctness of a step can be made.
///
/// The ordering is deliberate: increasing ordinal means increasing unverifiability, and
/// Article 11 ties autonomy to verifiability, so this ordering is also the order of
/// decreasing permitted autonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrecisionClass {
    /// Deterministic. 100% script. An LLM must not participate (Article 1).
    P0,
    /// Generative but verifiable. LLM may produce; a Machine Verifier decides.
    P1,
    /// Partially verifiable. Facts are mechanical, interpretation is agentic.
    P2,
    /// Subjective or exploratory. Agent plus recorded evidence.
    P3,
}

impl PrecisionClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            PrecisionClass::P0 => "p0",
            PrecisionClass::P1 => "p1",
            PrecisionClass::P2 => "p2",
            PrecisionClass::P3 => "p3",
        }
    }

    /// Whether an LLM may participate in *producing* the result.
    ///
    /// Only P0 forbids it outright; that is Article 1 in one line.
    pub fn permits_llm_generation(&self) -> bool {
        !matches!(self, PrecisionClass::P0)
    }

    /// Whether a Machine Verifier is expected to be able to decide correctness.
    pub fn expects_machine_verification(&self) -> bool {
        matches!(self, PrecisionClass::P0 | PrecisionClass::P1)
    }
}

/// The quality contract of a task — 系統開發需求書 §22.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualitySpec {
    /// The business requires exact confirmation of this result.
    pub exactness_required: bool,
    /// The result is produced mechanically.
    #[serde(default)]
    pub deterministic_generation: bool,
    /// A Machine Verifier can decide correctness.
    pub deterministic_verification: bool,
}

impl QualitySpec {
    /// A task whose correctness is fully mechanical in both directions.
    pub const fn mechanical() -> Self {
        Self {
            exactness_required: true,
            deterministic_generation: true,
            deterministic_verification: true,
        }
    }

    /// A task that must be exact but has no verifier yet — the Article 2 Case B shape.
    pub const fn exact_but_unverifiable() -> Self {
        Self {
            exactness_required: true,
            deterministic_generation: false,
            deterministic_verification: false,
        }
    }

    /// A best-effort task with no exactness demand.
    pub const fn best_effort() -> Self {
        Self {
            exactness_required: false,
            deterministic_generation: false,
            deterministic_verification: false,
        }
    }

    /// Applies the Exactness Gate.
    ///
    /// The gate exists to stop the single most damaging failure mode: a task that the
    /// business needs to be exact quietly reporting success on an agent's own say-so.
    /// When exactness is demanded but nothing can mechanically confirm it, the honest
    /// terminal state is `UNVERIFIED`, not `SUCCESS`.
    pub fn gate(&self) -> ExactnessGate {
        match (self.exactness_required, self.deterministic_verification) {
            (true, false) => ExactnessGate::BlockAutoComplete,
            _ => ExactnessGate::Allow,
        }
    }
}

/// The Exactness Gate verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactnessGate {
    /// Auto-completion to `VERIFIED_SUCCESS` is permitted, subject to evidence.
    Allow,
    /// Auto-completion is forbidden. Requires a verifier or a Human Gate.
    BlockAutoComplete,
}

impl ExactnessGate {
    pub fn blocks(&self) -> bool {
        matches!(self, ExactnessGate::BlockAutoComplete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p0_forbids_llm_generation() {
        assert!(!PrecisionClass::P0.permits_llm_generation());
        for c in [PrecisionClass::P1, PrecisionClass::P2, PrecisionClass::P3] {
            assert!(c.permits_llm_generation(), "{c:?} should permit generation");
        }
    }

    #[test]
    fn only_p0_and_p1_expect_full_machine_verification() {
        assert!(PrecisionClass::P0.expects_machine_verification());
        assert!(PrecisionClass::P1.expects_machine_verification());
        assert!(!PrecisionClass::P2.expects_machine_verification());
        assert!(!PrecisionClass::P3.expects_machine_verification());
    }

    #[test]
    fn classes_order_by_increasing_unverifiability() {
        assert!(PrecisionClass::P0 < PrecisionClass::P1);
        assert!(PrecisionClass::P1 < PrecisionClass::P2);
        assert!(PrecisionClass::P2 < PrecisionClass::P3);
    }

    #[test]
    fn exact_without_verifier_blocks_auto_complete() {
        let gate = QualitySpec::exact_but_unverifiable().gate();
        assert_eq!(gate, ExactnessGate::BlockAutoComplete);
        assert!(gate.blocks());
    }

    #[test]
    fn exact_with_verifier_is_allowed() {
        assert_eq!(QualitySpec::mechanical().gate(), ExactnessGate::Allow);
    }

    #[test]
    fn non_exact_work_is_not_gated() {
        assert_eq!(QualitySpec::best_effort().gate(), ExactnessGate::Allow);
    }

    #[test]
    fn gate_ignores_generation_determinism() {
        // Only verification determinism decides the gate. How the value was produced
        // is irrelevant to whether we can confirm it.
        let llm_generated_but_verifiable = QualitySpec {
            exactness_required: true,
            deterministic_generation: false,
            deterministic_verification: true,
        };
        assert_eq!(llm_generated_but_verifiable.gate(), ExactnessGate::Allow);
    }
}
