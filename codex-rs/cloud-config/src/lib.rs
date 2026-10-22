//! Cloud-hosted configuration data for Codex.
//!
//! This crate owns authenticated transport and startup snapshots for cloud-delivered
//! config data. Unauthenticated disk caches are not used. Parsing and composition
//! remain in `codex-config`; effective validation remains with the config consumer.

mod backend;
mod bundle_loader;
mod metrics;
mod service;
mod validation;

pub use bundle_loader::cloud_config_bundle_loader;
pub use bundle_loader::cloud_config_bundle_loader_for_storage;
