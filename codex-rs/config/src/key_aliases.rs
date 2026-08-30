use toml::Value as TomlValue;
use toml::map::Map as TomlMap;

#[derive(Debug, Clone, Copy)]
struct ConfigKeyAlias {
    table_path: &'static [&'static str],
    legacy_key: &'static str,
    canonical_key: &'static str,
}

const CONFIG_KEY_ALIASES: &[ConfigKeyAlias] = &[ConfigKeyAlias {
    table_path: &["memories"],
    legacy_key: "no_memories_if_mcp_or_web_search",
    canonical_key: "disable_on_external_context",
}];

pub(crate) fn normalize_key_aliases(path: &[String], table: &mut TomlMap<String, TomlValue>) {
    for alias in CONFIG_KEY_ALIASES {
        if path
            .iter()
            .map(String::as_str)
            .eq(alias.table_path.iter().copied())
            && let Some(value) = table.remove(alias.legacy_key)
        {
            table
                .entry(alias.canonical_key.to_string())
                .or_insert(value);
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
