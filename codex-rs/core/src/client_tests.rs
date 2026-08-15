use super::AuthRequestTelemetryContext;
use super::CanonicalPrefixHash;
use super::CompactConversationRequestSettings;
use super::LastResponse;
use super::MODEL_ATTEMPT_RECONCILIATION_TOLERANCE_BYTES;
use super::ModelAttemptGuard;
use super::ModelAttemptOffsets;
use super::ModelClient;
use super::ModelRequestMeasurements;
use super::PendingUnauthorizedRetry;
use super::Prompt;
use super::PromptContextBaseline;
use super::UnauthorizedRecoveryExecution;
use super::WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION;
use super::WebsocketCachePublicationPermit;
use super::WebsocketHistoryBaseline;
use super::WebsocketSession;
use super::WebsocketTransportCache;
use super::X_CODEX_INSTALLATION_ID_HEADER;
use super::X_CODEX_PARENT_THREAD_ID_HEADER;
use super::X_CODEX_TURN_METADATA_HEADER;
use super::X_CODEX_WINDOW_ID_HEADER;
use super::X_OPENAI_SUBAGENT_HEADER;
use super::attempt_offsets_are_nondecreasing;
use super::new_attempt_id;
use super::new_sampling_request_id;
use crate::AttestationContext;
use crate::AttestationProvider;
use crate::GenerateAttestationFuture;
use crate::context::PromptProvenanceSidecar;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::stable_context::StableContextManifest;
use crate::test_support::TestCodexResponsesRequestKind;
use crate::test_support::responses_metadata as test_responses_metadata;
use crate::tool_history::ToolHistorySubstitution;
use codex_api::AgentIdentityTelemetry;
use codex_api::ApiError;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesWsRequest;
use codex_api::TransportError;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider::BearerAuthProvider;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_model_provider_info::create_oss_provider_with_base_url;
use codex_otel::ModelAttemptRequestKind;
use codex_otel::ModelAttemptRetryReason;
use codex_otel::ModelAttemptTransport;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::auth::AuthMode;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::InferenceTraceAttempt;
use codex_rollout_trace::InferenceTraceContext;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::RolloutTrace;
use codex_rollout_trace::TraceWriter;
use codex_rollout_trace::replay_bundle;
use codex_utils_output_truncation::approx_token_count;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::Visit;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const TEST_CHATGPT_ID_TOKEN: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJlbWFpbF92ZXJpZmllZCI6dHJ1ZSwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfdXNlcl9pZCI6InVzZXItMTIzNDUiLCJ1c2VyX2lkIjoidXNlci0xMjM0NSIsImNoYXRncHRfcGxhbl90eXBlIjoicHJvIiwiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC0xMjMifX0.c2ln";
const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn test_model_client(session_source: SessionSource) -> ModelClient {
    test_model_client_with_thread_id(ThreadId::new(), session_source)
}

