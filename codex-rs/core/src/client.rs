//! Session- and turn-scoped helpers for talking to model provider APIs.
//!
//! `ModelClient` is intended to live for the lifetime of a Codex session and holds the stable
//! configuration and state needed to talk to a provider (auth, provider selection, conversation id,
//! and transport fallback state).
//!
//! Per-turn settings (model selection, reasoning controls, telemetry context, and turn metadata)
//! are passed explicitly to streaming and unary methods so that the turn lifetime is visible at the
//! call site.
//!
//! A [`ModelClientSession`] is created per turn and is used to stream one or more Responses API
//! requests during that turn. It caches a Responses WebSocket connection (opened lazily) and stores
//! per-turn state such as the `x-codex-turn-state` token used for sticky routing.
//!
//! WebSocket prewarm is a v2-only `response.create` with `generate=false`; it waits for completion
//! so the next request can reuse the same connection and `previous_response_id`.
//!
//! Turn execution performs prewarm as a best-effort step before the first stream request so the
//! subsequent request can reuse the same connection.
//!
//! ## Retry-Budget Tradeoff
//!
//! WebSocket prewarm is treated as the first websocket connection attempt for a turn. If it
//! fails, normal stream retry/fallback logic handles recovery on the same turn.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use codex_api::AgentIdentityTelemetry;
use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider as ApiProvider;
use codex_api::RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE;
use codex_api::Reasoning;
use codex_api::ReasoningContext;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient as ApiResponsesClient;
use codex_api::ResponsesOptions as ApiResponsesOptions;
use codex_api::ResponsesWebsocketClient as ApiWebSocketResponsesClient;
use codex_api::ResponsesWebsocketConnection as ApiWebSocketConnection;
use codex_api::ResponsesWsRequest;
use codex_api::SharedAuthProvider;
use codex_api::SseCleanupOutcome;
use codex_api::SsePollPhase;
use codex_api::SseTelemetry;
use codex_api::StreamOptions;
use codex_api::TextControls;
use codex_api::TransportError;
use codex_api::WebsocketTelemetry;
pub use codex_api::X_CODEX_TURN_STATE_HEADER;
use codex_api::auth_header_telemetry;
use codex_api::build_session_headers;
use codex_api::create_text_param_for_request;
use codex_api::response_create_client_metadata;
use codex_client::TransportPhaseObservation;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::RefreshTokenError;
use codex_login::UnauthorizedRecovery;
use codex_otel::ModelAttemptOutcome;
use codex_otel::ModelAttemptProviderBaseline;
use codex_otel::ModelAttemptRequestKind;
use codex_otel::ModelAttemptRetryReason;
use codex_otel::ModelAttemptTelemetry;
use codex_otel::ModelAttemptTransport;
use codex_otel::ModelContextComponentTelemetry;
use codex_otel::ModelPromptContextCategoryTelemetry;
use codex_otel::ModelToolSchemaTelemetry;
use codex_otel::SessionTelemetry;
use codex_otel::current_span_w3c_trace_context;
use codex_protocol::auth::AuthMode;

use crate::stable_context::StableContextManifest;
use crate::stable_context::short_hash;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnTimingRequestTokenCategories;
use codex_protocol::protocol::TurnTimingTokenCategoryBasis;
use codex_protocol::protocol::W3cTraceContext;
use codex_tools::create_tools_json_for_responses_api;
use codex_utils_output_truncation::approx_token_count;
use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use futures::StreamExt;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use http::StatusCode;
use http::StatusCode as HttpStatusCode;
use lru::LruCache;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::instrument;
use tracing::trace;
use tracing::warn;
use uuid::Uuid;

use crate::attestation::AttestationContext;
use crate::attestation::AttestationProvider;
use crate::attestation::X_OAI_ATTESTATION_HEADER;
use crate::client_common::Prompt;
use crate::client_common::PromptDigests;
use crate::client_common::ResponseAttemptIdentity;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::context::PromptContextBreakdown;
use crate::context::PromptContextCategory;
use crate::context::PromptContextMeasurement;
use crate::context::PromptProvenanceSidecar;
use crate::feedback::emit_feedback_auth_recovery_tags;
use crate::feedback_tags;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::turn_timing::ModelAttemptGenerationMetadata;
use crate::turn_timing::TurnLocalPhase;
use crate::turn_timing::TurnTimingGuard;
use crate::turn_timing::TurnTimingState;
use crate::turn_timing::response_event_records_actionable_output;
use crate::turn_timing::response_event_records_model_content;
use crate::turn_timing::response_event_records_model_output;
use crate::turn_timing::response_event_records_visible_output;
use codex_feedback::FeedbackRequestTags;
use codex_feedback::emit_feedback_request_tags_with_auth_env;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_login::auth_env_telemetry::AuthEnvTelemetry;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_model_provider::AgentIdentitySessionFallback;
use codex_model_provider::ProviderAuthScope;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::extract_response_debug_context_from_api_error;
use codex_response_debug_context::telemetry_api_error_message;
use codex_response_debug_context::telemetry_transport_error_message;

pub const OPENAI_BETA_HEADER: &str = "OpenAI-Beta";
pub use codex_api::X_CODEX_INSTALLATION_ID_HEADER;
pub const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
pub const X_CODEX_PARENT_THREAD_ID_HEADER: &str = "x-codex-parent-thread-id";
pub const X_CODEX_WINDOW_ID_HEADER: &str = "x-codex-window-id";
pub const X_CODEX_ROUTING_HINT_HEADER: &str = "x-codex-routing-hint";
pub const X_OPENAI_SUBAGENT_HEADER: &str = "x-openai-subagent";
pub const X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER: &str =
    "x-responsesapi-include-timing-metrics";
const X_CODEX_WS_STREAM_REQUEST_START_MS_CLIENT_METADATA_KEY: &str =
    "x-codex-ws-stream-request-start-ms";
const WS_REQUEST_HEADER_RESPONSES_LITE_CLIENT_METADATA_KEY: &str =
    "ws_request_header_x_openai_internal_codex_responses_lite";
const X_OPENAI_INTERNAL_CODEX_RESPONSES_LITE_HEADER: &str =
    "x-openai-internal-codex-responses-lite";
const RESPONSES_ENDPOINT: &str = "/responses";
/// Independent component serialization adds small JSON wrappers that are represented as envelope
/// overhead in the final request. This is a diagnostic bound, not a request-building requirement.
const MODEL_ATTEMPT_RECONCILIATION_TOLERANCE_BYTES: i64 = 256;
const MATCHED_TASK_CACHE_COVERAGE_BASELINE_BPS: u64 = 8_500;

#[derive(Debug, Clone, Default)]
struct ModelRequestMeasurements {
    tool_token_count: i64,
    wire_request_bytes: u64,
    logical_request_bytes: u64,
    base_instructions_bytes: u64,
    tool_schemas_bytes: u64,
    conversation_history_bytes: u64,
    current_input_bytes: u64,
    repository_context_bytes: u64,
    skills_bytes: u64,
    other_injected_context_bytes: u64,
    envelope_overhead_bytes: u64,
    reconciliation_residual_bytes: i64,
    prompt_context_categories: Vec<PromptContextMeasurement>,
    tool_schema_breakdown: Vec<ModelToolSchemaTelemetry>,
    full_prompt_estimated_tokens: u64,
    reusable_prompt_estimated_tokens: Option<u64>,
    fixed_prefix_reuse_eligible: bool,
    /// True once a previous request baseline existed for comparison, so an
    /// ineligible first request is not reported as changed categories.
    prompt_context_baseline_compared: bool,
    stable_context_manifest: StableContextManifest,
    logical_context_tokens: i64,
    active_component_bytes: u64,
    local_constructed_bytes: u64,
    local_reused_bytes: u64,
    local_component_reuse_count: u64,
    baseline_generation: Option<u64>,
    provider_baseline: ModelAttemptProviderBaseline,
    previous_response_id_present: bool,
    local_projection_policy_active: bool,
    /// Digest of each input item in the request that was actually sent, in
    /// order, after projection and normalization.
    ///
    /// The whole-history digest changes on every request by design, so it
    /// cannot tell an append from a rewrite. Per-item digests can.
    input_item_digests: Vec<[u8; 32]>,
    fixed_prefix_item_count: usize,
    /// Digests of the dispatched request settings; absent when the
    /// dispatched bytes were unavailable.
    request_setting_digests: Option<RequestSettingDigests>,
    /// Settings whose dispatched value differs from the preceding request's.
    changed_request_settings: Vec<&'static str>,
    /// Whether the leading fixed input items differ from the preceding
    /// request's, as compared by exact per-item digest. `None` before any
    /// comparison, so a first request reports no cause rather than a false one.
    fixed_prefix_items_changed: Option<bool>,
    /// Filled by comparison against the preceding request; absent on the first.
    history_divergence: Option<HistoryPrefixDivergence>,
    /// Representation selected at dispatch, not inferred from the manifest.
    selected_representation: Option<SelectedInputRepresentation>,
}

/// How this request's input prefix relates to the preceding request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HistoryPrefixDivergence {
    items_previous: u32,
    prefix_items_reused: u32,
    first_divergent_index: u32,
}

/// Compares two ordered input digests.
///
/// `prefix_items_reused == items_previous` means the request only appended, so
/// the provider's prefix cache could still apply. A smaller value locates the
/// index at which the prefix was rewritten or truncated.
fn prefix_divergence(previous: &[[u8; 32]], current: &[[u8; 32]]) -> HistoryPrefixDivergence {
    let reused = previous
        .iter()
        .zip(current)
        .take_while(|(previous, current)| previous == current)
        .count();
    let reused = u32::try_from(reused).unwrap_or(u32::MAX);
    HistoryPrefixDivergence {
        items_previous: u32::try_from(previous.len()).unwrap_or(u32::MAX),
        prefix_items_reused: reused,
        // The first index that differs is exactly the length of the shared
        // prefix; when nothing differs it is one past the shared items.
        first_divergent_index: reused,
    }
}

/// Digests each input item of the request that is actually sent.
fn input_item_digests(input: &[ResponseItem]) -> Vec<[u8; 32]> {
    input
        .iter()
        .map(|item| {
            let encoded = serde_json::to_vec(item).unwrap_or_default();
            let mut digest = [0_u8; 32];
            digest.copy_from_slice(&Sha256::digest(&encoded));
            digest
        })
        .collect()
}

/// Request settings that shape the provider's rendered prompt or cache
/// routing. `instructions`, `tools`, and `input` already have category and
/// per-item digests; transport fields such as `client_metadata` change per
/// request by design and are excluded.
const REQUEST_SETTING_FIELDS: [&str; 6] = [
    "model",
    "reasoning",
    "text",
    "tool_choice",
    "parallel_tool_calls",
    "service_tier",
];

type RequestSettingDigests = [Option<[u8; 32]>; REQUEST_SETTING_FIELDS.len()];

/// Request sections rendered ahead of every input item, in wire order.
///
/// These are the only categories whose whole-request hash can invalidate the
/// provider's cached prefix on its own. Everything else reaches the model as
/// input items and is compared by per-item digest over the leading fixed run,
/// so it must not be blamed for a break. Reuse eligibility and the telemetry
/// that explains it share this list so they cannot diverge.
const ORDERED_FIXED_PREFIX_SECTIONS: [PromptContextCategory; 2] = [
    PromptContextCategory::BaseSystem,
    PromptContextCategory::ToolSchemas,
];

/// Reported when the leading fixed input items changed. The individual
/// categories inside that run are not separable by whole-request hash, so the
/// run is named as one cause rather than guessing a category.
const FIXED_PREFIX_ITEMS_CATEGORY: &str = "fixed_prefix_items";

/// Digests each request setting exactly as dispatched; an omitted setting has
/// no digest.
fn request_setting_digests(
    encoded_request: &[u8],
    cancellation: &CancellationToken,
) -> serde_json::Result<RequestSettingDigests> {
    ensure_measurement_not_cancelled(Some(cancellation))?;
    let fields: BTreeMap<&str, &RawValue> = serde_json::from_slice(encoded_request)?;
    Ok(REQUEST_SETTING_FIELDS.map(|field| {
        fields
            .get(field)
            .map(|value| Sha256::digest(value.get().as_bytes()).into())
    }))
}

#[derive(Debug, Clone)]
struct PromptContextBaseline {
    prompt_cache_key: Option<String>,
    request_setting_digests: Option<RequestSettingDigests>,
    category_hashes: BTreeMap<&'static str, [u8; 32]>,
    ordered_fixed_hashes: Vec<(&'static str, [u8; 32])>,
    input_item_digests: Vec<[u8; 32]>,
    fixed_prefix_item_count: usize,
    full_prompt_estimated_tokens: u64,
}

/// Independent one-pass basis: JSON [instructions, input, tools, text]. It
/// includes logical envelopes, excludes transport/settings, and uses the same
/// published image policy as category estimates rather than encoded payloads.
fn independent_logical_prompt_estimate(request: &ResponsesApiRequest) -> serde_json::Result<u64> {
    let logical = serde_json::to_vec(&(
        &request.instructions, &request.input, &request.tools, &request.text,
    ))?;
    Ok(crate::context::content_aware_serialized_tokens(&logical))
}

impl ModelRequestMeasurements {
    async fn compare_after_predecessor(
        &mut self,
        predecessor: Option<oneshot::Receiver<Option<PromptContextBaseline>>>,
        initial_baseline: Option<PromptContextBaseline>,
        prompt_cache_key: Option<&str>,
        prompt_digests: PromptDigests,
    ) -> Option<PromptContextBaseline> {
        let mut baseline = match predecessor {
            Some(predecessor) => predecessor.await.unwrap_or(None),
            None => initial_baseline,
        };
        if self.prompt_context_categories.is_empty() {
            return None;
        }
        self.compare_and_remember_prompt_context(&mut baseline, prompt_cache_key, prompt_digests);
        baseline
    }

    fn tokens(&self, category: PromptContextCategory) -> u64 {
        self.prompt_context_categories
            .iter()
            .find(|measurement| measurement.category == category.as_str())
            .map_or(0, |measurement| measurement.estimated_tokens)
    }

    /// Whether the leading fixed input items broke the prefix. False before
    /// any baseline comparison.
    fn fixed_prefix_items_diverged(&self) -> bool {
        self.fixed_prefix_items_changed.unwrap_or(false)
    }

    fn logical_prompt_tokens(&self) -> u64 {
        self.prompt_context_categories
            .iter()
            .fold(0_u64, |total, measurement| {
                total.saturating_add(measurement.estimated_tokens)
            })
    }

    fn producer_selected_context_tokens(&self) -> u64 {
        PromptContextCategory::ALL
            .into_iter()
            .filter(|category| category.is_producer_selected_context())
            .fold(0_u64, |total, category| {
                total.saturating_add(self.tokens(category))
            })
    }

    fn density_bps(&self, numerator: u64) -> u32 {
        let logical_total = self.logical_prompt_tokens();
        if logical_total == 0 {
            return 0;
        }
        u32::try_from(
            numerator
                .saturating_mul(10_000)
                .checked_div(logical_total)
                .unwrap_or_default(),
        )
        .unwrap_or(10_000)
        .min(10_000)
    }

    fn request_token_categories(&self) -> TurnTimingRequestTokenCategories {
        let tool_outputs = self.prompt_context_categories.iter().find(|measurement| {
            measurement.category == PromptContextCategory::ToolOutputs.as_str()
        });
        let other_injected_context = [
            PromptContextCategory::AgentRole,
            PromptContextCategory::Plugins,
            PromptContextCategory::PluginCatalog,
            PromptContextCategory::AppDesktop,
            PromptContextCategory::Collaboration,
            PromptContextCategory::EnvironmentPermissions,
            PromptContextCategory::OtherInjected,
        ]
        .into_iter()
        .fold(0_u64, |total, category| {
            total.saturating_add(self.tokens(category))
        });
        let logical_total = self.logical_prompt_tokens();
        let repeated_unchanged_context = self
            .prompt_context_categories
            .iter()
            .filter(|measurement| measurement.unchanged_from_previous_request)
            .fold(0_u64, |total, measurement| {
                total.saturating_add(measurement.estimated_tokens)
            });
        // Report the same basis that decided reuse. Only the ordered request
        // sections ahead of every input item, the leading fixed input run, and
        // dispatched settings can break the provider's cached prefix; a
        // category appended after history cannot. Attributing a break to every
        // changed fixed-prefix category would name sections that sit behind
        // the divergence and were never the cause.
        let fixed_prefix_changed_categories =
            if self.prompt_context_baseline_compared && !self.fixed_prefix_reuse_eligible {
                ORDERED_FIXED_PREFIX_SECTIONS
                    .into_iter()
                    .filter(|category| {
                        self.prompt_context_categories.iter().any(|measurement| {
                            measurement.category == category.as_str()
                                && !measurement.unchanged_from_previous_request
                        })
                    })
                    .map(|category| category.as_str().to_string())
                    .chain(
                        self.fixed_prefix_items_diverged()
                            .then(|| FIXED_PREFIX_ITEMS_CATEGORY.to_string()),
                    )
                    .chain(
                        self.changed_request_settings
                            .iter()
                            .map(|field| format!("request.{field}")),
                    )
                    .collect()
            } else {
                Vec::new()
            };
        TurnTimingRequestTokenCategories {
            prompt_section_sha256: self
                .prompt_context_categories
                .iter()
                .map(|section| (section.category.to_string(), section.sha256.clone()))
                .collect(),
            accounting_basis: TurnTimingTokenCategoryBasis::FullLogicalPrompt,
            base_instructions: self.tokens(PromptContextCategory::BaseSystem),
            tool_schemas: self
                .tokens(PromptContextCategory::ToolSchemas)
                .saturating_add(self.tokens(PromptContextCategory::DiscoveredToolSchemas)),
            // Compatibility total: tool results are attributed separately by
            // name in the categories, but remain conversation history here.
            conversation_history: self
                .tokens(PromptContextCategory::History)
                .saturating_add(self.tokens(PromptContextCategory::ToolOutputs)),
            tool_output_serialized_bytes: tool_outputs.map(|measurement| measurement.serialized_bytes),
            tool_output_tokens_estimate: tool_outputs.map(|measurement| measurement.estimated_tokens),
            current_input: self.tokens(PromptContextCategory::TaskInput),
            repository_context: self.tokens(PromptContextCategory::Repository),
            skills: self
                .tokens(PromptContextCategory::Skills)
                .saturating_add(self.tokens(PromptContextCategory::SkillCatalog)),
            other_injected_context,
            logical_total,
            local_input_estimate: self.full_prompt_estimated_tokens,
            local_reconciliation_residual: signed_difference(
                self.full_prompt_estimated_tokens,
                logical_total,
            ),
            provider_input_tokens: None,
            provider_reconciliation_residual: None,
            repeated_unchanged_context,
            fixed_prefix_changed_categories,
            history_items_previous: self
                .history_divergence
                .map(|divergence| divergence.items_previous),
            history_prefix_items_reused: self
                .history_divergence
                .map(|divergence| divergence.prefix_items_reused),
            history_first_divergent_index: self
                .history_divergence
                .map(|divergence| divergence.first_divergent_index),
            // No aggregate tool-output budget runs; the fields stay for the wire schema.
            tool_output_budget_drop_count: 0,
            tool_output_budget_dropped_token_count: 0,
        }
    }

    #[cfg(test)]
    fn for_responses_request(
        request: &ResponsesApiRequest,
        provenance: &PromptProvenanceSidecar,
        base_instructions: &str,
    ) -> serde_json::Result<Self> {
        Self::for_responses_request_cancellable(
            request,
            provenance,
            base_instructions,
            /*cancellation*/ None,
            /*logical_request_bytes*/ None,
        )
    }

    fn for_responses_request_cancellable(
        request: &ResponsesApiRequest,
        provenance: &PromptProvenanceSidecar,
        base_instructions: &str,
        cancellation: Option<&CancellationToken>,
        logical_request_bytes: Option<u64>,
    ) -> serde_json::Result<Self> {
        let logical_request_bytes = match logical_request_bytes {
            Some(bytes) => bytes,
            None => serialized_len_cancellable(request, cancellation)?,
        };
        let measurement_provenance =
            if base_instructions.is_empty() || !request.instructions.is_empty() {
                provenance.clone()
            } else if request.input.first().is_some_and(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "developer"
                            && matches!(
                                content.as_slice(),
                                [ContentItem::InputText { text }] if text == base_instructions
                            )
                )
            }) {
                provenance.with_response_item_category(
                    &request.input,
                    0,
                    PromptContextCategory::BaseSystem,
                )
            } else {
                provenance.clone()
            };
        let mut context =
            PromptContextBreakdown::from_response_items(&request.input, &measurement_provenance)?;
        ensure_measurement_not_cancelled(cancellation)?;
        if !request.instructions.is_empty() {
            context.record_serialized(
                PromptContextCategory::BaseSystem,
                &serde_json::to_vec(&request.instructions)?,
            );
        }
        let mut tool_schema_breakdown =
            measure_input_tool_schemas(&request.input, None, cancellation)?;
        if let Some(tools) = request.tools.as_deref() {
            for (index, tool) in tools.iter().enumerate() {
                ensure_measurement_not_cancelled(cancellation)?;
                let serialized_tool = serde_json::to_vec(tool)?;
                context.record_serialized(PromptContextCategory::ToolSchemas, &serialized_tool);
                tool_schema_breakdown.extend(measure_tool_schemas(tool, index, &serialized_tool)?);
            }
            context.record_sequence_envelope(
                PromptContextCategory::ToolSchemas,
                tools.len(),
                b"tool_schema_array_envelope",
            );
        }
        if let Some(text) = &request.text {
            context.record_serialized(
                PromptContextCategory::OtherInjected,
                &serde_json::to_vec(text)?,
            );
        }
        Self::from_context(
            logical_request_bytes,
            independent_logical_prompt_estimate(request)?,
            context,
            tool_schema_breakdown,
            input_item_digests(&request.input),
        )
    }

    fn for_responses_request_from_encoded_cancellable(
        request: &ResponsesApiRequest,
        provenance: &PromptProvenanceSidecar,
        base_instructions: &str,
        cancellation: Option<&CancellationToken>,
        encoded_request: &[u8],
        logical_request_bytes: u64,
    ) -> serde_json::Result<Self> {
        #[derive(Deserialize)]
        struct EncodedResponsesRequest<'a> {
            #[serde(borrow)]
            instructions: Option<&'a RawValue>,
            #[serde(borrow)]
            input: Vec<&'a RawValue>,
            #[serde(borrow)]
            tools: Option<Vec<&'a RawValue>>,
        }

        ensure_measurement_not_cancelled(cancellation)?;
        let encoded: EncodedResponsesRequest<'_> = serde_json::from_slice(encoded_request)?;
        if encoded.input.len() != request.input.len()
            || encoded.tools.as_ref().map(Vec::len)
                != request.tools.as_deref().map(<[serde_json::Value]>::len)
        {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded transport request is an incremental projection",
            )));
        }
        let embedded_base_index = (!base_instructions.is_empty()
            && request.instructions.is_empty()
            && request.input.first().is_some_and(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "developer"
                            && matches!(
                                content.as_slice(),
                                [ContentItem::InputText { text }] if text == base_instructions
                            )
                )
            }))
        .then_some(0);
        let mut context = PromptContextBreakdown::from_serialized_response_items(
            &request.input,
            &encoded.input,
            provenance,
            embedded_base_index,
        )?;
        ensure_measurement_not_cancelled(cancellation)?;
        if !request.instructions.is_empty() {
            let instructions = encoded.instructions.ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "encoded request omitted logical instructions",
                ))
            })?;
            context.record_serialized(
                PromptContextCategory::BaseSystem,
                instructions.get().as_bytes(),
            );
        }
        let mut tool_schema_breakdown =
            measure_input_tool_schemas(&request.input, Some(&encoded.input), cancellation)?;
        if let (Some(tools), Some(serialized_tools)) =
            (request.tools.as_deref(), encoded.tools.as_deref())
        {
            for (index, (tool, serialized_tool)) in
                tools.iter().zip(serialized_tools.iter()).enumerate()
            {
                ensure_measurement_not_cancelled(cancellation)?;
                let serialized_tool_bytes = serialized_tool.get().as_bytes();
                context
                    .record_serialized(PromptContextCategory::ToolSchemas, serialized_tool_bytes);
                tool_schema_breakdown.extend(measure_tool_schemas_from_encoded(
                    tool,
                    index,
                    serialized_tool,
                )?);
            }
            context.record_sequence_envelope(
                PromptContextCategory::ToolSchemas,
                tools.len(),
                b"tool_schema_array_envelope",
            );
        }
        if let Some(text) = &request.text {
            context.record_serialized(
                PromptContextCategory::OtherInjected,
                &serde_json::to_vec(text)?,
            );
        }
        Self::from_context(
            logical_request_bytes,
            independent_logical_prompt_estimate(request)?,
            context,
            tool_schema_breakdown,
            encoded
                .input
                .iter()
                .map(|item| Sha256::digest(item.get().as_bytes()).into())
                .collect(),
        )
    }

    fn from_context(
        logical_request_bytes: u64,
        full_prompt_estimated_tokens: u64,
        context: PromptContextBreakdown,
        tool_schema_breakdown: Vec<ModelToolSchemaTelemetry>,
        input_item_digests: Vec<[u8; 32]>,
    ) -> serde_json::Result<Self> {
        let tool_token_count = i64::try_from(
            context
                .estimated_tokens(PromptContextCategory::ToolSchemas)
                .saturating_add(
                    context.estimated_tokens(PromptContextCategory::DiscoveredToolSchemas),
                ),
        )
        .unwrap_or(i64::MAX);
        let base_instructions_bytes = context.bytes(PromptContextCategory::BaseSystem);
        let tool_schemas_bytes = context
            .bytes(PromptContextCategory::ToolSchemas)
            .saturating_add(context.bytes(PromptContextCategory::DiscoveredToolSchemas));
        let conversation_history_bytes = context
            .bytes(PromptContextCategory::History)
            .saturating_add(context.bytes(PromptContextCategory::ToolOutputs));
        let current_input_bytes = context.bytes(PromptContextCategory::TaskInput);
        let repository_context_bytes = context.bytes(PromptContextCategory::Repository);
        let skills_bytes = context
            .bytes(PromptContextCategory::Skills)
            .saturating_add(context.bytes(PromptContextCategory::SkillCatalog));
        let other_injected_context_bytes = [
            PromptContextCategory::AgentRole,
            PromptContextCategory::Plugins,
            PromptContextCategory::PluginCatalog,
            PromptContextCategory::AppDesktop,
            PromptContextCategory::Collaboration,
            PromptContextCategory::EnvironmentPermissions,
            PromptContextCategory::OtherInjected,
        ]
        .into_iter()
        .fold(0_u64, |total, category| {
            total.saturating_add(context.bytes(category))
        });
        let classified_and_other = context.total_bytes();
        let envelope_overhead_bytes = logical_request_bytes.saturating_sub(classified_and_other);
        let reconciliation_residual_bytes = i128::from(logical_request_bytes)
            .saturating_sub(i128::from(classified_and_other))
            .saturating_sub(i128::from(envelope_overhead_bytes))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64;
        debug_assert!(
            reconciliation_residual_bytes.abs() <= MODEL_ATTEMPT_RECONCILIATION_TOLERANCE_BYTES
        );

        Ok(Self {
            tool_token_count,
            wire_request_bytes: logical_request_bytes,
            logical_request_bytes,
            base_instructions_bytes,
            tool_schemas_bytes,
            conversation_history_bytes,
            current_input_bytes,
            repository_context_bytes,
            skills_bytes,
            other_injected_context_bytes,
            envelope_overhead_bytes,
            reconciliation_residual_bytes,
            prompt_context_categories: context.measurements(),
            tool_schema_breakdown,
            full_prompt_estimated_tokens,
            reusable_prompt_estimated_tokens: None,
            fixed_prefix_reuse_eligible: false,
            prompt_context_baseline_compared: false,
            stable_context_manifest: StableContextManifest::default(),
            logical_context_tokens: 0,
            active_component_bytes: 0,
            local_constructed_bytes: 0,
            local_reused_bytes: 0,
            local_component_reuse_count: 0,
            baseline_generation: None,
            provider_baseline: ModelAttemptProviderBaseline::StatelessFull,
            previous_response_id_present: false,
            local_projection_policy_active: false,
            input_item_digests,
            fixed_prefix_item_count: context.fixed_prefix_item_count,
            request_setting_digests: None,
            changed_request_settings: Vec::new(),
            fixed_prefix_items_changed: None,
            history_divergence: None,
            selected_representation: None,
        })
    }

    fn compare_and_remember_prompt_context(
        &mut self,
        baseline: &mut Option<PromptContextBaseline>,
        prompt_cache_key: Option<&str>,
        _digests: PromptDigests,
    ) {
        self.prompt_context_baseline_compared = baseline.is_some();
        // Settings are compared only when both requests were measured from
        // dispatched bytes; unavailable bytes leave a setting unknown, not changed.
        self.changed_request_settings = baseline
            .as_ref()
            .and_then(|previous| previous.request_setting_digests.as_ref())
            .zip(self.request_setting_digests.as_ref())
            .map(|(previous, current)| {
                REQUEST_SETTING_FIELDS
                    .iter()
                    .zip(previous.iter().zip(current))
                    .filter(|(_, (previous, current))| previous != current)
                    .map(|(field, _)| *field)
                    .collect()
            })
            .unwrap_or_default();
        // A first request has no predecessor to diverge from, so it reports no
        // divergence rather than a vacuous zero.
        self.history_divergence = baseline.as_ref().map(|previous| {
            prefix_divergence(&previous.input_item_digests, &self.input_item_digests)
        });
        if let Some(previous) = baseline.as_ref() {
            for measurement in &mut self.prompt_context_categories {
                // Prepared digests precede fallback and provider formatting;
                // only the measured dispatched representation proves equality.
                measurement.unchanged_from_previous_request = previous
                        .category_hashes
                        .get(measurement.category)
                        .is_some_and(|hash| *hash == measurement.hash);
            }
        }
        let category_hashes = self
            .prompt_context_categories
            .iter()
            .map(|measurement| (measurement.category, measurement.hash))
            .collect::<BTreeMap<_, _>>();
        // Instructions and tool schemas are request sections ahead of every
        // input item. Fixed input items are compared below by exact per-item
        // digest over the leading fixed run, so whole-request category hashes
        // must not decide reuse: context appended after history (such as a
        // developer notice) would otherwise make an intact prefix look changed.
        let ordered_fixed_hashes = ORDERED_FIXED_PREFIX_SECTIONS
            .into_iter()
            .filter_map(|category| {
                category_hashes
                    .get(category.as_str())
                    .copied()
                    .map(|hash| (category.as_str(), hash))
            })
            .collect::<Vec<_>>();
        // Compared once, so eligibility and the telemetry explaining it read
        // the same result instead of recomputing it from different inputs.
        self.fixed_prefix_items_changed = baseline.as_ref().map(|previous| {
            previous.fixed_prefix_item_count != self.fixed_prefix_item_count
                || previous
                    .input_item_digests
                    .get(..previous.fixed_prefix_item_count)
                    != self.input_item_digests.get(..self.fixed_prefix_item_count)
        });
        self.fixed_prefix_reuse_eligible = baseline.as_ref().is_some_and(|previous| {
            previous.prompt_cache_key.as_deref() == prompt_cache_key
                && self.changed_request_settings.is_empty()
                && previous.ordered_fixed_hashes == ordered_fixed_hashes
                && self.fixed_prefix_items_changed == Some(false)
        });
        self.reusable_prompt_estimated_tokens = baseline
            .as_ref()
            .filter(|_| {
                self.fixed_prefix_reuse_eligible
                    && self
                        .history_divergence
                        .is_some_and(|prefix| prefix.prefix_items_reused == prefix.items_previous)
            })
            .map(|previous| {
                previous
                    .full_prompt_estimated_tokens
                    .min(self.full_prompt_estimated_tokens)
            });
        *baseline = Some(PromptContextBaseline {
            prompt_cache_key: prompt_cache_key.map(str::to_string),
            request_setting_digests: self.request_setting_digests,
            category_hashes,
            ordered_fixed_hashes,
            input_item_digests: self.input_item_digests.clone(),
            fixed_prefix_item_count: self.fixed_prefix_item_count,
            full_prompt_estimated_tokens: self.full_prompt_estimated_tokens,
        });
    }

    fn record_stable_context(
        &mut self,
        manifest: &StableContextManifest,
        baseline_generation: Option<u64>,
        provider_baseline: ModelAttemptProviderBaseline,
        previous_response_id_present: bool,
    ) {
        self.stable_context_manifest = manifest.clone();
        self.logical_context_tokens = manifest.projected_tokens();
        self.active_component_bytes = manifest.projected_bytes();
        self.local_constructed_bytes = manifest
            .components()
            .iter()
            .filter(|component| component.active && !component.local_reused)
            .map(|component| component.identity.serialized_bytes)
            .fold(0_u64, u64::saturating_add);
        self.local_reused_bytes = manifest
            .components()
            .iter()
            .filter(|component| component.active && component.local_reused)
            .map(|component| component.identity.serialized_bytes)
            .fold(0_u64, u64::saturating_add);
        self.local_component_reuse_count = u64::try_from(
            manifest
                .components()
                .iter()
                .filter(|component| component.active && component.local_reused)
                .count(),
        )
        .unwrap_or(u64::MAX);
        self.baseline_generation = baseline_generation;
        self.provider_baseline = provider_baseline;
        self.previous_response_id_present = previous_response_id_present;
        self.local_projection_policy_active =
            manifest.projection_enabled() && !manifest.fail_open();
    }

    fn expected_reusable_input_tokens(&self, input_tokens: Option<i64>) -> Option<i64> {
        let reusable = self.reusable_prompt_estimated_tokens?;
        let input = u64::try_from(input_tokens?).ok()?;
        i64::try_from(
            input
                .saturating_mul(reusable)
                .checked_div(self.full_prompt_estimated_tokens)?,
        )
        .ok()
    }

    fn matched_task_reuse_eligible(&self) -> bool {
        self.fixed_prefix_reuse_eligible
            && self.prompt_context_categories.iter().any(|measurement| {
                measurement.category == PromptContextCategory::TaskInput.as_str()
                    && measurement.unchanged_from_previous_request
            })
    }
}

