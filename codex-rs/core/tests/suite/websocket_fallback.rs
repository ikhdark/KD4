use anyhow::Result;
use codex_model_provider_info::WireApi;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::require_network;
use core_test_support::responses;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_websocket_server_with_headers;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::task::AbortOnDropHandle;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::http::Method;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_recovers_first_abrupt_close_and_stays_on_http() -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("recovered", "recovered over HTTP"),
                ev_completed("resp-http-1"),
            ]),
            sse(vec![ev_completed("resp-http-2")]),
        ],
    )
    .await;
    let websocket = start_websocket_server_with_headers(vec![WebSocketConnectionConfig {
        requests: vec![
            vec![ev_completed("warmup")],
            vec![
                ev_response_created("interrupted"),
                ev_assistant_message("accepted", "accepted before disconnect"),
            ],
        ],
        response_headers: Vec::new(),
        accept_delay: None,
        close_after_requests: false,
    }])
    .await;

    // Route both transports through one provider URL using the existing scripted servers.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://{}/v1", listener.local_addr()?);
    let ws_addr = websocket.uri().trim_start_matches("ws://").to_string();
    let http_addr = server.address().to_owned();
    let upgrades = Arc::new(AtomicUsize::new(0));
    let observed_upgrades = Arc::clone(&upgrades);
    let relay = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut downstream, _) = accepted.expect("accept provider connection");
                    let ws_addr = ws_addr.clone();
                    let observed_upgrades = Arc::clone(&observed_upgrades);
                    connections.spawn(async move {
                        let mut prefix = [0_u8; 1];
                        assert_eq!(downstream.peek(&mut prefix).await.unwrap(), 1);
                        let mut upstream = if prefix[0] == b'G' {
                            observed_upgrades.fetch_add(1, Ordering::SeqCst);
                            TcpStream::connect(ws_addr).await.unwrap()
                        } else {
                            assert_eq!(prefix[0], b'P');
                            TcpStream::connect(http_addr).await.unwrap()
                        };
                        // Abrupt peer closure is intentional in this test.
                        let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                    });
                }
                result = connections.join_next(), if !connections.is_empty() => {
                    result.unwrap().expect("provider relay must not panic");
                }
            }
        }
    }));
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider.base_url = Some(base_url);
        config.model_provider.supports_websockets = true;
        config.model_provider.stream_max_retries = Some(2);
        config.model_provider.request_max_retries = Some(0);
    });
    let test = builder.build(&server).await?;
    timeout(
        Duration::from_secs(10),
        websocket.wait_for_response_batch(0, 0),
    )
    .await?;
    let disconnect = AbortOnDropHandle::new(tokio::spawn(async move {
        websocket.wait_for_response_batch(0, 1).await;
        // Drop TCP without a WebSocket close frame after accepting partial output.
        websocket.shutdown().await;
    }));
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let mut fallback_warnings = 0;
    loop {
        match timeout(Duration::from_secs(10), test.codex.next_event())
            .await??
            .msg
        {
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("Falling back from WebSockets to HTTPS") =>
            {
                assert!(
                    warning
                        .message
                        .contains("websocket closed before response.completed")
                );
                fallback_warnings += 1;
            }
            EventMsg::Error(error) => panic!("unexpected terminal error: {error:?}"),
            EventMsg::StreamError(error) => {
                panic!("must switch before retrying WebSockets: {error:?}")
            }
            EventMsg::TurnComplete(completed) => {
                assert_eq!(
                    completed.last_agent_message.as_deref(),
                    Some("recovered over HTTP")
                );
                break;
            }
            _ => {}
        }
    }
    disconnect.await?;
    assert_eq!(fallback_warnings, 1);
    test.submit_turn("second").await?;
    assert_eq!(upgrades.load(Ordering::SeqCst), 1);
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0].body_json();
    assert!(first.get("previous_response_id").is_none());
    let accepted = first["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| {
            item["role"] == "assistant"
                && item["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|part| part["text"] == "accepted before disconnect")
                })
        })
        .count();
    assert_eq!(
        accepted, 1,
        "fallback must preserve accepted output exactly once"
    );
    relay.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_on_upgrade_required_connect() -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;

    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            // If we don't treat 426 specially, the sampling loop would retry the WebSocket
            // handshake before switching to the HTTP transport.
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The startup prewarm request sees 426 and immediately switches the session to HTTP fallback,
    // so the first turn goes straight to HTTP with no additional websocket connect attempt.
    assert_eq!(websocket_attempts, 1);
    assert_eq!(http_attempts, 1);
    assert_eq!(response_mock.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_switches_to_http_after_retries_exhausted() -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let _response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("hello").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The first turn makes 3 websocket stream attempts (initial try + 2 retries),
    // after which fallback activates and the request is replayed over HTTP. Startup
    // prewarm is speculative, so cancellation can leave zero, one, or two additional
    // upgrade attempts depending on scheduler timing.
    assert!(
        (3..=5).contains(&websocket_attempts),
        "expected three turn attempts and at most two startup prewarm attempts, got {websocket_attempts}"
    );
    assert_eq!(http_attempts, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_surfaces_every_websocket_retry_stream_error() -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let _response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let TestCodex {
        codex,
        session_configured,
        cwd,
        ..
    } = builder.build(&server).await?;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.path());

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    let mut stream_error_messages = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("event stream ended unexpectedly")
            .msg;
        match event {
            EventMsg::StreamError(e) => stream_error_messages.push(e.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    let expected_stream_errors = vec!["Reconnecting... 1/2", "Reconnecting... 2/2"];
    assert_eq!(stream_error_messages.len(), expected_stream_errors.len());
    for (actual, expected) in stream_error_messages.iter().zip(expected_stream_errors) {
        let delay = actual
            .strip_prefix(&format!("{expected} (next retry in "))
            .and_then(|message| message.strip_suffix("ms)"))
            .expect("retry notice includes the millisecond backoff");
        assert!(delay.parse::<f64>()? > 0.0);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_fallback_is_sticky_across_turns() -> Result<()> {
    require_network!();

    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path_regex(".*/responses$"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config({
        let base_url = format!("{}/v1", server.uri());
        move |config| {
            config.model_provider.base_url = Some(base_url);
            config.model_provider.wire_api = WireApi::Responses;
            config.model_provider.supports_websockets = true;
            config.model_provider.stream_max_retries = Some(2);
            config.model_provider.request_max_retries = Some(0);
        }
    });
    let test = builder.build(&server).await?;

    test.submit_turn("first").await?;
    test.submit_turn("second").await?;

    let requests = server.received_requests().await.unwrap_or_default();
    let websocket_attempts = requests
        .iter()
        .filter(|req| req.method == Method::GET && req.url.path().ends_with("/responses"))
        .count();
    let http_attempts = requests
        .iter()
        .filter(|req| req.method == Method::POST && req.url.path().ends_with("/responses"))
        .count();

    // The startup prewarm sees 426 and activates the session fallback. Both turns remain on HTTP,
    // proving that no later turn re-enables WebSockets.
    assert_eq!(websocket_attempts, 1);
    assert_eq!(http_attempts, 2);
    assert_eq!(response_mock.requests().len(), 2);

    Ok(())
}