fn test_model_client_with_thread_id(
    thread_id: ThreadId,
    session_source: SessionSource,
) -> ModelClient {
    let provider = create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    ModelClient::new(
        /*auth_manager*/ None,
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id,
        provider,
        session_source,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*item_ids_enabled*/ false,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

fn websocket_test_model_client() -> ModelClient {
    let mut provider =
        create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    provider.supports_websockets = true;
    ModelClient::new(
        /*auth_manager*/ None,
        AgentIdentityAuthPolicy::JwtOnly,
        ThreadId::new(),
        provider,
        SessionSource::Cli,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*item_ids_enabled*/ false,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

#[test]
fn websocket_stream_retries_when_another_session_already_activated_http_fallback() {
    let client = websocket_test_model_client();
    let telemetry = test_session_telemetry();
    let model_info = test_model_info();
    let mut first_session = client.new_session();
    let mut concurrent_session = client.new_session();
    first_session.last_stream_was_websocket = true;
    concurrent_session.last_stream_was_websocket = true;

    assert!(first_session.try_switch_fallback_transport(&telemetry, &model_info));
    assert!(concurrent_session.try_switch_fallback_transport(&telemetry, &model_info));
    assert!(!concurrent_session.last_stream_was_websocket);
    assert!(!concurrent_session.try_switch_fallback_transport(&telemetry, &model_info));
}

#[test]
fn lane2_websocket_cache_denies_speculative_publication() {
    let client = websocket_test_model_client();
    let normal_session = client.new_session();
    let speculative_session = client.new_speculative_session();

    assert!(normal_session.websocket_cache_publication.is_some());
    assert!(speculative_session.websocket_cache_publication.is_none());
}

#[test]
fn lane2_websocket_cache_admits_only_one_current_publisher() {
    let cache = Arc::new(Mutex::new(WebsocketTransportCache::default()));
    let permit = WebsocketCachePublicationPermit { epoch: 0 };
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut publishers = Vec::new();

    for _ in 0..2 {
        let cache = Arc::clone(&cache);
        let barrier = Arc::clone(&barrier);
        publishers.push(std::thread::spawn(move || {
            barrier.wait();
            cache
                .lock()
                .expect("websocket transport cache lock")
                .publish_if_current(permit, WebsocketSession::default())
        }));
    }
    barrier.wait();

    let published = publishers
        .into_iter()
        .map(|publisher| publisher.join().expect("publisher thread"))
        .filter(|published| *published)
        .count();
    assert_eq!(published, 1);
    assert!(
        cache
            .lock()
            .expect("websocket transport cache lock")
            .session
            .is_some()
    );
}

#[test]
fn lane2_websocket_cache_rejects_revoked_publisher() {
    let mut cache = WebsocketTransportCache {
        epoch: 1,
        session: None,
    };

    assert!(!cache.publish_if_current(
        WebsocketCachePublicationPermit { epoch: 0 },
        WebsocketSession::default(),
    ));
    assert!(cache.session.is_none());
}

#[test]
fn lane2_websocket_cache_drops_turn_scoped_state() {
    let request = history_test_request(vec![history_test_item("warmup", Some("turn-1"))]);
    let request_prefix = CanonicalPrefixHash::from_items(&request.input).expect("prefix hash");
    let request_properties_fingerprint =
        super::responses_request_properties_fingerprint(&request).expect("request fingerprint");
    let (_last_response_tx, last_response_rx) = tokio::sync::oneshot::channel();
    let session = WebsocketSession {
        setup_fingerprint: Some(super::WebsocketSetupFingerprint([7; 32])),
        last_request: Some(request),
        last_request_history: Some(WebsocketHistoryBaseline {
            request_prefix,
            request_properties_fingerprint,
            stable_context_fingerprint: [3; 32],
            provider_response_id_established: true,
            generation: 9,
            normalization_policy_version: WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION,
        }),
        next_history_generation: 10,
        last_response_rx: Some(last_response_rx),
        last_response_from_untraced_warmup: true,
        ..WebsocketSession::default()
    };
    session.set_connection_reused(/*connection_reused*/ true);

    let cached = session.into_transport_only();

    assert_eq!(
        cached.setup_fingerprint,
        Some(super::WebsocketSetupFingerprint([7; 32]))
    );
    assert!(cached.last_request.is_none());
    assert!(cached.last_request_history.is_none());
    assert_eq!(cached.next_history_generation, 0);
    assert!(cached.last_response_rx.is_none());
    assert!(!cached.last_response_from_untraced_warmup);
    assert!(!cached.connection_reused());
}

#[test]
fn startup_prewarm_rebases_only_an_empty_stable_context_prefix() {
    let empty_request = history_test_request(Vec::new());
    let mut empty_baseline = WebsocketHistoryBaseline {
        request_prefix: CanonicalPrefixHash::from_items(&empty_request.input)
            .expect("empty prefix hash"),
        request_properties_fingerprint: super::responses_request_properties_fingerprint(
            &empty_request,
        )
        .expect("request fingerprint"),
        stable_context_fingerprint: [1; 32],
        provider_response_id_established: true,
        generation: 1,
        normalization_policy_version: WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION,
    };
    assert!(super::rebase_empty_startup_prewarm_stable_context(
        &mut empty_baseline,
        /*previous_request_input_empty*/ true,
        [2; 32],
    ));
    assert_eq!(empty_baseline.stable_context_fingerprint, [2; 32]);

    let nonempty_request =
        history_test_request(vec![history_test_item("established", Some("turn-1"))]);
    let mut nonempty_baseline = WebsocketHistoryBaseline {
        request_prefix: CanonicalPrefixHash::from_items(&nonempty_request.input)
            .expect("non-empty prefix hash"),
        request_properties_fingerprint: super::responses_request_properties_fingerprint(
            &nonempty_request,
        )
        .expect("request fingerprint"),
        stable_context_fingerprint: [1; 32],
        provider_response_id_established: true,
        generation: 1,
        normalization_policy_version: WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION,
    };
    assert!(!super::rebase_empty_startup_prewarm_stable_context(
        &mut nonempty_baseline,
        /*previous_request_input_empty*/ false,
        [2; 32],
    ));
    assert_eq!(nonempty_baseline.stable_context_fingerprint, [1; 32]);
}

fn history_test_item(text: &str, turn_id: Option<&str>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: turn_id.map(|turn_id| {
            InternalChatMessageMetadataPassthrough {
                turn_id: Some(turn_id.to_string()),
            }
        }),
    }
}

fn history_test_request(input: Vec<ResponseItem>) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "test-model".to_string(),
        instructions: "test instructions".to_string(),
        input: input.into(),
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: true,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
    }
}

fn history_test_tool_output(call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn history_test_provenance(request: &ResponsesApiRequest) -> PromptProvenanceSidecar {
    PromptProvenanceSidecar::from_assembled_items(&request.input, &StableContextManifest::default())
}

#[test]
fn model_attempt_ids_are_opaque_per_lifecycle_not_content_derived() {
    let sensitive_payload = "UNIQUE_PROMPT_SENTINEL_42";
    let sampling_request_id = new_sampling_request_id();
    let independently_created_sampling_request_id = new_sampling_request_id();
    let first_attempt_id = new_attempt_id();
    let second_attempt_id = new_attempt_id();

    assert_ne!(
        sampling_request_id,
        independently_created_sampling_request_id
    );
    assert_ne!(first_attempt_id, second_attempt_id);
    for id in [
        &sampling_request_id,
        &independently_created_sampling_request_id,
        &first_attempt_id,
        &second_attempt_id,
    ] {
        assert!(!id.contains(sensitive_payload));
        assert!(uuid::Uuid::parse_str(id).is_ok());
    }

    let request = history_test_request(vec![history_test_item(sensitive_payload, None)]);
    let measurements = ModelRequestMeasurements::for_responses_request(
        &request,
        &history_test_provenance(&request),
    )
    .expect("measure request");
    let mut first = ModelAttemptGuard::new(
        test_session_telemetry(),
        &sampling_request_id,
        0,
        ModelAttemptRetryReason::None,
        ModelAttemptRequestKind::Initial,
        ModelAttemptTransport::ResponsesHttp,
        measurements.clone(),
        super::ModelAttemptClock::new(),
    );
    let mut retry = ModelAttemptGuard::new(
        test_session_telemetry(),
        &sampling_request_id,
        1,
        ModelAttemptRetryReason::Unauthorized,
        ModelAttemptRequestKind::Initial,
        ModelAttemptTransport::ResponsesHttp,
        measurements,
        super::ModelAttemptClock::new(),
    );
    assert_eq!(first.sampling_request_id, retry.sampling_request_id);
    assert_ne!(first.attempt_id, retry.attempt_id);
    first.emitted = true;
    retry.emitted = true;
}

#[test]
fn model_request_measurements_count_serialized_tools_independently() {
    let without_tools = history_test_request(vec![history_test_item("input", None)]);
    let mut with_tools = without_tools.clone();
    with_tools.tools = Some(vec![json!({
        "type": "function",
        "name": "lookup",
        "description": "A deliberately verbose schema for directional token counting",
        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
    })]);

    let baseline = ModelRequestMeasurements::for_responses_request(
        &without_tools,
        &history_test_provenance(&without_tools),
    )
    .expect("measure request without tools");
    let measured = ModelRequestMeasurements::for_responses_request(
        &with_tools,
        &history_test_provenance(&with_tools),
    )
    .expect("measure request with tools");
    let serialized_tools = serde_json::to_string(with_tools.tools.as_ref().unwrap()).unwrap();

    assert_eq!(baseline.tool_token_count, 0);
    assert!(measured.tool_token_count > baseline.tool_token_count);
    assert_eq!(
        measured.tool_token_count,
        i64::try_from(approx_token_count(&serialized_tools)).expect("tool token count fits in i64")
    );
    assert_ne!(measured.tool_token_count, 123_456);
}

#[test]
fn model_request_measurements_reconcile_and_match_serialized_wire_payload() {
    let request = history_test_request(vec![history_test_item(r#"input with escaping: \""#, None)]);
    let measured = ModelRequestMeasurements::for_responses_request(
        &request,
        &history_test_provenance(&request),
    )
    .expect("measure request");
    let final_payload = serde_json::to_vec(&request).expect("serialize final request");
    let classified = measured.base_instructions_bytes
        + measured.tool_schemas_bytes
        + measured.conversation_history_bytes
        + measured.current_input_bytes
        + measured.repository_context_bytes
        + measured.memory_bytes
        + measured.skills_bytes;
    let reconciled =
        classified + measured.other_injected_context_bytes + measured.envelope_overhead_bytes;

    assert_eq!(measured.wire_request_bytes, final_payload.len() as u64);
    assert_eq!(measured.logical_request_bytes, final_payload.len() as u64);
    assert_eq!(
        measured.reconciliation_residual_bytes,
        measured.logical_request_bytes as i64 - reconciled as i64
    );
    assert!(
        measured.reconciliation_residual_bytes.abs()
            <= MODEL_ATTEMPT_RECONCILIATION_TOLERANCE_BYTES
    );
    assert_eq!(measured.conversation_history_bytes, 0);
    assert!(measured.current_input_bytes > 0);
    assert_eq!(measured.other_injected_context_bytes > 0, true);
}

#[test]
fn prompt_context_hashes_track_categories_and_gate_fixed_prefix_reuse() {
    let first_request = history_test_request(vec![history_test_item("first task", Some("turn-1"))]);
    let mut first = ModelRequestMeasurements::for_responses_request(
        &first_request,
        &history_test_provenance(&first_request),
    )
    .expect("measure first request");
    let mut baseline = None;
    first.compare_and_remember_prompt_context(&mut baseline, Some("cache-key"));
    assert!(!first.fixed_prefix_reuse_eligible);

    let second_request =
        history_test_request(vec![history_test_item("different task", Some("turn-2"))]);
    let mut second = ModelRequestMeasurements::for_responses_request(
        &second_request,
        &history_test_provenance(&second_request),
    )
    .expect("measure second request");
    second.compare_and_remember_prompt_context(&mut baseline, Some("cache-key"));
    assert!(second.fixed_prefix_reuse_eligible);
    let category = |name| {
        second
            .prompt_context_categories
            .iter()
            .find(|measurement| measurement.category == name)
            .expect("category measurement")
    };
    assert!(category("base_system").unchanged_from_previous_request);
    assert!(!category("task_input").unchanged_from_previous_request);

    let mut changed_tools = second_request;
    changed_tools.tools = Some(vec![json!({"type": "function", "name": "changed"})]);
    let mut third = ModelRequestMeasurements::for_responses_request(
        &changed_tools,
        &history_test_provenance(&changed_tools),
    )
    .expect("measure changed fixed prefix");
    third.compare_and_remember_prompt_context(&mut baseline, Some("cache-key"));
    assert!(!third.fixed_prefix_reuse_eligible);
    assert!(
        !third
            .prompt_context_categories
            .iter()
            .find(|measurement| measurement.category == "tool_schemas")
            .expect("tool schema category")
            .unchanged_from_previous_request
    );
}

#[test]
fn model_attempt_offsets_require_monotonic_elapsed_values_and_allow_nulls() {
    let minimal = ModelAttemptOffsets::default();
    assert!(attempt_offsets_are_nondecreasing(&minimal, 1));

    let ordered = ModelAttemptOffsets {
        dispatch_ready_us: 10,
        stream_established_us: Some(20),
        first_provider_event_us: Some(30),
        first_model_output_us: Some(40),
        first_visible_output_us: Some(50),
    };
    assert!(attempt_offsets_are_nondecreasing(&ordered, 60));

    let out_of_order = ModelAttemptOffsets {
        first_model_output_us: Some(9),
        ..ordered
    };
    assert!(!attempt_offsets_are_nondecreasing(&out_of_order, 60));
}

#[test]
fn websocket_prefix_hash_ignores_internal_metadata_only() {
    let first = CanonicalPrefixHash::from_items(&[history_test_item("same", Some("turn-a"))])
        .expect("hash should serialize");
    let second = CanonicalPrefixHash::from_items(&[history_test_item("same", Some("turn-b"))])
        .expect("hash should serialize");
    let visible_change =
        CanonicalPrefixHash::from_items(&[history_test_item("different", Some("turn-a"))])
            .expect("hash should serialize");

    assert_eq!(first, second);
    assert_ne!(first, visible_change);
}

#[test]
fn websocket_incremental_history_uses_digest_and_preserves_full_compare_fallback() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let original = history_test_request(vec![history_test_item("user", Some("turn-a"))]);
    session.remember_request_history(&original, [1; 32]);
    session.websocket_session.last_request = Some(original.clone());
    let response = LastResponse {
        response_id: "response-1".to_string(),
        items_added: vec![history_test_item("assistant", Some("turn-a"))],
    };
    let delta = history_test_item("next", Some("turn-b"));
    let mut extended = original.clone();
    let mut extended_input = extended.input.to_vec();
    extended_input.extend(response.items_added.clone());
    extended_input.push(delta.clone());
    extended.input = extended_input.into();

    assert_eq!(
        session.get_incremental_items(&extended, Some(&response), false),
        Some(vec![delta.clone()])
    );

    let mut changed_prefix = extended.clone();
    Arc::make_mut(&mut changed_prefix.input)[0] = history_test_item("changed", Some("turn-a"));
    assert_eq!(
        session.get_incremental_items(&changed_prefix, Some(&response), false),
        None
    );

    // Missing hash state is uncertainty, not failure: retain the canonical
    // full-materialization/full-comparison behavior.
    session.websocket_session.last_request_history = None;
    assert_eq!(
        session.get_incremental_items(&extended, Some(&response), false),
        Some(vec![delta])
    );
}

#[test]
fn websocket_incremental_history_invalidates_without_dropping_transport_contract() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let request = history_test_request(vec![history_test_item("user", None)]);
    session.remember_request_history(&request, [1; 32]);
    session.websocket_session.last_request = Some(request);
    let generation_before = session.websocket_session.next_history_generation;

    session.invalidate_incremental_history("test history replacement");

    assert!(session.websocket_session.last_request.is_none());
    assert!(session.websocket_session.last_request_history.is_none());
    assert!(session.websocket_session.next_history_generation > generation_before);
}

#[test]
fn websocket_exact_stable_prefix_inherits_existing_response_id() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let original = history_test_request(vec![history_test_item("user", None)]);
    session.remember_request_history(&original, [7; 32]);
    session.websocket_session.last_request = Some(original.clone());
    let response = LastResponse {
        response_id: "response-old".to_string(),
        items_added: vec![history_test_item("assistant", None)],
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(response.clone())
        .expect("response receiver open");
    session.websocket_session.last_response_rx = Some(receiver);
    let delta = history_test_item("next", None);
    let mut current = original;
    current.input = current
        .input
        .iter()
        .cloned()
        .chain(response.items_added)
        .chain([delta.clone()])
        .collect::<Vec<_>>()
        .into();

    let (prepared, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [7; 32],
        &[],
    );
    let ResponsesWsRequest::ResponseCreate(prepared) = prepared;
    assert_eq!(
        prepared.previous_response_id.as_deref(),
        Some("response-old")
    );
    assert_eq!(prepared.input.as_ref(), &[delta]);
}

#[test]
fn websocket_stable_replacement_rebases_without_stale_inheritance() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let old = history_test_request(vec![history_test_item("old stable", None)]);
    session.remember_request_history(&old, [1; 32]);
    session.websocket_session.last_request = Some(old);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(LastResponse {
            response_id: "response-stale".to_string(),
            items_added: Vec::new(),
        })
        .expect("response receiver open");
    session.websocket_session.last_response_rx = Some(receiver);
    let current = history_test_request(vec![history_test_item("new stable", None)]);

    let (prepared, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [2; 32],
        &[],
    );
    let ResponsesWsRequest::ResponseCreate(prepared) = prepared;
    assert!(prepared.previous_response_id.is_none());
    assert_eq!(prepared.input, current.input);
    assert!(session.websocket_session.last_request.is_none());
    assert!(session.websocket_session.last_response_rx.is_none());

    // A failed fresh replay has not installed any new response baseline, so a
    // retry remains a complete, non-inheriting replay.
    let (retry, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [2; 32],
        &[],
    );
    let ResponsesWsRequest::ResponseCreate(retry) = retry;
    assert!(retry.previous_response_id.is_none());
    assert_eq!(retry.input, current.input);
}

#[test]
fn tool_history_receipt_inside_provider_prefix_forces_transactional_rebase() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let bounded = "bounded provider-visible output";
    let receipt = r#"{"version":1,"receipt_id":"receipt"}"#;
    let original = history_test_request(vec![history_test_tool_output("call-1", bounded)]);
    session.remember_request_history(&original, [7; 32]);
    session.websocket_session.last_request = Some(original);
    session.prompt_context_baseline = Some(PromptContextBaseline {
        prompt_cache_key: Some("stale".to_string()),
        category_hashes: BTreeMap::new(),
        ordered_fixed_hashes: Vec::new(),
    });
    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(LastResponse {
            response_id: "response-stale".to_string(),
            items_added: Vec::new(),
        })
        .expect("response receiver open");
    session.websocket_session.last_response_rx = Some(receiver);
    let current = history_test_request(vec![history_test_tool_output("call-1", receipt)]);
    let substitutions = [ToolHistorySubstitution {
        item_index: 0,
        call_id: "call-1".to_string(),
        bounded_output_sha256: crate::tool_history::sha256(bounded.as_bytes()),
        receipt_id: "receipt".to_string(),
        substituted_output_sha256: crate::tool_history::sha256(receipt.as_bytes()),
    }];

    let (prepared, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [7; 32],
        &substitutions,
    );
    let ResponsesWsRequest::ResponseCreate(prepared) = prepared;
    assert!(prepared.previous_response_id.is_none());
    assert_eq!(prepared.input, current.input);
    assert!(session.websocket_session.last_request.is_none());
    assert!(session.websocket_session.last_request_history.is_none());
    assert!(session.prompt_context_baseline.is_none());
    assert!(session.websocket_cache_publication.is_none());

    // A failed receipt-bearing rebase cannot resurrect the stale provider id.
    let (retry, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [7; 32],
        &substitutions,
    );
    let ResponsesWsRequest::ResponseCreate(retry) = retry;
    assert!(retry.previous_response_id.is_none());
    assert_eq!(retry.input, current.input);
}

#[test]
fn tool_history_receipt_only_in_new_tail_keeps_proven_inheritance() {
    let client = test_model_client(SessionSource::Cli);
    let mut session = client.new_session();
    let original = history_test_request(vec![history_test_item("user", None)]);
    session.remember_request_history(&original, [7; 32]);
    session.websocket_session.last_request = Some(original.clone());
    let assistant = history_test_item("assistant", None);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(LastResponse {
            response_id: "response-proven".to_string(),
            items_added: vec![assistant.clone()],
        })
        .expect("response receiver open");
    session.websocket_session.last_response_rx = Some(receiver);
    let receipt = history_test_tool_output("call-1", "receipt tail");
    let mut current = original;
    current.input = current
        .input
        .iter()
        .cloned()
        .chain([assistant, receipt.clone()])
        .collect::<Vec<_>>()
        .into();
    let substitutions = [ToolHistorySubstitution {
        item_index: 2,
        call_id: "call-1".to_string(),
        bounded_output_sha256: crate::tool_history::sha256(b"bounded tail"),
        receipt_id: "receipt".to_string(),
        substituted_output_sha256: crate::tool_history::sha256(b"receipt tail"),
    }];

    let (prepared, _) = session.prepare_websocket_request(
        ResponseCreateWsRequest::from(&current),
        &current,
        [7; 32],
        &substitutions,
    );
    let ResponsesWsRequest::ResponseCreate(prepared) = prepared;
    assert_eq!(
        prepared.previous_response_id.as_deref(),
        Some("response-proven")
    );
    assert_eq!(prepared.input.as_ref(), &[receipt]);
}

#[tokio::test]
async fn stable_context_fallback_request_replays_complete_input() {
    let client = test_model_client(SessionSource::Cli);
    let projected = history_test_item("projected delta", None);
    let stable = history_test_item("stable prefix", None);
    let prompt = Prompt {
        input: vec![projected.clone()].into(),
        stable_context_fallback_input: vec![stable.clone(), projected.clone()].into(),
        ..Prompt::default()
    };
    let model_info = test_model_info();
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        /* turn_id */ None,
        format!("{}:0", client.state.thread_id),
        /* parent_thread_id */ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let setup = client
        .current_client_setup()
        .await
        .expect("client setup should resolve");

    let normal = client
        .build_responses_request_with_input(
            &setup.api_provider,
            &prompt,
            &model_info,
            /* effort */ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /* service_tier */ None,
            &responses_metadata,
            /* use_stable_context_fallback */ false,
        )
        .expect("normal request should build");
    let fallback = client
        .build_responses_request_with_input(
            &setup.api_provider,
            &prompt,
            &model_info,
            /* effort */ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /* service_tier */ None,
            &responses_metadata,
            /* use_stable_context_fallback */ true,
        )
        .expect("fallback request should build");

    assert_eq!(normal.input.as_ref(), std::slice::from_ref(&projected));
    assert_eq!(fallback.input.as_ref(), &[stable, projected]);
}

#[tokio::test]
async fn tool_history_fail_open_request_uses_unreplaced_input() {
    let client = test_model_client(SessionSource::Cli);
    let receipt = history_test_tool_output("call-1", "receipt-substituted");
    let bounded = history_test_tool_output("call-1", "bounded provider-visible output");
    let prompt = Prompt {
        input: vec![receipt].into(),
        stable_context_fallback_input: Vec::new().into(),
        tool_history_fallback_input: vec![bounded.clone()].into(),
        stable_context_tool_history_fallback_input: vec![bounded.clone()].into(),
        ..Prompt::default()
    };
    let model_info = test_model_info();
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        /* turn_id */ None,
        format!("{}:0", client.state.thread_id),
        /* parent_thread_id */ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let setup = client
        .current_client_setup()
        .await
        .expect("client setup should resolve");

    let request = client
        .build_responses_request_with_fallbacks(
            &setup.api_provider,
            &prompt,
            &model_info,
            /* effort */ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /* service_tier */ None,
            &responses_metadata,
            /* use_stable_context_fallback */ false,
            /* use_tool_history_fallback */ true,
        )
        .expect("fail-open request should build");
    assert_eq!(request.input.as_ref(), &[bounded]);
}

#[test]
fn request_schema_serialization_cache_is_keyed_by_model_visible_schema() {
    let client = test_model_client(SessionSource::Cli);
    let prompt = Prompt {
        output_schema: Some(json!({"type": "object", "properties": {"value": {"type": "string"}}})),
        ..Prompt::default()
    };

    client
        .request_schema_components(&prompt, None, /*use_responses_lite*/ false)
        .expect("first serialization should succeed");
    client
        .request_schema_components(&prompt, None, /*use_responses_lite*/ false)
        .expect("cached serialization should succeed");
    let mut changed = prompt;
    changed.output_schema = Some(json!({"type": "array"}));
    client
        .request_schema_components(&changed, None, /*use_responses_lite*/ false)
        .expect("changed schema should serialize independently");

    let cache = client
        .state
        .request_schema_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(cache.hits, 1);
    assert_eq!(cache.misses, 2);
    assert_eq!(cache.entries.len(), 2);
}

#[tokio::test]
async fn compact_uses_bearer_after_agent_identity_session_fallback() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let registration_count = Arc::new(AtomicUsize::new(0));
    let response_count = Arc::clone(&registration_count);
    Mock::given(method("POST"))
        .and(path("/v1/agent/register"))
        .respond_with(move |_request: &wiremock::Request| {
            response_count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(/*status*/ 503)
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "output": []
        })))
        .expect(/*requests*/ 1)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let auth_manager = chatgpt_auth_manager(&codex_home, server.uri()).await;
    let mut provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let thread_id = ThreadId::new();
    let client = ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::ChatGptAuth,
        thread_id,
        provider,
        SessionSource::Cli,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*item_ids_enabled*/ false,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "please compact".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }]
        .into(),
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        /*turn_id*/ None,
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );

    let output = client
        .compact_conversation_history(
            &prompt,
            &test_model_info(),
            /*turn_state*/ None,
            CompactConversationRequestSettings {
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::None,
                service_tier: None,
            },
            &test_session_telemetry(),
            &CompactionTraceContext::disabled(),
            &responses_metadata,
        )
        .await?;

    assert!(output.is_empty());
    assert_eq!(registration_count.load(Ordering::SeqCst), 3);
    let requests = server
        .received_requests()
        .await
        .expect("server should record requests");
    let compact_request = requests
        .iter()
        .find(|request| request.url.path() == "/v1/responses/compact")
        .expect("compact request should be captured");
    assert_eq!(
        compact_request
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-access-token")
    );
    assert_eq!(
        compact_request
            .headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("account-123")
    );

    Ok(())
}

