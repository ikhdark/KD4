use super::*;
use codex_core::skills::SkillsLoadInput;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::LOCAL_FS;
use codex_login::CodexAuth;
use core_test_support::load_default_config_for_test;
use futures::FutureExt;
use tempfile::TempDir;
use tokio::sync::mpsc;

#[tokio::test]
async fn remote_local_version_requires_matching_materialized_package() {
    async fn assert_version(home: &Path, plugin: &mut PluginSummary, version: Option<&str>) {
        // Seed a stale value to prove rejection clears it, not merely leaves None untouched.
        plugin.local_version = Some("stale-local-version".to_string());
        let mut expected = plugin.clone();
        expected.local_version = version.map(str::to_string);
        hydrate_remote_plugin_local_versions(home, vec![&mut *plugin])
            .await
            .expect("read real local package evidence");
        assert_eq!(
            *plugin, expected,
            "only the evidenced local version may change"
        );
    }

    let home = TempDir::new().unwrap();
    let store = codex_core_plugins::store::PluginStore::new(home.path().to_path_buf());
    let id = PluginId::parse("sample@openai-curated-remote").unwrap();
    let root = store.plugin_root(&id, "0.9.0");
    std::fs::create_dir_all(root.join(".codex-plugin").as_path()).unwrap();
    let manifest_path = root.join(".codex-plugin/plugin.json");
    std::fs::write(
        manifest_path.as_path(),
        r#"{"name":"sample","version":"0.8.0"}"#,
    )
    .unwrap();
    store
        .write_remote_plugin_id(&id, "plugins~Plugin_expected")
        .unwrap();
    let mut plugin = PluginSummary {
        id: id.as_key(),
        remote_plugin_id: Some("plugins~Plugin_expected".to_string()),
        version: Some("9.9.9".to_string()),
        local_version: None,
        name: "sample".to_string(),
        share_context: None,
        source: PluginSource::Remote,
        installed: true,
        enabled: true,
        install_policy: PluginInstallPolicy::Available,
        install_policy_source: None,
        auth_policy: codex_app_server_protocol::PluginAuthPolicy::OnUse,
        availability: PluginAvailability::Available,
        interface: None,
        keywords: Vec::new(),
    };

    // PluginSummary documents the materialized package version, so its manifest
    // wins over both the cache directory's label and the backend's advertised version.
    assert_version(home.path(), &mut plugin, Some("0.8.0")).await;

    // The same cache key cannot attest the version of a different backend plugin.
    store
        .write_remote_plugin_id(&id, "plugins~Plugin_other")
        .unwrap();
    assert_version(home.path(), &mut plugin, None).await;
    let metadata_path = store
        .plugin_base_root(&id)
        .join(".codex-remote-plugin-install.json");
    std::fs::write(metadata_path.as_path(), "{").unwrap();
    assert_version(home.path(), &mut plugin, None).await;

    store
        .write_remote_plugin_id(&id, "plugins~Plugin_expected")
        .unwrap();
    std::fs::write(
        manifest_path.as_path(),
        r#"{"name":"another-plugin","version":"0.8.0"}"#,
    )
    .unwrap();
    assert_version(home.path(), &mut plugin, None).await;

    // A legacy manifest without a version can use its installer's concrete cache
    // version, but the Store's "local" placeholder is not a package version.
    std::fs::write(manifest_path.as_path(), r#"{"name":"sample"}"#).unwrap();
    assert_version(home.path(), &mut plugin, Some("0.9.0")).await;
    let unversioned_root = store.plugin_root(&id, "local");
    std::fs::rename(root.as_path(), unversioned_root.as_path()).unwrap();
    assert_version(home.path(), &mut plugin, None).await;
    std::fs::write(
        unversioned_root.join(".codex-plugin/plugin.json").as_path(),
        r#"{"name":"sample","version":"0.7.0"}"#,
    )
    .unwrap();
    assert_version(home.path(), &mut plugin, Some("0.7.0")).await;

    // This adapter must not reinterpret versions already supplied by the local source owner.
    plugin.source = PluginSource::Local {
        path: AbsolutePathBuf::try_from(home.path()).unwrap(),
    };
    plugin.local_version = Some("local-source-version".to_string());
    let expected = plugin.clone();
    hydrate_remote_plugin_local_versions(home.path(), vec![&mut plugin])
        .await
        .expect("local summaries bypass remote hydration");
    assert_eq!(plugin, expected);
}

#[test]
fn remote_catalog_jsonrpc_error_preserves_typed_recovery_data() {
    let error = remote_plugin_catalog_error_to_jsonrpc(
        codex_core_plugins::remote::RemotePluginCatalogError::UnexpectedStatus {
            url: "https://example.test/plugins".to_string(),
            status: http::StatusCode::FORBIDDEN,
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
fn remote_uninstall_cache_failure_is_committed_and_schedules_reconciliation() {
    assert_eq!(
        remote_plugin_uninstall_effects(&Err(RemotePluginCatalogError::CacheRemove(
            "injected cache failure".to_string(),
        ))),
        RemotePluginUninstallEffects {
            track_success: true,
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

    // Also assert the asynchronous path: a cache miss is not evidence that the
    // refreshed consumer actually excludes the removed plugin and its skill.
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    assert!(
        plugin_outcome
            .plugins()
            .iter()
            .all(|plugin| { plugin.config_name != "sample-plugin@debug" || !plugin.is_active() })
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
            .all(|skill| { skill.name != "sample-plugin:sample-skill" })
    );

    Ok(())
}

#[test]
fn authoritative_auth_requirements_survive_missing_catalog_metadata() {
    let ids = vec![codex_plugin::AppConnectorId("missing".to_string())];
    let apps = connectors::connectors_for_plugin_apps(Vec::new(), &ids);
    let summaries = plugin_apps_needing_auth(&apps, &[], &ids, true);
    assert_eq!(
        summaries,
        vec![AppSummary {
            id: "missing".to_string(),
            name: "missing".to_string(),
            description: None,
            install_url: Some("https://chatgpt.com/apps/missing/missing".to_string()),
            category: None,
        }]
    );
}
