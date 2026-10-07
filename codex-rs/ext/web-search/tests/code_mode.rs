#![allow(clippy::expect_used)]

use anyhow::Result;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::config_types::WebSearchMode;
use codex_web_search_extension::install as install_web_search_extension;
use core_test_support::require_network;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::sync::Arc;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn custom_tool_output_last_non_empty_text(req: &ResponsesRequest, call_id: &str) -> Option<String> {
    let output = req.custom_tool_call_output(call_id);
    let text = match &output["output"] {
        Value::String(text) => Some(text.as_str()),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .rfind(|text| !text.trim().is_empty()),
        _ => panic!("custom tool output should be text or content items"),
    }?;
    let text = text
        .split_once("\nOutput:\n")
        .map_or(text, |(_, body)| body.trim_start_matches('\n'));
    Some(text.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_can_call_standalone_web_search() -> Result<()> {
    assert_code_mode_standalone_web_search(WebSearchMode::Live, serde_json::json!(true)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_can_call_indexed_standalone_web_search() -> Result<()> {
    assert_code_mode_standalone_web_search(WebSearchMode::Indexed, serde_json::json!("indexed"))
        .await
}

async fn assert_code_mode_standalone_web_search(
    web_search_mode: WebSearchMode,
    expected_external_web_access: Value,
) -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/alpha/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": "Search result",
        })))
        .expect(1)
        .mount(&server)
        .await;

    responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call(
                "call-1",
                "exec",
                r#"
const result = await resolve_tool("web.run")({
  search_query: [{ q: "standalone web search" }],
});
text(result);
"#,
            ),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let follow_up_mock = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    let auth = CodexAuth::from_api_key("dummy");
    let auth_manager = codex_core::test_support::auth_manager_from_auth(auth.clone());
    let mut extension_builder = ExtensionRegistryBuilder::<Config>::new();
    install_web_search_extension(&mut extension_builder, auth_manager);
    let mut builder = test_codex()
        .with_auth(auth)
        .with_extensions(Arc::new(extension_builder.build()))
        .with_model("test-gpt-5.1-codex")
        .with_config(move |config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
            config
                .features
                .enable(Feature::StandaloneWebSearch)
                .expect("standalone web search should be enabled");
            config
                .web_search_mode
                .set(web_search_mode)
                .expect("web search mode should be accepted");
        });
    let test = builder.build(&server).await?;

    test.submit_turn("Search the web from code mode").await?;

    let search_request = server
        .received_requests()
        .await
        .expect("received requests should be available")
        .into_iter()
        .find(|request| request.url.path() == "/v1/alpha/search")
        .expect("standalone search request should be sent");
    let search_body = search_request
        .body_json::<Value>()
        .expect("search request body should be JSON");
    assert_eq!(
        search_body["model"],
        serde_json::json!("test-gpt-5.1-codex")
    );
    assert_eq!(
        search_body["commands"],
        serde_json::json!({
            "search_query": [{"q": "standalone web search"}],
        })
    );
    assert_eq!(
        search_body["settings"],
        serde_json::json!({
            "allowed_callers": ["direct"],
            "external_web_access": expected_external_web_access,
        })
    );
    assert_eq!(
        custom_tool_output_last_non_empty_text(&follow_up_mock.single_request(), "call-1"),
        Some("Search result".to_string())
    );

    Ok(())
}