fn test_model_provider() -> SharedModelProvider {
    test_model_client(SessionSource::Cli).state.provider.clone()
}

fn test_responses_metadata_for_client(
    client: &ModelClient,
    turn_id: Option<&str>,
    window_id: String,
    parent_thread_id: Option<ThreadId>,
    request_kind: TestCodexResponsesRequestKind,
) -> CodexResponsesMetadata {
    let thread_id = client.state.thread_id.to_string();
    test_responses_metadata(
        TEST_INSTALLATION_ID,
        &thread_id,
        &thread_id,
        turn_id,
        window_id,
        &client.state.session_source,
        parent_thread_id,
        request_kind,
    )
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gpt-test",
        "display_name": "gpt-test",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "base_instructions": "base instructions",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

#[test]
fn ultra_reasoning_uses_max_for_requests() {
    let mut model: serde_json::Value = serde_json::to_value(test_model_info()).unwrap();
    model["supports_reasoning_summaries"] = json!(true);
    model["default_reasoning_level"] = json!("ultra");
    model["supported_reasoning_levels"] = json!([{"effort": "ultra", "description": ""}]);
    let model: ModelInfo = serde_json::from_value(model).unwrap();
    let request_effort =
        crate::client::request_effort_for_model(&model, Some(ReasoningEffort::Ultra));
    let serialized = serde_json::to_value(ModelClient::build_reasoning(
        &model,
        request_effort,
        codex_protocol::config_types::ReasoningSummary::None,
    ))
    .unwrap();
    assert_eq!(serialized["effort"], json!("max"),);
}

#[test]
fn omitted_memory_summary_effort_stays_omitted() {
    let mut model: serde_json::Value = serde_json::to_value(test_model_info()).unwrap();
    model["supports_reasoning_summaries"] = json!(true);
    model["default_reasoning_level"] = json!("high");
    let model: ModelInfo = serde_json::from_value(model).unwrap();

    assert!(crate::client::memory_summary_reasoning(&model, None).is_none());
}

fn write_chatgpt_auth_json(codex_home: &std::path::Path) {
    let auth_json = json!({
        "tokens": {
            "id_token": TEST_CHATGPT_ID_TOKEN,
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": "account-123"
        },
        "last_refresh": "2099-01-01T00:00:00Z"
    });
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::to_string_pretty(&auth_json).expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

async fn chatgpt_auth_manager(
    codex_home: &TempDir,
    agent_identity_authapi_base_url: String,
) -> Arc<AuthManager> {
    write_chatgpt_auth_json(codex_home.path());
    let auth_manager = AuthManager::shared(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await;
    let auth = auth_manager.auth().await.expect("auth should load");
    AuthManager::from_auth_for_testing_with_agent_identity_authapi_base_url(
        auth,
        agent_identity_authapi_base_url,
    )
}

#[derive(Default)]
struct TagCollectorVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for TagCollectorVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.tags
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct TagCollectorLayer {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
}

impl<S> Layer<S> for TagCollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "feedback_tags" {
            return;
        }
        let mut visitor = TagCollectorVisitor::default();
        event.record(&mut visitor);
        self.tags.lock().unwrap().extend(visitor.tags);
    }
}

fn started_inference_attempt(temp: &TempDir) -> anyhow::Result<InferenceTraceAttempt> {
    let writer = Arc::new(TraceWriter::create(
        temp.path(),
        "trace-1".to_string(),
        "rollout-1".to_string(),
        "thread-root".to_string(),
    )?);
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "thread-root".to_string(),
        agent_path: "/root".to_string(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn-1".to_string(),
        thread_id: "thread-root".to_string(),
    })?;

    let inference_trace = InferenceTraceContext::enabled(
        writer,
        "thread-root".to_string(),
        "turn-1".to_string(),
        "gpt-test".to_string(),
        "test-provider".to_string(),
    );
    let attempt = inference_trace.start_attempt();
    attempt.record_started(&json!({
        "model": "gpt-test",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
    }));
    Ok(attempt)
}

