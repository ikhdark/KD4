//! Runtime support for Model Context Protocol (MCP) servers.
//!
//! This module contains data that describes the runtime environment in which MCP
//! servers execute, plus the sandbox state payload sent to capable servers and a
//! tiny shared metrics helper. Transport startup and orchestration live in
//! [`crate::rmcp_client`] and [`crate::connection_manager`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::HttpClient;
use codex_exec_server::ReqwestHttpClient;
use codex_protocol::models::PermissionProfile;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxState {
    pub permission_profile: PermissionProfile,
    pub sandbox_cwd: PathUri,
}

/// Runtime context used when resolving per-server MCP environments.
///
/// `McpConfig` describes what servers exist. This value carries the canonical
/// environment registry plus the local stdio fallback cwd used when a local
/// stdio server omits its own working directory.
#[derive(Clone)]
pub struct McpRuntimeContext {
    environment_manager: Arc<EnvironmentManager>,
    local_stdio_fallback_cwd: PathBuf,
    selected_environments: std::collections::HashMap<String, Option<Arc<Environment>>>,
}

impl McpRuntimeContext {
    pub fn new(
        environment_manager: Arc<EnvironmentManager>,
        local_stdio_fallback_cwd: PathBuf,
    ) -> Self {
        Self {
            environment_manager,
            local_stdio_fallback_cwd,
            selected_environments: Default::default(),
        }
    }

    pub fn with_selected_environments(
        mut self,
        environments: std::collections::HashMap<String, Option<Arc<Environment>>>,
    ) -> Self {
        self.selected_environments = environments;
        self
    }

    pub fn local_fallback_cwd(&self) -> PathBuf {
        self.local_stdio_fallback_cwd.clone()
    }

    pub fn selected_scope_matches(&self, other: &Self) -> bool {
        self.local_stdio_fallback_cwd == other.local_stdio_fallback_cwd
            && self.selected_environments.len() == other.selected_environments.len()
            && self.selected_environments.iter().all(|(id, left)| {
                other
                    .selected_environments
                    .get(id)
                    .is_some_and(|right| match (left, right) {
                        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                        (None, None) => true,
                        _ => false,
                    })
            })
    }

    pub fn server_scope_matches(
        &self,
        other: &Self,
        config: &codex_config::McpServerConfig,
    ) -> bool {
        if config.is_local_environment()
            && matches!(
                config.transport,
                codex_config::McpServerTransportConfig::Stdio { cwd: None, .. }
            )
            && self.local_stdio_fallback_cwd != other.local_stdio_fallback_cwd
        {
            return false;
        }
        match (
            self.resolve_server_environment("scope", config),
            other.resolve_server_environment("scope", config),
        ) {
            (Ok(Some(left)), Ok(Some(right))) => Arc::ptr_eq(&left, &right),
            (Ok(None), Ok(None)) => true,
            (Err(left), Err(right)) => left == right,
            _ => false,
        }
    }

    pub(crate) fn local_stdio_fallback_cwd(&self) -> PathBuf {
        self.local_stdio_fallback_cwd.clone()
    }

    pub(crate) fn is_compatible_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.environment_manager, &other.environment_manager)
    }

    pub(crate) fn resolve_server_environment(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Option<Arc<Environment>>, String> {
        if let Some(selected) = self.selected_environments.get(&config.environment_id) {
            return selected.clone().map(Some).ok_or_else(|| {
                format!(
                    "MCP server `{server_name}` selected environment `{}` is unavailable",
                    config.environment_id,
                )
            });
        }
        // Resolve `"local"` through the shared registry when available. Local
        // HTTP is the one current exception: it can use the ambient HTTP client
        // even when no local Environment is configured.
        if let Some(environment) = self
            .environment_manager
            .get_environment(&config.environment_id)
        {
            return Ok(Some(environment));
        }

        if config.is_local_environment() {
            return match config.transport {
                codex_config::McpServerTransportConfig::Stdio { .. } => Err(format!(
                    "local stdio MCP server `{server_name}` requires a local environment"
                )),
                codex_config::McpServerTransportConfig::StreamableHttp { .. } => Ok(None),
            };
        }

        Err(format!(
            "MCP server `{server_name}` references unknown environment id `{}`",
            config.environment_id
        ))
    }

    /// Resolves the HTTP capability owned by the server's configured environment.
    pub fn resolve_http_client(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Arc<dyn HttpClient>, String> {
        Ok(self
            .resolve_server_environment(server_name, config)?
            .map_or_else(
                || Arc::new(ReqwestHttpClient) as Arc<dyn HttpClient>,
                |environment| environment.get_http_client(),
            ))
    }
}

pub(crate) fn emit_duration(metric: &str, duration: Duration, tags: &[(&str, &str)]) {
    if let Some(metrics) = codex_otel::global() {
        let _ = metrics.record_duration(metric, duration, tags);
    }
}

