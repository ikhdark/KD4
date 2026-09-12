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
    enabled_tools: Vec<codex_code_mode::ToolDefinition>,
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

    fn keep_open(&self) {
        self.state
            .keep_open
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) async fn record_trace(&self, record: impl FnOnce() + Send + 'static) {
        // Accepted trace work retains the same cleanup owner if its waiter is dropped.
        let lease = self.clone();
        let runtime = tokio::runtime::Handle::current();
        if let Err(error) = self
            .state
            .session
            .terminal_tasks
            .spawn_blocking_on(
                move || {
                    record();
                    drop(lease);
                },
                &runtime,
            )
            .await
        {
            tracing::warn!(%error, "code mode trace recording task failed");
        }
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
        let mut by_global_name = HashMap::<String, ToolName>::with_capacity(enabled_tools.len());
        for definition in &enabled_tools {
            let global_name = codex_code_mode::normalize_code_mode_identifier(&definition.name);
            if let Some(existing) = by_global_name.get(&global_name) {
                return Err(format!(
                    "code mode tool identifier collision: `{existing}` and `{}` both normalize to `{global_name}`",
                    definition.tool_name
                ));
            }
            by_global_name.insert(global_name, definition.tool_name.clone());
        }
        // The rendered descriptions already contain the schema-derived TypeScript
        // declarations. The isolate consumes only callable metadata, so do not
        // clone and transport the original JSON schema trees for every cell.
        for definition in &mut enabled_tools {
            definition.input_schema = None;
            definition.output_schema = None;
        }
        Ok(Self {
            spec,
            enabled_tools,
        })
    }

    async fn execute(
        &self,
        session: std::sync::Arc<crate::session::session::Session>,
        turn: std::sync::Arc<crate::session::turn_context::TurnContext>,
        call_id: String,
        code: String,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        let args =
            codex_code_mode::parse_exec_source(&code).map_err(FunctionCallError::RespondToModel)?;
        let exec = ExecContext { session, turn };
        let started_at = std::time::Instant::now();
        let started_cell = exec
            .session
            .services
            .code_mode_service
            .execute(codex_code_mode::ExecuteRequest {
                tool_call_id: call_id.clone(),
                enabled_tools: self.enabled_tools.clone(),
                source: args.code.to_owned(),
                // Give ordinary awaited cells the runtime's completion budget.
                // If that budget expires, the owner takes over and waits for a
                // material state change without another model-mediated poll.
                yield_time_ms: None,
                max_output_tokens: args.max_output_tokens,
            })
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let cell_id = started_cell.cell_id.clone();
        let runtime_cell_id = cell_id.to_string();
        exec.session
            .services
            .code_mode_service
            .record_cell_parent_call_id(&cell_id, &call_id);
        // Establish cleanup ownership before the first trace await.
        let dispatch_lease = CellDispatchLease::new(Arc::clone(&exec.session), cell_id.clone());
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
            dispatch_lease
                .record_trace(move || trace.record_started(parent_call_id, source))
                .await;
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
            .subscribe_activity(turn_state.as_deref())
            .await;
        // Consume the immediate initial observation before making the held
        // wait steerable. This clears the runtime's initial observer, so
        // steering cannot leave a stale observer that rejects a later wait.
        let initial_response = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                terminate_interrupted_cell(&exec, &cell_id, dispatch_lease.clone()).await;
                return Err(FunctionCallError::RespondToModel("exec cancelled".to_string()));
            }
            response = started_cell.initial_response() => {
                response.map_err(FunctionCallError::RespondToModel)?
            }
        };
        let initial_is_empty = matches!(
            &initial_response,
            codex_code_mode::RuntimeResponse::Yielded { content_items, .. }
                if content_items.is_empty()
        );
        let (response, live_cell, drained_observations) = if initial_is_empty {
            let held = hold_until_state_change(
                || {
                    exec.session
                        .services
                        .code_mode_service
                        .wait_for_state_change(cell_id.clone())
                },
                &cancellation_token,
                activity_rx,
                pending_activity,
                "exec cancelled",
            )
            .await;
            let held = match held {
                Ok(held) => held,
                Err(mut error) => {
                    error.drained_observations = error.drained_observations.saturating_add(1);
                    record_internally_drained_waits(&exec, error.drained_observations);
                    if error.timed_out || cancellation_token.is_cancelled() {
                        terminate_interrupted_cell(&exec, &cell_id, dispatch_lease.clone()).await;
                    }
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
            dispatch_lease
                .record_trace(move || {
                    code_cell_trace.record_initial_response(&traced_response);
                    if live_cell && !keep_dispatch_open {
                        code_cell_trace.record_ended(&traced_response);
                    }
                })
                .await;
        }
        exec.session.services.elicitations.wait_until_clear().await;
        emit_failed_code_mode_cell_item(&exec, &call_id, &response, started_at).await;
        let output = handle_runtime_response(&exec, response, args.max_output_tokens, started_at)
            .map_err(FunctionCallError::RespondToModel)?;
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
        let turn = Arc::clone(&step_context.turn);

        match payload {
            ToolPayload::Custom { input } if is_exec_tool_name(&tool_name) => self
                .execute(session, turn, call_id, input, cancellation_token)
                .await
                .map(boxed_tool_output),
            _ => Err(FunctionCallError::RespondToModel(format!(
                "{PUBLIC_TOOL_NAME} expects raw JavaScript source text"
            ))),
        }
    }
}