fn output_message(id: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::with_suffix("msg", id)),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn replay_until_cancelled(temp: &TempDir) -> anyhow::Result<RolloutTrace> {
    let mut rollout = replay_bundle(temp.path())?;
    for _ in 0..50 {
        let inference = rollout
            .inference_calls
            .values()
            .next()
            .expect("inference should be reduced");
        if inference.execution.status == ExecutionStatus::Cancelled {
            return Ok(rollout);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        rollout = replay_bundle(temp.path())?;
    }
    Ok(rollout)
}

struct NotifyAfterEventStream {
    events: VecDeque<ResponseEvent>,
    yielded: usize,
    notify_after: usize,
    notify: Arc<Notify>,
}

impl futures::Stream for NotifyAfterEventStream {
    type Item = std::result::Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(event) = self.events.pop_front() else {
            return Poll::Pending;
        };
        self.yielded += 1;
        if self.yielded == self.notify_after {
            self.notify.notify_one();
        }
        Poll::Ready(Some(Ok(event)))
    }
}

#[test]
fn build_subagent_headers_sets_other_subagent_label() {
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
        "memory_consolidation".to_string(),
    )));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[test]
fn build_subagent_headers_sets_internal_memory_consolidation_label() {
    let client = test_model_client(SessionSource::Internal(
        InternalSessionSource::MemoryConsolidation,
    ));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
    assert_eq!(
        headers.get("originator"),
        Some(&http::HeaderValue::from_static("test_originator"))
    );
}

