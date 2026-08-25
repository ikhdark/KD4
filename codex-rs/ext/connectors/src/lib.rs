//! Executor-backed connector declaration loading.

use codex_connectors::parse_plugin_app_config;
use codex_core_plugins::ResolvedExecutorPlugin;
use codex_plugin::AppDeclaration;
use codex_plugin::PluginResourceLocator;
use codex_utils_path_uri::PathUri;
use std::io;
use thiserror::Error;

/// Failure to load connector declarations from an executor plugin.
#[derive(Debug, Error)]
pub enum LoadExecutorPluginConnectorsError {
    #[error("failed to read app config for selected plugin `{plugin_id}` at `{path}`: {source}")]
    ReadConfig {
        plugin_id: String,
        path: PathUri,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse app config for selected plugin `{plugin_id}` at `{path}`: {source}")]
    ParseConfig {
        plugin_id: String,
        path: PathUri,
        #[source]
        source: serde_json::Error,
    },
}

/// Returns the connector declarations contributed by `plugin`.
pub async fn load_executor_plugin_connectors(
    plugin: &ResolvedExecutorPlugin,
) -> Result<Vec<AppDeclaration>, LoadExecutorPluginConnectorsError> {
    let resolved_plugin = plugin.plugin();
    let plugin_id = resolved_plugin.selected_root_id();
    let Some(PluginResourceLocator::Environment {
        path: config_path, ..
    }) = resolved_plugin.manifest().paths.apps.as_ref()
    else {
        return Ok(Vec::new());
    };
    let contents = plugin
        .file_system()
        .read_file_text(config_path, /*sandbox*/ None)
        .await
        .map_err(|source| LoadExecutorPluginConnectorsError::ReadConfig {
            plugin_id: plugin_id.to_string(),
            path: config_path.clone(),
            source,
        })?;

    parse_plugin_app_config(&contents).map_err(|source| {
        LoadExecutorPluginConnectorsError::ParseConfig {
            plugin_id: plugin_id.to_string(),
            path: config_path.clone(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn executor_connector_loading_stays_a_free_function() {
        let source = include_str!("lib.rs");
        let removed_provider = ["struct ExecutorPluginConnector", "Provider"].concat();
        assert!(!source.contains(&removed_provider));
        assert!(source.contains("pub async fn load_executor_plugin_connectors"));
    }
}
