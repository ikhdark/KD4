//! Pure Windows sandbox configuration resolution; enforcement remains in its runtime owner.
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
    fn from_config(config: &Config) -> WindowsSandboxLevel {
        match config.permissions.windows_sandbox_mode {
            Some(WindowsSandboxModeToml::Elevated) => WindowsSandboxLevel::Elevated,
            Some(WindowsSandboxModeToml::Unelevated) => WindowsSandboxLevel::RestrictedToken,
            None => Self::from_features(&config.features),
        }
    }

    fn from_features(features: &Features) -> WindowsSandboxLevel {
        if features.enabled(Feature::WindowsSandboxElevated) {
            return WindowsSandboxLevel::Elevated;
        }
        if features.enabled(Feature::WindowsSandbox) {
            WindowsSandboxLevel::RestrictedToken
        } else {
            WindowsSandboxLevel::Disabled
        }
    }
}

pub fn windows_sandbox_level_from_config(config: &Config) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_config(config)
}

pub fn windows_sandbox_level_from_features(features: &Features) -> WindowsSandboxLevel {
    WindowsSandboxLevel::from_features(features)
}

pub fn resolve_windows_sandbox_mode(cfg: &ConfigToml) -> Option<WindowsSandboxModeToml> {
    cfg.windows
        .as_ref()
        .and_then(|windows| windows.sandbox)
        .or_else(|| legacy_windows_sandbox_mode(cfg.features.as_ref()))
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
