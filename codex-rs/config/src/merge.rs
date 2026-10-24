use toml::Value as TomlValue;

/// Merge config `overlay` into `base`, giving `overlay` precedence.
pub fn merge_toml_values(base: &mut TomlValue, overlay: &TomlValue) {
    merge_owned_toml_values(base, overlay.clone());
}

/// Owned counterpart to [`merge_toml_values`].
///
/// Moving overlay values avoids re-copying every nested subtree.
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

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
