use std::collections::HashMap;
use std::collections::HashSet;

use crate::AppInfo;
use crate::canonical_connector_id;
use crate::metadata::connector_install_url;
use crate::metadata::sort_connectors_by_accessibility_and_name;

pub fn merge_connectors(
    connectors: Vec<AppInfo>,
    accessible_connectors: Vec<AppInfo>,
) -> Vec<AppInfo> {
    let mut merged: HashMap<String, AppInfo> = connectors
        .into_iter()
        .filter_map(canonicalize_connector)
        .map(|mut connector| {
            connector.is_accessible = false;
            (connector.id.clone(), connector)
        })
        .collect();

    for mut connector in accessible_connectors.into_iter().filter_map(canonicalize_connector) {
        connector.is_accessible = true;
        let connector_id = connector.id.clone();
        if let Some(existing) = merged.get_mut(&connector_id) {
            existing.is_accessible = true;
            if existing.name == existing.id && connector.name != connector.id {
                existing.name = connector.name;
            }
            if existing.description.is_none() && connector.description.is_some() {
                existing.description = connector.description;
            }
            if existing.logo_url.is_none() && connector.logo_url.is_some() {
                existing.logo_url = connector.logo_url;
            }
            if existing.logo_url_dark.is_none() && connector.logo_url_dark.is_some() {
                existing.logo_url_dark = connector.logo_url_dark;
            }
            if existing.icon_assets.is_none() && connector.icon_assets.is_some() {
                existing.icon_assets = connector.icon_assets;
            }
            if existing.icon_dark_assets.is_none() && connector.icon_dark_assets.is_some() {
                existing.icon_dark_assets = connector.icon_dark_assets;
            }
            if existing.distribution_channel.is_none() && connector.distribution_channel.is_some() {
                existing.distribution_channel = connector.distribution_channel;
            }
            if existing.branding.is_none() {
                existing.branding = connector.branding;
            }
            if existing.app_metadata.is_none() {
                existing.app_metadata = connector.app_metadata;
            }
            if existing.labels.is_none() {
                existing.labels = connector.labels;
            }
            // The accessible result carries runtime enablement; directory entries and
            // plugin placeholders are metadata. Hosts still apply effective app policy.
            existing.is_enabled = connector.is_enabled;
            existing
                .plugin_display_names
                .extend(connector.plugin_display_names);
        } else {
            merged.insert(connector_id, connector);
        }
    }

    let mut merged = merged.into_values().collect::<Vec<_>>();
    for connector in &mut merged {
        if connector.install_url.is_none() {
            connector.install_url = Some(connector_install_url(&connector.name, &connector.id));
        }
        connector.plugin_display_names.sort_unstable();
        connector.plugin_display_names.dedup();
    }
    sort_connectors_by_accessibility_and_name(&mut merged);
    merged
}

pub fn merge_plugin_connectors<I>(connectors: Vec<AppInfo>, plugin_app_ids: I) -> Vec<AppInfo>
where
    I: IntoIterator<Item = String>,
{
    let mut connector_ids = HashSet::new();
    let mut merged = connectors
        .into_iter()
        .filter_map(canonicalize_connector)
        .filter(|connector| connector_ids.insert(connector.id.clone()))
        .collect::<Vec<_>>();

    for connector_id in plugin_app_ids {
        let Some(canonical_id) = canonical_connector_id(&connector_id) else {
            continue;
        };
        // Existing directory rows and earlier canonical aliases win without
        // constructing a placeholder name or install URL that will be dropped.
        if connector_ids.contains(canonical_id) {
            continue;
        }
        if let Some(connector) = plugin_connector_to_app_info(connector_id) {
            connector_ids.insert(connector.id.clone());
            merged.push(connector);
        }
    }

    sort_connectors_by_accessibility_and_name(&mut merged);
    merged
}

pub fn merge_plugin_connectors_with_accessible<I>(
    _plugin_app_ids: I,
    accessible_connectors: Vec<AppInfo>,
) -> Vec<AppInfo>
where
    I: IntoIterator<Item = String>,
{
    // Declarations add no installed IDs here. Preserve accessible metadata directly.
    merge_connectors(Vec::new(), accessible_connectors)
}

/// Creates a metadata-only placeholder for a nonempty canonical connector ID.
pub fn plugin_connector_to_app_info(connector_id: String) -> Option<AppInfo> {
    let connector_id = canonical_connector_id(&connector_id)?.to_string();
    // Leave the placeholder name as the connector id so merge_connectors() can
    // replace it with canonical app metadata from directory fetches or
    // connector_name values from codex_apps tool discovery.
    let install_url = connector_install_url(&connector_id, &connector_id);
    Some(AppInfo {
        name: connector_id.clone(),
        id: connector_id,
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: Some(install_url),
        is_accessible: false,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    })
}

fn canonicalize_connector(mut connector: AppInfo) -> Option<AppInfo> {
    let id = canonical_connector_id(&connector.id)?;
    if id != connector.id {
        let id = id.to_string();
        let generated_install_url = connector
            .install_url
            .as_ref()
            .is_some_and(|url| *url == connector_install_url(&connector.name, &connector.id));
        if connector.name == connector.id {
            connector.name = id.clone();
        }
        connector.id = id;
        if generated_install_url {
            connector.install_url = Some(connector_install_url(&connector.name, &connector.id));
        }
    }
    Some(connector)
}