fn cache_coverage_below_matched_task_baseline(
    matched_task_reuse_eligible: bool,
    outcome: ModelAttemptOutcome,
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
) -> Option<u64> {
    if !matched_task_reuse_eligible || outcome != ModelAttemptOutcome::Success {
        return None;
    }
    let input_tokens = u64::try_from(input_tokens?).ok()?;
    let cached_input_tokens = u64::try_from(cached_input_tokens?).ok()?;
    if input_tokens == 0 {
        return None;
    }
    let coverage_bps = cached_input_tokens
        .min(input_tokens)
        .saturating_mul(10_000)
        .checked_div(input_tokens)?;
    (coverage_bps < MATCHED_TASK_CACHE_COVERAGE_BASELINE_BPS).then_some(coverage_bps)
}

fn measure_input_tool_schemas(
    items: &[ResponseItem],
    serialized_items: Option<&[&RawValue]>,
    cancellation: Option<&CancellationToken>,
) -> serde_json::Result<Vec<ModelToolSchemaTelemetry>> {
    #[derive(Deserialize)]
    struct EncodedSchemas<'a> {
        #[serde(borrow)]
        tools: Vec<&'a RawValue>,
    }

    let mut measured = Vec::new();
    for (item_index, item) in items.iter().enumerate() {
        let tools = match item {
            ResponseItem::AdditionalTools { tools, .. }
            | ResponseItem::ToolSearchOutput { tools, .. } => tools,
            _ => continue,
        };
        ensure_measurement_not_cancelled(cancellation)?;
        let encoded = serialized_items
            .and_then(|items| items.get(item_index))
            .map(|item| serde_json::from_str::<EncodedSchemas<'_>>(item.get()))
            .transpose()?;
        if encoded
            .as_ref()
            .is_some_and(|encoded| encoded.tools.len() != tools.len())
        {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded input schemas do not match logical schemas",
            )));
        }
        for (index, tool) in tools.iter().enumerate() {
            ensure_measurement_not_cancelled(cancellation)?;
            if codex_tools::LoadableToolSpec::callable_names_from_value(tool).is_none() {
                continue;
            }
            if let Some(encoded) = &encoded {
                measured.extend(measure_tool_schemas_from_encoded(
                    tool,
                    index,
                    encoded.tools[index],
                )?);
            } else {
                measured.extend(measure_tool_schemas(
                    tool,
                    index,
                    &serde_json::to_vec(tool)?,
                )?);
            }
        }
    }
    Ok(measured)
}

fn measure_tool_schemas(
    tool: &serde_json::Value,
    index: usize,
    serialized_tool: &[u8],
) -> serde_json::Result<Vec<ModelToolSchemaTelemetry>> {
    let namespace = (tool.get("type").and_then(serde_json::Value::as_str) == Some("namespace"))
        .then(|| tool.get("name").and_then(serde_json::Value::as_str))
        .flatten();
    if let Some(namespace) = namespace
        && let Some(tools) = tool.get("tools").and_then(serde_json::Value::as_array)
    {
        return tools
            .iter()
            .enumerate()
            .map(|(nested_index, nested)| {
                measure_leaf_tool_schema(nested, nested_index, Some(namespace), None)
            })
            .collect();
    }
    Ok(vec![measure_leaf_tool_schema(
        tool,
        index,
        None,
        Some(serialized_tool),
    )?])
}

fn measure_tool_schemas_from_encoded(
    tool: &serde_json::Value,
    index: usize,
    serialized_tool: &RawValue,
) -> serde_json::Result<Vec<ModelToolSchemaTelemetry>> {
    let namespace = (tool.get("type").and_then(serde_json::Value::as_str) == Some("namespace"))
        .then(|| tool.get("name").and_then(serde_json::Value::as_str))
        .flatten();
    if let Some(namespace) = namespace
        && let Some(tools) = tool.get("tools").and_then(serde_json::Value::as_array)
    {
        #[derive(Deserialize)]
        struct EncodedNamespace<'a> {
            #[serde(borrow)]
            tools: Vec<&'a RawValue>,
        }

        let encoded_namespace: EncodedNamespace<'_> = serde_json::from_str(serialized_tool.get())?;
        if tools.len() != encoded_namespace.tools.len() {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "encoded namespace tools do not match logical tools",
            )));
        }
        return tools
            .iter()
            .zip(encoded_namespace.tools)
            .enumerate()
            .map(|(nested_index, (nested, serialized_nested))| {
                Ok(measure_leaf_tool_schema_from_encoded(
                    nested,
                    nested_index,
                    Some(namespace),
                    serialized_nested.get().as_bytes(),
                ))
            })
            .collect();
    }
    Ok(vec![measure_leaf_tool_schema_from_encoded(
        tool,
        index,
        None,
        serialized_tool.get().as_bytes(),
    )])
}

fn measure_leaf_tool_schema_from_encoded(
    tool: &serde_json::Value,
    index: usize,
    namespace: Option<&str>,
    serialized: &[u8],
) -> ModelToolSchemaTelemetry {
    let name = tool
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| tool.get("type").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool_{index}"));
    ModelToolSchemaTelemetry {
        name,
        namespace: namespace.map(str::to_string),
        serialized_bytes: u64::try_from(serialized.len()).unwrap_or(u64::MAX),
        approx_tokens: u64::try_from(approx_token_count(
            std::str::from_utf8(serialized).unwrap_or_default(),
        ))
        .unwrap_or(u64::MAX),
        selected_by_model: false,
    }
}

fn measure_leaf_tool_schema(
    tool: &serde_json::Value,
    index: usize,
    namespace: Option<&str>,
    serialized_tool: Option<&[u8]>,
) -> serde_json::Result<ModelToolSchemaTelemetry> {
    let owned_serialized;
    let serialized = if let Some(serialized) = serialized_tool {
        serialized
    } else {
        owned_serialized = serde_json::to_vec(tool)?;
        &owned_serialized
    };
    let name = tool
        .get("name")
        .and_then(serde_json::Value::as_str)
        .or_else(|| tool.get("type").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("tool_{index}"));
    Ok(ModelToolSchemaTelemetry {
        name,
        namespace: namespace.map(str::to_string),
        serialized_bytes: u64::try_from(serialized.len()).unwrap_or(u64::MAX),
        approx_tokens: u64::try_from(approx_token_count(
            std::str::from_utf8(serialized).unwrap_or_default(),
        ))
        .unwrap_or(u64::MAX),
        selected_by_model: false,
    })
}

fn selected_tool_schema_breakdown(
    schemas: &[ModelToolSchemaTelemetry],
    output_items: &[ResponseItem],
) -> Vec<ModelToolSchemaTelemetry> {
    let selected = output_items
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall {
                name, namespace, ..
            }
            | ResponseItem::CustomToolCall {
                name, namespace, ..
            } => Some((namespace.clone(), name.clone())),
            ResponseItem::LocalShellCall { .. } => Some((None, "local_shell".to_string())),
            ResponseItem::ToolSearchCall { .. } => Some((None, "tool_search".to_string())),
            ResponseItem::WebSearchCall { .. } => Some((None, "web_search".to_string())),
            ResponseItem::ImageGenerationCall { .. } => {
                Some((None, "image_generation".to_string()))
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    schemas
        .iter()
        .cloned()
        .map(|mut schema| {
            schema.selected_by_model = selected
                .contains(&(schema.namespace.clone(), schema.name.clone()))
                || (schema.namespace.is_none()
                    && selected.iter().any(|(namespace, name)| {
                        namespace.is_none()
                            && ((name == "web_search" && schema.name.starts_with("web_search"))
                                || (name == "image_generation"
                                    && schema.name.starts_with("image_generation")))
                    }));
            schema
        })
        .collect()
}

fn signed_difference(lhs: u64, rhs: u64) -> i64 {
    i128::from(lhs)
        .saturating_sub(i128::from(rhs))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn ensure_measurement_not_cancelled(
    cancellation: Option<&CancellationToken>,
) -> serde_json::Result<()> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "request diagnostics cancelled",
        )));
    }
    Ok(())
}

fn serialized_len_cancellable(
    value: &impl serde::Serialize,
    cancellation: Option<&CancellationToken>,
) -> serde_json::Result<u64> {
    struct CountingWriter<'a> {
        bytes: u64,
        cancellation: Option<&'a CancellationToken>,
    }

    impl std::io::Write for CountingWriter<'_> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self
                .cancellation
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "request diagnostics cancelled",
                ));
            }
            self.bytes = self
                .bytes
                .saturating_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    ensure_measurement_not_cancelled(cancellation)?;
    let mut writer = CountingWriter {
        bytes: 0,
        cancellation,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

fn new_sampling_request_id() -> String {
    Uuid::now_v7().to_string()
}

fn new_attempt_id() -> String {
    Uuid::now_v7().to_string()
}

fn new_attempt_identity(sampling_request_id: &str) -> ResponseAttemptIdentity {
    ResponseAttemptIdentity {
        sampling_request_id: sampling_request_id.to_owned(),
        physical_attempt_id: new_attempt_id(),
    }
}

#[derive(Debug)]
struct PostDispatchRequestMeasurements {
    measurements: ModelRequestMeasurements,
    stable_context_manifest: StableContextManifest,
}

/// Which of the four prepared input representations a request dispatched.
///
/// The other three are prepared and discarded, so only this one's aggregate
/// budget drops may be attributed to the request.
#[derive(Clone, Copy, Debug, Default)]
struct SelectedInputRepresentation {
    stable_context_fallback: bool,
    tool_history_fallback: bool,
}

impl SelectedInputRepresentation {
    fn as_str(self) -> &'static str {
        match (self.stable_context_fallback, self.tool_history_fallback) {
            (false, false) => "projected",
            (true, false) => "stable_context_fallback",
            (false, true) => "tool_history_fallback",
            (true, true) => "stable_context_tool_history_fallback",
        }
    }

    /// A fallback build replaced the logical input, so provenance has to be
    /// recomputed from the items that were actually sent.
    fn is_reprojected(self) -> bool {
        self.stable_context_fallback || self.tool_history_fallback
    }
}

#[allow(clippy::too_many_arguments)]
fn measure_responses_request_after_dispatch(
    request: ResponsesApiRequest,
    prompt: Prompt,
    selected_representation: SelectedInputRepresentation,
    cancellation: CancellationToken,
    logical_request_bytes: Option<u64>,
    encoded_request: Option<Bytes>,
    payload_timing: Option<(Arc<TurnTimingState>, ResponseAttemptIdentity)>,
    diagnostic_permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> tokio::task::JoinHandle<PostDispatchRequestMeasurements> {
    let input_reprojected = selected_representation.is_reprojected();
    tokio::spawn(async move {
        let fallback_stable_context_manifest = prompt.stable_context_manifest.clone();
        let blocking_cancellation = cancellation.clone();
        let span = tracing::Span::current();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = diagnostic_permit;
            let _span = span.enter();
            // Reuse the dispatched bytes and this worker. Canonical JSON
            // parsing/hashing must not hold up either HTTP or WS transmission.
            // Publish before token categories so calibration never sees those
            // categories without their matching model fingerprint.
            if !blocking_cancellation.is_cancelled()
                && let Some((timing, identity)) = payload_timing
                && let Some(encoded) = encoded_request.as_deref()
            {
                timing.record_model_request_sections(
                    &identity.sampling_request_id,
                    &identity.physical_attempt_id,
                    encoded,
                );
            }
            let provenance = if input_reprojected {
                prompt
                    .prompt_provenance
                    .for_reprojected_items(&prompt.input, &request.input)
            } else {
                prompt.prompt_provenance.clone()
            };
            let mut measured_full_encoded_input = false;
            let measurements = (|| {
                let wire_request_bytes = encoded_request.as_ref().map_or(0, |encoded| {
                    u64::try_from(encoded.len()).unwrap_or(u64::MAX)
                });
                let mut measurements = match encoded_request.as_deref() {
                    Some(encoded) => {
                        ModelRequestMeasurements::for_responses_request_from_encoded_cancellable(
                            &request,
                            &provenance,
                            &prompt.base_instructions.text,
                            Some(&blocking_cancellation),
                            encoded,
                            logical_request_bytes.unwrap_or(wire_request_bytes),
                        )
                        .inspect(|_| {
                            measured_full_encoded_input = true;
                        })
                        .or_else(|_| {
                            ModelRequestMeasurements::for_responses_request_cancellable(
                                &request,
                                &provenance,
                                &prompt.base_instructions.text,
                                Some(&blocking_cancellation),
                                logical_request_bytes,
                            )
                        })?
                    }
                    None => ModelRequestMeasurements::for_responses_request_cancellable(
                        &request,
                        &provenance,
                        &prompt.base_instructions.text,
                        Some(&blocking_cancellation),
                        logical_request_bytes,
                    )?,
                };
                measurements.wire_request_bytes = wire_request_bytes;
                // Incremental transport bodies still carry their settings, so
                // this reads the dispatched bytes even when input measurement
                // fell back to the logical request.
                measurements.request_setting_digests =
                    encoded_request.as_deref().and_then(|encoded| {
                        request_setting_digests(encoded, &blocking_cancellation).ok()
                    });
                Ok::<_, serde_json::Error>(measurements)
            })();
            let stable_context_manifest =
                if measurements.is_ok() && !blocking_cancellation.is_cancelled() {
                    #[derive(Deserialize)]
                    struct EncodedManifestInput<'a> {
                        #[serde(borrow)]
                        input: &'a RawValue,
                    }
                    // A successful encoded measurement has checked that input
                    // is complete. Fallbacks, incremental WS bodies and model
                    // normalization must keep measuring the original prompt.
                    let serialized_input = measured_full_encoded_input
                        .then_some(())
                        .filter(|_| request.input.as_ref() == prompt.input.as_ref())
                        .and_then(|_| encoded_request.as_deref())
                        .and_then(|encoded| {
                            serde_json::from_slice::<EncodedManifestInput<'_>>(encoded).ok()
                        });
                    match serialized_input {
                        Some(encoded) => prompt.measured_stable_context_manifest_from_serialized_input(
                            encoded.input.get().as_bytes(),
                        ),
                        None => prompt.measured_stable_context_manifest(),
                    }
                } else {
                    prompt.stable_context_manifest
                };
            (measurements, stable_context_manifest)
        })
        .await;
        let mut result = match result {
            Ok((Ok(measurements), stable_context_manifest)) => PostDispatchRequestMeasurements {
                measurements,
                stable_context_manifest,
            },
            Ok((Err(err), stable_context_manifest)) => {
                warn!(error = %err, "model request diagnostics were unavailable after dispatch");
                PostDispatchRequestMeasurements {
                    measurements: ModelRequestMeasurements::default(),
                    stable_context_manifest,
                }
            }
            Err(err) => {
                warn!(error = %err, "model request diagnostics task failed after dispatch");
                PostDispatchRequestMeasurements {
                    measurements: ModelRequestMeasurements::default(),
                    stable_context_manifest: fallback_stable_context_manifest,
                }
            }
        };
        result.measurements.selected_representation = Some(selected_representation);
        result
    }.in_current_span())
}

#[derive(Debug, Default)]
struct ModelAttemptOffsets {
    stream_delivery_json: Option<String>,
    transport_attempt: u64,
    request_send_started_ns: Option<u64>,
    response_headers_received_ns: Option<u64>,
    first_response_body_byte_ns: Option<u64>,
    provider_processing_us: Option<u64>,
    queue_started_us: Option<u64>,
    queue_us: u64,
    connection_setup_started_us: Option<u64>,
    connection_setup_us: u64,
    dispatch_ready_us: Option<u64>,
    stream_established_us: Option<u64>,
    first_provider_event_us: Option<u64>,
    first_model_output_us: Option<u64>,
    first_actionable_output_us: Option<u64>,
    completed_us: Option<u64>,
    /// Candidate arrival in the internal relay, before session visibility
    /// filtering. Core publication is recorded separately after acceptance.
    first_visible_output_us: Option<u64>,
}

#[derive(Debug, Clone)]
struct ModelAttemptClock {
    started: Instant,
    started_at_us: u64,
    provider_event_at_transport: bool,
    offsets: Arc<StdMutex<ModelAttemptOffsets>>,
}

impl ModelAttemptClock {
    fn record_response_start(&self, response: &codex_otel::ResponseStartTelemetry<'_>) {
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if response.request_send_started_ns.is_some() {
            offsets.request_send_started_ns = response.request_send_started_ns;
        }
        if response.response_headers_received_ns.is_some() {
            offsets.response_headers_received_ns = response.response_headers_received_ns;
        }
        if response.first_response_body_byte_ns.is_some() {
            offsets.first_response_body_byte_ns = response.first_response_body_byte_ns;
        }
        if response.provider_processing_us.is_some() {
            offsets.provider_processing_us = response.provider_processing_us;
        }
    }

    fn new() -> Self {
        Self {
            started_at_us: u64::try_from(codex_protocol::timing::process_monotonic_ns() / 1_000)
                .unwrap_or(u64::MAX),
            provider_event_at_transport: false,
            started: Instant::now(),
            offsets: Arc::new(StdMutex::new(ModelAttemptOffsets::default())),
        }
    }

    fn elapsed_us(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn mark_dispatch_ready(&self) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_queue_span(&mut offsets, now_us);
        offsets.dispatch_ready_us.get_or_insert(now_us);
    }

    /// A later safe pre-dispatch transport try of the same attempt.
    fn mark_redispatch_ready(&self, transport_attempt: u64) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        offsets.transport_attempt = transport_attempt;
        redispatch_offsets(&mut offsets, now_us);
    }

    fn mark_queue_started(&self) {
        let now_us = self.elapsed_us();
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue_started_us
            .get_or_insert(now_us);
    }

    fn mark_queue_finished(&self) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_queue_span(&mut offsets, now_us);
    }

    fn mark_connection_setup_started(&self) {
        let now_us = self.elapsed_us();
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection_setup_started_us
            .get_or_insert(now_us);
    }

    fn mark_connection_setup_finished(&self) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(started_us) = offsets.connection_setup_started_us.take() {
            offsets.connection_setup_us = offsets
                .connection_setup_us
                .saturating_add(now_us.saturating_sub(started_us));
        }
    }

    fn mark_stream_established(&self) {
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stream_established_us
            .get_or_insert_with(|| self.elapsed_us());
    }

    fn mark_completed(&self) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if offsets.completed_us.is_none() {
            finish_queue_span(&mut offsets, now_us);
            if let Some(started) = offsets.connection_setup_started_us.take() {
                offsets.connection_setup_us = offsets
                    .connection_setup_us
                    .saturating_add(now_us.saturating_sub(started));
            }
            offsets.completed_us = Some(now_us);
        }
    }

    fn mark_first_provider_event(&self) {
        let now_us = self.elapsed_us();
        let mut offsets = self
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A decoded event proves the stream exists. The spawned HTTP body task can decode
        // it before the dispatching task records establishment; keep the offsets ordered.
        offsets.stream_established_us.get_or_insert(now_us);
        offsets.first_provider_event_us.get_or_insert(now_us);
    }

    fn mark_first_model_output(&self) {
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_model_output_us
            .get_or_insert_with(|| self.elapsed_us());
    }

    fn mark_first_actionable_output(&self) {
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_actionable_output_us
            .get_or_insert_with(|| self.elapsed_us());
    }

    fn mark_first_visible_output(&self) {
        self.offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .first_visible_output_us
            .get_or_insert_with(|| self.elapsed_us());
    }
}

const MODEL_STREAM_GAP_BOUNDS_US: [u64; 13] = [
    0, 1_000, 5_000, 10_000, 25_000, 50_000, 100_000,
    250_000, 500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000,
];

/// Constant-space mapper observations: deltas are chunks, never provider tokens.
/// No per-delta mutex, allocation, logging or extra task is introduced.
#[derive(Default, serde::Serialize)]
struct ModelStreamDelivery {
    version: u32,
    physical_attempt_id: String,
    clock_id: &'static str,
    dispatch_ready_us: Option<u64>,
    completed_us: u64,
    connection_setup_us: u64,
    request_send_started_ns: Option<u64>,
    response_headers_received_ns: Option<u64>,
    first_response_body_byte_ns: Option<u64>,
    provider_processing_us: Option<u64>,
    first_output_chunk_us: Option<u64>,
    last_output_chunk_us: Option<u64>,
    output_chunk_count: u64,
    payload_measurement_version: u32,
    output_chunk_utf8_bytes: u64,
    first_output_chunk_utf8_bytes: Option<u64>,
    gap_histogram_version: u32,
    gap_histogram_upper_bounds_us: [u64; 13],
    gap_histogram_counts: [u64; 14],
    max_inter_chunk_gap_us: Option<u64>,
    inter_chunk_stall_count: u64,
    inter_chunk_stall_us: u64,
    stall_threshold_us: u64,
    complete: bool,
    downstream_backpressured: bool,
    saturated: bool,
}

struct ModelStreamDeliveryObserver {
    identity: Option<ResponseAttemptIdentity>,
    clock: Option<ModelAttemptClock>,
    turn_timing: Option<Arc<TurnTimingState>>,
    summary: ModelStreamDelivery,
    published: bool,
}

impl ModelStreamDeliveryObserver {
    fn new(
        identity: Option<ResponseAttemptIdentity>,
        clock: Option<ModelAttemptClock>,
        turn_timing: Option<Arc<TurnTimingState>>,
    ) -> Self {
        Self {
            identity,
            clock,
            turn_timing,
            summary: ModelStreamDelivery {
                version: 1,
                clock_id: codex_protocol::timing::process_clock_domain_id(),
                stall_threshold_us: 1_000_000,
                payload_measurement_version: 1,
                gap_histogram_version: 1,
                gap_histogram_upper_bounds_us: MODEL_STREAM_GAP_BOUNDS_US,
                ..Default::default()
            },
            published: false,
        }
    }

    fn observe(&mut self, event: &ResponseEvent) {
        let delta = match event {
            ResponseEvent::OutputTextDelta(delta)
            | ResponseEvent::ReasoningSummaryDelta { delta, .. }
            | ResponseEvent::ReasoningContentDelta { delta, .. }
            | ResponseEvent::ToolCallInputDelta { delta, .. } => delta,
            // Done/items can repeat previously delivered content. Do not count
            // them as another generated chunk or invent a token count.
            _ => return,
        };
        let Some(clock) = self.clock.as_ref().filter(|_| !delta.is_empty()) else {
            return;
        };
        let now_us = clock.elapsed_us();
        let payload_bytes = u64::try_from(delta.len()).unwrap_or(u64::MAX);
        let summary = &mut self.summary;
        if let Some(previous) = summary.last_output_chunk_us {
            let gap = now_us.saturating_sub(previous);
            let bucket = MODEL_STREAM_GAP_BOUNDS_US.partition_point(|bound| gap > *bound);
            summary.gap_histogram_counts[bucket] = summary.gap_histogram_counts[bucket].saturating_add(1);
            summary.saturated |= summary.gap_histogram_counts[bucket] == u64::MAX;
            summary.max_inter_chunk_gap_us =
                Some(summary.max_inter_chunk_gap_us.unwrap_or(0).max(gap));
            if gap >= summary.stall_threshold_us {
                summary.inter_chunk_stall_count = summary.inter_chunk_stall_count.saturating_add(1);
                summary.inter_chunk_stall_us = summary.inter_chunk_stall_us.saturating_add(gap);
            }
        }
        summary.first_output_chunk_us.get_or_insert(now_us);
        summary.first_output_chunk_utf8_bytes.get_or_insert(payload_bytes);
        summary.output_chunk_utf8_bytes = summary.output_chunk_utf8_bytes.saturating_add(payload_bytes);
        summary.last_output_chunk_us = Some(now_us);
        summary.output_chunk_count = summary.output_chunk_count.saturating_add(1);
        summary.saturated |= now_us == u64::MAX
            || payload_bytes == u64::MAX
            || summary.output_chunk_utf8_bytes == u64::MAX
            || summary.output_chunk_count == u64::MAX
            || summary.inter_chunk_stall_count == u64::MAX
            || summary.inter_chunk_stall_us == u64::MAX;
    }

    fn publish(&mut self, complete: bool) {
        if self.published {
            return;
        }
        self.published = true;
        let (Some(identity), Some(clock)) = (&self.identity, &self.clock) else {
            return;
        };
        self.summary.complete = complete;
        let mut offsets = clock
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.summary.dispatch_ready_us = offsets.dispatch_ready_us;
        self.summary.completed_us = offsets.completed_us.unwrap_or_else(|| clock.elapsed_us());
        self.summary.connection_setup_us = offsets.connection_setup_us;
        self.summary.request_send_started_ns = offsets.request_send_started_ns;
        self.summary.response_headers_received_ns = offsets.response_headers_received_ns;
        self.summary.first_response_body_byte_ns = offsets.first_response_body_byte_ns;
        self.summary.provider_processing_us = offsets.provider_processing_us;
        self.summary.physical_attempt_id = if offsets.transport_attempt == 0 {
            identity.physical_attempt_id.clone()
        } else {
            format!("{}:http-{}", identity.physical_attempt_id, offsets.transport_attempt)
        };
        let json = serde_json::to_string(&self.summary).unwrap_or_default();
        offsets.stream_delivery_json = Some(json.clone());
        drop(offsets);
        // Never hold the clock mutex while acquiring turn state. This reuses
        // bounded physical-attempt retention and checkpoint persistence.
        if let Some(timing) = self.turn_timing.as_ref() {
            timing.record_model_stream_delivery(
                &identity.sampling_request_id,
                &self.summary.physical_attempt_id,
                &json,
            );
        }
        tracing::info!(
            event.name = "codex.model_stream_delivery",
            sampling_request_id = %identity.sampling_request_id,
            attempt_id = %identity.physical_attempt_id,
            clock_id = codex_protocol::timing::process_clock_domain_id(),
            stream_delivery_json = %json,
            "content-free model stream delivery summary"
        );
    }
}

impl Drop for ModelStreamDeliveryObserver {
    fn drop(&mut self) {
        // Cancellation, dropped consumers and early transport exits preserve
        // observed partial evidence rather than asserting a complete stream.
        self.publish(false);
    }
}

struct ModelAttemptGuard {
    turn_timing: Option<Arc<TurnTimingState>>,
    details_only: bool,
    session_telemetry: SessionTelemetry,
    turn_id: Option<String>,
    generation: Option<ModelAttemptGenerationMetadata>,
    sampling_request_id: String,
    attempt_id: String,
    retry_index: u32,
    retry_reason: ModelAttemptRetryReason,
    request_kind: ModelAttemptRequestKind,
    transport: ModelAttemptTransport,
    connection_reused: Option<bool>,
    measurements: ModelRequestMeasurements,
    clock: ModelAttemptClock,
    fresh_response_id_established: bool,
    emitted: bool,
    terminal_usage: Option<TokenUsage>,
    response_id: Option<String>,
    provider_completed: bool,
    _diagnostic_permit: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
}

impl ModelAttemptGuard {
    fn with_diagnostic_permit(mut self, permit: Arc<tokio::sync::OwnedSemaphorePermit>) -> Self {
        self._diagnostic_permit = Some(permit);
        self.details_only = true;
        self
    }

