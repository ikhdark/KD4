use super::*;
use crate::plugins::test_support::load_plugins_config;
use crate::plugins::test_support::write_curated_plugin_sha;
use crate::plugins::test_support::write_openai_curated_marketplace;
use crate::plugins::test_support::write_plugins_feature_config;
use crate::session::tests::make_session_and_context_with_rx;
use codex_app_server_protocol::PluginAuthPolicy;
use codex_app_server_protocol::PluginAvailability;
use codex_app_server_protocol::PluginInstallPolicy;
use codex_config::CONFIG_TOML_FILE;
use codex_config::config_toml::ConfigToml;
use codex_config::types::ToolSuggestConfig;
use codex_config::types::ToolSuggestDisabledTool;
use codex_config::types::ToolSuggestDiscoverable;
use codex_config::types::ToolSuggestDiscoverableType;
use codex_core_plugins::PluginInstallRequest;
use codex_core_plugins::PluginsManager;
use codex_core_plugins::remote::RemoteMarketplace;
use codex_core_plugins::remote::RemotePluginSummary;
use codex_core_plugins::startup_sync::curated_plugins_repo_path;
use codex_rmcp_client::ElicitationResponse;
use codex_tools::DiscoverablePluginInfo;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::PathExt;
use pretty_assertions::assert_eq;
use rmcp::model::ElicitationAction;
use serde_json::json;
use tempfile::tempdir;

#[test]
fn request_plugin_install_does_not_support_parallel_tool_calls() {
    let handler = RequestPluginInstallHandler::new(
        Vec::new(),
        ToolSuggestPresentation::RecommendationContext,
    );

    assert!(!handler.supports_parallel_tool_calls());
}

fn remote_marketplace_with_plugin(
    config_id: &str,
    remote_plugin_id: &str,
    installed: bool,
) -> RemoteMarketplace {
    RemoteMarketplace {
        name: REMOTE_GLOBAL_MARKETPLACE_NAME.to_string(),
        display_name: "OpenAI Curated Remote".to_string(),
        plugins: vec![RemotePluginSummary {
            id: config_id.to_string(),
            remote_plugin_id: remote_plugin_id.to_string(),
            version: None,
            local_version: None,
            name: "Calendar".to_string(),
            share_context: None,
            installed,
            enabled: true,
            install_policy: PluginInstallPolicy::Available,
            install_policy_source: None,
            auth_policy: PluginAuthPolicy::OnUse,
            availability: PluginAvailability::Available,
            interface: None,
            keywords: Vec::new(),
        }],
    }
}

#[test]
fn remote_plugin_completion_requires_requested_plugin_even_without_connectors() {
    let marketplaces = vec![remote_marketplace_with_plugin(
        "other@openai-curated-remote",
        "plugin_other",
        true,
    )];

    assert!(!verified_remote_plugin_install_completed(
        Some(&marketplaces),
        "calendar@openai-curated-remote",
        Some("plugin_calendar"),
        true,
    ));
}

#[test]
fn remote_plugin_completion_accepts_present_plugin_without_connectors() {
    let matching_config_id = vec![remote_marketplace_with_plugin(
        "calendar@openai-curated-remote",
        "plugin_other",
        true,
    )];
    let matching_remote_id = vec![remote_marketplace_with_plugin(
        "renamed-calendar@openai-curated-remote",
        "plugin_calendar",
        true,
    )];

    assert!(verified_remote_plugin_install_completed(
        Some(&matching_config_id),
        "calendar@openai-curated-remote",
        Some("plugin_calendar"),
        true,
    ));
    assert!(verified_remote_plugin_install_completed(
        Some(&matching_remote_id),
        "calendar@openai-curated-remote",
        Some("plugin_calendar"),
        true,
    ));
}

