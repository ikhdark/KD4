use crate::FunctionCallError;
use crate::agent::task_capabilities::TypedToolClass;
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

fn direct_delivery_text(
    response: &codex_code_mode::RuntimeResponse,
    limit: usize,
) -> Result<String, serde_json::Value> {
    use codex_code_mode::FunctionCallOutputContentItem;
    let codex_code_mode::RuntimeResponse::Result {
        content_items,
        error_text: None,
        output_loss: None,
        ..
    } = response
    else {
        let category = match response {
            codex_code_mode::RuntimeResponse::Result {
                error_text: Some(_),
                ..
            } => "runtime_error",
            codex_code_mode::RuntimeResponse::Result {
                output_loss: Some(_),
                ..
            } => "output_loss",
            codex_code_mode::RuntimeResponse::Terminated { .. } => "terminated",
            _ => "cell_running",
        };
        return Err(serde_json::json!({"category":category}));
    };
    let mut parts = Vec::with_capacity(content_items.len());
    let mut text_parts = Vec::new();
    for item in content_items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                text_parts.push(text.as_str());
                parts.push(text.clone());
            }
            FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                // Use the already-produced media; never fetch or re-encode it.
                // Reject Markdown delimiters/control characters rather than
                // allowing a media URL to introduce additional response text.
                let supported = image_url.split_once(':').is_some_and(|(scheme, rest)| {
                    ((scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
                        && rest.starts_with("//"))
                        || (scheme.eq_ignore_ascii_case("data")
                            && rest
                                .get(..6)
                                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/")))
                }) || std::path::Path::new(image_url).is_absolute();
                if image_url
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, '<' | '>'))
                    || !supported
                {
                    return Err(serde_json::json!({"category":"unsupported_media"}));
                }
                parts.push(format!("![image](<{image_url}>)"));
            }
        }
    }
    let message = parts.join("\n");
    if message.trim().is_empty() {
        return Err(serde_json::json!({"category":"empty_output"}));
    }
    // Native media already crossed runtime byte admission. Charge only its
    // accompanying text, never base64 image bytes as model text tokens.
    if codex_utils_output_truncation::model_token_count(&text_parts.join("\n")) > limit {
        return Err(serde_json::json!({"category":"output_budget", "limit":limit}));
    }
    Ok(message)
}

pub(super) fn schema_validated_delivery(
    response: &codex_code_mode::RuntimeResponse,
    limit: usize,
    schema: Option<&crate::session::turn_context::PreparedOutputSchema>,
) -> Result<String, serde_json::Value> {
    let text = direct_delivery_text(response, limit)?;
    if let Some(schema) = schema {
        schema.validate_text(Some(&text))?;
    }
    Ok(text)
}

