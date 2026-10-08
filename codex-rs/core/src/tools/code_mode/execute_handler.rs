use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutionTiming;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::sync::Arc;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::emit_failed_code_mode_cell_item;
use super::handle_runtime_response;
use super::is_exec_tool_name;
use super::wait_handler::OwnerHeldCodeModeExit;
use super::wait_handler::attach_drained_wait_evidence;
use super::wait_handler::hold_until_state_change;
use super::wait_handler::input_activity_response;
use super::wait_handler::record_internally_drained_waits;
use super::wait_handler::terminate_interrupted_cell;

pub struct CodeModeExecuteHandler {
    spec: ToolSpec,
    enabled_tools: Arc<[codex_code_mode::ToolDefinition]>,
    timeout_catalog: std::sync::Mutex<Option<TimeoutCatalog>>,
}

struct TimeoutCatalog {
    overrides: Vec<Option<u64>>,
    tools: Arc<[codex_code_mode::ToolDefinition]>,
}

fn direct_delivery_text(response: &codex_code_mode::RuntimeResponse, limit: usize) -> Result<String, serde_json::Value> {
    use codex_code_mode::FunctionCallOutputContentItem;
    let codex_code_mode::RuntimeResponse::Result {
        content_items,
        error_text: None,
        output_loss: None,
        ..
    } = response
    else {
        let category = match response {
            codex_code_mode::RuntimeResponse::Result { error_text: Some(_), .. } => "runtime_error",
            codex_code_mode::RuntimeResponse::Result { output_loss: Some(_), .. } => "output_loss",
            codex_code_mode::RuntimeResponse::Terminated { .. } => "terminated",
            _ => "cell_running",
        };
        return Err(serde_json::json!({"category":category}));
    };
    let mut parts = Vec::with_capacity(content_items.len());
    for item in content_items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => parts.push(text.clone()),
            FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                // Use the already-produced media; never fetch or re-encode it.
                // Reject Markdown delimiters/control characters rather than
                // allowing a media URL to introduce additional response text.
                let supported = image_url.split_once(':').is_some_and(|(scheme, rest)| {
                    ((scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
                        && rest.starts_with("//"))
                        || (scheme.eq_ignore_ascii_case("data")
                            && rest.get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/")))
                }) || std::path::Path::new(image_url).is_absolute();
                if image_url.chars().any(|ch| ch.is_control() || matches!(ch, '<' | '>'))
                    || !supported
                {
                    return Err(serde_json::json!({"category":"unsupported_media"}));
                }
                parts.push(format!("![image](<{image_url}>)"));
            }
        }
    }
    let message = parts.join("\n");
    if message.trim().is_empty() { return Err(serde_json::json!({"category":"empty_output"})); }
    if codex_utils_output_truncation::model_token_count(&message) > limit {
        return Err(serde_json::json!({"category":"output_budget", "limit":limit}));
    }
    Ok(message)
}

pub(super) fn schema_validated_delivery(
    response: &codex_code_mode::RuntimeResponse,
    limit: usize,
    schema: Option<&serde_json::Value>,
) -> Result<String, serde_json::Value> {
    let text = direct_delivery_text(response, limit)?;
    if let Some(schema) = schema {
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|error|
            serde_json::json!({"category":"invalid_json", "line":error.line(), "column":error.column()}))?;
        let validator = jsonschema::validator_for(schema).map_err(|_|
            serde_json::json!({"category":"invalid_schema"}))?;
        if let Err(error) = validator.validate(&value) {
            let pointer = error.instance_path().as_str().chars().take(512).collect::<String>();
            return Err(serde_json::json!({"category":"schema_mismatch", "instance_pointer":pointer}));
        }
    }
    Ok(text)
}

pub(super) fn attach_delivery_decision(output: &mut FunctionToolOutput, decision: Result<Option<String>, serde_json::Value>) {
    let refusal = match decision {
        Ok(None) => return,
        Err(refusal) => refusal,
        Ok(Some(message)) => {
            if output.success != Some(true) {
                serde_json::json!({"category":"failed_output"})
            } else if output.essential_inline.contains_key(super::VISIBLE_OUTPUT_TRUNCATED_KEY) {
                serde_json::json!({"category":"output_truncated"})
            } else if let Some(signal) = output.sampling_request_signal.as_mut().and_then(serde_json::Value::as_object_mut) {
                signal.insert("explicit_completion_message".into(), message.into());
                return;
            } else { serde_json::json!({"category":"completion_signal_unavailable"}) }
        }
    };
    output.essential_inline.insert("delivery_refused".into(), refusal.clone());
    let item = codex_protocol::models::FunctionCallOutputContentItem::InputText {
        text: serde_json::json!({"delivery_refused":refusal}).to_string(),
    };
    output.body.push(item.clone());
    if let Some(canonical) = &mut output.canonical_body { canonical.push(item); }
}

/// Headroom added to the longest wait a nested tool can be asked to perform, so
/// dispatch, hooks, transport, and lock queueing cannot turn a full-length
/// cooperative yield into a guaranteed hard-timeout failure.
/// Allow a tool-owned long poll or transport timeout to finish before its host deadline.
fn extended_nested_tool_timeout_ms(tool_timeout_ms: u64) -> u64 {
    codex_code_mode::tool_owned_timeout_with_grace(tool_timeout_ms)
}

