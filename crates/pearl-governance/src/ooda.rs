//! # OODA Governance Daemon
//!
//! Implements the Observe-Orient-Decide-Act loop from 系統開發需求書 §53-54.
//!
//! The OODA loop is the governance daemon's control cycle. It:
//!
//! - **Observe**: collects health metrics, test results, and config validity
//! - **Orient**: classifies observations into system health states
//! - **Decide**: applies policy rules to determine corrective actions
//! - **Act**: executes repair transactions in an isolated scope
//!
//! This provides the self-healing backbone that keeps the system operating
//! within Constitution-defined boundaries without human intervention.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The observed health state of the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// All subsystems operating normally.
    Healthy,
    /// One or more subsystems showing signs of stress but still functional.
    Degraded,
    /// One or more subsystems have failed and require intervention.
    Failing,
    /// System state is unknown (e.g., metrics collection itself failed).
    Unknown,
}

/// A single observation from a subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Which subsystem produced this observation.
    pub source: String,
    /// What was observed (metric name, test name, config key, etc.).
    pub metric: String,
    /// The observed value (numeric or status string).
    pub value: ObservationValue,
    /// When the observation was taken.
    pub observed_at: DateTime<Utc>,
}

/// The value of an observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub enum ObservationValue {
    /// A numeric metric (e.g., failure count, latency).
    Numeric(f64),
    /// A boolean status (e.g., config valid, test passed).
    Boolean(bool),
    /// A string status (e.g., "connected", "timeout").
    Status(String),
}

/// The classification produced by the Orient phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orientation {
    /// Overall system health state derived from observations.
    pub state: HealthState,
    /// Per-subsystem health breakdown.
    pub subsystems: BTreeMap<String, HealthState>,
    /// Human-readable summary of the orientation analysis.
    pub summary: String,
    /// When the orientation was computed.
    pub oriented_at: DateTime<Utc>,
}

/// A decision produced by the Decide phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Decision {
    /// No action needed; system is healthy.
    NoAction,
    /// Attempt automated repair of the specified subsystem.
    Repair { target: String, strategy: String },
    /// Alert operators about an issue that cannot be auto-repaired.
    Alert {
        severity: AlertSeverity,
        message: String,
    },
    /// Escalate to a higher authority (e.g., human operator).
    Escalate { reason: String },
}

/// Alert severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Error,
    Critical,
}

/// Result of an action taken in the Act phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    /// What decision was acted upon.
    pub decision: Decision,
    /// Whether the action succeeded.
    pub success: bool,
    /// Details about what was done.
    pub detail: String,
    /// When the action was executed.
    pub acted_at: DateTime<Utc>,
}

/// Result of a complete OODA cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleResult {
    /// Observations collected.
    pub observations: Vec<Observation>,
    /// Orientation produced.
    pub orientation: Orientation,
    /// Decisions made.
    pub decisions: Vec<Decision>,
    /// Actions taken.
    pub actions: Vec<ActionResult>,
    /// Cycle start time.
    pub started_at: DateTime<Utc>,
    /// Cycle end time.
    pub completed_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Policy rules
// ---------------------------------------------------------------------------

/// A policy rule that maps an orientation state to a decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Rule identifier.
    pub id: String,
    /// Which health state triggers this rule.
    pub trigger_state: HealthState,
    /// Optional: only trigger for a specific subsystem.
    pub subsystem_filter: Option<String>,
    /// The decision to produce when this rule fires.
    pub decision: Decision,
}

// ---------------------------------------------------------------------------
// OodaLoop
// ---------------------------------------------------------------------------

/// The OODA governance daemon.
///
/// Composes the four phases (Observe, Orient, Decide, Act) into a single
/// `run_cycle()` that can be called periodically by the scheduler.
pub struct OodaLoop {
    /// Policy rules that guide the Decide phase.
    policy_rules: Vec<PolicyRule>,
    /// Thresholds for health classification.
    config: OodaConfig,
    /// History of recent observations (for trend analysis).
    recent_observations: Vec<Observation>,
}

