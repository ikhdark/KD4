use super::*;
use crate::config_toml::ConfigToml;
use crate::config_toml::ReasoningPhaseEfforts;
use crate::types::MemoriesToml;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

fn parse_toml(value: &str) -> TomlValue {
    toml::from_str(value).expect("TOML should parse")
}

#[test]
fn empty_higher_reasoning_phase_table_preserves_lower_entries() {
    let mut base = parse_toml(
        r#"
[reasoning_phase_efforts]
orient = "medium"
inspect = "low"
"#,
    );
    let overlay = parse_toml("[reasoning_phase_efforts]");

    merge_toml_values(&mut base, &overlay);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.reasoning_phase_efforts,
        Some(ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::Medium),
            inspect: Some(ReasoningEffort::Low),
            implement: None,
            diagnose: None,
            verify: None,
            finalize: None,
            deterministic_continuation: None,
        })
    );
}

#[test]
fn partial_higher_reasoning_phase_table_merges_per_field() {
    let mut base = parse_toml(
        r#"
[reasoning_phase_efforts]
orient = "medium"
inspect = "low"
deterministic_continuation = "medium"
"#,
    );
    let overlay = parse_toml(
        r#"
[reasoning_phase_efforts]
inspect = "high"
verify = "medium"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.reasoning_phase_efforts,
        Some(ReasoningPhaseEfforts {
            orient: Some(ReasoningEffort::Medium),
            inspect: Some(ReasoningEffort::High),
            implement: None,
            diagnose: None,
            verify: Some(ReasoningEffort::Medium),
            finalize: None,
            deterministic_continuation: Some(ReasoningEffort::Medium),
        })
    );
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_base_layer() {
    let mut base = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn merge_toml_values_normalizes_legacy_key_from_overlay_layer() {
    let mut base = parse_toml(
        r#"
[memories]
disable_on_external_context = false
"#,
    );
    let overlay = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = true
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);

    let config: ConfigToml = base.try_into().expect("merged config should deserialize");
    assert_eq!(
        config.memories,
        Some(MemoriesToml {
            disable_on_external_context: Some(true),
            ..Default::default()
        })
    );
}

#[test]
fn merge_toml_values_prefers_canonical_key_when_one_layer_has_both_names() {
    let mut base = TomlValue::Table(toml::map::Map::new());
    let overlay = parse_toml(
        r#"
[memories]
disable_on_external_context = true
no_memories_if_mcp_or_web_search = false
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[memories]
disable_on_external_context = true
"#,
    );
    assert_eq!(base, expected);
}

#[test]
fn merge_toml_values_normalizes_permission_network_domains_before_overlaying() {
    let mut base = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "deny"
"#,
    );
    let overlay = parse_toml(
        r#"
[permissions.dev.network.domains]
"EXAMPLE.COM" = "allow"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    let expected = parse_toml(
        r#"
[permissions.dev.network.domains]
"example.com" = "allow"
"#,
    );
    assert_eq!(base, expected);
}

#[test]
fn merge_toml_values_normalizes_nested_overlay_subtrees_absent_from_base() {
    // The overlay-only branch normalizes an owned subtree instead of a borrowed
    // one. Alias rewriting stays anchored to its configured table path, and
    // nested tables and arrays of tables must survive that handoff intact.
    let mut base = parse_toml(
        r#"
[memories]
enabled = true
"#,
    );
    let overlay = parse_toml(
        r#"
[memories.nested]
no_memories_if_mcp_or_web_search = true

[projects.demo]
no_memories_if_mcp_or_web_search = true

[[projects.demo.hooks]]
name = "first"

[[projects.demo.hooks]]
name = "second"
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(base["memories"]["enabled"], TomlValue::Boolean(true));
    assert_eq!(
        base["memories"]["nested"]["no_memories_if_mcp_or_web_search"],
        TomlValue::Boolean(true),
        "aliases apply only at their configured table path"
    );
    assert_eq!(
        base["projects"]["demo"]["no_memories_if_mcp_or_web_search"],
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
fn merge_toml_values_replaces_scalar_base_with_normalized_overlay_table() {
    // A non-table base takes the whole overlay through the same owned
    // normalization path that nested tables use.
    let mut base = parse_toml(r#"memories = "unset""#);
    let overlay = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = false
"#,
    );

    merge_toml_values(&mut base, &overlay);

    assert_eq!(
        base["memories"]["disable_on_external_context"],
        TomlValue::Boolean(false)
    );
}

#[test]
fn merge_owned_toml_values_matches_the_borrowed_entry_point() {
    let overlay = parse_toml(
        r#"
[memories]
no_memories_if_mcp_or_web_search = true

[reasoning_phase_efforts]
orient = "high"
"#,
    );
    let mut borrowed_base = parse_toml(
        r#"[reasoning_phase_efforts]
inspect = "low"
"#,
    );
    let mut owned_base = borrowed_base.clone();

    merge_toml_values(&mut borrowed_base, &overlay);
    merge_owned_toml_values(&mut owned_base, overlay);

    assert_eq!(borrowed_base, owned_base);
}
