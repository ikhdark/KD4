use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Weak;

use codex_core_skills::config_rules::SkillConfigRules;
use codex_plugin::AppDeclaration;
use codex_plugin::PluginCapabilitySummary;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_plugin::app_connector_ids_from_declarations;
use codex_plugin::prompt_safe_plugin_description;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::Product;
use tokio::sync::OnceCell;

use crate::app_mcp_routing::apply_app_mcp_routing_policy;
use crate::loader::PluginSkillInventory;
use crate::loader::load_plugin_apps;
use crate::loader::load_plugin_mcp_servers;
use crate::loader::load_plugin_skill_inventory;
use crate::manager::ConfiguredMarketplacePlugin;
use crate::manager::remote_plugin_install_required_description;
use crate::manifest::load_plugin_manifest;
use crate::marketplace::MarketplaceError;
use crate::marketplace::MarketplacePluginSource;

const MAX_TOOL_SUGGEST_METADATA_CACHE_ENTRIES: usize = 1024;

type ToolSuggestMetadataEntry = Result<Arc<ToolSuggestMetadataFragment>, String>;

/// Source-derived plugin metadata cached for tool suggestions.
///
/// `PluginsManager` clears these entries alongside its loaded-plugin cache. Current skill config
/// and auth routing are projected after each lookup and are not part of this cache.
pub(crate) struct ToolSuggestMetadataCache {
    state: RwLock<ToolSuggestMetadataCacheState>,
}

#[derive(Default)]
struct ToolSuggestMetadataCacheState {
    generation: u64,
    entries: VecDeque<(PluginArtifactIdentity, ToolSuggestMetadataEntry)>,
    flights: Vec<ToolSuggestMetadataFlight>,
}