#[test]
fn build_ws_client_metadata_includes_window_lineage_and_turn_metadata() {
    let parent_thread_id = ThreadId::new();
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    }));

    let thread_id = client.state.thread_id.to_string();
    let expected_window_id = format!("{thread_id}:1");
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("turn-123"),
        expected_window_id.clone(),
        Some(parent_thread_id),
        TestCodexResponsesRequestKind::Turn,
    );
    let client_metadata =
        client.build_ws_client_metadata(&responses_metadata, /*use_responses_lite*/ false);
    let parent_thread_id = parent_thread_id.to_string();
    let turn_metadata: serde_json::Value = serde_json::from_str(
        client_metadata
            .get(X_CODEX_TURN_METADATA_HEADER)
            .expect("turn metadata"),
    )
    .expect("valid turn metadata");
    for (client_key, metadata_key, expected) in [
        (
            X_CODEX_INSTALLATION_ID_HEADER,
            "installation_id",
            "11111111-1111-4111-8111-111111111111",
        ),
        ("session_id", "session_id", thread_id.as_str()),
        ("thread_id", "thread_id", thread_id.as_str()),
        ("turn_id", "turn_id", "turn-123"),
        (
            X_CODEX_WINDOW_ID_HEADER,
            "window_id",
            expected_window_id.as_str(),
        ),
        (
            X_CODEX_PARENT_THREAD_ID_HEADER,
            "parent_thread_id",
            parent_thread_id.as_str(),
        ),
    ] {
        assert_eq!(
            client_metadata.get(client_key).map(String::as_str),
            Some(expected)
        );
        assert_eq!(turn_metadata[metadata_key].as_str(), Some(expected));
    }
    assert_eq!(
        client_metadata
            .get(X_OPENAI_SUBAGENT_HEADER)
            .map(String::as_str),
        Some("collab_spawn")
    );
}

