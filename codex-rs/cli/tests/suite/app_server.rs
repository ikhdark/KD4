use anyhow::Result;
use app_test_support::app_server_json_shutdown_event;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::codex_command;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_app_server_uses_requested_model_and_effort() -> Result<()> {
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let server = MockServer::start().await;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = 'gpt-5.1'\nmodel_provider = 'mock'\nmodel_reasoning_effort = 'low'\n\
             [model_providers.mock]\nname = 'Mock'\nbase_url = '{}/v1'\nwire_api = 'responses'\n",
            server.uri()
        ),
    )?;
    let events = [
        json!({"type":"response.created", "response":{"id":"resp-debug"}}),
        json!({"type":"response.output_item.done", "output_index":0, "item":{
            "type":"message", "id":"msg-debug", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"done"}]
        }}),
        json!({"type":"response.completed", "response":{"id":"resp-debug", "status":"completed",
            "usage":{"input_tokens":1,"input_tokens_details":null,
                "output_tokens":1,"output_tokens_details":null,"total_tokens":2}}}),
    ];
    let body = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(&server)
        .await;

    codex_command(home.path())?
        .current_dir(workspace.path())
        .timeout(std::time::Duration::from_secs(30))
        .args([
            "-c",
            "model='gpt-5.4'",
            "debug",
            "app-server",
            "send-message-v2",
            "probe",
            "-c",
            "model_reasoning_effort='high'",
        ])
        .assert()
        .success();

    let requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        1,
        "the adapter must submit exactly one model turn"
    );
    let request: serde_json::Value = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(request["model"], "gpt-5.4");
    assert_eq!(request["reasoning"]["effort"], "high");
    Ok(())
}

#[test]
fn strict_config_rejects_unknown_config_fields_for_app_server() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"
foo = "bar"
"#,
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["app-server", "--strict-config", "--listen", "off"])
        .assert()
        .failure()
        .stderr(contains("unknown configuration field"));

    Ok(())
}

#[test]
fn app_server_emits_json_info_events() -> Result<()> {
    let codex_home = TempDir::new()?;
    let event = app_server_json_shutdown_event("codex", &["app-server"], codex_home.path())?;

    assert_eq!(
        event,
        json!({
            "level": "INFO",
            "fields": {
                "message": "processor task exited",
                "exit_reason": "last_connection_closed",
                "remaining_connection_count": 0,
                "shutdown_forced": false,
            },
            "target": "codex_app_server",
        })
    );

    Ok(())
}