    // These fields are the complete dimensions of one attempt telemetry event;
    // grouping them would only obscure their call-site correspondence.
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_telemetry: SessionTelemetry,
        identity: ResponseAttemptIdentity,
        retry_index: u32,
        retry_reason: ModelAttemptRetryReason,
        request_kind: ModelAttemptRequestKind,
        transport: ModelAttemptTransport,
        connection_reused: Option<bool>,
        measurements: ModelRequestMeasurements,
        clock: ModelAttemptClock,
        turn_id: Option<String>,
        generation: Option<ModelAttemptGenerationMetadata>,
    ) -> Self {
        Self {
            _diagnostic_permit: None,
            turn_timing: None,
            details_only: false,
            session_telemetry,
            turn_id,
            generation,
            sampling_request_id: identity.sampling_request_id,
            attempt_id: identity.physical_attempt_id,
            retry_index,
            retry_reason,
            request_kind,
            transport,
            connection_reused,
            measurements,
            clock,
            fresh_response_id_established: false,
            emitted: false,
            terminal_usage: None,
            response_id: None,
            provider_completed: false,
        }
    }

    fn clock(&self) -> ModelAttemptClock {
        self.clock.clone()
    }

    fn response_identity(&self) -> crate::client_common::ResponseAttemptIdentity {
        crate::client_common::ResponseAttemptIdentity {
            sampling_request_id: self.sampling_request_id.clone(),
            physical_attempt_id: self.attempt_id.clone(),
        }
    }

    fn tool_token_count(&self) -> i64 {
        self.measurements.tool_token_count
    }

    fn mark_fresh_response_id_established(&mut self, response_id: &str) {
        self.fresh_response_id_established = !response_id.is_empty()
            && self.transport == ModelAttemptTransport::ResponsesWebsocket
            && self.measurements.provider_baseline == ModelAttemptProviderBaseline::FreshFullReplay;
    }

    fn observe_terminal_usage(&mut self, response_id: Option<&str>, usage: Option<&TokenUsage>) {
        if self.emitted {
            return;
        }
        self.response_id = response_id.map(str::to_owned);
        self.terminal_usage = usage.cloned();
        if let Some(timing) = self.turn_timing.as_ref() {
            if let Some(usage) = usage {
                timing.record_model_attempt_token_usage(
                    &self.sampling_request_id,
                    &self.attempt_id,
                    usage,
                );
            }
            if let Some(response_id) = response_id {
                timing.record_model_response_id(&self.attempt_id, response_id);
            }
        }
    }

    fn finish(
        &mut self,
        outcome: ModelAttemptOutcome,
        input_tokens: Option<i64>,
        cached_input_tokens: Option<i64>,
        output_items: &[ResponseItem],
    ) {
        if self.emitted {
            return;
        }
        self.emitted = true;
        self.clock.mark_completed();
        if self.provider_completed && let Some(timing) = &self.turn_timing {
            timing.record_provider_terminal(&self.sampling_request_id, &self.attempt_id);
        }
        let uncached_input_tokens = input_tokens
            .zip(cached_input_tokens)
            .and_then(|(input, cached)| input.checked_sub(cached))
            .filter(|uncached| *uncached >= 0);
        if let Some(cache_coverage_bps) = cache_coverage_below_matched_task_baseline(
            self.measurements.matched_task_reuse_eligible(),
            outcome,
            self.measurements
                .expected_reusable_input_tokens(input_tokens),
            cached_input_tokens,
        ) {
            warn!(
                sampling_request_id = %self.sampling_request_id,
                attempt_id = %self.attempt_id,
                cache_coverage_bps,
                baseline_bps = MATCHED_TASK_CACHE_COVERAGE_BASELINE_BPS,
                input_tokens,
                cached_input_tokens,
                "matched-task prompt cache coverage fell below baseline"
            );
        }
        let local_prompt_context_categories = compact_prompt_context_categories(
            &self.measurements.prompt_context_categories,
            self.measurements.prompt_context_baseline_compared,
        );
        // The OTEL-only event is deliberately excluded from local SQLite logs.
        // Keep a compact, content-free completion record locally as well, so
        // cache investigations do not depend on offloaded timing snapshots.
        //
        // The per-category byte/token breakdown is included here rather than
        // only in the OTEL attempt event: without it, a local investigation can
        // see that the cached prefix moved but not which context category paid
        // for it. These measurements are counts and domain-separated hashes, so
        // the record stays content-free.
        tracing::info!(
            event.name = if self.details_only { "codex.model_cache_details" } else { "codex.model_cache" },
            turn_id = self.turn_id.as_deref(),
            sampling_request_id = %self.sampling_request_id,
            physical_attempt_id = %self.attempt_id,
            transport = ?self.transport,
            outcome = ?outcome,
            input_representation = self.measurements.selected_representation.map(SelectedInputRepresentation::as_str),
            diagnostics_available = !self.measurements.prompt_context_categories.is_empty(),
            prompt_context_categories = %local_prompt_context_categories,
            full_prompt_estimated_tokens = self.measurements.full_prompt_estimated_tokens,
            fixed_prefix_reuse_eligible = self.measurements.fixed_prefix_reuse_eligible,
            fixed_prefix_item_count = self.measurements.fixed_prefix_item_count,
            tool_schemas_bytes = self.measurements.tool_schemas_bytes,
            local_reused_bytes = self.measurements.local_reused_bytes,
            local_constructed_bytes = self.measurements.local_constructed_bytes,
            baseline_compared = self.measurements.prompt_context_baseline_compared,
            provider_baseline = ?self.measurements.provider_baseline,
            previous_response_id_present = self.measurements.previous_response_id_present,
            history_items_previous = self.measurements.history_divergence.map(|value| value.items_previous),
            history_prefix_items_reused = self.measurements.history_divergence.map(|value| value.prefix_items_reused),
            history_first_divergent_index = self.measurements.history_divergence.map(|value| value.first_divergent_index),
            changed_request_settings = ?self.measurements.changed_request_settings,
            input_tokens,
            cached_input_tokens,
            uncached_input_tokens,
            response_id = self.response_id.as_deref(),
            provider_completed = self.provider_completed,
            cache_write_input_tokens = self.terminal_usage.as_ref().and_then(TokenUsage::known_cache_write_input),
            output_tokens = self.terminal_usage.as_ref().and_then(TokenUsage::known_output),
            reasoning_output_tokens = self.terminal_usage.as_ref().and_then(TokenUsage::known_reasoning_output),
            total_tokens = self.terminal_usage.as_ref().and_then(TokenUsage::known_total),
            "model cache diagnostics"
        );
        if !self.session_telemetry.model_attempt_logging_enabled() {
            return;
        }
        let offsets = self
            .clock
            .offsets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let completed_us = offsets
            .completed_us
            .unwrap_or_else(|| self.clock.elapsed_us());
        let fresh_response_id_established =
            outcome == ModelAttemptOutcome::Success && self.fresh_response_id_established;
        let (request_construction_us, queue_us, transport_us) =
            model_attempt_phase_durations(&offsets);
        let producer_selected_context_tokens = self.measurements.producer_selected_context_tokens();
        let tool_schema_breakdown =
            selected_tool_schema_breakdown(&self.measurements.tool_schema_breakdown, output_items);
        let observed_selected_tool_schema_tokens = tool_schema_breakdown
            .iter()
            .filter(|schema| schema.selected_by_model)
            .fold(0_u64, |total, schema| {
                total.saturating_add(schema.approx_tokens)
            });
        let operational_relevance_proxy_tokens = producer_selected_context_tokens
            .saturating_add(observed_selected_tool_schema_tokens)
            .min(self.measurements.logical_prompt_tokens());
        debug_assert!(attempt_offsets_are_nondecreasing(&offsets, completed_us));
        self.session_telemetry
            .model_attempt_completed(&ModelAttemptTelemetry {
                details_only: self.details_only,
                diagnostics_available: !self.measurements.prompt_context_categories.is_empty(),
                clock_id: codex_protocol::timing::process_clock_domain_id().to_string(),
                attempt_started_us: self.clock.started_at_us,
                turn_id: self.turn_id.clone(),
                generation_index: self
                    .generation
                    .as_ref()
                    .map(|metadata| metadata.generation_index),
                generation_purpose: self
                    .generation
                    .as_ref()
                    .and_then(ModelAttemptGenerationMetadata::purpose_label)
                    .map(str::to_string),
                generation_disposition: self
                    .generation
                    .as_ref()
                    .map(ModelAttemptGenerationMetadata::disposition_label)
                    .map(str::to_string),
                relevant_state_fingerprint: self
                    .generation
                    .as_ref()
                    .and_then(|metadata| metadata.relevant_state_fingerprint.clone()),
                sampling_request_id: self.sampling_request_id.clone(),
                attempt_id: self.attempt_id.clone(),
                retry_index: self.retry_index,
                retry_reason: self.retry_reason,
                outcome,
                request_kind: self.request_kind,
                transport: self.transport,
                connection_reused: self.connection_reused,
                baseline_generation: self.measurements.baseline_generation,
                provider_baseline: self.measurements.provider_baseline,
                previous_response_id_present: self.measurements.previous_response_id_present,
                local_projection_policy_active: self.measurements.local_projection_policy_active,
                fresh_response_id_established,
                input_tokens,
                cached_input_tokens,
                uncached_input_tokens,
                response_id: self.response_id.clone(),
                output_tokens: self
                    .terminal_usage
                    .as_ref()
                    .and_then(TokenUsage::known_output),
                reasoning_output_tokens: self
                    .terminal_usage
                    .as_ref()
                    .and_then(TokenUsage::known_reasoning_output),
                total_tokens: self
                    .terminal_usage
                    .as_ref()
                    .and_then(TokenUsage::known_total),
                tool_token_count: self.measurements.tool_token_count,
                wire_request_bytes: self.measurements.wire_request_bytes,
                logical_request_bytes: self.measurements.logical_request_bytes,
                base_instructions_bytes: self.measurements.base_instructions_bytes,
                tool_schemas_bytes: self.measurements.tool_schemas_bytes,
                conversation_history_bytes: self.measurements.conversation_history_bytes,
                current_input_bytes: self.measurements.current_input_bytes,
                repository_context_bytes: self.measurements.repository_context_bytes,
                skills_bytes: self.measurements.skills_bytes,
                other_injected_context_bytes: self.measurements.other_injected_context_bytes,
                envelope_overhead_bytes: self.measurements.envelope_overhead_bytes,
                reconciliation_residual_bytes: self.measurements.reconciliation_residual_bytes,
                prompt_context_categories: self
                    .measurements
                    .prompt_context_categories
                    .iter()
                    .map(|measurement| ModelPromptContextCategoryTelemetry {
                        category: measurement.category.to_string(),
                        serialized_bytes: measurement.serialized_bytes,
                        approx_tokens: measurement.estimated_tokens,
                        sha256: measurement.sha256.clone(),
                        unchanged_from_previous_request: measurement
                            .unchanged_from_previous_request,
                    })
                    .collect(),
                producer_selected_context_tokens,
                producer_selected_context_density_bps: self
                    .measurements
                    .density_bps(producer_selected_context_tokens),
                observed_selected_tool_schema_tokens,
                operational_relevance_proxy_tokens,
                operational_relevance_proxy_density_bps: self
                    .measurements
                    .density_bps(operational_relevance_proxy_tokens),
                tool_schema_breakdown,
                full_prompt_estimated_tokens: self.measurements.full_prompt_estimated_tokens,
                fixed_prefix_reuse_eligible: self.measurements.fixed_prefix_reuse_eligible,
                logical_context_tokens: self.measurements.logical_context_tokens,
                active_component_bytes: self.measurements.active_component_bytes,
                local_constructed_bytes: self.measurements.local_constructed_bytes,
                local_reused_bytes: self.measurements.local_reused_bytes,
                local_component_reuse_count: self.measurements.local_component_reuse_count,
                request_construction_us,
                queue_us,
                connection_setup_us: offsets.connection_setup_us,
                transport_us,
                dispatch_ready_us: offsets.dispatch_ready_us,
                stream_established_us: offsets.stream_established_us,
                first_provider_event_us: offsets.first_provider_event_us,
                first_model_output_us: offsets.first_model_output_us,
                first_actionable_output_us: offsets.first_actionable_output_us,
                first_visible_output_us: offsets.first_visible_output_us,
                stream_delivery_json: offsets.stream_delivery_json.clone(),
                completed_us,
            });
        for receipt in self.measurements.stable_context_manifest.filtering_receipts() {
            tracing::info!(event.name = "codex.stable_context.filtering_receipt",
                sampling_request_id = %self.sampling_request_id, attempt_id = %self.attempt_id,
                kind = receipt.kind.as_str(), semantic_id = %receipt.identity.semantic_id,
                disposition = receipt.disposition.as_str(), active = receipt.active,
                "sampling history filtering disposition (separate from assembled prompt)");
        }
        for component in self.measurements.stable_context_manifest.components() {
            self.session_telemetry
                .model_context_component(&ModelContextComponentTelemetry {
                    sampling_request_id: self.sampling_request_id.clone(),
                    attempt_id: self.attempt_id.clone(),
                    retry_index: self.retry_index,
                    kind: component.kind.as_str().to_string(),
                    contract_version: component.identity.contract_version,
                    semantic_id: component.identity.semantic_id.clone(),
                    content_hash: short_hash(&component.identity.content_hash),
                    serialized_bytes: component.identity.serialized_bytes,
                    approx_tokens: component.identity.approx_tokens,
                    active: component.active,
                    disposition: component.disposition.as_str().to_string(),
                    local_reused: component.local_reused,
                    baseline_generation: self.measurements.baseline_generation,
                    provider_baseline: self.measurements.provider_baseline,
                    previous_response_id_present: self.measurements.previous_response_id_present,
                    local_projection_policy_active: self
                        .measurements
                        .local_projection_policy_active,
                    fresh_response_id_established,
                });
        }
    }
}

fn finish_queue_span(offsets: &mut ModelAttemptOffsets, now_us: u64) {
    if let Some(started_us) = offsets.queue_started_us.take() {
        offsets.queue_us = offsets
            .queue_us
            .saturating_add(now_us.saturating_sub(started_us));
    }
}

/// A safe pre-dispatch retry sends nothing on the failed dial. Fold that dial and its
/// backoff into connection setup and move dispatch-ready to the retry, so post-dispatch
/// offsets (TTFT included) exclude client-side recovery. Phase totals are unchanged.
fn redispatch_offsets(offsets: &mut ModelAttemptOffsets, now_us: u64) {
    offsets.request_send_started_ns = None;
    offsets.response_headers_received_ns = None;
    offsets.first_response_body_byte_ns = None;
    offsets.provider_processing_us = None;
    finish_queue_span(offsets, now_us);
    if let Some(previous_us) = offsets.dispatch_ready_us.replace(now_us) {
        offsets.connection_setup_us = offsets
            .connection_setup_us
            .saturating_add(now_us.saturating_sub(previous_us));
    }
}

/// Renders the per-category breakdown for the local cache-diagnostics event.
///
/// Only categories that contributed bytes are listed, so an ordinary request
/// does not carry fourteen zeroed entries. `changed` reports the comparison
/// against the preceding request, which is what identifies the category that
/// invalidated a cached prefix; it is omitted when no baseline existed. Values
/// are counts and the category's domain-separated hash, never prompt text.
fn compact_prompt_context_categories(
    measurements: &[PromptContextMeasurement],
    baseline_compared: bool,
) -> String {
    let entries = measurements
        .iter()
        .filter(|measurement| measurement.serialized_bytes > 0)
        .map(|measurement| {
            serde_json::json!({
                "category": measurement.category,
                "bytes": measurement.serialized_bytes,
                "tokens": measurement.estimated_tokens,
                "sha256": measurement.sha256,
                // Absent rather than `true` on the first request of a session:
                // with no predecessor every category is trivially "changed",
                // which would otherwise read as cache churn.
                "changed": baseline_compared
                    .then_some(!measurement.unchanged_from_previous_request),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

fn model_attempt_phase_durations(offsets: &ModelAttemptOffsets) -> (u64, u64, Option<u64>) {
    let local_end = offsets
        .dispatch_ready_us
        .or(offsets.completed_us)
        .unwrap_or_default();
    let queue_us = offsets.queue_us.min(local_end);
    let request_construction_us = local_end
        .saturating_sub(queue_us)
        .saturating_sub(offsets.connection_setup_us);
    let transport_us = offsets
        .stream_established_us
        .zip(offsets.dispatch_ready_us)
        .map(|(established_us, dispatched_us)| {
            offsets
                .connection_setup_us
                .saturating_add(established_us.saturating_sub(dispatched_us))
        });
    (request_construction_us, queue_us, transport_us)
}

impl Drop for ModelAttemptGuard {
    fn drop(&mut self) {
        self.finish(ModelAttemptOutcome::Cancelled, None, None, &[]);
    }
}

enum ModelAttemptState {
    Ready(Box<ModelAttemptGuard>),
    Pending(PendingModelAttempt),
}

struct PendingModelAttempt {
    essential: Box<ModelAttemptGuard>,
    turn_timing: Option<Arc<TurnTimingState>>,
    _diagnostic_permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    identity: ResponseAttemptIdentity,
    clock: ModelAttemptClock,
    task: AbortOnDropHandle<ModelAttemptGuard>,
    measurement_cancellation: CancellationToken,
}

impl Drop for PendingModelAttempt {
    fn drop(&mut self) {
        self.measurement_cancellation.cancel();
        if !self.task.is_finished()
            && let Some(timing) = &self.turn_timing
        {
            timing.record_model_request_diagnostics_unavailable(
                &self.identity.sampling_request_id,
                &self.identity.physical_attempt_id,
            );
        }
    }
}

impl ModelAttemptState {
    fn observe_response_queue<E>(
        &self,
        receiver: &mut codex_api::response_event_channel::ResponseEventReceiver<E>,
        stage: &'static str,
    ) {
        let guard = match self {
            Self::Ready(guard) => guard.as_ref(),
            Self::Pending(pending) => pending.essential.as_ref(),
        };
        if let Some(timing) = guard.turn_timing.as_ref() {
            let timing = Arc::clone(timing);
            let identity = self.response_identity();
            receiver.observe_timing(move |observation| {
                timing.record_response_queue_timing(
                    &identity.sampling_request_id,
                    &identity.physical_attempt_id,
                    stage,
                    observation,
                );
            });
        }
    }

    fn try_diagnostic_permit() -> Option<Arc<tokio::sync::OwnedSemaphorePermit>> {
        static DIAGNOSTICS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
            std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(32)));
        Arc::clone(&DIAGNOSTICS)
            .try_acquire_owned()
            .ok()
            .map(Arc::new)
    }

    fn pending(
        mut essential: ModelAttemptGuard,
        task: tokio::task::JoinHandle<ModelAttemptGuard>,
        measurement_cancellation: CancellationToken,
        turn_timing: Option<Arc<TurnTimingState>>,
        permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Self {
        essential.turn_timing = turn_timing.clone();
        Self::Pending(PendingModelAttempt {
            identity: essential.response_identity(),
            clock: essential.clock(),
            essential: Box::new(essential),
            turn_timing,
            _diagnostic_permit: permit,
            task: AbortOnDropHandle::new(task),
            measurement_cancellation,
        })
    }

    fn response_identity(&self) -> ResponseAttemptIdentity {
        match self {
            Self::Ready(attempt) => attempt.response_identity(),
            Self::Pending(pending) => pending.identity.clone(),
        }
    }

    fn clock(&self) -> ModelAttemptClock {
        match self {
            Self::Ready(attempt) => attempt.clock(),
            Self::Pending(pending) => pending.clock.clone(),
        }
    }

    async fn resolve(self) -> Option<ModelAttemptGuard> {
        match self {
            Self::Ready(attempt) => Some(*attempt),
            Self::Pending(mut pending) => match (&mut pending.task).await {
                Ok(mut attempt) => {
                    attempt._diagnostic_permit = Some(Arc::clone(&pending._diagnostic_permit));
                    attempt.details_only = true;
                    Some(attempt)
                }
                Err(err) => {
                    warn!(error = %err, "model attempt diagnostics task failed after dispatch");
                    if let Some(timing) = pending.turn_timing.as_ref() {
                        timing.record_model_request_diagnostics_unavailable(
                            &pending.identity.sampling_request_id,
                            &pending.identity.physical_attempt_id,
                        );
                    }
                    None
                }
            },
        }
    }

    fn finish_essential(
        &mut self,
        outcome: ModelAttemptOutcome,
        response_id: Option<&str>,
        usage: Option<&TokenUsage>,
        items: &[ResponseItem],
        provider_completed: bool,
    ) {
        let guard = match self {
            Self::Ready(guard) => guard.as_mut(),
            Self::Pending(pending) => pending.essential.as_mut(),
        };
        guard.provider_completed = provider_completed;
        guard.observe_terminal_usage(response_id, usage);
        guard.finish(
            outcome,
            usage.and_then(TokenUsage::known_input),
            usage.and_then(TokenUsage::known_cached_input),
            items,
        );
    }
}

impl From<ModelAttemptGuard> for ModelAttemptState {
    fn from(attempt: ModelAttemptGuard) -> Self {
        Self::Ready(Box::new(attempt))
    }
}

async fn resolve_model_attempt(
    attempt: &mut Option<ModelAttemptState>,
) -> Option<ModelAttemptGuard> {
    attempt.take()?.resolve().await
}

fn finish_failed_model_attempt(attempt: &mut Option<ModelAttemptState>, items: Vec<ResponseItem>) {
    finish_failed_model_attempt_with_usage(attempt, items, None, None);
}

fn finish_failed_model_attempt_with_usage(
    attempt: &mut Option<ModelAttemptState>,
    items: Vec<ResponseItem>,
    response_id: Option<String>,
    usage: Option<TokenUsage>,
) {
    finish_model_attempt_with_usage(
        attempt,
        items,
        response_id,
        usage,
        ModelAttemptOutcome::Failed,
    );
}

fn finish_model_attempt_with_usage(
    attempt: &mut Option<ModelAttemptState>,
    items: impl AsRef<[ResponseItem]> + Send + 'static,
    response_id: Option<String>,
    usage: Option<TokenUsage>,
    outcome: ModelAttemptOutcome,
) {
    if let Some(mut attempt) = attempt.take() {
        attempt.clock().mark_completed();
        attempt.finish_essential(
            outcome,
            response_id.as_deref(),
            usage.as_ref(),
            items.as_ref(),
            outcome == ModelAttemptOutcome::Cancelled,
        );
        // Diagnostics own their lifetime, but never delay transport recovery.
        tokio::spawn(
            async move {
                if let Some(mut attempt) = attempt.resolve().await {
                    // A cancelled delivery can still follow an observed provider
                    // completion. Keep those independent of diagnostic readiness.
                    attempt.provider_completed = outcome == ModelAttemptOutcome::Cancelled;
                    attempt.observe_terminal_usage(response_id.as_deref(), usage.as_ref());
                    attempt.finish(
                        outcome,
                        usage.as_ref().and_then(TokenUsage::known_input),
                        usage.as_ref().and_then(TokenUsage::known_cached_input),
                        items.as_ref(),
                    );
                }
            }
            .in_current_span(),
        );
    }
}

fn attempt_offsets_are_nondecreasing(offsets: &ModelAttemptOffsets, completed_us: u64) -> bool {
    let mut previous = 0;
    for offset in [
        offsets.dispatch_ready_us,
        offsets.stream_established_us,
        offsets.first_provider_event_us,
        offsets.first_model_output_us,
        offsets.first_visible_output_us,
        Some(completed_us),
    ]
    .into_iter()
    .flatten()
    {
        if offset < previous {
            return false;
        }
        previous = offset;
    }
    offsets.first_actionable_output_us.is_none_or(|offset| {
        offsets
            .dispatch_ready_us
            .is_none_or(|dispatch| offset >= dispatch)
            && offset <= completed_us
    })
}

pub(crate) fn request_effort_for_model(
    model_info: &ModelInfo,
    effort: Option<ReasoningEffortConfig>,
) -> Option<ReasoningEffortConfig> {
    effort
        .or_else(|| model_info.default_reasoning_level.clone())
        .and_then(|effort| {
            if !matches!(
                effort,
                ReasoningEffortConfig::Ultra | ReasoningEffortConfig::Max
            ) {
                return Some(effort);
            }
            let supported = &model_info.supported_reasoning_levels;
            // Ultra is a local alias for wire Max. Older catalogs stop at xhigh;
            // do not send an unsupported Max merely because a preset requested it.
            if supported.is_empty()
                || supported.iter().any(|level| {
                    matches!(
                        level.effort,
                        ReasoningEffortConfig::Ultra | ReasoningEffortConfig::Max
                    )
                })
            {
                return Some(ReasoningEffortConfig::Max);
            }
            [
                ReasoningEffortConfig::XHigh,
                ReasoningEffortConfig::High,
                ReasoningEffortConfig::Medium,
                ReasoningEffortConfig::Low,
                ReasoningEffortConfig::Minimal,
                ReasoningEffortConfig::None,
            ]
            .into_iter()
            .find(|candidate| supported.iter().any(|level| &level.effort == candidate))
            .or_else(|| model_info.default_reasoning_level.clone())
        })
}

/// Validate a child agent's already normalized wire effort. Main-turn requests
/// intentionally preserve explicit ordinary and custom values independently of
/// picker metadata. Unknown fallback descriptors have no authoritative list.
pub(crate) fn validate_request_effort_for_model(
    model: &ModelInfo,
    effort: Option<&ReasoningEffortConfig>,
) -> std::result::Result<(), String> {
    if model.used_fallback_model_metadata {
        return Ok(());
    }
    let Some(effort) = effort else {
        return Ok(());
    };
    validate_reasoning_effort(&model.slug, &model.supported_reasoning_levels, effort)
}

pub(crate) fn validate_reasoning_effort(
    model: &str,
    supported: &[codex_protocol::openai_models::ReasoningEffortPreset],
    effort: &ReasoningEffortConfig,
) -> std::result::Result<(), String> {
    if supported.iter().any(|level| {
        &level.effort == effort
            || (*effort == ReasoningEffortConfig::Max
                && level.effort == ReasoningEffortConfig::Ultra)
    }) {
        return Ok(());
    }
    let choices = supported
        .iter()
        .map(|level| level.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Reasoning effort `{effort}` is not supported for model `{model}`. Supported reasoning efforts: {choices}. Update model_reasoning_effort or the turn reasoning setting."
    ))
}

fn session_telemetry_for_request(
    session_telemetry: &SessionTelemetry,
    request: &ResponsesApiRequest,
) -> SessionTelemetry {
    session_telemetry.clone().with_inference_request(
        request.service_tier.as_deref(),
        request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_ref()),
    )
}

/// Bounds model HTTP connection establishment (TCP, proxy CONNECT, TLS) like the default
/// WebSocket connect timeout. Without it a stalled handshake is only caught by the
/// response-header deadline, which must treat the request as possibly dispatched.
const MODEL_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn model_http_client_builder(headers: ApiHeaderMap) -> HttpClientBuilder {
    HttpClientBuilder::new()
        .default_headers(headers)
        .with_chatgpt_cloudflare_cookie_store()
        .connect_timeout(MODEL_HTTP_CONNECT_TIMEOUT)
}

/// Thread-manager-owned HTTP transports. Conversation and WebSocket state are never shared.
/// Like individual route pools, these retain process TLS settings until their owner is replaced.
#[derive(Default)]
pub(crate) struct SharedModelHttpClients {
    pools: StdMutex<VecDeque<(HttpClientFactory, ApiHeaderMap, RouteAwareClientPool)>>,
}

impl SharedModelHttpClients {
    fn pool(&self, factory: &HttpClientFactory, headers: ApiHeaderMap) -> RouteAwareClientPool {
        const MAX_POOLS: usize = 8;
        let mut pools = self
            .pools
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = pools
            .iter()
            .position(|(cached_factory, cached_headers, _)| {
                cached_factory == factory && cached_headers == &headers
            })
        {
            #[expect(
                clippy::expect_used,
                reason = "the index was selected from this deque while holding the same exclusive lock"
            )]
            let entry = pools.remove(index).expect("matched pool exists");
            let pool = entry.2.clone();
            pools.push_back(entry);
            return pool;
        }
        // Snapshot the defaults once so the cache key and the actual headers cannot diverge.
        // No network or TLS construction occurs while holding this lock; route pools are lazy.
        let pool = RouteAwareClientPool::with_builder(
            factory.clone(),
            ClientRouteClass::Api,
            model_http_client_builder(headers.clone()),
        );
        if pools.len() == MAX_POOLS {
            pools.pop_front();
        }
        pools.push_back((factory.clone(), headers, pool.clone()));
        pool
    }
}

/// Session-scoped state shared by all [`ModelClient`] clones.
///
/// This is intentionally kept minimal so `ModelClient` does not need to hold a full `Config`. Most
/// configuration is per turn and is passed explicitly to streaming/unary methods.
#[derive(Debug)]
struct ModelClientState {
    thread_id: ThreadId,
    provider: SharedModelProvider,
    auth_env_telemetry: AuthEnvTelemetry,
    session_source: SessionSource,
    originator: String,
    model_verbosity: Option<VerbosityConfig>,
    enable_request_compression: bool,
    include_timing_metrics: bool,
    beta_features_header: Option<String>,
    concurrent_reasoning_summaries_enabled: bool,
    include_attestation: bool,
    attestation_provider: Option<Arc<dyn AttestationProvider>>,
    agent_identity_session_fallback: AgentIdentitySessionFallback,
    http_clients: RouteAwareClientPool,
    cached_websocket_transport: StdMutex<WebsocketTransportCache>,
    request_schema_cache: StdMutex<RequestSchemaSerializationCache>,
    /// Prompt-context diagnostics ordered by request readiness across all thread sessions.
    ///
    /// Every turn-scoped [`ModelClientSession`] shares this cell, so a turn's first request is
    /// compared with the previous turn's final request instead of reporting no predecessor. The
    /// comparison still withholds reuse eligibility when the cache key, a dispatched setting such
    /// as the model or service tier, an ordered prefix section, or a fixed prefix item changed.
    prompt_context_measurements: Arc<StdMutex<PromptContextMeasurements>>,
    startup_timing: Option<Arc<crate::startup_timing::StartupTimingState>>,
}

#[derive(Debug, Default)]
struct PromptContextMeasurements {
    baseline: Option<PromptContextBaseline>,
    latest_attempt: Option<String>,
    predecessor: Option<oneshot::Receiver<Option<PromptContextBaseline>>>,
}

impl PromptContextMeasurements {
    fn begin(&mut self, physical_attempt_id: String) -> PromptContextMeasurementDispatch {
        let (sender, receiver) = oneshot::channel();
        let predecessor = self.predecessor.replace(receiver);
        self.latest_attempt = Some(physical_attempt_id.clone());
        PromptContextMeasurementDispatch {
            physical_attempt_id,
            predecessor,
            initial_baseline: self.baseline.clone(),
            sender,
        }
    }
}

struct PromptContextMeasurementDispatch {
    physical_attempt_id: String,
    predecessor: Option<oneshot::Receiver<Option<PromptContextBaseline>>>,
    initial_baseline: Option<PromptContextBaseline>,
    sender: oneshot::Sender<Option<PromptContextBaseline>>,
}

impl PromptContextMeasurementDispatch {
    async fn compare_and_publish(
        self,
        owner: &StdMutex<PromptContextMeasurements>,
        measurements: &mut ModelRequestMeasurements,
        prompt_cache_key: Option<&str>,
        prompt_digests: PromptDigests,
    ) {
        let baseline = measurements
            .compare_after_predecessor(
                self.predecessor,
                self.initial_baseline,
                prompt_cache_key,
                prompt_digests,
            )
            .await;
        {
            let mut owner = owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if owner.latest_attempt.as_deref() == Some(self.physical_attempt_id.as_str()) {
                owner.baseline = baseline.clone();
            }
        }
        // Cancellation or skipped diagnostics drop this sender, making the next
        // comparison unavailable rather than reusing an unrelated older baseline.
        let _ = self.sender.send(baseline);
    }
}

#[derive(Debug, Default)]
struct WebsocketTransportCache {
    epoch: u64,
    session: Option<WebsocketSession>,
    fallback: WebsocketFallbackState,
}

const WEBSOCKET_FALLBACK_COOLDOWN: Duration = Duration::from_secs(60);
const WEBSOCKET_FAILURE_WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Default)]
struct WebsocketFallbackState {
    disabled: bool,
    last_transient_failure: Option<tokio::time::Instant>,
}

impl WebsocketFallbackState {
    fn enabled(&self, now: tokio::time::Instant) -> bool {
        !self.disabled
            && self.last_transient_failure.is_none_or(|failed_at| {
                now.saturating_duration_since(failed_at) >= WEBSOCKET_FALLBACK_COOLDOWN
            })
    }

    fn record_failure(&mut self, recoverable: bool, now: tokio::time::Instant) {
        if !recoverable
            || self.last_transient_failure.is_some_and(|failed_at| {
                now.saturating_duration_since(failed_at) <= WEBSOCKET_FAILURE_WINDOW
            })
        {
            self.disabled = true;
        } else {
            self.last_transient_failure = Some(now);
        }
    }
}

impl WebsocketTransportCache {
    fn record_success(&mut self, epoch: u64) {
        // A completion from before a concurrent fallback must not erase that
        // newer failure or re-enable a permanently disabled transport.
        if self.epoch == epoch && !self.fallback.disabled {
            self.fallback.last_transient_failure = None;
        }
    }

    fn publish_if_current(
        &mut self,
        permit: WebsocketCachePublicationPermit,
        websocket_session: WebsocketSession,
    ) -> bool {
        if self.epoch != permit.epoch || self.session.is_some() {
            return false;
        }
        self.session = Some(websocket_session);
        true
    }
}

#[derive(Debug, Clone, Copy)]
struct WebsocketCachePublicationPermit {
    epoch: u64,
}

const REQUEST_SCHEMA_CACHE_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RequestSchemaCacheKey([u8; 32]);

