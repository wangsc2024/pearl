//! The Constitution CI gate — 系統開發需求書 §56.
//!
//! > "這樣憲法才不是 Markdown 標語。"
//! > (This is what stops the Constitution being a Markdown slogan.)
//!
//! Each function here is one of the named checks listed in the `CONSTITUTION.md`
//! enforcement table. They take declarations, not opinions: a manifest either declares an
//! idempotency key or it does not, and no amount of prose in a PR description changes the
//! verdict.

use crate::manifest::{CapabilityManifest, CapabilityType, OnError};
use pearl_core::QualitySpec;

/// Which article a finding relates to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Article(pub u8);

impl std::fmt::Display for Article {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Article {}", self.0)
    }
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Fails CI.
    Violation,
    /// Reported but does not fail CI.
    Warning,
}

/// One Constitution finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub article: Article,
    /// The named check, matching the `CONSTITUTION.md` table.
    pub check: &'static str,
    pub severity: Severity,
    /// What was inspected.
    pub subject: String,
    /// What is wrong, and what would fix it.
    pub detail: String,
}

impl Finding {
    fn violation(
        article: u8,
        check: &'static str,
        subject: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            article: Article(article),
            check,
            severity: Severity::Violation,
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    fn warning(
        article: u8,
        check: &'static str,
        subject: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            article: Article(article),
            check,
            severity: Severity::Warning,
            subject: subject.into(),
            detail: detail.into(),
        }
    }

    pub fn is_violation(&self) -> bool {
        matches!(self.severity, Severity::Violation)
    }
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tag = if self.is_violation() {
            "VIOLATION"
        } else {
            "warning"
        };
        write!(
            f,
            "{tag} [{} / {}] {}: {}",
            self.article, self.check, self.subject, self.detail
        )
    }
}

/// The outcome of a gate run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateReport {
    pub findings: Vec<Finding>,
    /// How many subjects were inspected, so an empty report can be distinguished from
    /// a run that silently checked nothing.
    pub inspected: usize,
}

impl GateReport {
    pub fn violations(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.is_violation())
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| !f.is_violation())
    }

    pub fn violation_count(&self) -> usize {
        self.violations().count()
    }

    /// Whether CI should pass.
    pub fn passed(&self) -> bool {
        self.violation_count() == 0
    }

    fn extend(&mut self, findings: Vec<Finding>) {
        self.findings.extend(findings);
    }
}

/// Article 1 — deterministic work must not be routed to an LLM.
///
/// A capability that declares `deterministic: true` is asserting that the same input
/// always yields the same output. An agent cannot honour that assertion, so the
/// combination is a contradiction rather than a risk.
pub fn check_no_llm_for_deterministic(m: &CapabilityManifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    if m.quality.deterministic && m.execution.kind.involves_llm() {
        findings.push(Finding::violation(
            1,
            "check_no_llm_for_deterministic",
            &m.id,
            format!(
                "declares deterministic: true but executes as '{}'. Deterministic work must run as a script.",
                m.execution.kind.as_str()
            ),
        ));
    }
    if m.quality.deterministic && !m.execution.runtime.is_mechanical() {
        findings.push(Finding::violation(
            1,
            "check_no_llm_for_deterministic",
            &m.id,
            format!(
                "declares deterministic: true but targets runtime '{}', which is not a mechanical script runtime.",
                m.execution.runtime.as_str()
            ),
        ));
    }
    findings
}

/// Article 5 — a side-effecting capability must declare an idempotency key template.
///
/// Also checks the template is *usable*: a template with no placeholders would produce
/// one identical key for every invocation, deduplicating unrelated effects against each
/// other. That is worse than having no key, because it fails silently.
pub fn check_effect_has_idempotency_key(m: &CapabilityManifest) -> Vec<Finding> {
    if !m.risk.side_effect {
        return Vec::new();
    }

    let Some(template) = m.idempotency_template() else {
        return vec![Finding::violation(
            5,
            "check_effect_has_idempotency_key",
            &m.id,
            "declares side_effect: true but no risk.idempotency.key_template. Retry could duplicate the effect.",
        )];
    };

    let mut findings = Vec::new();
    if template.placeholders().is_empty() {
        findings.push(Finding::violation(
            5,
            "check_effect_has_idempotency_key",
            &m.id,
            format!(
                "idempotency template '{}' has no placeholders, so every invocation would share one key and unrelated effects would deduplicate against each other.",
                template.as_str()
            ),
        ));
    }
    if !template.as_str().contains(':') {
        findings.push(Finding::violation(
            5,
            "check_effect_has_idempotency_key",
            &m.id,
            format!(
                "idempotency template '{}' needs at least two ':'-separated segments ({{effect}}:{{target}}).",
                template.as_str()
            ),
        ));
    }
    findings
}

