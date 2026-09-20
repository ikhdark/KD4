use super::AnalyticsEventsClient;
use super::AnalyticsEventsDestination;
use super::AnalyticsEventsQueue;
use super::analytics_failure_log_metadata;
#[cfg(debug_assertions)]
use super::capture_track_events_request;
use super::send_track_events_request;
use super::track_event_request_batches;
use crate::events::CodexAcceptedLineFingerprintsEventParams;
use crate::events::CodexAcceptedLineFingerprintsEventRequest;
use crate::events::SkillInvocationEventParams;
use crate::events::SkillInvocationEventRequest;
use crate::events::TrackEventRequest;
use crate::facts::AnalyticsFact;
use crate::facts::InvocationType;
use codex_app_server_protocol::AccountUpdatedNotification;
use codex_app_server_protocol::AskForApproval as AppServerAskForApproval;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxPolicy as AppServerSandboxPolicy;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionSource as AppServerSessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus as AppServerThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus as AppServerTurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::cache_system_proxy_route_for_test;
use codex_login::CodexAuth;
use codex_login::default_client::create_client_pool;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use std::collections::HashSet;
#[cfg(debug_assertions)]
use std::fs;
#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(debug_assertions)]
use std::time::SystemTime;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;

#[test]
fn analytics_failure_metadata_preserves_status_and_optional_declared_length() {
    let metadata = analytics_failure_log_metadata(500, Some(20));

    assert_eq!(metadata.status, 500);
    assert_eq!(metadata.declared_body_bytes, Some(20));
    assert_eq!(
        analytics_failure_log_metadata(503, None).declared_body_bytes,
        None
    );
}

fn sample_accepted_line_fingerprint_event(thread_id: &str) -> TrackEventRequest {
    TrackEventRequest::AcceptedLineFingerprints(Box::new(
        CodexAcceptedLineFingerprintsEventRequest {
            event_type: "codex_accepted_line_fingerprints",
            event_params: CodexAcceptedLineFingerprintsEventParams {
                event_type: "codex.accepted_line_fingerprints",
                turn_id: "turn-1".to_string(),
                thread_id: thread_id.to_string(),
                product_surface: Some("codex".to_string()),
                model_slug: Some("gpt-5.1-codex".to_string()),
                completed_at: 1,
                repo_hash: None,
                accepted_added_lines: 1,
                accepted_deleted_lines: 0,
                line_fingerprints: Vec::new(),
            },
        },
    ))
}

fn sample_regular_track_event(thread_id: &str) -> TrackEventRequest {
    TrackEventRequest::SkillInvocation(SkillInvocationEventRequest {
        event_type: "skill_invocation",
        skill_id: format!("skill-{thread_id}"),
        skill_name: "doc".to_string(),
        event_params: SkillInvocationEventParams {
            product_client_id: None,
            skill_scope: None,
            plugin_id: None,
            repo_url: None,
            thread_id: Some(thread_id.to_string()),
            turn_id: Some("turn-1".to_string()),
            invoke_type: Some(InvocationType::Explicit),
            model_slug: Some("gpt-5.1-codex".to_string()),
        },
    })
}

fn test_http_clients() -> codex_http_client::RouteAwareClientPool {
    create_client_pool(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Api,
    )
}

#[cfg(debug_assertions)]
fn unique_capture_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "codex-analytics-{name}-{}-{nonce}.jsonl",
        std::process::id()
    ))
}

fn client_with_receiver() -> (AnalyticsEventsClient, mpsc::Receiver<AnalyticsFact>) {
    let (sender, receiver) = mpsc::channel(8);
    let queue = AnalyticsEventsQueue {
        sender,
        app_used_emitted_keys: Arc::new(Mutex::new(HashSet::new())),
        plugin_used_emitted_keys: Arc::new(Mutex::new(HashSet::new())),
    };
    (AnalyticsEventsClient { queue: Some(queue) }, receiver)
}