#[derive(Debug, Clone)]
struct RequestSchemaCacheValue {
    tools: Arc<[serde_json::Value]>,
    text: Option<TextControls>,
}

#[derive(Debug)]
struct RequestSchemaSerializationCache {
    entries: LruCache<RequestSchemaCacheKey, RequestSchemaCacheValue>,
    #[cfg(test)]
    hits: u64,
    #[cfg(test)]
    misses: u64,
}

impl Default for RequestSchemaSerializationCache {
    fn default() -> Self {
        Self {
            entries: LruCache::new(
                NonZeroUsize::new(REQUEST_SCHEMA_CACHE_CAPACITY).unwrap_or(NonZeroUsize::MIN),
            ),
            #[cfg(test)]
            hits: 0,
            #[cfg(test)]
            misses: 0,
        }
    }
}

impl RequestSchemaSerializationCache {
    fn get(&mut self, key: RequestSchemaCacheKey) -> Option<RequestSchemaCacheValue> {
        let value = self.entries.get(&key)?.clone();
        #[cfg(test)]
        {
            self.hits = self.hits.saturating_add(1);
        }
        Some(value)
    }

    #[cfg(test)]
    fn record_miss(&mut self) {
        self.misses = self.misses.saturating_add(1);
    }

    #[cfg(test)]
    fn diagnostics(&self) -> (u64, u64, usize) {
        (self.hits, self.misses, self.entries.len())
    }

    fn insert(&mut self, key: RequestSchemaCacheKey, value: RequestSchemaCacheValue) {
        self.entries.put(key, value);
    }
}

/// Resolved API client setup for a single request attempt.
///
/// Keeping this as a single bundle ensures prewarm and normal request paths
/// share the same auth/provider setup flow.
struct CurrentClientSetup {
    auth: Option<CodexAuth>,
    api_provider: ApiProvider,
    api_auth: SharedAuthProvider,
    agent_identity_telemetry: Option<AgentIdentityTelemetry>,
}

pub(crate) type AttemptPreparedCallback = Arc<
    dyn Fn(
            ResponseAttemptIdentity,
        )
            -> futures::future::BoxFuture<'static, Result<Option<tokio::sync::OwnedMutexGuard<()>>>>
        + Send
        + Sync,
>;

#[derive(Clone, Copy)]
struct RequestRouteTelemetry {
    endpoint: &'static str,
}

impl RequestRouteTelemetry {
    fn for_endpoint(endpoint: &'static str) -> Self {
        Self { endpoint }
    }
}

/// A session-scoped client for model-provider API calls.
///
/// This holds configuration and state that should be shared across turns within a Codex session
/// (auth, provider selection, thread id, transport fallback state, and the prompt-context
/// diagnostic baseline that a turn's first request is compared against).
///
/// WebSocket fallback is session-scoped. A transient failure uses HTTP for 60 seconds; a second
/// failure within 30 minutes without an intervening successful request, or a non-transient
/// failure, disables WebSockets for the session.
///
/// Turn-scoped settings (model selection, reasoning controls, telemetry context, and turn
/// metadata) are passed explicitly to the relevant methods to keep turn lifetime visible at the
/// call site.
///
#[derive(Debug, Clone)]
pub struct ModelClient {
    state: Arc<ModelClientState>,
    agent_identity_policy: AgentIdentityAuthPolicy,
    http_client_factory: HttpClientFactory,
}

/// A turn-scoped streaming session created from a [`ModelClient`].
///
/// The session establishes a Responses WebSocket connection lazily and reuses it across multiple
/// requests within the turn. It also caches per-turn state:
///
/// - The last full request, so subsequent calls can reuse incremental websocket request payloads
///   only when the current request is an incremental extension of the previous one.
/// - The `x-codex-turn-state` sticky-routing token, which must be replayed for all requests within
///   the same turn.
///
/// Create a fresh `ModelClientSession` for each Codex turn. Reusing it across turns would replay
/// the previous turn's sticky-routing token into the next turn, which violates the client/server
/// contract and can cause routing bugs.
pub struct ModelClientSession {
    client: ModelClient,
    websocket_session: WebsocketSession,
    websocket_cache_publication: Option<WebsocketCachePublicationPermit>,
    pending_websocket_preconnect: Option<AbortOnDropHandle<Result<Option<WebsocketSession>>>>,
    preconnect_failure_setup: Option<Arc<StdMutex<Option<WebsocketSetupFingerprint>>>>,
    failed_websocket_setup: Option<WebsocketSetupFingerprint>,
    dispatched_account: Option<crate::responses_retry::AccountChangeIdentity>,
    preconnect_failure_credit: bool,
    prepared_startup_websocket_attempt: Option<PreparedStartupWebsocketAttempt>,
    /// A receipt-bearing request invalidated provider inheritance. Subsequent
    /// requests must keep using the unreplaced logical history while those
    /// substitutions remain, though they may inherit a replacement response.
    tool_history_fail_open_pending: bool,
    /// Transport selected for the current stream, including failed connection attempts.
    /// `None` means no stream has been selected yet.
    last_stream_transport: Option<ModelAttemptTransport>,
    /// Once a prepared turn selects HTTP, cooldown expiry must not put another
    /// WebSocket recovery cycle in front of that turn's continuations.
    http_for_turn: bool,
    effective_input: Option<Arc<[ResponseItem]>>,
    turn_timing: Option<Arc<TurnTimingState>>,
    startup_timing: Option<Arc<crate::startup_timing::StartupTimingState>>,
    logical_sampling_request_count: u32,
    /// Whether the latest dispatch advanced `logical_sampling_request_count`, so an outer
    /// replay of that same request can reuse its Initial/Continuation kind.
    last_logical_request_counted: bool,
    /// Baseline publication and pending predecessor ownership are thread-scoped together.
    /// A new turn can follow a completed response whose optional diagnostics are still pending.
    prompt_context_measurements: Arc<StdMutex<PromptContextMeasurements>>,
    /// Turn state for sticky routing.
    ///
    /// This is an `OnceLock` that stores the turn state value received from the server
    /// on turn start via the `x-codex-turn-state` response header. Once set, this value
    /// should be sent back to the server in the `x-codex-turn-state` request header for
    /// all subsequent requests within the same turn to maintain sticky routing.
    ///
    /// This is a contract between the client and server: we receive it at turn start,
    /// keep sending it unchanged between turn requests (e.g., for retries, incremental
    /// appends, or continuation requests), and must not send it between different turns.
    /// The exception is a stream throughput collapse, which replaces the token together
    /// with the websocket so the next full request can be routed afresh.
    turn_state: Arc<OnceLock<String>>,
}

#[derive(Debug, Clone)]
struct LastResponse {
    response_id: String,
    items_added: Arc<[ResponseItem]>,
}

const WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION: u16 = 1;
const WEBSOCKET_HISTORY_HASH_DOMAIN: &[u8] = b"codex.websocket.history.v1";
const WEBSOCKET_REQUEST_FINGERPRINT_DOMAIN: &[u8] = b"codex.websocket.request-properties.v1";
const WEBSOCKET_SETUP_FINGERPRINT_DOMAIN: &[u8] = b"codex.websocket.setup.v1";

fn normalized_websocket_history_item(item: &ResponseItem) -> std::borrow::Cow<'_, ResponseItem> {
    if item.internal_chat_message_metadata_passthrough().is_none() {
        return std::borrow::Cow::Borrowed(item);
    }
    let mut normalized = item.clone();
    normalized.clear_internal_chat_message_metadata_passthrough();
    std::borrow::Cow::Owned(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalPrefixHash {
    item_count: usize,
    digest: [u8; 32],
}

impl CanonicalPrefixHash {
    fn empty() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(WEBSOCKET_HISTORY_HASH_DOMAIN);
        hasher.update(WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION.to_be_bytes());
        Self {
            item_count: 0,
            digest: hasher.finalize().into(),
        }
    }

    #[cfg(test)]
    fn from_items(items: &[ResponseItem]) -> serde_json::Result<Self> {
        Self::from_iter(items.iter())
    }

    fn from_iter<'a>(
        items: impl IntoIterator<Item = &'a ResponseItem>,
    ) -> serde_json::Result<Self> {
        let mut prefix = Self::empty();
        prefix.extend_iter(items)?;
        Ok(prefix)
    }

    fn extend_items(&mut self, items: &[ResponseItem]) -> serde_json::Result<()> {
        self.extend_iter(items.iter())
    }

    fn extend_iter<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a ResponseItem>,
    ) -> serde_json::Result<()> {
        // Reuse scratch storage within this pass, not across requests. Large
        // histories otherwise allocate and grow a fresh JSON buffer per item.
        let mut serialized = Vec::new();
        for item in items {
            #[cfg(test)]
            AUDIT143_PREFIX_ITEMS_HASHED.with(|count| count.set(count.get() + 1));
            let normalized = normalized_websocket_history_item(item);
            serialized.clear();
            serde_json::to_writer(&mut serialized, &normalized)?;
            let mut hasher = Sha256::new();
            hasher.update(WEBSOCKET_HISTORY_HASH_DOMAIN);
            hasher.update(self.digest);
            hasher.update((serialized.len() as u64).to_be_bytes());
            hasher.update(&serialized);
            self.digest = hasher.finalize().into();
            self.item_count = self.item_count.saturating_add(1);
        }
        Ok(())
    }
}

