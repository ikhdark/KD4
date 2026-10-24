use super::*;
use codex_core::config::ConfigBuilder;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test]
async fn workspace_dot_segments_are_rejected_without_network_work() {
    let (_home, mut config, _) = workspace_config().await;
    let server = MockServer::start().await;
    config.chatgpt_base_url = server.uri();
    for id in [".", ".."] {
        let auth = CodexAuth::from_external_chatgpt_tokens("e30.e30.signature", id, Some("enterprise")).unwrap();
        assert!(codex_plugins_enabled_for_workspace(&config, Some(&auth), None).await.is_err());
        assert!(codex_plugins_enabled_for_workspace_or_default(&config, Some(&auth), None).await);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn active_refresh_outcomes_are_key_scoped_bounded_and_cancellation_safe() {
    let cache = WorkspaceSettingsCache::default();
    let key = |id: &str| WorkspaceSettingsCacheKey { chatgpt_base_url: "https://example.test".into(), account_id: id.into() };
    let a = cache.active_refresh(&key("a")).unwrap();
    let a_waiter = cache.active_refresh(&key("a")).unwrap();
    let b = cache.active_refresh(&key("b")).unwrap();
    *a.try_lock().unwrap() = Some(Err("a failed".into()));
    *b.try_lock().unwrap() = Some(Ok(false));
    assert_eq!(a_waiter.try_lock().unwrap().as_ref(), Some(&Err("a failed".into())));
    drop(a);
    drop(a_waiter);
    assert!(cache.active_refresh(&key("a")).unwrap().try_lock().unwrap().is_none());
    // An abandoned fetch leaves None and releases its async mutex for a waiter.
    let cancelled = cache.active_refresh(&key("cancelled")).unwrap();
    let waiter = cache.active_refresh(&key("cancelled")).unwrap();
    drop(cancelled.try_lock().unwrap());
    drop(cancelled);
    assert!(waiter.try_lock().unwrap().is_none());
    let retained: Vec<_> = (0..MAX_ACTIVE_REFRESH_KEYS).filter_map(|i| cache.active_refresh(&key(&i.to_string()))).collect();
    assert!(cache.active_refresh(&key("overflow")).is_none());
    drop(retained);
    assert!(cache.active_refresh(&key("overflow")).is_some());
}

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
async fn concurrent_waiters_share_a_failed_refresh_but_later_calls_retry() {
    let (_home, mut config, auth) = workspace_config().await;
    let server = MockServer::start().await;
    config.chatgpt_base_url = server.uri();
    // One slow failure for three queued callers, then one retry by a later call.
    Mock::given(method("GET"))
        .and(path("/accounts/account-a/settings"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(200)))
        .expect(2)
        .mount(&server)
        .await;
    let cache = WorkspaceSettingsCache::default();
    let (first, second, third) = tokio::join!(
        codex_plugins_enabled_for_workspace(&config, Some(&auth), Some(&cache)),
        codex_plugins_enabled_for_workspace(&config, Some(&auth), Some(&cache)),
        codex_plugins_enabled_for_workspace(&config, Some(&auth), Some(&cache)),
    );
    let errors = [first, second, third].map(|result| result.expect_err("HTTP 503").to_string());
    assert_eq!(
        errors,
        ["Request failed with status 503 Service Unavailable: "; 3].map(str::to_string)
    );
    assert!(
        codex_plugins_enabled_for_workspace(&config, Some(&auth), Some(&cache))
            .await
            .is_err()
    );
    server.verify().await;
}

#[tokio::test]
async fn interleaved_workspace_keys_do_not_erase_waiting_callers_failures() {
    for b_status in [200, 503] {
        let (_home, mut config, a) = workspace_config().await;
        let b = CodexAuth::from_external_chatgpt_tokens("e30.e30.signature", "account-b", Some("enterprise")).unwrap();
        let server = MockServer::start().await;
        config.chatgpt_base_url = server.uri();
        Mock::given(path("/accounts/account-a/settings"))
            .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(50)))
            .expect(2).mount(&server).await;
        Mock::given(path("/accounts/account-b/settings"))
            .respond_with(ResponseTemplate::new(b_status).set_body_json(serde_json::json!({"beta_settings": {}})))
            .expect(1).mount(&server).await;
        let cache = WorkspaceSettingsCache::default();
        let (a1, _, a2) = tokio::join!(
            codex_plugins_enabled_for_workspace(&config, Some(&a), Some(&cache)),
            codex_plugins_enabled_for_workspace(&config, Some(&b), Some(&cache)),
            codex_plugins_enabled_for_workspace(&config, Some(&a), Some(&cache)),
        );
        assert_eq!(a1.unwrap_err().to_string(), a2.unwrap_err().to_string());
        assert_eq!(server.received_requests().await.unwrap().iter()
            .filter(|request| request.url.path() == "/accounts/account-a/settings").count(), 1);
        assert!(codex_plugins_enabled_for_workspace(&config, Some(&a), Some(&cache)).await.is_err());
        server.verify().await;
    }
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
fn encode_path_segment_preserves_unreserved_bytes_and_escapes_other_bytes() {
    for (input, expected) in [
        ("account-123_ABC.~", "account-123_ABC.~"),
        ("account/123 with space", "account%2F123%20with%20space"),
        ("é?#%", "%C3%A9%3F%23%25"),
        ("", ""),
    ] {
        assert_eq!(encode_path_segment(input), expected, "{input:?}");
    }
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
