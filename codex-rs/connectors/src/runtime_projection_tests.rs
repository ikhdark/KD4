use std::collections::BTreeMap;

use codex_config::AppRequirementToml;
use codex_config::AppsRequirementsToml;
use codex_config::ConfigLayerStack;
use codex_config::ConfigRequirements;
use codex_config::ConfigRequirementsToml;
use pretty_assertions::assert_eq;

use super::*;

#[test]
fn projection_deduplicates_apps_and_ignores_non_runtime_tools() {
    let config = ConfigLayerStack::new(
        Vec::new(),
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("config layer stack");
    let apps = installed_connector_runtime(
        &config,
        [
            tool(Some(" drive "), /*connector_name*/ None, "files/list"),
            tool(Some("drive"), Some(" Drive "), "files/get"),
            ConnectorRuntimeTool {
                synthetic: true,
                ..tool(Some("synthetic"), Some("Synthetic"), "link")
            },
            tool(Some(" "), Some("Empty"), "empty"),
            tool(/*connector_id*/ None, Some("Missing"), "missing"),
        ],
    )
    .unwrap();

    assert_eq!(
        apps,
        vec![InstalledConnectorRuntime {
            id: "drive".to_string(),
            runtime_name: Some("Drive".to_string()),
            enabled: true,
            callable: true,
        }]
    );
}

#[test]
fn projection_applies_managed_app_policy_and_model_visibility() {
    let requirements = ConfigRequirementsToml {
        apps: Some(AppsRequirementsToml {
            apps: BTreeMap::from([(
                "disabled".to_string(),
                AppRequirementToml {
                    enabled: Some(false),
                    tools: None,
                },
            )]),
        }),
        ..Default::default()
    };
    let config = ConfigLayerStack::new(Vec::new(), ConfigRequirements::default(), requirements)
        .expect("config layer stack");
    let apps = installed_connector_runtime(
        &config,
        [
            tool(Some("disabled"), Some("Disabled"), "disabled/tool"),
            ConnectorRuntimeTool {
                model_visible: false,
                ..tool(Some("hidden"), Some("Hidden"), "hidden/tool")
            },
            tool(Some("callable"), Some("Callable"), "callable/tool"),
        ],
    )
    .unwrap();

    assert_eq!(
        apps,
        vec![
            InstalledConnectorRuntime {
                id: "callable".to_string(),
                runtime_name: Some("Callable".to_string()),
                enabled: true,
                callable: true,
            },
            InstalledConnectorRuntime {
                id: "disabled".to_string(),
                runtime_name: Some("Disabled".to_string()),
                enabled: false,
                callable: false,
            },
            InstalledConnectorRuntime {
                id: "hidden".to_string(),
                runtime_name: Some("Hidden".to_string()),
                enabled: true,
                callable: false,
            },
        ]
    );
}

fn tool<'a>(
    connector_id: Option<&'a str>,
    connector_name: Option<&'a str>,
    tool_name: &'a str,
) -> ConnectorRuntimeTool<'a> {
    ConnectorRuntimeTool {
        connector_id,
        connector_name,
        tool_name,
        tool_title: None,
        destructive_hint: None,
        open_world_hint: None,
        synthetic: false,
        model_visible: true,
    }
}

#[test]
fn projection_keeps_any_callable_tool_and_later_names_in_either_order() {
    let config = ConfigLayerStack::new(
        Vec::new(),
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .unwrap();
    let path =
        codex_config::AbsolutePathBuf::try_from(std::env::temp_dir().join("config.toml")).unwrap();
    let config = config.with_user_config(
        &path,
        codex_config::TomlValue::try_from(serde_json::json!({
            "apps": {"drive": {"tools": {"blocked": {"enabled": false}}}}
        }))
        .unwrap(),
    );
    for hidden in [false, true] {
        for reverse in [false, true] {
            let mut tools = [
                tool(Some("drive"), None, "allowed"),
                ConnectorRuntimeTool {
                    model_visible: !hidden,
                    ..tool(
                        Some("drive"),
                        Some(" Drive "),
                        if hidden { "hidden" } else { "blocked" },
                    )
                },
            ];
            if reverse {
                tools.reverse();
            }
            assert_eq!(
                installed_connector_runtime(&config, tools).unwrap(),
                vec![InstalledConnectorRuntime {
                    id: "drive".to_string(),
                    runtime_name: Some("Drive".to_string()),
                    enabled: true,
                    callable: true,
                }]
            );
        }
    }
    assert_eq!(
        installed_connector_runtime(&config, [tool(Some("drive"), None, "blocked")]).unwrap()[0]
            .callable,
        false
    );
}

#[test]
fn synthetic_marker_requires_boolean_true() {
    for (metadata, expected) in [
        (None, false),
        (Some(serde_json::json!({})), false),
        (Some(serde_json::json!({"synthetic_link": true})), true),
        (Some(serde_json::json!({"synthetic_link": false})), false),
        (Some(serde_json::json!({"synthetic_link": "true"})), false),
    ] {
        assert_eq!(connector_tool_is_synthetic(metadata.as_ref()), expected);
    }
}
