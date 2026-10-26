use toml::Value as TomlValue;

/// Merge config `overlay` into `base`, giving `overlay` precedence.
pub fn merge_toml_values(base: &mut TomlValue, overlay: &TomlValue) {
    merge_owned_config_values(base, overlay.clone());
}

/// Merge an owned complete config layer without cloning its nested values.
pub fn merge_owned_config_values(base: &mut TomlValue, mut overlay: TomlValue) {
    if let (Some(base_feature), Some(overlay_feature)) = (
        base.get_mut("features")
            .and_then(|features| features.get_mut("multi_agent_v2")),
        overlay
            .get_mut("features")
            .and_then(|features| features.get_mut("multi_agent_v2")),
    ) {
        let path = ["features", "multi_agent_v2"];
        match (&*base_feature, &*overlay_feature) {
            (TomlValue::Table(_), TomlValue::Boolean(_)) => {
                normalize_multi_agent_v2_feature_value(&path, overlay_feature);
            }
            (TomlValue::Boolean(_), TomlValue::Table(_)) => {
                normalize_multi_agent_v2_feature_value(&path, base_feature);
            }
            _ => {}
        }
    }
    merge_owned_toml_values(base, overlay);
}

/// Structural merge for owned values, including arbitrary configuration subtrees.
///
/// Moving overlay values avoids re-copying every nested subtree. Complete config
/// layers use [`merge_toml_values`] to preserve mixed feature representations.
pub fn merge_owned_toml_values(base: &mut TomlValue, overlay: TomlValue) {
    let (base_table, overlay_table) = match (base, overlay) {
        (TomlValue::Table(base_table), TomlValue::Table(overlay_table)) => {
            (base_table, overlay_table)
        }
        (base, overlay) => {
            *base = overlay;
            return;
        }
    };


    for (key, value) in overlay_table {
        if let Some(existing) = base_table.get_mut(&key) {
            merge_owned_toml_values(existing, value);
        } else {
            base_table.insert(key, value);
        }
    }
}

/// The supported structured feature path in both base and profile-v2 files.
pub fn is_multi_agent_v2_feature_path<S: AsRef<str>>(path: &[S]) -> bool {
    match path {
        [features, feature] => {
            features.as_ref() == "features" && feature.as_ref() == "multi_agent_v2"
        }
        _ => false,
    }
}

/// Preserve a boolean feature toggle before writing or merging nested settings.
pub fn normalize_multi_agent_v2_feature_value<S: AsRef<str>>(
    path: &[S],
    value: &mut TomlValue,
) {
    if is_multi_agent_v2_feature_path(path)
        && let TomlValue::Boolean(enabled) = value
    {
        *value = TomlValue::Table(toml::map::Map::from_iter([(
            "enabled".to_string(),
            TomlValue::Boolean(*enabled),
        )]));
    }
}
