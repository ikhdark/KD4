use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::AppInfo;
use crate::metadata::connector_install_url;
use crate::normalize_connector_value;

pub struct AccessibleConnectorTool {
    pub connector_id: String,
    pub connector_name: Option<String>,
    pub connector_description: Option<String>,
    pub plugin_display_names: Vec<String>,
}

pub fn collect_accessible_connectors<I>(tools: I) -> Vec<AppInfo>
where
    I: IntoIterator<Item = AccessibleConnectorTool>,
{
    let mut connectors: HashMap<String, (AppInfo, BTreeSet<String>)> = HashMap::new();
    for tool in tools {
        let connector_id = tool.connector_id;
        if let Some((existing, existing_plugin_display_names)) = connectors.get_mut(&connector_id) {
            if existing.name == connector_id
                && let Some(connector_name) =
                    normalize_connector_value(tool.connector_name.as_deref())
            {
                existing.name = connector_name;
            }
            if existing.description.is_none() {
                existing.description =
                    normalize_connector_value(tool.connector_description.as_deref());
            }
            existing_plugin_display_names.extend(tool.plugin_display_names);
        } else {
            connectors.insert(
                connector_id.clone(),
                (
                    AppInfo {
                        id: connector_id.clone(),
                        name: normalize_connector_value(tool.connector_name.as_deref())
                            .unwrap_or_else(|| connector_id.clone()),
                        description: normalize_connector_value(
                            tool.connector_description.as_deref(),
                        ),
                        logo_url: None,
                        logo_url_dark: None,
                        icon_assets: None,
                        icon_dark_assets: None,
                        distribution_channel: None,
                        branding: None,
                        app_metadata: None,
                        labels: None,
                        install_url: None,
                        is_accessible: true,
                        is_enabled: true,
                        plugin_display_names: Vec::new(),
                    },
                    tool.plugin_display_names
                        .into_iter()
                        .collect::<BTreeSet<String>>(),
                ),
            );
        }
    }
    let mut accessible: Vec<AppInfo> = connectors
        .into_values()
        .map(|(mut connector, plugin_display_names)| {
            connector.plugin_display_names = plugin_display_names.into_iter().collect();
            connector.install_url = Some(connector_install_url(&connector.name, &connector.id));
            connector
        })
        .collect();
    accessible.sort_by(|left, right| {
        right
            .is_accessible
            .cmp(&left.is_accessible)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    accessible
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn repeated_tools_fill_missing_metadata_and_keep_first_real_values() {
        let tools = [
            (None, Some("  ")),
            (Some(" Calendar "), None),
            (Some("Ignored"), Some(" Plan events ")),
            (Some("Later"), Some("Ignored")),
        ]
        .into_iter()
        .map(|(name, description)| AccessibleConnectorTool {
            connector_id: "calendar".to_string(),
            connector_name: name.map(str::to_string),
            connector_description: description.map(str::to_string),
            plugin_display_names: vec!["Plugin".to_string()],
        });
        let mut expected = crate::merge::plugin_connector_to_app_info("calendar".to_string());
        expected.name = "Calendar".to_string();
        expected.description = Some("Plan events".to_string());
        expected.install_url = Some(connector_install_url("Calendar", "calendar"));
        expected.is_accessible = true;
        expected.plugin_display_names = vec!["Plugin".to_string()];
        assert_eq!(collect_accessible_connectors(tools), vec![expected]);
    }
}
