use super::*;
use crate::config_toml::ConfigToml;
use crate::types::MemoriesToml;
use codex_features::FeatureConfigSource;
use codex_features::FeatureOverrides;
use codex_features::Features;
use codex_features::feature_for_key;
use codex_features::legacy_feature_keys;
use pretty_assertions::assert_eq;

fn parse_toml(value: &str) -> TomlValue {
    toml::from_str(value).expect("TOML should parse")
}

fn feature_config(key: &str, enabled: bool) -> TomlValue {
    let mut features = toml::map::Map::new();
    features.insert(key.to_string(), TomlValue::Boolean(enabled));
    let mut config = toml::map::Map::new();
    config.insert("features".to_string(), TomlValue::Table(features));
    TomlValue::Table(config)
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_base_layer() {
    let mut base = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_overlay_layer() {
    let mut base = parse_toml(
        r#"
[memories]
disable_on_external_context = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn merge_toml_values_prefers_canonical_key_when_one_layer_has_both_names() {
    let mut base = TomlValue::Table(toml::map::Map::new());
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
no_memories_if_mcp_or_web_search = false
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);
}

#[test]
fn merge_toml_values_preserves_feature_alias_layer_precedence() {
    for legacy_key in legacy_feature_keys() {
        let feature = feature_for_key(legacy_key).expect("legacy feature key should resolve");
        let canonical_key = feature.key();

        for (base_key, overlay_key) in [(canonical_key, legacy_key), (legacy_key, canonical_key)] {
            for (base_enabled, overlay_enabled) in [(false, true), (true, false)] {
                let mut merged = feature_config(base_key, base_enabled);
                merge_toml_values(&mut merged, &feature_config(overlay_key, overlay_enabled));

                let config: ConfigToml = merged
                    .clone()
                    .try_into()
                    .expect("merged feature config should deserialize");
                let features = Features::from_sources(
                    FeatureConfigSource {
                        features: config.features.as_ref(),
                        experimental_use_unified_exec_tool: config
                            .experimental_use_unified_exec_tool,
                    },
                    FeatureConfigSource::default(),
                    FeatureOverrides::default(),
                );

                assert_eq!(
                    features.enabled(feature),
                    overlay_enabled,
                    "higher-precedence `{overlay_key}` should override `{base_key}` for `{canonical_key}`"
                );
                assert!(
                    features
                        .legacy_feature_usages()
                        .any(|usage| usage.alias == legacy_key && usage.feature == feature),
                    "legacy feature key `{legacy_key}` should retain its deprecation notice"
                );
            }
        }
    }
}

#[test]
fn merge_toml_values_preserves_alias_identity_without_a_canonical_layer() {
    for legacy_key in legacy_feature_keys() {
        let feature = feature_for_key(legacy_key).expect("legacy feature key should resolve");
        let mut merged = TomlValue::Table(toml::map::Map::new());
        merge_toml_values(&mut merged, &feature_config(legacy_key, true));

        let feature_table = merged
            .get("features")
            .and_then(TomlValue::as_table)
            .expect("merged features table");
        assert!(feature_table.contains_key(legacy_key));
        assert!(!feature_table.contains_key(feature.key()));

        let config: ConfigToml = merged
            .try_into()
            .expect("merged feature config should deserialize");
        let features = Features::from_sources(
            FeatureConfigSource {
                features: config.features.as_ref(),
                experimental_use_unified_exec_tool: config.experimental_use_unified_exec_tool,
            },
            FeatureConfigSource::default(),
            FeatureOverrides::default(),
        );
        let usages = features.legacy_feature_usages().collect::<Vec<_>>();

        assert!(features.enabled(feature));
        assert_eq!(usages.len(), 1, "legacy feature key `{legacy_key}`");
        assert_eq!(usages[0].alias, legacy_key);
        assert_eq!(usages[0].feature, feature);
    }
}

#[test]
fn merge_toml_values_normalizes_permission_network_domains_before_overlaying() {
    let mut base = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "deny"
"#,
    );
    let overlay = parse_toml(
        r#"
[permissions.dev.network.domains]
"EXAMPLE.COM" = "allow"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "allow"
"#,
    );
    assert_eq!(base, expected);
}
