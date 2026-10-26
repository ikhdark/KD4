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
    let mut current = root;
    let mut segments_iter = segments.into_iter().peekable();

    while let Some(segment) = segments_iter.next() {
        let is_last = segments_iter.peek().is_none();

        if is_last {
            match current {
                TomlValue::Table(table) => {
                    table.insert(segment, value);
                }
                _ => {
                    let mut table = Table::new();
                    table.insert(segment, value);
                    *current = TomlValue::Table(table);
                }
            }
            return Ok(());
        }

        match current {
            TomlValue::Table(table) => {
                current = table
                    .entry(segment)
                    .or_insert_with(|| TomlValue::Table(Table::new()));
            }
            _ => {
                *current = TomlValue::Table(Table::new());
                if let TomlValue::Table(tbl) = current {
                    current = tbl
                        .entry(segment)
                        .or_insert_with(|| TomlValue::Table(Table::new()));
                }
            }
        }
    }
    Ok(())
}
