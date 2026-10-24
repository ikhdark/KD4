use super::*;
use crate::CONFIG_TOML_FILE;
use crate::ConfigLayerStack;
use crate::ConfigLayerStackOrdering;
use crate::ConfigRequirements;
use crate::ConfigRequirementsToml;
use crate::config_toml::ConfigToml;
use crate::first_layer_config_error_from_entries;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;
use std::path::Path;

fn fragment(id: &str, name: &str, contents: &str) -> CloudConfigFragment {
    CloudConfigFragment {
        id: id.to_string(),
        name: name.to_string(),
        contents: contents.to_string(),
    }
}

fn toml(contents: &str) -> TomlValue {
    toml::from_str(contents).expect("test TOML should parse")
}

fn base_dir() -> AbsolutePathBuf {
    test_path_buf("/var/lib/codex").abs()
}

#[test]
fn enterprise_layers_precede_user_and_override_system() {
    let base_dir = base_dir();
    let mut layers = vec![ConfigLayerEntry::new(
        ConfigLayerSource::System {
            file: test_path_buf("/etc/codex/config.toml").abs(),
        },
        toml(
            r#"
model = "system"
model_provider = "system"
review_model = "system-review"
"#,
        ),
    )];
    layers.extend(
        cloud_config_layers_from_fragments(
            vec![
                fragment("high", "High priority", "model_provider = \"cloud-high\""),
                fragment("low", "Low priority", "review_model = \"cloud-low-review\""),
            ],
            &base_dir,
        )
        .expect("cloud config layers should compose"),
    );
    layers.push(ConfigLayerEntry::new(
        ConfigLayerSource::User {
            file: test_path_buf("/home/alice/.codex/config.toml").abs(),
            profile: None,
        },
        toml("model = \"user\""),
    ));

    let stack = ConfigLayerStack::new(
        layers,
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("stack should be ordered");

    assert_eq!(
        stack
            .get_layers(
                ConfigLayerStackOrdering::LowestPrecedenceFirst,
                /*include_disabled*/ false
            )
            .iter()
            .map(|layer| layer.name.clone())
            .collect::<Vec<_>>(),
        vec![
            ConfigLayerSource::System {
                file: test_path_buf("/etc/codex/config.toml").abs(),
            },
            ConfigLayerSource::EnterpriseManaged {
                id: "low".to_string(),
                name: "Low priority".to_string(),
            },
            ConfigLayerSource::EnterpriseManaged {
                id: "high".to_string(),
                name: "High priority".to_string(),
            },
            ConfigLayerSource::User {
                file: test_path_buf("/home/alice/.codex/config.toml").abs(),
                profile: None,
            },
        ]
    );
    assert_eq!(
        stack.effective_config(),
        toml(
            r#"
model = "user"
model_provider = "cloud-high"
review_model = "cloud-low-review"
config_version = 1
"#,
        )
    );
}

#[test]
fn cloud_fragments_apply_local_migrations_and_reject_future_versions() {
    let raw = r#"
experimental_use_unified_exec_tool = false
model_supports_reasoning_summaries = true
[features]
experimental_windows_sandbox = true
search_tool = true
"#;
    for strict in [false, true] {
        let layers = cloud_config_layers_from_fragments_impl(
            [fragment("legacy", "Legacy cloud", raw)],
            &base_dir(),
            strict,
        )
        .expect("cloud migration precedes validation");
        let config = &layers[0].config;
        assert_eq!(config["config_version"].as_integer(), Some(1));
        assert_eq!(config["features"]["unified_exec"].as_bool(), Some(false));
        assert_eq!(config["windows"]["sandbox"].as_str(), Some("unelevated"));
        assert!(config.get("experimental_use_unified_exec_tool").is_none());
        assert!(config.get("model_supports_reasoning_summaries").is_none());
        assert!(
            config["features"]
                .get("experimental_windows_sandbox")
                .is_none()
        );
        assert!(config["features"].get("search_tool").is_none());
        assert_eq!(layers[0].raw_toml(), Some(raw));
        let err = cloud_config_layers_from_fragments_impl(
            [fragment("future", "Future cloud", "config_version = 999")],
            &base_dir(),
            strict,
        )
        .expect_err("unsupported versions must not depend on delivery source");
        assert!(
            matches!(err, CloudConfigLayerError::Invalid { fragment, message }
            if fragment.id == "future" && message.contains("unsupported config_version 999")
                && message.contains("Future cloud (future)"))
        );
    }
}

#[test]
fn relative_absolute_path_fields_resolve_against_base_dir() {
    let base_dir = base_dir();
    let layers = cloud_config_layers_from_fragments(
        vec![fragment(
            "cfg_123",
            "Base policy",
            "model_instructions_file = \"instructions.md\"",
        )],
        &base_dir,
    )
    .expect("relative paths should match existing MDM semantics");

    let path = layers[0]
        .config
        .get("model_instructions_file")
        .and_then(TomlValue::as_str)
        .expect("path should be present");
    let expected =
        AbsolutePathBuf::resolve_path_against_base("instructions.md", base_dir.as_path());
    assert_eq!(path, expected.to_string_lossy());
}

#[test]
fn home_relative_path_fields_are_allowed_and_resolved() {
    let base_dir = base_dir();
    let layers = cloud_config_layers_from_fragments(
        vec![fragment(
            "cfg_123",
            "Base policy",
            "model_instructions_file = \"~/instructions.md\"",
        )],
        &base_dir,
    )
    .expect("home-relative paths should be accepted");

    let path = layers[0]
        .config
        .get("model_instructions_file")
        .and_then(TomlValue::as_str)
        .expect("path should be present");
    let expected =
        AbsolutePathBuf::resolve_path_against_base("~/instructions.md", base_dir.as_path());
    assert_eq!(path, expected.to_string_lossy());
}

#[tokio::test]
async fn raw_toml_diagnostics_use_enterprise_layer_name() {
    let base_dir = base_dir();
    let layers = cloud_config_layers_from_fragments(
        vec![fragment(
            "cfg_123",
            "Base policy",
            "model_instructions_file = \"instructions.md\"\nmodel = 1",
        )],
        &base_dir,
    )
    .expect("cloud config layers should parse");

    let error = first_layer_config_error_from_entries::<ConfigToml>(&layers, CONFIG_TOML_FILE)
        .await
        .expect("invalid raw TOML should produce a layer diagnostic");

    assert_eq!(
        error.path,
        Path::new("enterprise-managed (Base policy, cfg_123)").to_path_buf()
    );
    assert_eq!(error.range.start.line, 2);
    assert_eq!(error.range.start.column, 9);
    assert!(error.message.contains("invalid type: integer `1`"));
}
