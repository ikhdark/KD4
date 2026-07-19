use pretty_assertions::assert_eq;

use super::PluginId;

#[test]
fn constructors_preserve_validated_segments() {
    let from_parts = PluginId::new("sample-plugin".to_string(), "test-market".to_string())
        .expect("valid plugin id");
    let parsed = PluginId::parse("sample-plugin@test-market").expect("valid plugin id");

    assert_eq!(from_parts, parsed);
    assert_eq!(parsed.plugin_name(), "sample-plugin");
    assert_eq!(parsed.marketplace_name(), "test-market");
    assert_eq!(parsed.as_key(), "sample-plugin@test-market");
}

#[test]
fn constructors_reject_path_traversal_segments() {
    assert_eq!(
        PluginId::new("..".to_string(), "test".to_string())
            .expect_err("plugin segment should be rejected")
            .to_string(),
        "invalid plugin name: only ASCII letters, digits, `_`, and `-` are allowed"
    );
    assert_eq!(
        PluginId::parse("sample@../test")
            .expect_err("marketplace segment should be rejected")
            .to_string(),
        "invalid marketplace name: only ASCII letters, digits, `_`, and `-` are allowed in `sample@../test`"
    );
}
