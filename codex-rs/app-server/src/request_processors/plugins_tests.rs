use super::*;
use codex_core::skills::SkillsLoadInput;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::LOCAL_FS;
use codex_login::CodexAuth;
use core_test_support::load_default_config_for_test;
use futures::FutureExt;
use tempfile::TempDir;
use tokio::sync::mpsc;

#[test]
fn remote_catalog_jsonrpc_error_preserves_typed_recovery_data() {
    let error = remote_plugin_catalog_error_to_jsonrpc(
        codex_core_plugins::remote::RemotePluginCatalogError::UnexpectedStatus {
            url: "https://example.test/plugins".to_string(),
            status: reqwest::StatusCode::FORBIDDEN,
            body: "localized body".to_string(),
        },
        "list remote plugins",
    );

    assert_eq!(error.code, crate::error_code::INVALID_REQUEST_ERROR_CODE);
    assert_eq!(
        serde_json::from_value::<PluginRemoteErrorData>(error.data.expect("typed error data"))
            .expect("valid error data"),
        PluginRemoteErrorData {
            reason: PluginRemoteErrorReason::AccessDenied,
            retryable: false,
        }
    );
}

#[test]
fn missing_error_path_remote_uninstall_cache_failure_is_not_tracked_as_success() {
    assert_eq!(
        remote_plugin_uninstall_effects(&Err(RemotePluginCatalogError::CacheRemove(
            "injected cache failure".to_string(),
        ))),
        RemotePluginUninstallEffects {
            track_success: false,
            refresh_caches: true,
        }
    );
    assert_eq!(
        remote_plugin_uninstall_effects(&Ok(())),
        RemotePluginUninstallEffects {
            track_success: true,
            refresh_caches: true,
        }
    );
    assert_eq!(
        remote_plugin_uninstall_effects(&Err(RemotePluginCatalogError::AuthRequired)),
        RemotePluginUninstallEffects {
            track_success: false,
            refresh_caches: false,
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_uninstall_invalidates_caches_before_returning() -> anyhow::Result<()> {
    let codex_home = TempDir::new()?;
    let plugin_root = codex_home
        .path()
        .join("plugins/cache/debug/sample-plugin/local");
    let skill_dir = plugin_root.join("skills/sample-skill");
    std::fs::create_dir_all(plugin_root.join(".codex-plugin"))?;
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        r#"{"name":"sample-plugin"}"#,
    )?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: sample-skill\ndescription: Sample skill\n---\n",
    )?;
    std::fs::write(
        codex_home.path().join(codex_config::CONFIG_TOML_FILE),
        r#"[features]
plugins = true

[plugins."sample-plugin@debug"]
enabled = true
"#,
    )?;

    let config = load_default_config_for_test(&codex_home).await;
    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            Arc::new(EnvironmentManager::default_for_tests()),
        ),
    );
    let plugins_manager = thread_manager.plugins_manager();
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    assert!(
        plugin_outcome
            .plugins()
            .iter()
            .any(|plugin| { plugin.config_name == "sample-plugin@debug" && plugin.is_active() })
    );

    let skills_service = thread_manager.skills_service();
    let skills_input = SkillsLoadInput::new(
        config.cwd.clone(),
        plugin_outcome.effective_plugin_skill_roots(),
        config.config_layer_stack.clone(),
        config.bundled_skills_enabled(),
    );
    let skills_snapshot = skills_service
        .snapshot_for_cwd(
            &skills_input,
            /*force_reload*/ false,
            Some(Arc::clone(&LOCAL_FS)),
        )
        .await;
    assert!(
        skills_snapshot
            .outcome()
            .skills
            .iter()
            .any(|skill| { skill.name == "sample-plugin:sample-skill" })
    );

    let auth_manager = codex_core::test_support::auth_manager_from_auth_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        config.codex_home.to_path_buf(),
    );
    let (outgoing_tx, _outgoing_rx) = mpsc::channel(crate::CHANNEL_CAPACITY);
    let processor = PluginRequestProcessor::new(
        auth_manager,
        Arc::clone(&thread_manager),
        Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            AnalyticsEventsClient::disabled(),
        )),
        AnalyticsEventsClient::disabled(),
        ConfigManager::without_managed_config_for_tests(config.codex_home.to_path_buf()),
        Arc::new(workspace_settings::WorkspaceSettingsCache::default()),
    );

    processor
        .plugin_uninstall_response(PluginUninstallParams {
            plugin_id: "sample-plugin@debug".to_string(),
        })
        .await
        .expect("plugin uninstall should succeed");

    // The current-thread runtime cannot poll the detached refresh while these futures are
    // polled once. A warm cache returns immediately, so any returned snapshot must already
    // exclude the uninstalled plugin.
    if let Some(plugin_outcome) = plugins_manager
        .plugins_for_config(&plugins_input)
        .now_or_never()
    {
        assert!(
            plugin_outcome.plugins().iter().all(|plugin| {
                plugin.config_name != "sample-plugin@debug" || !plugin.is_active()
            })
        );
    }
    if let Some(skills_snapshot) = skills_service
        .snapshot_for_cwd(
            &skills_input,
            /*force_reload*/ false,
            Some(Arc::clone(&LOCAL_FS)),
        )
        .now_or_never()
    {
        assert!(
            skills_snapshot
                .outcome()
                .skills
                .iter()
                .all(|skill| { skill.name != "sample-plugin:sample-skill" })
        );
    }

    Ok(())
}
