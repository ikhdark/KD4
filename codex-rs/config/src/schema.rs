use crate::config_toml::ConfigToml;
use crate::types::RawMcpServerConfig;
use codex_features::user_settable_features;
use schemars::r#gen::SchemaGenerator;
use schemars::r#gen::SchemaSettings;
use schemars::schema::InstanceType;
use schemars::schema::ObjectValidation;
use schemars::schema::RootSchema;
use schemars::schema::Schema;
use schemars::schema::SchemaObject;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;

/// Schema for the public, user-settable `[features]` map.
pub fn features_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let mut object = SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        ..Default::default()
    };

    let mut validation = ObjectValidation::default();
    for feature in user_settable_features() {
        if feature.id == codex_features::Feature::CodeMode {
            validation.properties.insert(
                feature.key.to_string(),
                schema_gen.subschema_for::<codex_features::FeatureToml<
                    codex_features::CodeModeConfigToml,
                >>(),
            );
            continue;
        }
        if feature.id == codex_features::Feature::MultiAgentV2 {
            validation.properties.insert(
                feature.key.to_string(),
                schema_gen.subschema_for::<codex_features::FeatureToml<
                    codex_features::MultiAgentV2ConfigToml,
                >>(),
            );
            continue;
        }
        if feature.id == codex_features::Feature::CurrentTimeReminder {
            validation.properties.insert(
                feature.key.to_string(),
                schema_gen.subschema_for::<codex_features::FeatureToml<
                    codex_features::CurrentTimeReminderConfigToml,
                >>(),
            );
            continue;
        }
        if feature.id == codex_features::Feature::NetworkProxy {
            validation.properties.insert(
                feature.key.to_string(),
                schema_gen.subschema_for::<codex_features::FeatureToml<
                    codex_features::NetworkProxyConfigToml,
                >>(),
            );
            continue;
        }
        validation
            .properties
            .insert(feature.key.to_string(), schema_gen.subschema_for::<bool>());
    }
    validation.additional_properties = Some(Box::new(Schema::Bool(false)));
    object.object = Some(Box::new(validation));

    Schema::Object(object)
}

/// Schema for the `[mcp_servers]` map using the raw input shape.
pub fn mcp_servers_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let mut object = SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        ..Default::default()
    };

    let validation = ObjectValidation {
        additional_properties: Some(Box::new(schema_gen.subschema_for::<RawMcpServerConfig>())),
        ..Default::default()
    };
    object.object = Some(Box::new(validation));

    Schema::Object(object)
}

/// Build the config schema for `config.toml`.
pub fn config_schema() -> RootSchema {
    SchemaSettings::draft07()
        .with(|settings| {
            settings.option_add_null_type = false;
        })
        .into_generator()
        .into_root_schema_for::<ConfigToml>()
}

/// Canonicalize a JSON value by sorting its keys.
pub fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let mut sorted = Map::with_capacity(map.len());
            for (key, child) in entries {
                sorted.insert(key.clone(), canonicalize(child));
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

/// Render the config schema as pretty-printed JSON.
pub fn config_schema_json() -> anyhow::Result<Vec<u8>> {
    let schema = config_schema();
    let value = serde_json::to_value(schema)?;
    let value = canonicalize(&value);
    let json = serde_json::to_vec_pretty(&value)?;
    Ok(json)
}

/// Write the config schema fixture to disk.
pub fn write_config_schema(out_path: &Path) -> anyhow::Result<()> {
    let json = config_schema_json()?;
    std::fs::write(out_path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_schema_omits_removed_and_compatibility_only_settings() {
        let schema = serde_json::to_value(config_schema()).expect("schema should serialize");
        let properties = schema["properties"]
            .as_object()
            .expect("config schema properties");

        for compatibility_key in [
            "model_supports_reasoning_summaries",
            "experimental_use_unified_exec_tool",
            "ghost_snapshot",
            "profile",
            "profiles",
        ] {
            assert!(!properties.contains_key(compatibility_key));
        }

        let feature_properties = properties["features"]["properties"]
            .as_object()
            .expect("feature schema properties");
        assert!(feature_properties.contains_key("unified_exec"));
        assert!(!feature_properties.contains_key("artifact"));
        assert!(!feature_properties.contains_key("enable_mcp_apps"));
        assert!(!feature_properties.contains_key("experimental_use_unified_exec_tool"));
        assert!(!feature_properties.contains_key("terminal_resize_reflow"));
        assert!(!feature_properties.contains_key("chronicle"));

        let notice_properties = schema["definitions"]["Notice"]["properties"]
            .as_object()
            .expect("notice schema properties");
        assert!(!notice_properties.contains_key("hide_full_access_warning"));
        assert!(!notice_properties.contains_key("external_config_migration_prompts"));

        let rendered = serde_json::to_string(&schema).expect("schema should render");
        assert!(!rendered.contains("\"ConfigProfile\""));
        assert!(!rendered.contains("\"usage_hint_enabled\""));
        assert!(!rendered.contains("\"ExternalConfigMigrationPrompts\""));
    }

    #[test]
    fn background_terminal_timeout_description_matches_runtime_default() {
        let schema = serde_json::to_value(config_schema()).expect("schema should serialize");
        let description = schema["properties"]["background_terminal_max_timeout"]["description"]
            .as_str()
            .expect("background terminal timeout description");

        assert!(description.contains("Default: `60000` (1 minute)."));
    }
}
