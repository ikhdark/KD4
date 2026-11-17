//! Legacy Windows sandbox configuration compatibility; KD4 always runs with full access.
use crate::config::Config;
use codex_config::config_toml::ConfigToml;
use codex_config::types::WindowsSandboxModeToml;
use codex_features::{Feature, Features, FeaturesToml};
use codex_protocol::config_types::WindowsSandboxLevel;
use std::collections::BTreeMap;

pub trait WindowsSandboxLevelExt {
    fn from_config(config: &Config) -> WindowsSandboxLevel;
    fn from_features(features: &Features) -> WindowsSandboxLevel;
}

impl WindowsSandboxLevelExt for WindowsSandboxLevel {
    fn from_config(_config: &Config) -> WindowsSandboxLevel {
        WindowsSandboxLevel::Disabled
    }

    fn from_features(_features: &Features) -> WindowsSandboxLevel {
        WindowsSandboxLevel::Disabled
    }
}

pub fn windows_sandbox_level_from_config(config: &Config) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_config(config)
}

pub fn windows_sandbox_level_from_features(features: &Features) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_features(features)
}

pub fn resolve_windows_sandbox_mode(cfg: &ConfigToml) -> Option<WindowsSandboxModeToml> {
    let _ = cfg;
    None
}

pub fn resolve_windows_sandbox_private_desktop(cfg: &ConfigToml) -> bool {
    cfg.windows
        .as_ref()
        .and_then(|windows| windows.sandbox_private_desktop)
        .unwrap_or(true)
}

pub fn legacy_windows_sandbox_mode(
    features: Option<&FeaturesToml>,
) -> Option<WindowsSandboxModeToml> {
    let entries = features.map(FeaturesToml::entries)?;
    legacy_windows_sandbox_mode_from_entries(&entries)
}

pub fn legacy_windows_sandbox_mode_from_entries(
    entries: &BTreeMap<String, bool>,
) -> Option<WindowsSandboxModeToml> {
    if entries
        .get(Feature::WindowsSandboxElevated.key())
        .copied()
        .unwrap_or(false)
    {
        return Some(WindowsSandboxModeToml::Elevated);
    }
    if entries
        .get(Feature::WindowsSandbox.key())
        .copied()
        .unwrap_or(false)
        || entries
            .get("enable_experimental_windows_sandbox")
            .copied()
            .unwrap_or(false)
    {
        Some(WindowsSandboxModeToml::Unelevated)
    } else {
        None
    }
}
