use codex_plugin::AppConnectorId;
use codex_plugin::AppDeclaration;
use pretty_assertions::assert_eq;

use super::parse_plugin_app_config;
use super::parse_plugin_app_config_value;

#[test]
fn parses_plugin_app_config_in_order_without_validating_connector_ids() {
    let parsed = parse_plugin_app_config(
        r#"{
            "apps": {
                "calendar": {
                    "id": "connector_calendar",
                    "category": "  productivity  "
                },
                "drive": {
                    "id": "connector_calendar",
                    "category": "  "
                },
                "blank": {
                    "id": "  "
                }
            }
        }"#,
    )
    .expect("plugin app config should parse");

    assert_eq!(
        parsed,
        vec![
            AppDeclaration {
                name: "calendar".to_string(),
                connector_id: AppConnectorId("connector_calendar".to_string()),
                category: Some("productivity".to_string()),
            },
            AppDeclaration {
                name: "drive".to_string(),
                connector_id: AppConnectorId("connector_calendar".to_string()),
                category: None,
            },
            AppDeclaration {
                name: "blank".to_string(),
                connector_id: AppConnectorId("  ".to_string()),
                category: None,
            },
        ]
    );
}

#[test]
fn rejects_invalid_plugin_app_config() {
    assert!(parse_plugin_app_config("not json").is_err());
    for contents in [r#"{"apps":{"app":{}}}"#, r#"{"apps":{"app":{"id":42}}}"#] {
        assert!(parse_plugin_app_config(contents).is_err());
        assert!(parse_plugin_app_config_value(serde_json::from_str(contents).unwrap()).is_err());
    }
}

#[test]
fn value_parser_preserves_available_order_and_cleans_categories() {
    let value = serde_json::json!({"apps": {
        "zeta": {"id": " z ", "category": " work "},
        "alpha": {"id": "a", "category": " "}
    }});
    let expected = value["apps"]
        .as_object()
        .unwrap()
        .keys()
        .map(|name| {
            if name == "zeta" {
                AppDeclaration {
                    name: name.clone(),
                    connector_id: AppConnectorId(" z ".to_string()),
                    category: Some("work".to_string()),
                }
            } else {
                AppDeclaration {
                    name: name.clone(),
                    connector_id: AppConnectorId("a".to_string()),
                    category: None,
                }
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(parse_plugin_app_config_value(value).unwrap(), expected);
}