fn nested_tool_timeout_override(
    tool: &ToolName,
    mcp_timeouts: &HashMap<ToolName, u64>,
    terminal_poll_ms: u64,
) -> Option<u64> {
    mcp_timeouts.get(tool).copied().or_else(|| {
        if tool == &ToolName::plain("exec_command") {
            Some(extended_nested_tool_timeout_ms(
                crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS,
            ))
        } else if tool == &ToolName::plain("write_stdin") {
            Some(extended_nested_tool_timeout_ms(terminal_poll_ms))
        } else if tool == &ToolName::plain("shell_command") {
            Some(extended_nested_tool_timeout_ms(
                crate::tools::handlers::VALIDATION_COMMAND_TIMEOUT_MS,
            ))
        } else {
            None
        }
    })
}

#[derive(Clone)]
pub(super) struct CellDispatchLease {
    state: Arc<CellDispatchLeaseState>,
}

struct CellDispatchLeaseState {
    session: Arc<crate::session::session::Session>,
    cell_id: codex_code_mode::CellId,
    keep_open: std::sync::atomic::AtomicBool,
}

impl CellDispatchLease {
    pub(super) fn new(
        session: Arc<crate::session::session::Session>,
        cell_id: codex_code_mode::CellId,
    ) -> Self {
        Self {
            state: Arc::new(CellDispatchLeaseState {
                session,
                cell_id,
                keep_open: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub(super) fn keep_open(&self) {
        self.state
            .keep_open
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn record_trace(&self, record: impl FnOnce() + Send + 'static) {
        // Queue ordered persistence without gating dispatch or response delivery.
        // Each accepted write owns cleanup even if its tool caller is cancelled.
        let lease = self.clone();
        let (done, tail) = tokio::sync::oneshot::channel();
        let previous = self
            .state
            .session
            .services
            .code_mode_service
            .trace_tails
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.state.cell_id.to_string(), tail);
        let tasks = self.state.session.terminal_tasks.clone();
        self.state.session.terminal_tasks.spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let runtime = tokio::runtime::Handle::current();
            if let Err(error) = tasks
                .spawn_blocking_on(
                    move || {
                        record();
                        let _ = done.send(());
                        drop(lease);
                    },
                    &runtime,
                )
                .await
            {
                tracing::warn!(%error, "code mode trace recording task failed");
            }
        });
    }
}

impl Drop for CellDispatchLeaseState {
    fn drop(&mut self) {
        if !self.keep_open.load(std::sync::atomic::Ordering::Acquire) {
            self.session
                .services
                .code_mode_service
                .finish_cell_dispatch(&self.cell_id);
        }
    }
}

impl CodeModeExecuteHandler {
    pub(crate) fn new(
        spec: ToolSpec,
        direct_nested_tool_specs: Vec<ToolSpec>,
        deferred_nested_tool_specs: Vec<ToolSpec>,
    ) -> Result<Self, String> {
        let mut nested_tool_specs = direct_nested_tool_specs;
        // Deferred tools stay out of the model-visible `exec` description, but
        // code running inside the isolate can discover and invoke them through
        // `ALL_TOOLS`. Cache the complete runtime registry once per handler so
        // every cell does not rebuild it from cloned tool specs.
        nested_tool_specs.extend(deferred_nested_tool_specs);
        let mut enabled_tools = codex_tools::collect_code_mode_tool_definitions(&nested_tool_specs);
        // The collector assigns unique normalized names. The runtime also
        // validates catalogs at its host boundary; no second pass is needed here.
        // The rendered descriptions already contain the schema-derived TypeScript
        // declarations. The isolate consumes only callable metadata, so do not
        // clone and transport the original JSON schema trees for every cell.
        for definition in &mut enabled_tools {
            definition.input_schema = None;
            definition.output_schema = None;
        }
        Ok(Self {
            spec,
            enabled_tools: enabled_tools.into(),
            timeout_catalog: std::sync::Mutex::new(None),
        })
    }

    fn catalog_with_timeouts(
        &self,
        mcp_timeouts: &HashMap<ToolName, u64>,
        terminal_poll_ms: u64,
    ) -> Arc<[codex_code_mode::ToolDefinition]> {
        let overrides = self.enabled_tools.iter().map(|tool| {
            nested_tool_timeout_override(&tool.tool_name, mcp_timeouts, terminal_poll_ms)
        }).collect::<Vec<_>>();
        let mut cached = self.timeout_catalog.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(catalog) = cached.as_ref()
            && catalog.overrides == overrides
        {
            return Arc::clone(&catalog.tools);
        }
        // Publish one immutable catalog per effective timeout overlay. Active
        // cells retain their old snapshot; a changed timeout cannot mutate it.
        let tools = self.enabled_tools.iter().zip(&overrides).map(|(tool, timeout)| {
            let mut tool = tool.clone();
            tool.default_timeout_ms = *timeout;
            tool
        }).collect::<Arc<[_]>>();
        *cached = Some(TimeoutCatalog { overrides, tools: Arc::clone(&tools) });
        tools
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        step_context: std::sync::Arc<crate::session::step_context::StepContext>,
        call_id: String,
        code: String,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let turn = Arc::clone(&step_context.turn);
        let mcp_timeouts = step_context
            .mcp_tools()
            .await
            .iter()
            .map(|tool| {
                let timeout = step_context
                    .mcp
                    .config()
                    .mcp_server_catalog
                    .server(&tool.server_name)
                    .and_then(|server| server.config().tool_timeout_sec)
                    .unwrap_or(codex_mcp::DEFAULT_TOOL_TIMEOUT);
                (
                    tool.canonical_tool_name(),
                    extended_nested_tool_timeout_ms(
                        u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let enabled_tools = self.catalog_with_timeouts(
            &mcp_timeouts,
            session.services.unified_exec_manager.max_write_stdin_yield_time_ms(),
        );
        let exec = ExecContext { session, turn };
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(codex_code_mode::ExecuteRequest {
                state_path: args.persist.then(|| {
                    exec.turn.config.codex_home.join("code-mode-state")
                        .join(format!("{}.json", exec.session.thread_id)).to_path_buf()
                }),
                tool_call_id: call_id.clone(),
                enabled_tools,
                source: args.code.to_owned(),
                // Hand observation to the steerable owner immediately. Output
                // from an awaited script is buffered until its next decision
                // boundary, rather than turning a timer into a model call.
                yield_time_ms: Some(0),
                max_output_tokens: args.max_output_tokens,
                default_tool_timeout_ms: Some(codex_code_mode::DEFAULT_TOOL_TIMEOUT_MS),
            })
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        let runtime_cell_id = cell_id.to_string();
        exec.session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, &call_id);
        exec.session.services.code_mode_service.record_cell_turn(&cell_id, &exec.turn.sub_id);
        exec.session.services.code_mode_service.record_output_budget(
            &cell_id,
            Some(args.max_output_tokens
                .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
                .min(exec.turn.config.tool_output_token_limit
                    .unwrap_or(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL))
                .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL)),
        );
        // Establish cleanup ownership before queuing trace work.
        let dispatch_lease = CellDispatchLease::new(Arc::clone(&exec.session), cell_id.clone());
        emit_failed_code_mode_cell_item(
            &exec,
            &call_id,
            &codex_code_mode::RuntimeResponse::Yielded {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
            },
            started_at,
        ).await;
        let trace_enabled = exec.session.services.rollout_thread_trace.is_enabled();
        let code_cell_trace = exec
            .session
            .services
            .rollout_thread_trace
            .code_cell_trace_context(exec.turn.sub_id.as_str(), runtime_cell_id.as_str());
        if trace_enabled {
            let trace = code_cell_trace.clone();
            let parent_call_id = call_id.clone();
            let source = args.code.to_owned();
            dispatch_lease.record_trace(move || trace.record_started(parent_call_id, source));
        }
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        // Any early return after registration must release both the dispatch
        // gate and the outer-call ownership record. A yielded live cell is the
        // only path that deliberately transfers that lease to a later wait.
        let turn_state = exec
            .session
            .input_queue
            .turn_state_for_sub_id(&exec.session.active_turn, &exec.turn.sub_id)
            .await;
        let (activity_rx, pending_activity) = exec
            .session
            .input_queue
            .subscribe_code_mode_activity(turn_state.as_deref(), false)
            .await;
        if args.deliver && pending_activity.is_none() {
            exec.session.services.code_mode_service.record_delivery_intent(
                &cell_id, &exec.turn, activity_rx.clone(),
            );
        }
        // Consume the immediate initial observation before making the held
        // wait steerable. This clears the runtime's initial observer, so
        // steering cannot leave a stale observer that rejects a later wait.
        let mut initial_response = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                terminate_interrupted_cell(&exec, &cell_id).await
                    .map_err(FunctionCallError::RespondToModel)?.into()
            }
            response = started_cell.initial_response() => {
                response.map_err(FunctionCallError::RespondToModel)?
            }
        };
        let initial_is_automatic_yield = matches!(
            &initial_response,
            codex_code_mode::RuntimeResponse::Yielded { .. }
        );
        let mut initial_output = match &mut initial_response {
            codex_code_mode::RuntimeResponse::Yielded { content_items, .. } => {
                std::mem::take(content_items)
            }
            _ => Vec::new(),
        };
        let (mut response, live_cell, drained_observations) = if initial_is_automatic_yield {
            let held = hold_until_state_change(
                || {
                    exec.session
                        .services
                        .code_mode_service
                        .wait_for_decision(cell_id.clone())
                },
                &cancellation_token,
                super::wait_handler::queued_input_activity(&exec, turn_state.as_deref(), activity_rx),
                "exec cancelled",
            )
            .await;
            let held = match held {
                Ok(held) => held,
                Err(error) if cancellation_token.is_cancelled() => {
                    super::wait_handler::OwnerHeldCodeModeWait {
                        exit: OwnerHeldCodeModeExit::Runtime(
                            terminate_interrupted_cell(&exec, &cell_id).await
                                .map_err(FunctionCallError::RespondToModel)?,
                        ),
                        drained_observations: error.drained_observations,
                    }
                }
                Err(error) => {
                    record_internally_drained_waits(&exec, error.drained_observations.saturating_add(1));
                    return Err(FunctionCallError::RespondToModel(error.message));
                }
            };
            let drained_observations = held.drained_observations.saturating_add(1);
            match held.exit {
                OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                    response,
                )) => (response, true, drained_observations),
                OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::MissingCell(
                    response,
                )) => (response, false, drained_observations),
                OwnerHeldCodeModeExit::InputActivity(activity) => (
                    input_activity_response(&cell_id, activity),
                    true,
                    drained_observations,
                ),
            }
        } else {
            (initial_response, true, 0)
        };
        if !initial_output.is_empty() {
            let tail = match &mut response {
                codex_code_mode::RuntimeResponse::Yielded { content_items, .. }
                | codex_code_mode::RuntimeResponse::ExplicitYield { content_items, .. }
                | codex_code_mode::RuntimeResponse::Terminated { content_items, .. }
                | codex_code_mode::RuntimeResponse::Result { content_items, .. } => content_items,
            };
            initial_output.append(tail);
            *tail = initial_output;
        }
        // Record the raw runtime boundary. The model-visible custom-tool output
        // is produced by `handle_runtime_response` and later linked through
        // `CodeCell.output_item_ids` in the reduced trace.
        // Yielded cells keep running, so terminal lifecycle is only emitted
        // here when the first response also ended the runtime.
        let keep_dispatch_open = live_cell
            && matches!(
                response,
                codex_code_mode::RuntimeResponse::Yielded { .. }
                    | codex_code_mode::RuntimeResponse::ExplicitYield { .. }
            );
        if trace_enabled {
            let traced_response = response.clone();
            dispatch_lease.record_trace(move || {
                code_cell_trace
                    .record_initial_response(&traced_response, live_cell && !keep_dispatch_open);
            });
        }
        // Preserve the captured response while allowing cancellation to finish
        // delivery and dispatch cleanup despite an unrelated open dialog.
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {}
            _ = exec.session.services.elicitations.wait_until_clear() => {}
        }
        emit_failed_code_mode_cell_item(&exec, &call_id, &response, started_at).await;
        let delivery = exec.session.services.code_mode_service.delivery_for_response(
            &cell_id, &exec.turn, &response,
        );
        let mut output = handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .map_err(FunctionCallError::RespondToModel)?;
        attach_delivery_decision(&mut output, delivery);
        if keep_dispatch_open {
            dispatch_lease.keep_open();
        }
        Ok(attach_drained_wait_evidence(
            &exec,
            output,
            &cell_id,
            drained_observations,
        ))
    }
}

