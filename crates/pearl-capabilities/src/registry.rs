//! The capability registry: loads, indexes, and queries capability manifests.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use pearl_core::precision::PrecisionClass;
use pearl_governance::{
    run_gate, CapabilityManifest, CapabilityType, GateReport, ManifestError, Runtime,
};
use pearl_precision::{ClassificationInput, PrecisionDecisionEngine};

/// A capability that has been registered in the catalog.
#[derive(Debug, Clone)]
pub struct RegisteredCapability {
    /// The parsed manifest.
    pub manifest: CapabilityManifest,
    /// The file path this manifest was loaded from, if any.
    pub source_path: Option<PathBuf>,
    /// The precision class assigned by the Precision Decision Engine.
    pub precision_class: PrecisionClass,
    /// When this capability was registered.
    pub registered_at: DateTime<Utc>,
}

/// Errors that can occur during registry operations.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Failed to read a manifest file from disk.
    #[error("failed to read manifest at {path}: {detail}")]
    Io { path: PathBuf, detail: String },

    /// A YAML file could not be parsed as a valid manifest.
    #[error("failed to parse manifest at {path}: {detail}")]
    Parse { path: PathBuf, detail: String },

    /// The directory itself could not be read.
    #[error("failed to read directory {path}: {detail}")]
    Directory { path: PathBuf, detail: String },

    /// A manifest error (wrapper for ManifestError).
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// The runtime catalog of registered capabilities.
///
/// Holds all known capabilities and provides indexed query methods for the router
/// to find matching capabilities by various criteria.
#[derive(Debug, Clone)]
pub struct CapabilityRegistry {
    capabilities: Vec<RegisteredCapability>,
    engine: PrecisionDecisionEngine,
}

