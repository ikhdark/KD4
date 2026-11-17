//! Request diagnostics and attribution; never authorizes provider-held history reuse.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use codex_api::ResponsesApiRequest;
use codex_conversation::context::PromptContextBreakdown;
use codex_conversation::context::PromptContextCategory;
use codex_conversation::context::PromptContextMeasurement;
use codex_conversation::context::PromptProvenanceSidecar;
use codex_conversation::stable_context::StableContextManifest;
use codex_otel::ModelAttemptOutcome;
use codex_otel::ModelAttemptProviderBaseline;
use codex_otel::ModelToolResultTelemetry;
use codex_otel::ModelToolSchemaTelemetry;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TurnTimingRequestTokenCategories;
use codex_protocol::protocol::TurnTimingTokenCategoryBasis;
use codex_turn_timing::TurnTimingState;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::warn;

use crate::client_common::Prompt;
use crate::client_common::ResponseAttemptIdentity;

/// Independent component serialization adds small JSON wrappers that are represented as envelope
/// overhead in the final request. This is a diagnostic bound, not a request-building requirement.
const MODEL_ATTEMPT_RECONCILIATION_TOLERANCE_BYTES: i64 = 256;
pub(super) const MATCHED_TASK_CACHE_COVERAGE_BASELINE_BPS: u64 = 8_500;

#[derive(Debug, Clone, Default)]
pub(super) struct ModelRequestMeasurements {
    pub(super) tool_token_count: i64,
    pub(super) wire_request_bytes: u64,
    pub(super) logical_request_bytes: u64,
    pub(super) base_instructions_bytes: u64,
    pub(super) tool_schemas_bytes: u64,
    pub(super) conversation_history_bytes: u64,
    pub(super) current_input_bytes: u64,
    pub(super) repository_context_bytes: u64,
    pub(super) skills_bytes: u64,
    pub(super) other_injected_context_bytes: u64,
    pub(super) envelope_overhead_bytes: u64,
    pub(super) reconciliation_residual_bytes: i64,
    pub(super) prompt_context_categories: Vec<PromptContextMeasurement>,
    pub(super) tool_schema_breakdown: Vec<ModelToolSchemaTelemetry>,
    pub(super) tool_result_attribution: Vec<ModelToolResultTelemetry>,
    pub(super) full_prompt_estimated_tokens: u64,
    reusable_prompt_estimated_tokens: Option<u64>,
    pub(super) fixed_prefix_reuse_eligible: bool,
    /// True once a previous request baseline existed for comparison, so an
    /// ineligible first request is not reported as changed categories.
    pub(super) prompt_context_baseline_compared: bool,
    pub(super) stable_context_manifest: StableContextManifest,
    pub(super) logical_context_tokens: i64,
    pub(super) active_component_bytes: u64,
    pub(super) local_constructed_bytes: u64,
    pub(super) local_reused_bytes: u64,
    pub(super) local_component_reuse_count: u64,
    pub(super) baseline_generation: Option<u64>,
    pub(super) provider_baseline: ModelAttemptProviderBaseline,
    pub(super) previous_response_id_present: bool,
    pub(super) local_projection_policy_active: bool,
    /// Digest of each input item in the request that was actually sent, in
    /// order, after projection and normalization.
    ///
    /// The whole-history digest changes on every request by design, so it
    /// cannot tell an append from a rewrite. Per-item digests can.
    input_item_digests: Vec<[u8; 32]>,
    pub(super) fixed_prefix_item_count: usize,
    /// Digests of the dispatched request settings; absent when the
    /// dispatched bytes were unavailable.
    request_setting_digests: Option<RequestSettingDigests>,
    /// Settings whose dispatched value differs from the preceding request's.
    pub(super) changed_request_settings: Vec<&'static str>,
    /// Whether the leading fixed input items differ from the preceding
    /// request's, as compared by exact per-item digest. `None` before any
    /// comparison, so a first request reports no cause rather than a false one.
    fixed_prefix_items_changed: Option<bool>,
    /// Filled by comparison against the preceding request; absent on the first.
    pub(super) history_divergence: Option<HistoryPrefixDivergence>,
    /// Representation selected at dispatch, not inferred from the manifest.
    pub(super) selected_representation: Option<SelectedInputRepresentation>,
}

/// How this request's input prefix relates to the preceding request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoryPrefixDivergence {
    pub(super) items_previous: u32,
    pub(super) prefix_items_reused: u32,
    pub(super) first_divergent_index: u32,
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
pub(super) struct PromptContextBaseline {
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
    Ok(codex_conversation::context::content_aware_serialized_tokens(&logical))
}

