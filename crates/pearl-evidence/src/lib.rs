//! # pearl-evidence
//!
//! Independent evidence store crate -- 系統開發需求書 §57.
//!
//! Provides a content-addressed evidence store that records cryptographic proofs of
//! task outcomes. Evidence is append-only and immutable: once recorded, it constitutes
//! the provability record required by Constitution Article 4.
//!
//! This crate wraps and extends `pearl_core::evidence` with a filesystem-backed store
//! that persists evidence bundles as content-addressed files.

use chrono::{DateTime, Utc};
use pearl_core::EvidenceSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Errors from the evidence store.
#[derive(Debug, thiserror::Error)]
pub enum EvidenceStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("evidence bundle not found: {digest}")]
    NotFound { digest: String },
}

/// A stored evidence bundle with its content-address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceBundle {
    /// SHA-256 digest of the serialized evidence set (content address).
    pub digest: String,
    /// The task this evidence pertains to.
    pub task_id: String,
    /// The evidence items.
    pub items: Vec<EvidenceItem>,
    /// When this bundle was stored.
    pub stored_at: DateTime<Utc>,
}

/// A single evidence item within a bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceItem {
    /// Type of evidence (e.g., "exit_code", "diff_hash", "test_report").
    pub evidence_type: String,
    /// Who/what produced this evidence.
    pub producer: String,
    /// Whether the evidence indicates success.
    pub passed: bool,
    /// Optional payload (e.g., hash value, test output).
    pub payload: Option<String>,
}

/// The evidence store backed by a filesystem directory.
///
/// Evidence bundles are stored as JSON files named by their SHA-256 digest.
/// This provides natural deduplication and integrity verification.
pub struct EvidenceStore {
    base_path: PathBuf,
}

impl EvidenceStore {
    /// Open or create an evidence store at the given path.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, EvidenceStoreError> {
        let base_path = path.into();
        std::fs::create_dir_all(&base_path)?;
        Ok(Self { base_path })
    }

    /// Store an evidence bundle. Returns the content-address digest.
    pub fn store(
        &self,
        task_id: &str,
        items: Vec<EvidenceItem>,
        now: DateTime<Utc>,
    ) -> Result<EvidenceBundle, EvidenceStoreError> {
        let bundle = EvidenceBundle {
            digest: String::new(), // computed below
            task_id: task_id.to_string(),
            items,
            stored_at: now,
        };

        // Compute digest from the serialized content (excluding the digest field itself).
        let content = serde_json::to_string(&(&bundle.task_id, &bundle.items, &bundle.stored_at))?;
        let digest = hex::encode(Sha256::digest(content.as_bytes()));

        let bundle = EvidenceBundle {
            digest: digest.clone(),
            ..bundle
        };
        let json = serde_json::to_string_pretty(&bundle)?;

        let file_path = self.bundle_path(&digest);
        std::fs::write(&file_path, json)?;

        Ok(bundle)
    }

    /// Retrieve an evidence bundle by its digest.
    pub fn get(&self, digest: &str) -> Result<EvidenceBundle, EvidenceStoreError> {
        let file_path = self.bundle_path(digest);
        if !file_path.exists() {
            return Err(EvidenceStoreError::NotFound {
                digest: digest.to_string(),
            });
        }
        let content = std::fs::read_to_string(&file_path)?;
        let bundle: EvidenceBundle = serde_json::from_str(&content)?;
        Ok(bundle)
    }

    /// List all evidence digests for a given task.
    pub fn list_for_task(&self, task_id: &str) -> Result<Vec<String>, EvidenceStoreError> {
        let mut digests = Vec::new();
        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                let content = std::fs::read_to_string(&path)?;
                if let Ok(bundle) = serde_json::from_str::<EvidenceBundle>(&content) {
                    if bundle.task_id == task_id {
                        digests.push(bundle.digest);
                    }
                }
            }
        }
        Ok(digests)
    }

    /// Verify the integrity of a stored bundle (re-hash and compare).
    pub fn verify_integrity(&self, digest: &str) -> Result<bool, EvidenceStoreError> {
        let bundle = self.get(digest)?;
        let content = serde_json::to_string(&(&bundle.task_id, &bundle.items, &bundle.stored_at))?;
        let computed = hex::encode(Sha256::digest(content.as_bytes()));
        Ok(computed == bundle.digest)
    }

    fn bundle_path(&self, digest: &str) -> PathBuf {
        self.base_path.join(format!("{digest}.json"))
    }
}

/// Convert a core EvidenceSet into storable EvidenceItems.
pub fn evidence_set_to_items(set: &EvidenceSet) -> Vec<EvidenceItem> {
    set.items()
        .iter()
        .map(|e| EvidenceItem {
            evidence_type: e.evidence_type.as_str().to_string(),
            producer: e.producer.clone(),
            passed: e.passed(),
            payload: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_retrieve_evidence() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();

        let items = vec![
            EvidenceItem {
                evidence_type: "exit_code".to_string(),
                producer: "script.test".to_string(),
                passed: true,
                payload: Some("0".to_string()),
            },
            EvidenceItem {
                evidence_type: "diff_hash".to_string(),
                producer: "verifier.hash".to_string(),
                passed: true,
                payload: Some("abc123".to_string()),
            },
        ];

        let bundle = store.store("task-001", items, Utc::now()).unwrap();
        assert!(!bundle.digest.is_empty());
        assert_eq!(bundle.task_id, "task-001");
        assert_eq!(bundle.items.len(), 2);

        // Retrieve
        let retrieved = store.get(&bundle.digest).unwrap();
        assert_eq!(retrieved.digest, bundle.digest);
        assert_eq!(retrieved.task_id, "task-001");
    }

    #[test]
    fn verify_integrity_passes_for_valid_bundle() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();

        let items = vec![EvidenceItem {
            evidence_type: "test_report".to_string(),
            producer: "cargo-test".to_string(),
            passed: true,
            payload: None,
        }];

        let bundle = store.store("task-002", items, Utc::now()).unwrap();
        assert!(store.verify_integrity(&bundle.digest).unwrap());
    }

    #[test]
    fn not_found_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();

        let result = store.get("nonexistent");
        assert!(matches!(result, Err(EvidenceStoreError::NotFound { .. })));
    }

    #[test]
    fn list_for_task_filters_correctly() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = EvidenceStore::open(dir.path()).unwrap();
        let now = Utc::now();

        store
            .store(
                "task-a",
                vec![EvidenceItem {
                    evidence_type: "exit".to_string(),
                    producer: "p".to_string(),
                    passed: true,
                    payload: None,
                }],
                now,
            )
            .unwrap();
        store
            .store(
                "task-b",
                vec![EvidenceItem {
                    evidence_type: "exit".to_string(),
                    producer: "p".to_string(),
                    passed: false,
                    payload: None,
                }],
                now,
            )
            .unwrap();

        let a_digests = store.list_for_task("task-a").unwrap();
        assert_eq!(a_digests.len(), 1);

        let b_digests = store.list_for_task("task-b").unwrap();
        assert_eq!(b_digests.len(), 1);

        let c_digests = store.list_for_task("task-c").unwrap();
        assert_eq!(c_digests.len(), 0);
    }
}
