use super::*;
use codex_config::types::AppToolApproval;
use codex_config::types::McpServerOAuthConfig;
use codex_config::types::McpServerToolConfig;
use codex_config::types::McpServerTransportConfig;
use codex_config::types::SessionPickerViewMode;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

use tempfile::tempdir;
use toml::Value as TomlValue;

#[test]
fn expected_version_is_compared_under_the_persistence_lock() {
    let tmp = tempdir().expect("tmpdir");
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, "model = \"initial\"\n").expect("seed config");
    let initial: TomlValue = toml::from_str("model = \"initial\"\n").expect("parse seed");
    let expected_version = version_for_toml(&initial);

    let writers = ["first", "second"].map(|model| {
        let config_path = config_path.clone();
        let expected_version = expected_version.clone();
        std::thread::spawn(move || {
            ConfigEditsBuilder::for_config_path(&config_path)
                .with_edits([ConfigEdit::SetPath {
                    segments: vec!["model".to_string()],
                    value: value(model),
                }])
                .with_expected_version(Some(expected_version))
                .apply_blocking_with_outcome()
                .expect("apply config edit")
        })
    });

    let outcomes = writers.map(|writer| writer.join().expect("writer thread"));
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ConfigApplyOutcome::Applied)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ConfigApplyOutcome::VersionConflict)
            .count(),
        1
    );
    let persisted = std::fs::read_to_string(config_path).expect("read persisted config");
    assert!(persisted == "model = \"first\"\n" || persisted == "model = \"second\"\n");
}

#[test]
fn blocking_set_model_top_level() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    apply_blocking(
        codex_home,
        &[ConfigEdit::SetModel {
            model: Some("gpt-5.4".to_string()),
            effort: Some(ReasoningEffort::High),
        }],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"model = "gpt-5.4"
model_reasoning_effort = "high"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn set_service_tier_saves_default_as_default() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .set_service_tier(Some(SERVICE_TIER_DEFAULT_REQUEST_VALUE.to_string()))
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "service_tier = \"default\"\n");
}

#[test]
fn set_service_tier_saves_priority() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .set_service_tier(Some(ServiceTier::Fast.request_value().to_string()))
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "service_tier = \"priority\"\n");
}

#[test]
fn set_service_tier_preserves_unknown_service_tier() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .set_service_tier(Some("experimental-tier-id".to_string()))
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "service_tier = \"experimental-tier-id\"\n");
}

#[test]
fn builder_with_edits_applies_custom_paths() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits(vec![ConfigEdit::SetPath {
            segments: vec!["enabled".to_string()],
            value: value(true),
        }])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "enabled = true\n");
}

#[test]
fn session_picker_view_edit_writes_root_tui_setting() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([session_picker_view_edit(SessionPickerViewMode::Dense)])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[tui]
session_picker_view = "dense"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn keymap_binding_edit_writes_root_action_binding() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([keymap_binding_edit("composer", "submit", "ctrl-enter")])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[tui.keymap.composer]
submit = "ctrl-enter"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn keymap_bindings_edit_writes_single_binding_as_string() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([keymap_bindings_edit(
            "composer",
            "submit",
            &["ctrl-enter".to_string()],
        )])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[tui.keymap.composer]
submit = "ctrl-enter"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn keymap_bindings_edit_writes_multiple_bindings_as_array() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([keymap_bindings_edit(
            "composer",
            "submit",
            &["enter".to_string(), "ctrl-enter".to_string()],
        )])
        .apply_blocking()
        .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let value: TomlValue = toml::from_str(&raw).expect("parse config");

    assert_eq!(
        value
            .get("tui")
            .and_then(|value| value.get("keymap"))
            .and_then(|value| value.get("composer"))
            .and_then(|value| value.get("submit"))
            .and_then(TomlValue::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(TomlValue::as_str)
                    .collect::<Vec<_>>()
            }),
        Some(vec!["enter", "ctrl-enter"])
    );
}

#[test]
fn keymap_binding_edit_replaces_existing_binding_without_touching_profile() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"profile = "team"

