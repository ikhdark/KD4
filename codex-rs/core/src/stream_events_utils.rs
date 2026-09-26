use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::TurnItem;
use tokio_util::sync::CancellationToken;

use crate::parse_turn_item;
use crate::session::session::Session;
use crate::session::turn::reconcile_turn_progress_event;
use crate::session::turn_context::TurnContext;
use crate::tools::parallel::ToolCallCompletion;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallBuildError;
use crate::tools::router::ToolRouter;
use crate::tools::tool_dispatch_trace::ToolDispatchTiming;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_stream_parser::strip_proposed_plan_blocks;
use futures::Future;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::debug;
use tracing::instrument;
use tracing::warn;

const GENERATED_IMAGE_ARTIFACTS_DIR: &str = "generated_images";

fn tool_call_arguments_length(call: &ToolCall) -> usize {
    call.payload.log_payload().len()
}

/// Returns the host-owned default artifact path for a generated image.
pub fn image_generation_artifact_path(
    codex_home: &AbsolutePathBuf,
    session_id: &str,
    call_id: &str,
) -> AbsolutePathBuf {
    let sanitize = |value: &str| {
        let mut sanitized: String = value
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        if sanitized.is_empty() {
            sanitized = "generated_image".to_string();
        }
        sanitized
    };

    codex_home
        .join(GENERATED_IMAGE_ARTIFACTS_DIR)
        .join(sanitize(session_id))
        .join(format!("{}.png", sanitize(call_id)))
}

fn strip_hidden_assistant_markup(text: String, plan_mode: bool) -> String {
    if plan_mode {
        strip_proposed_plan_blocks(&text)
    } else {
        text
    }
}

