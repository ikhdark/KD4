use super::Config;
use super::ConfigManager;
use super::finish_startup_config_loaders;
use codex_config::CloudConfigBundle;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::config_types::ForcedLoginMethod;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

async fn fixture() -> (tempfile::TempDir, Config) {
    let home = tempfile::tempdir().expect("temporary home");
    let manager = ConfigManager::without_managed_config_for_tests(home.path().to_path_buf());
    let mut config = manager.load_latest_config(None).await.expect("test config");
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::Ephemeral;
    (home, config)
}

#[tokio::test]
async fn startup_loaders_reuse_auth_and_completed_bundle_for_unchanged_inputs() {
    let (home, config) = fixture().await;
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let manager = ConfigManager::new_for_tests(
        home.path().to_path_buf(),
        Vec::new(),
        LoaderOverrides::without_managed_config_for_tests(),
        CloudConfigBundleLoader::new(async move {
            loader_loads.fetch_add(1, Ordering::Relaxed);
            Ok(Some(CloudConfigBundle::default()))
        }),
    );
    let bundle = manager.current_cloud_config_bundle().get().await.unwrap();
    let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
    let mut effective = config.clone();
    // Managed settings unrelated to these loaders do not require another auth load.
    effective.model_context_window = Some(123456);
    let reused =
        finish_startup_config_loaders(&manager, &effective, Some((config, Arc::clone(&auth))))
            .await;
    assert!(Arc::ptr_eq(&auth, &reused));
    assert_eq!(
        manager.current_cloud_config_bundle().get().await.unwrap(),
        bundle
    );
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn startup_loaders_rebuild_for_changed_auth_routing_or_thread_endpoint() {
    let (home, config) = fixture().await;
    let changes: [fn(&mut Config); 8] = [
        |c| c.codex_home = c.codex_home.join("different-home"),
        |c| c.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File,
        |c| c.forced_login_method = Some(ForcedLoginMethod::Api),
        |c| c.forced_chatgpt_workspace_id = Some(vec!["other-account".to_string()]),
        |c| c.chatgpt_base_url = "http://127.0.0.1:1/backend-api".to_string(),
        |c| c.respect_system_proxy = !c.respect_system_proxy,
        |c| {
            let feature = codex_features::Feature::SecretAuthStorage;
            let enabled = c.features.enabled(feature);
            c.features
                .set_enabled(feature, !enabled)
                .expect("unconstrained test features");
        },
        |c| c.experimental_thread_config_endpoint = Some("http://127.0.0.1:1".to_string()),
    ];
    for change in changes {
        let manager = ConfigManager::without_managed_config_for_tests(home.path().to_path_buf());
        let auth = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test"));
        let mut effective = config.clone();
        change(&mut effective);
        let replaced = finish_startup_config_loaders(
            &manager,
            &effective,
            Some((config.clone(), Arc::clone(&auth))),
        )
        .await;
        assert!(!Arc::ptr_eq(&auth, &replaced));
        manager
            .current_cloud_config_bundle()
            .get()
            .await
            .expect("anonymous bundle");
    }
}

#[tokio::test]
async fn startup_loaders_install_after_bootstrap_failure() {
    let (home, config) = fixture().await;
    let manager = ConfigManager::without_managed_config_for_tests(home.path().to_path_buf());
    let auth = finish_startup_config_loaders(&manager, &config, None).await;
    assert!(auth.auth_cached().is_none());
    assert!(
        manager
            .current_cloud_config_bundle()
            .get()
            .await
            .unwrap()
            .is_none()
    );
}
