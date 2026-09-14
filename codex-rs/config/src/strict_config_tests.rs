use super::*;
use crate::config_toml::ConfigToml;
use crate::diagnostics::TextPosition;
use crate::diagnostics::TextRange;
use pretty_assertions::assert_eq;
use std::path::PathBuf;

#[test]
fn ignored_toml_field_errors_accept_non_file_source_names() {
    let source_name = "com.openai.codex:config_toml_base64";
    let contents = r#"
model = "gpt-5"
unknown_key = true"#;

    let value = toml::from_str::<TomlValue>(contents).expect("valid TOML");
    let error = config_error_from_ignored_toml_value_fields_for_source_name::<ConfigToml>(
        source_name,
        contents,
        value,
    )
    .expect("unknown field error");

    assert_eq!(
        error,
        ConfigError::new(
            PathBuf::from(source_name),
            TextRange {
                start: TextPosition { line: 3, column: 1 },
                end: TextPosition {
                    line: 3,
                    column: 11,
                },
            },
            "unknown configuration field `unknown_key`",
        )
    );
}

#[test]
fn type_errors_take_precedence_over_ignored_fields() {
    let path = Path::new("/tmp/config.toml");
    let contents = r#"
model_context_window = "wide"
unknown_key = true"#;

    let error =
        config_error_from_ignored_toml_fields::<ConfigToml>(path, contents).expect("type error");

    assert_eq!(
        error,
        ConfigError::new(
            path.to_path_buf(),
            TextRange {
                start: TextPosition {
                    line: 2,
                    column: 24,
                },
                end: TextPosition {
                    line: 2,
                    column: 29,
                },
            },
            "invalid type: string \"wide\", expected i64",
        )
    );
}

#[test]
fn strict_config_rejects_unknown_feature_key() {
    let path = Path::new("/tmp/config.toml");
    let contents = r#"
[features]
foo = true"#;

    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, contents)
        .expect("unknown feature error");

    assert_eq!(
        error,
        ConfigError::new(
            path.to_path_buf(),
            TextRange {
                start: TextPosition { line: 3, column: 1 },
                end: TextPosition { line: 3, column: 3 },
            },
            "unknown configuration field `features.foo`",
        )
    );
}

#[test]
fn strict_config_rejects_removed_inline_profiles() {
    let path = Path::new("/tmp/config.toml");
    let contents = r#"
[profiles.work.features]
foo = true"#;

    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, contents)
        .expect("removed inline profiles error");

    assert_eq!(
        error,
        ConfigError::new(
            path.to_path_buf(),
            TextRange {
                start: TextPosition { line: 2, column: 2 },
                end: TextPosition { line: 2, column: 9 },
            },
            "unknown configuration field `profiles`",
        )
    );
}

#[test]
fn strict_config_rejects_removed_code_mode_waiting_policy() {
    let path = Path::new("/tmp/config.toml");
    let contents = r#"
[features.code_mode]
enabled = true
waiting_policy = "yield_after"
"#;

    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, contents)
        .expect("removed waiting_policy should be rejected");

    assert_eq!(error.path, path);
    // FeatureToml's untagged enum reports a shape error at the outer boundary.
    // Check the inner diagnostic too, so another failure cannot satisfy this
    // regression merely by rejecting the whole feature block.
    assert!(error.message.contains("FeatureToml"), "{error:?}");
    let feature_error = toml::from_str::<codex_features::CodeModeConfigToml>(
        "enabled = true\nwaiting_policy = 'yield_after'\n",
    )
    .expect_err("removed field must fail in the structured feature config");
    assert!(feature_error.to_string().contains("waiting_policy"));
    assert_eq!(
        config_error_from_ignored_toml_fields::<ConfigToml>(
            path,
            "[features.code_mode]\nenabled = true\n"
        ),
        None
    );
}

#[test]
fn strict_permission_profile_reports_unknown_fixed_field() {
    let path = Path::new("/tmp/config.toml");
    let valid = "[permissions.dev.network]\nenabled = true\n";
    assert_eq!(
        config_error_from_ignored_toml_fields::<ConfigToml>(path, valid),
        None
    );
    for (invalid, unknown_path) in [
        (
            format!("{valid}unknown_network_setting = true\n"),
            "permissions.dev.network.unknown_network_setting",
        ),
        (
            "[permissions.dev]\nunknown_profile_setting = true\n".to_string(),
            "permissions.dev.unknown_profile_setting",
        ),
    ] {
        let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, &invalid)
            .expect("unknown permission setting");
        assert_eq!(
            error.message,
            format!("unknown configuration field `{unknown_path}`")
        );
        // Ordinary parsing remains permissive; strict validation owns diagnostics.
        let _: ConfigToml = toml::from_str(&invalid).expect("ordinary config parsing");
    }
}

#[test]
fn strict_config_accepts_opaque_desktop_keys() {
    let path = Path::new("/tmp/config.toml");
    let contents = r#"
[desktop]
appearanceTheme = "dark"

[desktop.workspace]
collapsed = true"#;

    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, contents);

    assert_eq!(error, None);
}

#[test]
fn strict_config_keeps_first_unknown_but_finishes_type_validation() {
    let path = Path::new("/tmp/config.toml");
    let contents = "first_unknown = true\nsecond_unknown = false\n";
    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, contents).unwrap();
    assert_eq!(error.message, "unknown configuration field `first_unknown`");
    let value = toml::from_str(contents).unwrap();
    assert_eq!(
        ignored_toml_value_field::<ConfigToml>(value),
        Some("first_unknown".to_string())
    );
    let invalid = format!("{contents}model_context_window = 'wide'\n");
    let error = config_error_from_ignored_toml_fields::<ConfigToml>(path, &invalid).unwrap();
    assert_eq!(error.message, "invalid type: string \"wide\", expected i64");
    assert_eq!(
        ignored_toml_value_field::<ConfigToml>(toml::from_str(&invalid).unwrap()),
        None
    );
}