pub(crate) fn raw_assistant_output_text_from_item(item: &ResponseItem) -> Option<String> {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let combined = content
            .iter()
            .filter_map(|ci| match ci {
                codex_protocol::models::ContentItem::OutputText { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        return Some(combined);
    }
    None
}

pub(crate) async fn record_completed_response_item_with_finalized_facts(
    sess: &Session,
    turn_context: &TurnContext,
    item: &ResponseItem,
    finalized_facts: Option<&FinalizedTurnItemFacts>,
) -> std::io::Result<()> {
    sess.record_conversation_items(turn_context, std::slice::from_ref(item))
        .await?;
    let defers_mailbox_delivery = finalized_facts.map_or_else(
        || {
            completed_item_defers_mailbox_delivery_to_next_turn(
                item,
                turn_context.collaboration_mode.mode == ModeKind::Plan,
            )
        },
        |facts| facts.defers_mailbox_delivery_to_next_turn,
    );
    if defers_mailbox_delivery {
        sess.input_queue
            .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &turn_context.sub_id)
            .await;
    }
    Ok(())
}

/// Handle a completed output item from the model stream, recording it and
/// queuing any tool execution futures. This records items immediately so
/// history and rollout stay in sync even if the turn is later cancelled.
pub(crate) type InFlightFuture<'f> =
    Pin<Box<dyn Future<Output = Result<ToolCallCompletion>> + Send + 'f>>;

pub(crate) struct InFlightToolCall {
    pub(crate) call: ToolCall,
    pub(crate) call_id: String,
    pub(crate) execution_id: ToolExecutionId,
    pub(crate) timing: Arc<ToolDispatchTiming>,
    future: InFlightFuture<'static>,
}

pub(crate) struct InFlightToolResult {
    pub(crate) call: ToolCall,
    pub(crate) call_id: String,
    pub(crate) execution_id: ToolExecutionId,
    pub(crate) timing: Arc<ToolDispatchTiming>,
    pub(crate) result: Result<ToolCallCompletion>,
}

impl InFlightToolCall {
    #[cfg(test)]
    pub(crate) fn from_test_future(
        call_id: impl Into<String>,
        future: Pin<Box<dyn Future<Output = Result<ResponseInputItem>> + Send + 'static>>,
    ) -> Self {
        let call_id = call_id.into();
        let timing = Arc::new(ToolDispatchTiming::new(tokio::time::Instant::now(), false));
        let execution_id = timing.execution_id().clone();
        Self {
            call: ToolCall {
                tool_name: codex_tools::ToolName::plain("test_tool"),
                call_id: call_id.clone(),
                payload: crate::tools::context::ToolPayload::Function {
                    arguments: "{}".to_string(),
                },
            },
            call_id,
            execution_id,
            timing,
            future: Box::pin(async move { future.await.map(ToolCallCompletion::nonterminal) }),
        }
    }

    /// Retires a deferred call that must never reach its handler.
    ///
    /// A deferred tool is admitted only after the provider response tail closes
    /// successfully. When the tail ends in a terminal stream error or a cancellation, the
    /// call is retired with a model-visible failure output instead of being executed, so the
    /// history still pairs every call with an output while no side effect is performed.
    pub(crate) fn into_unexecuted_result(self, message: String) -> InFlightToolResult {
        let response = crate::tools::parallel::ToolCallRuntime::failure_response_for_message(
            &self.call, message,
        );
        self.timing.mark_relay_enqueue();
        if let Some(turn_timing_state) = self.timing.turn_timing_state() {
            reconcile_turn_progress_event(&turn_timing_state, 1, "relay enqueue");
        }
        InFlightToolResult {
            call: self.call,
            call_id: self.call_id,
            execution_id: self.execution_id,
            timing: self.timing,
            result: Ok(ToolCallCompletion::nonterminal(response)),
        }
    }

    pub(crate) async fn into_future(self) -> InFlightToolResult {
        let result = self.future.await;
        self.timing.mark_relay_enqueue();
        if let Some(turn_timing_state) = self.timing.turn_timing_state() {
            reconcile_turn_progress_event(&turn_timing_state, 1, "relay enqueue");
        }
        InFlightToolResult {
            call: self.call,
            call_id: self.call_id,
            execution_id: self.execution_id,
            timing: self.timing,
            result,
        }
    }
}

#[derive(Default)]
pub(crate) struct OutputItemResult {
    pub last_agent_message: Option<String>,
    pub needs_follow_up: bool,
    pub tool_future: Option<InFlightToolCall>,
    pub eager_read_eligible: bool,
}

pub(crate) struct HandleOutputCtx {
    pub sess: Arc<Session>,
    pub turn_context: Arc<TurnContext>,
    pub turn_store: Arc<ExtensionData>,
    pub tool_runtime: ToolCallRuntime,
    pub cancellation_token: CancellationToken,
    pub response_item_recorder: OrderedResponseItemRecorder,
}

type ResponseItemPersistenceBarrier = Shared<BoxFuture<'static, std::result::Result<(), String>>>;

#[derive(Default)]
struct OrderedResponseItemRecorderState {
    required_tail: Option<ResponseItemPersistenceBarrier>,
    auxiliary_tail: Option<ResponseItemPersistenceBarrier>,
    accepted_tool_calls: BTreeMap<String, ToolCall>,
}

#[derive(Clone, Default)]
pub(crate) struct OrderedResponseItemRecorder {
    state: Arc<Mutex<OrderedResponseItemRecorderState>>,
}

impl OrderedResponseItemRecorder {
    async fn accepted_tool_call_replay(&self, call: &ToolCall) -> Option<bool> {
        self.state
            .lock()
            .await
            .accepted_tool_calls
            .get(&call.call_id)
            .map(|accepted| accepted == call)
    }

    async fn record_accepted_tool_call(&self, call: ToolCall) {
        self.state
            .lock()
            .await
            .accepted_tool_calls
            .insert(call.call_id.clone(), call);
    }

    pub(crate) async fn enqueue(
        &self,
        sess: Arc<Session>,
        turn_context: Arc<TurnContext>,
        item: ResponseItem,
        following_items: Vec<ResponseItem>,
        finalized_facts: Option<FinalizedTurnItemFacts>,
    ) -> ResponseItemPersistenceBarrier {
        let mut state = self.state.lock().await;
        let preceding_required = state.required_tail.clone();
        let preceding_auxiliary = state.auxiliary_tail.clone();
        let primary = item.clone();
        let required_sess = Arc::clone(&sess);
        let required_turn_context = Arc::clone(&turn_context);
        let required_barrier = async move {
            if let Some(preceding) = preceding_required {
                preceding.await?;
            }
            let mut items = Vec::with_capacity(1 + following_items.len());
            items.push(item);
            items.extend(following_items);
            required_sess
                .record_conversation_items_ordered(&required_turn_context, &items)
                .await
                .map_err(|error| error.to_string())
        }
        .boxed()
        .shared();
        let required_for_auxiliary = required_barrier.clone();
        let auxiliary_barrier = async move {
            if let Some(preceding) = preceding_auxiliary {
                preceding.await?;
            }
            required_for_auxiliary.await?;
            let defers_mailbox_delivery = finalized_facts.as_ref().map_or_else(
                || {
                    completed_item_defers_mailbox_delivery_to_next_turn(
                        &primary,
                        turn_context.collaboration_mode.mode == ModeKind::Plan,
                    )
                },
                |facts| facts.defers_mailbox_delivery_to_next_turn,
            );
            if defers_mailbox_delivery {
                sess.input_queue
                    .defer_mailbox_delivery_to_next_turn(&sess.active_turn, &turn_context.sub_id)
                    .await;
            }
            Ok(())
        }
        .boxed()
        .shared();
        state.required_tail = Some(required_barrier.clone());
        state.auxiliary_tail = Some(auxiliary_barrier.clone());
        drop(tokio::spawn(required_barrier.clone()));
        drop(tokio::spawn(auxiliary_barrier));
        required_barrier
    }

    pub(crate) async fn flush(&self) -> codex_protocol::error::Result<()> {
        let tail = self.state.lock().await.auxiliary_tail.clone();
        if let Some(tail) = tail {
            tail.await.map_err(CodexErr::Fatal)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn block_required_persistence_for_test(
        &self,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        let barrier = async move {
            let _ = release.await;
            Ok(())
        }
        .boxed()
        .shared();
        self.state.lock().await.required_tail = Some(barrier);
    }

    #[cfg(test)]
    pub(crate) async fn block_auxiliary_persistence_for_test(
        &self,
        release: tokio::sync::oneshot::Receiver<()>,
    ) {
        let barrier = async move {
            let _ = release.await;
            Ok(())
        }
        .boxed()
        .shared();
        self.state.lock().await.auxiliary_tail = Some(barrier);
    }
}

pub(crate) async fn apply_turn_item_contributors(
    sess: &Session,
    turn_store: &ExtensionData,
    item: &mut TurnItem,
) {
    let contributors = sess.services.extensions.turn_item_contributors().to_vec();
    for contributor in contributors {
        if let Err(err) = contributor
            .contribute(&sess.services.thread_extension_data, turn_store, item)
            .await
        {
            warn!("turn item contributor failed: {err}");
        }
    }
}

pub(crate) enum TurnItemContributorPolicy<'a> {
    Skip,
    Run(&'a ExtensionData),
}

pub(crate) struct FinalizedTurnItem {
    pub(crate) turn_item: TurnItem,
    pub(crate) facts: FinalizedTurnItemFacts,
}

#[derive(Clone, Default)]
pub(crate) struct FinalizedTurnItemFacts {
    pub(crate) last_agent_message: Option<String>,
    pub(crate) defers_mailbox_delivery_to_next_turn: bool,
}

pub(crate) async fn finalize_non_tool_response_item(
    sess: &Session,
    contributor_policy: TurnItemContributorPolicy<'_>,
    item: &ResponseItem,
    plan_mode: bool,
) -> Option<FinalizedTurnItem> {
    let turn_item =
        handle_non_tool_response_item(sess, contributor_policy, item, plan_mode).await?;
    let (last_agent_message, defers_mailbox_delivery_to_next_turn) = match &turn_item {
        TurnItem::AgentMessage(agent_message) => {
            let combined = agent_message
                .content
                .iter()
                .map(|entry| match entry {
                    codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
                })
                .collect::<String>();
            let last_agent_message = if combined.trim().is_empty() {
                None
            } else {
                Some(combined)
            };
            let defers_mailbox_delivery_to_next_turn =
                !matches!(agent_message.phase, Some(MessagePhase::Commentary))
                    && last_agent_message.is_some();
            (last_agent_message, defers_mailbox_delivery_to_next_turn)
        }
        _ => (None, false),
    };
    Some(FinalizedTurnItem {
        turn_item,
        facts: FinalizedTurnItemFacts {
            last_agent_message,
            defers_mailbox_delivery_to_next_turn,
        },
    })
}

#[instrument(level = "trace", skip_all)]
pub(crate) async fn handle_output_item_done(
    ctx: &mut HandleOutputCtx,
    item: ResponseItem,
    previously_active_item: Option<TurnItem>,
    earlier_tool_calls_eligible: &mut bool,
) -> Result<OutputItemResult> {
    let item_accepted_at = Instant::now();
    let mut output = OutputItemResult::default();
    let plan_mode = ctx.turn_context.collaboration_mode.mode == ModeKind::Plan;

    match ToolRouter::build_tool_call(&item) {
        // The model emitted a tool call; admit it, persist it, and queue the tool execution.
        Ok(Some(call)) => {
            ctx.tool_runtime
                .ensure_execution_allowed()
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            ctx.turn_context
                .turn_timing_state
                .record_model_emitted_tool_call();

            if ctx
                .response_item_recorder
                .accepted_tool_call_replay(&call)
                .await
                == Some(true)
            {
                tracing::debug!(
                    call_id = %call.call_id,
                    tool_name = %call.tool_name,
                    "ignored exact replay of an accepted tool call"
                );
                return Ok(output);
            }

            let call_id = call.call_id.clone();
            if ctx
                .turn_context
                .turn_timing_state
                .reject_duplicate_tool_call_id_if_accepted(
                    &call_id,
                    TurnTimingToolCallSource::Direct,
                )
            {
                return Err(CodexErr::Fatal(format!(
                    "refusing tool call `{call_id}` because the same call ID was already accepted in this model generation"
                )));
            }
            let mut next_earlier_tool_calls_eligible = *earlier_tool_calls_eligible;
            output.eager_read_eligible = ctx
                .tool_runtime
                .take_eager_read_eligibility(&call, &mut next_earlier_tool_calls_eligible);
            let timing = ctx
                .tool_runtime
                .create_tool_dispatch_timing(item_accepted_at, output.eager_read_eligible);
            let execution_id = timing.execution_id().clone();
            let accepted = ctx.turn_context.tool_call_acceptance.try_accept(|| {
                ctx.turn_context
                    .turn_timing_state
                    .try_record_accepted_tool_call(
                        &call_id,
                        &execution_id,
                        TurnTimingToolCallSource::Direct,
                        None,
                    )
            });
            if !accepted {
                return Err(CodexErr::Fatal(format!(
                    "refusing tool call `{call_id}` because acceptance was sealed or the same call ID was already accepted in this model generation"
                )));
            }
            *earlier_tool_calls_eligible = next_earlier_tool_calls_eligible;
            ctx.response_item_recorder
                .record_accepted_tool_call(call.clone())
                .await;
            ctx.sess
                .input_queue
                .accept_mailbox_delivery_for_current_turn(
                    &ctx.sess.active_turn,
                    &ctx.turn_context.sub_id,
                )
                .await;
            tracing::info!(
                thread_id = %ctx.sess.thread_id,
                tool_name = %call.tool_name,
                call_id = %call.call_id,
                arguments_length = tool_call_arguments_length(&call),
                "ToolCall"
            );

            let persistence_barrier = ctx
                .response_item_recorder
                .enqueue(
                    Arc::clone(&ctx.sess),
                    Arc::clone(&ctx.turn_context),
                    item,
                    Vec::new(),
                    None,
                )
                .await;

            let cancellation_token = ctx.cancellation_token.child_token();
            let tool_runtime = ctx.tool_runtime.clone();
            let accepted_call = call.clone();
            let future_timing = Arc::clone(&timing);
            // Keep deferred dispatch genuinely lazy. Eager callers poll this
            // future after its ordered persistence barrier; deferred callers
            // do not construct the runtime dispatch task until the response
            // tail has completed.
            let completion = async move {
                persistence_barrier.await.map_err(CodexErr::Fatal)?;
                tool_runtime
                    .handle_model_tool_call_with_trace(call, cancellation_token, future_timing)
                    .await
            };
            let tool_future: InFlightFuture<'static> = Box::pin(completion);

            output.needs_follow_up = true;
            output.tool_future = Some(InFlightToolCall {
                call: accepted_call,
                call_id,
                execution_id,
                timing,
                future: tool_future,
            });
        }
        // No tool call: convert messages/reasoning into turn items and mark them as complete.
        Ok(None) => {
            let finalized_turn_item = finalize_non_tool_response_item(
                ctx.sess.as_ref(),
                TurnItemContributorPolicy::Run(ctx.turn_store.as_ref()),
                &item,
                plan_mode,
            )
            .await;
            let finalized_facts = finalized_turn_item
                .as_ref()
                .map(|finalized| finalized.facts.clone());
            if let Some(finalized_turn_item) = finalized_turn_item {
                if previously_active_item.is_none() {
                    ctx.sess
                        .emit_turn_item_started(&ctx.turn_context, &finalized_turn_item.turn_item)
                        .await;
                    // S1: this item never streamed live, either because a turn-item
                    // contributor may rewrite assistant text or because the provider sent no
                    // `OutputItemAdded`. Replay the finalized text as a delta so a client
                    // still observes started -> delta -> completed, and so the deltas it sees
                    // agree with the contributed item rather than the pre-contribution text.
                    emit_finalized_assistant_text_replay(
                        ctx.sess.as_ref(),
                        &ctx.turn_context,
                        &finalized_turn_item.turn_item,
                    )
                    .await;
                }

                ctx.sess
                    .emit_turn_item_completed(&ctx.turn_context, finalized_turn_item.turn_item)
                    .await;
            }
            drop(
                ctx.response_item_recorder
                    .enqueue(
                        Arc::clone(&ctx.sess),
                        Arc::clone(&ctx.turn_context),
                        item,
                        Vec::new(),
                        finalized_facts.clone(),
                    )
                    .await,
            );

            output.last_agent_message = finalized_facts.and_then(|facts| facts.last_agent_message);
        }
        // Preserve the tool-search response shape and call ID when argument parsing fails.
        Err(ToolCallBuildError::ToolSearchArguments { call_id, message }) => {
            let failure_detail = ToolCallRuntime::search_failure_detail_for_call_id(
                &call_id,
                &crate::FunctionCallError::RespondToModel(message),
            );
            let response = ResponseInputItem::ToolSearchOutput {
                call_id,
                status: "incomplete".to_string(),
                execution: "client".to_string(),
                tools: Vec::new(),
                omitted_result_count: None,
            };
            // A malformed tool item is deferred and closes the eager prefix for
            // every later call in this model response.
            *earlier_tool_calls_eligible = false;
            let following_items = response_input_to_response_item(&response)
                .into_iter()
                .chain(std::iter::once(failure_detail))
                .collect();
            drop(
                ctx.response_item_recorder
                    .enqueue(
                        Arc::clone(&ctx.sess),
                        Arc::clone(&ctx.turn_context),
                        item,
                        following_items,
                        None,
                    )
                    .await,
            );

            output.needs_follow_up = true;
        }
    }

    Ok(output)
}

pub(crate) async fn handle_non_tool_response_item(
    sess: &Session,
    contributor_policy: TurnItemContributorPolicy<'_>,
    item: &ResponseItem,
    plan_mode: bool,
) -> Option<TurnItem> {
    let item_type = match item {
        ResponseItem::AdditionalTools { .. } => "additional_tools",
        ResponseItem::Message { .. } => "message",
        ResponseItem::AgentMessage { .. } => "agent_message",
        ResponseItem::Reasoning { .. } => "reasoning",
        ResponseItem::LocalShellCall { .. } => "local_shell_call",
        ResponseItem::FunctionCall { .. } => "function_call",
        ResponseItem::ToolSearchCall { .. } => "tool_search_call",
        ResponseItem::FunctionCallOutput { .. } => "function_call_output",
        ResponseItem::CustomToolCall { .. } => "custom_tool_call",
        ResponseItem::CustomToolCallOutput { .. } => "custom_tool_call_output",
        ResponseItem::ToolSearchOutput { .. } => "tool_search_output",
        ResponseItem::WebSearchCall { .. } => "web_search_call",
        ResponseItem::ImageGenerationCall { .. } => "image_generation_call",
        ResponseItem::Compaction { .. } => "compaction",
        ResponseItem::CompactionTrigger { .. } => "compaction_trigger",
        ResponseItem::ContextCompaction { .. } => "context_compaction",
        ResponseItem::Other => "other",
    };
    debug!(
        item_type,
        item_id = item.id().map(ResponseItemId::as_str),
        "Output item"
    );

    match item {
        ResponseItem::Message { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. } => {
            let mut turn_item = parse_turn_item(item)?;
            finalize_turn_item(sess, contributor_policy, &mut turn_item, plan_mode).await;
            Some(turn_item)
        }
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. } => {
            debug!("unexpected tool output from stream");
            None
        }
        _ => None,
    }
}

/// Emits the finalized assistant text of an item that was never streamed live.
///
/// Clients treat assistant text as an incremental stream. An item that produced no deltas
/// would otherwise appear only at completion, so replay its finalized text as one delta
/// between the started and completed events. Items with no visible text emit nothing.
async fn emit_finalized_assistant_text_replay(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: &TurnItem,
) {
    let TurnItem::AgentMessage(agent_message) = turn_item else {
        return;
    };
    let text = agent_message
        .content
        .iter()
        .map(|content| match content {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect::<String>();
    if text.is_empty() {
        return;
    }
    sess.send_event(
        turn_context,
        codex_protocol::protocol::EventMsg::AgentMessageContentDelta(
            codex_protocol::protocol::AgentMessageContentDeltaEvent {
                thread_id: sess.thread_id.to_string(),
                turn_id: turn_context.sub_id.clone(),
                item_id: turn_item.id(),
                delta: text,
            },
        ),
    )
    .await;
}

pub(crate) async fn finalize_turn_item(
    sess: &Session,
    contributor_policy: TurnItemContributorPolicy<'_>,
    turn_item: &mut TurnItem,
    plan_mode: bool,
) {
    if let TurnItemContributorPolicy::Run(turn_store) = contributor_policy {
        apply_turn_item_contributors(sess, turn_store, turn_item).await;
    }
    if let TurnItem::AgentMessage(agent_message) = &mut *turn_item {
        let combined = agent_message
            .content
            .iter()
            .map(|entry| match entry {
                codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
            })
            .collect::<String>();
        let stripped = strip_hidden_assistant_markup(combined, plan_mode);
        agent_message.content =
            vec![codex_protocol::items::AgentMessageContent::Text { text: stripped }];
    }
}

pub(crate) fn last_assistant_message_from_item(
    item: &ResponseItem,
    plan_mode: bool,
) -> Option<String> {
    if let Some(combined) = raw_assistant_output_text_from_item(item) {
        if combined.is_empty() {
            return None;
        }
        let stripped = strip_hidden_assistant_markup(combined, plan_mode);
        if stripped.trim().is_empty() {
            return None;
        }
        return Some(stripped);
    }
    None
}

fn completed_item_defers_mailbox_delivery_to_next_turn(
    item: &ResponseItem,
    plan_mode: bool,
) -> bool {
    match item {
        ResponseItem::Message { role, phase, .. } => {
            if role != "assistant" || matches!(phase, Some(MessagePhase::Commentary)) {
                return false;
            }
            // Treat `None` like final-answer text so untagged providers default
            // to the safer "defer mailbox mail" behavior.
            last_assistant_message_from_item(item, plan_mode).is_some()
        }
        _ => false,
    }
}

pub(crate) fn response_input_to_response_item(input: &ResponseInputItem) -> Option<ResponseItem> {
    match input {
        ResponseInputItem::FunctionCallOutput { call_id, output } => {
            Some(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: call_id.clone(),
                output: output.clone(),
                internal_chat_message_metadata_passthrough: None,
            })
        }
        ResponseInputItem::CustomToolCallOutput {
            call_id,
            name,
            output,
        } => Some(ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: call_id.clone(),
            name: name.clone(),
            output: output.clone(),
            internal_chat_message_metadata_passthrough: None,
        }),
        ResponseInputItem::McpToolCallOutput { call_id, output } => {
            let output = output.as_function_call_output_payload();
            Some(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: call_id.clone(),
                output,
                internal_chat_message_metadata_passthrough: None,
            })
        }
        ResponseInputItem::ToolSearchOutput {
            call_id,
            status,
            execution,
            tools,
            omitted_result_count,
        } => Some(ResponseItem::ToolSearchOutput {
            id: None,
            call_id: Some(call_id.clone()),
            status: status.clone(),
            execution: execution.clone(),
            tools: tools.clone(),
            omitted_result_count: *omitted_result_count,
            internal_chat_message_metadata_passthrough: None,
        }),
        _ => None,
    }
}

#[cfg(test)]
#[path = "stream_events_utils_tests.rs"]
mod tests;
