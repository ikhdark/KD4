use crate::merge::is_multi_agent_v2_feature_path;
use crate::merge::merge_owned_toml_values;
use crate::merge::normalize_multi_agent_v2_feature_value;
use toml::Value as TomlValue;

pub(crate) fn default_empty_table() -> TomlValue {
    TomlValue::Table(Default::default())
}

pub fn build_cli_overrides_layer(
    cli_overrides: &[(String, TomlValue)],
) -> std::io::Result<TomlValue> {
    let mut root = default_empty_table();
    for (path, value) in cli_overrides {
        apply_toml_override(&mut root, path, value.clone())?;
    }
    Ok(root)
}

/// Parse a TOML dotted key, including quoted segments, before changing config.
///
/// Unquoted keys that are not valid TOML bare keys (e.g. plugin ids such as
/// `codex-app-tools@openai-bundled`) fall back to the upstream `.` split so
/// callers like Desktop can keep passing them unquoted.
pub fn parse_override_key(path: &str) -> std::io::Result<Vec<String>> {
    toml_edit::Key::parse(path)
        .map(|keys| keys.into_iter().map(|key| key.get().to_owned()).collect())
        .or_else(|err| {
            let segments: Vec<&str> = path.split('.').collect();
            let is_plain = !path.contains(['"', '\''])
                && segments
                    .iter()
                    .all(|segment| !segment.is_empty() && !segment.contains(char::is_whitespace));
            if is_plain {
                Ok(segments.into_iter().map(str::to_owned).collect())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("invalid configuration override key `{path}`: {err}"),
                ))
            }
        })
}

/// Apply a single dotted-path override onto a TOML value.
fn apply_toml_override(root: &mut TomlValue, path: &str, value: TomlValue) -> std::io::Result<()> {
    use toml::value::Table;

    let segments = parse_override_key(path)?;
    let mut value = value;
    let mut current = root;

    for (index, segment) in segments.iter().enumerate() {
        normalize_multi_agent_v2_feature_value(&segments[..index], current);
        let is_last = index + 1 == segments.len();

        if is_last {
            match current {
                TomlValue::Table(table) => {
                    if matches!(segments.as_slice(), [features] if features == "features")
                        && let Some(TomlValue::Table(existing)) = table.get_mut(segment)
                        && let TomlValue::Table(replacement) = &mut value
                        && matches!(
                            (existing.get("multi_agent_v2"), replacement.get("multi_agent_v2")),
                            (Some(TomlValue::Table(_)), Some(TomlValue::Boolean(_)))
                                | (Some(TomlValue::Boolean(_)), Some(TomlValue::Table(_)))
                        )
                        && let Some(mut feature) = existing.remove("multi_agent_v2")
                        && let Some(mut overlay) = replacement.remove("multi_agent_v2")
                    {
                        // Replacing the container still replaces unrelated feature keys.
                        let path = ["features", "multi_agent_v2"];
                        normalize_multi_agent_v2_feature_value(&path, &mut feature);
                        normalize_multi_agent_v2_feature_value(&path, &mut overlay);
                        merge_owned_toml_values(&mut feature, overlay);
                        replacement.insert("multi_agent_v2".to_string(), feature);
                    }
                    if is_multi_agent_v2_feature_path(&segments)
                        && let Some(existing) = table.get_mut(segment)
                        && matches!(
                            (&*existing, &value),
                            (TomlValue::Table(_), TomlValue::Boolean(_))
                                | (TomlValue::Boolean(_), TomlValue::Table(_))
                                | (TomlValue::Table(_), TomlValue::Table(_))
                        )
                    {
                        normalize_multi_agent_v2_feature_value(&segments, existing);
                        normalize_multi_agent_v2_feature_value(&segments, &mut value);
                        merge_owned_toml_values(existing, value);
                        return Ok(());
                    }
                    table.insert(segment.clone(), value);
                }
                _ => {
                    let mut table = Table::new();
                    table.insert(segment.clone(), value);
                    *current = TomlValue::Table(table);
                }
            }
            return Ok(());
        }

        match current {
            TomlValue::Table(table) => {
                current = table
                    .entry(segment.clone())
                    .or_insert_with(|| TomlValue::Table(Table::new()));
            }
            _ => {
                *current = TomlValue::Table(Table::new());
                if let TomlValue::Table(tbl) = current {
                    current = tbl
                        .entry(segment.clone())
                        .or_insert_with(|| TomlValue::Table(Table::new()));
                }
            }
        }
    }
    Ok(())
}
