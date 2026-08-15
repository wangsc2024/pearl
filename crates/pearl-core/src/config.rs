//! Config Resolution — Constitution Article 10.
//!
//! Configuration has exactly one source of truth, resolved through a fixed precedence
//! chain. Every run records which revision it used, because a run whose configuration
//! cannot be reconstructed is not reproducible and therefore not auditable.
//!
//! ```text
//! System → Profile → Task Type → Task → Runtime Emergency Override
//! ```
//!
//! Later layers win. The resolved result carries a `config_hash` over the *merged*
//! value, so two runs with the same hash provably saw the same configuration.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The precedence chain, lowest priority first.
///
/// The ordinal value is the precedence: a layer with a higher ordinal overrides a
/// lower one. `EmergencyOverride` is last because it exists precisely to win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    System = 0,
    Profile = 1,
    TaskType = 2,
    Task = 3,
    EmergencyOverride = 4,
}

impl Layer {
    pub const ALL: [Layer; 5] = [
        Layer::System,
        Layer::Profile,
        Layer::TaskType,
        Layer::Task,
        Layer::EmergencyOverride,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::System => "system",
            Layer::Profile => "profile",
            Layer::TaskType => "task_type",
            Layer::Task => "task",
            Layer::EmergencyOverride => "emergency_override",
        }
    }
}

/// Runtime Profile — 系統開發需求書 §48.
///
/// The profile caps what the system is allowed to attempt. It is a mechanical input
/// to config resolution, never an agent decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfile {
    #[default]
    Normal,
    /// Reduced concurrency, research disabled.
    Degraded,
    /// Only repair-class work admitted.
    Recovery,
    /// Side effects forbidden; diagnostics only.
    Emergency,
}

impl RuntimeProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeProfile::Normal => "normal",
            RuntimeProfile::Degraded => "degraded",
            RuntimeProfile::Recovery => "recovery",
            RuntimeProfile::Emergency => "emergency",
        }
    }

    /// Whether external side effects may be committed under this profile.
    ///
    /// Emergency withholds side effects because the system has, by definition, lost
    /// confidence in its own judgement; the safe failure is to observe and not act.
    pub fn allows_side_effects(&self) -> bool {
        !matches!(self, RuntimeProfile::Emergency)
    }

    /// Upper bound on concurrent workers, independent of what config requests.
    pub fn concurrency_cap(&self) -> u32 {
        match self {
            RuntimeProfile::Normal => u32::MAX,
            RuntimeProfile::Degraded => 2,
            RuntimeProfile::Recovery => 1,
            RuntimeProfile::Emergency => 1,
        }
    }
}

/// One contribution to the resolved configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    pub layer: Layer,
    /// Where this came from, recorded for provenance (a path, or a marker like `"builtin"`).
    pub origin: String,
    pub values: BTreeMap<String, serde_json::Value>,
}

impl ConfigSource {
    pub fn new(layer: Layer, origin: impl Into<String>) -> Self {
        Self {
            layer,
            origin: origin.into(),
            values: BTreeMap::new(),
        }
    }

    pub fn set(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Loads a flat YAML mapping.
    ///
    /// A `version` key is mandatory, following the version-gate pattern proven in
    /// `daily_rust/src/config/mod.rs`: an unversioned config file is a config file
    /// whose shape can change without anyone noticing.
    pub fn from_yaml_str(
        layer: Layer,
        origin: impl Into<String>,
        yaml: &str,
    ) -> Result<Self, ConfigError> {
        let origin = origin.into();
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml)
            .map_err(|e| ConfigError::Parse { origin: origin.clone(), detail: e.to_string() })?;

        let mapping = parsed
            .as_mapping()
            .ok_or_else(|| ConfigError::NotAMapping { origin: origin.clone() })?;

        if !mapping.contains_key(serde_yaml::Value::String("version".into())) {
            return Err(ConfigError::MissingVersion { origin });
        }

        let mut values = BTreeMap::new();
        for (k, v) in mapping {
            let key = k
                .as_str()
                .ok_or_else(|| ConfigError::NonStringKey { origin: origin.clone() })?;
            let json = serde_json::to_value(v)
                .map_err(|e| ConfigError::Parse { origin: origin.clone(), detail: e.to_string() })?;
            values.insert(key.to_string(), json);
        }

        Ok(Self { layer, origin, values })
    }
}

/// The outcome of resolution: a merged value set plus the provenance needed to
/// reproduce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub values: BTreeMap<String, serde_json::Value>,
    /// Human-readable revision, e.g. `system@builtin+profile@normal.yaml`.
    pub config_revision: String,
    /// SHA-256 over the canonical serialization of `values`.
    pub config_hash: String,
    /// Which layer supplied each final key, so a surprising value can be traced home.
    pub provenance: BTreeMap<String, String>,
}

impl ResolvedConfig {
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.values.get(key)
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.values.get(key)?.as_u64()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.values.get(key)?.as_bool()
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_str()
    }

    /// Which layer decided the final value of `key`.
    pub fn origin_of(&self, key: &str) -> Option<&str> {
        self.provenance.get(key).map(String::as_str)
    }
}

/// Merges layered sources into a single reproducible configuration.
#[derive(Debug, Default)]
pub struct ConfigResolver {
    sources: Vec<ConfigSource>,
}

