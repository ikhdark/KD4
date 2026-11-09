use std::collections::HashMap;

use codex_config::AppToolApproval;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;

/// MCP server after runtime additions have been applied.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveMcpServer {
    config: Box<McpServerConfig>,
    host_owned_apps: bool,
}

impl EffectiveMcpServer {
    pub fn configured(config: McpServerConfig) -> Self {
        Self {
            config: Box::new(config),
            host_owned_apps: false,
        }
    }

    pub(crate) fn host_owned_apps(config: McpServerConfig) -> Self {
        Self {
            config: Box::new(config),
            host_owned_apps: true,
        }
    }

    pub(crate) fn is_host_owned_apps(&self) -> bool {
        self.host_owned_apps
    }

    pub(crate) fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn configured_config(&self) -> Option<&McpServerConfig> {
        Some(self.config())
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn required(&self) -> bool {
        self.config.required
    }
}

/// Transport origin retained for metrics and diagnostics after server launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpServerOrigin {
    Stdio,
    StreamableHttp(String),
}

impl McpServerOrigin {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stdio => "stdio",
            Self::StreamableHttp(origin) => origin,
        }
    }

    fn from_transport(transport: &McpServerTransportConfig) -> Option<Self> {
        match transport {
            McpServerTransportConfig::StreamableHttp { url, .. } => {
                let parsed = url::Url::parse(url).ok()?;
                Some(Self::StreamableHttp(parsed.origin().ascii_serialization()))
            }
            McpServerTransportConfig::Stdio { .. } => Some(Self::Stdio),
        }
    }
}

/// Semantic metadata that must survive after the server is launched.
#[derive(Debug, Clone)]
pub(crate) struct McpServerMetadata {
    pub environment_id: String,
    pub origin: Option<McpServerOrigin>,
    pub supports_parallel_tool_calls: bool,
    pub default_tools_approval_mode: Option<AppToolApproval>,
    pub tool_approval_modes: HashMap<String, AppToolApproval>,
}

impl McpServerMetadata {
    pub fn tool_approval_mode(&self, tool_name: &str) -> AppToolApproval {
        self.tool_approval_modes
            .get(tool_name)
            .copied()
            .or(self.default_tools_approval_mode)
            .unwrap_or_default()
    }
}

impl From<&EffectiveMcpServer> for McpServerMetadata {
    fn from(server: &EffectiveMcpServer) -> Self {
        let config = server.config();
        Self {
            environment_id: config.environment_id.clone(),
            origin: McpServerOrigin::from_transport(&config.transport),
            supports_parallel_tool_calls: config.supports_parallel_tool_calls,
            default_tools_approval_mode: config.default_tools_approval_mode,
            tool_approval_modes: config
                .tools
                .iter()
                .filter_map(|(name, config)| {
                    config
                        .approval_mode
                        .map(|approval_mode| (name.clone(), approval_mode))
                })
                .collect(),
        }
    }
}