/// Article 7 — guards fail closed.
///
/// A guard that fails open is not a guard. If it cannot reach its policy source it must
/// deny, because silence is never consent.
pub fn check_guard_fail_closed(m: &CapabilityManifest) -> Vec<Finding> {
    if m.capability_type != CapabilityType::Guard {
        return Vec::new();
    }
    match m.on_error {
        Some(OnError::Deny) => Vec::new(),
        Some(OnError::Allow) => vec![Finding::violation(
            7,
            "check_guard_fail_closed",
            &m.id,
            "is a guard but declares on_error: allow. Guards must fail closed.",
        )],
        None => vec![Finding::violation(
            7,
            "check_guard_fail_closed",
            &m.id,
            "is a guard but does not declare on_error. It must declare on_error: deny.",
        )],
    }
}

/// Article 2 — a capability whose output must be exact needs a verifier.
///
/// Applied to capabilities that produce results claimed to be exact. The check here is
/// structural: a verifier must declare an output schema, otherwise nothing can mechanically
/// interpret its verdict.
pub fn check_verifier_declares_schemas(m: &CapabilityManifest) -> Vec<Finding> {
    if m.capability_type != CapabilityType::Verifier {
        return Vec::new();
    }
    let mut findings = Vec::new();
    if m.schemas.output.is_none() {
        findings.push(Finding::violation(
            2,
            "check_verifier_declares_schemas",
            &m.id,
            "is a verifier but declares no output schema. Its verdict could not be machine-read.",
        ));
    }
    if !m.quality.deterministic {
        findings.push(Finding::violation(
            2,
            "check_verifier_declares_schemas",
            &m.id,
            "is a verifier but declares deterministic: false. A verdict that varies between runs cannot establish verification.",
        ));
    }
    findings
}

/// Article 9 — every capability needs a bounded execution time.
///
/// A capability with no timeout cannot be cancelled on a deadline, so its runtime cannot
/// satisfy the cancellability contract.
pub fn check_has_timeout(m: &CapabilityManifest) -> Vec<Finding> {
    match m.timeout_seconds {
        None => vec![Finding::violation(
            9,
            "check_has_timeout",
            &m.id,
            "declares no timeout_seconds. Work with no deadline cannot be cancelled on one.",
        )],
        Some(0) => vec![Finding::violation(
            9,
            "check_has_timeout",
            &m.id,
            "declares timeout_seconds: 0, which would cancel the work before it starts.",
        )],
        Some(_) => Vec::new(),
    }
}

/// Structural sanity that does not map to a single article.
pub fn check_manifest_coherence(m: &CapabilityManifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    if !m.runs_anywhere() {
        findings.push(Finding::violation(
            10,
            "check_manifest_coherence",
            &m.id,
            "declares neither windows nor linux support, so it can never be scheduled.",
        ));
    }
    if m.id.trim().is_empty() {
        findings.push(Finding::violation(
            10,
            "check_manifest_coherence",
            "<empty>",
            "has an empty id.",
        ));
    }
    if m.description.is_none() {
        findings.push(Finding::warning(
            10,
            "check_manifest_coherence",
            &m.id,
            "has no description. Registry consumers cannot tell what it does.",
        ));
    }
    findings
}

/// Article 2, applied to a task's quality contract.
///
/// This is the same rule the runtime enforces via the Exactness Gate, surfaced at review
/// time so the problem is visible before a task is ever submitted.
pub fn check_task_exactness_has_verifier(
    task_id: &str,
    quality: QualitySpec,
    has_assurance: bool,
) -> Vec<Finding> {
    if quality.gate().blocks() && !has_assurance {
        return vec![Finding::violation(
            2,
            "check_exactness_has_verifier",
            task_id,
            "requires exactness but declares neither deterministic_verification nor any assurance step. It could never reach VERIFIED_SUCCESS.",
        )];
    }
    Vec::new()
}

/// Runs every manifest check.
pub fn check_manifest(m: &CapabilityManifest) -> Vec<Finding> {
    let mut findings = Vec::new();
    findings.extend(check_no_llm_for_deterministic(m));
    findings.extend(check_effect_has_idempotency_key(m));
    findings.extend(check_guard_fail_closed(m));
    findings.extend(check_verifier_declares_schemas(m));
    findings.extend(check_has_timeout(m));
    findings.extend(check_manifest_coherence(m));
    findings
}

