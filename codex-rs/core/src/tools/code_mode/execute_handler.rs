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
        mcp_timeouts: &HashMap<ToolName, u64>,
        terminal_poll_ms: u64,
    ) -> Arc<[codex_code_mode::ToolDefinition]> {
        let overrides = self
            .enabled_tools
            .iter()
            .map(|tool| {
                nested_tool_timeout_override(&tool.tool_name, mcp_timeouts, terminal_poll_ms)
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
                    max_output_tokens: args.max_output_tokens,
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
                    args.max_output_tokens
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
        let delivery = exec
            .session
            .services
            .code_mode_service
            .delivery_for_response(&cell_id, &exec.turn, &response);
        let mut output =
            handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
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

    #[test]
    fn audit144_canonical_input_schema_reaches_cached_code_mode_metadata() {
        let spec = codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "conditional".into(),
            description: "provider".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        });
        let canonical = serde_json::json!({"type":"object","properties":{"mode":{"type":"string"},"text":{"type":"string"}},
        "if":{"properties":{"mode":{"const":"edit"}},"required":["mode"]},"then":{"required":["text"]},"dependentSchemas":{"text":{"required":["mode"]}},"not":{"required":["forbidden"]}});
        let handler = CodeModeExecuteHandler::new_with_input_schemas(
            super::super::execute_spec::create_code_mode_tool(false, true, &[], &[]),
            Vec::new(),
            vec![spec],
            &HashMap::from([(
                codex_tools::ToolName::plain("conditional"),
                canonical.clone(),
            )]),
        )
        .unwrap();
        let description = &handler.enabled_tools[0].description;
        for keyword in ["dependentSchemas", "if", "then", "not"] {
            assert!(description.contains(keyword), "{description}");
        }
        let validator = jsonschema::validator_for(&canonical).unwrap();
        for valid in [
            serde_json::json!({"mode":"edit","text":"ok"}),
            serde_json::json!({"mode":"view"}),
            serde_json::json!({}),
        ] {
            assert!(validator.is_valid(&valid));
        }
        for invalid in [
            serde_json::json!({"mode":"edit"}),
            serde_json::json!({"text":"orphan"}),
            serde_json::json!({"forbidden":true}),
        ] {
            assert!(!validator.is_valid(&invalid));
        }
    }

    #[test]
    fn audit144_native_image_bytes_are_not_charged_as_caption_tokens() {
        use base64::Engine;
        let pixels = image::RgbImage::from_fn(256, 256, |x, y| {
            image::Rgb([(x * y) as u8, (x + y) as u8, (x ^ y) as u8])
        });
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        );
        assert!(uri.len() > 1024);
        let response = codex_code_mode::RuntimeResponse::Result {
            cell_id: CellId::new("media".into()),
            error_text: None,
            output_loss: None,
            content_items: vec![
                codex_code_mode::FunctionCallOutputContentItem::InputText {
                    text: "caption".into(),
                },
                codex_code_mode::FunctionCallOutputContentItem::InputImage {
                    image_url: uri.clone(),
                    detail: None,
                },
            ],
        };
        let delivered = direct_delivery_text(&response, 10).unwrap();
        assert!(delivered.contains(&uri));
    }

    #[tokio::test]
    async fn completed_code_cell_cancellation_bypasses_unrelated_elicitation() {
        for use_wait in [false, true] {
            let (mut session, turn) = crate::session::tests::make_session_and_context().await;
            session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
                codex_code_mode_runtime::InProcessCodeModeSessionProvider,
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
                        let started = session
                            .services
                            .code_mode_service
                            .execute(codex_code_mode::ExecuteRequest {
                                state_path: None,
                                tool_call_id: "outer".into(),
                                enabled_tools: Vec::new().into(),
                                source: "await yield_control(); text('captured result');".into(),
                                yield_time_ms: None,
                                max_output_tokens: None,
                                default_tool_timeout_ms: None,
                            })
                            .await
                            .unwrap();
                        let cell = started.cell_id.clone();
                        session
                            .services
                            .code_mode_service
                            .record_cell_parent_call_id(&cell, "outer");
                        session
                            .services
                            .code_mode_service
                            .mark_cell_ready_for_dispatch(&cell);
                        assert!(matches!(
                            started.initial_response().await.unwrap(),
                            codex_code_mode::RuntimeResponse::ExplicitYield { .. }
                        ));
                        super::super::wait_handler::CodeModeWaitHandler
                            .handle(ToolInvocation {
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
                                    arguments: serde_json::json!({"cell_id": cell.as_str()})
                                        .to_string(),
                                },
                            })
                            .await
                            .unwrap()
                            .log_preview()
                    } else {
                        let handler = CodeModeExecuteHandler::new(
                            super::super::execute_spec::create_code_mode_tool(
                                true,
                                false,
                                &[],
                                &[],
                            ),
                            Vec::new(),
                            Vec::new(),
                        )
                        .unwrap();
                        let output = handler
                            .execute(
                                session,
                                step,
                                "exec".into(),
                                "text('captured result');".into(),
                                cancellation,
                            )
                            .await
                            .unwrap();
                        boxed_tool_output(output).log_preview()
                    }
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !session.services.elicitations.has_waiters_for_test() {
                    assert!(
                        !pending.is_finished(),
                        "handler must reach the final delivery gate"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("completed cell reaches elicitation gate");
            assert!(!pending.is_finished());
            cancellation.cancel();
            let output = tokio::time::timeout(std::time::Duration::from_secs(2), pending)
                .await
                .expect("cancellation releases delivery")
                .unwrap();
            assert!(output.contains("captured result"), "{output}");
            assert_eq!(
                leases.active_count(),
                1,
                "cancellation must not close the dialog"
            );
            assert!(
                !session
                    .services
                    .code_mode_service
                    .dispatch_broker
                    .has_waitable_cells()
            );
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
        let schema = crate::session::turn_context::PreparedOutputSchema::prepare(
            serde_json::json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false})).unwrap();
        let json = response(vec![FunctionCallOutputContentItem::InputText {
            text: r#"{"count":2}"#.into(),
        }]);
        assert_eq!(
            schema_validated_delivery(&json, 100, Some(&schema)).as_deref(),
            Ok(r#"{"count":2}"#)
        );
        assert_eq!(
            schema_validated_delivery(
                &json,
                100,
                Some(
                    &crate::session::turn_context::PreparedOutputSchema::prepare(
                        serde_json::json!({"type":"array"})
                    )
                    .unwrap()
                )
            )
            .unwrap_err()["category"],
            "schema_mismatch"
        );
        let image = response(vec![FunctionCallOutputContentItem::InputImage {
            image_url: "https://example.com/result.png".into(),
            detail: None,
        }]);
        assert_eq!(
            schema_validated_delivery(&image, 100, None).as_deref(),
            Ok("![image](<https://example.com/result.png>)")
        );
        assert_eq!(
            schema_validated_delivery(&image, 100, Some(&schema)).unwrap_err()["category"],
            "invalid_json"
        );
        for image_url in [
            "javascript:alert(1)",
            "relative.png",
            "https://example.com/image.png> injected",
            "https://example.com/image.png\nextra",
            "data:text/html,unsafe",
        ] {
            let image = response(vec![FunctionCallOutputContentItem::InputImage {
                image_url: image_url.into(),
                detail: None,
            }]);
            assert_eq!(
                schema_validated_delivery(&image, 100, None).unwrap_err()["category"],
                "unsupported_media",
                "{image_url:?}"
            );
        }
        assert_eq!(
            schema_validated_delivery(&json, 0, None).unwrap_err()["category"],
            "output_budget"
        );
        assert_eq!(
            schema_validated_delivery(&response(Vec::new()), 100, None).unwrap_err()["category"],
            "empty_output"
        );
        let bad = response(vec![FunctionCallOutputContentItem::InputText {
            text: r#"{"count":"bad"}"#.into(),
        }]);
        assert_eq!(
            schema_validated_delivery(&bad, 100, Some(&schema)).unwrap_err()["instance_pointer"],
            "/count"
        );
        let mut output = FunctionToolOutput::from_text("partial".into(), Some(true))
            .with_sampling_request_signal(serde_json::json!({}));
        output.essential_inline.insert(
            super::super::VISIBLE_OUTPUT_TRUNCATED_KEY.into(),
            true.into(),
        );
        attach_delivery_decision(&mut output, Ok(Some("must not deliver".into())));
        assert_eq!(
            output.essential_inline["delivery_refused"]["category"],
            "output_truncated"
        );
        assert!(
            output
                .sampling_request_signal
                .unwrap()
                .get("explicit_completion_message")
                .is_none()
        );
    }

    #[test]
    fn long_poll_timeout_does_not_extend_ordinary_nested_tools() {
        let timeouts = HashMap::from([(ToolName::plain("mcp_test"), 123_000)]);
        assert_eq!(
            nested_tool_timeout_override(&ToolName::plain("shell_command"), &timeouts, 5_000),
            Some(315_000)
        );
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
        assert!(
            !definition
                .description
                .contains("Authoritative input_schema")
        );
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
        let ToolSpec::Function(tool) = &mut nested else {
            panic!("function tool");
        };
        tool.parameters
            .properties
            .as_mut()
            .unwrap()
            .get_mut("chars")
            .unwrap()
            .description = Some(format!(
            "{}Only use an authorized handle.",
            "x".repeat(150_000)
        ));
        let authoritative = serde_json::to_string(&tool.parameters).unwrap();
        let handler =
            CodeModeExecuteHandler::new(nested.clone(), vec![nested], Vec::new()).unwrap();
        let definition = &handler.enabled_tools[0];
        assert!(definition.input_schema.is_none());
        assert!(
            definition
                .description
                .contains("Authoritative input_schema")
        );
        // Value serialization uses a sorted map; compare JSON rather than order.
        let raw = definition
            .description
            .split_once("```json\n")
            .unwrap()
            .1
            .split_once("\n```")
            .unwrap()
            .0;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(raw).unwrap(),
            serde_json::from_str::<serde_json::Value>(&authoritative).unwrap()
        );
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

    struct ObservationFailureSession {
        initial_fails: bool,
        observation_fails: std::sync::atomic::AtomicBool,
    }

    const OBSERVATION_CAPACITY_ERROR: &str =
        "code-mode pending observation capacity reached; resume an existing cell";

    impl codex_code_mode::CodeModeSession for ObservationFailureSession {
        fn execute<'a>(
            &'a self,
            _request: codex_code_mode::ExecuteRequest,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::StartedCell>
        {
            let initial_fails = self.initial_fails;
            Box::pin(async move {
                let cell_id = CellId::new("observation-failure-cell".into());
                let response_cell_id = cell_id.clone();
                Ok(codex_code_mode::StartedCell::from_result_future(
                    cell_id,
                    Box::pin(async move {
                        if initial_fails {
                            Err(OBSERVATION_CAPACITY_ERROR.into())
                        } else {
                            Ok(codex_code_mode::RuntimeResponse::Yielded {
                                cell_id: response_cell_id,
                                content_items: Vec::new(),
                            })
                        }
                    }),
                ))
            })
        }

        fn wait<'a>(
            &'a self,
            request: codex_code_mode::WaitRequest,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome>
        {
            Box::pin(async move {
                if self.observation_fails.load(std::sync::atomic::Ordering::Acquire) {
                    Err(OBSERVATION_CAPACITY_ERROR.into())
                } else {
                    Ok(codex_code_mode::WaitOutcome::LiveCell(
                        codex_code_mode::RuntimeResponse::Result {
                            cell_id: request.cell_id,
                            content_items: Vec::new(),
                            error_text: None,
                            output_loss: None,
                        },
                    ))
                }
            })
        }

        fn terminate<'a>(
            &'a self,
            cell_id: CellId,
        ) -> codex_code_mode::CodeModeSessionResultFuture<'a, codex_code_mode::WaitOutcome>
        {
            Box::pin(async move {
                if self.observation_fails.load(std::sync::atomic::Ordering::Acquire) {
                    Err(OBSERVATION_CAPACITY_ERROR.into())
                } else {
                    Ok(codex_code_mode::WaitOutcome::LiveCell(
                        codex_code_mode::RuntimeResponse::Terminated {
                            cell_id,
                            content_items: Vec::new(),
                        },
                    ))
                }
            })
        }

        fn shutdown<'a>(&'a self) -> codex_code_mode::CodeModeSessionResultFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    struct ObservationFailureProvider(Arc<ObservationFailureSession>);

    impl codex_code_mode::CodeModeSessionProvider for ObservationFailureProvider {
        fn create_session<'a>(
            &'a self,
            _delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
        ) -> codex_code_mode::CodeModeSessionProviderFuture<'a> {
            Box::pin(
                async move { Ok(Arc::clone(&self.0) as Arc<dyn codex_code_mode::CodeModeSession>) },
            )
        }
    }

    #[tokio::test]
    async fn observation_failures_preserve_dispatch_until_confirmed_terminal_recovery() {
        // Exercise both exec observation owners: the initial receiver and the
        // held wait after automatic yield. Refusals are injected, not timed.
        for initial_fails in [false, true] {
            let runtime = Arc::new(ObservationFailureSession {
                initial_fails,
                observation_fails: std::sync::atomic::AtomicBool::new(true),
            });
            let (mut session, turn) = crate::session::tests::make_session_and_context().await;
            session.services.code_mode_service = super::super::CodeModeService::new(Arc::new(
                ObservationFailureProvider(Arc::clone(&runtime)),
            ));
            let session = Arc::new(session);
            let step = crate::session::step_context::StepContext::for_test(Arc::new(turn));
            let handler = CodeModeExecuteHandler::new(
                super::super::execute_spec::create_code_mode_tool(true, false, &[], &[]),
                Vec::new(),
                Vec::new(),
            )
            .unwrap();
            let output = handler
                .execute(
                    Arc::clone(&session),
                    Arc::clone(&step),
                    "outer-observation-call".into(),
                    "text('not replayed');".into(),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
            assert_eq!(output.essential_inline["cell_execution_state"], "unknown");

            let service = &session.services.code_mode_service;
            let cell_id = CellId::new("observation-failure-cell".into());
            assert!(service.dispatch_broker.has_waitable_cells());
            assert_eq!(
                service.cell_parent_call_id(&cell_id).as_deref(),
                Some("outer-observation-call")
            );
            // Dispatch admission must still work after the observation error.
            // Its required outcome must survive subsequent uncertain packets.
            let pending = service.begin_packet_call(&cell_id).expect("live dispatch admission");
            service.complete_packet_call(
                &cell_id,
                pending,
                Vec::new(),
                None,
                Some((
                    crate::tools::context::RequiredToolTerminalCause::Failure,
                    "REQUIRED_AFTER_OBSERVATION_FAILURE".into(),
                )),
            );

            for (call_id, terminate) in [("wait-refused", false), ("terminate-refused", true)] {
                let output = super::super::CodeModeWaitHandler
                    .handle(ToolInvocation {
                        session: Arc::clone(&session),
                        step_context: Arc::clone(&step),
                        cancellation_token: tokio_util::sync::CancellationToken::new(),
                        tracker: Arc::new(tokio::sync::Mutex::new(
                            crate::turn_diff_tracker::TurnDiffTracker::new(),
                        )),
                        call_id: call_id.into(),
                        tool_name: ToolName::plain(super::super::WAIT_TOOL_NAME),
                        source: crate::tools::router::ToolCallSource::Direct,
                        payload: ToolPayload::Function {
                            arguments: serde_json::json!({
                                "cell_id": cell_id.as_str(), "terminate": terminate,
                            }).to_string(),
                        },
                    })
                    .await
                    .unwrap();
                assert!(output.log_preview().contains(OBSERVATION_CAPACITY_ERROR));
                assert!(output.sampling_request_signal().as_ref().is_none_or(|signal| {
                    signal.get("authoritative_wait_owner_v1").is_none()
                }));
                assert!(service.dispatch_broker.has_waitable_cells());
                assert_eq!(
                    service.cell_parent_call_id(&cell_id).as_deref(),
                    Some("outer-observation-call")
                );
                assert!(service.packet_admission.lock().unwrap().cells[cell_id.as_str()]
                    .first_required_terminal.is_some());
            }

            runtime.observation_fails.store(false, std::sync::atomic::Ordering::Release);
            let output = super::super::CodeModeWaitHandler
                .handle(ToolInvocation {
                    session: Arc::clone(&session),
                    step_context: step,
                    cancellation_token: tokio_util::sync::CancellationToken::new(),
                    tracker: Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    call_id: "terminal-recovery".into(),
                    tool_name: ToolName::plain(super::super::WAIT_TOOL_NAME),
                    source: crate::tools::router::ToolCallSource::Direct,
                    payload: ToolPayload::Function {
                        arguments: serde_json::json!({
                            "cell_id": cell_id.as_str(), "terminate": initial_fails,
                        }).to_string(),
                    },
                })
                .await
                .unwrap();
            let signal = output.sampling_request_signal().unwrap();
            assert_eq!(signal["outcome"], "failure");
            assert_eq!(
                signal["authoritative_wait_owner_v1"]["state_revision"],
                if initial_fails { "terminated" } else { "completed" }
            );
            assert!(!service.dispatch_broker.has_waitable_cells());
            assert_eq!(service.cell_parent_call_id(&cell_id), None);
            assert_eq!(service.begin_packet_call(&cell_id), None);
            service.shutdown().await.unwrap();
        }
    }

    #[test]
    fn observation_failure_does_not_publish_a_terminal_cell_item() {
        let cell_id = CellId::new("uncertain-cell".into());
        let response = super::super::observation_error_response(
            &cell_id,
            OBSERVATION_CAPACITY_ERROR.into(),
        );
        let item = super::super::failed_code_mode_cell_item(
            "outer-call",
            &response,
            std::time::Duration::ZERO,
        ).unwrap();
        assert_eq!(item.status, codex_protocol::items::DynamicToolCallStatus::InProgress);
        assert_eq!(item.success, None);
        assert_eq!(item.arguments["state"], "in_progress");
        assert!(item.error.as_deref().unwrap().contains(OBSERVATION_CAPACITY_ERROR));
    }

}
