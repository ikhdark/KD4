//! External marketplace selection routed through the retained plugin owners.
use super::{ExternalAgentConfigRequestProcessor, failure, files};
use codex_app_server_protocol::{ExternalAgentConfigImportTypeResult, ExternalAgentConfigMigrationItem, ExternalAgentConfigImportItemTypeSuccess, PluginsMigration};
use codex_core_plugins::PluginInstallRequest;
use codex_core_plugins::marketplace::find_marketplace_manifest_path;
use codex_core_plugins::marketplace_add::{MarketplaceAddRequest, add_marketplace};
use codex_config::ConfigLayerStackOrdering;

fn configured_plugin_keys(config: &codex_core::config::Config) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for layer in config.config_layer_stack.get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, false) {
        if let Some(plugins) = layer.config.get("plugins").and_then(toml::Value::as_table) {
            keys.extend(plugins.keys().cloned());
        }
    }
    keys
}

impl ExternalAgentConfigRequestProcessor {
    pub(super) async fn filter_detected_plugins(&self, items: &mut Vec<ExternalAgentConfigMigrationItem>) -> Result<(), std::io::Error> {
        for item in items.iter_mut().filter(|item| item.item_type == codex_app_server_protocol::ExternalAgentConfigMigrationItemType::Plugins) {
            let config = self.config_manager.load_latest_config(item.cwd.clone()).await?;
            let configured = configured_plugin_keys(&config);
            if let Some(details) = item.details.as_mut() {
                for group in &mut details.plugins {
                    group.plugin_names.retain(|name| !configured.contains(&format!("{name}@{}", group.marketplace_name)));
                }
                details.plugins.retain(|group| !group.plugin_names.is_empty());
            }
        }
        items.retain(|item| item.item_type != codex_app_server_protocol::ExternalAgentConfigMigrationItemType::Plugins
            || item.details.as_ref().is_some_and(|details| !details.plugins.is_empty()));
        Ok(())
    }

    pub(super) async fn import_plugin_item(&self, item: ExternalAgentConfigMigrationItem) -> ExternalAgentConfigImportTypeResult {
        let mut result = ExternalAgentConfigImportTypeResult { item_type: item.item_type, successes: Vec::new(), failures: Vec::new() };
        let Some(details) = item.details.as_ref() else {
            result.failures.push(failure(&item, "plugin_import", "plugins migration item is missing details".to_string(), None));
            return result;
        };
        let home = self.codex_home.clone();
        let selection = item.clone();
        let sources = match tokio::task::spawn_blocking(move || files::plugin_sources(&home, &selection)).await {
            Ok(Ok(sources)) => sources,
            Ok(Err(err)) => { result.failures.push(failure(&item, "plugin_import", err.to_string(), None)); return result; },
            Err(err) => { result.failures.push(failure(&item, "plugin_import", format!("marketplace source lookup failed: {err}"), None)); return result; },
        };
        let cwd = item.cwd.clone().filter(|cwd| !cwd.as_os_str().is_empty());
        let mut selected = std::collections::HashSet::new();
        for group in &details.plugins {
            if self.shutdown.is_cancelled() {
                group_failure(&mut result, &item, group, "plugin import cancelled".to_string());
                continue;
            }
            let Some(source) = sources.get(&group.marketplace_name) else {
                group_failure(&mut result, &item, group, format!("external agent plugin marketplace source was not found or uses an unsupported source kind: {}", group.marketplace_name));
                continue;
            };
            let config = match self.config_manager.load_latest_config(cwd.clone()).await {
                Ok(config) => config,
                Err(err) => { group_failure(&mut result, &item, group, format!("failed to load config: {err}")); continue; },
            };
            // Current marketplace policy validates source and requirements; the
            // current install owner stages files and config under one lifetime.
            let added = match add_marketplace(self.codex_home.clone(), config.config_layer_stack.requirements().clone(), MarketplaceAddRequest {
                source: source.source.clone(), ref_name: source.ref_name.clone(), sparse_paths: Vec::new(),
            }).await {
                Ok(added) => added,
                Err(err) => { group_failure(&mut result, &item, group, err.to_string()); continue; },
            };
            self.config_manager.invalidate_load_cache();
            if added.marketplace_name != group.marketplace_name {
                group_failure(&mut result, &item, group, format!("marketplace source identifies as {:?}, not {:?}", added.marketplace_name, group.marketplace_name));
                continue;
            }
            let root = added.installed_root;
            let manifest = match tokio::task::spawn_blocking(move || find_marketplace_manifest_path(root.as_path())).await {
                Ok(Some(manifest)) => manifest,
                _ => { group_failure(&mut result, &item, group, "plugin marketplace manifest was not found after install".to_string()); continue; },
            };
            let config = match self.config_manager.load_latest_config(cwd.clone()).await {
                Ok(config) => config,
                Err(err) => { group_failure(&mut result, &item, group, format!("failed to reload config after adding marketplace: {err}")); continue; },
            };
            let configured = configured_plugin_keys(&config);
            for name in &group.plugin_names {
                let id = format!("{name}@{}", group.marketplace_name);
                if !selected.insert(id.clone()) { continue; }
                if configured.contains(&id) { continue; }
                if self.shutdown.is_cancelled() { result.failures.push(failure(&item, "plugin_import", "plugin import cancelled".to_string(), Some(id))); continue; }
                match self.thread_manager.plugins_manager().install_plugin(&config.config_layer_stack, PluginInstallRequest {
                    plugin_name: name.clone(), marketplace_path: manifest.clone(),
                }).await {
                    Ok(_) => result.successes.push(ExternalAgentConfigImportItemTypeSuccess {
                        item_type: item.item_type, cwd: item.cwd.clone(), source: Some(id.clone()), target: Some(id),
                    }),
                    Err(err) => {
                        let mut failure = failure(&item, "plugin_import", err.to_string(), Some(id));
                        if matches!(&err, codex_core_plugins::PluginInstallError::Marketplace(
                            codex_core_plugins::marketplace::MarketplaceError::PluginNotFound { .. }
                        )) { failure.error_type = Some("plugin_not_found".to_string()); }
                        result.failures.push(failure);
                    },
                }
            }
            self.config_processor.handle_config_mutation();
        }
        result
    }
}

fn group_failure(result: &mut ExternalAgentConfigImportTypeResult, item: &ExternalAgentConfigMigrationItem, group: &PluginsMigration, message: String) {
    result.failures.extend(group.plugin_names.iter().map(|name| failure(
        item, "plugin_import", message.clone(), Some(format!("{name}@{}", group.marketplace_name)),
    )));
}