/// Hashes verified against the exact current request during preparation. This
/// value travels with that request and is discarded whenever its input changes.
#[cfg(test)]
thread_local! { static AUDIT143_PREFIX_ITEMS_HASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

struct VerifiedRequestHistory {
    prefix: CanonicalPrefixHash,
    properties: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct WebsocketHistoryBaseline {
    /// Monotonic diagnostic identity for the immutable request prefix. Equality
    /// of this generation is never used as proof of model-visible equivalence.
    generation: u64,
    request_prefix: CanonicalPrefixHash,
    request_properties_fingerprint: [u8; 32],
    stable_context_fingerprint: [u8; 32],
    provider_response_id_established: bool,
    normalization_policy_version: u16,
}

/// The Responses websocket ends every connection after 60 minutes and fails any
/// response still streaming on it. Rotate at a request boundary before then.
const WEBSOCKET_ROTATION_AGE: Duration = Duration::from_secs(55 * 60);
const SPAWNED_AGENT_ATTESTATION_WAIT: Duration = Duration::from_millis(100);

fn websocket_connection_expiring(connected_at: Option<Instant>, now: Instant) -> bool {
    connected_at.is_some_and(|connected_at| {
        now.saturating_duration_since(connected_at) >= WEBSOCKET_ROTATION_AGE
    })
}

/// A turn stays on the backend selected by its sticky turn-state token, so a turn
/// whose output rate collapses keeps streaming slowly until it ends. Responses
/// below this size say too little about the stream rate to be judged.
const STREAM_THROUGHPUT_MIN_OUTPUT_TOKENS: i64 = 150;
const STREAM_THROUGHPUT_WINDOW: usize = 9;
const STREAM_THROUGHPUT_MIN_BASELINE_SAMPLES: usize = 5;
const STREAM_THROUGHPUT_COLLAPSE_RATIO: f64 = 0.5;
const STREAM_THROUGHPUT_COLLAPSE_STREAK: u8 = 2;
/// Responses to observe on a replacement connection before replacing it again.
/// A provider-wide slowdown fills the window meanwhile and becomes the baseline.
const STREAM_THROUGHPUT_ROTATION_COOLDOWN: u8 = 4;
/// Rotating discards the provider's inherited prefix, so the next request
/// replays the whole conversation uncached. Measured sessions re-bill
/// 180k-328k input tokens per rotation at a large context while the escaped
/// slowdown costs far less, so a collapse only warrants rotation when the
/// expected output-time saving outweighs replaying that prefix.
///
/// Prefill of replayed input is much cheaper per token than generating output,
/// so the replay is charged at a conservative fraction of the observed output
/// rate rather than token-for-token.
const STREAM_REPLAY_PREFILL_SPEEDUP: f64 = 10.0;
/// Output a replacement connection is expected to serve before the turn ends.
/// Rotation only pays off across the responses that follow it, so the saving is
/// credited over this horizon instead of the single collapsed response.
const STREAM_ROTATION_PAYOFF_OUTPUT_TOKENS: f64 = 4096.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct StreamThroughputCollapse {
    observed_tokens_per_second: f64,
    baseline_tokens_per_second: f64,
    /// Input tokens the collapsed response billed, including the cached
    /// prefix. Rotation would resend this prefix uncached.
    inherited_input_tokens: i64,
}

impl StreamThroughputCollapse {
    /// Whether replacing the connection is expected to recover more time than
    /// replaying the inherited prefix uncached costs.
    ///
    /// Both sides are expressed in seconds of the current connection's own
    /// observed rate, so a provider-wide slowdown cancels out instead of
    /// making rotation look cheaper.
    fn rotation_saves_more_than_replay(&self) -> bool {
        if self.observed_tokens_per_second <= 0.0 || self.inherited_input_tokens <= 0 {
            // Without a usable rate or context measurement, keep the existing
            // escape behavior rather than pinning the turn to a bad backend.
            return true;
        }
        let recovered_tokens_per_second =
            self.baseline_tokens_per_second - self.observed_tokens_per_second;
        if recovered_tokens_per_second <= 0.0 {
            return false;
        }
        let saved_seconds = STREAM_ROTATION_PAYOFF_OUTPUT_TOKENS / self.observed_tokens_per_second
            - STREAM_ROTATION_PAYOFF_OUTPUT_TOKENS / self.baseline_tokens_per_second;
        let replay_seconds = self.inherited_input_tokens as f64
            / (self.observed_tokens_per_second * STREAM_REPLAY_PREFILL_SPEEDUP);
        saved_seconds > replay_seconds
    }
}

/// Output rates of completed websocket responses, judged against their recent median.
#[derive(Debug, Default)]
struct WebsocketStreamThroughput {
    workload: Option<serde_json::Value>,
    recent_tokens_per_second: VecDeque<(i64, f64)>,
    collapsed_streak: u8,
    latest_collapse: Option<StreamThroughputCollapse>,
    responses_since_rotation: Option<u8>,
}

impl WebsocketStreamThroughput {
    fn select_workload(&mut self, workload: serde_json::Value) {
        if self.workload.as_ref() != Some(&workload) {
            self.workload = Some(workload);
            self.recent_tokens_per_second.clear();
            self.collapsed_streak = 0;
            self.latest_collapse = None;
            // A model switch must not bypass the connection-rotation cooldown.
        }
    }

    fn record(
        &mut self,
        output_tokens: i64,
        elapsed: Duration,
        input_tokens: i64,
        sustained_delivery_interval: bool,
    ) {
        if !sustained_delivery_interval {
            // Total usage over request-to-completion includes queue/prefill and
            // hidden reasoning. It is advisory, never a socket-rotation signal.
            // No transport currently supplies a compatible token/delivery interval.
            self.collapsed_streak = 0;
            self.latest_collapse = None;
            return;
        }
        if output_tokens < STREAM_THROUGHPUT_MIN_OUTPUT_TOKENS || elapsed.is_zero() {
            return;
        }
        let tokens_per_second = output_tokens as f64 / elapsed.as_secs_f64();
        self.latest_collapse = self
            .baseline_tokens_per_second(output_tokens)
            .filter(|baseline| tokens_per_second < baseline * STREAM_THROUGHPUT_COLLAPSE_RATIO)
            .map(|baseline_tokens_per_second| StreamThroughputCollapse {
                observed_tokens_per_second: tokens_per_second,
                baseline_tokens_per_second,
                inherited_input_tokens: input_tokens,
            });
        self.collapsed_streak = if self.latest_collapse.is_some() {
            self.collapsed_streak.saturating_add(1)
        } else {
            0
        };
        if self.recent_tokens_per_second.len() == STREAM_THROUGHPUT_WINDOW {
            self.recent_tokens_per_second.pop_front();
        }
        self.recent_tokens_per_second
            .push_back((output_tokens, tokens_per_second));
        if let Some(responses) = self.responses_since_rotation.as_mut() {
            *responses = responses.saturating_add(1);
        }
    }

    fn baseline_tokens_per_second(&self, output_tokens: i64) -> Option<f64> {
        // Only compare similar lengths after the caller establishes compatible
        // token/interval provenance. Request-average rates never enter this history.
        let mut sorted = self
            .recent_tokens_per_second
            .iter()
            .filter(|(tokens, _)| {
                *tokens <= output_tokens.saturating_mul(2)
                    && output_tokens <= tokens.saturating_mul(2)
            })
            .map(|(_, rate)| *rate)
            .collect::<Vec<_>>();
        if sorted.len() < STREAM_THROUGHPUT_MIN_BASELINE_SAMPLES {
            return None;
        }
        sorted.sort_by(f64::total_cmp);
        Some(sorted[sorted.len() / 2])
    }

    /// Returns the collapse that warrants replacing the connection, at most once per cooldown.
    ///
    /// A collapse whose rotation would cost more in replayed input than it
    /// recovers in output time is not returned, and leaves the cooldown and
    /// streak untouched: the slowdown is real but escaping it is not worth
    /// resending the inherited prefix uncached. A later collapse at a smaller
    /// context, or a worse one at the same context, can still rotate.
    fn take_collapse(&mut self) -> Option<StreamThroughputCollapse> {
        if self.collapsed_streak < STREAM_THROUGHPUT_COLLAPSE_STREAK
            || self
                .responses_since_rotation
                .is_some_and(|responses| responses < STREAM_THROUGHPUT_ROTATION_COOLDOWN)
        {
            return None;
        }
        if self
            .latest_collapse
            .is_some_and(|collapse| !collapse.rotation_saves_more_than_replay())
        {
            return None;
        }
        self.collapsed_streak = 0;
        self.responses_since_rotation = Some(0);
        self.latest_collapse.take()
    }
}

#[derive(Debug, Default)]
struct WebsocketSession {
    connection: Option<ApiWebSocketConnection>,
    /// When `connection` was established; drives rotation before the server's age limit.
    connected_at: Option<Instant>,
    /// Shared with each response stream, which reports its rate on completion.
    stream_throughput: Arc<StdMutex<WebsocketStreamThroughput>>,
    setup_fingerprint: Option<WebsocketSetupFingerprint>,
    last_request: Option<ResponsesApiRequest>,
    last_request_history: Option<WebsocketHistoryBaseline>,
    next_history_generation: u64,
    last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    last_response: Option<Arc<LastResponse>>,
    last_response_from_untraced_warmup: bool,
    connection_reused: StdMutex<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WebsocketSetupFingerprint([u8; 32]);

struct PreparedStartupWebsocketAttempt {
    client_setup: CurrentClientSetup,
    transport_readiness_guard: Option<TurnTimingGuard>,
    request_transformation_guard: Option<TurnTimingGuard>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupPrewarmClaim {
    ResponseChain,
    TransportOnly,
    Rejected,
}

// Input is compared through the normalized prefix chain. Delivery-only stream
// options and client metadata do not affect previous_response_id reuse. Keep
// this destructuring exhaustive so new fields require an explicit reuse decision.
// Both equality and fingerprinting use this same ordered projection.
fn responses_request_reuse_properties(
    request: &ResponsesApiRequest,
) -> impl serde::Serialize + PartialEq + '_ {
    let ResponsesApiRequest {
        model,
        instructions,
        input: _,
        tools,
        tool_choice,
        parallel_tool_calls,
        reasoning,
        store,
        stream,
        stream_options: _,
        include,
        service_tier,
        prompt_cache_key,
        text,
        client_metadata: _,
    } = request;
    (
        model,
        instructions,
        tools,
        tool_choice,
        parallel_tool_calls,
        reasoning,
        store,
        stream,
        include,
        service_tier,
        prompt_cache_key,
        text,
    )
}

fn responses_request_properties_fingerprint(
    request: &ResponsesApiRequest,
) -> serde_json::Result<[u8; 32]> {
    let serialized = serde_json::to_vec(&responses_request_reuse_properties(request))?;
    let mut hasher = Sha256::new();
    hasher.update(WEBSOCKET_REQUEST_FINGERPRINT_DOMAIN);
    hasher.update(WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION.to_be_bytes());
    hasher.update((serialized.len() as u64).to_be_bytes());
    hasher.update(serialized);
    Ok(hasher.finalize().into())
}

fn websocket_setup_fingerprint(
    provider: &ApiProvider,
    api_auth: &dyn AuthProvider,
) -> Option<WebsocketSetupFingerprint> {
    let websocket_url = provider.websocket_url_for_path("responses").ok()?;
    let mut headers = provider.headers.clone();
    api_auth.try_add_auth_headers(&mut headers).ok()?;
    let connection_identity = api_auth.connection_identity();
    if connection_identity.is_some() {
        headers.remove(http::header::AUTHORIZATION);
    }
    let mut canonical_headers = headers
        .iter()
        .map(|(name, value)| (name.as_str().as_bytes(), value.as_bytes()))
        .collect::<Vec<_>>();
    canonical_headers.sort_unstable();

    let mut hasher = Sha256::new();
    hasher.update(WEBSOCKET_SETUP_FINGERPRINT_DOMAIN);
    if let Some(identity) = connection_identity {
        hasher.update((identity.len() as u64).to_be_bytes());
        hasher.update(identity);
    }
    hasher.update((provider.name.len() as u64).to_be_bytes());
    hasher.update(provider.name.as_bytes());
    hasher.update((websocket_url.as_str().len() as u64).to_be_bytes());
    hasher.update(websocket_url.as_str().as_bytes());
    for (name, value) in canonical_headers {
        hasher.update((name.len() as u64).to_be_bytes());
        hasher.update(name);
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    Some(WebsocketSetupFingerprint(hasher.finalize().into()))
}

// This is intentionally not a `PartialEq` implementation: request equality includes `input` and
// `client_metadata`, while websocket reuse compares the input separately and ignores metadata.
fn responses_request_properties_match(
    previous: &ResponsesApiRequest,
    current: &ResponsesApiRequest,
) -> bool {
    responses_request_reuse_properties(previous) == responses_request_reuse_properties(current)
}

// Value equality ignores object insertion order, while the reuse fingerprint
// includes it. Check the JSON-bearing properties before inheriting that digest.
fn responses_request_json_serialization_matches(
    previous: &ResponsesApiRequest,
    current: &ResponsesApiRequest,
) -> bool {
    let tools_match = match (&previous.tools, &current.tools) {
        (Some(previous), Some(current)) => {
            Arc::ptr_eq(previous, current)
                || (previous.len() == current.len()
                    && previous.iter().zip(current.iter()).all(|(previous, current)| {
                        ordered_json_values_match(previous, current)
                    }))
        }
        (None, None) => true,
        _ => false,
    };
    fn schema(text: &Option<TextControls>) -> Option<&serde_json::Value> {
        text.as_ref()
            .and_then(|text| text.format.as_ref())
            .map(|format| &format.schema)
    }
    tools_match
        && match (schema(&previous.text), schema(&current.text)) {
            (Some(previous), Some(current)) => ordered_json_values_match(previous, current),
            (None, None) => true,
            _ => false,
        }
}

fn ordered_json_values_match(previous: &serde_json::Value, current: &serde_json::Value) -> bool {
    if std::ptr::eq(previous, current) {
        return true;
    }
    match (previous, current) {
        (serde_json::Value::Object(previous), serde_json::Value::Object(current)) => {
            previous.len() == current.len()
                && previous.iter().zip(current.iter()).all(|((previous_key, previous), (current_key, current))| {
                    previous_key == current_key && ordered_json_values_match(previous, current)
                })
        }
        (serde_json::Value::Array(previous), serde_json::Value::Array(current)) => {
            previous.len() == current.len()
                && previous.iter().zip(current.iter()).all(|(previous, current)| {
                    ordered_json_values_match(previous, current)
                })
        }
        // The workspace enables arbitrary_precision, retaining the exact lexeme.
        (serde_json::Value::Number(previous), serde_json::Value::Number(current)) => {
            previous.as_str() == current.as_str()
        }
        _ => previous == current,
    }
}

fn rebase_empty_startup_prewarm_stable_context(
    baseline: &mut WebsocketHistoryBaseline,
    previous_request_input_empty: bool,
    stable_context_fingerprint: [u8; 32],
) -> bool {
    if baseline.stable_context_fingerprint == stable_context_fingerprint {
        return true;
    }
    if baseline.request_prefix.item_count != 0 || !previous_request_input_empty {
        return false;
    }
    baseline.stable_context_fingerprint = stable_context_fingerprint;
    true
}

impl WebsocketSession {
    async fn is_compatible_startup_prewarm(
        &mut self,
        setup_fingerprint: Option<WebsocketSetupFingerprint>,
        request: &ResponsesApiRequest,
        stable_context_fingerprint: [u8; 32],
    ) -> bool {
        let (Some(setup_fingerprint), Some(connection), Some(previous_request), Some(baseline)) = (
            setup_fingerprint,
            self.connection.as_ref(),
            self.last_request.as_ref(),
            self.last_request_history.as_ref(),
        ) else {
            return false;
        };
        if self.setup_fingerprint != Some(setup_fingerprint)
            || connection.is_closed().await
            || !self.last_response_from_untraced_warmup
            || (self.last_response_rx.is_none() && self.last_response.is_none())
            || baseline.normalization_policy_version
                != WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION
            || !responses_request_properties_match(previous_request, request)
            || responses_request_properties_fingerprint(request).ok()
                != Some(baseline.request_properties_fingerprint)
            || request.input.len() < baseline.request_prefix.item_count
        {
            return false;
        }
        if CanonicalPrefixHash::from_iter(
            request
                .input
                .iter()
                .take(baseline.request_prefix.item_count),
        )
        .ok()
            != Some(baseline.request_prefix)
        {
            return false;
        }

        // The startup request deliberately has an empty input prefix. It therefore cannot have
        // established any provider-side stable context: the complete first-turn input will still
        // be sent as the delta. Rebase only that empty proof to the realized first-turn manifest.
        // A non-empty warmup prefix remains fail-closed because it may already have established
        // context that the projected request omits.
        let previous_request_input_empty = previous_request.input.is_empty();
        let Some(baseline) = self.last_request_history.as_mut() else {
            return false;
        };
        rebase_empty_startup_prewarm_stable_context(
            baseline,
            previous_request_input_empty,
            stable_context_fingerprint,
        )
    }

    async fn has_compatible_startup_transport(
        &self,
        setup_fingerprint: Option<WebsocketSetupFingerprint>,
    ) -> bool {
        let (Some(setup_fingerprint), Some(connection)) =
            (setup_fingerprint, self.connection.as_ref())
        else {
            return false;
        };
        self.setup_fingerprint == Some(setup_fingerprint) && !connection.is_closed().await
    }

    fn into_transport_only(mut self) -> Self {
        self.last_request = None;
        self.last_request_history = None;
        self.next_history_generation = 0;
        self.last_response_rx = None;
        self.last_response = None;
        self.last_response_from_untraced_warmup = false;
        self.set_connection_reused(/*connection_reused*/ false);
        self
    }

    fn set_connection_reused(&self, connection_reused: bool) {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = connection_reused;
    }

    fn connection_reused(&self) -> bool {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

enum WebsocketStreamOutcome {
    Stream(ResponseStream),
    FallbackToHttp,
}

impl ModelClient {
    #[allow(clippy::too_many_arguments)]
    /// Creates a new session-scoped `ModelClient`.
    ///
    /// All arguments are expected to be stable for the lifetime of a Codex session. Per-turn values
    /// are passed to [`ModelClientSession::stream`] (and other turn-scoped methods) explicitly. The
    /// HTTP client factory must come from the effective session configuration so every transport
    /// observes the resolved outbound proxy policy.
    pub fn new(
        auth_manager: Option<Arc<AuthManager>>,
        agent_identity_policy: AgentIdentityAuthPolicy,
        thread_id: ThreadId,
        provider_info: ModelProviderInfo,
        session_source: SessionSource,
        originator: String,
        model_verbosity: Option<VerbosityConfig>,
        enable_request_compression: bool,
        include_timing_metrics: bool,
        beta_features_header: Option<String>,
        concurrent_reasoning_summaries_enabled: bool,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
        http_client_factory: HttpClientFactory,
    ) -> Self {
        let model_provider = create_model_provider(provider_info, auth_manager);
        let codex_api_key_env_enabled = model_provider
            .auth_manager()
            .as_ref()
            .is_some_and(|manager| manager.codex_api_key_env_enabled());
        let auth_env_telemetry =
            collect_auth_env_telemetry(model_provider.info(), codex_api_key_env_enabled);
        let include_attestation = model_provider.supports_attestation();
        let http_clients = RouteAwareClientPool::with_builder(
            http_client_factory.clone(),
            ClientRouteClass::Api,
            model_http_client_builder(codex_login::default_client::default_headers()),
        );
        Self {
            state: Arc::new(ModelClientState {
                thread_id,
                provider: model_provider,
                auth_env_telemetry,
                session_source,
                originator,
                model_verbosity,
                enable_request_compression,
                include_timing_metrics,
                beta_features_header,
                concurrent_reasoning_summaries_enabled,
                include_attestation,
                attestation_provider,
                agent_identity_session_fallback: AgentIdentitySessionFallback::default(),
                http_clients,
                cached_websocket_transport: StdMutex::new(WebsocketTransportCache::default()),
                request_schema_cache: StdMutex::new(RequestSchemaSerializationCache::default()),
                prompt_context_measurements: Arc::default(),
                startup_timing: None,
            }),
            agent_identity_policy,
            http_client_factory,
        }
    }

    /// Attach shared transports immediately after construction, before cloning the client.
    pub(crate) fn with_shared_http_clients(
        mut self,
        shared: Option<&SharedModelHttpClients>,
    ) -> Self {
        if let Some(shared) = shared {
            #[expect(
                clippy::expect_used,
                reason = "this construction-only builder requires an unshared client; cloning before attachment violates its contract"
            )]
            let state = Arc::get_mut(&mut self.state)
                .expect("shared HTTP transports must be attached before cloning the model client");
            state.http_clients = shared.pool(
                &self.http_client_factory,
                codex_login::default_client::default_headers(),
            );
        }
        self
    }

    fn prompt_cache_key(&self, metadata: &CodexResponsesMetadata) -> String {
        // Match the session-id affinity sent on both transports. Child threads
        // share the agent-tree session, not their distinct conversation IDs.
        metadata.session_id.clone()
    }

    /// Attach before cloning, so regular and compaction sessions share the
    /// same once-only startup cut without retaining the owning Session.
    pub(crate) fn with_startup_timing(
        mut self,
        timing: Arc<crate::startup_timing::StartupTimingState>,
    ) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "construction-only builder, before cloning"
        )]
        let state =
            Arc::get_mut(&mut self.state).expect("startup observer attached before cloning");
        state.startup_timing = Some(timing);
        self
    }

    /// Creates a fresh turn-scoped streaming session.
    ///
    /// This constructor does not perform network I/O itself; the session opens a websocket lazily
    /// when the first stream request is issued.
    pub fn new_session(&self) -> ModelClientSession {
        let (websocket_session, websocket_cache_publication) =
            self.take_cached_websocket_transport();
        ModelClientSession {
            client: self.clone(),
            websocket_session,
            websocket_cache_publication: Some(websocket_cache_publication),
            pending_websocket_preconnect: None,
            preconnect_failure_setup: None,
            failed_websocket_setup: None,
            dispatched_account: None,
            preconnect_failure_credit: false,
            prepared_startup_websocket_attempt: None,
            tool_history_fail_open_pending: false,
            last_stream_transport: None,
            http_for_turn: false,
            effective_input: None,
            turn_timing: None,
            startup_timing: self.state.startup_timing.clone(),
            logical_sampling_request_count: 0,
            last_logical_request_counted: false,
            prompt_context_measurements: Arc::clone(&self.state.prompt_context_measurements),
            turn_state: Arc::new(OnceLock::new()),
        }
    }

    /// Creates a session whose transport may only become reusable by being adopted by a normal
    /// turn-scoped session. A losing or cancelled speculative owner must never publish on drop.
    pub(crate) fn new_speculative_session(&self) -> ModelClientSession {
        ModelClientSession {
            client: self.clone(),
            websocket_session: WebsocketSession::default(),
            websocket_cache_publication: None,
            pending_websocket_preconnect: None,
            preconnect_failure_setup: None,
            failed_websocket_setup: None,
            dispatched_account: None,
            preconnect_failure_credit: false,
            prepared_startup_websocket_attempt: None,
            tool_history_fail_open_pending: false,
            last_stream_transport: None,
            http_for_turn: false,
            effective_input: None,
            turn_timing: None,
            startup_timing: None,
            logical_sampling_request_count: 0,
            last_logical_request_counted: false,
            prompt_context_measurements: Arc::clone(&self.state.prompt_context_measurements),
            turn_state: Arc::new(OnceLock::new()),
        }
    }

    fn take_cached_websocket_transport(
        &self,
    ) -> (WebsocketSession, WebsocketCachePublicationPermit) {
        let mut cache = self
            .state
            .cached_websocket_transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            cache.session.take().unwrap_or_default(),
            WebsocketCachePublicationPermit { epoch: cache.epoch },
        )
    }

    fn try_store_cached_websocket_transport(
        &self,
        permit: WebsocketCachePublicationPermit,
        websocket_session: WebsocketSession,
    ) -> bool {
        // An unfinished response still owns the socket. Its mapper cancellation
        // is asynchronous, so publishing it here can race the next turn's health
        // check and turn local cancellation into a provider/fallback failure.
        if websocket_session
            .connection
            .as_ref()
            .is_none_or(ApiWebSocketConnection::has_active_response)
        {
            return false;
        }
        let mut cache = self
            .state
            .cached_websocket_transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cache.fallback.enabled(tokio::time::Instant::now()) {
            return false;
        }
        cache.publish_if_current(permit, websocket_session.into_transport_only())
    }

    pub(crate) fn force_http_fallback(&self, session_telemetry: &SessionTelemetry) -> bool {
        self.activate_http_fallback(session_telemetry, /*recoverable*/ false)
    }

    fn activate_http_fallback(
        &self,
        session_telemetry: &SessionTelemetry,
        recoverable: bool,
    ) -> bool {
        let mut cache = self
            .state
            .cached_websocket_transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = tokio::time::Instant::now();
        let activated =
            self.state.provider.info().supports_websockets && cache.fallback.enabled(now);
        cache.fallback.record_failure(recoverable, now);
        // Invalidate cached and in-flight publishers atomically with the fallback decision.
        cache.epoch = cache.epoch.wrapping_add(1);
        cache.session = None;
        drop(cache);
        if activated {
            warn!("falling back to HTTP");
            session_telemetry.counter(
                "codex.transport.fallback_to_http",
                /*inc*/ 1,
                &[("from_wire_api", "responses_websocket")],
            );
        }

        activated
    }

    fn build_responses_compatibility_headers(
        &self,
        responses_metadata: &CodexResponsesMetadata,
    ) -> ApiHeaderMap {
        responses_metadata.compatibility_headers()
    }

    fn build_ws_client_metadata(
        &self,
        responses_metadata: &CodexResponsesMetadata,
        use_responses_lite: bool,
    ) -> HashMap<String, String> {
        let mut client_metadata = responses_metadata.client_metadata();
        if use_responses_lite {
            client_metadata.insert(
                WS_REQUEST_HEADER_RESPONSES_LITE_CLIENT_METADATA_KEY.to_string(),
                "true".to_string(),
            );
        }
        client_metadata
    }

    async fn generate_attestation_header_for(&self) -> Option<HeaderValue> {
        if !self.state.include_attestation {
            return None;
        }

        self.state
            .attestation_provider
            .as_ref()?
            .header_for_request(AttestationContext {
                thread_id: self.state.thread_id,
            })
            .await
    }

    fn build_reasoning(
        model_info: &ModelInfo,
        request_effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
    ) -> Option<Reasoning> {
        if request_effort.is_some() || model_info.supports_reasoning_summaries {
            Some(Reasoning {
                // Sampling and compaction callers resolve their raw
                // configured effort exactly once. In particular, do not feed an
                // already-normalized request effort (such as ultra -> max) back
                // through model capability normalization here.
                effort: request_effort,
                summary: if !model_info.supports_reasoning_summaries
                    || summary == ReasoningSummaryConfig::None
                {
                    None
                } else {
                    Some(summary)
                },
                // When Responses Lite is disabled, omit context so Responses uses the default,
                // which is currently `current_turn`.
                context: model_info
                    .use_responses_lite
                    .then_some(ReasoningContext::AllTurns),
            })
        } else {
            None
        }
    }

    fn request_schema_cache_key(
        prompt: &Prompt,
        verbosity: Option<VerbosityConfig>,
    ) -> serde_json::Result<RequestSchemaCacheKey> {
        let artifact_digest = prompt.tools.digest();
        let tool_identity = prompt
            .digests
            .tools
            .filter(|digest| *digest == artifact_digest)
            .unwrap_or(artifact_digest);
        let serialized = serde_json::to_vec(&(
            tool_identity,
            &prompt.output_schema,
            prompt.output_schema_strict,
            verbosity,
        ))?;
        let mut hasher = Sha256::new();
        hasher.update(b"codex.request-schema-cache.v1");
        hasher.update((serialized.len() as u64).to_be_bytes());
        hasher.update(serialized);
        Ok(RequestSchemaCacheKey(hasher.finalize().into()))
    }

    fn request_schema_components(
        &self,
        prompt: &Prompt,
        verbosity: Option<VerbosityConfig>,
        _use_responses_lite: bool,
    ) -> Result<RequestSchemaCacheValue> {
        let key = Self::request_schema_cache_key(prompt, verbosity)?;
        {
            let mut cache = self
                .state
                .request_schema_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(value) = cache.get(key) {
                trace!("request schema serialization cache hit");
                return Ok(value);
            }
            #[cfg(test)]
            cache.record_miss();
        }

        let value = RequestSchemaCacheValue {
            tools: create_tools_json_for_responses_api(prompt.tools.specs())?.into(),
            text: create_text_param_for_request(
                verbosity,
                &prompt.output_schema,
                prompt.output_schema_strict,
            ),
        };
        let mut cache = self
            .state
            .request_schema_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = cache.get(key) {
            // Another request populated the same immutable entry while this
            // request serialized outside the lock.
            return Ok(value);
        }
        cache.insert(key, value.clone());
        Ok(value)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_responses_request(
        &self,
        provider: &codex_api::Provider,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
    ) -> Result<ResponsesApiRequest> {
        self.build_responses_request_with_input(
            provider,
            prompt,
            model_info,
            effort,
            summary,
            service_tier,
            responses_metadata,
            /* use_stable_context_fallback */ false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_responses_request_with_input(
        &self,
        provider: &codex_api::Provider,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
        use_stable_context_fallback: bool,
    ) -> Result<ResponsesApiRequest> {
        self.build_responses_request_with_fallbacks(
            provider,
            prompt,
            model_info,
            effort,
            summary,
            service_tier,
            responses_metadata,
            use_stable_context_fallback,
            /* use_tool_history_fallback */ false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_responses_request_with_fallbacks(
        &self,
        provider: &codex_api::Provider,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
        use_stable_context_fallback: bool,
        use_tool_history_fallback: bool,
    ) -> Result<ResponsesApiRequest> {
        let mut input = if use_tool_history_fallback {
            prompt.get_formatted_tool_history_fallback_input_for_request(
                model_info.use_responses_lite,
                use_stable_context_fallback,
            )
        } else if use_stable_context_fallback {
            prompt.get_formatted_fallback_input_for_request(model_info.use_responses_lite)
        } else {
            prompt.get_formatted_input_for_request(model_info.use_responses_lite)
        };
        input = crate::image_preparation::for_model_image_detail(
            input,
            model_info.supports_image_detail_original,
        );
        let is_openai = self.state.provider.info().is_openai();
        if !is_openai
            && input
                .iter()
                .any(|item| item.internal_chat_message_metadata_passthrough().is_some())
        {
            Arc::make_mut(&mut input)
                .iter_mut()
                .for_each(ResponseItem::clear_internal_chat_message_metadata_passthrough);
        }
        let verbosity = if model_info.support_verbosity {
            self.state.model_verbosity.or(model_info.default_verbosity)
        } else {
            if self.state.model_verbosity.is_some() {
                warn!(
                    "model_verbosity is set but ignored as the model does not support verbosity: {}",
                    model_info.slug
                );
            }
            None
        };
        let RequestSchemaCacheValue { tools, text } =
            self.request_schema_components(prompt, verbosity, model_info.use_responses_lite)?;
        let mut prefix = Vec::new();
        let (instructions, tools) = if model_info.use_responses_lite {
            if !prompt.base_instructions.text.is_empty() {
                prefix.push(ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: prompt.base_instructions.text.clone(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            prefix.push(ResponseItem::AdditionalTools {
                id: None,
                role: "developer".to_string(),
                tools: tools.to_vec(),
            });
            (String::new(), None)
        } else {
            (prompt.base_instructions.text.clone(), Some(tools))
        };
        let stream_options = (self.state.concurrent_reasoning_summaries_enabled && is_openai)
            .then_some(StreamOptions {
                reasoning_summary_delivery: codex_api::ReasoningSummaryDelivery::SequentialCutoff,
            });
        let reasoning = Self::build_reasoning(model_info, effort, summary);
        let include = if reasoning.is_some() {
            vec!["reasoning.encrypted_content".to_string()]
        } else {
            Vec::new()
        };
        let prompt_cache_key = Some(self.prompt_cache_key(responses_metadata));
        let service_tier = model_info.service_tier_for_request(service_tier);
        let request = ResponsesApiRequest {
            model: model_info.slug.clone(),
            instructions,
            input: codex_api::ResponsesInput::with_prefix(prefix, input),
            tools,
            tool_choice: if prompt.tool_calls_disabled {
                "none"
            } else {
                "auto"
            }
            .to_string(),
            // Responses Lite requires this field to be false even when the model
            // advertises parallel tool calls. Apply the constraint here for both
            // HTTP and WebSocket requests, including fallback requests.
            parallel_tool_calls: prompt.parallel_tool_calls && !model_info.use_responses_lite,
            reasoning,
            store: provider.is_azure_responses_endpoint(),
            stream: true,
            stream_options,
            include,
            service_tier,
            prompt_cache_key,
            text,
            client_metadata: Some(responses_metadata.client_metadata()),
        };
        Ok(request)
    }

    fn prepare_response_items_for_request(&self, input: &mut codex_api::ResponsesInput) {
        let strip_all_ids = !self.state.provider.info().is_openai();
        let requires_change = input.iter().any(|item| {
            item.id()
                .is_some_and(|id| strip_all_ids || !id.is_prefixed())
        });
        if !requires_change {
            return;
        }
        input.update_segments(|input| {
            if !input.iter().any(|item| {
                item.id()
                    .is_some_and(|id| strip_all_ids || !id.is_prefixed())
            }) {
                return;
            }
            for item in Arc::make_mut(input).iter_mut() {
                if item
                    .id()
                    .is_some_and(|id| strip_all_ids || !id.is_prefixed())
                {
                    item.set_id(/*new_id*/ None);
                }
            }
        });
    }

    /// Returns whether the Responses-over-WebSocket transport is active for this session.
    ///
    /// WebSocket use is controlled by provider capability and session-scoped fallback state.
    pub fn responses_websocket_enabled(&self) -> bool {
        self.state.provider.info().supports_websockets
            && self
                .state
                .cached_websocket_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fallback
                .enabled(tokio::time::Instant::now())
    }

    pub(crate) fn startup_websocket_enabled(&self) -> bool {
        self.responses_websocket_enabled()
            && (!self.state.include_attestation
                || self
                    .state
                    .attestation_provider
                    .as_ref()
                    .is_none_or(|provider| provider.supports_startup_requests()))
    }

    /// Returns auth + provider configuration resolved from the current session auth state.
    ///
    /// This centralizes setup used by both prewarm and normal request paths so they stay in
    /// lockstep when auth/provider resolution changes.
    async fn current_client_setup(&self) -> Result<CurrentClientSetup> {
        let setup = self
            .state
            .provider
            .resolve_client_setup(ProviderAuthScope {
                agent_identity_policy: self.agent_identity_policy,
                session_source: self.state.session_source.clone(),
                agent_identity_session_fallback: self.state.agent_identity_session_fallback.clone(),
            })
            .await?;
        Ok(CurrentClientSetup {
            auth: setup.auth,
            api_provider: setup.api_provider,
            api_auth: setup.resolved_auth.auth,
            agent_identity_telemetry: setup.resolved_auth.agent_identity_telemetry,
        })
    }

    async fn build_api_transport(
        &self,
        api_provider: &ApiProvider,
        endpoint: &str,
    ) -> Result<ReqwestTransport> {
        let request_url = api_provider.url_for_path(endpoint);
        self.state
            .http_clients
            .client_for_url(&request_url)
            .await
            .map_err(std::io::Error::other)?;
        // Keep the eager setup/error boundary, but let the pool select the route
        // for every dispatched URL and redirect hop under either proxy policy.
        // Arbitrary sensitive provider headers must not follow cross-origin hops.
        Ok(ReqwestTransport::from_client_pool(
            self.state.http_clients.clone(),
        ))
    }

    pub(crate) async fn prewarm_auth(&self) -> Result<()> {
        self.current_client_setup().await.map(|_| ())
    }

    /// Opens a websocket connection using the same header and telemetry wiring as normal turns.
    ///
    /// Both startup prewarm and in-turn `needs_new` reconnects call this path so handshake
    /// behavior remains consistent across both flows.
    #[allow(clippy::too_many_arguments)]
    async fn connect_websocket(
        &self,
        session_telemetry: &SessionTelemetry,
        api_provider: codex_api::Provider,
        api_auth: SharedAuthProvider,
        responses_metadata: &CodexResponsesMetadata,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        routing_hint: Option<HeaderValue>,
    ) -> std::result::Result<ApiWebSocketConnection, ApiError> {
        let mut headers = self.build_websocket_headers(responses_metadata).await;
        if let Some(routing_hint) = routing_hint {
            headers.insert(X_CODEX_ROUTING_HINT_HEADER, routing_hint);
        }
        let websocket_telemetry = ModelClientSession::build_websocket_telemetry(
            session_telemetry,
            auth_context.clone(),
            request_route_telemetry,
            self.state.auth_env_telemetry.clone(),
        );
        let websocket_connect_timeout = self.state.provider.info().websocket_connect_timeout();
        let start = Instant::now();
        let result = match tokio::time::timeout(
            websocket_connect_timeout,
            ApiWebSocketResponsesClient::new(api_provider, api_auth).connect(
                &self.http_client_factory,
                headers,
                codex_login::default_client::default_headers(),
                /*turn_state*/ None,
                Some(websocket_telemetry),
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ApiError::Transport(TransportError::ConnectionTimeout)),
        };
        let error_message = result.as_ref().err().map(telemetry_api_error_message);
        let response_debug = result
            .as_ref()
            .err()
            .map(extract_response_debug_context_from_api_error)
            .unwrap_or_default();
        let status = result.as_ref().err().and_then(api_error_http_status);
        session_telemetry.record_websocket_connect(
            start.elapsed(),
            status,
            error_message.as_deref(),
            auth_context.auth_header_attached,
            auth_context.auth_header_name,
            auth_context.retry_after_unauthorized,
            auth_context.recovery_mode,
            auth_context.recovery_phase,
            request_route_telemetry.endpoint,
            /*connection_reused*/ false,
            response_debug.request_id.as_deref(),
            response_debug.cf_ray.as_deref(),
            response_debug.auth_error.as_deref(),
            response_debug.auth_error_code.as_deref(),
            auth_context.agent_identity_telemetry(),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: request_route_telemetry.endpoint,
                auth_header_attached: auth_context.auth_header_attached,
                auth_header_name: auth_context.auth_header_name,
                auth_mode: auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(auth_context.retry_after_unauthorized),
                auth_recovery_mode: auth_context.recovery_mode,
                auth_recovery_phase: auth_context.recovery_phase,
                auth_connection_reused: Some(false),
                auth_request_id: response_debug.request_id.as_deref(),
                auth_cf_ray: response_debug.cf_ray.as_deref(),
                auth_error: response_debug.auth_error.as_deref(),
                auth_error_code: response_debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: auth_context
                    .retry_after_unauthorized
                    .then_some(result.is_ok()),
                auth_recovery_followup_status: auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.state.auth_env_telemetry,
        );
        result
    }

    /// Builds websocket handshake headers for both prewarm and turn-time reconnect.
    async fn build_websocket_headers(
        &self,
        responses_metadata: &CodexResponsesMetadata,
    ) -> ApiHeaderMap {
        let mut headers = build_responses_headers(
            self.state.beta_features_header.as_deref(),
            /*turn_state*/ None,
        );
        add_originator_header(&mut headers, self.state.originator.as_str());
        if let Ok(header_value) = HeaderValue::from_str(&responses_metadata.thread_id) {
            headers.insert("x-client-request-id", header_value);
        }
        headers.extend(build_session_headers(
            Some(responses_metadata.session_id.to_string()),
            Some(responses_metadata.thread_id.to_string()),
        ));
        headers.extend(self.build_responses_compatibility_headers(responses_metadata));
        if let Some(header_value) = self.generate_attestation_header_for().await {
            headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
        }
        headers.insert(
            OPENAI_BETA_HEADER,
            HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
        );
        if self.state.include_timing_metrics {
            headers.insert(
                X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        headers
    }
}

impl Drop for ModelClientSession {
    fn drop(&mut self) {
        let Some(permit) = self.websocket_cache_publication.take() else {
            return;
        };
        let websocket_session = std::mem::take(&mut self.websocket_session);
        self.client
            .try_store_cached_websocket_transport(permit, websocket_session);
    }
}

impl ModelClientSession {
    pub(crate) fn revoke_cache_publication(&mut self) {
        self.websocket_cache_publication = None;
    }

    pub(crate) fn set_turn_timing(&mut self, turn_timing: Arc<TurnTimingState>) {
        self.turn_timing = Some(turn_timing);
    }

    pub(crate) fn dispatched_account(&self) -> Option<&crate::responses_retry::AccountChangeIdentity> {
        self.dispatched_account.as_ref()
    }

    pub(crate) fn set_startup_timing(
        &mut self,
        timing: Arc<crate::startup_timing::StartupTimingState>,
    ) {
        self.startup_timing = Some(timing);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn claim_startup_prewarm(
        &mut self,
        mut prewarmed_session: ModelClientSession,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
    ) -> Result<StartupPrewarmClaim> {
        prewarmed_session.revoke_cache_publication();
        if !Arc::ptr_eq(&self.client.state, &prewarmed_session.client.state)
            || self.http_for_turn
            || !self.client.responses_websocket_enabled()
        {
            return Ok(StartupPrewarmClaim::Rejected);
        }

        let transport_readiness_guard = self
            .turn_timing
            .as_ref()
            .map(|timing| timing.begin_local_phase(TurnLocalPhase::TransportReadiness));
        let client_setup = self.client.current_client_setup().await?;
        let request_transformation_guard = self
            .turn_timing
            .as_ref()
            .map(|timing| timing.begin_local_phase(TurnLocalPhase::RequestTransformation));
        let request = self.client.build_responses_request(
            &client_setup.api_provider,
            prompt,
            model_info,
            effort,
            summary,
            service_tier,
            responses_metadata,
        )?;
        let setup_fingerprint =
            websocket_setup_fingerprint(&client_setup.api_provider, client_setup.api_auth.as_ref());
        let compatible = prewarmed_session
            .websocket_session
            .is_compatible_startup_prewarm(
                setup_fingerprint,
                &request,
                prompt.stable_context_manifest.fingerprint(),
            )
            .await;
        let transport_compatible = compatible
            || prewarmed_session
                .websocket_session
                .has_compatible_startup_transport(setup_fingerprint)
                .await;

        self.prepared_startup_websocket_attempt = Some(PreparedStartupWebsocketAttempt {
            client_setup,
            transport_readiness_guard,
            request_transformation_guard,
        });
        if !transport_compatible {
            return Ok(StartupPrewarmClaim::Rejected);
        }

        let adopted_session = std::mem::take(&mut prewarmed_session.websocket_session);
        let adopted_session = if compatible {
            adopted_session
        } else {
            // Request-property or non-empty-prefix incompatibility must discard response IDs and
            // history, but the already-authenticated socket is still valid for the real request.
            adopted_session.into_transport_only()
        };
        let replaced_session = std::mem::replace(&mut self.websocket_session, adopted_session);
        drop(replaced_session);
        Ok(if compatible {
            StartupPrewarmClaim::ResponseChain
        } else {
            StartupPrewarmClaim::TransportOnly
        })
    }

    fn reset_websocket_session(&mut self) {
        self.websocket_session.connection = None;
        self.websocket_session.connected_at = None;
        self.websocket_session.setup_fingerprint = None;
        self.invalidate_incremental_history("websocket reset");
        self.websocket_session
            .set_connection_reused(/*connection_reused*/ false);
    }

    /// Steering abandons an unfinished response, not the turn. Its socket may
    /// still be draining asynchronously, so reconnect without treating local
    /// interruption as a provider failure or inheriting the abandoned response.
    pub(crate) fn interrupt_response_for_steering(&mut self) {
        self.reset_websocket_session();
    }

    /// Replaces a websocket the provider closed mid-response with a code that
    /// invites another connection, so the next attempt of the same request
    /// reconnects instead of abandoning WebSockets for the session.
    ///
    /// The sticky turn-state token is dropped for the same reason as a
    /// throughput collapse: the backend that closed is the one the token would
    /// route back to.
    pub(crate) fn replace_websocket_after_reconnectable_close(&mut self) {
        self.reset_websocket_session();
        self.turn_state = Arc::new(OnceLock::new());
    }

    /// Replaces a websocket whose responses stopped streaming at this client's
    /// established rate. The sticky turn-state token is dropped with it: replaying
    /// the token would route the full request back to the backend that collapsed.
    fn replace_websocket_after_throughput_collapse(&mut self) {
        let collapse = self
            .websocket_session
            .stream_throughput
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_collapse();
        let Some(collapse) = collapse else {
            return;
        };
        warn!(
            observed_tokens_per_second = collapse.observed_tokens_per_second,
            baseline_tokens_per_second = collapse.baseline_tokens_per_second,
            "replacing websocket and sticky turn routing after stream throughput collapse"
        );
        self.reset_websocket_session();
        self.turn_state = Arc::new(OnceLock::new());
    }

    /// Invalidates request-prefix reuse without discarding a healthy transport.
    ///
    /// Callers use this for compaction, truncation, rollback, reinjection,
    /// history replacement, and any model-visible state change. Reconnects and
    /// request-property/model changes are also guarded locally below.
    pub(crate) fn invalidate_incremental_history(&mut self, reason: &'static str) {
        self.prepared_startup_websocket_attempt = None;
        if let Some(baseline) = self.websocket_session.last_request_history.as_ref() {
            trace!(
                reason,
                history_generation = baseline.generation,
                "invalidating websocket incremental history"
            );
        }
        self.websocket_session.last_request = None;
        self.websocket_session.last_request_history = None;
        self.websocket_session.last_response_rx = None;
        self.websocket_session.last_response = None;
        self.websocket_session.last_response_from_untraced_warmup = false;
        self.websocket_session.next_history_generation = self
            .websocket_session
            .next_history_generation
            .wrapping_add(1)
            .max(1);
    }

    /// Central invalidation for an unknown realized context baseline. This
    /// disables every provider-held inheritance path while retaining a healthy
    /// transport for the next complete request.
    pub(crate) fn invalidate_provider_history_inheritance(&mut self, reason: &'static str) {
        self.invalidate_incremental_history(reason);
        // Provider inheritance and local diagnostics have separate lifetimes.
        // Keep the last dispatched measurement (including an unfinished
        // predecessor) so the full replay can report exactly where it diverged.
        // These measurements never authorize previous_response_id reuse.
    }

    #[cfg(test)]
    fn remember_request_history(
        &mut self,
        request: &ResponsesApiRequest,
        stable_context_fingerprint: [u8; 32],
    ) {
        self.remember_verified_request_history(request, stable_context_fingerprint, None);
    }

    fn remember_verified_request_history(
        &mut self,
        request: &ResponsesApiRequest,
        stable_context_fingerprint: [u8; 32],
        verified: Option<VerifiedRequestHistory>,
    ) {
        let (request_prefix, request_properties_fingerprint) = match verified {
            Some(verified) => (Ok(verified.prefix), Ok(verified.properties)),
            None => (
                CanonicalPrefixHash::from_iter(request.input.iter()),
                responses_request_properties_fingerprint(request),
            ),
        };
        self.websocket_session.next_history_generation = self
            .websocket_session
            .next_history_generation
            .wrapping_add(1)
            .max(1);
        let generation = self.websocket_session.next_history_generation;
        self.websocket_session.last_request_history =
            match (request_prefix, request_properties_fingerprint) {
                (Ok(request_prefix), Ok(request_properties_fingerprint)) => {
                    Some(WebsocketHistoryBaseline {
                        generation,
                        request_prefix,
                        request_properties_fingerprint,
                        stable_context_fingerprint,
                        provider_response_id_established: false,
                        normalization_policy_version:
                            WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION,
                    })
                }
                (request_prefix, request_properties_fingerprint) => {
                    trace!(
                        history_generation = generation,
                        prefix_hash_error = ?request_prefix.err(),
                        properties_hash_error = ?request_properties_fingerprint.err(),
                        "websocket history hashing unavailable; retaining full-comparison fallback"
                    );
                    None
                }
            };
    }

    #[allow(clippy::too_many_arguments)]
    /// Builds shared Responses API transport options and request-body options.
    ///
    /// Keeping option construction in one place ensures request-scoped headers are consistent
    /// regardless of transport choice.
    async fn build_responses_options(
        &self,
        responses_metadata: &CodexResponsesMetadata,
        compression: Compression,
        use_responses_lite: bool,
    ) -> ApiResponsesOptions {
        ApiResponsesOptions {
            session_id: Some(responses_metadata.session_id.to_string()),
            thread_id: Some(responses_metadata.thread_id.to_string()),
            session_source: Some(self.client.state.session_source.clone()),
            extra_headers: {
                let mut headers = build_responses_headers(
                    self.client.state.beta_features_header.as_deref(),
                    Some(&self.turn_state),
                );
                add_originator_header(&mut headers, self.client.state.originator.as_str());
                headers.extend(
                    self.client
                        .build_responses_compatibility_headers(responses_metadata),
                );
                if let Some(header_value) = self.client.generate_attestation_header_for().await {
                    headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
                }
                add_responses_lite_header(&mut headers, use_responses_lite);
                headers
            },
            compression,
            turn_state: Some(Arc::clone(&self.turn_state)),
        }
    }

    /// Checks whether the current request is an incremental extension of the previous request.
    /// We only reuse an incremental input delta when non-input request fields are unchanged and
    /// `input` is a strict extension of the previous known input. Server-returned output items
    /// are treated as part of the baseline so we do not resend them.
    #[cfg(test)]
    fn get_incremental_items(
        &self,
        request: &ResponsesApiRequest,
        last_response: Option<&LastResponse>,
        allow_empty_delta: bool,
    ) -> Option<Vec<ResponseItem>> {
        self.get_incremental_items_with_history(request, last_response, allow_empty_delta)
            .map(|(items, _)| items)
    }

    fn get_incremental_items_with_history(
        &self,
        request: &ResponsesApiRequest,
        last_response: Option<&LastResponse>,
        allow_empty_delta: bool,
    ) -> Option<(Vec<ResponseItem>, Option<VerifiedRequestHistory>)> {
        let previous_request = self.websocket_session.last_request.as_ref()?;
        if !responses_request_properties_match(previous_request, request) {
            trace!("incremental request failed, websocket reuse properties didn't match");
            return None;
        }

        if let Some(baseline) = self.websocket_session.last_request_history.as_ref()
            && baseline.normalization_policy_version
                == WEBSOCKET_HISTORY_NORMALIZATION_POLICY_VERSION
        {
            if !responses_request_json_serialization_matches(previous_request, request) {
                trace!("incremental request failed, request-property serialization changed");
                return None;
            }
            // Exact equality above covers the same exhaustive property projection
            // as this fingerprint. The baseline and previous request are installed
            // together, so reserializing the unchanged properties proves nothing new.
            let current_properties_fingerprint = baseline.request_properties_fingerprint;

            let mut expected_prefix = baseline.request_prefix;
            if let Some(response) = last_response
                && let Err(err) = expected_prefix.extend_items(&response.items_added)
            {
                trace!(
                    ?err,
                    "response-prefix hashing unavailable; using full comparison"
                );
                return Self::get_incremental_items_full_compare(
                    previous_request,
                    request,
                    last_response,
                    allow_empty_delta,
                )
                .map(|items| (items, None));
            }
            if request.input.len() < expected_prefix.item_count {
                trace!("incremental request failed, incompatible request length");
                return None;
            };
            // Exact normalized comparison proves the unchanged prefix without
            // serializing it again. Extend the already trusted digest only for
            // new response/input items, and carry it through remember().
            let incremental_items = Self::get_incremental_items_full_compare(
                previous_request, request, last_response, allow_empty_delta,
            )?;
            let mut prefix = expected_prefix;
            let verified = prefix
                .extend_iter(request.input.iter().skip(expected_prefix.item_count))
                .ok()
                .map(|()| VerifiedRequestHistory {
                    prefix,
                    properties: current_properties_fingerprint,
                });
            return Some((
                incremental_items,
                verified,
            ));
        }

        Self::get_incremental_items_full_compare(
            previous_request,
            request,
            last_response,
            allow_empty_delta,
        )
        .map(|items| (items, None))
    }

    /// Correctness fallback for missing/unknown hash state. Compare the
    /// normalized prefix without materializing copies of both histories.
    fn get_incremental_items_full_compare(
        previous_request: &ResponsesApiRequest,
        request: &ResponsesApiRequest,
        last_response: Option<&LastResponse>,
        allow_empty_delta: bool,
    ) -> Option<Vec<ResponseItem>> {
        let response_items =
            last_response.map_or(&[][..], |response| response.items_added.as_ref());
        let previous_len = previous_request
            .input
            .len()
            .checked_add(response_items.len())?;
        if request.input.len() < previous_len {
            trace!("incremental request failed, incompatible request length");
            return None;
        };
        if !previous_request
            .input
            .iter()
            .chain(response_items)
            .zip(request.input.iter().take(previous_len))
            .all(|(previous, current)| {
                previous == current
                    || normalized_websocket_history_item(previous)
                        == normalized_websocket_history_item(current)
            })
        {
            trace!("incremental request failed, items didn't match");
            return None;
        }
        if !allow_empty_delta && request.input.len() == previous_len {
            return None;
        }
        Some(request.input.iter().skip(previous_len).cloned().collect())
    }

    fn get_last_response(&mut self) -> Option<Arc<LastResponse>> {
        if let Some(receiver) = self.websocket_session.last_response_rx.as_mut() {
            match receiver.try_recv() {
                Ok(response) => {
                    self.websocket_session.last_response = Some(Arc::new(response));
                    self.websocket_session.last_response_rx = None;
                }
                Err(TryRecvError::Closed) => self.websocket_session.last_response_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let response = self.websocket_session.last_response.clone();
        if response
            .as_ref()
            .is_some_and(|last_response| !last_response.response_id.is_empty())
            && let Some(baseline) = self.websocket_session.last_request_history.as_mut()
        {
            baseline.provider_response_id_established = true;
        }
        response
    }

    fn substitutions_overlap_provider_history(
        &self,
        substitutions: &[crate::tool_history::ToolHistorySubstitution],
        response: &LastResponse,
    ) -> bool {
        let previous = self.websocket_session.last_request.as_ref();
        let previous_len = previous.map_or(0, |request| request.input.len());
        crate::tool_history::substitutions_overlap_items(substitutions, |index| {
            if index < previous_len {
                previous.and_then(|request| request.input.get_item(index))
            } else {
                response.items_added.get(index - previous_len)
            }
        })
    }

    fn prepare_websocket_request(
        &mut self,
        mut payload: ResponseCreateWsRequest,
        request: &ResponsesApiRequest,
        _stable_context_fingerprint: [u8; 32],
        tool_history_substitutions: &[crate::tool_history::ToolHistorySubstitution],
        mut build_tool_history_fail_open_request: Option<
            Box<dyn FnOnce() -> Result<ResponsesApiRequest> + '_>,
        >,
    ) -> Result<(
        ResponsesWsRequest,
        bool,
        Option<ResponsesApiRequest>,
        Option<VerifiedRequestHistory>,
    )> {
        // Appended context changes the manifest without changing the inherited
        // prefix. The request-property and normalized-prefix checks below are
        // authoritative; a manifest change alone must not discard that proof.
        if tool_history_substitutions.is_empty() {
            self.tool_history_fail_open_pending = false;
        }
        let mut logical_request_override = None;
        if self.tool_history_fail_open_pending {
            let build_fallback_request =
                build_tool_history_fail_open_request.take().ok_or_else(|| {
                    CodexErr::Fatal(
                        "tool-history fail-open replay is missing its rebuilt request".to_string(),
                    )
                })?;
            let fallback_request = build_fallback_request()?;
            payload.input = fallback_request.input.clone();
            logical_request_override = Some(fallback_request);
        }
        let effective_request = logical_request_override.as_ref().unwrap_or(request);
        let last_response = self.get_last_response();
        let Some(last_response) = last_response else {
            return Ok((
                ResponsesWsRequest::ResponseCreate(payload),
                false,
                logical_request_override,
                None,
            ));
        };
        let previous_response_id_from_untraced_warmup =
            self.websocket_session.last_response_from_untraced_warmup;
        if !tool_history_substitutions.is_empty() && !self.tool_history_fail_open_pending {
            if !crate::tool_history::substitutions_match_items(
                tool_history_substitutions,
                |index| request.input.get_item(index),
            ) {
                trace!("completed-tool substitution metadata did not match request input");
            }
            if self
                .substitutions_overlap_provider_history(tool_history_substitutions, &last_response)
            {
                self.tool_history_fail_open_pending = true;
                let build_fallback_request =
                    build_tool_history_fail_open_request.take().ok_or_else(|| {
                        CodexErr::Fatal(
                            "tool-history rebase is missing its fail-open request".to_string(),
                        )
                    })?;
                let fallback_request = build_fallback_request()?;
                debug_assert!(
                    !tool_history_substitutions.iter().any(|substitution| {
                        fallback_request.input.iter().any(|item| {
                            crate::tool_history::canonical_textual_output_identity(item)
                                .is_some_and(|(call_id, text)| {
                                    call_id == substitution.call_id
                                        && crate::tool_history::sha256(text.as_bytes())
                                            == substitution.substituted_output_sha256
                                })
                        })
                    }),
                    "tool-history fail-open request retained substituted receipts"
                );
                if let Some((incremental_items, verified_history)) = self
                    .get_incremental_items_with_history(
                        &fallback_request,
                        Some(&last_response),
                        true,
                    )
                    && !last_response.response_id.is_empty()
                {
                    payload.previous_response_id = Some(last_response.response_id.clone());
                    payload.input = incremental_items.into();
                    return Ok((
                        ResponsesWsRequest::ResponseCreate(payload),
                        previous_response_id_from_untraced_warmup,
                        Some(fallback_request),
                        verified_history,
                    ));
                }
                self.invalidate_provider_history_inheritance(
                    "completed-tool fallback did not match inherited history",
                );
                payload.previous_response_id = None;
                payload.input = fallback_request.input.clone();
                return Ok((
                    ResponsesWsRequest::ResponseCreate(payload),
                    false,
                    Some(fallback_request),
                    None,
                ));
            }
        }
        let Some((incremental_items, verified_history)) = self.get_incremental_items_with_history(
            effective_request,
            Some(&last_response),
            /*allow_empty_delta*/ true,
        ) else {
            self.invalidate_provider_history_inheritance(
                "request no longer matches inherited history",
            );
            return Ok((
                ResponsesWsRequest::ResponseCreate(payload),
                false,
                logical_request_override,
                None,
            ));
        };

        if last_response.response_id.is_empty() {
            trace!("incremental request failed, no previous response id");
            return Ok((
                ResponsesWsRequest::ResponseCreate(payload),
                false,
                logical_request_override,
                None,
            ));
        }

        Ok((
            ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
                previous_response_id: Some(last_response.response_id.clone()),
                input: incremental_items.into(),
                ..payload
            }),
            previous_response_id_from_untraced_warmup,
            logical_request_override,
            verified_history,
        ))
    }

    /// Starts transport-only setup while the caller prepares the turn. The task belongs to
    /// this session: dropping the turn cancels it, and it cannot publish a cached socket.
    /// Healthy transports (including startup prewarm) remain under their current owner.
    pub async fn start_websocket_preconnect(
        &mut self,
        model_info: &ModelInfo,
        service_tier: Option<String>,
        session_telemetry: &SessionTelemetry,
        responses_metadata: &CodexResponsesMetadata,
    ) {
        if self.http_for_turn
            || !self.client.responses_websocket_enabled()
            || self.pending_websocket_preconnect.is_some()
        {
            return;
        }
        // A spawned agent's first turn can start before its thread is attached to an
        // attestation-capable client; the handshake needs that client.
        let state = &self.client.state;
        let attestation_ready = state
            .attestation_provider
            .clone()
            .filter(|provider| {
                state.session_source.is_non_root_agent()
                    && state.include_attestation
                    && !provider.supports_startup_requests()
            })
            .map(|provider| {
                (
                    provider,
                    AttestationContext {
                        thread_id: state.thread_id,
                    },
                )
            });
        if let Some(connection) = self.websocket_session.connection.as_ref() {
            if !websocket_connection_expiring(self.websocket_session.connected_at, Instant::now())
                && !connection.is_closed().await
            {
                return;
            }
            self.reset_websocket_session();
        }
        let mut session = self.client.new_speculative_session();
        let failure_setup = Arc::new(StdMutex::new(None));
        self.preconnect_failure_setup = Some(Arc::clone(&failure_setup));
        let model_info = model_info.clone();
        let session_telemetry = session_telemetry.clone();
        let responses_metadata = responses_metadata.clone();
        self.pending_websocket_preconnect = Some(AbortOnDropHandle::new(tokio::spawn(
            async move {
                // Dispatch awaits this task, so an attachment that is not imminent yields to
                // the connect that dispatch performs anyway.
                if let Some((provider, context)) = attestation_ready
                    && tokio::time::timeout(
                        SPAWNED_AGENT_ATTESTATION_WAIT,
                        provider.wait_until_ready(context),
                    )
                    .await
                    .is_err()
                {
                    return Ok(None);
                }
                let result = session
                    .preconnect_websocket(
                        &model_info,
                        service_tier,
                        &session_telemetry,
                        &responses_metadata,
                    )
                    .await;
                if result.as_ref().err().is_some_and(|error| match error {
                    CodexErr::ConnectionFailed(_) | CodexErr::RequestTimeout => true,
                    CodexErr::UnexpectedStatus(error) => error.status.is_server_error(),
                    _ => false,
                }) {
                    *failure_setup
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        session.failed_websocket_setup;
                }
                result?;
                Ok(Some(
                    std::mem::take(&mut session.websocket_session).into_transport_only(),
                ))
            }
            .in_current_span(),
        )));
    }

    async fn finish_websocket_preconnect(&mut self) -> Result<()> {
        let Some(task) = self.pending_websocket_preconnect.take() else {
            return Ok(());
        };
        // A startup-prewarm claim may have won while preparation was running. Its socket
        // already serves this dispatch: never wait for, or fail on, the redundant dial.
        if self.websocket_session.connection.is_some() {
            drop(task);
            return Ok(());
        }
        match task.await {
            Ok(Ok(Some(transport))) if self.client.responses_websocket_enabled() => {
                self.websocket_session = transport;
            }
            Ok(Err(error)) if self.client.responses_websocket_enabled() => {
                // Count this connection attempt under the ordinary retry owner.
                // Discarding it would start another unaccounted connection wait
                // before that owner sees any failure. No prompt was dispatched.
                self.last_stream_transport = Some(ModelAttemptTransport::ResponsesWebsocket);
                return Err(error);
            }
            Ok(_) => {}
            Err(err) => warn!("turn websocket preconnect join failed: {err}"),
        }
        Ok(())
    }

    /// Kind of the logical sampling request this dispatch carries. Compaction shares the
    /// turn's session but is not a sampling step, so it never advances the sequence.
    fn next_logical_request_kind(
        &mut self,
        responses_metadata: &CodexResponsesMetadata,
    ) -> ModelAttemptRequestKind {
        let request_kind = if self.logical_sampling_request_count == 0 {
            ModelAttemptRequestKind::Initial
        } else {
            ModelAttemptRequestKind::Continuation
        };
        let counted = !matches!(
            responses_metadata.request_kind,
            Some(crate::responses_metadata::CodexResponsesRequestKind::Compaction(_))
        );
        if counted {
            self.logical_sampling_request_count =
                self.logical_sampling_request_count.saturating_add(1);
        }
        self.last_logical_request_counted = counted;
        request_kind
    }

    /// The turn's outer recovery re-dispatches the same logical sampling request. Keep the
    /// failed dispatch's Initial/Continuation kind instead of counting another step.
    pub(crate) fn note_logical_request_replay(&mut self) {
        if std::mem::take(&mut self.last_logical_request_counted) {
            self.logical_sampling_request_count =
                self.logical_sampling_request_count.saturating_sub(1);
        }
    }

    /// Opportunistically preconnects a websocket for this turn-scoped client session.
    ///
    /// This performs only connection setup; it never sends prompt payloads.
    pub async fn preconnect_websocket(
        &mut self,
        model_info: &ModelInfo,
        service_tier: Option<String>,
        session_telemetry: &SessionTelemetry,
        responses_metadata: &CodexResponsesMetadata,
    ) -> Result<()> {
        self.finish_websocket_preconnect().await?;
        if !self.client.responses_websocket_enabled() {
            return Ok(());
        }

        let provider = Arc::clone(&self.client.state.provider);
        let auth_manager = provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            );
            let routing_hint = build_routing_hint_header(
                client_setup
                    .auth
                    .as_ref()
                    .filter(|_| provider.info().is_openai()),
                &client_setup.api_provider,
                &model_info.slug,
                model_info
                    .service_tier_for_request(service_tier.clone())
                    .as_deref(),
            );
            match self
                .websocket_connection(WebsocketConnectParams {
                    session_telemetry,
                    api_provider: client_setup.api_provider,
                    api_auth: client_setup.api_auth,
                    responses_metadata,
                    auth_context,
                    request_route_telemetry: RequestRouteTelemetry::for_endpoint(
                        RESPONSES_ENDPOINT,
                    ),
                    routing_hint,
                })
                .await
            {
                Ok(_) => return Ok(()),
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if matches!(status, StatusCode::FORBIDDEN | StatusCode::UPGRADE_REQUIRED) =>
                {
                    // An upgrade can be blocked even when ordinary HTTPS works. No prompt
                    // was sent; let HTTP report any actual authorization failure.
                    self.client.force_http_fallback(session_telemetry);
                    return Ok(());
                }
                Err(ApiError::Transport(unauthorized_transport))
                    if is_recoverable_authentication_failure(&unauthorized_transport) =>
                {
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            session_telemetry,
                            &provider,
                        )
                        .await?,
                    );
                }
                Err(err) => return Err(provider.map_api_error(err)),
            }
        }
    }
    /// Returns a websocket connection for this turn.
    #[instrument(
        name = "model_client.websocket_connection",
        level = "info",
        skip_all,
        fields(
            provider = %self.client.state.provider.info().name,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "responses_websocket",
            api.path = "responses",
            turn.has_metadata_header = params.responses_metadata.has_turn_metadata()
        )
    )]
    async fn websocket_connection(
        &mut self,
        params: WebsocketConnectParams<'_>,
    ) -> std::result::Result<&ApiWebSocketConnection, ApiError> {
        let WebsocketConnectParams {
            session_telemetry,
            api_provider,
            api_auth,
            responses_metadata,
            auth_context,
            request_route_telemetry,
            routing_hint,
        } = params;
        let setup_fingerprint = websocket_setup_fingerprint(&api_provider, api_auth.as_ref());
        self.failed_websocket_setup = None;
        let needs_new = match self.websocket_session.connection.as_ref() {
            Some(conn) => {
                setup_fingerprint.is_none()
                    || self.websocket_session.setup_fingerprint != setup_fingerprint
                    || websocket_connection_expiring(
                        self.websocket_session.connected_at,
                        Instant::now(),
                    )
                    || conn.is_closed().await
            }
            None => true,
        };

        if needs_new {
            self.invalidate_incremental_history("websocket reconnect");
            // Recovery may happen in the same turn that invalidated the cache. Take a fresh
            // publication epoch before connecting, so a later fallback can still revoke it.
            // Never restore publication rights to a speculative or explicitly revoked owner.
            if let Some(permit) = self.websocket_cache_publication.as_mut() {
                permit.epoch = self
                    .client
                    .state
                    .cached_websocket_transport
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .epoch;
            }
            let new_conn = match self
                .client
                .connect_websocket(
                    session_telemetry,
                    api_provider,
                    api_auth,
                    responses_metadata,
                    auth_context,
                    request_route_telemetry,
                    routing_hint,
                )
                .await
            {
                Ok(new_conn) => new_conn,
                Err(err) => {
                    self.failed_websocket_setup = setup_fingerprint;
                    if matches!(err, ApiError::Transport(TransportError::ConnectionTimeout)) {
                        self.reset_websocket_session();
                    }
                    return Err(err);
                }
            };
            self.websocket_session.connection = Some(new_conn);
            self.websocket_session.connected_at = Some(Instant::now());
            self.websocket_session.setup_fingerprint = setup_fingerprint;
            self.websocket_session
                .set_connection_reused(/*connection_reused*/ false);
        } else {
            self.websocket_session
                .set_connection_reused(/*connection_reused*/ true);
        }

        self.websocket_session
            .connection
            .as_ref()
            .ok_or(ApiError::Stream(
                "websocket connection is unavailable".to_string(),
            ))
    }

    fn responses_request_compression(&self, auth: Option<&CodexAuth>) -> Compression {
        if self.client.state.enable_request_compression
            && auth.is_some_and(CodexAuth::uses_codex_backend)
            && self.client.state.provider.info().is_openai()
        {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Streams a turn via the OpenAI Responses API.
    ///
    /// Handles reasoning summaries, verbosity, and the `text` controls used for output schemas.
    #[allow(clippy::too_many_arguments)]
    fn stream_responses_api<'a>(
        &'a mut self,
        prompt: &'a Prompt,
        model_info: &'a ModelInfo,
        session_telemetry: &'a SessionTelemetry,
        sampling_request_id: &'a str,
        request_kind: ModelAttemptRequestKind,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &'a CodexResponsesMetadata,
        attempt_prepared: Option<AttemptPreparedCallback>,
    ) -> impl std::future::Future<Output = Result<ResponseStream>> + 'a {
        Box::pin(async move {
            let span = tracing::info_span!(
                "model_client.stream_responses_api",
                model = %model_info.slug,
                wire_api = %self.client.state.provider.info().wire_api,
                transport = "responses_http",
                http.method = "POST",
                api.path = "responses",
                turn.has_metadata_header = responses_metadata.has_turn_metadata()
            );
            // Instrument pointer-sized storage, not the transport state machine.
            // Keep span creation on first poll and cancellation on the caller task.
            Box::pin(async move {
                let auth_manager = self.client.state.provider.auth_manager();
                let mut auth_recovery = auth_manager
                    .as_ref()
                    .map(AuthManager::unauthorized_recovery);
                let mut pending_retry = PendingUnauthorizedRetry::default();
                let mut retry_index = 0_u32;
                loop {
                    // Include client-side composition, normalization, serialization, and transport setup
                    // in dispatch_ready_us for this physical attempt.
                    let attempt_clock = ModelAttemptClock {
                        provider_event_at_transport: true,
                        ..ModelAttemptClock::new()
                    };
                    let transport_readiness_guard = self
                        .turn_timing
                        .as_ref()
                        .map(|timing| timing.begin_local_phase(TurnLocalPhase::TransportReadiness));
                    let client_setup = self.client.current_client_setup().await?;
                    let transport = self
                        .client
                        .build_api_transport(&client_setup.api_provider, RESPONSES_ENDPOINT)
                        .await?;
                    let request_auth_context = AuthRequestTelemetryContext::new(
                        client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                        client_setup.api_auth.as_ref(),
                        client_setup.agent_identity_telemetry.clone(),
                        pending_retry,
                    );
                    self.dispatched_account = client_setup.auth.as_ref()
                        .and_then(crate::responses_retry::AccountChangeIdentity::from_auth);
                    let compression = self.responses_request_compression(client_setup.auth.as_ref());
                    let mut options = self
                        .build_responses_options(
                            responses_metadata,
                            compression,
                            model_info.use_responses_lite,
                        )
                        .await;

                    let request_transformation_guard = self
                        .turn_timing
                        .as_ref()
                        .map(|timing| timing.begin_local_phase(TurnLocalPhase::RequestTransformation));
                    let mut request = self.client.build_responses_request(
                        &client_setup.api_provider,
                        prompt,
                        model_info,
                        effort.clone(),
                        summary,
                        service_tier.clone(),
                        responses_metadata,
                    )?;
                    self.client
                        .prepare_response_items_for_request(&mut request.input);
                    drop(request_transformation_guard);
                    let request_session_telemetry = session_telemetry_for_request(session_telemetry, &request);
                    let attempt_identity = new_attempt_identity(sampling_request_id);
                    let mut essential_attempt = ModelAttemptGuard::new(
                        request_session_telemetry.clone(), attempt_identity.clone(), retry_index,
                        if retry_index == 0 { ModelAttemptRetryReason::None } else { ModelAttemptRetryReason::Unauthorized },
                        request_kind, ModelAttemptTransport::ResponsesHttp, None,
                        ModelRequestMeasurements::default(), attempt_clock.clone(),
                        responses_metadata.turn_id.clone(), self.turn_timing.as_ref()
                            .and_then(|timing| timing.current_model_attempt_metadata()),
                    );
                    essential_attempt.turn_timing = self.turn_timing.clone();
                    let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                        &request_session_telemetry,
                        request_auth_context,
                        RequestRouteTelemetry::for_endpoint(RESPONSES_ENDPOINT),
                        self.client.state.auth_env_telemetry.clone(),
                        ModelRequestTelemetryContext {
                            timing: self.turn_timing.clone(),
                            startup: self.startup_timing.clone(),
                            identity: attempt_identity.clone(),
                            clock: attempt_clock.clone(),
                            turn_id: responses_metadata.turn_id.clone(),
                            retry_index,
                            dispatched: AtomicBool::new(false),
                            highest_started_transport_attempt: StdMutex::new(None),
                            readiness: StdMutex::new(transport_readiness_guard),
                        },
                    );
                    if let Some(timing) = self.turn_timing.as_ref() {
                        timing.record_model_attempt_identity(
                            &attempt_identity.sampling_request_id,
                            &attempt_identity.physical_attempt_id,
                        );
                        timing.record_model_estimator_model(
                            &attempt_identity.sampling_request_id,
                            &attempt_identity.physical_attempt_id, &request.model,
                        );
                    }
                    attempt_clock.mark_queue_started();
                    let sampling_admission = match attempt_prepared.as_ref() {
                        Some(attempt_prepared) => attempt_prepared(attempt_identity.clone()).await,
                        None => Ok(None),
                    };
                    attempt_clock.mark_queue_finished();
                    let _sampling_admission = sampling_admission?;
                    let mut api_provider = client_setup.api_provider;
                    if let Some(timeout) = prompt.stream_progress_timeout {
                        api_provider.stream_idle_timeout = timeout;
                    }
                    let client = ApiResponsesClient::new(transport, api_provider, client_setup.api_auth)
                        .with_ollama_summary_compat(self.client.state.provider.info().kind == codex_model_provider_info::ProviderKind::Ollama)
                        .with_telemetry(Some(request_telemetry), Some(sse_telemetry));
                    let turn_timing = self.turn_timing.clone();
                    let serialization_timing_guard = self
                        .turn_timing
                        .as_ref()
                        .map(|timing| timing.begin_local_phase(TurnLocalPhase::Serialization));
                    let dispatched_request = Arc::new(OnceLock::new());
                    let dispatched_request_for_callback = Arc::clone(&dispatched_request);
                    let measurement_dispatch = Arc::new(StdMutex::new(None));
                    let measurement_dispatch_for_callback = Arc::clone(&measurement_dispatch);
                    let measurement_owner = Arc::clone(&self.prompt_context_measurements);
                    let stream_result = client
                        .stream_request_with_dispatch_ready(&request, options, {
                            let dispatched_identity = attempt_identity.clone();
                            move |request_bytes| {
                                let dispatch = measurement_owner
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .begin(dispatched_identity.physical_attempt_id.clone());
                                *measurement_dispatch_for_callback
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dispatch);
                                if let Some(timing) = turn_timing.as_ref() {
                                    timing.record_model_request_payload(
                                        &dispatched_identity.sampling_request_id,
                                        &dispatched_identity.physical_attempt_id,
                                        &request_bytes,
                                    );
                                }
                                let _ = dispatched_request_for_callback.set(request_bytes);
                                drop(serialization_timing_guard);
                            }
                        })
                        .await;
                    if stream_result.is_ok() {
                        attempt_clock.mark_stream_established();
                    } else {
                        attempt_clock.mark_completed();
                        if let Some(timing) = self.turn_timing.as_ref() {
                            timing.mark_model_request_attempt_ended();
                        }
                    }
                    self.effective_input = Some(Arc::clone(&request.input));
                    let prompt_cache_key = request.prompt_cache_key.clone();
                    let dispatched_request = dispatched_request.get().cloned();
                    let logical_request_bytes = dispatched_request
                        .as_ref()
                        .map(|request| u64::try_from(request.len()).unwrap_or(u64::MAX));
                    let mut attempt = if let Some(permit) = ModelAttemptState::try_diagnostic_permit() {
                        let measurement_cancellation = CancellationToken::new();
                        let measurement_task =
                            AbortOnDropHandle::new(measure_responses_request_after_dispatch(
                                request,
                                prompt.clone(),
                                // The HTTP path always dispatches the logical projection.
                                SelectedInputRepresentation::default(),
                                measurement_cancellation.clone(),
                                logical_request_bytes,
                                dispatched_request,
                                self.turn_timing
                                    .clone()
                                    .map(|timing| (timing, attempt_identity.clone())),
                                Arc::clone(&permit),
                            ));
                        let fallback_stable_context_manifest = prompt.stable_context_manifest.clone();
                        let prompt_digests = prompt.digests;
                        let prompt_context_measurements = Arc::clone(&self.prompt_context_measurements);
                        let turn_timing = self.turn_timing.clone();
                        let generation = turn_timing
                            .as_ref()
                            .and_then(|timing| timing.current_model_attempt_metadata());
                        let measurement_dispatch = measurement_dispatch
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        let attempt_task_identity = attempt_identity.clone();
                        let attempt_task_clock = attempt_clock.clone();
                        let attempt_task_telemetry = request_session_telemetry.clone();
                        let turn_id = responses_metadata.turn_id.clone();
                        let diagnostic_permit = Arc::clone(&permit);
                        let attempt_task = tokio::spawn(async move {
                        let mut measurement_task = measurement_task;
                        let PostDispatchRequestMeasurements {
                            mut measurements,
                            stable_context_manifest,
                        } = match (&mut measurement_task).await {
                            Ok(measurements) => measurements,
                            Err(err) => {
                                warn!(error = %err, "model request diagnostics join failed after dispatch");
                                PostDispatchRequestMeasurements {
                                    measurements: ModelRequestMeasurements::default(),
                                    stable_context_manifest: fallback_stable_context_manifest,
                                }
                            }
                        };
                        measurements.record_stable_context(
                            &stable_context_manifest,
                            /*baseline_generation*/ None,
                            ModelAttemptProviderBaseline::StatelessFull,
                            /*previous_response_id_present*/ false,
                        );
                        if let Some(dispatch) = measurement_dispatch {
                            dispatch.compare_and_publish(
                                &prompt_context_measurements,
                                &mut measurements,
                                prompt_cache_key.as_deref(),
                                prompt_digests,
                            ).await;
                        }
                        if let Some(timing) = turn_timing
                            .as_ref()
                            .filter(|_| !measurements.prompt_context_categories.is_empty())
                        {
                            timing.record_model_request_token_categories(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                                measurements.request_token_categories(),
                            );
                            timing.record_model_request_cache_identity(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                                measurements.fixed_prefix_reuse_eligible,
                                prompt_cache_key.as_deref(),
                            );
                        }
                        if measurements.prompt_context_categories.is_empty()
                            && let Some(timing) = turn_timing.as_ref()
                        {
                            timing.record_model_request_diagnostics_unavailable(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                            );
                        }
                        ModelAttemptGuard::new(
                            attempt_task_telemetry,
                            attempt_task_identity,
                            retry_index,
                            if retry_index == 0 {
                                ModelAttemptRetryReason::None
                            } else {
                                ModelAttemptRetryReason::Unauthorized
                            },
                            request_kind,
                            ModelAttemptTransport::ResponsesHttp,
                            None,
                            measurements,
                            attempt_task_clock,
                            turn_id,
                            generation,
                        ).with_diagnostic_permit(diagnostic_permit)
                    }.in_current_span());
                        Some(ModelAttemptState::pending(
                            essential_attempt,
                            attempt_task,
                            measurement_cancellation,
                            self.turn_timing.clone(),
                            permit,
                        ))
                    } else {
                        if let Some(timing) = self.turn_timing.as_ref() {
                            timing.record_model_request_diagnostics_unavailable(
                                &attempt_identity.sampling_request_id,
                                &attempt_identity.physical_attempt_id,
                            );
                        }
                        Some(ModelAttemptState::Ready(Box::new(essential_attempt)))
                    };

                    match stream_result {
                        Ok(stream) => {
                            let (stream, _) = map_response_stream(
                                stream,
                                request_session_telemetry,
                                Arc::clone(&self.client.state.provider),
                                attempt,
                                /*stream_throughput*/ None,
                                /*websocket_success*/ None,
                            );
                            return Ok(stream);
                        }
                        Err(ApiError::Transport(unauthorized_transport))
                            if is_recoverable_authentication_failure(&unauthorized_transport) =>
                        {
                            let response_debug_context =
                                extract_response_debug_context(&unauthorized_transport);
                            finish_failed_model_attempt(&mut attempt, Vec::new());
                            pending_retry = PendingUnauthorizedRetry::from_recovery(
                                handle_unauthorized(
                                    unauthorized_transport,
                                    &mut auth_recovery,
                                    session_telemetry,
                                    &self.client.state.provider,
                                )
                                .await?,
                            );
                            retry_index = retry_index.saturating_add(1);
                            continue;
                        }
                        Err(err) => {
                            let response_debug_context = extract_response_debug_context_from_api_error(&err);
                            let err = self.client.state.provider.map_api_error(err);
                            finish_failed_model_attempt(&mut attempt, Vec::new());
                            return Err(err);
                        }
                    }
                }
            }).instrument(span).await
        })
    }

    /// Streams a turn via the Responses API over WebSocket transport.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "model_client.stream_responses_websocket",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "responses_websocket",
            api.path = "responses",
            turn.has_metadata_header = responses_metadata.has_turn_metadata(),
            websocket.warmup = warmup
        )
    )]
    async fn stream_responses_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        sampling_request_id: &str,
        request_kind: ModelAttemptRequestKind,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
        warmup: bool,
        request_trace: Option<W3cTraceContext>,
        attempt_prepared: Option<AttemptPreparedCallback>,
    ) -> Result<WebsocketStreamOutcome> {
        let auth_manager = self.client.state.provider.auth_manager();

        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        loop {
            let websocket_epoch = self
                .client
                .state
                .cached_websocket_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .epoch;
            // `generate=false` warmup is transport setup rather than a sampling attempt.
            let attempt_clock = (!warmup).then(|| ModelAttemptClock {
                provider_event_at_transport: true,
                ..ModelAttemptClock::new()
            });
            let prepared_attempt = if warmup {
                None
            } else {
                self.prepared_startup_websocket_attempt.take()
            };
            let (client_setup, request, transport_readiness_guard, request_transformation_guard) =
                match prepared_attempt {
                    Some(PreparedStartupWebsocketAttempt {
                        client_setup,
                        transport_readiness_guard,
                        request_transformation_guard,
                    }) => {
                        // Setup is reusable; request inputs may have changed since the claim.
                        let request = self.client.build_responses_request(
                            &client_setup.api_provider,
                            prompt,
                            model_info,
                            effort.clone(),
                            summary,
                            service_tier.clone(),
                            responses_metadata,
                        )?;
                        (
                            client_setup,
                            request,
                            transport_readiness_guard,
                            request_transformation_guard,
                        )
                    }
                    None => {
                        let transport_readiness_guard = self.turn_timing.as_ref().map(|timing| {
                            timing.begin_local_phase(TurnLocalPhase::TransportReadiness)
                        });
                        let client_setup = self.client.current_client_setup().await?;
                        let request_transformation_guard =
                            self.turn_timing.as_ref().map(|timing| {
                                timing.begin_local_phase(TurnLocalPhase::RequestTransformation)
                            });
                        let request = self.client.build_responses_request(
                            &client_setup.api_provider,
                            prompt,
                            model_info,
                            effort.clone(),
                            summary,
                            service_tier.clone(),
                            responses_metadata,
                        )?;
                        (
                            client_setup,
                            request,
                            transport_readiness_guard,
                            request_transformation_guard,
                        )
                    }
                };
            // Judge the upcoming request only against comparable work, before
            // a stale collapse can discard this request's healthy connection.
            let setup = websocket_setup_fingerprint(
                &client_setup.api_provider,
                client_setup.api_auth.as_ref(),
            );
            self.websocket_session
                .stream_throughput
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .select_workload(serde_json::json!({
                    "setup": setup.map(|fingerprint| fingerprint.0),
                    "model": &request.model,
                    "reasoning": &request.reasoning,
                    "service_tier": &request.service_tier,
                    "compaction": matches!(responses_metadata.request_kind,
                        Some(crate::responses_metadata::CodexResponsesRequestKind::Compaction(_))),
                    "warmup": warmup,
                    "request_kind": format!("{request_kind:?}"),
                }));
            self.replace_websocket_after_throughput_collapse();
            let mut request = request;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            );
            if !warmup {
                self.dispatched_account = client_setup.auth.as_ref()
                    .and_then(crate::responses_retry::AccountChangeIdentity::from_auth);
            }
            let request_session_telemetry = if warmup {
                // `generate=false` prewarm is connection setup, not an inference request.
                session_telemetry.clone()
            } else {
                session_telemetry_for_request(session_telemetry, &request)
            };
            let mut client_metadata = self
                .client
                .build_ws_client_metadata(responses_metadata, model_info.use_responses_lite);
            if let Some(turn_state) = self.turn_state.get() {
                client_metadata.insert(X_CODEX_TURN_STATE_HEADER.to_string(), turn_state.clone());
            }
            let mut ws_payload = ResponseCreateWsRequest {
                client_metadata: response_create_client_metadata(
                    Some(client_metadata),
                    request_trace.as_ref(),
                ),
                ..ResponseCreateWsRequest::from(&request)
            };
            drop(request_transformation_guard);
            if warmup {
                ws_payload.generate = Some(false);
            }

            if let Some(clock) = attempt_clock.as_ref() {
                clock.mark_connection_setup_started();
            }
            let connection_result = self
                .websocket_connection(WebsocketConnectParams {
                    session_telemetry,
                    routing_hint: build_routing_hint_header(
                        client_setup
                            .auth
                            .as_ref()
                            .filter(|_| self.client.state.provider.info().is_openai()),
                        &client_setup.api_provider,
                        &request.model,
                        request.service_tier.as_deref(),
                    ),
                    api_provider: client_setup.api_provider.clone(),
                    api_auth: client_setup.api_auth,
                    responses_metadata,
                    auth_context: request_auth_context.clone(),
                    request_route_telemetry: RequestRouteTelemetry::for_endpoint(
                        RESPONSES_ENDPOINT,
                    ),
                })
                .await;
            if let Some(clock) = attempt_clock.as_ref() {
                clock.mark_connection_setup_finished();
            }
            match connection_result {
                Ok(_) => {}
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if matches!(status, StatusCode::FORBIDDEN | StatusCode::UPGRADE_REQUIRED) =>
                {
                    // Only a rejected handshake is replayable here, never a 403 from an
                    // established response stream or the subsequent HTTP request.
                    return Ok(WebsocketStreamOutcome::FallbackToHttp);
                }
                Err(ApiError::Transport(unauthorized_transport))
                    if is_recoverable_authentication_failure(&unauthorized_transport) =>
                {
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            session_telemetry,
                            &self.client.state.provider,
                        )
                        .await?,
                    );
                    continue;
                }
                Err(err) => return Err(self.client.state.provider.map_api_error(err)),
            }

            let request_transformation_guard = self
                .turn_timing
                .as_ref()
                .map(|timing| timing.begin_local_phase(TurnLocalPhase::RequestTransformation));
            let tool_history_substitutions = prompt.tool_history_substitutions_for_request(
                /* use_stable_context_fallback */ false,
            );
            let build_tool_history_fail_open_request: Option<
                Box<dyn FnOnce() -> Result<ResponsesApiRequest> + '_>,
            > = if tool_history_substitutions.is_empty() {
                None
            } else {
                let client = self.client.clone();
                let api_provider = client_setup.api_provider.clone();
                let fallback_effort = effort.clone();
                let fallback_service_tier = service_tier.clone();
                Some(Box::new(move || {
                    let mut fallback_request = client.build_responses_request_with_fallbacks(
                        &api_provider,
                        prompt,
                        model_info,
                        fallback_effort,
                        summary,
                        fallback_service_tier,
                        responses_metadata,
                        /* use_stable_context_fallback */ true,
                        /* use_tool_history_fallback */ true,
                    )?;
                    client.prepare_response_items_for_request(&mut fallback_request.input);
                    Ok(fallback_request)
                }))
            };
            let (
                mut ws_request,
                previous_response_id_from_untraced_warmup,
                tool_history_fail_open_override,
                mut verified_history,
            ) = self.prepare_websocket_request(
                ws_payload,
                &request,
                prompt.stable_context_manifest.fingerprint(),
                tool_history_substitutions,
                build_tool_history_fail_open_request,
            )?;
            let mut selected_representation = SelectedInputRepresentation::default();
            if let Some(fallback_request) = tool_history_fail_open_override {
                request = fallback_request;
                // Built above with both fallbacks enabled.
                selected_representation = SelectedInputRepresentation {
                    stable_context_fallback: true,
                    tool_history_fallback: true,
                };
            }
            let inherited_stable_context_matches = self
                .websocket_session
                .last_request_history
                .as_ref()
                .is_some_and(|baseline| {
                    baseline.stable_context_fingerprint
                        == prompt.stable_context_manifest.fingerprint()
                });
            let ResponsesWsRequest::ResponseCreate(final_payload) = &mut ws_request;
            if final_payload.previous_response_id.is_some()
                && !inherited_stable_context_matches
                && verified_history.is_none()
            {
                trace!("discarding unproven provider inheritance before stable-context dispatch");
                self.invalidate_incremental_history("stable context inheritance unproven");
                final_payload.previous_response_id = None;
                let mut fallback_request = self.client.build_responses_request_with_input(
                    &client_setup.api_provider,
                    prompt,
                    model_info,
                    effort.clone(),
                    summary,
                    service_tier.clone(),
                    responses_metadata,
                    /* use_stable_context_fallback */ true,
                )?;
                self.client
                    .prepare_response_items_for_request(&mut fallback_request.input);
                final_payload.input = fallback_request.input.clone();
                request = fallback_request;
                verified_history = None;
                selected_representation = SelectedInputRepresentation {
                    stable_context_fallback: true,
                    tool_history_fallback: false,
                };
            }
            // `prepare_websocket_request` may invalidate a superseded stable
            // context baseline. Read the generation only after that decision
            // so a fresh full replay is never attributed to the stale provider
            // generation it replaced.
            let baseline_generation = self
                .websocket_session
                .last_request_history
                .as_ref()
                .map(|baseline| baseline.generation);
            let previous_response_id_present = match &ws_request {
                ResponsesWsRequest::ResponseCreate(payload) => {
                    payload.previous_response_id.is_some()
                }
            };
            let provider_baseline = if previous_response_id_present {
                ModelAttemptProviderBaseline::ExactPrefixInherited
            } else {
                ModelAttemptProviderBaseline::FreshFullReplay
            };
            stamp_ws_stream_request_start_ms(&mut ws_request);
            let ResponsesWsRequest::ResponseCreate(ws_payload) = &mut ws_request;
            self.client
                .prepare_response_items_for_request(&mut ws_payload.input);
            let logical_request_for_measurement = if warmup {
                None
            } else {
                // Preserve the full logical request for accounting even when the WebSocket
                // transport sends a delta, and normalize it exactly like the wire payload.
                let mut logical_request = request.clone();
                self.client
                    .prepare_response_items_for_request(&mut logical_request.input);
                Some(logical_request)
            };
            drop(request_transformation_guard);
            let websocket_connection =
                self.websocket_session.connection.as_ref().ok_or_else(|| {
                    self.client.state.provider.map_api_error(ApiError::Stream(
                        "websocket connection is unavailable".to_string(),
                    ))
                })?;
            let attempt_identity = attempt_clock
                .as_ref()
                .map(|_| new_attempt_identity(sampling_request_id));
            let connection_reused = self.websocket_session.connection_reused();
            let mut essential_attempt =
                attempt_identity
                    .as_ref()
                    .zip(attempt_clock.as_ref())
                    .map(|(identity, clock)| {
                        ModelAttemptGuard::new(
                            request_session_telemetry.clone(),
                            identity.clone(),
                            0,
                            ModelAttemptRetryReason::None,
                            request_kind,
                            ModelAttemptTransport::ResponsesWebsocket,
                            Some(connection_reused),
                            ModelRequestMeasurements::default(),
                            clock.clone(),
                            responses_metadata.turn_id.clone(),
                            self.turn_timing
                                .as_ref()
                                .and_then(|timing| timing.current_model_attempt_metadata()),
                        )
                    });
            if let (Some(timing), Some(identity)) =
                (self.turn_timing.as_ref(), attempt_identity.as_ref())
            {
                timing.record_model_attempt_identity(
                    &identity.sampling_request_id,
                    &identity.physical_attempt_id,
                );
                timing.record_model_estimator_model(
                    &identity.sampling_request_id, &identity.physical_attempt_id,
                    &model_info.slug,
                );
            }
            if let Some(clock) = attempt_clock.as_ref() {
                clock.mark_queue_started();
            }
            let sampling_admission = match (attempt_identity.as_ref(), attempt_prepared.as_ref()) {
                (Some(identity), Some(attempt_prepared)) => {
                    attempt_prepared(identity.clone()).await
                }
                _ => Ok(None),
            };
            if let Some(clock) = attempt_clock.as_ref() {
                clock.mark_queue_finished();
            }
            let _sampling_admission = sampling_admission?;
            let turn_timing = self.turn_timing.clone();
            let serialization_timing_guard = self
                .turn_timing
                .as_ref()
                .map(|timing| timing.begin_local_phase(TurnLocalPhase::Serialization));
            let queue_clock = attempt_clock.clone();
            let established_clock = attempt_clock.clone();
            let dispatched_request = Arc::new(OnceLock::new());
            let dispatched_request_for_callback = Arc::clone(&dispatched_request);
            let measurement_dispatch = Arc::new(StdMutex::new(None));
            let measurement_dispatch_for_callback = Arc::clone(&measurement_dispatch);
            let measurement_owner = Arc::clone(&self.prompt_context_measurements);
            let dispatched_identity = attempt_identity.clone();
            let transport_identity = attempt_identity.clone();
            let transport_clock = attempt_clock.clone();
            let transport_timing = self.turn_timing.as_ref().map(Arc::downgrade);
            let logical_transport_id = websocket_connection
                .logical_transport_id()
                .map(str::to_owned);
            let response_telemetry =
                attempt_identity
                    .as_ref()
                    .zip(attempt_clock.as_ref())
                    .map(|(identity, clock)| {
                        let mut telemetry = ApiTelemetry::new(
                            request_session_telemetry.clone(),
                            request_auth_context,
                            RequestRouteTelemetry::for_endpoint(RESPONSES_ENDPOINT),
                            self.client.state.auth_env_telemetry.clone(),
                        );
                        telemetry.logical_request_id = logical_transport_id
                            .clone()
                            .unwrap_or_else(new_sampling_request_id);
                        telemetry.websocket_response = Some((identity.clone(), clock.clone()));
                        Arc::new(telemetry) as Arc<dyn WebsocketTelemetry>
                    });
            let startup_timing = self.startup_timing.clone();
            let stream_result = websocket_connection
                .stream_request_with_observers(
                    &ws_request,
                    connection_reused,
                    Some(Arc::clone(&self.turn_state)),
                    prompt.stream_progress_timeout,
                    move || {
                        if let Some(clock) = queue_clock {
                            clock.mark_queue_started();
                        }
                    },
                    move |request_bytes| {
                        if let Some(identity) = dispatched_identity.as_ref() {
                            let dispatch = measurement_owner
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .begin(identity.physical_attempt_id.clone());
                            *measurement_dispatch_for_callback
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dispatch);
                        }
                        if !warmup
                            && let Some(timing) = turn_timing.as_ref()
                            && let Some(identity) = dispatched_identity.as_ref()
                        {
                            timing.record_model_request_payload(
                                &identity.sampling_request_id,
                                &identity.physical_attempt_id,
                                &request_bytes,
                            );
                        }
                        let _ = dispatched_request_for_callback.set(request_bytes);
                        drop(serialization_timing_guard);
                        drop(transport_readiness_guard);
                    },
                    move || {
                        if let Some(clock) = established_clock {
                            clock.mark_stream_established();
                        }
                    },
                    (!warmup).then(|| {
                        Arc::new(move || {
                            if let Some(clock) = &transport_clock {
                                clock.mark_dispatch_ready();
                            }
                            if let Some(timing) =
                                transport_timing.as_ref().and_then(std::sync::Weak::upgrade)
                                && let Some(identity) = &transport_identity
                            {
                                timing.mark_model_attempt_dispatched(
                                    &identity.sampling_request_id,
                                    &identity.physical_attempt_id,
                                    false,
                                );
                            }
                            if let Some(identity) = &transport_identity {
                                tracing::info!(
                                    event.name = "codex.model_transport_identity",
                                    transport = "websocket",
                                    logical_request_id = logical_transport_id.as_deref(),
                                    sampling_request_id = %identity.sampling_request_id,
                                    physical_attempt_id = %identity.physical_attempt_id,
                                    connection_reused,
                                    "model transport identity at send attempt"
                                );
                            }
                            if let Some(timing) = &startup_timing {
                                timing.freeze_at_first_transport_attempt();
                            }
                        }) as Arc<dyn Fn() + Send + Sync>
                    }),
                    response_telemetry,
                )
                .await;
            if stream_result.is_err()
                && let Some(clock) = attempt_clock.as_ref()
            {
                clock.mark_completed();
            }
            let mut attempt = match (
                logical_request_for_measurement,
                attempt_clock,
                attempt_identity,
            ) {
                (Some(logical_request), Some(attempt_clock), Some(attempt_identity)) => {
                    // Identity and clock imply the essential owner exists. Take it only
                    // with a permit; otherwise the ready-attempt branch below keeps it.
                    if let Some(permit) = ModelAttemptState::try_diagnostic_permit()
                        && let Some(essential) = essential_attempt.take()
                    {
                        let prompt_cache_key = logical_request.prompt_cache_key.clone();
                        let dispatched_request = dispatched_request.get().cloned();
                        let measurement_cancellation = CancellationToken::new();
                        let measurement_task =
                            AbortOnDropHandle::new(measure_responses_request_after_dispatch(
                                logical_request,
                                prompt.clone(),
                                selected_representation,
                                measurement_cancellation.clone(),
                                /*logical_request_bytes*/ None,
                                dispatched_request,
                                self.turn_timing
                                    .clone()
                                    .map(|timing| (timing, attempt_identity.clone())),
                                Arc::clone(&permit),
                            ));
                        let fallback_stable_context_manifest =
                            prompt.stable_context_manifest.clone();
                        let prompt_digests = prompt.digests;
                        let prompt_context_measurements = Arc::clone(&self.prompt_context_measurements);
                        let turn_timing = self.turn_timing.clone();
                        let generation = turn_timing
                            .as_ref()
                            .and_then(|timing| timing.current_model_attempt_metadata());
                        let measurement_dispatch = measurement_dispatch
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take();
                        let attempt_task_identity = attempt_identity;
                        let attempt_task_clock = attempt_clock;
                        let attempt_task_telemetry = request_session_telemetry.clone();
                        let turn_id = responses_metadata.turn_id.clone();
                        let diagnostic_permit = Arc::clone(&permit);
                        let attempt_task = tokio::spawn(async move {
                        let mut measurement_task = measurement_task;
                        let PostDispatchRequestMeasurements {
                            mut measurements,
                            stable_context_manifest,
                        } = match (&mut measurement_task).await {
                            Ok(measurements) => measurements,
                            Err(err) => {
                                warn!(error = %err, "model request diagnostics join failed after dispatch");
                                PostDispatchRequestMeasurements {
                                    measurements: ModelRequestMeasurements::default(),
                                    stable_context_manifest: fallback_stable_context_manifest,
                                }
                            }
                        };
                        measurements.record_stable_context(
                            &stable_context_manifest,
                            baseline_generation,
                            provider_baseline,
                            previous_response_id_present,
                        );
                        if let Some(dispatch) = measurement_dispatch {
                            dispatch.compare_and_publish(
                                &prompt_context_measurements,
                                &mut measurements,
                                prompt_cache_key.as_deref(),
                                prompt_digests,
                            ).await;
                        }
                        if let Some(timing) = turn_timing
                            .as_ref()
                            .filter(|_| !measurements.prompt_context_categories.is_empty())
                        {
                            timing.record_model_request_token_categories(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                                measurements.request_token_categories(),
                            );
                            timing.record_model_request_cache_identity(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                                measurements.fixed_prefix_reuse_eligible,
                                prompt_cache_key.as_deref(),
                            );
                        }
                        if measurements.prompt_context_categories.is_empty()
                            && let Some(timing) = turn_timing.as_ref()
                        {
                            timing.record_model_request_diagnostics_unavailable(
                                &attempt_task_identity.sampling_request_id,
                                &attempt_task_identity.physical_attempt_id,
                            );
                        }
                        ModelAttemptGuard::new(
                            attempt_task_telemetry,
                            attempt_task_identity,
                            0,
                            ModelAttemptRetryReason::None,
                            request_kind,
                            ModelAttemptTransport::ResponsesWebsocket,
                            Some(connection_reused),
                            measurements,
                            attempt_task_clock,
                            turn_id,
                            generation,
                        ).with_diagnostic_permit(diagnostic_permit)
                    }.in_current_span());
                        Some(ModelAttemptState::pending(
                            essential,
                            attempt_task,
                            measurement_cancellation,
                            self.turn_timing.clone(),
                            permit,
                        ))
                    } else {
                        if let Some(timing) = self.turn_timing.as_ref() {
                            timing.record_model_request_diagnostics_unavailable(
                                &attempt_identity.sampling_request_id,
                                &attempt_identity.physical_attempt_id,
                            );
                        }
                        essential_attempt.take().map(|mut guard| {
                            guard.turn_timing = self.turn_timing.clone();
                            ModelAttemptState::Ready(Box::new(guard))
                        })
                    }
                }
                _ => None,
            };
            let stream_result = match stream_result {
                Ok(stream) => stream,
                Err(err) => {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let err = self.client.state.provider.map_api_error(err);
                    finish_failed_model_attempt(&mut attempt, Vec::new());
                    return Err(err);
                }
            };
            self.remember_verified_request_history(
                &request,
                prompt.stable_context_manifest.fingerprint(),
                verified_history,
            );
            self.effective_input = Some(Arc::clone(&request.input));
            self.websocket_session.last_request = Some(request);
            self.websocket_session.last_response_from_untraced_warmup = warmup;
            let (stream, last_request_rx) = map_response_stream(
                stream_result,
                request_session_telemetry,
                Arc::clone(&self.client.state.provider),
                attempt,
                Some(Arc::clone(&self.websocket_session.stream_throughput)),
                (!warmup).then(|| (self.client.clone(), websocket_epoch)),
            );
            self.websocket_session.last_response_rx = Some(last_request_rx);
            self.websocket_session.last_response = None;
            return Ok(WebsocketStreamOutcome::Stream(stream));
        }
    }

    /// Builds request and SSE telemetry for streaming API calls.
    fn build_streaming_telemetry(
        session_telemetry: &SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
        model_request: ModelRequestTelemetryContext,
    ) -> (Arc<dyn RequestTelemetry>, Arc<dyn SseTelemetry>) {
        let mut telemetry = ApiTelemetry::new(
            session_telemetry.clone(),
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        );
        // The transport logical request is this core API attempt; retries are
        // distinguished by the lower transport ordinal, not a second UUID.
        telemetry.logical_request_id = model_request.identity.physical_attempt_id.clone();
        session_telemetry.record_model_transport_identity(
            &telemetry.logical_request_id,
            Some(&model_request.identity.sampling_request_id),
            Some(&model_request.identity.physical_attempt_id),
            "model_request",
            None,
        );
        tracing::info!(
            event.name = "codex.model_transport_identity",
            logical_request_id = %telemetry.logical_request_id,
            sampling_request_id = %model_request.identity.sampling_request_id,
            physical_attempt_id = %model_request.identity.physical_attempt_id,
            turn_id = model_request.turn_id.as_deref(),
            "model transport identity"
        );
        telemetry.model_request = Some(model_request);
        let telemetry = Arc::new(telemetry);
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry.clone();
        let sse_telemetry: Arc<dyn SseTelemetry> = telemetry;
        (request_telemetry, sse_telemetry)
    }

    /// Builds telemetry for the Responses API WebSocket transport.
    fn build_websocket_telemetry(
        session_telemetry: &SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> Arc<dyn WebsocketTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(
            session_telemetry.clone(),
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        ));
        let websocket_telemetry: Arc<dyn WebsocketTelemetry> = telemetry;
        websocket_telemetry
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prewarm_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
    ) -> Result<()> {
        self.finish_websocket_preconnect().await?;
        if !self.client.responses_websocket_enabled() {
            return Ok(());
        }
        if self.websocket_session.last_request.is_some() {
            return Ok(());
        }

        self.last_stream_transport = Some(ModelAttemptTransport::ResponsesWebsocket);
        match self
            .stream_responses_websocket(
                prompt,
                model_info,
                session_telemetry,
                "",
                ModelAttemptRequestKind::Initial,
                effort,
                summary,
                service_tier,
                responses_metadata,
                /*warmup*/ true,
                current_span_w3c_trace_context(),
                None,
            )
            .await
        {
            Ok(WebsocketStreamOutcome::Stream(mut stream)) => {
                // Wait for the v2 warmup request to complete before sending the first turn request.
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(ResponseEvent::Completed { .. }) => return Ok(()),
                        Err(err) => return Err(err),
                        _ => {}
                    }
                }
                Err(CodexErr::Stream(
                    "stream closed before response.completed".to_string(),
                    None,
                ))
            }
            Ok(WebsocketStreamOutcome::FallbackToHttp) => {
                self.try_switch_fallback_transport(session_telemetry);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Streams a single model request within the current turn.
    ///
    /// The caller is responsible for passing per-turn settings explicitly (model selection,
    /// reasoning settings, telemetry context, and turn metadata). This method will prefer the
    /// Responses WebSocket transport when the provider supports it and it remains healthy, and will
    /// fall back to the HTTP Responses API transport otherwise.
    pub async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
    ) -> Result<ResponseStream> {
        // Standalone callers have no turn retry owner. Preserve their ordinary
        // connection attempt after transport-only speculation, including when
        // stream retries are disabled. Turn dispatch calls the prepared path
        // directly and accounts for the speculative failure in its retry budget.
        if let Err(error) = self.finish_websocket_preconnect().await {
            warn!("standalone websocket preconnect failed; connecting at dispatch: {error:#}");
        }
        self.stream_with_attempt_prepared_inner(
            prompt,
            model_info,
            session_telemetry,
            effort,
            summary,
            service_tier,
            responses_metadata,
            None,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn stream_with_attempt_prepared<'a>(
        &'a mut self,
        prompt: &'a Prompt,
        model_info: &'a ModelInfo,
        session_telemetry: &'a SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &'a CodexResponsesMetadata,
        attempt_prepared: Option<AttemptPreparedCallback>,
    ) -> impl std::future::Future<Output = Result<ResponseStream>> + 'a {
        self.stream_with_attempt_prepared_inner(
            prompt,
            model_info,
            session_telemetry,
            effort,
            summary,
            service_tier,
            responses_metadata,
            attempt_prepared,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_with_attempt_prepared_inner<'a>(
        &'a mut self,
        prompt: &'a Prompt,
        model_info: &'a ModelInfo,
        session_telemetry: &'a SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &'a CodexResponsesMetadata,
        attempt_prepared: Option<AttemptPreparedCallback>,
        turn_scoped: bool,
    ) -> impl std::future::Future<Output = Result<ResponseStream>> + 'a {
        // Construct the transport future outside the caller's polling frame;
        // the dispatch owner already heap-pins its input on this same task.
        codex_http_client::with_request_dispatch_scope(async move {
            // A receipt is valid only for this immediate ordinary dispatch.
            self.preconnect_failure_credit = false;
            self.dispatched_account = None;
            self.last_logical_request_counted = false;
            if turn_scoped && !self.client.responses_websocket_enabled() {
                self.http_for_turn = true;
            }
            if turn_scoped && self.http_for_turn {
                // Owned speculation cannot become useful in this turn again.
                self.pending_websocket_preconnect.take();
            }
            // Resolve before auth setup and transport selection: preconnect can refresh auth or
            // discover that the endpoint requires HTTP. The normal connection path still checks
            // provider/auth compatibility, expiry, and closure before reusing the socket.
            let failed_preconnect_setup =
                if let Err(error) = self.finish_websocket_preconnect().await {
                    // No prompt was dispatched by speculation. Preserve the immediate
                    // ordinary attempt, including zero-retry transient recovery.
                    warn!("turn websocket preconnect failed; retrying at dispatch: {error:#}");
                    let failed_setup = self.preconnect_failure_setup.take().and_then(|setup| {
                        setup
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .take()
                    });
                    if turn_scoped && !self.http_for_turn
                        && let Some(failed_setup) = failed_setup
                    {
                        let current = self.client.current_client_setup().await?;
                        if websocket_setup_fingerprint(&current.api_provider, current.api_auth.as_ref())
                            == Some(failed_setup)
                        {
                            // Let the ordinary retry/fallback owner charge this
                            // same-setup failure without a second handshake.
                            return Err(error);
                        }
                    }
                    failed_setup
                } else {
                    self.preconnect_failure_setup.take();
                    None
                };
            self.failed_websocket_setup = None;
            let sampling_request_id = new_sampling_request_id();
            let request_kind = self.next_logical_request_kind(responses_metadata);
            let wire_api = self.client.state.provider.info().wire_api;
            match wire_api {
                WireApi::Responses => {
                    if (!turn_scoped || !self.http_for_turn)
                        && self.client.responses_websocket_enabled()
                    {
                        self.last_stream_transport =
                            Some(ModelAttemptTransport::ResponsesWebsocket);
                        let request_trace = current_span_w3c_trace_context();
                        let websocket_result = Box::pin(self.stream_responses_websocket(
                            prompt,
                            model_info,
                            session_telemetry,
                            &sampling_request_id,
                            request_kind,
                            effort.clone(),
                            summary,
                            service_tier.clone(),
                            responses_metadata,
                            /*warmup*/ false,
                            request_trace,
                            attempt_prepared.clone(),
                        ))
                        .await;
                        if websocket_result.is_err() {
                            // Do not transfer failures across auth/provider
                            // transitions, or credit post-handshake stream errors.
                            self.preconnect_failure_credit = failed_preconnect_setup.is_some()
                                && failed_preconnect_setup == self.failed_websocket_setup;
                        }
                        match websocket_result? {
                            WebsocketStreamOutcome::Stream(stream) => return Ok(stream),
                            WebsocketStreamOutcome::FallbackToHttp => {
                                self.try_switch_fallback_transport(session_telemetry);
                            }
                        }
                    }

                    self.last_stream_transport = Some(ModelAttemptTransport::ResponsesHttp);
                    Box::pin(self.stream_responses_api(
                        prompt,
                        model_info,
                        session_telemetry,
                        &sampling_request_id,
                        request_kind,
                        effort,
                        summary,
                        service_tier,
                        responses_metadata,
                        attempt_prepared,
                    ))
                    .await
                }
            }
        })
    }

    pub(crate) fn effective_input(&self) -> Option<&[ResponseItem]> {
        self.effective_input.as_deref()
    }

    pub(crate) fn effective_input_shared(&self) -> Option<Arc<[ResponseItem]>> {
        self.effective_input.as_ref().map(Arc::clone)
    }

    /// Permanently disables WebSockets for this Codex session and resets WebSocket state.
    ///
    /// This is used when the server rejects the WebSocket upgrade.
    ///
    /// Returns `true` when the failed stream should be retried over HTTP. This includes a concurrent
    /// session having already activated the shared fallback while this session still had an
    /// in-flight WebSocket stream.
    pub(crate) fn try_switch_fallback_transport(
        &mut self,
        session_telemetry: &SessionTelemetry,
    ) -> bool {
        self.switch_fallback_transport(session_telemetry, /*recoverable*/ false)
    }

    pub(crate) fn try_switch_fallback_transport_after_error(
        &mut self,
        session_telemetry: &SessionTelemetry,
        err: &CodexErr,
    ) -> bool {
        let recoverable = match err {
            CodexErr::ResponseStreamFailed(error) => error.status.is_none(),
            CodexErr::ConnectionFailed(error) => error.status.is_none(),
            // An exhausted retryable upgrade failure can recover on a later
            // turn. The shared cooldown still makes a second failure permanent.
            CodexErr::UnexpectedStatus(error) => error.status.is_server_error(),
            _ => false,
        };
        self.switch_fallback_transport(session_telemetry, recoverable)
    }

    pub(crate) fn take_preconnect_failure_credit(&mut self) -> bool {
        std::mem::take(&mut self.preconnect_failure_credit)
    }

    fn switch_fallback_transport(
        &mut self,
        session_telemetry: &SessionTelemetry,
        recoverable: bool,
    ) -> bool {
        // The cooldown may expire during an HTTP request or network-recovery wait. Only
        // WebSocket failures can change its state, never failures of that HTTP attempt.
        if self.last_stream_transport == Some(ModelAttemptTransport::ResponsesHttp) {
            return false;
        }
        let failed_stream_was_websocket =
            self.last_stream_transport == Some(ModelAttemptTransport::ResponsesWebsocket);
        if !failed_stream_was_websocket && !self.client.responses_websocket_enabled() {
            return false;
        }
        let activated = self
            .client
            .activate_http_fallback(session_telemetry, recoverable);
        self.websocket_session = WebsocketSession::default();
        self.last_stream_transport = Some(ModelAttemptTransport::ResponsesHttp);
        self.http_for_turn = true;
        activated || failed_stream_was_websocket
    }
}

