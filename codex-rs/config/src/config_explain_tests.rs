use super::CONFIG_OPTION_DOCS;
use super::render_config_explain;
use crate::schema::config_schema;
use std::collections::BTreeSet;

#[test]
fn renders_plain_english_config_reference() {
    let rendered = render_config_explain(None);

    assert!(rendered.contains("Codex config options"));
    assert!(rendered.contains("Model and provider"));
    assert!(rendered.contains("- model: Default model used for new turns."));
    assert!(rendered.contains("Approvals and sandbox"));
    assert!(rendered.contains("- sandbox_mode: Command sandbox level"));
}

#[test]
fn covers_schema_backed_runtime_options() {
    let rendered = render_config_explain(None);

    assert!(rendered.contains("- tool_output_token_limit:"));
    assert!(rendered.contains("- background_terminal_max_timeout:"));
    assert!(rendered.contains("- hooks:"));
    assert!(rendered.contains("- auto_review:"));
    assert!(rendered.contains("- debug:"));
    assert!(rendered.contains("- tools:"));
    assert!(rendered.contains("- reasoning_phase_efforts:"));
    assert!(rendered.contains("- reasoning_phase_efforts.deterministic_continuation:"));
}

#[test]
fn every_public_schema_option_has_exactly_one_explain_entry() {
    let schema = serde_json::to_value(config_schema()).expect("schema should serialize");
    let schema_options = schema["properties"]
        .as_object()
        .expect("config schema properties")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let documented_roots = CONFIG_OPTION_DOCS
        .iter()
        .map(|doc| doc.name.split('.').next().expect("non-empty option name"))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

    assert_eq!(documented_roots, schema_options);

    let unique_names = CONFIG_OPTION_DOCS
        .iter()
        .map(|doc| doc.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(unique_names.len(), CONFIG_OPTION_DOCS.len());
}

#[test]
fn filters_config_reference_by_option_name() {
    let rendered = render_config_explain(Some("sandbox"));

    assert!(rendered.contains("sandbox_mode"));
    assert!(rendered.contains("sandbox_workspace_write"));
    assert!(!rendered.contains("- model: Default model used for new turns."));
}

#[test]
fn explains_deterministic_continuation_precedence() {
    let rendered = render_config_explain(Some("deterministic_continuation"));

    assert!(rendered.contains("- reasoning_phase_efforts.deterministic_continuation:"));
    assert!(rendered.contains("proven non-decision-bearing residual request"));
    assert!(rendered.contains("takes precedence over the broader phase effort"));
    assert!(rendered.contains("defaults to `low` or the model's lowest supported equivalent"));
}

#[test]
fn explains_empty_filter_result() {
    let rendered = render_config_explain(Some("definitely-not-a-config-option"));

    assert_eq!(
        rendered,
        "No config options matched `definitely-not-a-config-option`.\nTry `codex config explain` to list all known options."
    );
}
