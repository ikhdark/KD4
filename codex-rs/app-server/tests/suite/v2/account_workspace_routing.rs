use super::*;
use wiremock::matchers::header;

#[tokio::test]
async fn account_read_workspace_routing_is_authoritative_and_optional() -> Result<()> {
    let codex_home = TempDir::new()?;
    let server = MockServer::start().await;
    create_config_toml(
        codex_home.path(),
        CreateConfigTomlParams {
            requires_openai_auth: Some(true),
            ..Default::default()
        },
    )?;
    let config_path = codex_home.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "chatgpt_base_url = \"{}/backend-api\"\n{}",
            server.uri(),
            std::fs::read_to_string(&config_path)?
        ),
    )?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("access-chatgpt")
            .account_id(WORKSPACE_ID_ALLOWED)
            .email("user@example.com")
            .plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;
    write_models_cache(codex_home.path())?;
    let auth_path = codex_home.path().join("auth.json");
    let original_auth = std::fs::read(&auth_path)?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let route = json!({
        "id": WORKSPACE_ID_ALLOWED,
        "workspace_backend_origin": "https://chatgpt.com",
        "account_routing_override": "NO_CONSTRAINT"
    });
    let other_route = json!({
        "id": WORKSPACE_ID_DISALLOWED,
        "workspace_backend_origin": "https://other.example",
        "account_routing_override": "us"
    });
    for (status, accounts, expected_routing) in [
        (
            200,
            json!([other_route.clone(), route]),
            json!({
                "chatgptAccountId": WORKSPACE_ID_ALLOWED,
                "backendOrigin": "https://chatgpt.com",
                "accountRoutingOverride": "NO_CONSTRAINT"
            }),
        ),
        (200, json!([other_route]), serde_json::Value::Null),
        (
            200,
            json!([{"id": WORKSPACE_ID_ALLOWED}]),
            serde_json::Value::Null,
        ),
        (503, json!([]), serde_json::Value::Null),
    ] {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/backend-api/wham/accounts/check"))
            .and(header("authorization", "Bearer access-chatgpt"))
            .and(header("chatgpt-account-id", WORKSPACE_ID_ALLOWED))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "accounts": accounts
            })))
            .expect(1)
            .mount(&server)
            .await;
        let id = mcp
            .send_get_account_request(GetAccountParams {
                refresh_token: false,
            })
            .await?;
        let response = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(id)),
        )
        .await??;
        assert_eq!(
            response.result,
            json!({
                "account": {"type": "chatgpt", "email": "user@example.com", "planType": "pro"},
                "requiresOpenaiAuth": true,
                "workspaceRouting": expected_routing
            })
        );
        server.verify().await;
    }
    assert_eq!(std::fs::read(auth_path)?, original_auth);

    let id = mcp.send_logout_account_request().await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(id)),
    )
    .await??;
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/wham/accounts/check"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let id = mcp
        .send_get_account_request(GetAccountParams {
            refresh_token: false,
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(id)),
    )
    .await??;
    assert_eq!(
        response.result,
        json!({
            "account": null, "requiresOpenaiAuth": true, "workspaceRouting": null
        })
    );
    server.verify().await;
    Ok(())
}