/// Stamp a ResponsesWsRequest with the current time.
///
/// Meant to be called just before sending the request over the socket, to capture realistic
/// transport timing.
fn stamp_ws_stream_request_start_ms(request: &mut ResponsesWsRequest) {
    let ResponsesWsRequest::ResponseCreate(payload) = request;
    payload
        .client_metadata
        .get_or_insert_with(HashMap::new)
        .insert(
            X_CODEX_WS_STREAM_REQUEST_START_MS_CLIENT_METADATA_KEY.to_string(),
            crate::turn_timing::now_unix_timestamp_ms().to_string(),
        );
}

/// Builds the extra headers attached to Responses API requests.
///
/// These headers implement Codex-specific conventions:
///
/// - `x-codex-beta-features`: comma-separated beta feature keys enabled for the session.
/// - `x-codex-turn-state`: sticky routing token captured earlier in the turn.
fn build_responses_headers(
    beta_features_header: Option<&str>,
    turn_state: Option<&Arc<OnceLock<String>>>,
) -> ApiHeaderMap {
    let mut headers = ApiHeaderMap::new();
    if let Some(value) = beta_features_header
        && !value.is_empty()
        && let Ok(header_value) = HeaderValue::from_str(value)
    {
        headers.insert("x-codex-beta-features", header_value);
    }
    if let Some(turn_state) = turn_state
        && let Some(state) = turn_state.get()
        && let Ok(header_value) = HeaderValue::from_str(state)
    {
        headers.insert(X_CODEX_TURN_STATE_HEADER, header_value);
    }
    headers
}