impl Default for CapabilityRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            capabilities: Vec::new(),
            engine: PrecisionDecisionEngine::new(),
        }
    }

    /// Loads all YAML manifests from a directory recursively.
    ///
    /// Walks the directory tree, loading every `.yaml` and `.yml` file as a
    /// [`CapabilityManifest`]. Each manifest is classified with the Precision Decision
    /// Engine and indexed for querying.
    ///
    /// Non-YAML files are silently skipped. Malformed YAML files produce an error
    /// rather than silently ignoring them -- a broken manifest is a signal worth
    /// surfacing.
    pub fn load_directory(path: &Path) -> Result<Self, RegistryError> {
        let mut registry = Self::new();
        registry.load_directory_recursive(path)?;
        Ok(registry)
    }

    /// Recursively walk a directory and load all YAML manifests.
    fn load_directory_recursive(&mut self, path: &Path) -> Result<(), RegistryError> {
        let entries = fs::read_dir(path).map_err(|e| RegistryError::Directory {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| RegistryError::Directory {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;

            let entry_path = entry.path();

            if entry_path.is_dir() {
                self.load_directory_recursive(&entry_path)?;
            } else if is_yaml_file(&entry_path) {
                self.load_manifest_file(&entry_path)?;
            }
            // Non-YAML files are silently skipped.
        }

        Ok(())
    }

    /// Load a single manifest file from disk, classify it, and register it.
    fn load_manifest_file(&mut self, path: &Path) -> Result<(), RegistryError> {
        let content = fs::read_to_string(path).map_err(|e| RegistryError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;

        let manifest =
            CapabilityManifest::from_yaml(&content).map_err(|e| RegistryError::Parse {
                path: path.to_path_buf(),
                detail: e.to_string(),
            })?;

        let input = ClassificationInput::from_manifest_inferred(&manifest);
        let result = self.engine.classify(&input);

        self.capabilities.push(RegisteredCapability {
            manifest,
            source_path: Some(path.to_path_buf()),
            precision_class: result.class,
            registered_at: Utc::now(),
        });

        Ok(())
    }

    /// Register a capability programmatically.
    ///
    /// Classifies the manifest with the Precision Decision Engine and adds it to
    /// the registry.
    pub fn register(
        &mut self,
        manifest: CapabilityManifest,
        source_path: Option<PathBuf>,
    ) -> &RegisteredCapability {
        let input = ClassificationInput::from_manifest_inferred(&manifest);
        let result = self.engine.classify(&input);

        self.capabilities.push(RegisteredCapability {
            manifest,
            source_path,
            precision_class: result.class,
            registered_at: Utc::now(),
        });

        self.capabilities.last().expect("just pushed")
    }

    /// Look up a capability by its id.
    pub fn find_by_id(&self, id: &str) -> Option<&RegisteredCapability> {
        self.capabilities.iter().find(|c| c.manifest.id == id)
    }

    /// Find all capabilities of a given type.
    pub fn find_by_type(&self, capability_type: CapabilityType) -> Vec<&RegisteredCapability> {
        self.capabilities
            .iter()
            .filter(|c| c.manifest.capability_type == capability_type)
            .collect()
    }

    /// Find all capabilities that use a given runtime.
    pub fn find_by_runtime(&self, runtime: Runtime) -> Vec<&RegisteredCapability> {
        self.capabilities
            .iter()
            .filter(|c| c.manifest.execution.runtime == runtime)
            .collect()
    }

    /// Find all capabilities with a given precision class.
    pub fn find_by_precision(&self, class: PrecisionClass) -> Vec<&RegisteredCapability> {
        self.capabilities
            .iter()
            .filter(|c| c.precision_class == class)
            .collect()
    }

    /// Find all mechanical (P0) capabilities.
    ///
    /// These are the fully deterministic scripts that Article 1 requires be routed
    /// to script execution, never to an LLM.
    pub fn find_mechanical(&self) -> Vec<&RegisteredCapability> {
        self.find_by_precision(PrecisionClass::P0)
    }

    /// Run the governance Constitution checks on every registered manifest.
    ///
    /// Returns a combined [`GateReport`] covering all capabilities. This enables
    /// programmatic validation of the entire registry.
    pub fn validate_all(&self) -> GateReport {
        let manifests: Vec<&CapabilityManifest> =
            self.capabilities.iter().map(|c| &c.manifest).collect();
        run_gate(manifests)
    }

    /// The number of registered capabilities.
    pub fn len(&self) -> usize {
        self.capabilities.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
    }

    /// Iterate over all registered capabilities.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredCapability> {
        self.capabilities.iter()
    }
}

/// Check if a path has a YAML extension.
fn is_yaml_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A deterministic script manifest (should classify as P0).
    const SCRIPT_MANIFEST: &str = r#"
id: script.task-score
version: 1
type: script
description: Computes a task priority score from a fixed weighted formula.
execution:
  kind: script
  runtime: rust
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
schemas:
  output: verification-result-v1
timeout_seconds: 10
"#;

    /// A guard manifest (deterministic, should classify as P0).
    const GUARD_MANIFEST: &str = r#"
id: guard.dangerous-shell
version: 1
type: guard
description: Blocks destructive shell commands before execution.
execution:
  kind: script
  runtime: python
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
on_error: deny
timeout_seconds: 5
"#;

    /// A verifier manifest (deterministic, should classify as P0).
    const VERIFIER_MANIFEST: &str = r#"
id: verifier.task-result
version: 1
type: verifier
description: Validates a task result document against its declared output schema.
execution:
  kind: script
  runtime: python
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
schemas:
  input: task-spec-v1
  output: verification-result-v1
timeout_seconds: 60
"#;

    /// A non-deterministic agent capability (should classify as P3).
    const AGENT_MANIFEST: &str = r#"
id: agent.code-review
version: 1
type: agent
description: Performs code review using an LLM.
execution:
  kind: agent
  runtime: claude_code
quality:
  deterministic: false
risk:
  side_effect: false
platform:
  windows: true
  linux: true
timeout_seconds: 120
"#;

    fn write_manifest(dir: &Path, filename: &str, content: &str) {
        fs::write(dir.join(filename), content).unwrap();
    }

    fn setup_test_directory() -> TempDir {
        let dir = TempDir::new().unwrap();

        // Create subdirectories mimicking the capabilities/ layout.
        let scripts = dir.path().join("scripts");
        let guards = dir.path().join("guards");
        let verifiers = dir.path().join("verifiers");
        fs::create_dir_all(&scripts).unwrap();
        fs::create_dir_all(&guards).unwrap();
        fs::create_dir_all(&verifiers).unwrap();

        write_manifest(&scripts, "script.task-score.yaml", SCRIPT_MANIFEST);
        write_manifest(&guards, "guard.dangerous-shell.yaml", GUARD_MANIFEST);
        write_manifest(&verifiers, "verifier.task-result.yaml", VERIFIER_MANIFEST);

        dir
    }

    // -----------------------------------------------------------------------
    // load_directory tests
    // -----------------------------------------------------------------------

    #[test]
    fn loads_all_yaml_manifests_from_directory() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn loads_yml_extension_too() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "script.yml", SCRIPT_MANIFEST);

        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn skips_non_yaml_files() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "script.task-score.yaml", SCRIPT_MANIFEST);
        write_manifest(dir.path(), "README.md", "# Not a manifest");
        write_manifest(dir.path(), "notes.txt", "some notes");
        write_manifest(dir.path(), "data.json", "{}");

        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn reports_error_for_malformed_yaml() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "bad.yaml", "id: [unclosed bracket");

        let result = CapabilityRegistry::load_directory(dir.path());
        assert!(result.is_err());

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad.yaml"),
            "error should mention the file: {msg}"
        );
    }

    #[test]
    fn reports_error_for_incomplete_manifest() {
        let dir = TempDir::new().unwrap();
        // Valid YAML but missing required CapabilityManifest fields.
        write_manifest(dir.path(), "incomplete.yaml", "id: x\nversion: 1\n");

        let result = CapabilityRegistry::load_directory(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn reports_error_for_nonexistent_directory() {
        let result = CapabilityRegistry::load_directory(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // find_by_id tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_by_id_returns_correct_capability() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let cap = registry.find_by_id("script.task-score").unwrap();
        assert_eq!(cap.manifest.id, "script.task-score");
        assert_eq!(cap.manifest.capability_type, CapabilityType::Script);
    }

    #[test]
    fn find_by_id_returns_none_for_unknown() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        assert!(registry.find_by_id("nonexistent").is_none());
    }

    // -----------------------------------------------------------------------
    // find_by_type tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_by_type_returns_matching_capabilities() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let scripts = registry.find_by_type(CapabilityType::Script);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].manifest.id, "script.task-score");

        let guards = registry.find_by_type(CapabilityType::Guard);
        assert_eq!(guards.len(), 1);
        assert_eq!(guards[0].manifest.id, "guard.dangerous-shell");

        let verifiers = registry.find_by_type(CapabilityType::Verifier);
        assert_eq!(verifiers.len(), 1);
        assert_eq!(verifiers[0].manifest.id, "verifier.task-result");
    }

    #[test]
    fn find_by_type_returns_empty_for_no_match() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let agents = registry.find_by_type(CapabilityType::Agent);
        assert!(agents.is_empty());
    }

    // -----------------------------------------------------------------------
    // find_by_runtime tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_by_runtime_returns_matching_capabilities() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let python = registry.find_by_runtime(Runtime::Python);
        assert_eq!(python.len(), 2); // guard + verifier

        let rust = registry.find_by_runtime(Runtime::Rust);
        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].manifest.id, "script.task-score");
    }

    #[test]
    fn find_by_runtime_returns_empty_for_no_match() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let shell = registry.find_by_runtime(Runtime::Shell);
        assert!(shell.is_empty());
    }

    // -----------------------------------------------------------------------
    // find_by_precision tests
    // -----------------------------------------------------------------------

    #[test]
    fn find_by_precision_returns_matching_capabilities() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        // All three test manifests are deterministic scripts => P0.
        let p0 = registry.find_by_precision(PrecisionClass::P0);
        assert_eq!(p0.len(), 3);
    }

    #[test]
    fn find_mechanical_returns_only_p0() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "script.yaml", SCRIPT_MANIFEST);
        write_manifest(dir.path(), "agent.yaml", AGENT_MANIFEST);

        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let mechanical = registry.find_mechanical();
        assert_eq!(mechanical.len(), 1);
        assert_eq!(mechanical[0].manifest.id, "script.task-score");
        assert_eq!(mechanical[0].precision_class, PrecisionClass::P0);

        // The agent should be P3.
        let p3 = registry.find_by_precision(PrecisionClass::P3);
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].manifest.id, "agent.code-review");
    }

    // -----------------------------------------------------------------------
    // register tests
    // -----------------------------------------------------------------------

    #[test]
    fn programmatic_registration_works() {
        let mut registry = CapabilityRegistry::new();

        let manifest = CapabilityManifest::from_yaml(SCRIPT_MANIFEST).unwrap();
        let registered = registry.register(manifest.clone(), None);

        assert_eq!(registered.manifest.id, "script.task-score");
        assert_eq!(registered.precision_class, PrecisionClass::P0);
        assert!(registered.source_path.is_none());
        assert_eq!(registry.len(), 1);

        // Should be findable.
        assert!(registry.find_by_id("script.task-score").is_some());
    }

    #[test]
    fn register_with_source_path() {
        let mut registry = CapabilityRegistry::new();

        let manifest = CapabilityManifest::from_yaml(GUARD_MANIFEST).unwrap();
        let path = PathBuf::from("/capabilities/guards/guard.dangerous-shell.yaml");
        let registered = registry.register(manifest, Some(path.clone()));

        assert_eq!(registered.source_path, Some(path));
    }

    // -----------------------------------------------------------------------
    // validate_all tests
    // -----------------------------------------------------------------------

    #[test]
    fn validate_all_passes_for_clean_manifests() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let report = registry.validate_all();
        assert!(
            report.passed(),
            "clean manifests should pass: {:?}",
            report.findings
        );
        assert_eq!(report.inspected, 3);
    }

    #[test]
    fn validate_all_reports_violations() {
        let mut registry = CapabilityRegistry::new();

        // A guard that fails open violates Article 7.
        let bad_guard_yaml = r#"
id: guard.bad
version: 1
type: guard
description: A bad guard.
execution:
  kind: script
  runtime: python
quality:
  deterministic: true
risk:
  side_effect: false
platform:
  windows: true
  linux: true
on_error: allow
timeout_seconds: 5
"#;
        let manifest = CapabilityManifest::from_yaml(bad_guard_yaml).unwrap();
        registry.register(manifest, None);

        let report = registry.validate_all();
        assert!(!report.passed());
        assert_eq!(report.inspected, 1);
        assert!(report.violation_count() >= 1);
    }

    #[test]
    fn validate_all_on_empty_registry() {
        let registry = CapabilityRegistry::new();
        let report = registry.validate_all();
        assert!(report.passed());
        assert_eq!(report.inspected, 0);
    }

    // -----------------------------------------------------------------------
    // Utility tests
    // -----------------------------------------------------------------------

    #[test]
    fn empty_registry_is_empty() {
        let registry = CapabilityRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn iter_yields_all_capabilities() {
        let dir = setup_test_directory();
        let registry = CapabilityRegistry::load_directory(dir.path()).unwrap();

        let ids: Vec<&str> = registry.iter().map(|c| c.manifest.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"script.task-score"));
        assert!(ids.contains(&"guard.dangerous-shell"));
        assert!(ids.contains(&"verifier.task-result"));
    }

    #[test]
    fn is_yaml_file_works() {
        assert!(is_yaml_file(Path::new("foo.yaml")));
        assert!(is_yaml_file(Path::new("foo.yml")));
        assert!(is_yaml_file(Path::new("foo.YAML")));
        assert!(is_yaml_file(Path::new("foo.YML")));
        assert!(!is_yaml_file(Path::new("foo.json")));
        assert!(!is_yaml_file(Path::new("foo.md")));
        assert!(!is_yaml_file(Path::new("foo")));
    }

    #[test]
    fn load_from_actual_capabilities_directory() {
        // Test with the real capabilities/ directory if it exists.
        let caps_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("capabilities");

        if caps_dir.exists() {
            let registry = CapabilityRegistry::load_directory(&caps_dir).unwrap();
            assert!(
                registry.len() >= 3,
                "expected at least 3 manifests in capabilities/"
            );

            // All should pass Constitution checks.
            let report = registry.validate_all();
            assert!(
                report.passed(),
                "capabilities/ manifests should pass the gate: {:?}",
                report.findings
            );
        }
    }
}