impl ModelRequestMeasurements {
    pub(super) async fn compare_after_predecessor(
        &mut self,
        predecessor: Option<oneshot::Receiver<Option<PromptContextBaseline>>>,
        initial_baseline: Option<PromptContextBaseline>,
        prompt_cache_key: Option<&str>,
    ) -> Option<PromptContextBaseline> {
        let mut baseline = match predecessor {
            Some(predecessor) => predecessor.await.unwrap_or(None),
            None => initial_baseline,
        };
        if self.prompt_context_categories.is_empty() {
            return None;
        }
        self.compare_and_remember_prompt_context(&mut baseline, prompt_cache_key);
        baseline
    }

    pub(super) fn tokens(&self, category: PromptContextCategory) -> u64 {
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

    pub(super) fn logical_prompt_tokens(&self) -> u64 {
        self.prompt_context_categories
            .iter()
            .fold(0_u64, |total, measurement| {
                total.saturating_add(measurement.estimated_tokens)
            })
    }

    pub(super) fn producer_selected_context_tokens(&self) -> u64 {
        PromptContextCategory::ALL
            .into_iter()
            .filter(|category| category.is_producer_selected_context())
            .fold(0_u64, |total, category| {
                total.saturating_add(self.tokens(category))
            })
    }

    pub(super) fn density_bps(&self, numerator: u64) -> u32 {
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

    pub(super) fn request_token_categories(&self) -> TurnTimingRequestTokenCategories {
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
            tool_result_attribution: context.tool_result_attribution,
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

    pub(super) fn record_stable_context(
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

    pub(super) fn expected_reusable_input_tokens(&self, input_tokens: Option<i64>) -> Option<i64> {
        let reusable = self.reusable_prompt_estimated_tokens?;
        let input = u64::try_from(input_tokens?).ok()?;
        i64::try_from(
            input
                .saturating_mul(reusable)
                .checked_div(self.full_prompt_estimated_tokens)?,
        )
        .ok()
    }

    pub(super) fn matched_task_reuse_eligible(&self) -> bool {
        self.fixed_prefix_reuse_eligible
            && self.prompt_context_categories.iter().any(|measurement| {
                measurement.category == PromptContextCategory::TaskInput.as_str()
                    && measurement.unchanged_from_previous_request
            })
    }
}

pub(super) fn cache_coverage_below_matched_task_baseline(
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
    Ok(measure_leaf_tool_schema_from_encoded(
        tool, index, namespace, serialized,
    ))
}

pub(super) fn selected_tool_schema_breakdown(
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

#[derive(Debug)]
pub(super) struct PostDispatchRequestMeasurements {
    pub(super) measurements: ModelRequestMeasurements,
    pub(super) stable_context_manifest: StableContextManifest,
}

/// Which of the four prepared input representations a request dispatched.
///
/// The other three are prepared and discarded, so only this one's aggregate
/// budget drops may be attributed to the request.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SelectedInputRepresentation {
    pub(super) stable_context_fallback: bool,
    pub(super) tool_history_fallback: bool,
}

impl SelectedInputRepresentation {
    pub(super) fn source_input(self, prompt: &Prompt) -> Arc<[ResponseItem]> {
        Arc::clone(match (self.stable_context_fallback, self.tool_history_fallback) {
            (false, false) => &prompt.input,
            (true, false) => &prompt.stable_context_fallback_input,
            (false, true) => &prompt.tool_history_fallback_input,
            (true, true) => &prompt.stable_context_tool_history_fallback_input,
        })
    }

    pub(super) fn as_str(self) -> &'static str {
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
pub(super) fn measure_responses_request_after_dispatch(
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
                && let Some((timing, identity)) = payload_timing.as_ref()
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
            if let (Ok(measured), Some((timing, identity))) = (&measurements, payload_timing.as_ref()) {
                // Persist only the missing request/representation links. Reuse
                // the existing per-inclusion hashes; do not serialize, hash or
                // tokenize the old tool results a second time.
                let outputs = measured.tool_result_attribution.iter().take(1024).map(|output| {
                    serde_json::json!({"call_id_sha256":output.call_id_sha256,
                        "representation_sha256":output.representation_sha256,
                        "serialized_bytes":output.serialized_bytes})
                }).collect();
                timing.record_model_output_exposures(&identity.sampling_request_id,
                    &identity.physical_attempt_id, outputs, measured.tool_result_attribution.len() <= 1024);
            }
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
                        .and(encoded_request.as_deref())
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
                warn!(target: "codex_core::client", error = %err, "model request diagnostics were unavailable after dispatch");
                PostDispatchRequestMeasurements {
                    measurements: ModelRequestMeasurements::default(),
                    stable_context_manifest,
                }
            }
            Err(err) => {
                warn!(target: "codex_core::client", error = %err, "model request diagnostics task failed after dispatch");
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