pub(crate) fn add_originator_header(headers: &mut ApiHeaderMap, originator: &str) {
    let default_originator = codex_login::default_client::originator();
    if originator == default_originator.value.as_str() {
        return;
    }

    match HeaderValue::from_str(originator) {
        Ok(header_value) => {
            headers.insert("originator", header_value);
        }
        Err(err) => {
            warn!("ignoring invalid thread originator header value: {err}");
        }
    }
}

fn add_responses_lite_header(headers: &mut ApiHeaderMap, use_responses_lite: bool) {
    if use_responses_lite {
        headers.insert(
            X_OPENAI_INTERNAL_CODEX_RESPONSES_LITE_HEADER,
            HeaderValue::from_static("true"),
        );
    }
}

const RESPONSE_STREAM_CHANNEL_CAPACITY: usize = 1600;

fn map_response_stream(
    api_stream: codex_api::ResponseStream,
    session_telemetry: SessionTelemetry,
    provider: SharedModelProvider,
    attempt: Option<ModelAttemptState>,
    stream_throughput: Option<Arc<StdMutex<WebsocketStreamThroughput>>>,
    websocket_success: Option<(ModelClient, u64)>,
) -> (ResponseStream, oneshot::Receiver<LastResponse>) {
    let codex_api::ResponseStream {
        mut rx_event,
        upstream_request_id,
    } = api_stream;
    if let Some(attempt) = attempt.as_ref() {
        attempt.observe_response_queue(&mut rx_event, "api_to_mapper");
    }
    let api_stream = futures::stream::poll_fn(move |cx| rx_event.poll_recv_charged(cx))
    .inspect(move |event| {
        // Opening a stream or receiving partial output is not a successful
        // request. HTTP and connection-only warmup never clear WS failures.
        if matches!(event.event(), Ok(ResponseEvent::Completed { .. }))
            && let Some((client, epoch)) = websocket_success.as_ref()
        {
            client
                .state
                .cached_websocket_transport
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .record_success(*epoch);
        }
    });
    map_response_events(
        upstream_request_id,
        api_stream,
        session_telemetry,
        provider,
        attempt,
        stream_throughput,
    )
}

