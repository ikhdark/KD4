use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use codex_analytics::InvocationType;
use codex_analytics::SkillInvocation;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_plugin::LoadedPlugin;
use codex_plugin::PluginLoadOutcome;
use codex_plugin::manifest::PluginSkillToolExposure;
use codex_plugin::manifest::PluginToolExposure;
use codex_plugin::manifest::PluginToolExposureConfig;
use codex_protocol::protocol::SkillScope;
use codex_tools::ToolName;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Meta;
use rmcp::model::Tool;

use super::*;
use crate::config::CONFIG_TOML_FILE;
use crate::config::ConfigBuilder;
use crate::config::test_config;
use crate::connectors::AppInfo;
use tempfile::tempdir;

fn make_connector(id: &str, name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

fn make_mcp_tool(
    server_name: &str,
    tool_name: &str,
    callable_namespace: &str,
    callable_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: callable_name.to_string(),
        callable_namespace: callable_namespace.to_string(),
        namespace_description: None,
        tool: Tool::new(
            tool_name.to_string(),
            format!("Test tool: {tool_name}"),
            Arc::new(JsonObject::default()),
        ),
        connector_id: connector_id.map(str::to_string),
        connector_name: connector_name.map(str::to_string),
        plugin_display_names: Vec::new(),
    }
}

fn numbered_mcp_tools(count: usize) -> Vec<ToolInfo> {
    (0..count)
        .map(|index| {
            let tool_name = format!("tool_{index}");
            make_mcp_tool(
                "rmcp",
                &tool_name,
                "mcp__rmcp",
                &tool_name,
                /*connector_id*/ None,
                /*connector_name*/ None,
            )
        })
        .collect()
}

fn tool_names(tools: &[ToolInfo]) -> HashSet<ToolName> {
    tools
        .iter()
        .map(codex_mcp::ToolInfo::canonical_tool_name)
        .collect()
}

fn with_visibility(mut tool: ToolInfo, visibility: &[&str]) -> ToolInfo {
    tool.tool.meta = Some(Meta(
        serde_json::json!({ "ui": { "visibility": visibility } })
            .as_object()
            .expect("metadata object")
            .clone(),
    ));
    tool
}

fn selected_plugin_skill(plugin_id: &str, skill_name: &str) -> SkillInvocation {
    SkillInvocation {
        skill_name: skill_name.to_string(),
        skill_scope: SkillScope::User,
        skill_path: PathBuf::from("SKILL.md"),
        plugin_id: Some(plugin_id.to_string()),
        invocation_type: InvocationType::Explicit,
    }
}

fn test_mcp_server() -> McpServerConfig {
    McpServerConfig {
        auth: Default::default(),
        transport: McpServerTransportConfig::StreamableHttp {
            url: "https://example.invalid/mcp".to_string(),
            bearer_token_env_var: None,
            http_headers: None,
            env_http_headers: None,
        },
        environment_id: "local".to_string(),
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
    }
}

fn plugin_outcome(tool_exposure: Option<PluginToolExposure>) -> PluginLoadOutcome<McpServerConfig> {
    PluginLoadOutcome::from_plugins(vec![LoadedPlugin {
        config_name: "repo-atlas".to_string(),
        manifest_name: Some("repo-atlas".to_string()),
        plugin_namespace: Some("repo-atlas".to_string()),
        manifest_description: None,
        tool_exposure,
        root: AbsolutePathBuf::from_absolute_path_checked(std::env::temp_dir().join("repo-atlas"))
            .expect("temporary path should be absolute"),
        enabled: true,
        skill_roots: Vec::new(),
        disabled_skill_paths: HashSet::new(),
        has_enabled_skills: true,
        mcp_servers: HashMap::from([("repo-atlas".to_string(), test_mcp_server())]),
        apps: Vec::new(),
        hook_sources: Vec::new(),
        hook_load_warnings: Vec::new(),
        error: None,
    }])
}

fn repo_atlas_tool_exposure(tool_names: &[&str]) -> PluginToolExposure {
    PluginToolExposure::Valid(PluginToolExposureConfig {
        skills: BTreeMap::from([(
            "repo-atlas".to_string(),
            PluginSkillToolExposure {
                mcp_tools: BTreeMap::from([(
                    "repo-atlas".to_string(),
                    tool_names.iter().map(ToString::to_string).collect(),
                )]),
            },
        )]),
    })
}

fn repo_atlas_tools() -> Vec<ToolInfo> {
    ["task", "trace"]
        .into_iter()
        .map(|tool_name| {
            make_mcp_tool(
                "repo-atlas",
                tool_name,
                "mcp__repo_atlas",
                tool_name,
                None,
                None,
            )
        })
        .collect()
}

#[test]
fn selected_skill_promotes_only_declared_mcp_entrypoints() {
    let tools = repo_atlas_tools();
    let exposure = resolve_selected_skill_mcp_exposure(
        &[selected_plugin_skill("repo-atlas", "repo-atlas")],
        &plugin_outcome(Some(repo_atlas_tool_exposure(&["task"]))),
        &tools,
    );

    assert_eq!(
        exposure.direct_entrypoints,
        vec![DirectMcpToolEntrypoint {
            server_name: "repo-atlas".to_string(),
            tool_name: "task".to_string(),
        }]
    );
    assert!(exposure.selection.includes(&tools[0]));
    assert!(!exposure.selection.includes(&tools[1]));
    assert!(exposure.diagnostics.is_empty());
}