/// Runs the gate over a set of manifests.
pub fn run_gate<'a>(manifests: impl IntoIterator<Item = &'a CapabilityManifest>) -> GateReport {
    let mut report = GateReport::default();
    for m in manifests {
        report.inspected += 1;
        report.extend(check_manifest(m));
    }
    report.findings.sort_by(|a, b| {
        a.article
            .cmp(&b.article)
            .then(a.subject.cmp(&b.subject))
            .then(a.detail.cmp(&b.detail))
    });
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{
        Execution, ExecutionKind, Idempotency, Platform, Quality, Risk, Runtime, Schemas,
    };

    fn base() -> CapabilityManifest {
        CapabilityManifest {
            id: "script.example".into(),
            version: 1,
            capability_type: CapabilityType::Script,
            functional_kind: None,
            description: Some("example".into()),
            execution: Execution::new(ExecutionKind::Script, Runtime::Python),
            retry: None,
            quality: Quality {
                deterministic: true,
            },
            risk: Risk {
                side_effect: false,
                idempotency: None,
            },
            platform: Platform {
                windows: true,
                linux: true,
            },
            schemas: Schemas::default(),
            timeout_seconds: Some(60),
            on_error: None,
        }
    }

    #[test]
    fn a_clean_script_manifest_passes() {
        assert!(check_manifest(&base()).is_empty());
    }

    #[test]
    fn article_1_deterministic_agent_is_a_violation() {
        let mut m = base();
        m.execution = Execution::new(ExecutionKind::Agent, Runtime::ClaudeCode);

        let findings = check_no_llm_for_deterministic(&m);
        assert_eq!(
            findings.len(),
            2,
            "both kind and runtime are wrong: {findings:?}"
        );
        assert!(findings.iter().all(|f| f.article == Article(1)));
        assert!(findings.iter().all(Finding::is_violation));
    }

    #[test]
    fn article_1_non_deterministic_agent_is_fine() {
        let mut m = base();
        m.quality = Quality {
            deterministic: false,
        };
        m.execution = Execution::new(ExecutionKind::Agent, Runtime::ClaudeCode);
        assert!(check_no_llm_for_deterministic(&m).is_empty());
    }

    #[test]
    fn article_5_side_effect_without_key_is_a_violation() {
        let mut m = base();
        m.quality = Quality {
            deterministic: false,
        };
        m.risk = Risk {
            side_effect: true,
            idempotency: None,
        };

        let findings = check_effect_has_idempotency_key(&m);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].article, Article(5));
        assert!(findings[0].detail.contains("duplicate"));
    }

    #[test]
    fn article_5_side_effect_with_good_key_passes() {
        let mut m = base();
        m.quality = Quality {
            deterministic: false,
        };
        m.risk = Risk {
            side_effect: true,
            idempotency: Some(Idempotency {
                key_template: "ntfy:{channel}:{date}".into(),
            }),
        };
        assert!(check_effect_has_idempotency_key(&m).is_empty());
    }

    #[test]
    fn article_5_constant_key_is_a_violation() {
        // A key with no placeholders would collapse every invocation into one effect.
        let mut m = base();
        m.quality = Quality {
            deterministic: false,
        };
        m.risk = Risk {
            side_effect: true,
            idempotency: Some(Idempotency {
                key_template: "ntfy:constant".into(),
            }),
        };

        let findings = check_effect_has_idempotency_key(&m);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("no placeholders"));
    }

    #[test]
    fn article_5_single_segment_key_is_a_violation() {
        let mut m = base();
        m.quality = Quality {
            deterministic: false,
        };
        m.risk = Risk {
            side_effect: true,
            idempotency: Some(Idempotency {
                key_template: "{id}".into(),
            }),
        };
        let findings = check_effect_has_idempotency_key(&m);
        assert!(findings
            .iter()
            .any(|f| f.detail.contains("two ':'-separated")));
    }

    #[test]
    fn article_5_ignores_capabilities_with_no_side_effect() {
        assert!(check_effect_has_idempotency_key(&base()).is_empty());
    }

    #[test]
    fn article_7_guard_must_declare_deny() {
        let mut m = base();
        m.capability_type = CapabilityType::Guard;

        // Missing on_error.
        let findings = check_guard_fail_closed(&m);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].article, Article(7));

        // Explicitly fail-open.
        m.on_error = Some(OnError::Allow);
        let findings = check_guard_fail_closed(&m);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("fail closed"));

        // Correct.
        m.on_error = Some(OnError::Deny);
        assert!(check_guard_fail_closed(&m).is_empty());
    }

    #[test]
    fn article_7_does_not_apply_to_non_guards() {
        let mut m = base();
        m.on_error = Some(OnError::Allow);
        assert!(
            check_guard_fail_closed(&m).is_empty(),
            "a hook may legitimately fail open"
        );
    }

    #[test]
    fn article_2_verifier_needs_an_output_schema() {
        let mut m = base();
        m.capability_type = CapabilityType::Verifier;

        let findings = check_verifier_declares_schemas(&m);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("output schema"));

        m.schemas.output = Some("verification-result-v1".into());
        assert!(check_verifier_declares_schemas(&m).is_empty());
    }

    #[test]
    fn article_2_verifier_must_be_deterministic() {
        let mut m = base();
        m.capability_type = CapabilityType::Verifier;
        m.schemas.output = Some("verification-result-v1".into());
        m.quality = Quality {
            deterministic: false,
        };

        let findings = check_verifier_declares_schemas(&m);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("varies between runs"));
    }

    #[test]
    fn article_9_timeout_is_required() {
        let mut m = base();
        m.timeout_seconds = None;
        assert_eq!(check_has_timeout(&m).len(), 1);

        m.timeout_seconds = Some(0);
        assert!(check_has_timeout(&m)[0].detail.contains("before it starts"));

        m.timeout_seconds = Some(1);
        assert!(check_has_timeout(&m).is_empty());
    }

    #[test]
    fn a_capability_for_no_platform_is_rejected() {
        let mut m = base();
        m.platform = Platform {
            windows: false,
            linux: false,
        };
        let findings = check_manifest_coherence(&m);
        assert!(findings.iter().any(|f| f.is_violation()));
    }

    #[test]
    fn a_missing_description_is_only_a_warning() {
        let mut m = base();
        m.description = None;
        let findings = check_manifest_coherence(&m);
        assert_eq!(findings.len(), 1);
        assert!(
            !findings[0].is_violation(),
            "documentation is not a Constitution matter"
        );
    }

    #[test]
    fn article_2_task_exactness_without_assurance_is_a_violation() {
        let findings =
            check_task_exactness_has_verifier("t1", QualitySpec::exact_but_unverifiable(), false);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].article, Article(2));
    }

    #[test]
    fn article_2_task_exactness_with_assurance_passes() {
        assert!(check_task_exactness_has_verifier(
            "t1",
            QualitySpec::exact_but_unverifiable(),
            true
        )
        .is_empty());
    }

    #[test]
    fn article_2_verifiable_task_needs_no_assurance_declaration() {
        assert!(
            check_task_exactness_has_verifier("t1", QualitySpec::mechanical(), false).is_empty()
        );
    }

    #[test]
    fn gate_passes_on_clean_manifests() {
        let m = base();
        let report = run_gate([&m]);
        assert!(report.passed());
        assert_eq!(report.inspected, 1);
        assert_eq!(report.violation_count(), 0);
    }

    #[test]
    fn gate_fails_and_reports_every_violation() {
        let mut bad = base();
        bad.id = "bad.capability".into();
        bad.execution = Execution::new(ExecutionKind::Agent, Runtime::ClaudeCode);
        bad.risk = Risk {
            side_effect: true,
            idempotency: None,
        };
        bad.timeout_seconds = None;

        let good = base();
        let report = run_gate([&bad, &good]);

        assert!(!report.passed());
        assert_eq!(report.inspected, 2);
        // Article 1 (x2), Article 5, Article 9.
        assert_eq!(report.violation_count(), 4, "{:#?}", report.findings);
        assert!(report.violations().all(|f| f.subject == "bad.capability"));
    }

    #[test]
    fn gate_output_is_ordered_deterministically() {
        let mut a = base();
        a.id = "z.capability".into();
        a.timeout_seconds = None;
        let mut b = base();
        b.id = "a.capability".into();
        b.timeout_seconds = None;

        let first = run_gate([&a, &b]);
        let second = run_gate([&b, &a]);
        assert_eq!(
            first.findings, second.findings,
            "gate output must not depend on input order"
        );
    }

    #[test]
    fn an_empty_gate_run_is_distinguishable_from_a_clean_one() {
        let report = run_gate(std::iter::empty());
        assert!(report.passed());
        assert_eq!(
            report.inspected, 0,
            "a run that checked nothing must say so"
        );
    }

    #[test]
    fn findings_render_readably() {
        let mut m = base();
        m.timeout_seconds = None;
        let rendered = check_has_timeout(&m)[0].to_string();
        assert!(rendered.contains("VIOLATION"));
        assert!(rendered.contains("Article 9"));
        assert!(rendered.contains("check_has_timeout"));
        assert!(rendered.contains("script.example"));
    }
}
