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
pub fn parse_override_key(path: &str) -> std::io::Result<Vec<String>> {
    toml_edit::Key::parse(path)
        .map(|keys| keys.into_iter().map(|key| key.get().to_owned()).collect())
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid configuration override key `{path}`: {err}"),
            )
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
                    table.insert(segment.to_string(), value);
                }
                _ => {
                    let mut table = Table::new();
                    table.insert(segment.to_string(), value);
                    *current = TomlValue::Table(table);
                }
            }
            return Ok(());
        }

        match current {
            TomlValue::Table(table) => {
                current = table
                    .entry(segment.to_string())
                    .or_insert_with(|| TomlValue::Table(Table::new()));
            }
            _ => {
                *current = TomlValue::Table(Table::new());
                if let TomlValue::Table(tbl) = current {
                    current = tbl
                        .entry(segment.to_string())
                        .or_insert_with(|| TomlValue::Table(Table::new()));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_segments_preserve_dots_and_override_order() {
        let root = build_cli_overrides_layer(&[
            (
                r#"mcp_servers."docs.v1".enabled"#.into(),
                TomlValue::Boolean(true),
            ),
            (
                r#"mcp_servers.'docs.v1'.enabled"#.into(),
                TomlValue::Boolean(false),
            ),
        ])
        .unwrap();
        assert_eq!(
            root["mcp_servers"]["docs.v1"]["enabled"],
            TomlValue::Boolean(false)
        );
        assert!(root["mcp_servers"].get("docs").is_none());
    }

    #[test]
    fn malformed_paths_fail_before_mutation() {
        for path in ["", "a..b", "a.", "\"unterminated", "a = 2\nb"] {
            let mut root = default_empty_table();
            assert!(
                apply_toml_override(&mut root, path, TomlValue::Boolean(true)).is_err(),
                "{path}"
            );
            assert_eq!(root, default_empty_table());
        }
    }

    #[test]
    fn later_child_override_replaces_scalar_parent() {
        let root = build_cli_overrides_layer(&[
            ("a".into(), TomlValue::Integer(1)),
            ("a.b".into(), TomlValue::Integer(2)),
        ])
        .unwrap();
        assert_eq!(root["a"]["b"], TomlValue::Integer(2));
    }
}
