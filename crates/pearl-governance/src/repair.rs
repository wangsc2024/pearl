//! # Repair Transaction
//!
//! Implements the Finding -> Proposal -> IsolatedWorkspace -> Apply -> Verify ->
//! Promote -> Commit/Rollback pattern from 系統開發需求書 §54.
//!
//! The OODA loop's Act phase discovers a defect and proposes a repair. Rather than
//! applying directly to the live system, the repair is first attempted in an isolated
//! workspace (a tempdir). Only after verification passes is the change promoted to
//! the production path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A finding that triggers a repair attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Which subsystem discovered the issue.
    pub source: String,
    /// Short description of the problem.
    pub summary: String,
    /// The severity of the finding.
    pub severity: FindingSeverity,
    /// When the finding was discovered.
    pub discovered_at: DateTime<Utc>,
}

/// Severity of a repair finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// A proposal for how to address a finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// The finding this proposal addresses.
    pub finding: Finding,
    /// Human-readable description of the proposed fix.
    pub description: String,
    /// The repair strategy to apply.
    pub strategy: RepairStrategy,
}

/// Strategies the repair engine knows how to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStrategy {
    /// Replace a configuration file with corrected content.
    ReplaceConfig { target: String, content: String },
    /// Remove a corrupt/stale file.
    RemoveFile { target: String },
    /// Write a new file that was missing.
    CreateFile { target: String, content: String },
    /// Custom strategy carried as opaque JSON.
    Custom { kind: String, payload: String },
}

/// Outcome of a repair transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairOutcome {
    /// Repair was applied, verified, and promoted successfully.
    Committed,
    /// Repair failed verification and was rolled back.
    RolledBack { reason: String },
    /// Repair could not be attempted (e.g., workspace creation failed).
    Aborted { reason: String },
}

/// The repair transaction engine.
///
/// Executes the full lifecycle: isolated workspace creation, apply, verify, promote
/// or rollback. The production path is never modified until verification passes.
pub struct RepairTransaction {
    /// The production directory that will receive the promoted change.
    production_path: PathBuf,
    /// Temporary directory used as the isolated workspace.
    workspace: Option<tempfile::TempDir>,
}

impl RepairTransaction {
    /// Create a new repair transaction targeting the given production path.
    pub fn new(production_path: impl Into<PathBuf>) -> Self {
        Self {
            production_path: production_path.into(),
            workspace: None,
        }
    }

    /// Execute the full repair lifecycle for a proposal.
    ///
    /// Steps:
    /// 1. Create isolated workspace (tempdir)
    /// 2. Apply the proposed change in the workspace
    /// 3. Verify the workspace is consistent
    /// 4. Promote (copy to production) or Rollback
    ///
    /// The workspace is always cleaned up regardless of outcome.
    pub fn execute(&mut self, proposal: &Proposal, verifier: &dyn RepairVerifier) -> RepairOutcome {
        // Step 1: Create isolated workspace
        let workspace = match tempfile::TempDir::new() {
            Ok(dir) => dir,
            Err(e) => {
                return RepairOutcome::Aborted {
                    reason: format!("failed to create isolated workspace: {e}"),
                }
            }
        };
        let workspace_path = workspace.path().to_path_buf();
        self.workspace = Some(workspace);

        // Step 2: Apply in isolated workspace
        if let Err(e) = self.apply_in_workspace(&workspace_path, &proposal.strategy) {
            return RepairOutcome::Aborted {
                reason: format!("apply failed in workspace: {e}"),
            };
        }

        // Step 3: Verify
        match verifier.verify(&workspace_path, proposal) {
            Ok(true) => {}
            Ok(false) => {
                return RepairOutcome::RolledBack {
                    reason: "verification returned false".to_string(),
                };
            }
            Err(e) => {
                return RepairOutcome::RolledBack {
                    reason: format!("verification error: {e}"),
                };
            }
        }

        // Step 4: Promote to production
        if let Err(e) = self.promote(&workspace_path, &proposal.strategy) {
            return RepairOutcome::RolledBack {
                reason: format!("promotion failed: {e}"),
            };
        }

        // Commit - workspace is dropped automatically
        RepairOutcome::Committed
    }

    /// Apply a repair strategy within the isolated workspace.
    fn apply_in_workspace(
        &self,
        workspace: &Path,
        strategy: &RepairStrategy,
    ) -> Result<(), String> {
        match strategy {
            RepairStrategy::ReplaceConfig { target, content } => {
                let dest = workspace.join(target);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
                }
                fs::write(&dest, content).map_err(|e| format!("write: {e}"))?;
            }
            RepairStrategy::CreateFile { target, content } => {
                let dest = workspace.join(target);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
                }
                fs::write(&dest, content).map_err(|e| format!("write: {e}"))?;
            }
            RepairStrategy::RemoveFile { target } => {
                let dest = workspace.join(target);
                // In workspace, the file may not exist yet (it is a representation).
                // Mark it for removal by creating a tombstone.
                let tombstone = workspace.join(format!("{target}.tombstone"));
                fs::write(&tombstone, "").map_err(|e| format!("tombstone: {e}"))?;
                let _ = fs::remove_file(&dest);
            }
            RepairStrategy::Custom { kind, payload } => {
                // Store the custom repair as a descriptor file in the workspace.
                let descriptor = workspace.join("__custom_repair.json");
                let json = format!("{{\"kind\":\"{kind}\",\"payload\":{payload}}}");
                fs::write(&descriptor, json).map_err(|e| format!("descriptor: {e}"))?;
            }
        }
        Ok(())
    }

    /// Promote a verified workspace change to the production path.
    fn promote(&self, workspace: &Path, strategy: &RepairStrategy) -> Result<(), String> {
        match strategy {
            RepairStrategy::ReplaceConfig { target, content: _ }
            | RepairStrategy::CreateFile { target, content: _ } => {
                let source = workspace.join(target);
                let dest = self.production_path.join(target);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("promote mkdir: {e}"))?;
                }
                fs::copy(&source, &dest).map_err(|e| format!("promote copy: {e}"))?;
            }
            RepairStrategy::RemoveFile { target } => {
                let dest = self.production_path.join(target);
                if dest.exists() {
                    fs::remove_file(&dest).map_err(|e| format!("promote remove: {e}"))?;
                }
            }
            RepairStrategy::Custom { .. } => {
                // Custom strategies must be promoted by their own handlers.
                // This is a no-op in the generic transaction.
            }
        }
        Ok(())
    }

    /// Get the workspace path (only valid during execute).
    pub fn workspace_path(&self) -> Option<&Path> {
        self.workspace.as_ref().map(|w| w.path())
    }
}

