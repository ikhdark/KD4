use crate::key_aliases::normalize_key_aliases;
use crate::key_aliases::normalize_owned_key_aliases;
use toml::Value as TomlValue;

/// Merge config `overlay` into `base`, giving `overlay` precedence.
pub fn merge_toml_values(base: &mut TomlValue, overlay: &TomlValue) {
    merge_owned_toml_values(base, overlay.clone());
}

/// Owned counterpart to [`merge_toml_values`].
///
/// The merge has to normalize key aliases on a mutable copy of each overlay
/// table, so taking `overlay` by value keeps that to a single copy of the
/// tree. Borrowing it instead re-copies every subtree once per level of
/// nesting above it.
pub fn merge_owned_toml_values(base: &mut TomlValue, overlay: TomlValue) {
    merge_toml_values_at_path(base, overlay, &mut Vec::new());
}

fn merge_toml_values_at_path(base: &mut TomlValue, overlay: TomlValue, path: &mut Vec<String>) {
    let (base_table, mut overlay_table) = match (base, overlay) {
        (TomlValue::Table(base_table), TomlValue::Table(overlay_table)) => {
            (base_table, overlay_table)
        }
        (base, overlay) => {
            *base = normalize_owned_key_aliases(overlay, path);
            return;
        }
    };

    normalize_key_aliases(path, base_table);
    normalize_key_aliases(path, &mut overlay_table);

    for (key, value) in overlay_table {
        path.push(key.clone());
        if let Some(existing) = base_table.get_mut(&key) {
            merge_toml_values_at_path(existing, value, path);
        } else {
            base_table.insert(key, normalize_owned_key_aliases(value, path));
        }
        path.pop();
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
