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

    for connector in plugin_app_ids.into_iter().filter_map(plugin_connector_to_app_info) {
        if connector_ids.insert(connector.id.clone()) {
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
    let name = connector_id.clone();
    Some(AppInfo {
        id: connector_id.clone(),
        name: name.clone(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: Some(connector_install_url(&name, &connector_id)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::connector_install_url;
    use crate::metadata::connector_mention_slug;
    use pretty_assertions::assert_eq;

    fn plugin_names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn google_calendar_accessible_connector(plugin_display_names: &[&str]) -> AppInfo {
        AppInfo {
            id: "calendar".to_string(),
            name: "Google Calendar".to_string(),
            description: Some("Plan events".to_string()),
            logo_url: Some("https://example.com/logo.png".to_string()),
            logo_url_dark: Some("https://example.com/logo-dark.png".to_string()),
            icon_assets: None,
            icon_dark_assets: None,
            distribution_channel: Some("workspace".to_string()),
            branding: None,
            app_metadata: None,
            labels: None,
            install_url: None,
            is_accessible: true,
            is_enabled: true,
            plugin_display_names: plugin_names(plugin_display_names),
        }
    }

    #[test]
    fn merge_connectors_replaces_placeholder_and_dedupes_plugin_names() {
        for (declared, runtime, expected) in [
            (vec![], vec![], vec![]),
            (
                vec!["sample", "alpha", "sample"],
                vec!["beta", "alpha"],
                vec!["alpha", "beta", "sample"],
            ),
        ] {
            let mut plugin = plugin_connector_to_app_info("calendar".to_string()).unwrap();
            plugin.plugin_display_names = plugin_names(&declared);
            let accessible = google_calendar_accessible_connector(&runtime);

            let merged = merge_connectors(vec![plugin], vec![accessible]);

            assert_eq!(
                merged,
                vec![AppInfo {
                    id: "calendar".to_string(),
                    name: "Google Calendar".to_string(),
                    description: Some("Plan events".to_string()),
                    logo_url: Some("https://example.com/logo.png".to_string()),
                    logo_url_dark: Some("https://example.com/logo-dark.png".to_string()),
                    icon_assets: None,
                    icon_dark_assets: None,
                    distribution_channel: Some("workspace".to_string()),
                    branding: None,
                    app_metadata: None,
                    labels: None,
                    install_url: Some(connector_install_url("calendar", "calendar")),
                    is_accessible: true,
                    is_enabled: true,
                    plugin_display_names: plugin_names(&expected),
                }]
            );
            assert_eq!(connector_mention_slug(&merged[0]), "google-calendar");
        }
    }

    #[test]
    fn merge_preserves_accessible_metadata_and_enablement_on_collision() {
        let mut accessible = google_calendar_accessible_connector(&[]);
        accessible.is_enabled = false;
        accessible.branding = Some(
            serde_json::from_value(serde_json::json!({
                "category": "productivity", "isDiscoverableApp": true
            }))
            .unwrap(),
        );
        accessible.app_metadata = Some(
            serde_json::from_value(serde_json::json!({
                "categories": ["calendar"], "developer": "Calendar developer"
            }))
            .unwrap(),
        );
        accessible.labels = Some(HashMap::from([("label".to_string(), "value".to_string())]));
        let mut expected = accessible.clone();
        expected.install_url = Some(connector_install_url("calendar", "calendar"));
        assert_eq!(
            merge_connectors(
                vec![plugin_connector_to_app_info("calendar".to_string()).unwrap()],
                vec![accessible.clone()]
            ),
            vec![expected]
        );

        let mut directory = plugin_connector_to_app_info("calendar".to_string()).unwrap();
        directory.labels = Some(HashMap::from([(
            "directory".to_string(),
            "kept".to_string(),
        )]));
        let merged = merge_connectors(vec![directory.clone()], vec![accessible]);
        assert_eq!(merged[0].labels, directory.labels);
    }

    #[test]
    fn plugin_merge_canonicalizes_raw_declarations_like_snapshots() {
        let declarations = crate::parse_plugin_app_config(
            r#"{"apps":{"Calendar":{"id":" calendar "},"Duplicate":{"id":"calendar"},"CaseDistinct":{"id":"Calendar"},"Blank":{"id":" \t "}}}"#,
        )
        .unwrap();
        let raw_ids = declarations
            .iter()
            .map(|declaration| declaration.connector_id.0.clone())
            .collect::<Vec<_>>();
        let snapshot = crate::ConnectorSnapshot::from_plugin_sources([
            crate::PluginConnectorSource::new("plugin", "Plugin", declarations),
        ]);
        let directory = vec![plugin_connector_to_app_info("calendar".to_string()).unwrap()];
        let raw = merge_plugin_connectors(directory.clone(), raw_ids);
        let normalized = merge_plugin_connectors(
            directory,
            snapshot.connector_ids().iter().map(|id| id.0.clone()),
        );
        assert_eq!(raw, normalized);
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().any(|connector| connector.id == "Calendar"));
        assert!(raw.iter().any(|connector| connector.id == "calendar"));
        let merged = merge_connectors(raw, vec![google_calendar_accessible_connector(&[])]);
        assert_eq!(merged.len(), 2);
        let calendar = merged
            .iter()
            .find(|connector| connector.id == "calendar")
            .unwrap();
        assert!(calendar.is_accessible);
        assert_eq!(calendar.name, "Google Calendar");
        assert_eq!(
            calendar.install_url,
            Some(connector_install_url("calendar", "calendar"))
        );
    }

    #[test]
    fn merge_boundaries_canonicalize_existing_and_accessible_rows() {
        let mut padded = plugin_connector_to_app_info("calendar".to_string()).unwrap();
        padded.id = " calendar ".to_string();
        padded.name = padded.id.clone();
        padded.install_url = Some(connector_install_url(&padded.name, &padded.id));
        let mut invalid = padded.clone();
        invalid.id = " \t ".to_string();
        let mut accessible = google_calendar_accessible_connector(&[]);
        accessible.id = "\tcalendar ".to_string();
        let mut expected = accessible.clone();
        expected.id = "calendar".to_string();
        expected.install_url = Some(connector_install_url("calendar", "calendar"));
        assert_eq!(
            merge_connectors(
                vec![padded.clone(), invalid.clone()],
                vec![accessible, invalid.clone()]
            ),
            vec![expected]
        );
        assert_eq!(
            merge_plugin_connectors(
                vec![padded.clone(), padded.clone(), invalid],
                [" calendar ".to_string()]
            ),
            vec![plugin_connector_to_app_info("calendar".to_string()).unwrap()]
        );
        assert_eq!(plugin_connector_to_app_info(" \t ".to_string()), None);
        assert_eq!(
            plugin_connector_to_app_info(" calendar ".to_string()),
            plugin_connector_to_app_info("calendar".to_string())
        );
        // Only generated install URLs are repaired; canonicalization must not
        // replace real metadata supplied by the directory.
        padded.name = "Google Calendar".to_string();
        padded.install_url = Some("https://example.com/custom-calendar-install".to_string());
        let merged = merge_connectors(vec![padded], Vec::new());
        assert_eq!(merged[0].id, "calendar");
        assert_eq!(merged[0].name, "Google Calendar");
        assert_eq!(
            merged[0].install_url.as_deref(),
            Some("https://example.com/custom-calendar-install")
        );
    }

    #[test]
    fn plugin_accessible_merge_preserves_real_metadata_and_only_accessible_ids() {
        let mut accessible = google_calendar_accessible_connector(&["Plugin", "Plugin"]);
        accessible.is_enabled = false;
        accessible.labels = Some(HashMap::from([("label".to_string(), "value".to_string())]));
        let mut expected = accessible.clone();
        expected.plugin_display_names = vec!["Plugin".to_string()];
        expected.install_url = Some(connector_install_url("Google Calendar", "calendar"));
        for plugin_ids in [
            vec![],
            vec!["calendar".to_string(), "uninstalled".to_string()],
        ] {
            assert_eq!(
                merge_plugin_connectors_with_accessible(plugin_ids, vec![accessible.clone()]),
                vec![expected.clone()]
            );
        }
    }
}