pub(super) fn attach_delivery_decision(
    output: &mut FunctionToolOutput,
    decision: Result<Option<String>, serde_json::Value>,
) {
    let refusal = match decision {
        Ok(None) => return,
        Err(refusal) => refusal,
        Ok(Some(message)) => {
            if output.success != Some(true) {
                serde_json::json!({"category":"failed_output"})
            } else if output
                .essential_inline
                .contains_key(super::VISIBLE_OUTPUT_TRUNCATED_KEY)
            {
                serde_json::json!({"category":"output_truncated"})
            } else if let Some(signal) = output
                .sampling_request_signal
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
            {
                signal.insert("explicit_completion_message".into(), message.into());
                return;
            } else {
                serde_json::json!({"category":"completion_signal_unavailable"})
            }
        }
    };
    output
        .essential_inline
        .insert("delivery_refused".into(), refusal.clone());
    let item = codex_protocol::models::FunctionCallOutputContentItem::InputText {
        text: serde_json::json!({"delivery_refused":refusal}).to_string(),
    };
    output.body.push(item.clone());
    if let Some(canonical) = &mut output.canonical_body {
        canonical.push(item);
    }
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
    tool_timeouts: &HashMap<ToolName, u64>,
    terminal_poll_ms: u64,
) -> Option<u64> {
    tool_timeouts.get(tool).copied().or_else(|| {
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
        Self::new_with_input_schemas(
            spec,
            direct_nested_tool_specs,
            deferred_nested_tool_specs,
            &HashMap::new(),
        )
    }

    pub(crate) fn new_with_input_schemas(
        spec: ToolSpec,
        direct_nested_tool_specs: Vec<ToolSpec>,
        deferred_nested_tool_specs: Vec<ToolSpec>,
        canonical_input_schemas: &HashMap<codex_tools::ToolName, serde_json::Value>,
    ) -> Result<Self, String> {
        let mut nested_tool_specs = direct_nested_tool_specs;
        // Deferred tools stay out of the model-visible `exec` description, but
        // code running inside the isolate can discover and invoke them through
        // `ALL_TOOLS`. Cache the complete runtime registry once per handler so
        // every cell does not rebuild it from cloned tool specs.
        nested_tool_specs.extend(deferred_nested_tool_specs);
        // Keep multi-member namespace expansion and sorting with the shared
        // collector, but render provider constraints from their canonical owner.
        let mut enabled_tools = codex_tools::collect_code_mode_tool_definitions_with_input_schemas(
            &nested_tool_specs,
            canonical_input_schemas,
        );
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
        tool_timeouts: &HashMap<ToolName, u64>,
        terminal_poll_ms: u64,
    ) -> Arc<[codex_code_mode::ToolDefinition]> {
        let overrides = self
            .enabled_tools
            .iter()
            .map(|tool| {
                nested_tool_timeout_override(&tool.tool_name, tool_timeouts, terminal_poll_ms)
            })
            .collect::<Vec<_>>();
        let mut cached = self
            .timeout_catalog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(catalog) = cached.as_ref()
            && catalog.overrides == overrides
        {
            return Arc::clone(&catalog.tools);
        }
        // Publish one immutable catalog per effective timeout overlay. Active
        // cells retain their old snapshot; a changed timeout cannot mutate it.
        let tools = self
            .enabled_tools
            .iter()
            .zip(&overrides)
            .map(|(tool, timeout)| {
                let mut tool = tool.clone();
                tool.default_timeout_ms = *timeout;
                tool
            })
            .collect::<Arc<[_]>>();
        *cached = Some(TimeoutCatalog {
            overrides,
            tools: Arc::clone(&tools),
        });
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
        if args.deliver
            && !step_context
                .turn
                .config
                .features
                .enabled(codex_features::Feature::Kd4Runtime)
        {
            return Err(FunctionCallError::RespondToModel(
                "exec `deliver: true` is unavailable when `kd4_runtime` is disabled; no cell was started. Omit `deliver` and return the answer in a normal final response."
                    .to_string(),
            ));
        }
        // Apply the public ceiling before crossing either session transport.
        // The remote wire uses i32, while the pragma accepts safe integers.
        let max_output_tokens = args
            .max_output_tokens
            .map(|limit| limit.min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL));
        let turn = Arc::clone(&step_context.turn);
        let mut tool_timeouts = step_context
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
        if let Some(router) = step_context.tool_router() {
            for tool in self.enabled_tools.iter() {
                if tool.tool_name.name == "wait_agent"
                    && router.classify_tool_name(&turn, &tool.tool_name)
                        == TypedToolClass::AgentCommunication
                {
                    // Native agent waits own both passive observation and their
                    // explicit input deadline. A generic wrapper would turn an
                    // unchanged wait into a spurious model continuation. Match
                    // the registered owner, not a fixed namespace or an external
                    // tool with the same leaf name. Explicit call options still
                    // override this host-owned zero in the runtime callback.
                    tool_timeouts.insert(tool.tool_name.clone(), 0);
                } else if tool.tool_name == ToolName::plain("spawn_agents_on_csv")
                    && router.classify_tool_name(&turn, &tool.tool_name)
                        == TypedToolClass::RootTaskControl
                {
                    // The batch owner enforces worker runtime limits and joins
                    // cancellation/cleanup. A generic 60-second deadline can
                    // cancel a healthy batch after only some rows finish.
                    // Retain native limits and explicit per-call overrides.
                    tool_timeouts.insert(tool.tool_name.clone(), 0);
                }
            }
        }
        let enabled_tools = self.catalog_with_timeouts(
            &tool_timeouts,
            session
                .services
                .unified_exec_manager
                .max_write_stdin_yield_time_ms(),
        );
        let exec = ExecContext { session, turn };
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute_for_step(
                codex_code_mode::ExecuteRequest {
                    state_path: args.persist.then(|| {
                        exec.turn
                            .config
                            .codex_home
                            .join("code-mode-state")
                            .join(format!("{}.json", exec.session.thread_id))
                            .to_path_buf()
                    }),
                    tool_call_id: call_id.clone(),
                    enabled_tools,
                    source: args.code.to_owned(),
                    // Hand observation to the steerable owner immediately. Output
                    // from an awaited script is buffered until its next decision
                    // boundary, rather than turning a timer into a model call.
                    yield_time_ms: Some(0),
                    max_output_tokens,
                    default_tool_timeout_ms: Some(codex_code_mode::DEFAULT_TOOL_TIMEOUT_MS),
                },
                &step_context,
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        exec.session
            .services
            .code_mode_service
            .bind_cell_environments(&cell_id, &step_context);
        let runtime_cell_id = cell_id.to_string();
        exec.session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, &call_id);
        exec.session
            .services
            .code_mode_service
            .bind_cell_delivery_turn(&cell_id, &exec.turn);
        exec.session
            .services
            .code_mode_service
            .record_output_budget(
                &cell_id,
                Some(
                    max_output_tokens
                        .unwrap_or(codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
                        .min(
                            exec.turn
                                .config
                                .tool_output_token_limit
                                .unwrap_or(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL),
                        )
                        .min(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL),
                ),
            );
        // Establish cleanup ownership before opening dispatch.
        let dispatch_lease = CellDispatchLease::new(Arc::clone(&exec.session), cell_id.clone());
        // Register one owned publication before opening dispatch. Downstream
        // presentation pressure must not gate the cell's nested tool work.
        // The session tracker drains it even if this caller is cancelled.
        let started_publication = exec.session.terminal_tasks.spawn({
            let exec = ExecContext { session: Arc::clone(&exec.session), turn: Arc::clone(&exec.turn) };
            let call_id = call_id.clone();
            let cell_id = cell_id.clone();
            async move {
                emit_failed_code_mode_cell_item(
                    &exec, &call_id,
                    &codex_code_mode::RuntimeResponse::Yielded { cell_id, content_items: Vec::new() },
                    started_at,
                ).await;
            }
        });
        exec.session
            .services
            .code_mode_service
            .mark_cell_ready_for_dispatch(&cell_id);
        // Early returns before observing the cell release both the dispatch
        // gate and outer-call ownership. Live or uncertain observations instead
        // transfer that lease to a later wait.
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
            exec.session
                .services
                .code_mode_service
                .record_delivery_intent(&cell_id, &exec.turn, activity_rx.clone());
        }
        // Consume the immediate initial observation before making the held
        // wait steerable. This clears the runtime's initial observer, so
        // steering cannot leave a stale observer that rejects a later wait.
        let mut initial_response = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                terminate_interrupted_cell(&exec, &cell_id).await
                    .map(Into::into).unwrap_or_else(|error| super::observation_error_response(&cell_id, error))
            }
            response = started_cell.initial_response() => {
                response.unwrap_or_else(|error| super::observation_error_response(&cell_id, error))
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
                super::wait_handler::queued_input_activity(
                    &exec,
                    turn_state.as_deref(),
                    activity_rx.clone(),
                ),
                super::wait_handler::queued_steering_activity(
                    &exec,
                    turn_state.as_deref(),
                    activity_rx,
                ),
                "exec cancelled",
            )
            .await;
            let held = match held {
                Ok(held) => held,
                Err(error) if cancellation_token.is_cancelled() => {
                    super::wait_handler::OwnerHeldCodeModeWait {
                        exit: OwnerHeldCodeModeExit::Runtime(
                            terminate_interrupted_cell(&exec, &cell_id)
                                .await
                                .unwrap_or_else(|error| codex_code_mode::WaitOutcome::LiveCell(super::observation_error_response(&cell_id, error))),
                        ),
                        drained_observations: error.drained_observations,
                    }
                }
                Err(error) => {
                    super::wait_handler::OwnerHeldCodeModeWait {
                        exit: OwnerHeldCodeModeExit::Runtime(codex_code_mode::WaitOutcome::LiveCell(
                            super::observation_error_response(&cell_id, error.message))),
                        drained_observations: error.drained_observations,
                    }
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
        // Observation failure is not terminal evidence. Transfer ownership
        // before fallible delivery/packaging so those failures cannot retire
        // a cell whose runtime may still be dispatching nested work.
        if live_cell && !super::runtime_response_is_terminal(&response) {
            dispatch_lease.keep_open();
        }
        // Preserve the captured response while allowing cancellation to finish
        // delivery and dispatch cleanup despite an unrelated open dialog.
        let dialog_wait_started = std::time::Instant::now();
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {}
            _ = exec.session.services.elicitations.wait_until_clear() => {}
        }
        tracing::info!(event.name = "codex.code_mode.dialog_delivery_gate",
            turn_id = %exec.turn.sub_id, runtime_cell_id = %cell_id,
            parent_tool_call_id = %call_id,
            dialog_wait_ms = u64::try_from(dialog_wait_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            cancelled = cancellation_token.is_cancelled(),
            scope = "session_wide", "captured cell response finished dialog delivery gate");
        // Sequence presentation in its owned task, not ahead of retention or
        // packaging. Even a still-blocked started event must not gate those.
        let completed_publication = exec.session.terminal_tasks.spawn({
            let exec = ExecContext { session: Arc::clone(&exec.session), turn: Arc::clone(&exec.turn) };
            let call_id = call_id.clone();
            let response = response.clone();
            let lease = dispatch_lease.clone();
            async move {
                started_publication.await?;
                emit_failed_code_mode_cell_item(&exec, &call_id, &response, started_at).await;
                drop(lease);
                Ok::<(), tokio::task::JoinError>(())
            }
        });
        let retention_settled = exec
            .session
            .services
            .code_mode_service
            .observe_packet_retention(
                &cell_id,
                matches!(
                    &response,
                    codex_code_mode::RuntimeResponse::Terminated { .. }
                ) || super::observation_failed(&response),
                &cancellation_token,
            )
            .await;
        let (mut output, delivered_call_ids, delivery) =
            handle_runtime_response(&exec, response, max_output_tokens, started_at)
                .map_err(FunctionCallError::RespondToModel)?;
        exec.session
            .services
            .code_mode_service
            .elide_bodies_already_in_context(&mut output);
        if !retention_settled {
            super::attach_pending_retention(&mut output, &cell_id, &call_id);
        }
        attach_delivery_decision(&mut output, delivery);
        // Packaging and retention may overlap presentation, but accepted
        // finalization still joins every required lifecycle publication.
        completed_publication.await.map_err(|error| {
            FunctionCallError::RespondToModel(format!("code-mode completed publication failed: {error}"))
        })?.map_err(|error| {
            FunctionCallError::RespondToModel(format!("code-mode started publication failed: {error}"))
        })?;
        exec.turn
            .turn_timing_state
            .record_code_mode_projection_delivery(&call_id, &delivered_call_ids);
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