struct ToolSuggestMetadataFlight {
    generation: u64,
    artifact: PluginArtifactIdentity,
    entry: Weak<OnceCell<ToolSuggestMetadataEntry>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PluginArtifactIdentity {
    plugin_id: String,
    source: MarketplacePluginSource,
}

pub(crate) struct ToolSuggestMetadataFragment {
    config_name: String,
    display_name: String,
    description: Option<String>,
    mcp_server_names: Vec<String>,
    app_declarations: Vec<AppDeclaration>,
    skill_inventory: Option<PluginSkillInventory>,
}

impl ToolSuggestMetadataFragment {
    pub(crate) fn project(
        &self,
        skill_config_rules: &SkillConfigRules,
        auth_mode: Option<AuthMode>,
    ) -> PluginCapabilitySummary {
        let mut app_declarations = self.app_declarations.clone();
        let mut mcp_servers = self
            .mcp_server_names
            .iter()
            .cloned()
            .map(|name| (name, ()))
            .collect::<HashMap<_, _>>();
        if auth_mode.is_some() {
            apply_app_mcp_routing_policy(
                &mut app_declarations,
                &mut mcp_servers,
                auth_mode,
                /*plugin_active*/ true,
            );
        }
        let mut mcp_server_names = mcp_servers.into_keys().collect::<Vec<_>>();
        mcp_server_names.sort_unstable();

        PluginCapabilitySummary {
            config_name: self.config_name.clone(),
            display_name: self.display_name.clone(),
            description: self.description.clone(),
            has_skills: self
                .skill_inventory
                .as_ref()
                .is_some_and(|inventory| inventory.has_enabled_skills(skill_config_rules)),
            mcp_server_names,
            app_connector_ids: app_connector_ids_from_declarations(&app_declarations),
        }
    }
}

impl ToolSuggestMetadataCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(ToolSuggestMetadataCacheState::default()),
        }
    }

    pub(crate) fn clear(&self) {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
        state.flights.clear();
    }

    pub(crate) async fn metadata_for_plugin(
        &self,
        marketplace_name: &str,
        plugin: &ConfiguredMarketplacePlugin,
        restriction_product: Option<Product>,
    ) -> Result<Arc<ToolSuggestMetadataFragment>, MarketplaceError> {
        let artifact = PluginArtifactIdentity {
            plugin_id: plugin.id.clone(),
            source: plugin.source.clone(),
        };
        loop {
            if let Some(entry) = self.cached_entry(&artifact) {
                return entry.map_err(MarketplaceError::InvalidPlugin);
            }

            let (generation, load) = self.flight_for_artifact(&artifact);
            let entry = load
                .get_or_init(|| async {
                    load_plugin_metadata(marketplace_name, plugin, restriction_product).await
                })
                .await
                .clone();
            if self.publish_flight_if_current(
                generation,
                &artifact,
                &load,
                entry.clone(),
            ) {
                return entry.map_err(MarketplaceError::InvalidPlugin);
            }
        }
    }

    fn cached_entry(&self, artifact: &PluginArtifactIdentity) -> Option<ToolSuggestMetadataEntry> {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        let position = state
            .entries
            .iter()
            .position(|(cached, _entry)| cached == artifact)?;
        let cached = state.entries.remove(position)?;
        let entry = cached.1.clone();
        state.entries.push_back(cached);
        Some(entry)
    }

    fn flight_for_artifact(
        &self,
        artifact: &PluginArtifactIdentity,
    ) -> (u64, Arc<OnceCell<ToolSuggestMetadataEntry>>) {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        let generation = state.generation;
        state.flights.retain(|flight| flight.entry.strong_count() > 0);
        if let Some(entry) = state
            .flights
            .iter()
            .find(|flight| flight.generation == generation && flight.artifact == *artifact)
            .and_then(|flight| flight.entry.upgrade())
        {
            return (generation, entry);
        }
        let entry = Arc::new(OnceCell::new());
        state.flights.push(ToolSuggestMetadataFlight {
            generation,
            artifact: artifact.clone(),
            entry: Arc::downgrade(&entry),
        });
        (generation, entry)
    }

    fn publish_flight_if_current(
        &self,
        generation: u64,
        artifact: &PluginArtifactIdentity,
        load: &Arc<OnceCell<ToolSuggestMetadataEntry>>,
        entry: ToolSuggestMetadataEntry,
    ) -> bool {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        state.flights.retain(|flight| {
            flight.entry.upgrade().is_some_and(|entry| {
                !(flight.generation == generation
                    && flight.artifact == *artifact
                    && Arc::ptr_eq(&entry, load))
            })
        });
        if state.generation != generation {
            return false;
        }
        if let Some(position) = state
            .entries
            .iter()
            .position(|(cached, _entry)| cached == artifact)
        {
            state.entries.remove(position);
        }
        if state.entries.len() >= MAX_TOOL_SUGGEST_METADATA_CACHE_ENTRIES {
            state.entries.pop_front();
        }
        state.entries.push_back((artifact.clone(), entry));
        true
    }
}

