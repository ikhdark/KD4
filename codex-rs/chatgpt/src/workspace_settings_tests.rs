use super::*;
use codex_core::config::ConfigBuilder;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn workspace_config() -> (tempfile::TempDir, Config, CodexAuth) {
    let home = tempfile::tempdir().expect("Codex home");
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .build()
        .await
        .expect("config");
    let auth = CodexAuth::from_external_chatgpt_tokens(
        "e30.e30.signature",
        "account-a",
        Some("enterprise"),
    )
    .expect("workspace auth");
    (home, config, auth)
}

#[tokio::test]
async fn concurrent_workspace_settings_requests_share_cached_false() {
    let (_home, mut config, auth) = workspace_config().await;
    let server = MockServer::start().await;
    config.chatgpt_base_url = server.uri();
    Mock::given(method("GET"))
        .and(path("/accounts/account-a/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"beta_settings": {"enable_plugins": false}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let cache = WorkspaceSettingsCache::default();
    let (first, second) = tokio::join!(
        codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)),
        codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)),
    );
    assert_eq!((first, second), (false, false));
    assert!(
        !codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    server.verify().await;
}

#[tokio::test]
async fn workspace_settings_identity_misses_do_not_leak_or_evict_cached_false() {
    let (_home, mut config, auth) = workspace_config().await;
    let server = MockServer::start().await;
    let other_server = MockServer::start().await;
    config.chatgpt_base_url = server.uri();
    Mock::given(path("/accounts/account-a/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"beta_settings": {"enable_plugins": false}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/accounts/account-b/settings"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/accounts/account-a/settings"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&other_server)
        .await;
    let other_auth = CodexAuth::from_external_chatgpt_tokens(
        "e30.e30.signature",
        "account-b",
        Some("enterprise"),
    )
    .expect("other workspace auth");
    let cache = WorkspaceSettingsCache::default();
    assert!(
        !codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    assert!(
        codex_plugins_enabled_for_workspace_or_default(&config, Some(&other_auth), Some(&cache))
            .await
    );
    assert!(
        !codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    config.chatgpt_base_url = other_server.uri();
    assert!(
        codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    config.chatgpt_base_url = server.uri();
    assert!(
        !codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    server.verify().await;
    other_server.verify().await;
}

#[tokio::test]
async fn expired_workspace_setting_and_failure_fallback_are_not_served_from_cache() {
    let (_home, mut config, auth) = workspace_config().await;
    let server = MockServer::start().await;
    config.chatgpt_base_url = server.uri();
    let cache = WorkspaceSettingsCache::default();
    *cache.entry.write().expect("cache") = Some(CachedWorkspaceSettings {
        key: WorkspaceSettingsCacheKey {
            chatgpt_base_url: server.uri(),
            account_id: "account-a".to_string(),
        },
        expires_at: Instant::now() - Duration::from_secs(1),
        codex_plugins_enabled: false,
    });
    Mock::given(method("GET"))
        .and(path("/accounts/account-a/settings"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/accounts/account-a/settings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"beta_settings": {"enable_plugins": false}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        !codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), Some(&cache)).await
    );
    server.verify().await;
}

#[test]
fn encode_path_segment_leaves_unreserved_ascii_unchanged() {
    assert_eq!(
        encode_path_segment("account-123_ABC.~"),
        "account-123_ABC.~"
    );
}

#[test]
fn encode_path_segment_escapes_path_separators_and_spaces() {
    assert_eq!(
        encode_path_segment("account/123 with space"),
        "account%2F123%20with%20space"
    );
}

#[tokio::test]
async fn workspace_settings_fetch_preserves_disabled_values_and_fails_open() {
    let codex_home = tempfile::tempdir().expect("temporary Codex home");
    let cwd = tempfile::tempdir().expect("temporary cwd");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(cwd.path().to_path_buf()))
        .build()
        .await
        .expect("load config");
    let auth = CodexAuth::from_external_chatgpt_tokens(
        "e30.e30.signature",
        "account/123 with space",
        Some("enterprise"),
    )
    .expect("workspace auth");

    for (status, body, expected) in [
        (200, r#"{"beta_settings":{"enable_plugins":false}}"#, false),
        (503, "settings unavailable", true),
        (200, r#"{"beta_settings":{}}"#, true),
    ] {
        let server = MockServer::start().await;
        config.chatgpt_base_url = server.uri();
        Mock::given(method("GET"))
            .and(path("/accounts/account%2F123%20with%20space/settings"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), None).await,
            expected,
            "HTTP {status}: {body}"
        );
        server.verify().await;
    }
}