[tui.keymap.composer]
submit = "enter"

[profiles.team.tui.keymap.composer]
submit = "shift-enter"
"#,
    )
    .expect("seed config");

    ConfigEditsBuilder::new(codex_home)
        .with_edits([keymap_binding_edit("composer", "submit", "ctrl-enter")])
        .apply_blocking()
        .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let value: TomlValue = toml::from_str(&raw).expect("parse config");

    assert_eq!(
        value
            .get("tui")
            .and_then(|value| value.get("keymap"))
            .and_then(|value| value.get("composer"))
            .and_then(|value| value.get("submit"))
            .and_then(TomlValue::as_str),
        Some("ctrl-enter")
    );
    assert_eq!(
        value
            .get("profiles")
            .and_then(|value| value.get("team"))
            .and_then(|value| value.get("tui"))
            .and_then(|value| value.get("keymap"))
            .and_then(|value| value.get("composer"))
            .and_then(|value| value.get("submit"))
            .and_then(TomlValue::as_str),
        Some("shift-enter")
    );
}

#[test]
fn keymap_binding_clear_edit_removes_root_action_binding_without_touching_profile() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"profile = "team"

[tui.keymap.composer]
submit = "enter"

[profiles.team.tui.keymap.composer]
submit = "shift-enter"
"#,
    )
    .expect("seed config");

    ConfigEditsBuilder::new(codex_home)
        .with_edits([keymap_binding_clear_edit("composer", "submit")])
        .apply_blocking()
        .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let value: TomlValue = toml::from_str(&raw).expect("parse config");

    assert_eq!(
        value
            .get("tui")
            .and_then(|value| value.get("keymap"))
            .and_then(|value| value.get("composer"))
            .and_then(|value| value.get("submit")),
        None
    );
    assert_eq!(
        value
            .get("profiles")
            .and_then(|value| value.get("team"))
            .and_then(|value| value.get("tui"))
            .and_then(|value| value.get("keymap"))
            .and_then(|value| value.get("composer"))
            .and_then(|value| value.get("submit"))
            .and_then(TomlValue::as_str),
        Some("shift-enter")
    );
}

#[test]
fn set_model_availability_nux_count_writes_shown_count() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    let shown_count = HashMap::from([("gpt-foo".to_string(), 4)]);

    ConfigEditsBuilder::new(codex_home)
        .set_model_availability_nux_count(&shown_count)
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[tui.model_availability_nux]
gpt-foo = 4
"#;
    assert_eq!(contents, expected);
}

#[test]
fn set_skill_config_writes_disabled_entry() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([ConfigEdit::SetSkillConfig {
            path: PathBuf::from("/tmp/skills/demo/SKILL.md"),
            enabled: false,
        }])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[[skills.config]]
path = "/tmp/skills/demo/SKILL.md"
enabled = false
"#;
    assert_eq!(contents, expected);
}

#[test]
fn set_skill_config_removes_entry_when_enabled() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[[skills.config]]
path = "/tmp/skills/demo/SKILL.md"
enabled = false
"#,
    )
    .expect("seed config");

    ConfigEditsBuilder::new(codex_home)
        .with_edits([ConfigEdit::SetSkillConfig {
            path: PathBuf::from("/tmp/skills/demo/SKILL.md"),
            enabled: true,
        }])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, "");
}

#[test]
fn set_skill_config_writes_name_selector_entry() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    ConfigEditsBuilder::new(codex_home)
        .with_edits([ConfigEdit::SetSkillConfigByName {
            name: "github:yeet".to_string(),
            enabled: false,
        }])
        .apply_blocking()
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[[skills.config]]
name = "github:yeet"
enabled = false
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_set_model_ignores_inline_legacy_profile_contents() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    // Seed with inline tables for profiles to simulate common user config.
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"profile = "fast"