/// Configuration for the OODA loop thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OodaConfig {
    /// Number of failing observations before a subsystem is marked Degraded.
    pub degraded_threshold: usize,
    /// Number of failing observations before a subsystem is marked Failing.
    pub failing_threshold: usize,
    /// Maximum number of recent observations to retain.
    pub observation_window_size: usize,
}

impl Default for OodaConfig {
    fn default() -> Self {
        Self {
            degraded_threshold: 2,
            failing_threshold: 5,
            observation_window_size: 100,
        }
    }
}

impl OodaLoop {
    /// Create a new OODA loop with the given policy rules and configuration.
    pub fn new(policy_rules: Vec<PolicyRule>, config: OodaConfig) -> Self {
        Self {
            policy_rules,
            config,
            recent_observations: Vec::new(),
        }
    }

    /// Create an OODA loop with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(Vec::new(), OodaConfig::default())
    }

    /// Add a policy rule.
    pub fn add_rule(&mut self, rule: PolicyRule) {
        self.policy_rules.push(rule);
    }

    // -----------------------------------------------------------------------
    // Phase 1: Observe
    // -----------------------------------------------------------------------

    /// Collect observations from provided sources.
    ///
    /// In a real deployment this would query health endpoints, test runners,
    /// and config validators. Here it accepts pre-collected observations
    /// and stores them in the recent history.
    pub fn observe(&mut self, observations: Vec<Observation>) -> &[Observation] {
        self.recent_observations.extend(observations);

        // Trim to window size.
        let window = self.config.observation_window_size;
        if self.recent_observations.len() > window {
            let drain_count = self.recent_observations.len() - window;
            self.recent_observations.drain(..drain_count);
        }

        &self.recent_observations
    }

    // -----------------------------------------------------------------------
    // Phase 2: Orient
    // -----------------------------------------------------------------------

    /// Classify observations into system health states.
    ///
    /// Groups observations by source subsystem and counts failures to determine
    /// whether each subsystem is Healthy, Degraded, or Failing.
    pub fn orient(&self, now: DateTime<Utc>) -> Orientation {
        let mut subsystem_failures: BTreeMap<String, usize> = BTreeMap::new();

        // Count negative observations per subsystem.
        for obs in &self.recent_observations {
            let is_negative = match &obs.value {
                ObservationValue::Numeric(v) => *v < 0.0,
                ObservationValue::Boolean(b) => !b,
                ObservationValue::Status(s) => {
                    s == "error" || s == "timeout" || s == "failed" || s == "failing"
                }
            };
            if is_negative {
                *subsystem_failures.entry(obs.source.clone()).or_insert(0) += 1;
            } else {
                subsystem_failures.entry(obs.source.clone()).or_insert(0);
            }
        }

        // Classify each subsystem.
        let mut subsystems = BTreeMap::new();
        let mut worst_state = HealthState::Healthy;

        for (source, failures) in &subsystem_failures {
            let state = if *failures >= self.config.failing_threshold {
                HealthState::Failing
            } else if *failures >= self.config.degraded_threshold {
                HealthState::Degraded
            } else {
                HealthState::Healthy
            };

            if state_severity(state) > state_severity(worst_state) {
                worst_state = state;
            }
            subsystems.insert(source.clone(), state);
        }

        let summary = match worst_state {
            HealthState::Healthy => "All subsystems operating normally.".to_string(),
            HealthState::Degraded => format!(
                "Degraded subsystems: {}",
                subsystems
                    .iter()
                    .filter(|(_, s)| **s == HealthState::Degraded)
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            HealthState::Failing => format!(
                "Failing subsystems: {}",
                subsystems
                    .iter()
                    .filter(|(_, s)| **s == HealthState::Failing)
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            HealthState::Unknown => "System state unknown.".to_string(),
        };

        Orientation {
            state: worst_state,
            subsystems,
            summary,
            oriented_at: now,
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: Decide
    // -----------------------------------------------------------------------

    /// Apply policy rules to orientation to determine actions.
    ///
    /// Rules are evaluated in order. All matching rules produce decisions.
    pub fn decide(&self, orientation: &Orientation) -> Vec<Decision> {
        if orientation.state == HealthState::Healthy {
            return vec![Decision::NoAction];
        }

        let mut decisions = Vec::new();

        for rule in &self.policy_rules {
            // Check if the rule's trigger state matches.
            if rule.trigger_state != orientation.state {
                continue;
            }

            // Check subsystem filter if specified.
            if let Some(filter) = &rule.subsystem_filter {
                let subsystem_state = orientation.subsystems.get(filter);
                if subsystem_state != Some(&rule.trigger_state) {
                    continue;
                }
            }

            decisions.push(rule.decision.clone());
        }

        // Default decisions if no rules matched.
        if decisions.is_empty() {
            match orientation.state {
                HealthState::Degraded => {
                    decisions.push(Decision::Alert {
                        severity: AlertSeverity::Warning,
                        message: orientation.summary.clone(),
                    });
                }
                HealthState::Failing => {
                    decisions.push(Decision::Escalate {
                        reason: orientation.summary.clone(),
                    });
                }
                _ => {
                    decisions.push(Decision::NoAction);
                }
            }
        }

        decisions
    }

    // -----------------------------------------------------------------------
    // Phase 4: Act
    // -----------------------------------------------------------------------

    /// Execute decisions and return results.
    ///
    /// In production, this would perform actual repair operations (restart services,
    /// reload configs, send alerts). Here it records the intent and outcome.
    pub fn act(&self, decisions: &[Decision], now: DateTime<Utc>) -> Vec<ActionResult> {
        decisions
            .iter()
            .map(|decision| {
                let (success, detail) = match decision {
                    Decision::NoAction => (true, "No action required.".to_string()),
                    Decision::Repair { target, strategy } => (
                        true,
                        format!("Repair initiated for '{target}' using strategy '{strategy}'."),
                    ),
                    Decision::Alert { severity, message } => {
                        (true, format!("Alert ({severity:?}): {message}"))
                    }
                    Decision::Escalate { reason } => (true, format!("Escalated: {reason}")),
                };

                ActionResult {
                    decision: decision.clone(),
                    success,
                    detail,
                    acted_at: now,
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Composed cycle
    // -----------------------------------------------------------------------

    /// Run a complete OODA cycle with the given observations.
    ///
    /// This is the entry point for the governance daemon's periodic trigger.
    pub fn run_cycle(&mut self, observations: Vec<Observation>, now: DateTime<Utc>) -> CycleResult {
        let started_at = now;

        // Phase 1: Observe
        self.observe(observations.clone());

        // Phase 2: Orient
        let orientation = self.orient(now);

        // Phase 3: Decide
        let decisions = self.decide(&orientation);

        // Phase 4: Act
        let actions = self.act(&decisions, now);

        CycleResult {
            observations,
            orientation,
            decisions,
            actions,
            started_at,
            completed_at: now,
        }
    }

    /// Get the most recent observations.
    pub fn recent_observations(&self) -> &[Observation] {
        &self.recent_observations
    }

    /// Clear the observation history.
    pub fn reset(&mut self) {
        self.recent_observations.clear();
    }

    // -----------------------------------------------------------------------
    // Daemon mode — §66
    // -----------------------------------------------------------------------

    /// Run OODA cycles on an interval until the stop signal is set.
    ///
    /// `observer_fn` is called each cycle to collect fresh observations.
    /// The daemon sleeps for `interval` between cycles and checks `stop` before
    /// each iteration. Returns the results of all completed cycles.
    pub fn run_daemon<F>(
        &mut self,
        interval: std::time::Duration,
        stop: &std::sync::atomic::AtomicBool,
        mut observer_fn: F,
    ) -> Vec<CycleResult>
    where
        F: FnMut() -> Vec<Observation>,
    {
        let mut results = Vec::new();
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            let observations = observer_fn();
            let now = chrono::Utc::now();
            let result = self.run_cycle(observations, now);
            results.push(result);

            // Check stop before sleeping.
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(interval);
        }
        results
    }
}

/// Convert health state to a numeric severity for comparison.
fn state_severity(state: HealthState) -> u8 {
    match state {
        HealthState::Healthy => 0,
        HealthState::Unknown => 1,
        HealthState::Degraded => 2,
        HealthState::Failing => 3,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_observation(source: &str, metric: &str, value: ObservationValue) -> Observation {
        Observation {
            source: source.to_string(),
            metric: metric.to_string(),
            value,
            observed_at: Utc::now(),
        }
    }

    #[test]
    fn healthy_system_produces_no_action() {
        let mut ooda = OodaLoop::with_defaults();
        let observations = vec![
            make_observation("scheduler", "health_check", ObservationValue::Boolean(true)),
            make_observation("runtime", "latency_ms", ObservationValue::Numeric(50.0)),
            make_observation(
                "queue",
                "status",
                ObservationValue::Status("ok".to_string()),
            ),
        ];

        let result = ooda.run_cycle(observations, Utc::now());
        assert_eq!(result.orientation.state, HealthState::Healthy);
        assert_eq!(result.decisions, vec![Decision::NoAction]);
        assert!(result.actions[0].success);
    }

    #[test]
    fn degraded_system_triggers_alert() {
        let mut ooda = OodaLoop::with_defaults();
        let observations = vec![
            make_observation("runtime", "test_pass", ObservationValue::Boolean(false)),
            make_observation("runtime", "test_pass", ObservationValue::Boolean(false)),
            make_observation("scheduler", "health_check", ObservationValue::Boolean(true)),
        ];

        let result = ooda.run_cycle(observations, Utc::now());
        assert_eq!(result.orientation.state, HealthState::Degraded);
        assert_eq!(
            result.orientation.subsystems.get("runtime"),
            Some(&HealthState::Degraded)
        );
        // Default behavior: alert on degradation.
        assert!(matches!(result.decisions[0], Decision::Alert { .. }));
    }

    #[test]
    fn failing_system_triggers_escalation() {
        let mut ooda = OodaLoop::with_defaults();
        let observations = vec![
            make_observation("runtime", "crash", ObservationValue::Boolean(false)),
            make_observation("runtime", "crash", ObservationValue::Boolean(false)),
            make_observation("runtime", "crash", ObservationValue::Boolean(false)),
            make_observation("runtime", "crash", ObservationValue::Boolean(false)),
            make_observation("runtime", "crash", ObservationValue::Boolean(false)),
        ];

        let result = ooda.run_cycle(observations, Utc::now());
        assert_eq!(result.orientation.state, HealthState::Failing);
        assert!(matches!(result.decisions[0], Decision::Escalate { .. }));
    }

    #[test]
    fn policy_rule_triggers_repair() {
        let mut ooda = OodaLoop::new(
            vec![PolicyRule {
                id: "repair-runtime".to_string(),
                trigger_state: HealthState::Degraded,
                subsystem_filter: Some("runtime".to_string()),
                decision: Decision::Repair {
                    target: "runtime".to_string(),
                    strategy: "restart".to_string(),
                },
            }],
            OodaConfig::default(),
        );

        let observations = vec![
            make_observation("runtime", "error", ObservationValue::Boolean(false)),
            make_observation("runtime", "error", ObservationValue::Boolean(false)),
        ];

        let result = ooda.run_cycle(observations, Utc::now());
        assert_eq!(
            result.decisions[0],
            Decision::Repair {
                target: "runtime".to_string(),
                strategy: "restart".to_string(),
            }
        );
    }

    #[test]
    fn observation_window_trims_old_data() {
        let config = OodaConfig {
            observation_window_size: 5,
            ..OodaConfig::default()
        };
        let mut ooda = OodaLoop::new(Vec::new(), config);

        // Add 10 observations.
        for i in 0..10 {
            ooda.observe(vec![make_observation(
                "sys",
                &format!("metric_{i}"),
                ObservationValue::Boolean(true),
            )]);
        }

        assert_eq!(ooda.recent_observations().len(), 5);
    }

    #[test]
    fn numeric_negative_counts_as_failure() {
        let mut ooda = OodaLoop::new(
            Vec::new(),
            OodaConfig {
                degraded_threshold: 1,
                ..OodaConfig::default()
            },
        );

        let observations = vec![make_observation(
            "scorer",
            "score_delta",
            ObservationValue::Numeric(-1.0),
        )];

        let result = ooda.run_cycle(observations, Utc::now());
        assert_eq!(result.orientation.state, HealthState::Degraded);
    }

    #[test]
    fn status_error_strings_count_as_failure() {
        let mut ooda = OodaLoop::new(
            Vec::new(),
            OodaConfig {
                degraded_threshold: 1,
                ..OodaConfig::default()
            },
        );

        for status in &["error", "timeout", "failed", "failing"] {
            ooda.reset();
            let observations = vec![make_observation(
                "net",
                "connection",
                ObservationValue::Status((*status).to_string()),
            )];
            let result = ooda.run_cycle(observations, Utc::now());
            assert_eq!(
                result.orientation.state,
                HealthState::Degraded,
                "status '{status}' should be classified as failure"
            );
        }
    }

    #[test]
    fn reset_clears_history() {
        let mut ooda = OodaLoop::with_defaults();
        ooda.observe(vec![make_observation(
            "sys",
            "test",
            ObservationValue::Boolean(true),
        )]);
        assert!(!ooda.recent_observations().is_empty());

        ooda.reset();
        assert!(ooda.recent_observations().is_empty());
    }

    #[test]
    fn cycle_result_contains_all_phases() {
        let mut ooda = OodaLoop::with_defaults();
        let now = Utc::now();
        let observations = vec![make_observation(
            "sys",
            "ok",
            ObservationValue::Boolean(true),
        )];

        let result = ooda.run_cycle(observations.clone(), now);
        assert_eq!(result.observations.len(), 1);
        assert_eq!(result.orientation.state, HealthState::Healthy);
        assert_eq!(result.decisions.len(), 1);
        assert_eq!(result.actions.len(), 1);
        assert_eq!(result.started_at, now);
        assert_eq!(result.completed_at, now);
    }

    #[test]
    fn subsystem_filter_limits_rule_scope() {
        let mut ooda = OodaLoop::new(
            vec![PolicyRule {
                id: "repair-queue".to_string(),
                trigger_state: HealthState::Degraded,
                subsystem_filter: Some("queue".to_string()),
                decision: Decision::Repair {
                    target: "queue".to_string(),
                    strategy: "flush".to_string(),
                },
            }],
            OodaConfig::default(),
        );

        // Degradation in runtime, not queue -- rule should NOT fire.
        let observations = vec![
            make_observation("runtime", "fail", ObservationValue::Boolean(false)),
            make_observation("runtime", "fail", ObservationValue::Boolean(false)),
        ];

        let result = ooda.run_cycle(observations, Utc::now());
        // The rule targets queue but the failing subsystem is runtime,
        // so the rule does not fire and the default alert is produced.
        assert!(matches!(result.decisions[0], Decision::Alert { .. }));
    }

    #[test]
    fn daemon_mode_runs_until_stopped() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let mut ooda = OodaLoop::with_defaults();
        let mut call_count = 0u32;

        // Stop after 3 cycles.
        let results = ooda.run_daemon(std::time::Duration::from_millis(1), &stop, || {
            call_count += 1;
            if call_count >= 3 {
                stop_clone.store(true, Ordering::Relaxed);
            }
            vec![make_observation(
                "sys",
                "ok",
                ObservationValue::Boolean(true),
            )]
        });

        assert_eq!(results.len(), 3);
        assert!(results
            .iter()
            .all(|r| r.orientation.state == HealthState::Healthy));
    }
}