#[test]
fn selected_skill_without_declaration_preserves_whole_server_promotion() {
    let tools = repo_atlas_tools();
    let exposure = resolve_selected_skill_mcp_exposure(
        &[selected_plugin_skill("repo-atlas", "repo-atlas")],
        &plugin_outcome(None),
        &tools,
    );

    assert_eq!(
        exposure.direct_entrypoints,
        vec![
            DirectMcpToolEntrypoint {
                server_name: "repo-atlas".to_string(),
                tool_name: "task".to_string(),
            },
            DirectMcpToolEntrypoint {
                server_name: "repo-atlas".to_string(),
                tool_name: "trace".to_string(),
            },
        ]
    );
    assert!(exposure.diagnostics.is_empty());
}

#[test]
fn invalid_explicit_declaration_does_not_broaden_exposure() {
    let tools = repo_atlas_tools();
    let exposure = resolve_selected_skill_mcp_exposure(
        &[selected_plugin_skill("repo-atlas", "repo-atlas")],
        &plugin_outcome(Some(PluginToolExposure::Invalid(
            "invalid fixture".to_string(),
        ))),
        &tools,
    );

    assert!(exposure.direct_entrypoints.is_empty());
    assert!(!exposure.selection.includes(&tools[0]));
    assert!(!exposure.selection.includes(&tools[1]));
    assert_eq!(exposure.diagnostics.len(), 1);
    assert!(exposure.diagnostics[0].contains("promoting nothing"));
}

#[test]
fn unselected_skills_promote_nothing() {
    let tools = repo_atlas_tools();
    let exposure = resolve_selected_skill_mcp_exposure(
        &[],
        &plugin_outcome(Some(repo_atlas_tool_exposure(&["task"]))),
        &tools,
    );

    assert!(exposure.direct_entrypoints.is_empty());
    assert!(!exposure.selection.includes(&tools[0]));
    assert!(!exposure.selection.includes(&tools[1]));
}

#[tokio::test]
async fn directly_exposes_effective_tool_sets_when_search_is_unavailable() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &config,
        /*search_tool_enabled*/ false,
        &DirectMcpToolSelection::default(),
    );

    assert_eq!(tool_names(&exposure.direct_tools), tool_names(&mcp_tools));
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn excludes_tools_hidden_from_model_exposure() {
    let config = test_config().await;
    let visible_tool = make_mcp_tool(
        "rmcp",
        "visible_tool",
        "mcp__rmcp",
        "visible_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let hidden_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "hidden_tool",
            "mcp__rmcp",
            "hidden_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &["app"],
    );
    let empty_visibility_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "empty_visibility_tool",
            "mcp__rmcp",
            "empty_visibility_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &[],
    );
    let visible_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_read",
            "mcp__codex_apps__calendar",
            "read",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app", "model"],
    );
    let hidden_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_open",
            "mcp__codex_apps__calendar",
            "open",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app"],
    );
    let mcp_tools = vec![
        visible_tool.clone(),
        hidden_tool,
        empty_visibility_tool,
        visible_app_tool.clone(),
        hidden_app_tool,
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ false,
        &DirectMcpToolSelection::default(),
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[visible_tool, visible_app_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn applies_per_tool_app_policy_across_the_exposure_build() {
    let codex_home = tempdir().expect("tempdir should succeed");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[apps.calendar]
default_tools_enabled = false

[apps.calendar.tools."events/create"]
enabled = true
"#,
    )
    .expect("write config");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config should build");
    let enabled_tool = make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "events/create",
        "mcp__codex_apps__calendar",
        "create",
        Some("calendar"),
        Some("Calendar"),
    );
    let disabled_tool = make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "events/list",
        "mcp__codex_apps__calendar",
        "list",
        Some("calendar"),
        Some("Calendar"),
    );
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &[enabled_tool.clone(), disabled_tool],
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ false,
        &DirectMcpToolSelection::default(),
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[enabled_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn defers_effective_tool_sets_when_search_is_available() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &config,
        /*search_tool_enabled*/ true,
        &DirectMcpToolSelection::default(),
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    assert_eq!(tool_names(deferred_tools), tool_names(&mcp_tools));
}

#[tokio::test]
async fn directly_exposes_tools_required_by_enabled_plugin_skills() {
    let config = test_config().await;
    let skill_tool = make_mcp_tool(
        "skill-plugin",
        "skill_tool",
        "mcp__skill_plugin",
        "skill_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let deferred_tool = make_mcp_tool(
        "other-plugin",
        "other_tool",
        "mcp__other_plugin",
        "other_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let mcp_tools = vec![skill_tool.clone(), deferred_tool.clone()];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        /*connectors*/ None,
        &config,
        /*search_tool_enabled*/ true,
        &DirectMcpToolSelection {
            legacy_server_names: HashSet::from(["skill-plugin".to_string()]),
            ..Default::default()
        },
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[skill_tool])
    );
    assert_eq!(
        tool_names(
            exposure
                .deferred_tools
                .as_deref()
                .expect("unrelated MCP tools should remain discoverable through tool_search")
        ),
        tool_names(&[deferred_tool])
    );
}

#[tokio::test]
async fn defers_apps_and_non_app_mcp_tools() {
    let config = test_config().await;
    let mcp_tools = vec![
        make_mcp_tool(
            "rmcp",
            "tool",
            "mcp__rmcp",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_create_event",
            "mcp__codex_apps__calendar",
            "_create_event",
            Some("calendar"),
            Some("Calendar"),
        ),
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ true,
        &DirectMcpToolSelection::default(),
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    let deferred_tool_names = tool_names(deferred_tools);
    assert!(deferred_tool_names.contains(&ToolName::namespaced("mcp__rmcp", "tool")));
    assert!(deferred_tool_names.contains(&ToolName::namespaced(
        "mcp__codex_apps__calendar",
        "_create_event"
    )));
}