profiles = { fast = { model = "gpt-4o", sandbox_mode = "strict" } }
"#,
    )
    .expect("seed");

    apply_blocking(
        codex_home,
        &[ConfigEdit::SetModel {
            model: Some("o4-mini".to_string()),
            effort: None,
        }],
    )
    .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let value: TomlValue = toml::from_str(&raw).expect("parse config");

    assert_eq!(
        value.get("model").and_then(TomlValue::as_str),
        Some("o4-mini")
    );

    // Legacy profile values stay untouched when root settings are updated.
    let profiles_tbl = value
        .get("profiles")
        .and_then(|v| v.as_table())
        .expect("profiles table");
    let fast_tbl = profiles_tbl
        .get("fast")
        .and_then(|v| v.as_table())
        .expect("fast table");
    assert_eq!(
        fast_tbl.get("sandbox_mode").and_then(|v| v.as_str()),
        Some("strict")
    );
    assert_eq!(
        fast_tbl.get("model").and_then(|v| v.as_str()),
        Some("gpt-4o")
    );
}

#[test]
fn batch_write_table_upsert_preserves_inline_comments() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    let original = r#"approval_policy = "never"

[mcp_servers.linear]
name = "linear"
# ok
url = "https://linear.example"

[mcp_servers.linear.http_headers]
foo = "bar"

[sandbox_workspace_write]
# ok 3
network_access = false
"#;
    std::fs::write(codex_home.join(CONFIG_TOML_FILE), original).expect("seed config");

    apply_blocking(
        codex_home,
        &[
            ConfigEdit::SetPath {
                segments: vec![
                    "mcp_servers".to_string(),
                    "linear".to_string(),
                    "url".to_string(),
                ],
                value: value("https://linear.example/v2"),
            },
            ConfigEdit::SetPath {
                segments: vec![
                    "sandbox_workspace_write".to_string(),
                    "network_access".to_string(),
                ],
                value: value(true),
            },
        ],
    )
    .expect("apply");

    let updated = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"approval_policy = "never"

[mcp_servers.linear]
name = "linear"
# ok
url = "https://linear.example/v2"

[mcp_servers.linear.http_headers]
foo = "bar"

[sandbox_workspace_write]
# ok 3
network_access = true
"#;
    assert_eq!(updated, expected);
}

#[test]
fn blocking_clear_model_does_not_follow_legacy_active_profile() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"profile = "fast"

