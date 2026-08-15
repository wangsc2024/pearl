//! # pearl-capabilities
//!
//! The Capability Manifest Registry -- Article 1 script-first routing.
//!
//! This crate provides the runtime catalog that indexes all registered capabilities
//! from YAML manifests, enabling lookup by id, by type, by runtime, and by precision
//! class. The registry loads from a directory of YAML manifests, validates each against
//! Constitution rules, and provides query methods for the router to find matching
//! capabilities.
//!
//! Article 1 requires deterministic work to be routed to scripts, never to an LLM.
//! The registry enables this by classifying each capability at registration time and
//! exposing [`CapabilityRegistry::find_mechanical`] for the router to discover all
//! P0-capable capabilities.

mod registry;

pub use registry::{
    parse_manifest_documents, CapabilityRegistry, RegisteredCapability, RegistryError,
    ResolvedEntrypoint,
};
