use super::*;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_models_manager::manager::RefreshStrategy;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn runtime_registration_resolves_endpoint_auth_and_catalog() {
    let providers = codex_model_provider_info::built_in_model_providers(None);
    let provider = crate::create_model_provider(
        providers["amazon-bedrock-runtime"].clone(),
        Some(AuthManager::from_auth_for_testing(
            CodexAuth::BedrockApiKey(BedrockApiKeyAuth {
                api_key: "runtime-test-token".to_string(),
                region: "eu-west-1".to_string(),
            }),
        )),
    );
    let setup = provider
        .resolve_client_setup(ProviderAuthScope {
            agent_identity_policy: codex_login::auth::AgentIdentityAuthPolicy::JwtOnly,
            session_source: codex_protocol::protocol::SessionSource::Cli,
            agent_identity_session_fallback: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        setup.api_provider.base_url,
        "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1"
    );
    assert_eq!(
        provider.runtime_base_url().await.unwrap(),
        Some(setup.api_provider.base_url.clone())
    );
    assert_eq!(
        provider.api_provider().await.unwrap().base_url,
        setup.api_provider.base_url
    );
    assert_eq!(
        setup.resolved_auth.auth.to_auth_headers()[http::header::AUTHORIZATION],
        "Bearer runtime-test-token"
    );
    assert!(
        !setup
            .api_provider
            .headers
            .contains_key("x-amzn-mantle-client-agent")
    );
    assert!(!provider.capabilities().web_search);
    let manager = provider.models_manager("amazon-bedrock-runtime", std::env::temp_dir(), None);
    let presets = manager
        .list_models(
            RefreshStrategy::Offline,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .unwrap();
    assert_eq!(presets.len(), 12);
    assert_eq!(
        presets.iter().find(|model| model.is_default).unwrap().model,
        "global.openai.gpt-6-sol"
    );
    for slug in [
        "global.openai.gpt-6-sol",
        "us.openai.gpt-6-sol",
        "global.openai.gpt-6-luna",
        "us.openai.gpt-6-luna",
    ] {
        let model = manager.get_model_info(slug, &Default::default()).await;
        assert!(!model.used_fallback_model_metadata);
        assert!(!model.supports_search_tool);
        assert_eq!(
            model.base_instructions,
            codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim()
        );
    }
}

#[test]
fn bedrock_sigv4_uses_endpoint_specific_service_in_isolated_process() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "amazon_bedrock::runtime_provider_tests::bedrock_sigv4_environment_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("CODEX_BEDROCK_SIGNING_TEST", "1")
        .env("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE")
        .env("AWS_SECRET_ACCESS_KEY", "test-secret")
        .env("AWS_REGION", "us-west-2")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env_remove("AWS_BEARER_TOKEN_BEDROCK")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"));
}

#[tokio::test]
#[ignore = "Runs in a child process with isolated AWS environment variables"]
async fn bedrock_sigv4_environment_worker() {
    assert_eq!(std::env::var("CODEX_BEDROCK_SIGNING_TEST").unwrap(), "1");
    for (info, service, underscore_header) in [
        (
            ModelProviderInfo::create_amazon_bedrock_provider(None),
            "bedrock-mantle",
            false,
        ),
        (
            ModelProviderInfo::create_amazon_bedrock_runtime_provider(None),
            "bedrock",
            true,
        ),
    ] {
        let provider = crate::create_model_provider(info, None);
        let setup = provider
            .resolve_client_setup(ProviderAuthScope {
                agent_identity_policy: codex_login::auth::AgentIdentityAuthPolicy::JwtOnly,
                session_source: codex_protocol::protocol::SessionSource::Cli,
                agent_identity_session_fallback: Default::default(),
            })
            .await
            .unwrap();
        let mut request = codex_http_client::Request::new(
            http::Method::POST,
            format!("{}/responses", setup.api_provider.base_url),
        );
        request
            .headers
            .insert("session_id", http::HeaderValue::from_static("test-session"));
        let signed = setup.resolved_auth.auth.apply_auth(request).await.unwrap();
        assert!(
            signed.headers[http::header::AUTHORIZATION]
                .to_str()
                .unwrap()
                .contains(&format!("/us-west-2/{service}/aws4_request"))
        );
        assert_eq!(signed.headers.contains_key("session_id"), underscore_header);
    }
}
