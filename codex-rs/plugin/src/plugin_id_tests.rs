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

#[test]
fn constructors_enforce_the_identifier_grammar() {
    for segment in [
        "", ".", "..", "a/b", "a\\b", "a b", "a\tb", "a\nb", "a@b", "café",
    ] {
        assert!(PluginId::new(segment.to_string(), "market".to_string()).is_err());
        assert!(PluginId::new("plugin".to_string(), segment.to_string()).is_err());
        assert!(PluginId::parse(&format!("{segment}@market")).is_err());
        assert!(PluginId::parse(&format!("plugin@{segment}")).is_err());
    }
    for key in ["", "plugin", "@", "plugin@", "@market", "plugin@@market"] {
        assert!(
            PluginId::parse(key).is_err(),
            "accepted malformed key {key:?}"
        );
    }

    let key = "Plugin_42-name@Market_7-place";
    let parsed = PluginId::parse(key).expect("all allowed character classes");
    assert_eq!(parsed.plugin_name(), "Plugin_42-name");
    assert_eq!(parsed.marketplace_name(), "Market_7-place");
    assert_eq!(parsed.as_key(), key);
    assert_eq!(
        parsed,
        PluginId::new("Plugin_42-name".to_string(), "Market_7-place".to_string())
            .expect("valid segments")
    );
}