profiles = { fast = { model = "gpt-4o", sandbox_mode = "strict" } }
"#,
    )
    .expect("seed");

    apply_blocking(
        codex_home,
        &[ConfigEdit::SetModel {
            model: None,
            effort: Some(ReasoningEffort::High),
        }],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"profile = "fast"

profiles = { fast = { model = "gpt-4o", sandbox_mode = "strict" } }
model_reasoning_effort = "high"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_set_model_does_not_follow_legacy_active_profile() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"profile = "team"

[profiles.team]
model_reasoning_effort = "low"
"#,
    )
    .expect("seed");

    apply_blocking(
        codex_home,
        &[ConfigEdit::SetModel {
            model: Some("o5-preview".to_string()),
            effort: Some(ReasoningEffort::Minimal),
        }],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"profile = "team"
model = "o5-preview"
model_reasoning_effort = "minimal"

[profiles.team]
model_reasoning_effort = "low"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_set_hide_rate_limit_model_nudge_preserves_table() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[notice]
existing = "value"
"#,
    )
    .expect("seed");

    apply_blocking(
        codex_home,
        &[ConfigEdit::SetNoticeHideRateLimitModelNudge(true)],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[notice]
existing = "value"
hide_rate_limit_model_nudge = true
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_record_model_migration_seen_preserves_table() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[notice]
existing = "value"
"#,
    )
    .expect("seed");
    apply_blocking(
        codex_home,
        &[ConfigEdit::RecordModelMigrationSeen {
            from: "gpt-5.2".to_string(),
            to: "gpt-5.4".to_string(),
        }],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[notice]
existing = "value"

[notice.model_migrations]
"gpt-5.2" = "gpt-5.4"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_replace_mcp_servers_round_trips() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    let mut servers = BTreeMap::new();
    servers.insert(
        "stdio".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "cmd".to_string(),
                args: vec!["--flag".to_string()],
                env: Some(
                    [
                        ("B".to_string(), "2".to_string()),
                        ("A".to_string(), "1".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
                env_vars: vec!["FOO".into()],
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: true,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: Some(vec!["one".to_string(), "two".to_string()]),
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    servers.insert(
        "http".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com".to_string(),
                bearer_token_env_var: Some("TOKEN".to_string()),
                http_headers: Some(
                    [("Z-Header".to_string(), "z".to_string())]
                        .into_iter()
                        .collect(),
                ),
                env_http_headers: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(std::time::Duration::from_secs(5)),
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: Some(vec!["forbidden".to_string()]),
            scopes: None,
            oauth: Some(McpServerOAuthConfig {
                client_id: Some("eci-prd-pub-codex-123".to_string()),
            }),
            oauth_resource: Some("https://resource.example.com".to_string()),
            tools: HashMap::new(),
        },
    );

    apply_blocking(
        codex_home,
        &[ConfigEdit::ReplaceMcpServers(servers.clone())],
    )
    .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = "\
[mcp_servers.http]
url = \"https://example.com\"
bearer_token_env_var = \"TOKEN\"
enabled = false
startup_timeout_sec = 5.0
disabled_tools = [\"forbidden\"]
oauth_resource = \"https://resource.example.com\"

[mcp_servers.http.http_headers]
Z-Header = \"z\"

[mcp_servers.http.oauth]
client_id = \"eci-prd-pub-codex-123\"

[mcp_servers.stdio]
command = \"cmd\"
args = [\"--flag\"]
env_vars = [\"FOO\"]
supports_parallel_tool_calls = true
enabled_tools = [\"one\", \"two\"]

[mcp_servers.stdio.env]
A = \"1\"
B = \"2\"
";
    assert_eq!(raw, expected);
}

#[test]
fn blocking_replace_mcp_servers_serializes_tool_approval_overrides() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    let mut servers = BTreeMap::new();
    servers.insert(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: Some(AppToolApproval::Prompt),
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::from([(
                "search".to_string(),
                McpServerToolConfig {
                    approval_mode: Some(AppToolApproval::Approve),
                },
            )]),
        },
    );

    apply_blocking(codex_home, &[ConfigEdit::ReplaceMcpServers(servers)]).expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = "\
[mcp_servers.docs]
command = \"docs-server\"
default_tools_approval_mode = \"prompt\"

[mcp_servers.docs.tools]

[mcp_servers.docs.tools.search]
approval_mode = \"approve\"
";
    assert_eq!(raw, expected);
}

#[test]
fn blocking_replace_mcp_servers_preserves_inline_comments() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[mcp_servers]
# keep me
foo = { command = "cmd" }
"#,
    )
    .expect("seed");

    let mut servers = BTreeMap::new();
    servers.insert(
        "foo".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "cmd".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    apply_blocking(codex_home, &[ConfigEdit::ReplaceMcpServers(servers)]).expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[mcp_servers]
# keep me
foo = { command = "cmd" }
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_replace_mcp_servers_preserves_inline_comment_suffix() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[mcp_servers]
foo = { command = "cmd" } # keep me
"#,
    )
    .expect("seed");

    let mut servers = BTreeMap::new();
    servers.insert(
        "foo".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "cmd".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    apply_blocking(codex_home, &[ConfigEdit::ReplaceMcpServers(servers)]).expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[mcp_servers]
foo = { command = "cmd" , enabled = false } # keep me
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_replace_mcp_servers_preserves_inline_comment_after_removing_keys() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[mcp_servers]
foo = { command = "cmd", args = ["--flag"] } # keep me
"#,
    )
    .expect("seed");

    let mut servers = BTreeMap::new();
    servers.insert(
        "foo".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "cmd".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    apply_blocking(codex_home, &[ConfigEdit::ReplaceMcpServers(servers)]).expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[mcp_servers]
foo = { command = "cmd"} # keep me
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_replace_mcp_servers_preserves_inline_comment_prefix_on_update() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"[mcp_servers]
# keep me
foo = { command = "cmd" }
"#,
    )
    .expect("seed");

    let mut servers = BTreeMap::new();
    servers.insert(
        "foo".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "cmd".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: false,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );

    apply_blocking(codex_home, &[ConfigEdit::ReplaceMcpServers(servers)]).expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"[mcp_servers]
# keep me
foo = { command = "cmd" , enabled = false }
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_clear_path_noop_when_missing() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    apply_blocking(
        codex_home,
        &[ConfigEdit::ClearPath {
            segments: vec!["missing".to_string()],
        }],
    )
    .expect("apply");

    assert!(
        !codex_home.join(CONFIG_TOML_FILE).exists(),
        "config.toml should not be created on noop"
    );
}

#[test]
fn blocking_set_path_updates_notifications() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    let item = value(false);
    apply_blocking(
        codex_home,
        &[ConfigEdit::SetPath {
            segments: vec!["tui".to_string(), "notifications".to_string()],
            value: item,
        }],
    )
    .expect("apply");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let config: TomlValue = toml::from_str(&raw).expect("parse config");
    let notifications = config
        .get("tui")
        .and_then(|item| item.as_table())
        .and_then(|tbl| tbl.get("notifications"))
        .and_then(toml::Value::as_bool);
    assert_eq!(notifications, Some(false));
}

#[tokio::test]
async fn async_builder_set_model_persists() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path().to_path_buf();

    ConfigEditsBuilder::new(&codex_home)
        .set_model(Some("gpt-5.4"), Some(ReasoningEffort::High))
        .apply()
        .await
        .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let expected = r#"model = "gpt-5.4"
model_reasoning_effort = "high"
"#;
    assert_eq!(contents, expected);
}

#[test]
fn blocking_builder_set_model_round_trips_back_and_forth() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();

    let initial_expected = r#"model = "o4-mini"
model_reasoning_effort = "low"
"#;
    ConfigEditsBuilder::new(codex_home)
        .set_model(Some("o4-mini"), Some(ReasoningEffort::Low))
        .apply_blocking()
        .expect("persist initial");
    let mut contents =
        std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, initial_expected);

    let updated_expected = r#"model = "gpt-5.4"
