use codex_otel::StatsigMetricsSettings;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

/// Versioned payload exchanged between the sandbox setup orchestrator and helper.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetupPayload {
    pub version: u32,
    pub offline_username: String,
    pub online_username: String,
    pub codex_home: PathBuf,
    pub command_cwd: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    #[serde(default)]
    pub deny_read_paths: Vec<PathBuf>,
    #[serde(default)]
    pub deny_write_paths: Vec<PathBuf>,
    pub proxy_ports: Vec<u16>,
    #[serde(default)]
    pub allow_local_binding: bool,
    #[serde(default)]
    pub otel: Option<StatsigMetricsSettings>,
    pub real_user: String,
    #[serde(default)]
    pub mode: SetupMode,
    #[serde(default)]
    pub refresh_only: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SetupMode {
    #[default]
    Full,
    ProvisionOnly,
    ReadAclsOnly,
    ReadAclsOnlyStrict,
}
