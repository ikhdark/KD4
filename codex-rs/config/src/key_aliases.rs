use codex_network_proxy::normalize_host;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;

pub(crate) fn normalize_key_aliases(path: &[String], table: &mut TomlMap<String, TomlValue>) {
    if matches!(
        path,
        [permissions, _, network, domains]
            if permissions == "permissions" && network == "network" && domains == "domains"
    ) {
        // Preserve the layer's last-declaration-wins order for equivalent hosts.
        let entries = std::mem::take(table);
        for (pattern, value) in entries {
            table.insert(normalize_host(&pattern), value);
        }
    }
}

pub(crate) fn normalized_with_key_aliases(value: &TomlValue, path: &[String]) -> TomlValue {
    normalize_owned_key_aliases(value.clone(), &mut path.to_vec())
}

/// Owned counterpart to [`normalized_with_key_aliases`].
///
/// Normalization rebuilds every table anyway, so a caller that already owns
/// `value` can hand it over instead of paying for a second deep copy. `path`
/// is used as a scratch buffer and is restored before returning.
pub(crate) fn normalize_owned_key_aliases(value: TomlValue, path: &mut Vec<String>) -> TomlValue {
    match value {
        TomlValue::Table(table) => {
            let mut normalized = TomlMap::new();
            for (key, child) in table {
                path.push(key.clone());
                let child = normalize_owned_key_aliases(child, path);
                path.pop();
                normalized.insert(key, child);
            }
            normalize_key_aliases(path, &mut normalized);
            TomlValue::Table(normalized)
        }
        TomlValue::Array(items) => TomlValue::Array(
            items
                .into_iter()
                .map(|item| normalize_owned_key_aliases(item, path))
                .collect(),
        ),
        other => other,
    }
}
