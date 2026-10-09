use super::*;
use codex_config::McpServerTransportConfig;
use codex_core::config::ConfigBuilder;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn hosted_plugin_runtime_retains_identity_and_originator_across_enablement()
-> Result<(), Box<dyn std::error::Error>> {
    let codex_home = tempfile::tempdir()?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cli_overrides(vec![
            ("features.apps".to_string(), true.into()),
            ("chatgpt_base_url".to_string(), "https://chatgpt.com".into()),
        ])
        .build()
        .await?;
    let thread_init = ExtensionDataInit::new();
    let thread_store = ExtensionData::new("thread");
    for enabled in [true, false] {
        if enabled {
            config.features.enable(codex_features::Feature::Apps)?;
        } else {
            config.features.disable(codex_features::Feature::Apps)?;
        }
        let contributions = HostedPluginRuntimeExtension
            .contribute(McpServerContributionContext::for_step(
                &config,
                &thread_init,
                &thread_store,
                "codex_work_desktop",
                &[],
            ))
            .await;
        let [McpServerContribution::Set { name, config: server }] = contributions.as_slice()
        else {
            panic!("hosted plugin runtime should retain exactly one server");
        };
        assert_eq!(name, CODEX_APPS_MCP_SERVER_NAME);
        assert_eq!(server.enabled, enabled);
        let McpServerTransportConfig::StreamableHttp { url, http_headers, .. } = &server.transport
        else {
            panic!("hosted plugin runtime should use streamable HTTP");
        };
        assert_eq!(url, "https://chatgpt.com/backend-api/ps/mcp");
        assert_eq!(
            http_headers.as_ref().and_then(|headers| headers.get("originator")),
            Some(&"codex_work_desktop".to_string())
        );
    }
    Ok(())
}
