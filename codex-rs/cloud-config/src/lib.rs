//! Cloud-hosted configuration data for Codex.
//!
//! This crate owns transport, retry, and validation for cloud-delivered config
//! data. Parsing and composition remain in `codex-config`.

mod backend;
mod bundle_loader;
mod metrics;
mod service;
mod validation;

pub use bundle_loader::cloud_config_bundle_loader;
pub use bundle_loader::cloud_config_bundle_loader_for_storage;
