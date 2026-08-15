//! # pearl-worker
//!
//! The PEARL worker process -- 系統開發需求書 §57.
//!
//! A worker pulls tasks from the queue, acquires a lease, executes the task via
//! the appropriate runtime adapter, and records the outcome. Multiple workers can
//! run concurrently; the lease mechanism prevents double-execution.
//!
//! This crate provides the worker loop and task execution coordination.
//! It delegates actual script execution to `pearl-runtime`.

use chrono::{DateTime, Utc};
use pearl_core::{Clock, TaskId, WorkerId};
use serde::{Deserialize, Serialize};

/// Configuration for a worker instance.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Unique identifier for this worker.
    pub worker_id: WorkerId,
    /// How many tasks this worker can process concurrently.
    pub concurrency: usize,
    /// How often to poll the queue for new work (milliseconds).
    pub poll_interval_ms: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: WorkerId::new("worker:default"),
            concurrency: 1,
            poll_interval_ms: 1000,
        }
    }
}

/// The result of a worker processing a single task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResult {
    /// The task that was processed.
    pub task_id: TaskId,
    /// Whether execution succeeded.
    pub success: bool,
    /// Exit code or status information.
    pub detail: String,
    /// When processing started.
    pub started_at: DateTime<Utc>,
    /// When processing completed.
    pub completed_at: DateTime<Utc>,
}

/// Worker errors.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("no tasks available")]
    NoWork,
    #[error("lease acquisition failed: {detail}")]
    LeaseError { detail: String },
    #[error("execution failed: {detail}")]
    ExecutionError { detail: String },
    #[error("state store error: {0}")]
    StateError(String),
}

/// The worker engine.
///
/// In a full deployment this would run as a long-lived process pulling from the
/// queue. Here it provides the core execute-one-task method that the daemon or
/// a test harness can drive.
pub struct Worker {
    config: WorkerConfig,
}

impl Worker {
    /// Create a new worker with the given configuration.
    pub fn new(config: WorkerConfig) -> Self {
        Self { config }
    }

    /// Get this worker's identity.
    pub fn worker_id(&self) -> &WorkerId {
        &self.config.worker_id
    }

    /// Get the configured concurrency level.
    pub fn concurrency(&self) -> usize {
        self.config.concurrency
    }

    /// Simulate processing a task (stub implementation).
    ///
    /// In production this would:
    /// 1. Acquire a lease from the state store
    /// 2. Route the task via pearl-router
    /// 3. Execute via pearl-runtime
    /// 4. Record the outcome
    ///
    /// This stub validates the worker lifecycle without requiring a full runtime.
    pub fn process_task(
        &self,
        task_id: &TaskId,
        clock: &dyn Clock,
    ) -> Result<WorkResult, WorkerError> {
        let started_at = clock.now();

        // Stub: in production, would acquire lease, route, execute.
        let completed_at = clock.now();

        Ok(WorkResult {
            task_id: task_id.clone(),
            success: true,
            detail: format!("worker {} processed task (stub)", self.config.worker_id),
            started_at,
            completed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pearl_core::SystemClock;

    #[test]
    fn worker_creates_with_default_config() {
        let worker = Worker::new(WorkerConfig::default());
        assert_eq!(worker.concurrency(), 1);
    }

    #[test]
    fn worker_processes_task_stub() {
        let worker = Worker::new(WorkerConfig::default());
        let task_id = TaskId::parse("test-task-001".to_string()).unwrap();
        let result = worker.process_task(&task_id, &SystemClock).unwrap();
        assert!(result.success);
        assert_eq!(result.task_id, task_id);
    }

    #[test]
    fn worker_id_is_accessible() {
        let config = WorkerConfig {
            worker_id: WorkerId::new("worker:test-42"),
            ..WorkerConfig::default()
        };
        let worker = Worker::new(config);
        assert_eq!(worker.worker_id().as_str(), "worker:test-42");
    }
}