fn map_response_events<S>(
    upstream_request_id: Option<String>,
    api_stream: S,
    session_telemetry: SessionTelemetry,
    provider: SharedModelProvider,
    mut attempt: Option<ModelAttemptState>,
    stream_throughput: Option<Arc<StdMutex<WebsocketStreamThroughput>>>,
) -> (ResponseStream, oneshot::Receiver<LastResponse>)
where
    S: futures::Stream
        + Unpin
        + Send
        + 'static,
    S::Item: Into<codex_api::response_event_channel::ChargedResponseEvent<ApiError>> + Send + 'static,
{
    let attempt_identity = attempt.as_ref().map(ModelAttemptState::response_identity);
    let attempt_clock = attempt.as_ref().map(ModelAttemptState::clock);
    let (tx_event, mut rx_event) =
        codex_api::response_event_channel::response_event_channel(RESPONSE_STREAM_CHANNEL_CAPACITY);
    if let Some(attempt) = attempt.as_ref() {
        attempt.observe_response_queue(&mut rx_event, "mapper_to_turn");
    }
    let (tx_last_response, rx_last_response) = oneshot::channel::<LastResponse>();
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_stream = consumer_dropped.clone();
    let dispatched_at = Instant::now();
    let terminal_delivery_identity = attempt_identity.clone();
    let delivery_turn_timing = attempt.as_ref().and_then(|attempt| {
        let guard = match attempt {
            ModelAttemptState::Ready(guard) => guard.as_ref(),
            ModelAttemptState::Pending(pending) => pending.essential.as_ref(),
        };
        guard.turn_timing.clone()
    });

    tokio::spawn(
        async move {
            let mut tx_last_response = Some(tx_last_response);
            let mut delivery = ModelStreamDeliveryObserver::new(
                terminal_delivery_identity.clone(),
                attempt_clock.clone(),
                delivery_turn_timing,
            );
            let mut items_added: Vec<ResponseItem> = Vec::new();
            let mut ttft_ms = None;
            // First-event telemetry is one-shot. Once recorded, later deltas must
            // not reacquire the timing mutex or clone the clock's Arc.
            let mut first_provider_event_recorded = false;
            let mut first_model_output_recorded = false;
            let mut first_actionable_output_recorded = false;
            let mut first_visible_output_recorded = false;
            let mut api_stream = api_stream;
            let mut downstream_backpressured = false;
            let upstream_request_id = upstream_request_id.as_deref();
            if let Some(upstream_request_id) = upstream_request_id {
                feedback_tags!(last_model_request_id = upstream_request_id);
            }
            loop {
                let event = tokio::select! {
                    _ = consumer_dropped.cancelled() => {
                        return;
                    }
                    event = api_stream.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                let mut event: codex_api::response_event_channel::ChargedResponseEvent<ApiError> = event.into();
                if let Ok(
                    ResponseEvent::OutputItemAdded(item) | ResponseEvent::OutputItemDone(item),
                ) = event.event()
                    && crate::stable_context::provider_context_item_id_needs_normalization(item)
                {
                    event = event.map(|mut event| {
                        if let Ok(ResponseEvent::OutputItemAdded(item) | ResponseEvent::OutputItemDone(item)) = &mut event {
                            crate::stable_context::normalize_provider_context_item_id(item);
                        }
                        event
                    });
                }
                let queued_payload_bytes = event.payload_bytes();
                if queued_payload_bytes
                    > codex_api::response_event_channel::RESPONSE_EVENT_QUEUE_BYTES
                {
                    event = Err(ApiError::invalid_response(
                        "decoded response event exceeds the queue payload limit",
                    )).into();
                }
                if let Ok(response_event) = event.event()
                    && let Some(clock) = attempt_clock.as_ref()
                {
                    if !first_provider_event_recorded && !clock.provider_event_at_transport {
                        clock.mark_first_provider_event();
                        first_provider_event_recorded = true;
                    }
                    if !first_model_output_recorded && response_event_records_model_output(response_event) {
                        clock.mark_first_model_output();
                        first_model_output_recorded = true;
                    }
                    delivery.observe(response_event);
                    if ttft_ms.is_none() && response_event_records_model_content(response_event) {
                        ttft_ms.get_or_insert_with(|| {
                            let offsets = clock
                                .offsets
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            i64::try_from(
                                clock.elapsed_us()
                                    .saturating_sub(offsets.dispatch_ready_us.unwrap_or_default())
                                    / 1_000,
                            )
                            .unwrap_or(i64::MAX)
                        });
                    }
                    if !first_actionable_output_recorded
                        && response_event_records_actionable_output(response_event)
                    {
                        clock.mark_first_actionable_output();
                        first_actionable_output_recorded = true;
                    }
                }
                let records_visible_output = !first_visible_output_recorded
                    && attempt_clock.is_some()
                    && event
                        .event()
                        .as_ref()
                        .is_ok_and(response_event_records_visible_output);
                // With one sender, an empty capacity here means forwarding this
                // nonterminal event can block polling the provider. Its completion
                // time no longer isolates backend speed. Do not subtract this wait:
                // it can overlap real provider work and invent an inflated rate.
                if !matches!(event.event(), Ok(ResponseEvent::Completed { .. }))
                    && tx_event.is_backpressured(queued_payload_bytes)
                {
                    downstream_backpressured = true;
                    delivery.summary.downstream_backpressured = true;
                }
                match event.event() {
                    Ok(ResponseEvent::OutputItemDone(item)) => {
                        items_added.push(item.clone());
                        if tx_event
                            .send_charged(event.map_err(|error| provider.map_api_error(error)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if records_visible_output && let Some(clock) = attempt_clock.as_ref() {
                            clock.mark_first_visible_output();
                            first_visible_output_recorded = true;
                        }
                    }
                    Ok(ResponseEvent::Completed {
                        response_id,
                        token_usage,
                        ..
                    }) => {
                        delivery.publish(true);
                        let response_id = response_id.clone();
                        let token_usage = token_usage.clone();
                        if let Some(throughput) = stream_throughput.as_ref() {
                            let mut throughput = throughput
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if downstream_backpressured || token_usage.is_none() {
                                // A censored sample neither pollutes the baseline
                                // nor joins unrelated slow responses into a streak.
                                throughput.collapsed_streak = 0;
                                throughput.latest_collapse = None;
                            } else if let Some(usage) = token_usage.as_ref() {
                                throughput.record(
                                    usage.output_tokens,
                                    dispatched_at.elapsed(),
                                    usage.input_tokens,
                                    /*sustained_delivery_interval*/ false,
                                );
                            }
                        }
                        feedback_tags!(last_model_response_id = &response_id);
                        if let Some(attempt) = attempt.as_ref() {
                            attempt.clock().mark_completed();
                        }
                        if let Some(attempt) = attempt.as_mut() {
                            // Provider truth must exist before optional trace IO or
                            // a backpressured relay can suspend this task. Pending
                            // diagnostics enrich it; they do not own its existence.
                            attempt.finish_essential(
                                ModelAttemptOutcome::Success,
                                Some(&response_id),
                                token_usage.as_ref(),
                                &items_added,
                                true,
                            );
                        }
                        // History and diagnostics share immutable completed output;
                        // converting the collection does not clone its payloads.
                        let items_added: Arc<[ResponseItem]> = items_added.into();
                        // Publish authoritative history before releasing the consumer.
                        // Optional diagnostics must not hold deferred tools or another
                        // model request behind token measurement work.
                        // HTTP discards this receiver. Do not clone a whole output
                        // history just to send it into an already-closed channel.
                        if let Some(sender) =
                            tx_last_response.take().filter(|sender| !sender.is_closed())
                        {
                            let _ = sender.send(LastResponse {
                                response_id: response_id.clone(),
                                items_added: Arc::clone(&items_added),
                            });
                        }
                        if tx_event
                            .send_charged(event.map_err(|error| provider.map_api_error(error)))
                            .await
                            .is_err()
                        {
                            if let Some(identity) = terminal_delivery_identity.as_ref() {
                                tracing::info!(
                                    event.name = "codex.model_terminal_delivery",
                                    sampling_request_id = %identity.sampling_request_id,
                                    physical_attempt_id = %identity.physical_attempt_id,
                                    provider_completed = true,
                                    delivery = "abandoned",
                                    "completed provider response was not delivered"
                                );
                            }
                            finish_model_attempt_with_usage(
                                &mut attempt,
                                items_added,
                                Some(response_id),
                                token_usage,
                                ModelAttemptOutcome::Cancelled,
                            );
                            return;
                        }
                        drop(tx_event);
                        drop(api_stream);
                        let mut resolved_attempt = resolve_model_attempt(&mut attempt).await;
                        if let Some(usage) = &token_usage
                            && let (Some(input), Some(output)) =
                                (usage.known_input(), usage.known_output())
                        {
                            session_telemetry.sse_event_completed(
                                input,
                                output,
                                usage.known_cached_input(),
                                usage.known_reasoning_output(),
                                resolved_attempt
                                    .as_ref()
                                    .map(ModelAttemptGuard::tool_token_count)
                                    .unwrap_or_default(),
                                ttft_ms,
                            );
                        }
                        if let Some(attempt) = resolved_attempt.as_mut() {
                            attempt.provider_completed = true;
                            attempt.mark_fresh_response_id_established(&response_id);
                            attempt
                                .observe_terminal_usage(Some(&response_id), token_usage.as_ref());
                            attempt.finish(
                                ModelAttemptOutcome::Success,
                                token_usage.as_ref().and_then(TokenUsage::known_input),
                                token_usage
                                    .as_ref()
                                    .and_then(TokenUsage::known_cached_input),
                                &items_added,
                            );
                        }
                        // Completion is terminal. Dropping the upstream stream here
                        // releases it without recording a second, failed terminal state.
                        return;
                    }
                    Ok(_) => {
                        if tx_event.send_charged(event.map_err(|error| provider.map_api_error(error))).await.is_err() {
                            return;
                        }
                        if records_visible_output && let Some(clock) = attempt_clock.as_ref() {
                            clock.mark_first_visible_output();
                            first_visible_output_recorded = true;
                        }
                    }
                    Err(_) => {
                        delivery.publish(false);
                        let Err(err) = event.into_event() else { unreachable!() };
                        if let Some(attempt) = attempt.as_ref() {
                            attempt.clock().mark_completed();
                        }
                        let response_debug_context =
                            extract_response_debug_context_from_api_error(&err);
                        let upstream_request_id =
                            upstream_request_id.or(response_debug_context.request_id.as_deref());
                        if let Some(upstream_request_id) = upstream_request_id {
                            feedback_tags!(last_model_request_id = upstream_request_id);
                        }
                        let mapped = provider.map_api_error(err);
                        if let Some(attempt) = attempt.as_mut() {
                            attempt.finish_essential(
                                ModelAttemptOutcome::Failed,
                                mapped.terminal_response_id(),
                                mapped.terminal_usage().and_then(Option::as_ref),
                                &items_added,
                                false,
                            );
                        }
                        session_telemetry.see_event_completed_failed(&mapped);
                        let terminal_usage = mapped.terminal_usage().and_then(Clone::clone);
                        let response_id = mapped.terminal_response_id().map(str::to_owned);
                        finish_failed_model_attempt_with_usage(
                            &mut attempt,
                            items_added,
                            response_id,
                            terminal_usage,
                        );
                        if tx_event.send(Err(mapped)).await.is_err() {
                            return;
                        }
                        drop(tx_event);
                        return;
                    }
                }
            }
            if let Some(attempt) = attempt.as_ref() {
                attempt.clock().mark_completed();
            }
            let mapped = provider.map_api_error(ApiError::Stream(
                "stream closed before response.completed".to_string(),
            ));
            session_telemetry.see_event_completed_failed(&mapped);
            let _ = tx_event.send(Err(mapped)).await;
            drop(tx_event);
            finish_failed_model_attempt(&mut attempt, items_added);
        }
        .in_current_span(),
    );

    (
        ResponseStream {
            rx_event,
            attempt_identity,
            consumer_dropped: consumer_dropped_for_stream,
        },
        rx_last_response,
    )
}

/// Handles a 401 response by optionally refreshing ChatGPT tokens once.
///
/// When refresh succeeds, the caller should retry the API call; otherwise
/// the mapped `CodexErr` is returned to the caller.
#[derive(Clone, Copy, Debug)]
struct UnauthorizedRecoveryExecution {
    mode: &'static str,
    phase: &'static str,
}

#[derive(Clone, Copy, Debug, Default)]
struct PendingUnauthorizedRetry {
    retry_after_unauthorized: bool,
    recovery_mode: Option<&'static str>,
    recovery_phase: Option<&'static str>,
}

impl PendingUnauthorizedRetry {
    fn from_recovery(recovery: UnauthorizedRecoveryExecution) -> Self {
        Self {
            retry_after_unauthorized: true,
            recovery_mode: Some(recovery.mode),
            recovery_phase: Some(recovery.phase),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct AuthRequestTelemetryContext {
    auth_mode: Option<&'static str>,
    auth_header_attached: bool,
    auth_header_name: Option<&'static str>,
    agent_identity_telemetry: Option<AgentIdentityTelemetry>,
    retry_after_unauthorized: bool,
    recovery_mode: Option<&'static str>,
    recovery_phase: Option<&'static str>,
}

impl AuthRequestTelemetryContext {
    fn new(
        auth_mode: Option<AuthMode>,
        api_auth: &dyn AuthProvider,
        agent_identity_telemetry: Option<AgentIdentityTelemetry>,
        retry: PendingUnauthorizedRetry,
    ) -> Self {
        let auth_telemetry = auth_header_telemetry(api_auth);
        Self {
            auth_mode: auth_mode.map(|mode| match mode {
                AuthMode::ApiKey | AuthMode::BedrockApiKey => "ApiKey",
                AuthMode::Chatgpt
                | AuthMode::ChatgptAuthTokens
                | AuthMode::Headers
                | AuthMode::AgentIdentity
                | AuthMode::PersonalAccessToken => "Chatgpt",
            }),
            auth_header_attached: auth_telemetry.attached,
            auth_header_name: auth_telemetry.name,
            agent_identity_telemetry,
            retry_after_unauthorized: retry.retry_after_unauthorized,
            recovery_mode: retry.recovery_mode,
            recovery_phase: retry.recovery_phase,
        }
    }

    fn agent_identity_telemetry(&self) -> Option<&AgentIdentityTelemetry> {
        self.agent_identity_telemetry.as_ref()
    }
}

struct WebsocketConnectParams<'a> {
    session_telemetry: &'a SessionTelemetry,
    api_provider: codex_api::Provider,
    api_auth: SharedAuthProvider,
    responses_metadata: &'a CodexResponsesMetadata,
    auth_context: AuthRequestTelemetryContext,
    request_route_telemetry: RequestRouteTelemetry,
    routing_hint: Option<HeaderValue>,
}

fn build_routing_hint_header(
    auth: Option<&CodexAuth>,
    provider: &codex_api::Provider,
    model: &str,
    service_tier: Option<&str>,
) -> Option<HeaderValue> {
    if !auth.is_some_and(CodexAuth::uses_codex_backend)
        || provider
            .headers
            .get("x-codex-guardian")
            .is_some_and(|value| value == "reviewer")
    {
        return None;
    }
    let mut hint = format!("model={model}");
    if let Some(service_tier) = service_tier {
        hint.push_str(";tier=");
        hint.push_str(service_tier);
    }
    HeaderValue::from_str(&hint).ok()
}

/// True when a transport error is a missing/expired credential that the
/// one-shot token refresh in [`handle_unauthorized`] can fix.
///
/// Covers 401 and the backend's 400 spelling of the same condition. A bearer
/// that is absent or expired is sometimes reported as HTTP 400 "Missing bearer
/// or basic authentication in header", which `map_api_error` turns into a
/// non-retryable `InvalidRequest`; the turn then died with no refresh ever
/// attempted. The 400 case is gated on an explicit authorization signal — the
/// `x-openai-authorization-error` header, or that exact body sentence — so an
/// ordinary malformed request still fails fast instead of burning a refresh.
fn is_recoverable_authentication_failure(transport: &TransportError) -> bool {
    let TransportError::Http {
        status,
        headers,
        body,
        ..
    } = transport
    else {
        return false;
    };
    if *status == StatusCode::UNAUTHORIZED {
        return true;
    }
    if *status != StatusCode::BAD_REQUEST {
        return false;
    }
    let has_auth_error_header = headers
        .as_ref()
        .is_some_and(|headers| headers.contains_key("x-openai-authorization-error"));
    has_auth_error_header
        || body
            .as_deref()
            .is_some_and(|body| body.contains("Missing bearer or basic authentication in header"))
}

async fn handle_unauthorized(
    transport: TransportError,
    auth_recovery: &mut Option<UnauthorizedRecovery>,
    session_telemetry: &SessionTelemetry,
    provider: &SharedModelProvider,
) -> Result<UnauthorizedRecoveryExecution> {
    let debug = extract_response_debug_context(&transport);
    if let Some(recovery) = auth_recovery
        && recovery.has_next()
    {
        let mode = recovery.mode_name();
        let phase = recovery.step_name();
        return match recovery.next().await {
            Ok(step_result) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_succeeded",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    step_result.auth_state_changed(),
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_succeeded",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Ok(UnauthorizedRecoveryExecution { mode, phase })
            }
            Err(RefreshTokenError::Permanent(failed)) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_failed_permanent",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    /*auth_state_changed*/ None,
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_failed_permanent",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Err(CodexErr::RefreshTokenFailed(failed))
            }
            Err(RefreshTokenError::Transient(other)) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_failed_transient",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    /*auth_state_changed*/ None,
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_failed_transient",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Err(CodexErr::Io(other))
            }
        };
    }

    let (mode, phase, recovery_reason) = match auth_recovery.as_ref() {
        Some(recovery) => (
            recovery.mode_name(),
            recovery.step_name(),
            Some(recovery.unavailable_reason()),
        ),
        None => ("none", "none", Some("auth_manager_missing")),
    };
    session_telemetry.record_auth_recovery(
        mode,
        phase,
        "recovery_not_run",
        debug.request_id.as_deref(),
        debug.cf_ray.as_deref(),
        debug.auth_error.as_deref(),
        debug.auth_error_code.as_deref(),
        recovery_reason,
        /*auth_state_changed*/ None,
    );
    emit_feedback_auth_recovery_tags(
        mode,
        phase,
        "recovery_not_run",
        debug.request_id.as_deref(),
        debug.cf_ray.as_deref(),
        debug.auth_error.as_deref(),
        debug.auth_error_code.as_deref(),
    );

    Err(provider.map_api_error(ApiError::Transport(transport)))
}

fn api_error_http_status(error: &ApiError) -> Option<u16> {
    match error {
        ApiError::Transport(TransportError::Http { status, .. }) => Some(status.as_u16()),
        _ => None,
    }
}

struct ModelRequestTelemetryContext {
    timing: Option<Arc<TurnTimingState>>,
    startup: Option<Arc<crate::startup_timing::StartupTimingState>>,
    identity: ResponseAttemptIdentity,
    clock: ModelAttemptClock,
    turn_id: Option<String>,
    retry_index: u32,
    dispatched: AtomicBool,
    // Lower transport attempts are sequential and monotonically numbered.
    // This watermark outlives capped diagnostic IDs, so duplicate callbacks
    // cannot re-register an evicted identity and inflate the additive total.
    highest_started_transport_attempt: StdMutex<Option<u64>>,
    readiness: StdMutex<Option<crate::turn_timing::TurnTimingGuard>>,
}

struct ApiTelemetry {
    websocket_response: Option<(ResponseAttemptIdentity, ModelAttemptClock)>,
    first_response_event: AtomicBool,
    model_request: Option<ModelRequestTelemetryContext>,
    session_telemetry: SessionTelemetry,
    auth_context: AuthRequestTelemetryContext,
    request_route_telemetry: RequestRouteTelemetry,
    auth_env_telemetry: AuthEnvTelemetry,
    logical_request_id: String,
    last_attempt: AtomicU64,
}

impl ApiTelemetry {
    fn new(
        session_telemetry: SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> Self {
        Self {
            websocket_response: None,
            first_response_event: AtomicBool::new(false),
            session_telemetry,
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
            logical_request_id: new_sampling_request_id(),
            model_request: None,
            last_attempt: AtomicU64::new(0),
        }
    }
}

impl RequestTelemetry for ApiTelemetry {
    fn on_request_dispatched(&self) {
        self.on_request_dispatched_attempt(0);
    }

    fn on_request_dispatched_attempt(&self, transport_attempt: u64) {
        if let Some(context) = &self.model_request {
            {
                let mut highest = context
                    .highest_started_transport_attempt
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if highest.is_some_and(|previous| transport_attempt <= previous) {
                    return;
                }
                *highest = Some(transport_attempt);
            }
            drop(
                context
                    .readiness
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take(),
            );
            context.dispatched.store(true, Ordering::Release);
            if transport_attempt == 0 {
                context.clock.mark_dispatch_ready();
            } else {
                context.clock.mark_redispatch_ready(transport_attempt);
            }
            // Safe pre-dispatch transport retries share the enclosing API
            // request but are distinct physical entries. Keep their identity
            // stable for duplicate notifications without deriving it from payloads.
            let physical_attempt_id = if transport_attempt == 0 {
                context.identity.physical_attempt_id.clone()
            } else {
                format!(
                    "{}:http-{transport_attempt}",
                    context.identity.physical_attempt_id
                )
            };
            if let Some(timing) = &context.timing {
                timing.record_model_attempt_identity(
                    &context.identity.sampling_request_id,
                    &physical_attempt_id,
                );
                timing.mark_model_attempt_dispatched(
                    &context.identity.sampling_request_id,
                    &physical_attempt_id,
                    context.retry_index > 0 || transport_attempt > 0,
                );
            }
            tracing::info!(
                event.name = "codex.model_transport_identity",
                transport = "http",
                sampling_request_id = %context.identity.sampling_request_id,
                physical_attempt_id,
                transport_attempt,
                "model transport identity at HTTP entry"
            );
            if let Some(startup) = &context.startup {
                startup.freeze_at_first_transport_attempt();
            }
        }
    }

    fn on_request_cancelled(&self, attempt: u64, duration: Duration) {
        self.on_request(
            attempt,
            None,
            Some(&TransportError::Network("request cancelled".into())),
            duration,
        );
    }

    fn on_transport_phase(&self, attempt: u64, observation: TransportPhaseObservation) {
        self.last_attempt.store(attempt, Ordering::Relaxed);
        self.session_telemetry.record_transport_phase(
            &self.logical_request_id,
            attempt,
            observation.phase.as_str(),
            observation.duration,
            observation.wire_bytes,
            observation.provenance,
            observation.unavailable_reason,
        );
    }

    fn on_transport_phase_with_timing(
        &self,
        attempt: u64,
        observation: TransportPhaseObservation,
        timing: &codex_http_client::TransportTiming,
        headers: Option<&ApiHeaderMap>,
    ) {
        self.last_attempt.store(attempt, Ordering::Relaxed);
        let identity = self.model_request.as_ref().map(|request| &request.identity);
        let provider_request_id = headers
            .and_then(|headers| headers.get("x-request-id"))
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                !value.is_empty() && value.len() <= 256 && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                })
            });
        let mut response_start = codex_otel::ResponseStartTelemetry::from_transport_timing(
            timing,
            identity.map(|identity| identity.sampling_request_id.as_str()),
            identity.map(|identity| identity.physical_attempt_id.as_str()),
            provider_request_id,
        );
        // A provider-reported processing duration is not isolated inference or
        // queue time. Preserve only a bounded numeric value, never raw headers.
        response_start.provider_processing_us = headers
            .and_then(|headers| headers.get("openai-processing-ms"))
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 64)
            .and_then(|value| value.parse::<f64>().ok())
            .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
            .and_then(|duration| u64::try_from(duration.as_micros()).ok())
            .filter(|value| *value < u64::MAX);
        if let Some(request) = self.model_request.as_ref() {
            request.clock.record_response_start(&response_start);
        }
        // Reuse the existing phase event; retain its numeric receipt above.
        self.session_telemetry
            .record_transport_phase_with_response_start(
                &self.logical_request_id,
                attempt,
                observation.phase.as_str(),
                observation.duration,
                observation.wire_bytes,
                observation.provenance,
                observation.unavailable_reason,
                Some(&response_start),
            );
    }

    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        if let Some(context) = &self.model_request {
            context.dispatched.store(false, Ordering::Release);
            if let Some(timing) = &context.timing {
                timing.mark_model_request_attempt_ended();
            }
            if error.is_some() {
                *context
                    .readiness
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = context
                    .timing
                    .as_ref()
                    .map(|timing| timing.begin_local_phase(TurnLocalPhase::TransportReadiness));
            }
        }
        self.last_attempt.store(attempt, Ordering::Relaxed);
        let error_message = error.map(telemetry_transport_error_message);
        let status = status.map(|s| s.as_u16());
        let debug = error
            .map(extract_response_debug_context)
            .unwrap_or_default();
        self.session_telemetry.record_api_request(
            attempt,
            status,
            error_message.as_deref(),
            duration,
            self.auth_context.auth_header_attached,
            self.auth_context.auth_header_name,
            self.auth_context.retry_after_unauthorized,
            self.auth_context.recovery_mode,
            self.auth_context.recovery_phase,
            self.request_route_telemetry.endpoint,
            debug.request_id.as_deref(),
            debug.cf_ray.as_deref(),
            debug.auth_error.as_deref(),
            debug.auth_error_code.as_deref(),
            self.auth_context.agent_identity_telemetry(),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: self.request_route_telemetry.endpoint,
                auth_header_attached: self.auth_context.auth_header_attached,
                auth_header_name: self.auth_context.auth_header_name,
                auth_mode: self.auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(self.auth_context.retry_after_unauthorized),
                auth_recovery_mode: self.auth_context.recovery_mode,
                auth_recovery_phase: self.auth_context.recovery_phase,
                auth_connection_reused: None,
                auth_request_id: debug.request_id.as_deref(),
                auth_cf_ray: debug.cf_ray.as_deref(),
                auth_error: debug.auth_error.as_deref(),
                auth_error_code: debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(error.is_none()),
                auth_recovery_followup_status: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.auth_env_telemetry,
        );
    }
}

impl SseTelemetry for ApiTelemetry {
    fn on_sse_poll(
        &self,
        result: &std::result::Result<
            Option<std::result::Result<Event, EventStreamError<TransportError>>>,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    ) {
        self.session_telemetry.log_sse_event(result, duration);
    }

    fn on_sse_event(&self, kind: &str, duration: Duration, error: Option<&dyn std::fmt::Display>) {
        // Called for decoded body events, never header-derived initial events.
        if !self.first_response_event.swap(true, Ordering::AcqRel)
            && let Some(request) = self.model_request.as_ref()
        {
            request.clock.mark_first_provider_event();
            tracing::info!(event.name = "codex.model_transport_milestone",
                sampling_request_id = %request.identity.sampling_request_id,
                endpoint = "first_decoded_sse_event", "first provider body event");
        }
        self.session_telemetry
            .log_sse_event_result(kind, duration, error);
    }

    fn on_sse_phase(&self, phase: SsePollPhase, ordinal: u64, duration: Duration) {
        let identity = self.model_request.as_ref().map(|request| &request.identity);
        let response_start = (ordinal == 0).then(|| {
            codex_otel::ResponseStartTelemetry::from_transport_timing(
                &codex_http_client::current_transport_timing(),
                identity.map(|identity| identity.sampling_request_id.as_str()),
                identity.map(|identity| identity.physical_attempt_id.as_str()),
                None,
            )
        });
        if let (Some(request), Some(response_start)) = (&self.model_request, &response_start) {
            request.clock.record_response_start(response_start);
        }
        self.session_telemetry.record_sse_phase_with_response_start(
            &self.logical_request_id,
            self.last_attempt.load(Ordering::Relaxed),
            phase.as_str(),
            Some(ordinal),
            duration,
            response_start.as_ref(),
        );
    }

    fn on_sse_cleanup(&self, outcome: SseCleanupOutcome, duration: Duration) {
        self.session_telemetry.record_sse_phase(
            &self.logical_request_id,
            self.last_attempt.load(Ordering::Relaxed),
            outcome.as_str(),
            None,
            duration,
        );
    }
}

impl WebsocketTelemetry for ApiTelemetry {
    fn on_response_event(&self) {
        if let Some((_, clock)) = &self.websocket_response
            && !self.first_response_event.swap(true, Ordering::Relaxed)
        {
            clock.mark_first_provider_event();
        }
    }
    fn logical_transport_id(&self) -> Option<&str> {
        Some(&self.logical_request_id)
    }

    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool) {
        self.session_telemetry.record_model_transport_identity(
            &self.logical_request_id,
            self.websocket_response
                .as_ref()
                .map(|(identity, _)| identity.sampling_request_id.as_str()),
            self.websocket_response
                .as_ref()
                .map(|(identity, _)| identity.physical_attempt_id.as_str()),
            if self.websocket_response.is_some() {
                "model_request"
            } else {
                "connection_or_warmup"
            },
            Some(duration),
        );
        tracing::info!(
            event.name = "codex.websocket_transport",
            logical_request_id = %self.logical_request_id,
            sampling_request_id = self.websocket_response.as_ref().map(|(identity, _)| identity.sampling_request_id.as_str()),
            physical_attempt_id = self.websocket_response.as_ref().map(|(identity, _)| identity.physical_attempt_id.as_str()),
            connection_reused,
            failed = error.is_some(),
            "websocket request observation"
        );
        let error_message = error.map(telemetry_api_error_message);
        let status = error.and_then(api_error_http_status);
        let debug = error
            .map(extract_response_debug_context_from_api_error)
            .unwrap_or_default();
        self.session_telemetry.record_websocket_request(
            duration,
            error_message.as_deref(),
            connection_reused,
            self.auth_context.agent_identity_telemetry(),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: self.request_route_telemetry.endpoint,
                auth_header_attached: self.auth_context.auth_header_attached,
                auth_header_name: self.auth_context.auth_header_name,
                auth_mode: self.auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(self.auth_context.retry_after_unauthorized),
                auth_recovery_mode: self.auth_context.recovery_mode,
                auth_recovery_phase: self.auth_context.recovery_phase,
                auth_connection_reused: Some(connection_reused),
                auth_request_id: debug.request_id.as_deref(),
                auth_cf_ray: debug.cf_ray.as_deref(),
                auth_error: debug.auth_error.as_deref(),
                auth_error_code: debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(error.is_none()),
                auth_recovery_followup_status: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.auth_env_telemetry,
        );
    }

    fn on_ws_event(
        &self,
        result: &std::result::Result<Option<std::result::Result<Message, Error>>, ApiError>,
        duration: Duration,
    ) {
        if let Some((identity, _)) = &self.websocket_response {
            self.session_telemetry.record_model_transport_identity(
                &self.logical_request_id,
                Some(&identity.sampling_request_id),
                Some(&identity.physical_attempt_id),
                "model_response_poll",
                Some(duration),
            );
            tracing::info!(
                event.name = "codex.websocket_response_poll",
                connection_id = %self.logical_request_id,
                sampling_request_id = %identity.sampling_request_id,
                physical_attempt_id = %identity.physical_attempt_id,
                duration_us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
                "websocket response observation"
            );
        }
        self.session_telemetry
            .record_websocket_event_with_transport(
                result,
                duration,
                Some(&self.logical_request_id),
            );
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "client_transport_accounting_tests.rs"]
mod transport_accounting_tests;