impl ToolExecutor<ToolInvocation> for CodeModeExecuteHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl CodeModeExecuteHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            call_id,
            tool_name,
            payload,
            cancellation_token,
            ..
        } = invocation;
        match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(session, step_context, call_id, input, cancellation_token)
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
    fn delegates_workspace_admission(&self) -> bool {
        true
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        // Cancellation must keep polling the handler through bounded cell
        // termination so the V8/runtime owner cannot be orphaned.
        true
    }

    fn tool_execution_timing(&self) -> ToolExecutionTiming {
        // Nested tools own their actual execution timing. Treating the entire
        // JavaScript cell as a handler interval double-counts orchestration and
        // waits as tool runtime.
        ToolExecutionTiming::NestedRuntime
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Custom { .. })
    }
}

#[cfg(test)]
mod tests {
    use codex_code_mode::CellId;

    use super::*;

    #[tokio::test]
    async fn completed_code_cell_cancellation_bypasses_unrelated_elicitation() {
        for use_wait in [false, true] {
            let (mut session, turn) = crate::session::tests::make_session_and_context().await;
            session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
                codex_code_mode::InProcessCodeModeSessionProvider,
            ));
            let session = Arc::new(session);
            let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
            let leases = crate::elicitation::OutOfBandElicitationLeases::new(
                session.services.elicitations.clone(),
            );
            let lease = crate::elicitation::OutOfBandElicitationLeaseId::new(1, "unrelated".into());
            leases.acquire(lease.clone()).unwrap();
            assert!(!session.services.elicitations.has_waiters_for_test());
            let cancellation = tokio_util::sync::CancellationToken::new();
            let pending = tokio::spawn({
                let session = Arc::clone(&session);
                let cancellation = cancellation.clone();
                async move {
                    if use_wait {
                        let started = session.services.code_mode_service.execute(
                            codex_code_mode::ExecuteRequest {
                                state_path: None,
                                tool_call_id: "outer".into(),
                                enabled_tools: Vec::new().into(),
                                source: "await yield_control(); text('captured result');".into(),
                                yield_time_ms: None,
                                max_output_tokens: None,
                                default_tool_timeout_ms: None,
                            },
                        ).await.unwrap();
                        let cell = started.cell_id.clone();
                        session.services.code_mode_service.record_cell_parent_call_id(&cell, "outer");
                        session.services.code_mode_service.mark_cell_ready_for_dispatch(&cell);
                        assert!(matches!(started.initial_response().await.unwrap(),
                            codex_code_mode::RuntimeResponse::ExplicitYield { .. }));
                        super::super::wait_handler::CodeModeWaitHandler.handle(ToolInvocation {
                            session,
                            step_context: step,
                            cancellation_token: cancellation,
                            tracker: Arc::new(tokio::sync::Mutex::new(
                                crate::turn_diff_tracker::TurnDiffTracker::new(),
                            )),
                            call_id: "wait".into(),
                            tool_name: ToolName::plain(super::super::WAIT_TOOL_NAME),
                            source: crate::tools::router::ToolCallSource::Direct,
                            payload: ToolPayload::Function {
                                arguments: serde_json::json!({"cell_id": cell.as_str()}).to_string(),
                            },
                        }).await.unwrap().log_preview()
                    } else {
                        let handler = CodeModeExecuteHandler::new(
                            super::super::execute_spec::create_code_mode_tool(true, false, &[], &[]),
                            Vec::new(), Vec::new(),
                        ).unwrap();
                        let output = handler.execute(session, step, "exec".into(),
                            "text('captured result');".into(), cancellation).await.unwrap();
                        boxed_tool_output(output).log_preview()
                    }
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !session.services.elicitations.has_waiters_for_test() {
                    assert!(!pending.is_finished(), "handler must reach the final delivery gate");
                    tokio::task::yield_now().await;
                }
            }).await.expect("completed cell reaches elicitation gate");
            assert!(!pending.is_finished());
            cancellation.cancel();
            let output = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
                .await.expect("cancellation releases delivery").unwrap();
            assert!(output.contains("captured result"), "{output}");
            assert_eq!(leases.active_count(), 1, "cancellation must not close the dialog");
            assert!(!session.services.code_mode_service.dispatch_broker.has_waitable_cells());
            leases.release(&lease);
            session.services.code_mode_service.shutdown().await.unwrap();
        }
    }

    #[test]
    fn direct_delivery_validates_final_schema_and_preserves_media() {
        use codex_code_mode::FunctionCallOutputContentItem;
        let response = |items| codex_code_mode::RuntimeResponse::Result {
            cell_id: CellId::new("delivery".into()),
            content_items: items,
            error_text: None,
            output_loss: None,
        };
        let schema = serde_json::json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false});
        let json = response(vec![FunctionCallOutputContentItem::InputText { text: r#"{"count":2}"#.into() }]);
        assert_eq!(schema_validated_delivery(&json, 100, Some(&schema)).as_deref(), Ok(r#"{"count":2}"#));
        assert_eq!(schema_validated_delivery(&json, 100, Some(&serde_json::json!({"type":"array"}))).unwrap_err()["category"], "schema_mismatch");
        let image = response(vec![FunctionCallOutputContentItem::InputImage {
            image_url: "https://example.com/result.png".into(), detail: None,
        }]);
        assert_eq!(schema_validated_delivery(&image, 100, None).as_deref(), Ok("![image](<https://example.com/result.png>)"));
        assert_eq!(schema_validated_delivery(&image, 100, Some(&schema)).unwrap_err()["category"], "invalid_json");
        assert_eq!(schema_validated_delivery(&json, 0, None).unwrap_err()["category"], "output_budget");
        assert_eq!(schema_validated_delivery(&response(Vec::new()), 100, None).unwrap_err()["category"], "empty_output");
        let bad = response(vec![FunctionCallOutputContentItem::InputText { text:r#"{"count":"bad"}"#.into() }]);
        assert_eq!(schema_validated_delivery(&bad, 100, Some(&schema)).unwrap_err()["instance_pointer"], "/count");
        let mut output = FunctionToolOutput::from_text("partial".into(), Some(true))
            .with_sampling_request_signal(serde_json::json!({}));
        output.essential_inline.insert(super::super::VISIBLE_OUTPUT_TRUNCATED_KEY.into(), true.into());
        attach_delivery_decision(&mut output, Ok(Some("must not deliver".into())));
        assert_eq!(output.essential_inline["delivery_refused"]["category"], "output_truncated");
        assert!(output.sampling_request_signal.unwrap().get("explicit_completion_message").is_none());
    }

    #[test]
    fn long_poll_timeout_does_not_extend_ordinary_nested_tools() {
        let timeouts = HashMap::from([(ToolName::plain("mcp_test"), 123_000)]);
        assert_eq!(nested_tool_timeout_override(&ToolName::plain("shell_command"), &timeouts, 5_000), Some(315_000));
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("read_file"), &timeouts, 300_000),
            None
        );
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("read_tool_output"), &timeouts, 300_000),
            None
        );
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("write_stdin"), &timeouts, 300_000),
            Some(315_000)
        );
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("exec_command"), &timeouts, 5_000),
            Some(315_000),
            "initial command observation is independent of the terminal poll cap"
        );
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("mcp_test"), &timeouts, 300_000),
            Some(123_000)
        );
    }

    #[test]
    fn cached_runtime_catalog_drops_schema_trees_after_rendering_descriptions() {
        let nested = crate::tools::handlers::shell_spec::create_write_stdin_tool();
        let handler = CodeModeExecuteHandler::new(nested.clone(), vec![nested], Vec::new())
            .expect("build code mode handler");

        assert_eq!(handler.enabled_tools.len(), 1);
        let definition = &handler.enabled_tools[0];
        assert!(definition.input_schema.is_none());
        assert!(definition.output_schema.is_none());
        assert!(definition.description.contains("exec tool declaration:"));
        assert!(!definition.description.contains("Authoritative input_schema"));
        let first = handler.catalog_with_timeouts(&HashMap::new(), 60_000);
        let same = handler.catalog_with_timeouts(&HashMap::new(), 60_000);
        assert!(Arc::ptr_eq(&first, &same));
        let changed = handler.catalog_with_timeouts(&HashMap::new(), 300_000);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(first[0].default_timeout_ms, Some(75_000));
        assert_eq!(changed[0].default_timeout_ms, Some(315_000));
    }

    #[test]
    fn cached_runtime_catalog_retains_contract_when_projection_fails() {
        let mut nested = crate::tools::handlers::shell_spec::create_write_stdin_tool();
        let ToolSpec::Function(tool) = &mut nested else { panic!("function tool"); };
        tool.parameters.properties.as_mut().unwrap().get_mut("chars").unwrap().description =
            Some(format!("{}Only use an authorized handle.", "x".repeat(150_000)));
        let authoritative = serde_json::to_string(&tool.parameters).unwrap();
        let handler = CodeModeExecuteHandler::new(nested.clone(), vec![nested], Vec::new()).unwrap();
        let definition = &handler.enabled_tools[0];
        assert!(definition.input_schema.is_none());
        assert!(definition.description.contains("Authoritative input_schema"));
        // Value serialization uses a sorted map; compare JSON rather than order.
        let raw = definition.description.split_once("```json\n").unwrap().1.split_once("\n```").unwrap().0;
        assert_eq!(serde_json::from_str::<serde_json::Value>(raw).unwrap(),
            serde_json::from_str::<serde_json::Value>(&authoritative).unwrap());
    }

    #[tokio::test]
    async fn dispatch_lease_cleans_parent_and_active_state_on_early_exit() {
        let (session, _) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let service = &session.services.code_mode_service;
        let cell_id = CellId::new("early-exit-cell".to_string());
        service.record_cell_parent_call_id(&cell_id, "outer-call");
        service.mark_cell_ready_for_dispatch(&cell_id);
        assert!(service.dispatch_broker.has_waitable_cells());
        let pending = service.begin_packet_call(&cell_id).unwrap();
        service.record_packet_call(
            &cell_id,
            false,
            42,
            vec![
                codex_protocol::models::FunctionCallOutputContentItem::InputText {
                    text: "abandoned feedback".to_string(),
                },
            ],
        );

        drop(CellDispatchLease::new(
            Arc::clone(&session),
            cell_id.clone(),
        ));

        assert_eq!(service.cell_parent_call_id(&cell_id), None);
        assert!(!service.dispatch_broker.has_waitable_cells());
        service.complete_packet_call(
            &cell_id,
            pending,
            false,
            10,
            Vec::new(),
            None,
            Some((
                crate::tools::context::RequiredToolTerminalCause::Failure,
                "late failure".to_string(),
            )),
        );
        assert_eq!(service.begin_packet_call(&cell_id), None);
        assert!(
            !service
                .packet_admission
                .lock()
                .unwrap()
                .cells
                .contains_key(cell_id.as_str())
        );
    }

    #[tokio::test]
    async fn dispatch_lease_preserves_a_yielded_live_cell() {
        let (session, _) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let service = &session.services.code_mode_service;
        let cell_id = CellId::new("yielded-cell".to_string());
        service.record_cell_parent_call_id(&cell_id, "outer-call");
        service.mark_cell_ready_for_dispatch(&cell_id);

        let lease = CellDispatchLease::new(Arc::clone(&session), cell_id.clone());
        lease.keep_open();
        drop(lease);

        assert_eq!(
            service.cell_parent_call_id(&cell_id).as_deref(),
            Some("outer-call")
        );
        assert!(service.dispatch_broker.has_waitable_cells());
        service.finish_cell_dispatch(&cell_id);
    }

    #[tokio::test]
    async fn registered_code_cells_persist_ordered_initial_and_terminal_traces() -> anyhow::Result<()> {
        use crate::session::step_context::StepContext;
        use crate::tools::context::ToolDispatchState;
        use crate::tools::router::ToolCall;
        use crate::tools::router::ToolCallSource;
        use crate::tools::router::ToolRouter;
        use crate::tools::router::ToolRouterParams;
        use codex_code_mode::FunctionCallOutputContentItem;
        use codex_rollout_trace::CodeCellRuntimeStatus;
        use codex_rollout_trace::RawTraceEvent;
        use codex_rollout_trace::RawTraceEventPayload;
        let root = tempfile::tempdir()?;
        let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
        session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
            codex_code_mode::InProcessCodeModeSessionProvider,
        ));
        turn.model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
        session.services.rollout_thread_trace =
            codex_rollout_trace::ThreadTraceContext::start_root_in_root_for_test(
                root.path(),
                codex_rollout_trace::ThreadStartedTraceMetadata {
                    thread_id: session.thread_id.to_string(),
                    agent_path: "/root".to_string(),
                    task_name: None,
                    nickname: None,
                    agent_role: None,
                    session_source: codex_protocol::protocol::SessionSource::Exec,
                    cwd: turn.cwd().to_path_buf(),
                    rollout_path: None,
                    model: turn.model_info.slug.clone(),
                    provider_name: "test-provider".to_string(),
                    approval_policy: "never".to_string(),
                    sandbox_policy: "danger-full-access".to_string(),
                },
            )?;
        session
            .services
            .rollout_thread_trace
            .record_codex_turn_started(turn.sub_id.as_str());
        let bundle = std::fs::read_dir(root.path())?
            .next()
            .expect("one trace bundle")?
            .path();
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let step = StepContext::for_test(Arc::clone(&turn));
        let router = ToolRouter::try_from_context(
            &step,
            ToolRouterParams {
                mcp_tools: None,
                deferred_mcp_tools: None,
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &Default::default(),
        )
        .map_err(anyhow::Error::msg)?;
        let tracker = Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        ));
        let read_events = || -> anyhow::Result<Vec<RawTraceEvent>> {
            std::fs::read_to_string(bundle.join("trace.jsonl"))?
                .lines()
                .map(|line| serde_json::from_str(line).map_err(Into::into))
                .collect()
        };
        for (case, code, terminal_status, terminal_text) in [
            (
                "completed",
                "text('ready');",
                CodeCellRuntimeStatus::Completed,
                "ready",
            ),
            (
                "waited",
                "text('first'); await yield_control(); text('second');",
                CodeCellRuntimeStatus::Completed,
                "second",
            ),
            (
                "terminated",
                "text('first'); await yield_control(); await new Promise(resolve => setTimeout(resolve, 60_000));",
                CodeCellRuntimeStatus::Terminated,
                "",
            ),
        ] {
            let call_id = format!("trace-{case}");
            let state = Arc::new(ToolDispatchState::new());
            assert!(state.try_admit());
            let result = router
                .dispatch_tool_call_with_terminal_outcome(
                    Arc::clone(&session),
                    Arc::clone(&step),
                    tokio_util::sync::CancellationToken::new(),
                    Arc::clone(&tracker),
                    ToolCall {
                        tool_name: codex_tools::ToolName::plain("exec"),
                        call_id: call_id.clone(),
                        payload: ToolPayload::Custom {
                            input: code.to_string(),
                        },
                    },
                    ToolCallSource::Direct,
                    state,
                )
                .await?;
            let preview = result.result.log_preview();
            assert!(
                preview.contains(if case == "completed" {
                    "ready"
                } else {
                    "first"
                }),
                "{preview}"
            );
            session.terminal_tasks.close();
            session.terminal_tasks.wait().await;
            session.terminal_tasks.reopen();
            let events = read_events()?;
            let cell_id = events
                .iter()
                .find_map(|event| match &event.payload {
                    RawTraceEventPayload::CodeCellStarted {
                        runtime_cell_id,
                        model_visible_call_id,
                        source_js,
                    } if model_visible_call_id == &call_id => {
                        assert_eq!(source_js, code);
                        Some(runtime_cell_id.clone())
                    }
                    _ => None,
                })
                .expect("actual exec start trace is durable before returning");
            let runtime_id = codex_code_mode::CellId::new(cell_id.clone());
            if case != "completed" {
                assert_eq!(
                    session
                        .services
                        .code_mode_service
                        .cell_parent_call_id(&runtime_id),
                    Some(call_id.clone())
                );
                assert!(!events.iter().any(|event| matches!(&event.payload,
                    RawTraceEventPayload::CodeCellEnded { runtime_cell_id, .. } if runtime_cell_id == &cell_id)));
                let state = Arc::new(ToolDispatchState::new());
                assert!(state.try_admit());
                let wait_payload = ToolPayload::Function {
                    arguments: serde_json::json!({"cell_id":cell_id,"terminate":case == "terminated"})
                        .to_string(),
                };
                let result = router
                    .dispatch_tool_call_with_terminal_outcome(
                        Arc::clone(&session),
                        Arc::clone(&step),
                        tokio_util::sync::CancellationToken::new(),
                        Arc::clone(&tracker),
                        ToolCall {
                            tool_name: codex_tools::ToolName::plain("wait"),
                            call_id: format!("wait-{case}"),
                            payload: wait_payload.clone(),
                        },
                        ToolCallSource::Direct,
                        state,
                    )
                    .await?;
                if case == "waited" {
                    let waited_preview = result.result.log_preview();
                    let observed = format!("{preview}\n{waited_preview}");
                    assert_eq!(
                        observed
                            .lines()
                            .filter(|line| matches!(*line, "first" | "second"))
                            .collect::<Vec<_>>(),
                        vec!["first", "second"],
                        "initial={preview:?}; wait={waited_preview:?}; signal={:?}; canonical={:?}",
                        result.result.sampling_request_signal(),
                        result.result.canonical_result(&wait_payload)
                    );
                    assert_eq!(
                        result.result.outcome_for_logging(),
                        codex_tools::ToolOutputOutcome::Success
                    );
                }
            }
            session.terminal_tasks.close();
            session.terminal_tasks.wait().await;
            session.terminal_tasks.reopen();
            assert_eq!(
                session
                    .services
                    .code_mode_service
                    .cell_parent_call_id(&runtime_id),
                None
            );
            let events = read_events()?;
            let mut sequence = Vec::new();
            let mut yielded_trace_output = Vec::new();
            for event in &events {
                match &event.payload {
                    RawTraceEventPayload::CodeCellStarted {
                        runtime_cell_id, ..
                    } if runtime_cell_id == &cell_id => sequence.push("started"),
                    RawTraceEventPayload::CodeCellInitialResponse {
                        runtime_cell_id,
                        status,
                        response_payload,
                    } if runtime_cell_id == &cell_id => {
                        sequence.push("initial");
                        assert_eq!(
                            *status,
                            if case == "completed" {
                                CodeCellRuntimeStatus::Completed
                            } else {
                                CodeCellRuntimeStatus::Yielded
                            }
                        );
                        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(
                            bundle.join(&response_payload.as_ref().expect("initial payload").path),
                        )?)?;
                        let response: codex_code_mode::RuntimeResponse =
                            serde_json::from_value(raw["response"].clone())?;
                        let content_items = match response {
                            codex_code_mode::RuntimeResponse::Result {
                                content_items,
                                error_text: None,
                                ..
                            } if case == "completed" => content_items,
                            codex_code_mode::RuntimeResponse::ExplicitYield {
                                content_items, ..
                            } if case != "completed" => content_items,
                            other => panic!("unexpected initial response: {other:?}"),
                        };
                        if case == "waited" {
                            // The script continues after yielding; already-buffered output
                            // may be delivered with either response, but never lost or repeated.
                            yielded_trace_output.extend(content_items);
                        } else {
                            assert_eq!(
                                content_items,
                                vec![FunctionCallOutputContentItem::InputText {
                                    text: if case == "completed" {
                                        "ready"
                                    } else {
                                        "first"
                                    }
                                    .to_string()
                                }]
                            );
                        }
                    }
                    RawTraceEventPayload::CodeCellEnded {
                        runtime_cell_id,
                        status,
                        response_payload,
                    } if runtime_cell_id == &cell_id => {
                        sequence.push("ended");
                        assert_eq!(status, &terminal_status);
                        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(
                            bundle.join(&response_payload.as_ref().expect("terminal payload").path),
                        )?)?;
                        let response: codex_code_mode::RuntimeResponse =
                            serde_json::from_value(raw["response"].clone())?;
                        let content_items =
                            match response {
                                codex_code_mode::RuntimeResponse::Result {
                                    content_items,
                                    error_text: None,
                                    ..
                                } if case != "terminated" => content_items,
                                codex_code_mode::RuntimeResponse::Terminated {
                                    content_items, ..
                                } if case == "terminated" => content_items,
                                other => panic!("unexpected terminal response: {other:?}"),
                            };
                        if case == "waited" {
                            yielded_trace_output.extend(content_items);
                        } else {
                            assert_eq!(
                                content_items,
                                if terminal_text.is_empty() {
                                    vec![]
                                } else {
                                    vec![FunctionCallOutputContentItem::InputText {
                                        text: terminal_text.to_string(),
                                    }]
                                }
                            );
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(sequence, vec!["started", "initial", "ended"]);
            if case == "waited" {
                assert_eq!(
                    yielded_trace_output,
                    vec![
                        FunctionCallOutputContentItem::InputText {
                            text: "first".to_string()
                        },
                        FunctionCallOutputContentItem::InputText {
                            text: "second".to_string()
                        },
                    ]
                );
            }
        }
        session
            .services
            .code_mode_service
            .shutdown()
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }

    #[tokio::test]
    async fn queued_code_cell_trace_returns_before_storage_and_retains_ordered_cleanup()
    -> anyhow::Result<()> {
        let (session, _) = crate::session::tests::make_session_and_context().await;
        let session = Arc::new(session);
        let cell_id = codex_code_mode::CellId::new("trace-cancelled-owner".to_string());
        session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, "outer-trace-call");
        session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        let lease = CellDispatchLease::new(Arc::clone(&session), cell_id.clone());
        let root = tempfile::tempdir()?;
        let output_path = root.path().join("trace-write.txt");
        let write_path = output_path.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        lease.record_trace(move || {
            started_tx.send(()).expect("recording caller still waits");
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("release writer");
            std::fs::write(write_path, "accepted trace write").expect("persist actual trace work");
        });
        let second_path = output_path.clone();
        lease.record_trace(move || {
            assert_eq!(
                std::fs::read_to_string(&second_path).unwrap(),
                "accepted trace write"
            );
            std::fs::write(second_path, "ordered second trace").unwrap();
        });
        drop(lease);
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx).await??;
        assert_eq!(
            session
                .services
                .code_mode_service
                .cell_parent_call_id(&cell_id)
                .as_deref(),
            Some("outer-trace-call")
        );
        assert!(
            session
                .services
                .code_mode_service
                .dispatch_broker
                .has_waitable_cells()
        );
        assert!(!output_path.exists());
        release_tx.send(())?;
        session.terminal_tasks.close();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            session.terminal_tasks.wait(),
        )
        .await?;
        assert_eq!(
            std::fs::read_to_string(output_path)?,
            "ordered second trace"
        );
        assert_eq!(
            session
                .services
                .code_mode_service
                .cell_parent_call_id(&cell_id),
            None
        );
        assert!(
            !session
                .services
                .code_mode_service
                .dispatch_broker
                .has_waitable_cells()
        );
        Ok(())
    }
}
