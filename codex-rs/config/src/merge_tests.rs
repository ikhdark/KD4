use super::*;
use pretty_assertions::assert_eq;

fn parse_toml(value: &str) -> TomlValue {
    toml::from_str(value).expect("TOML should parse")
}


#[test]
fn merge_toml_values_preserves_nested_overlay_subtrees_absent_from_base() {
    // The overlay-only branch inserts an owned subtree. Unrelated keys must remain unchanged, and
    // nested tables and arrays of tables must survive that handoff intact.
    let mut base = parse_toml(
        r#"
[example]
enabled = true
"#,
    );
    let overlay = parse_toml(
        r#"
[example.nested]
enabled = true

[projects.demo]
enabled = true

[[projects.demo.hooks]]
name = "first"

[[projects.demo.hooks]]
name = "second"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(base["example"]["enabled"], TomlValue::Boolean(true));
    assert_eq!(
        base["example"]["nested"]["enabled"],
        TomlValue::Boolean(true),
        "nested keys must survive the merge"
    );
    assert_eq!(
        base["projects"]["demo"]["enabled"],
        TomlValue::Boolean(true)
    );
    let hooks = base["projects"]["demo"]["hooks"]
        .as_array()
        .expect("hooks should remain an array");
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0]["name"], TomlValue::String("first".to_string()));
    assert_eq!(hooks[1]["name"], TomlValue::String("second".to_string()));
}

#[test]
fn merge_toml_values_replaces_scalar_base_with_overlay_table() {
    // A non-table base takes the whole overlay, as nested table replacements do.
    let mut base = parse_toml(r#"example = "unset""#);
    let overlay = parse_toml(
        r#"
[example]
enabled = false
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(base["example"]["enabled"], TomlValue::Boolean(false));
}

#[test]
fn merge_owned_toml_values_matches_the_borrowed_entry_point() {
    let overlay = parse_toml(
        r#"
[example]
enabled = true

[example_settings]
first = "high"
"#,
    );
    let mut borrowed_base = parse_toml(
        r#"[example_settings]
second = "low"
"#,
    );
    let mut owned_base = borrowed_base.clone();

    merge_toml_values(&mut borrowed_base, &overlay);
    merge_owned_toml_values(&mut owned_base, overlay);

    assert_eq!(borrowed_base, owned_base);
    assert_eq!(
        owned_base,
        parse_toml(
            r#"
[example]
enabled = true

[example_settings]
second = "low"
first = "high"
"#,
        )
    );
}
