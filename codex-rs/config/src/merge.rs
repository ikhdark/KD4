use crate::key_aliases::normalize_key_aliases;
use crate::key_aliases::normalized_with_key_aliases;
use codex_features::feature_for_key;
use codex_features::legacy_feature_keys;
use codex_network_proxy::normalize_host;
use toml::Value as TomlValue;

/// Merge config `overlay` into `base`, giving `overlay` precedence.
pub fn merge_toml_values(base: &mut TomlValue, overlay: &TomlValue) {
    merge_toml_values_at_path(base, overlay, &mut Vec::new());
}

fn merge_toml_values_at_path(base: &mut TomlValue, overlay: &TomlValue, path: &mut Vec<String>) {
    if let TomlValue::Table(overlay_table) = overlay
        && let TomlValue::Table(base_table) = base
    {
        normalize_key_aliases(path, base_table);
        let mut overlay_table = overlay_table.clone();
        normalize_key_aliases(path, &mut overlay_table);
        apply_feature_alias_overlay_precedence(path, base_table, &overlay_table);
        if is_permission_network_domains_path(path) {
            normalize_network_domain_keys(base_table);
            normalize_network_domain_keys(&mut overlay_table);
        }

        for (key, value) in overlay_table {
            path.push(key.clone());
            if let Some(existing) = base_table.get_mut(&key) {
                merge_toml_values_at_path(existing, &value, path);
            } else {
                base_table.insert(key, normalized_with_key_aliases(&value, path));
            }
            path.pop();
        }
    } else {
        *base = normalized_with_key_aliases(overlay, path);
    }
}

fn apply_feature_alias_overlay_precedence(
    path: &[String],
    base: &mut toml::map::Map<String, TomlValue>,
    overlay: &toml::map::Map<String, TomlValue>,
) {
    if !matches!(path, [features] if features == "features") {
        return;
    }

    for legacy_key in legacy_feature_keys() {
        let Some(alias_value) = overlay.get(legacy_key) else {
            continue;
        };
        let Some(feature) = feature_for_key(legacy_key) else {
            continue;
        };
        let canonical_key = feature.key();
        if overlay.contains_key(canonical_key) {
            continue;
        }
        if let Some(canonical_value) = base.get_mut(canonical_key) {
            *canonical_value = alias_value.clone();
        }
    }
}

fn is_permission_network_domains_path(path: &[String]) -> bool {
    matches!(
        path,
        [permissions, _, network, domains]
            if permissions == "permissions" && network == "network" && domains == "domains"
    )
}

fn normalize_network_domain_keys(table: &mut toml::map::Map<String, TomlValue>) {
    let entries = std::mem::take(table);
    for (pattern, value) in entries {
        table.insert(normalize_host(&pattern), value);
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
