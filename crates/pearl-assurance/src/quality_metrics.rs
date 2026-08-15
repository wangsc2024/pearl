//! # Quality Metrics
//!
//! 系統開發需求書 §71 -- mechanical quality metrics collection.
//!
//! `QualityMetrics` tracks:
//! - `mechanical_coverage`: percentage of tasks routed to P0/script execution
//! - `verification_coverage`: percentage of completed tasks with evidence
//!
//! These are computed from the state store and provide a mechanical health signal.

use serde::{Deserialize, Serialize};

/// Quality metrics for the system.
///
/// All values are percentages (0.0 to 100.0).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityMetrics {
    /// Percentage of tasks routed to mechanical (P0/script) execution.
    pub mechanical_coverage: f64,
    /// Percentage of completed tasks that have verification evidence.
    pub verification_coverage: f64,
    /// Total tasks considered in this measurement.
    pub total_tasks: u64,
    /// Tasks routed to scripts.
    pub mechanical_tasks: u64,
    /// Tasks with verification evidence.
    pub verified_tasks: u64,
}

impl QualityMetrics {
    /// Compute quality metrics from raw counts.
    pub fn compute(total_tasks: u64, mechanical_tasks: u64, verified_tasks: u64) -> Self {
        let mechanical_coverage = if total_tasks > 0 {
            (mechanical_tasks as f64 / total_tasks as f64) * 100.0
        } else {
            0.0
        };
        let verification_coverage = if total_tasks > 0 {
            (verified_tasks as f64 / total_tasks as f64) * 100.0
        } else {
            0.0
        };
        Self {
            mechanical_coverage,
            verification_coverage,
            total_tasks,
            mechanical_tasks,
            verified_tasks,
        }
    }

    /// Whether the system meets minimum quality thresholds.
    ///
    /// Default thresholds: mechanical >= 60%, verification >= 80%.
    pub fn meets_minimum(&self) -> bool {
        self.mechanical_coverage >= 60.0 && self.verification_coverage >= 80.0
    }

    /// Whether the system meets target quality thresholds.
    ///
    /// Target thresholds: mechanical >= 80%, verification >= 95%.
    pub fn meets_target(&self) -> bool {
        self.mechanical_coverage >= 80.0 && self.verification_coverage >= 95.0
    }
}

impl Default for QualityMetrics {
    fn default() -> Self {
        Self {
            mechanical_coverage: 0.0,
            verification_coverage: 0.0,
            total_tasks: 0,
            mechanical_tasks: 0,
            verified_tasks: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_with_zero_tasks() {
        let metrics = QualityMetrics::compute(0, 0, 0);
        assert_eq!(metrics.mechanical_coverage, 0.0);
        assert_eq!(metrics.verification_coverage, 0.0);
    }

    #[test]
    fn compute_percentages_correctly() {
        let metrics = QualityMetrics::compute(100, 80, 95);
        assert_eq!(metrics.mechanical_coverage, 80.0);
        assert_eq!(metrics.verification_coverage, 95.0);
    }

    #[test]
    fn meets_minimum_thresholds() {
        let good = QualityMetrics::compute(100, 70, 85);
        assert!(good.meets_minimum());

        let bad = QualityMetrics::compute(100, 50, 70);
        assert!(!bad.meets_minimum());
    }

    #[test]
    fn meets_target_thresholds() {
        let excellent = QualityMetrics::compute(100, 85, 96);
        assert!(excellent.meets_target());

        let good_not_excellent = QualityMetrics::compute(100, 75, 90);
        assert!(!good_not_excellent.meets_target());
    }

    #[test]
    fn partial_coverage() {
        let metrics = QualityMetrics::compute(10, 3, 7);
        assert!((metrics.mechanical_coverage - 30.0).abs() < 0.01);
        assert!((metrics.verification_coverage - 70.0).abs() < 0.01);
    }
}