impl ConfigResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_source(mut self, source: ConfigSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Resolves the chain.
    ///
    /// Sources are sorted by layer precedence before merging, so callers may add them
    /// in any order — resolution must not depend on registration order, otherwise the
    /// same inputs could produce two different hashes.
    pub fn resolve(&self) -> ResolvedConfig {
        let mut ordered = self.sources.clone();
        ordered.sort_by_key(|s| s.layer);

        let mut values: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut provenance: BTreeMap<String, String> = BTreeMap::new();

        for source in &ordered {
            for (key, value) in &source.values {
                values.insert(key.clone(), value.clone());
                provenance.insert(key.clone(), source.layer.as_str().to_string());
            }
        }

        let config_revision = ordered
            .iter()
            .map(|s| format!("{}@{}", s.layer.as_str(), s.origin))
            .collect::<Vec<_>>()
            .join("+");

        let config_hash = hash_values(&values);

        ResolvedConfig { values, config_revision, config_hash, provenance }
    }
}

/// Hashes a value map deterministically.
///
/// `BTreeMap` gives key ordering and `serde_json` emits canonical scalars, so the
/// digest is stable across processes and platforms. Two runs sharing a `config_hash`
/// provably saw identical configuration.
pub fn hash_values(values: &BTreeMap<String, serde_json::Value>) -> String {
    let canonical = serde_json::to_vec(values).expect("BTreeMap<String, Value> always serializes");
    let digest = Sha256::digest(&canonical);
    hex::encode(digest)
}

/// Configuration failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config '{origin}' is missing the mandatory 'version' key")]
    MissingVersion { origin: String },
    #[error("config '{origin}' must be a mapping at the top level")]
    NotAMapping { origin: String },
    #[error("config '{origin}' has a non-string key")]
    NonStringKey { origin: String },
    #[error("config '{origin}' failed to parse: {detail}")]
    Parse { origin: String, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_layers_override_earlier_ones() {
        let resolved = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::System, "builtin").set("timeout_seconds", 300))
            .with_source(ConfigSource::new(Layer::Task, "task.yaml").set("timeout_seconds", 30))
            .resolve();

        assert_eq!(resolved.get_u64("timeout_seconds"), Some(30));
        assert_eq!(resolved.origin_of("timeout_seconds"), Some("task"));
    }

    #[test]
    fn emergency_override_beats_every_other_layer() {
        let resolved = ConfigResolver::new()
            .with_source(
                ConfigSource::new(Layer::EmergencyOverride, "operator").set("max_workers", 1),
            )
            .with_source(ConfigSource::new(Layer::Task, "t").set("max_workers", 8))
            .with_source(ConfigSource::new(Layer::System, "builtin").set("max_workers", 4))
            .resolve();

        assert_eq!(resolved.get_u64("max_workers"), Some(1));
        assert_eq!(resolved.origin_of("max_workers"), Some("emergency_override"));
    }

    #[test]
    fn resolution_is_independent_of_registration_order() {
        let a = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::System, "s").set("k", 1))
            .with_source(ConfigSource::new(Layer::Task, "t").set("k", 2))
            .resolve();
        let b = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::Task, "t").set("k", 2))
            .with_source(ConfigSource::new(Layer::System, "s").set("k", 1))
            .resolve();

        assert_eq!(a.config_hash, b.config_hash);
        assert_eq!(a.config_revision, b.config_revision);
    }

    #[test]
    fn differing_values_produce_differing_hashes() {
        let a = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::System, "s").set("k", 1))
            .resolve();
        let b = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::System, "s").set("k", 2))
            .resolve();

        assert_ne!(a.config_hash, b.config_hash);
    }

    #[test]
    fn hash_is_stable_across_repeated_resolution() {
        let build = || {
            ConfigResolver::new()
                .with_source(
                    ConfigSource::new(Layer::System, "s")
                        .set("a", 1)
                        .set("b", "two")
                        .set("c", true),
                )
                .resolve()
        };
        assert_eq!(build().config_hash, build().config_hash);
    }

    #[test]
    fn yaml_without_version_is_rejected() {
        let err =
            ConfigSource::from_yaml_str(Layer::System, "sys.yaml", "timeout_seconds: 10").unwrap_err();
        assert!(matches!(err, ConfigError::MissingVersion { .. }));
    }

    #[test]
    fn yaml_with_version_loads() {
        let src = ConfigSource::from_yaml_str(
            Layer::Profile,
            "profile.yaml",
            "version: 1\ntimeout_seconds: 45\nresearch_enabled: false\n",
        )
        .unwrap();

        let resolved = ConfigResolver::new().with_source(src).resolve();
        assert_eq!(resolved.get_u64("timeout_seconds"), Some(45));
        assert_eq!(resolved.get_bool("research_enabled"), Some(false));
    }

    #[test]
    fn revision_string_records_every_contributing_layer() {
        let resolved = ConfigResolver::new()
            .with_source(ConfigSource::new(Layer::System, "builtin").set("k", 1))
            .with_source(ConfigSource::new(Layer::Profile, "normal.yaml").set("k", 2))
            .resolve();

        assert_eq!(resolved.config_revision, "system@builtin+profile@normal.yaml");
    }

    #[test]
    fn emergency_profile_withholds_side_effects() {
        assert!(RuntimeProfile::Normal.allows_side_effects());
        assert!(RuntimeProfile::Degraded.allows_side_effects());
        assert!(RuntimeProfile::Recovery.allows_side_effects());
        assert!(!RuntimeProfile::Emergency.allows_side_effects());
    }

    #[test]
    fn degraded_profiles_cap_concurrency() {
        assert_eq!(RuntimeProfile::Normal.concurrency_cap(), u32::MAX);
        assert_eq!(RuntimeProfile::Degraded.concurrency_cap(), 2);
        assert_eq!(RuntimeProfile::Recovery.concurrency_cap(), 1);
        assert_eq!(RuntimeProfile::Emergency.concurrency_cap(), 1);
    }
}
