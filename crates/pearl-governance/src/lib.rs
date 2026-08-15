//! # pearl-governance
//!
//! The Constitution CI gate — 系統開發需求書 §56.
//!
//! The Constitution is only binding if a machine can reject a violation. This crate
//! turns each article into a named check, so `pearl constitution check` fails a build
//! rather than a reviewer noticing.
//!
//! ```
//! use pearl_governance::{CapabilityManifest, run_gate};
//!
//! // A side-effecting capability with no idempotency key violates Article 5.
//! let manifest = CapabilityManifest::from_yaml(r#"
//! id: effect.send-mail
//! version: 1
//! type: tool
//! execution:
//!   kind: script
//!   runtime: python
//! quality:
//!   deterministic: false
//! risk:
//!   side_effect: true
//! platform:
//!   windows: false
//!   linux: true
//! timeout_seconds: 30
//! "#).unwrap();
//!
//! let report = run_gate([&manifest]);
//! assert!(!report.passed());
//! assert_eq!(report.violation_count(), 1);
//! ```

pub mod checks;
pub mod manifest;
pub mod ooda;

pub use checks::{
    check_effect_has_idempotency_key, check_guard_fail_closed, check_has_timeout, check_manifest,
    check_manifest_coherence, check_no_llm_for_deterministic, check_task_exactness_has_verifier,
    check_verifier_declares_schemas, run_gate, Article, Finding, GateReport, Severity,
};
pub use manifest::{
    CapabilityManifest, CapabilityType, Execution, ExecutionKind, Idempotency, ManifestError,
    OnError, Platform, Quality, Risk, Runtime, Schemas,
};
