use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::write_chatgpt_auth;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::OtelExporterKind;
use codex_config::types::OtelHttpProtocol;
use codex_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const SERVICE_VERSION: &str = "0.0.0-test";

fn set_metrics_exporter(config: &mut codex_core::config::Config) {
    config.otel.metrics_exporter = OtelExporterKind::OtlpHttp {
        endpoint: "http://localhost:4318".to_string(),
        headers: HashMap::new(),
        protocol: OtelHttpProtocol::Json,
        tls: None,
    };
}

#[tokio::test]
async fn app_server_analytics_config_overrides_the_default_flag() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await?;
    set_metrics_exporter(&mut config);

    for (configured, default_enabled, expected) in [
        (None, false, false),
        (None, true, true),
        (Some(false), true, false),
        (Some(true), false, true),
    ] {
        config.analytics_enabled = configured;
        let provider = codex_core::otel_init::build_provider(
            &config,
            SERVICE_VERSION,
            Some("codex-app-server"),
            default_enabled,
        )
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        // Other telemetry may still create a provider when metrics are disabled.
        let has_metrics = provider.as_ref().and_then(|otel| otel.metrics()).is_some();
        assert_eq!(
            has_metrics, expected,
            "config={configured:?}, default={default_enabled}"
        );
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct AnalyticsCapture {
    payloads: tokio::sync::watch::Sender<Vec<std::result::Result<Value, String>>>,
}

impl AnalyticsCapture {
    pub(crate) async fn mount(server: &MockServer) -> Self {
        let capture = Self {
            payloads: tokio::sync::watch::channel(Vec::new()).0,
        };
        let responder = capture.clone();
        Mock::given(method("POST"))
            .and(path("/codex/analytics-events/events"))
            .respond_with(move |request: &wiremock::Request| {
                responder.record(&request.body);
                ResponseTemplate::new(200)
            })
            .mount(server)
            .await;
        capture
    }

    fn record(&self, body: &[u8]) {
        let payload = serde_json::from_slice(body)
            .map_err(|error| format!("invalid analytics payload: {error}"));
        self.payloads.send_modify(|payloads| payloads.push(payload));
    }

    async fn wait(
        &self,
        read_timeout: Duration,
        select: impl Fn(&Value) -> Option<Value>,
    ) -> Result<Value> {
        let mut receiver = self.payloads.subscribe();
        timeout(read_timeout, async {
            loop {
                // Mark the observed version before examining it. A POST between
                // this borrow and changed() remains visible; payloads are never
                // consumed, so several selectors can observe the same event.
                {
                    let payloads = receiver.borrow_and_update();
                    for payload in payloads.iter() {
                        let payload = payload
                            .as_ref()
                            .map_err(|error| anyhow::anyhow!("{error}"))?;
                        if let Some(value) = select(payload) {
                            return Ok(value);
                        }
                    }
                }
                receiver.changed().await?;
            }
        })
        .await?
    }
}

pub(crate) async fn mount_analytics_capture(
    server: &MockServer,
    codex_home: &Path,
) -> Result<AnalyticsCapture> {
    let capture = AnalyticsCapture::mount(server).await;

    write_chatgpt_auth(
        codex_home,
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    Ok(capture)
}

pub(crate) async fn wait_for_analytics_payload(
    server: &AnalyticsCapture,
    read_timeout: Duration,
) -> Result<Value> {
    server
        .wait(read_timeout, |payload| Some(payload.clone()))
        .await
}

pub(crate) async fn wait_for_analytics_event(
    server: &AnalyticsCapture,
    read_timeout: Duration,
    event_type: &str,
) -> Result<Value> {
    wait_for_matching_analytics_event(server, read_timeout, |event| {
        event["event_type"] == event_type
    })
    .await
}

pub(crate) async fn wait_for_goal_event(
    server: &AnalyticsCapture,
    read_timeout: Duration,
    event_kind: &str,
    goal_status: &str,
) -> Result<Value> {
    wait_for_matching_analytics_event(server, read_timeout, |event| {
        event["event_type"] == "codex_goal_event"
            && event["event_params"]["event_kind"] == event_kind
            && event["event_params"]["goal_status"] == goal_status
    })
    .await
}

pub(crate) async fn wait_for_matching_analytics_event(
    server: &AnalyticsCapture,
    read_timeout: Duration,
    matches: impl Fn(&Value) -> bool,
) -> Result<Value> {
    let result = server
        .wait(read_timeout, |payload| {
            payload["events"]
                .as_array()?
                .iter()
                .find(|event| matches(event))
                .cloned()
        })
        .await;
    match result {
        Ok(event) => Ok(event),
        Err(err) => {
            let event_types = server
                .payloads
                .borrow()
                .iter()
                .filter_map(|payload| payload.as_ref().ok())
                .filter_map(|payload| payload["events"].as_array().cloned())
                .flatten()
                .filter_map(|event| {
                    let event_type = event["event_type"].as_str()?;
                    if event_type == "codex_goal_event" {
                        Some(format!(
                            "{event_type}:{}:{}",
                            event["event_params"]["event_kind"],
                            event["event_params"]["goal_status"]
                        ))
                    } else {
                        Some(event_type.to_owned())
                    }
                })
                .collect::<Vec<_>>();
            Err(anyhow::anyhow!(
                "{err}; observed analytics event types: {event_types:?}"
            ))
        }
    }
}

#[tokio::test]
async fn analytics_capture_retains_events_and_wakes_waiters() -> Result<()> {
    let server = MockServer::start().await;
    let capture = AnalyticsCapture::mount(&server).await;
    let post = |body: &'static str| {
        let address = server.address().to_owned();
        async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::net::TcpStream::connect(address).await?;
            stream.write_all(format!("POST /codex/analytics-events/events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await?;
            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            Ok::<(), anyhow::Error>(())
        }
    };
    let deadline = Duration::from_secs(5);
    timeout(deadline, post(r#"{"events":[{"event_type":"first"}]}"#)).await??;
    assert_eq!(
        wait_for_analytics_event(&capture, deadline, "first").await?["event_type"],
        "first"
    );
    let mut second = std::pin::pin!(wait_for_analytics_event(&capture, deadline, "second"));
    let mut another = std::pin::pin!(wait_for_analytics_event(&capture, deadline, "second"));
    assert!(futures::poll!(&mut second).is_pending());
    assert!(futures::poll!(&mut another).is_pending());
    timeout(deadline, post(r#"{"events":[{"event_type":"second"}]}"#)).await??;
    assert_eq!(second.await?, another.await?);
    assert_eq!(
        wait_for_analytics_event(&capture, deadline, "first").await?["event_type"],
        "first"
    );
    let error = wait_for_analytics_event(&capture, Duration::ZERO, "absent")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("first"));
    assert!(error.to_string().contains("second"));
    capture.record(b"invalid json");
    assert!(
        wait_for_analytics_event(&capture, deadline, "absent")
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid analytics payload")
    );
    Ok(())
}

pub(crate) fn thread_initialized_event(payload: &Value) -> Result<&Value> {
    let events = payload["events"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("analytics payload missing events array"))?;
    events
        .iter()
        .find(|event| event["event_type"] == "codex_thread_initialized")
        .ok_or_else(|| anyhow::anyhow!("codex_thread_initialized event should be present"))
}

pub(crate) fn assert_basic_thread_initialized_event(
    event: &Value,
    thread_id: &str,
    session_id: &str,
    expected_product_client_id: &str,
    expected_model: &str,
    initialization_mode: &str,
    expected_thread_source: &str,
) {
    assert_eq!(event["event_params"]["thread_id"], thread_id);
    assert_eq!(event["event_params"]["session_id"], session_id);
    assert_eq!(
        event["event_params"]["app_server_client"]["product_client_id"],
        expected_product_client_id
    );
    assert_eq!(
        event["event_params"]["app_server_client"]["client_name"],
        DEFAULT_CLIENT_NAME
    );
    assert_eq!(
        event["event_params"]["app_server_client"]["rpc_transport"],
        "stdio"
    );
    assert_eq!(event["event_params"]["model"], expected_model);
    assert_eq!(event["event_params"]["ephemeral"], false);
    assert_eq!(
        event["event_params"]["thread_source"],
        expected_thread_source
    );
    assert_eq!(
        event["event_params"]["subagent_source"],
        serde_json::Value::Null
    );
    assert_eq!(
        event["event_params"]["parent_thread_id"],
        serde_json::Value::Null
    );
    assert_eq!(
        event["event_params"]["initialization_mode"],
        initialization_mode
    );
    assert!(event["event_params"]["created_at"].as_u64().is_some());
}
