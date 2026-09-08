use super::*;

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
    use codex_core::config::ConfigBuilder;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

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