async fn load_plugin_metadata(
    marketplace_name: &str,
    plugin: &ConfiguredMarketplacePlugin,
    restriction_product: Option<Product>,
) -> ToolSuggestMetadataEntry {
    let plugin_id = PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(
        |err| match err {
            PluginIdError::Invalid(message) => message,
        },
    )?;

    let MarketplacePluginSource::Local { path: plugin_root } = &plugin.source else {
        return Ok(Arc::new(ToolSuggestMetadataFragment {
            config_name: plugin.id.clone(),
            display_name: plugin.name.clone(),
            description: prompt_safe_plugin_description(Some(
                &remote_plugin_install_required_description(&plugin.source),
            )),
            mcp_server_names: Vec::new(),
            app_declarations: Vec::new(),
            skill_inventory: None,
        }));
    };
    if !plugin_root.as_path().is_dir() {
        return Err("path does not exist or is not a directory".to_string());
    }
    let manifest = load_plugin_manifest(plugin_root.as_path())
        .ok_or_else(|| "missing or invalid plugin.json".to_string())?;
    let skill_inventory = load_plugin_skill_inventory(
        plugin_root,
        &plugin_id,
        &manifest,
        restriction_product,
        /*plugin_skill_snapshots*/ None,
    )
    .await;
    let mut mcp_server_names =
        load_plugin_mcp_servers(plugin_root.as_path(), /*auth_mode*/ None)
            .await
            .into_keys()
            .collect::<Vec<_>>();
    mcp_server_names.sort_unstable();
    mcp_server_names.dedup();
    let app_declarations = load_plugin_apps(plugin_root.as_path()).await;

    Ok(Arc::new(ToolSuggestMetadataFragment {
        config_name: plugin.id.clone(),
        display_name: plugin.name.clone(),
        description: prompt_safe_plugin_description(manifest.description.as_deref()),
        mcp_server_names,
        app_declarations,
        skill_inventory: Some(skill_inventory),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_utils_absolute_path::test_support::PathBufExt;

    #[test]
    fn metadata_cache_uses_keyed_singleflight() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let cache = ToolSuggestMetadataCache::new();
        let artifact_a = PluginArtifactIdentity {
            plugin_id: "a@test".to_string(),
            source: MarketplacePluginSource::Local {
                path: tempdir.path().join("a").abs(),
            },
        };
        let artifact_b = PluginArtifactIdentity {
            plugin_id: "b@test".to_string(),
            source: MarketplacePluginSource::Local {
                path: tempdir.path().join("b").abs(),
            },
        };

        let (generation_a, flight_a1) = cache.flight_for_artifact(&artifact_a);
        let (_, flight_a2) = cache.flight_for_artifact(&artifact_a);
        let (generation_b, flight_b) = cache.flight_for_artifact(&artifact_b);

        assert!(Arc::ptr_eq(&flight_a1, &flight_a2));
        assert!(!Arc::ptr_eq(&flight_a1, &flight_b));

        let entry = |config_name: &str| {
            Ok(Arc::new(ToolSuggestMetadataFragment {
                config_name: config_name.to_string(),
                display_name: config_name.to_string(),
                description: None,
                mcp_server_names: Vec::new(),
                app_declarations: Vec::new(),
                skill_inventory: None,
            }))
        };
        assert!(cache.publish_flight_if_current(
            generation_a,
            &artifact_a,
            &flight_a1,
            entry("a@test"),
        ));
        assert!(cache.publish_flight_if_current(
            generation_b,
            &artifact_b,
            &flight_b,
            entry("b@test"),
        ));
        assert_eq!(
            cache
                .cached_entry(&artifact_a)
                .and_then(Result::ok)
                .map(|fragment| fragment.config_name.clone()),
            Some("a@test".to_string())
        );
        assert_eq!(
            cache
                .cached_entry(&artifact_b)
                .and_then(Result::ok)
                .map(|fragment| fragment.config_name.clone()),
            Some("b@test".to_string())
        );
        assert!(cache.cached_entry(&artifact_a).is_some());

        let canceled_artifact = PluginArtifactIdentity {
            plugin_id: "c@test".to_string(),
            source: MarketplacePluginSource::Local {
                path: tempdir.path().join("c").abs(),
            },
        };
        let active_artifact = PluginArtifactIdentity {
            plugin_id: "d@test".to_string(),
            source: MarketplacePluginSource::Local {
                path: tempdir.path().join("d").abs(),
            },
        };
        let (_, canceled) = cache.flight_for_artifact(&canceled_artifact);
        drop(canceled);
        let (_, _active) = cache.flight_for_artifact(&active_artifact);
        assert_eq!(
            cache
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .flights
                .len(),
            1
        );
    }

    #[test]
    fn metadata_cache_rejects_stale_flight_publication() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let cache = ToolSuggestMetadataCache::new();
        let artifact = PluginArtifactIdentity {
            plugin_id: "a@test".to_string(),
            source: MarketplacePluginSource::Local {
                path: tempdir.path().join("a").abs(),
            },
        };
        let (generation, flight) = cache.flight_for_artifact(&artifact);
        cache.clear();
        let entry = Ok(Arc::new(ToolSuggestMetadataFragment {
            config_name: "a@test".to_string(),
            display_name: "a".to_string(),
            description: None,
            mcp_server_names: Vec::new(),
            app_declarations: Vec::new(),
            skill_inventory: None,
        }));

        assert!(!cache.publish_flight_if_current(
            generation,
            &artifact,
            &flight,
            entry,
        ));
        assert!(cache.cached_entry(&artifact).is_none());
    }
}