#[tokio::test]
async fn summarize_memories_returns_empty_for_empty_input() {
    let client = test_model_client(SessionSource::Cli);
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();

    let output = client
        .summarize_memories(
            Vec::new(),
            &model_info,
            /*effort*/ None,
            &session_telemetry,
        )
        .await
        .expect("empty summarize request should succeed");
    assert_eq!(output.len(), 0);
}

#[tokio::test]
async fn dropped_response_stream_traces_cancelled_partial_output() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;

    // The provider has produced one complete output item, but no terminal
    // response.completed event. The harness has enough information to keep this
    // item in history, so the trace should preserve it when the stream is
    // abandoned.
    let item = output_message("1", "partial answer");
    let api_stream = futures::stream::iter([Ok(ResponseEvent::OutputItemDone(item))])
        .chain(futures::stream::pending());
    let (mut stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
        None,
    );

    let observed = stream
        .next()
        .await
        .expect("mapped stream should yield output item")?;
    assert!(matches!(observed, ResponseEvent::OutputItemDone(_)));

    // Dropping the consumer is how turn interruption/preemption stops polling
    // the provider stream. The mapper task observes that drop asynchronously
    // and records cancellation using the output items it has already seen.
    drop(stream);

    // Cancellation is recorded by the mapper task after Drop wakes it, so the
    // replay may need a short wait before the terminal event appears on disk.
    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[tokio::test]