#[test]
fn remote_plugin_completion_rejects_catalog_refresh_failure_with_connectors_accessible() {
    assert!(!verified_remote_plugin_install_completed(
        None,
        "calendar@openai-curated-remote",
        Some("plugin_calendar"),
        true,
    ));
}

#[tokio::test]
async fn verified_plugin_install_completed_requires_installed_plugin() {
    let codex_home = tempdir().expect("tempdir should succeed");
    let curated_root = curated_plugins_repo_path(codex_home.path());
    write_openai_curated_marketplace(&curated_root, &["sample"]);
    write_curated_plugin_sha(codex_home.path());
    write_plugins_feature_config(codex_home.path());

    let config = load_plugins_config(codex_home.path()).await;
    let plugins_manager = PluginsManager::new(codex_home.path().to_path_buf());

    assert!(!verified_plugin_install_completed(
        "sample@openai-curated",
        &config,
        &plugins_manager,
    ));

    plugins_manager
        .install_plugin(
            &config.config_layer_stack,
            PluginInstallRequest {
                plugin_name: "sample".to_string(),
                marketplace_path: AbsolutePathBuf::try_from(
                    curated_root.join(".agents/plugins/marketplace.json"),
                )
                .expect("marketplace path"),
            },
        )
        .await
        .expect("plugin should install");

    let refreshed_config = load_plugins_config(codex_home.path()).await;
    assert!(verified_plugin_install_completed(
        "sample@openai-curated",
        &refreshed_config,
        &plugins_manager,
    ));
}

#[test]
fn remote_plugin_install_suggestions_skip_core_installed_verification() {
    assert!(is_remote_plugin_install_suggestion(
        "snowflake@openai-curated-remote"
    ));
    assert!(!is_remote_plugin_install_suggestion(
        "snowflake@openai-curated"
    ));
    assert!(!is_remote_plugin_install_suggestion("Plugin_123"));
}

#[test]
fn recommended_plugin_install_args_accept_legacy_tool_id() {
    let current: RecommendedPluginInstallArgs = serde_json::from_value(json!({
        "plugin_id": "google-drive@openai-curated-remote",
        "suggest_reason": "Use Google Drive for this request"
    }))
    .expect("current arguments should deserialize");
    let legacy: RecommendedPluginInstallArgs = serde_json::from_value(json!({
        "tool_type": "plugin",
        "action_type": "install",
        "tool_id": "google-drive@openai-curated-remote",
        "suggest_reason": "Use Google Drive for this request"
    }))
    .expect("legacy arguments should deserialize");

    assert_eq!(current, legacy);
}

#[test]
fn request_plugin_install_response_persists_only_decline_always_mode() {
    assert!(request_plugin_install_response_requests_persistent_disable(
        &ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: Some(json!({
                REQUEST_PLUGIN_INSTALL_PERSIST_KEY: REQUEST_PLUGIN_INSTALL_PERSIST_ALWAYS_VALUE
            })),
        }
    ));
    assert!(
        !request_plugin_install_response_requests_persistent_disable(&ElicitationResponse {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(json!({
                REQUEST_PLUGIN_INSTALL_PERSIST_KEY: REQUEST_PLUGIN_INSTALL_PERSIST_ALWAYS_VALUE
            })),
        })
    );
    assert!(
        !request_plugin_install_response_requests_persistent_disable(&ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: Some(json!({ REQUEST_PLUGIN_INSTALL_PERSIST_KEY: "session" })),
        })
    );
    assert!(
        !request_plugin_install_response_requests_persistent_disable(&ElicitationResponse {
            action: ElicitationAction::Decline,
            content: None,
            meta: None,
        })
    );
}

#[tokio::test]
async fn persist_disabled_install_request_writes_connector_config() {
    let codex_home = tempdir().expect("tempdir should succeed");
    let tool = connector_tool("connector_calendar", "Google Calendar");

    persist_disabled_install_request(&codex_home.path().abs(), &tool)
        .await
        .expect("persist connector disable");

    let contents =
        std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).expect("read config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse config");
    assert_eq!(
        parsed.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![ToolSuggestDisabledTool::connector("connector_calendar")],
        })
    );
}