/// Trait for verifying a repair in the isolated workspace.
pub trait RepairVerifier {
    /// Verify that the workspace state is consistent after the repair.
    /// Returns Ok(true) to promote, Ok(false) to rollback, Err to rollback with detail.
    fn verify(&self, workspace: &Path, proposal: &Proposal) -> Result<bool, String>;
}

/// A simple verifier that checks if expected files exist in the workspace.
pub struct FileExistsVerifier;

impl RepairVerifier for FileExistsVerifier {
    fn verify(&self, workspace: &Path, proposal: &Proposal) -> Result<bool, String> {
        match &proposal.strategy {
            RepairStrategy::ReplaceConfig { target, .. }
            | RepairStrategy::CreateFile { target, .. } => {
                let path = workspace.join(target);
                Ok(path.exists())
            }
            RepairStrategy::RemoveFile { target } => {
                let path = workspace.join(target);
                // The file should NOT exist (or a tombstone should).
                Ok(!path.exists())
            }
            RepairStrategy::Custom { .. } => {
                // Custom strategies pass by default.
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn test_finding() -> Finding {
        Finding {
            source: "config-watcher".to_string(),
            summary: "Config file is corrupt".to_string(),
            severity: FindingSeverity::High,
            discovered_at: Utc::now(),
        }
    }

    fn test_proposal(target: &str, content: &str) -> Proposal {
        Proposal {
            finding: test_finding(),
            description: format!("Replace {target} with corrected content"),
            strategy: RepairStrategy::ReplaceConfig {
                target: target.to_string(),
                content: content.to_string(),
            },
        }
    }

    struct AlwaysPassVerifier;
    impl RepairVerifier for AlwaysPassVerifier {
        fn verify(&self, _workspace: &Path, _proposal: &Proposal) -> Result<bool, String> {
            Ok(true)
        }
    }

    struct AlwaysFailVerifier;
    impl RepairVerifier for AlwaysFailVerifier {
        fn verify(&self, _workspace: &Path, _proposal: &Proposal) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[test]
    fn commit_on_successful_verification() {
        let production = tempfile::TempDir::new().unwrap();
        let mut tx = RepairTransaction::new(production.path());
        let proposal = test_proposal("app.conf", "key=value\n");

        let outcome = tx.execute(&proposal, &AlwaysPassVerifier);
        assert_eq!(outcome, RepairOutcome::Committed);

        // File should exist in production.
        let produced = production.path().join("app.conf");
        assert!(produced.exists());
        assert_eq!(fs::read_to_string(produced).unwrap(), "key=value\n");
    }

    #[test]
    fn rollback_on_failed_verification() {
        let production = tempfile::TempDir::new().unwrap();
        let mut tx = RepairTransaction::new(production.path());
        let proposal = test_proposal("app.conf", "bad content");

        let outcome = tx.execute(&proposal, &AlwaysFailVerifier);
        assert!(matches!(outcome, RepairOutcome::RolledBack { .. }));

        // File should NOT exist in production.
        let produced = production.path().join("app.conf");
        assert!(!produced.exists());
    }

    #[test]
    fn create_file_strategy_works() {
        let production = tempfile::TempDir::new().unwrap();
        let mut tx = RepairTransaction::new(production.path());
        let proposal = Proposal {
            finding: test_finding(),
            description: "Create missing config".to_string(),
            strategy: RepairStrategy::CreateFile {
                target: "subdir/new.conf".to_string(),
                content: "new=true\n".to_string(),
            },
        };

        let outcome = tx.execute(&proposal, &FileExistsVerifier);
        assert_eq!(outcome, RepairOutcome::Committed);
        assert!(production.path().join("subdir/new.conf").exists());
    }

    #[test]
    fn remove_file_strategy_works() {
        let production = tempfile::TempDir::new().unwrap();
        // Pre-create the file to remove.
        let target = production.path().join("stale.lock");
        fs::write(&target, "lock").unwrap();
        assert!(target.exists());

        let mut tx = RepairTransaction::new(production.path());
        let proposal = Proposal {
            finding: test_finding(),
            description: "Remove stale lock".to_string(),
            strategy: RepairStrategy::RemoveFile {
                target: "stale.lock".to_string(),
            },
        };

        let outcome = tx.execute(&proposal, &FileExistsVerifier);
        assert_eq!(outcome, RepairOutcome::Committed);
        assert!(!target.exists());
    }

    #[test]
    fn file_exists_verifier_validates_replacement() {
        let production = tempfile::TempDir::new().unwrap();
        let mut tx = RepairTransaction::new(production.path());
        let proposal = test_proposal("test.cfg", "hello");

        let outcome = tx.execute(&proposal, &FileExistsVerifier);
        assert_eq!(outcome, RepairOutcome::Committed);
    }
}
