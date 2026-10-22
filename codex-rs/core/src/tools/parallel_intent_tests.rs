use super::*;
use crate::session::TurnInput;
use crate::state::TurnState;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_tools::ToolExecutor;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

struct EffectHandler {
    path: std::path::PathBuf,
    entries: Arc<AtomicUsize>,
    hook: bool,
    queued: Arc<Notify>,
}

impl ToolExecutor<ToolInvocation> for EffectHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        codex_tools::ToolName::plain("intent_effect")
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "intent_effect".into(),
            description: "Fixture effect".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.entries.fetch_add(1, Ordering::SeqCst);
            std::fs::write(&self.path, "handler entered").unwrap();
            Ok(crate::tools::context::boxed_tool_output(
                crate::tools::context::FunctionToolOutput::from_text("ok".into(), Some(true)),
            ))
        })
    }
}

impl CoreToolRuntime for EffectHandler {
    fn prepare_invocation<'a>(
        &'a self,
        invocation: ToolInvocation,
    ) -> futures::future::BoxFuture<'a, Result<ToolInvocation, FunctionCallError>> {
        Box::pin(async move {
            if self.hook {
                let path = self.path.clone();
                let entries = Arc::clone(&self.entries);
                self.queued.notify_one();
                run_workspace_hook(
                    Arc::clone(&invocation.session),
                    Arc::clone(&invocation.step_context.turn),
                    Arc::clone(&invocation.tracker),
                    invocation.cancellation_token.clone(),
                    async move {
                        entries.fetch_add(1, Ordering::SeqCst);
                        std::fs::write(path, "hook entered").unwrap();
                    },
                )
                .await?;
            }
            Ok(invocation)
        })
    }
}

fn nested_source() -> ToolCallSource {
    ToolCallSource::CodeMode {
        cell_id: "authored-cell".into(),
        parent_call_id: Some("outer".into()),
        runtime_tool_call_id: "effect".into(),
        nested_deadline: Some(std::time::Instant::now() + Duration::from_secs(10)),
        cancellation_cause: None,
    }
}

fn effect_call(id: &str) -> ToolCall {
    ToolCall {
        tool_name: codex_tools::ToolName::plain("intent_effect"),
        call_id: id.into(),
        payload: ToolPayload::Function {
            arguments: "{}".into(),
        },
    }
}

#[tokio::test]
async fn steering_fences_dequeued_effects_and_hooks_after_workspace_wait() {
    for hook in [false, true] {
        for steer in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("effect.txt");
            let entries = Arc::new(AtomicUsize::new(0));
            let queued = Arc::new(Notify::new());
            let (session, turn) = crate::session::tests::make_session_and_context().await;
            let session = Arc::new(session);
            let turn = Arc::new(turn);
            let router = Arc::new(ToolRouter::from_parts(
                ToolRegistry::from_tools([Arc::new(EffectHandler {
                    path: path.clone(),
                    entries: Arc::clone(&entries),
                    hook,
                    queued: Arc::clone(&queued),
                }) as Arc<dyn CoreToolRuntime>]),
                Vec::new(),
            ));
            let step = StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(router);
            let runtime = ToolCallRuntime::new(
                Arc::clone(&session),
                step,
                Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            )
            .with_code_mode_user_intent("authored-cell", 0);
            let writer = Arc::clone(&runtime.parallel_execution).write_owned().await;
            let pending = tokio::sync::Mutex::new(TurnState::default());
            let mut result = tokio::spawn(runtime.clone().handle_tool_call_with_source(
                effect_call("queued-effect"),
                nested_source(),
                CancellationToken::new(),
            ));
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = &mut result => {
                        let outcome = result.unwrap().map(|output| output.result.log_preview());
                        panic!("dispatch ended before held lease: hook={hook}, steer={steer}: {outcome:?}");
                    }
                    _ = async {
                        if hook {
                            queued.notified().await;
                        } else {
                            while turn.turn_timing_state.lifecycle_context().parallel_gate_waiter_count == 0 {
                                tokio::task::yield_now().await;
                            }
                        }
                    } => {}
                }
            })
            .await
            .expect("real dispatch must reach its held lease");
            if steer {
                session
                    .input_queue
                    .extend_pending_input_for_turn_state(
                        &pending,
                        &[TurnInput::AdditionalContext(Default::default())],
                    )
                    .await
                    .unwrap();
            }
            drop(writer);
            let result = tokio::time::timeout(Duration::from_secs(5), result)
                .await
                .unwrap()
                .unwrap();
            if steer {
                let error = result
                    .err()
                    .expect("stale effect must not enter")
                    .to_string();
                assert!(error.contains("input_changed_decision_required"), "{error}");
                assert_eq!(entries.load(Ordering::SeqCst), 0);
                assert!(!path.exists());
                // Observing/consuming input does not reauthorize the old cell.
                session
                    .input_queue
                    .take_pending_for_state(Some(&pending), true)
                    .await;
                assert!(
                    runtime
                        .clone()
                        .handle_tool_call_with_source(
                            effect_call("still-old"),
                            nested_source(),
                            CancellationToken::new()
                        )
                        .await
                        .is_err()
                );
                let current = runtime.with_code_mode_user_intent("new-cell", 1);
                current
                    .handle_tool_call_with_source(
                        effect_call("new-decision"),
                        nested_source(),
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap();
            } else {
                result.unwrap();
            }
            assert_eq!(entries.load(Ordering::SeqCst), if hook { 2 } else { 1 });
            assert_eq!(std::fs::read_to_string(path).unwrap(), "handler entered");
            assert!(
                Arc::clone(&turn.workspace_execution_coordinator.parallel_execution)
                    .try_write_owned()
                    .is_ok(),
                "no leaked lease"
            );
        }
    }
}