#[tokio::test]
async fn persist_disabled_install_request_writes_plugin_config() {
    let codex_home = tempdir().expect("tempdir should succeed");
    let tool = DiscoverableTool::Plugin(Box::new(DiscoverablePluginInfo {
        id: "slack@openai-curated".to_string(),
        remote_plugin_id: None,
        name: "Slack".to_string(),
        description: None,
        has_skills: true,
        mcp_server_names: Vec::new(),
        app_connector_ids: Vec::new(),
    }));

    persist_disabled_install_request(&codex_home.path().abs(), &tool)
        .await
        .expect("persist plugin disable");

    let contents =
        std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).expect("read config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse config");
    assert_eq!(
        parsed.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: Vec::new(),
            disabled_tools: vec![ToolSuggestDisabledTool::plugin("slack@openai-curated")],
        })
    );
}

#[tokio::test]
async fn persistent_disable_write_failure_produces_nonfatal_warning_feedback() {
    let parent = tempdir().expect("tempdir should succeed");
    let codex_home_file = parent.path().join("codex-home-file");
    std::fs::write(&codex_home_file, "not a directory").expect("write codex home file");
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let tool = connector_tool("connector_calendar", "Google Calendar");
    let response = ElicitationResponse {
        action: ElicitationAction::Decline,
        content: None,
        meta: Some(json!({
            REQUEST_PLUGIN_INSTALL_PERSIST_KEY: REQUEST_PLUGIN_INSTALL_PERSIST_ALWAYS_VALUE
        })),
    };

    handle_disabled_install_persistence(&session, &turn, &codex_home_file.abs(), &tool, &response)
        .await;

    let warning = events
        .try_recv()
        .expect("persistence failure should emit a warning event");
    assert_eq!(warning.id, turn.sub_id);

    assert!(matches!(
        warning.msg,
        EventMsg::Warning(WarningEvent { message })
            if message
                == "Could not save the do-not-suggest-again preference for Google Calendar. This suggestion may appear again in a future turn."
    ));
}

#[tokio::test]
async fn persist_disabled_install_request_dedupes_existing_disabled_tools() {
    let codex_home = tempdir().expect("tempdir should succeed");
    let tool = connector_tool("connector_calendar", "Google Calendar");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[tool_suggest]
discoverables = [
  { type = "plugin", id = "sample@openai-curated" }
]

[[tool_suggest.disabled_tools]]
type = "connector"
id = " connector_calendar "

[[tool_suggest.disabled_tools]]
type = "connector"
id = "connector_calendar"

[[tool_suggest.disabled_tools]]
type = "connector"
id = "   "

[[tool_suggest.disabled_tools]]
type = "plugin"
id = "slack@openai-curated"
"#,
    )
    .expect("write config");

    persist_disabled_install_request(&codex_home.path().abs(), &tool)
        .await
        .expect("persist connector disable");

    let contents =
        std::fs::read_to_string(codex_home.path().join(CONFIG_TOML_FILE)).expect("read config");
    let parsed: ConfigToml = toml::from_str(&contents).expect("parse config");
    assert_eq!(
        parsed.tool_suggest,
        Some(ToolSuggestConfig {
            discoverables: vec![ToolSuggestDiscoverable {
                kind: ToolSuggestDiscoverableType::Plugin,
                id: "sample@openai-curated".to_string(),
            }],
            disabled_tools: vec![
                ToolSuggestDisabledTool::connector("connector_calendar"),
                ToolSuggestDisabledTool::plugin("slack@openai-curated"),
            ],
        })
    );
}

fn connector_tool(id: &str, name: &str) -> DiscoverableTool {
    DiscoverableTool::Connector(Box::new(AppInfo {
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
        is_accessible: false,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }))
}