impl CoreToolRuntime for CodeModeExecuteHandler {
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
    fn cached_runtime_catalog_drops_schema_trees_after_rendering_descriptions() {
        let nested = crate::tools::handlers::shell_spec::create_write_stdin_tool();
        let handler = CodeModeExecuteHandler::new(nested.clone(), vec![nested], Vec::new())
            .expect("build code mode handler");

        assert_eq!(handler.enabled_tools.len(), 1);
        let definition = &handler.enabled_tools[0];
        assert!(definition.input_schema.is_none());
        assert!(definition.output_schema.is_none());
        assert!(definition.description.contains("exec tool declaration:"));
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

        drop(CellDispatchLease::new(
            Arc::clone(&session),
            cell_id.clone(),
        ));

        assert_eq!(service.cell_parent_call_id(&cell_id), None);
        assert!(!service.dispatch_broker.has_waitable_cells());
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
    async fn registered_code_cells_persist_ordered_initial_and_terminal_traces()
    -> anyhow::Result<()> {
        use crate::session::step_context::StepContext;
        use crate::tools::context::ToolDispatchState;
        use crate::tools::router::{ToolCall, ToolCallSource, ToolRouter, ToolRouterParams};
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
                "text('first'); await yield_control(); await new Promise(() => {});",
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
                let result = router.dispatch_tool_call_with_terminal_outcome(
                    Arc::clone(&session), Arc::clone(&step), tokio_util::sync::CancellationToken::new(), Arc::clone(&tracker),
                    ToolCall { tool_name: codex_tools::ToolName::plain("wait"), call_id: format!("wait-{case}"),
                        payload: ToolPayload::Function { arguments: serde_json::json!({"cell_id":cell_id,"terminate":case == "terminated"}).to_string() } },
                    ToolCallSource::Direct, state,
                ).await?;
                if case == "waited" {
                    assert!(result.result.log_preview().contains("second"));
                }
            }
            assert_eq!(
                session
                    .services
                    .code_mode_service
                    .cell_parent_call_id(&runtime_id),
                None
            );
            let events = read_events()?;
            let mut sequence = Vec::new();
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
                                content_items,
                                ..
                            } if case != "completed" => content_items,
                            other => panic!("unexpected initial response: {other:?}"),
                        };
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
                        let content_items = match response {
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
                    _ => {}
                }
            }
            assert_eq!(sequence, vec!["started", "initial", "ended"]);
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
    async fn cancelled_code_cell_trace_waiter_retains_cleanup_until_write_finishes()
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
        let waiter = tokio::spawn(async move {
            lease
                .record_trace(move || {
                    started_tx.send(()).expect("recording caller still waits");
                    release_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("release writer");
                    std::fs::write(write_path, "accepted trace write")
                        .expect("persist actual trace work");
                })
                .await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx).await??;
        waiter.abort();
        assert!(waiter.await.expect_err("caller canceled").is_cancelled());
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
            "accepted trace write"
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