async fn response_stream_records_last_model_feedback_ids() {
    let tags = Arc::new(Mutex::new(BTreeMap::new()));
    let _guard = tracing_subscriber::registry()
        .with(TagCollectorLayer { tags: tags.clone() })
        .set_default();

    let api_stream = futures::stream::iter([
        Ok(ResponseEvent::Created),
        Ok(ResponseEvent::Completed {
            response_id: "resp-123".to_string(),
            token_usage: None,
            end_turn: Some(true),
        }),
    ]);
    let (mut stream, _) = super::map_response_events(
        Some("req-123".to_string()),
        api_stream,
        test_session_telemetry(),
        InferenceTraceAttempt::disabled(),
        test_model_provider(),
        None,
    );

    while stream.next().await.is_some() {}

    let tags = tags.lock().unwrap().clone();
    assert_eq!(
        tags.get("last_model_request_id").map(String::as_str),
        Some("\"req-123\"")
    );
    assert_eq!(
        tags.get("last_model_response_id").map(String::as_str),
        Some("\"resp-123\"")
    );
}

#[tokio::test]
async fn bedrock_unauthorized_error_uses_provider_mapping() {
    let provider = create_model_provider(
        ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
        /*auth_manager*/ None,
    );
    let mut auth_recovery = None;
    let url = "https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses";
    let error = super::handle_unauthorized(
        TransportError::Http {
            status: http::StatusCode::UNAUTHORIZED,
            url: Some(url.to_string()),
            headers: None,
            body: Some(
                "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z"
                    .to_string(),
            ),
        },
        &mut auth_recovery,
        &test_session_telemetry(),
        &provider,
    )
    .await
    .expect_err("expired Bedrock signature should fail");

    assert_eq!(
        error.to_string(),
        format!(
            "Amazon Bedrock rejected the request because its AWS signature has expired. Refresh your AWS credentials and retry. If `AWS_BEARER_TOKEN_BEDROCK` is set, update or unset it, then restart Codex, url: {url}"
        )
    );
}