model_reasoning_effort = "high"
"#;
    ConfigEditsBuilder::new(codex_home)
        .set_model(Some("gpt-5.4"), Some(ReasoningEffort::High))
        .apply_blocking()
        .expect("persist update");
    contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, updated_expected);

    ConfigEditsBuilder::new(codex_home)
        .set_model(Some("o4-mini"), Some(ReasoningEffort::Low))
        .apply_blocking()
        .expect("persist revert");
    contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert_eq!(contents, initial_expected);
}

#[tokio::test]
async fn blocking_set_asynchronous_helpers_available() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path().to_path_buf();

    ConfigEditsBuilder::new(&codex_home)
        .set_hide_world_writable_warning(/*acknowledged*/ true)
        .apply()
        .await
        .expect("persist");

    let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    let notice = toml::from_str::<TomlValue>(&raw)
        .expect("parse config")
        .get("notice")
        .and_then(|item| item.as_table())
        .and_then(|tbl| tbl.get("hide_world_writable_warning"))
        .and_then(toml::Value::as_bool);
    assert_eq!(notice, Some(true));
}

#[test]
fn replace_mcp_servers_blocking_clears_table_when_empty() {
    let tmp = tempdir().expect("tmpdir");
    let codex_home = tmp.path();
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        "[mcp_servers]\nfoo = { command = \"cmd\" }\n",
    )
    .expect("seed");

    apply_blocking(
        codex_home,
        &[ConfigEdit::ReplaceMcpServers(BTreeMap::new())],
    )
    .expect("persist");

    let contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
    assert!(!contents.contains("mcp_servers"));
}