#[tokio::test]
#[cfg(debug_assertions)]
async fn pending_analytics_queue_delivers_bounded_correlations_and_recovers() {
    use crate::analytics_client_tests::sample_command_approval_request;
    use crate::analytics_client_tests::sample_initialize_fact;

    let capture_path = unique_capture_path("pending-correlations");
    let destination = AnalyticsEventsDestination::from_base_url_and_capture_file(
        "https://unused.example".to_string(),
        Some(capture_path.clone()),
    );
    let queue = AnalyticsEventsQueue::new(
        codex_login::AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        ),
        destination,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let client = AnalyticsEventsClient { queue: Some(queue) };
    // Reserve and release one slot immediately before the synchronous public call.
    // This sole producer cannot lose facts to queue throughput while testing retention.
    async fn ready(client: &AnalyticsEventsClient) {
        drop(
            client
                .queue
                .as_ref()
                .unwrap()
                .sender
                .reserve()
                .await
                .unwrap(),
        );
    }
    client.record_fact(sample_initialize_fact(7));
    client.track_response(7, RequestId::Integer(-1), sample_thread_start_response());
    for id in 0..=4096 {
        ready(&client).await;
        client.track_request(7, RequestId::Integer(id), &sample_turn_steer_request());
        ready(&client).await;
        client.track_server_request(7, &sample_command_approval_request(id, None));
    }
    // Replacing an existing correlation at capacity must preserve the latest input.
    let mut replacement = sample_turn_steer_request();
    if let ClientRequest::TurnSteer { params, .. } = &mut replacement {
        params.expected_turn_id = "replacement-turn".to_string();
    }
    ready(&client).await;
    client.track_request(7, RequestId::Integer(0), &replacement);
    ready(&client).await;
    client.track_server_request(7, &sample_command_approval_request(0, Some("replacement")));
    for id in 0..=4096 {
        ready(&client).await;
        client.track_error_response(7, RequestId::Integer(id), None);
        ready(&client).await;
        client.track_server_request_aborted(2000, RequestId::Integer(id));
    }
    ready(&client).await;
    client.track_request(7, RequestId::Integer(4096), &sample_turn_steer_request());
    ready(&client).await;
    client.track_server_request(7, &sample_command_approval_request(4096, None));
    ready(&client).await;
    client.track_error_response(7, RequestId::Integer(4096), None);
    ready(&client).await;
    client.track_server_request_aborted(2000, RequestId::Integer(4096));
    // A later emitted fact provides an observable FIFO delivery barrier.
    ready(&client).await;
    client.track_response(7, RequestId::Integer(-2), sample_thread_resume_response());
    let events = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        use std::io::Read;
        let mut file = fs::File::open(&capture_path).expect("open capture");
        let mut pending = Vec::new();
        let mut events = Vec::new();
        loop {
            file.read_to_end(&mut pending)
                .expect("read appended capture");
            let mut consumed = 0;
            let mut barrier = false;
            for (index, byte) in pending.iter().enumerate() {
                if *byte != b'\n' {
                    continue;
                }
                let mut payload: serde_json::Value =
                    serde_json::from_slice(&pending[consumed..index])
                        .expect("complete capture record");
                let serde_json::Value::Array(batch) = payload["events"].take() else {
                    panic!("events array");
                };
                for event in batch {
                    barrier |= event["event_params"]["thread_id"] == "thread-2";
                    events.push(event);
                }
                consumed = index + 1;
            }
            pending.drain(..consumed);
            if barrier {
                break events;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("queue delivers terminal facts and FIFO barrier");
    let steers = events
        .iter()
        .filter(|event| event["event_type"] == "codex_turn_steer_event")
        .collect::<Vec<_>>();
    let reviews = events
        .iter()
        .filter(|event| event["event_type"] == "codex_review_event")
        .collect::<Vec<_>>();
    assert_eq!(
        steers.len(),
        4097,
        "overflow has no event, retry after completion does"
    );
    assert_eq!(
        reviews.len(),
        4097,
        "review overflow has no event, retry recovers"
    );
    assert_eq!(
        steers[0]["event_params"]["expected_turn_id"],
        "replacement-turn"
    );
    assert!(
        reviews
            .iter()
            .all(|event| event["event_params"]["status"] == "aborted")
    );
    assert_ne!(
        reviews[0]["event_params"]["trigger"],
        reviews[1]["event_params"]["trigger"]
    );
    drop(client);
    fs::remove_file(capture_path).expect("remove capture file");
}

#[test]
#[cfg(debug_assertions)]
fn analytics_destination_uses_explicit_capture_file() {
    let capture_path = unique_capture_path("destination");
    let destination = AnalyticsEventsDestination::from_base_url_and_capture_file(
        "https://chatgpt.com/backend-api/".to_string(),
        Some(capture_path.clone()),
    );

    assert_eq!(
        destination,
        AnalyticsEventsDestination::CaptureFile {
            path: capture_path.clone()
        }
    );
    assert_eq!(
        fs::read_to_string(&capture_path).expect("read capture file"),
        ""
    );

    fs::remove_file(capture_path).expect("remove capture file");
}

#[test]
fn analytics_destination_uses_http_without_capture_file() {
    let destination = AnalyticsEventsDestination::from_base_url_and_capture_file(
        "https://chatgpt.com/backend-api/".to_string(),
        /*capture_file*/ None,
    );

    assert_eq!(
        destination,
        AnalyticsEventsDestination::Http {
            url: "https://chatgpt.com/backend-api/codex/analytics-events/events".to_string()
        }
    );
}

#[test]
#[cfg(not(debug_assertions))]
fn analytics_destination_ignores_capture_file_in_release() {
    let destination = AnalyticsEventsDestination::from_base_url_and_capture_file(
        "https://chatgpt.com/backend-api/".to_string(),
        Some(std::path::PathBuf::from("ignored.jsonl")),
    );

    assert_eq!(
        destination,
        AnalyticsEventsDestination::Http {
            url: "https://chatgpt.com/backend-api/codex/analytics-events/events".to_string()
        }
    );
}

#[tokio::test]
#[cfg(debug_assertions)]
async fn capture_file_writes_exact_serialized_request() {
    let capture_path = unique_capture_path("single");
    let destination = AnalyticsEventsDestination::CaptureFile {
        path: capture_path.clone(),
    };
    let event = sample_regular_track_event("thread-1");
    let expected_event = serde_json::to_value(&event).expect("serialize expected event");
    let auth = codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let http_clients = test_http_clients();

    send_track_events_request(&auth, &destination, &http_clients, vec![event]).await;

    let contents = fs::read_to_string(&capture_path).expect("read capture file");
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(lines[0]).expect("parse captured payload");
    assert_eq!(payload, serde_json::json!({"events": [expected_event]}));

    fs::remove_file(capture_path).expect("remove capture file");
}

#[tokio::test]
#[cfg(debug_assertions)]
async fn capture_file_writes_final_batches_as_separate_lines() {
    let capture_path = unique_capture_path("batches");
    let destination = AnalyticsEventsDestination::CaptureFile {
        path: capture_path.clone(),
    };
    let auth = codex_login::CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let events = vec![
        sample_regular_track_event("thread-1"),
        sample_accepted_line_fingerprint_event("thread-2"),
        sample_regular_track_event("thread-3"),
    ];
    let http_clients = test_http_clients();
    for batch in track_event_request_batches(events) {
        send_track_events_request(&auth, &destination, &http_clients, batch).await;
    }

    let contents = fs::read_to_string(&capture_path).expect("read capture file");
    let payloads = contents
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse capture line"))
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 3);
    assert_eq!(payloads[0]["events"][0]["skill_id"], "skill-thread-1");
    assert_eq!(
        payloads[1]["events"][0]["event_type"],
        "codex_accepted_line_fingerprints"
    );
    assert_eq!(payloads[2]["events"][0]["skill_id"], "skill-thread-3");

    fs::remove_file(capture_path).expect("remove capture file");
}

#[test]
#[cfg(debug_assertions)]
fn capture_write_failure_still_consumes_delivery() {
    let capture_path = unique_capture_path("missing-parent").join("events.jsonl");
    let destination = AnalyticsEventsDestination::CaptureFile { path: capture_path };
    let payload = crate::events::TrackEventsRequest {
        events: vec![sample_regular_track_event("thread-1")],
    };

    assert!(capture_track_events_request(&destination, &payload));
}

fn sample_turn_start_request() -> ClientRequest {
    ClientRequest::TurnStart {
        request_id: RequestId::Integer(1),
        params: TurnStartParams {
            thread_id: "thread-1".to_string(),
            client_user_message_id: None,
            input: Vec::new(),
            ..Default::default()
        },
    }
}

fn sample_turn_steer_request() -> ClientRequest {
    ClientRequest::TurnSteer {
        request_id: RequestId::Integer(2),
        params: TurnSteerParams {
            thread_id: "thread-1".to_string(),
            expected_turn_id: "turn-1".to_string(),
            client_user_message_id: None,
            input: Vec::new(),
            responsesapi_client_metadata: None,
            additional_context: None,
        },
    }
}

fn sample_thread_archive_request() -> ClientRequest {
    ClientRequest::ThreadArchive {
        request_id: RequestId::Integer(3),
        params: ThreadArchiveParams {
            thread_id: "thread-1".to_string(),
        },
    }
}

fn sample_thread(thread_id: &str) -> Thread {
    Thread {
        project_id: None,
        id: thread_id.to_string(),
        extra: None,
        session_id: format!("session-{thread_id}"),
        forked_from_id: None,
        parent_thread_id: None,
        preview: "first prompt".to_string(),
        ephemeral: false,
        history_mode: Default::default(),
        model_provider: "openai".to_string(),
        created_at: 1,
        updated_at: 2,
        recency_at: Some(2),
        status: AppServerThreadStatus::Idle,
        path: None,
        cwd: test_path_buf("/tmp").abs(),
        cli_version: "0.0.0".to_string(),
        source: AppServerSessionSource::Exec,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: None,
        turns: Vec::new(),
    }
}

fn sample_thread_start_response() -> ClientResponsePayload {
    ClientResponsePayload::ThreadStart(ThreadStartResponse {
        thread: sample_thread("thread-1"),
        model: "gpt-5".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        selected_environment: None,
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: AppServerAskForApproval::OnRequest,
        sandbox: AppServerSandboxPolicy::DangerFullAccess,
        permission_profile: None,
        active_permission_profile: None,
        reasoning_effort: None,
    })
}

fn sample_thread_resume_response() -> ClientResponsePayload {
    ClientResponsePayload::ThreadResume(ThreadResumeResponse {
        thread: sample_thread("thread-2"),
        model: "gpt-5".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        selected_environment: None,
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: AppServerAskForApproval::OnRequest,
        sandbox: AppServerSandboxPolicy::DangerFullAccess,
        permission_profile: None,
        active_permission_profile: None,
        reasoning_effort: None,
        initial_turns_page: None,
    })
}

fn sample_thread_fork_response() -> ClientResponsePayload {
    ClientResponsePayload::ThreadFork(ThreadForkResponse {
        thread: sample_thread("thread-3"),
        model: "gpt-5".to_string(),
        model_provider: "openai".to_string(),
        service_tier: None,
        cwd: test_path_buf("/tmp").abs(),
        selected_environment: None,
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: AppServerAskForApproval::OnRequest,
        sandbox: AppServerSandboxPolicy::DangerFullAccess,
        permission_profile: None,
        active_permission_profile: None,
        reasoning_effort: None,
    })
}

fn sample_turn_start_response() -> ClientResponsePayload {
    ClientResponsePayload::TurnStart(TurnStartResponse {
        turn: Turn {
            id: "turn-1".to_string(),
            items_view: codex_app_server_protocol::TurnItemsView::Full,
            items: Vec::new(),
            status: AppServerTurnStatus::InProgress,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            timing: None,
            surfaced_result: None,
        },
    })
}

fn sample_turn_steer_response() -> ClientResponsePayload {
    ClientResponsePayload::TurnSteer(TurnSteerResponse {
        turn_id: "turn-2".to_string(),
    })
}

#[test]
fn track_request_only_enqueues_analytics_relevant_requests() {
    let (client, mut receiver) = client_with_receiver();

    for (request_id, request) in [
        (RequestId::Integer(1), sample_turn_start_request()),
        (RequestId::Integer(2), sample_turn_steer_request()),
    ] {
        client.track_request(/*connection_id*/ 7, request_id, &request);
        assert!(matches!(
            receiver.try_recv(),
            Ok(AnalyticsFact::ClientRequest { .. })
        ));
    }

    let ignored_request = sample_thread_archive_request();
    client.track_request(
        /*connection_id*/ 7,
        RequestId::Integer(3),
        &ignored_request,
    );
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn track_response_only_enqueues_analytics_relevant_responses() {
    let (client, mut receiver) = client_with_receiver();

    for (request_id, response) in [
        (RequestId::Integer(1), sample_thread_start_response()),
        (RequestId::Integer(2), sample_thread_resume_response()),
        (RequestId::Integer(3), sample_thread_fork_response()),
        (RequestId::Integer(4), sample_turn_start_response()),
        (RequestId::Integer(5), sample_turn_steer_response()),
    ] {
        client.track_response(/*connection_id*/ 7, request_id, response);
        assert!(matches!(
            receiver.try_recv(),
            Ok(AnalyticsFact::ClientResponse { .. })
        ));
    }

    client.track_response(
        /*connection_id*/ 7,
        RequestId::Integer(6),
        ClientResponsePayload::ThreadArchive(ThreadArchiveResponse {}),
    );
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn ignored_notifications_are_not_enqueued() {
    let (client, mut receiver) = client_with_receiver();
    let notification = ServerNotification::AccountUpdated(AccountUpdatedNotification {
        auth_mode: None,
        plan_type: None,
    });

    client.track_notification(&notification);
    client.track_notification(&notification);

    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

    let item = codex_app_server_protocol::ThreadItem::Sleep {
        id: "sleep".to_string(),
        duration_ms: 10,
    };
    client.track_notification(&ServerNotification::ItemStarted(
        codex_app_server_protocol::ItemStartedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            started_at_ms: 1,
            item: item.clone(),
        },
    ));
    client.track_notification(&ServerNotification::ItemCompleted(
        codex_app_server_protocol::ItemCompletedNotification {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            completed_at_ms: 11,
            item,
        },
    ));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn usage_deduplication_survives_queue_overflow_and_capacity_duplicates() {
    use crate::analytics_client_tests::sample_plugin_metadata;
    use crate::analytics_client_tests::test_tracking_context;
    use crate::facts::AppInvocation;
    use crate::facts::CustomAnalyticsFact;

    let (client, mut receiver) = client_with_receiver();
    let tracking = test_tracking_context("thread", "turn");
    let app = || AppInvocation {
        connector_id: Some("calendar".to_string()),
        app_name: None,
        invocation_type: None,
    };
    for id in 0..8 {
        client.track_error_response(7, RequestId::Integer(id), None);
    }
    client.track_app_used(tracking.clone(), app());
    client.track_plugin_used(tracking.clone(), sample_plugin_metadata());
    for _ in 0..8 {
        assert!(matches!(
            receiver.try_recv(),
            Ok(AnalyticsFact::ErrorResponse { .. })
        ));
    }
    client.track_app_used(tracking.clone(), app());
    client.track_plugin_used(tracking.clone(), sample_plugin_metadata());
    assert!(
        matches!(receiver.try_recv(), Ok(AnalyticsFact::Custom(CustomAnalyticsFact::AppUsed(input))) if input.tracking.turn_id == "turn")
    );
    assert!(
        matches!(receiver.try_recv(), Ok(AnalyticsFact::Custom(CustomAnalyticsFact::PluginUsed(input))) if input.tracking.turn_id == "turn")
    );

    for id in 1..super::ANALYTICS_EVENT_DEDUPE_MAX_KEYS {
        let tracking = test_tracking_context("thread", &format!("turn-{id}"));
        client.track_app_used(tracking.clone(), app());
        client.track_plugin_used(tracking, sample_plugin_metadata());
        assert!(matches!(
            receiver.try_recv(),
            Ok(AnalyticsFact::Custom(CustomAnalyticsFact::AppUsed(_)))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(AnalyticsFact::Custom(CustomAnalyticsFact::PluginUsed(_)))
        ));
    }
    client.track_app_used(tracking.clone(), app());
    client.track_plugin_used(tracking, sample_plugin_metadata());
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[tokio::test]
async fn queued_facts_are_sent_in_bounded_fifo_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;
    let queue = AnalyticsEventsQueue::new(
        codex_login::AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        ),
        AnalyticsEventsDestination::Http { url: server.uri() },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let client = AnalyticsEventsClient { queue: Some(queue) };
    // No await until all facts are queued: the worker then sees one bounded drain.
    for id in 0..33 {
        client.track_app_used(
            crate::analytics_client_tests::test_tracking_context("thread", &format!("turn-{id}")),
            crate::facts::AppInvocation {
                connector_id: Some("calendar".to_string()),
                app_name: None,
                invocation_type: None,
            },
        );
    }
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let requests = server.received_requests().await.expect("requests");
            if requests.len() == 2 {
                break requests;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two batches delivered");
    let batches: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("payload"))
        .collect();
    assert_eq!(batches[0]["events"].as_array().unwrap().len(), 32);
    assert_eq!(batches[1]["events"].as_array().unwrap().len(), 1);
    let turns: Vec<_> = batches
        .iter()
        .flat_map(|batch| batch["events"].as_array().unwrap())
        .map(|event| event["event_params"]["turn_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        turns,
        (0..33).map(|id| format!("turn-{id}")).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn analytics_request_uses_effective_proxy_route() {
    let proxy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&proxy)
        .await;
    let url = "http://analytics-events.test/codex/analytics-events/events".to_string();
    cache_system_proxy_route_for_test(&url, proxy.uri());
    let http_clients = create_client_pool(
        HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy),
        ClientRouteClass::Api,
    );
    let destination = AnalyticsEventsDestination::Http { url };

    send_track_events_request(
        &CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        &destination,
        &http_clients,
        vec![sample_regular_track_event("thread-proxy")],
    )
    .await;

    assert_eq!(
        proxy
            .received_requests()
            .await
            .expect("proxy requests")
            .len(),
        1
    );
}

#[test]
fn track_event_request_batches_only_isolates_accepted_line_fingerprint_events() {
    let batches = track_event_request_batches(vec![
        sample_regular_track_event("thread-1"),
        sample_regular_track_event("thread-2"),
        sample_accepted_line_fingerprint_event("thread-3"),
        sample_accepted_line_fingerprint_event("thread-4"),
        sample_regular_track_event("thread-5"),
        sample_regular_track_event("thread-6"),
    ]);

    assert_eq!(batches.len(), 4);
    assert_eq!(batches[0].len(), 2);
    assert_eq!(batches[1].len(), 1);
    assert_eq!(batches[2].len(), 1);
    assert_eq!(batches[3].len(), 2);
    assert!(batches[1][0].should_send_in_isolated_request());
    assert!(batches[2][0].should_send_in_isolated_request());
}