#[tokio::test]
async fn dropped_backpressured_response_stream_traces_cancelled_partial_output()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;
    let backpressured_item_yielded = Arc::new(Notify::new());
    let mut events = VecDeque::new();
    for _ in 0..super::RESPONSE_STREAM_CHANNEL_CAPACITY {
        events.push_back(ResponseEvent::Created);
    }
    events.push_back(ResponseEvent::OutputItemDone(output_message(
        "1",
        "partial answer",
    )));
    let api_stream = NotifyAfterEventStream {
        events,
        yielded: 0,
        notify_after: super::RESPONSE_STREAM_CHANNEL_CAPACITY + 1,
        notify: Arc::clone(&backpressured_item_yielded),
    };

    let (stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
        None,
    );

    // Fill the mapper channel with non-terminal events, then yield one output
    // item. The mapper has observed that item and is blocked trying to send it
    // downstream, so dropping the consumer covers the send-failure path rather
    // than the `consumer_dropped` select branch.
    backpressured_item_yielded.notified().await;
    drop(stream);

    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[test]
fn auth_request_telemetry_context_tracks_attached_auth_and_retry_phase() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(Some("access-token"), Some("workspace-123")),
        /*agent_identity_telemetry*/ None,
        PendingUnauthorizedRetry::from_recovery(UnauthorizedRecoveryExecution {
            mode: "managed",
            phase: "refresh_token",
        }),
    );

    assert_eq!(auth_context.auth_mode, Some("Chatgpt"));
    assert!(auth_context.auth_header_attached);
    assert_eq!(auth_context.auth_header_name, Some("authorization"));
    assert!(auth_context.retry_after_unauthorized);
    assert_eq!(auth_context.recovery_mode, Some("managed"));
    assert_eq!(auth_context.recovery_phase, Some("refresh_token"));
}

#[test]
fn auth_request_telemetry_context_tracks_agent_identity_ids() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(/*token*/ None, /*account_id*/ None),
        Some(AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        }),
        PendingUnauthorizedRetry::default(),
    );

    assert_eq!(
        auth_context.agent_identity_telemetry(),
        Some(&AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        })
    );
}

fn model_client_with_counting_attestation(
    include_attestation: bool,
) -> (ModelClient, Arc<AtomicUsize>) {
    #[derive(Debug)]
    struct CountingAttestationProvider {
        calls: Arc<AtomicUsize>,
    }

    impl AttestationProvider for CountingAttestationProvider {
        fn header_for_request(
            &self,
            _context: AttestationContext,
        ) -> GenerateAttestationFuture<'_> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Some(http::HeaderValue::from_bytes(format!("v1.header-{call}").as_bytes()).unwrap())
            })
        }
    }

    let attestation_calls = Arc::new(AtomicUsize::new(0));
    let (auth_manager, provider) = if include_attestation {
        (
            Some(AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            )),
            ModelProviderInfo::create_openai_provider(Some(CHATGPT_CODEX_BASE_URL.to_string())),
        )
    } else {
        (
            None,
            create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses),
        )
    };
    let model_client = ModelClient::new(
        auth_manager,
        AgentIdentityAuthPolicy::JwtOnly,
        ThreadId::new(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*item_ids_enabled*/ false,
        /*concurrent_reasoning_summaries_enabled*/ false,
        Some(Arc::new(CountingAttestationProvider {
            calls: attestation_calls.clone(),
        })),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    (model_client, attestation_calls)
}

#[tokio::test]
async fn websocket_handshake_includes_attestation_for_chatgpt_codex_responses() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ true);
    let responses_metadata = test_responses_metadata_for_client(
        &model_client,
        /*turn_id*/ None,
        format!("{}:0", model_client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::WebsocketConnection,
    );

    let headers = model_client
        .build_websocket_headers(&responses_metadata)
        .await;

    assert_eq!(
        headers
            .get(crate::attestation::X_OAI_ATTESTATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("v1.header-1"),
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn non_chatgpt_codex_endpoints_omit_attestation_generation() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ false);
    let mut response_headers = http::HeaderMap::new();

    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        response_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut compaction_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        compaction_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut realtime_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        realtime_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }

    assert_eq!(
        response_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        compaction_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        realtime_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 0);
}
