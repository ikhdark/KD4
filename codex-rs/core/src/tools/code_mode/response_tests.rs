use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem as RuntimeContentItem;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_tools::ToolExecutor;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;

// Controlled nested results cross the real JS runtime, broker and tool router.
// Tests below never manufacture packet entries or required-terminal metadata.
struct PacketTestTool {
    name: &'static str,
}

impl ToolExecutor<ToolInvocation> for PacketTestTool {
    fn tool_name(&self) -> codex_tools::ToolName {
        codex_tools::ToolName::plain(self.name)
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: self.name.to_string(),
            description: "Controlled packet regression result.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = invocation.payload else {
                panic!("nested function dispatch must preserve its payload kind");
            };
            let args: serde_json::Value = serde_json::from_str(&arguments).unwrap();
            if let Some(status) = args["terminal_status"].as_u64() {
                return Ok(crate::tools::context::boxed_tool_output(
                    FunctionToolOutput::from_text(
                        format!("request failed with status {status}"),
                        Some(false),
                    )
                    .with_sampling_request_signal(serde_json::json!({"retryable": false})),
                ));
            }

            if let Some(error) = args["error"]
                .as_str()
                .or_else(|| match args["cmd"].as_str() {
                    Some("Remove-Item output.txt") => Some("write rejected"),
                    Some("git log -1") => Some("history unavailable"),
                    Some("Get-Content missing.txt") => Some("file missing"),
                    Some("git status --short") => Some("status unavailable"),
                    _ => None,
                })
            {
                return Err(crate::FunctionCallError::RespondToModel(error.to_string()));
            }
            let output = FunctionToolOutput::from_text("READ_RESULT_42".to_string(), Some(true));
            let output = match args["outcome"].as_str() {
                Some("blocked") => output.with_skip_disposition(
                    codex_tools::ToolOutputSkipDisposition::BlockingRequiredOperation,
                ),
                Some("timeout") => output.with_outcome(ToolOutputOutcome::TimedOut),
                Some("failure") => output.with_outcome(ToolOutputOutcome::Failure),
                _ => output,
            };
            Ok(crate::tools::context::boxed_tool_output(output))
        })
    }
}

impl crate::tools::registry::CoreToolRuntime for PacketTestTool {}

struct PacketRuntime {
    session: Arc<crate::session::session::Session>,
    step: Arc<crate::session::step_context::StepContext>,
    tracker: crate::tools::context::SharedTurnDiffTracker,
    execute: super::execute_handler::CodeModeExecuteHandler,
    signals: crate::session::turn_execution::SamplingRequestSignalCollector,
    _worker: super::delegate::CodeModeDispatchWorker,
}

impl PacketRuntime {
    async fn new() -> Self {
        Self::with_nested_tool("read_tool_output").await
    }

    async fn with_nested_tool(name: &'static str) -> Self {
        let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
        session.services.code_mode_service = super::CodeModeService::new(Arc::new(
            codex_code_mode::InProcessCodeModeSessionProvider,
        ));
        turn.model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
        let session = Arc::new(session);
        let nested: Arc<dyn crate::tools::registry::CoreToolRuntime> =
            Arc::new(PacketTestTool { name });
        let execute = super::execute_handler::CodeModeExecuteHandler::new(
            super::execute_spec::create_code_mode_tool(false, false, &[], &[]),
            vec![nested.spec()],
            Vec::new(),
        )
        .unwrap();
        let router = Arc::new(crate::tools::router::ToolRouter::from_parts(
            crate::tools::registry::ToolRegistry::from_tools([nested]),
            Vec::new(),
        ));
        let step = crate::session::step_context::StepContext::for_test(Arc::new(turn))
            .with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        ));
        let signals: crate::session::turn_execution::SamplingRequestSignalCollector =
            Default::default();
        let worker = session
            .services
            .code_mode_service
            .start_turn_worker(
                &session,
                Arc::clone(&step),
                Arc::clone(&tracker),
                signals.clone(),
            )
            .expect("code-mode turn must start its normal dispatch worker");
        Self {
            session,
            step,
            tracker,
            execute,
            signals,
            _worker: worker,
        }
    }

    async fn call(
        &self,
        payload: ToolPayload,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> Result<Box<dyn ToolOutput>, crate::FunctionCallError> {
        let is_exec = matches!(&payload, ToolPayload::Custom { .. });
        let invocation = ToolInvocation {
            session: Arc::clone(&self.session),
            step_context: Arc::clone(&self.step),
            tracker: Arc::clone(&self.tracker),
            cancellation_token,
            call_id: if is_exec {
                "packet-exec"
            } else {
                "packet-wait"
            }
            .to_string(),
            tool_name: codex_tools::ToolName::plain(if is_exec { "exec" } else { "wait" }),
            source: crate::tools::router::ToolCallSource::Direct,
            payload,
        };
        tokio::time::timeout(Duration::from_secs(20), async {
            if is_exec {
                self.execute.handle(invocation).await
            } else {
                super::wait_handler::CodeModeWaitHandler
                    .handle(invocation)
                    .await
            }
        })
        .await
        .expect("packet regression must finish without polling")
    }

    async fn exec(&self, source: &str) -> Box<dyn ToolOutput> {
        self.call(
            ToolPayload::Custom {
                input: source.to_string(),
            },
            Default::default(),
        )
        .await
        .unwrap()
    }

    fn live_cell(&self) -> CellId {
        let cells = self
            .session
            .services
            .code_mode_service
            .packet_admission
            .lock()
            .unwrap();
        assert_eq!(cells.cells.len(), 1, "exactly one live JS cell");
        CellId::new(cells.cells.keys().next().unwrap().clone())
    }

    async fn wait(&self, cell: &CellId) -> Box<dyn ToolOutput> {
        self.call(
            ToolPayload::Function {
                arguments: serde_json::json!({"cell_id": cell.as_str(), "max_tokens": 200})
                    .to_string(),
            },
            Default::default(),
        )
        .await
        .unwrap()
    }

    async fn finish(self) {
        self.session
            .services
            .code_mode_service
            .shutdown()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn discarded_runtime_output_remains_machine_readable_after_projection() {
    let runtime = PacketRuntime::new().await;
    let output = runtime
        .exec("// @exec: {\"max_output_tokens\": 1}\ntext('x'.repeat(64 * 1024 * 1024 + 1));")
        .await;
    let rendered = packet_output_text(output.as_ref());
    assert!(rendered.contains("Script completed"), "{rendered}");
    let metadata = rendered
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("output_complete").is_some())
        .expect("structured output completeness");
    assert_eq!(metadata["output_complete"], false);
    assert_eq!(metadata["discarded_output_recoverable"], false);
    assert_eq!(metadata["output_loss"]["discarded_items"], 1);
    assert_eq!(
        metadata["output_loss"]["discarded_bytes_lower_bound"],
        64 * 1024 * 1024 + 1
    );
    runtime.finish().await;
}

fn packet_output_text(output: &dyn ToolOutput) -> String {
    let response = output.to_response_item(
        "packet-output",
        &ToolPayload::Custom {
            input: String::new(),
        },
    );
    let codex_protocol::models::ResponseInputItem::CustomToolCallOutput { output, .. } = response
    else {
        panic!("exec projection must produce a custom tool output");
    };
    output.body.to_text().unwrap()
}

#[tokio::test]
async fn nested_failures_preserve_only_observed_workspace_dependencies() {
    use crate::tool_history::SourceDependencyV1;
    use std::collections::BTreeSet;

    for (input, added_dependency, unscoped) in [
        (serde_json::json!(42), None, false),
        (
            serde_json::json!({"cmd": "Remove-Item output.txt"}),
            None,
            false,
        ),
        (serde_json::json!({"cmd": "git log -1"}), None, false),
        (
            serde_json::json!({"cmd": "Get-Content missing.txt"}),
            Some("missing.txt"),
            false,
        ),
        (serde_json::json!({"cmd": "git status --short"}), None, true),
    ] {
        let runtime = PacketRuntime::with_nested_tool("exec_command").await;
        let output = runtime
            .exec(&format!(
                "await tools.exec_command({{cmd: 'Get-Content source.txt'}}); try {{ await tools.exec_command({input}); }} catch {{}}"
            ))
            .await;
        let visible = packet_output_text(output.as_ref());
        assert_eq!(
            output.outcome_for_logging(),
            ToolOutputOutcome::Success,
            "caught input {input}: {visible}"
        );
        assert!(visible.contains("READ_RESULT_42"), "{visible}");
        let expected_error = match input["cmd"].as_str() {
            Some("Remove-Item output.txt") => "write rejected",
            Some("git log -1") => "history unavailable",
            Some("Get-Content missing.txt") => "file missing",
            Some("git status --short") => "status unavailable",
            _ => "expects a JSON object for arguments",
        };
        assert!(visible.contains(expected_error), "{visible}");

        let cell_id = output
            .deterministic_continuation_owner_key()
            .expect("completed cell retains its dependency owner");
        let mut expected = BTreeSet::from([SourceDependencyV1::new(
            &runtime.step.turn.config.cwd.join("source.txt"),
            false,
        )]);
        if let Some(path) = added_dependency {
            expected.insert(SourceDependencyV1::new(
                &runtime.step.turn.config.cwd.join(path),
                false,
            ));
        }
        if unscoped {
            expected.clear();
        }
        assert_eq!(
            runtime.signals.code_mode_source_dependencies(&cell_id),
            Some(expected),
            "failed nested input: {input}"
        );
        runtime.finish().await;
    }
}

#[tokio::test]
async fn exec_reports_omitted_nested_fallback_results() {
    let runtime = PacketRuntime::new().await;
    for (count, omitted) in [(20, 12), (8, 0)] {
        let source = format!(
            "await Promise.all(Array.from({{length: {count}}}, () => tools.read_tool_output({{}})));"
        );
        let output = runtime.exec(&source).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        let visible = packet_output_text(output.as_ref());
        assert_eq!(visible.matches("READ_RESULT_42").count(), 8, "{visible}");
        if omitted > 0 {
            assert!(
                visible.contains(&format!(
                    "{omitted} additional nested tool results were omitted"
                )),
                "{visible}"
            );
        } else {
            assert!(!visible.contains("results were omitted"), "{visible}");
        }
    }
    let output = runtime
        .exec("await Promise.all(Array.from({length: 20}, () => tools.read_tool_output({}))); text('explicit result');")
        .await;
    let visible = packet_output_text(output.as_ref());
    assert!(visible.contains("explicit result"), "{visible}");
    assert!(!visible.contains("results were omitted"), "{visible}");
    assert!(!visible.contains("READ_RESULT_42"), "{visible}");
    runtime.finish().await;
}

#[tokio::test]
async fn exec_caught_nested_error_allows_successful_fallback() {
    let runtime = PacketRuntime::new().await;
    let output = runtime.exec("try { await tools.read_tool_output({error: 'ordinary failure'}); } catch { text(await tools.read_tool_output({})); }").await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
    assert!(packet_output_text(output.as_ref()).contains("READ_RESULT_42"));
    let output = runtime
        .exec("await tools.read_tool_output({error: 'uncaught failure'});")
        .await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(packet_output_text(output.as_ref()).contains("uncaught failure"));
    for outcome in ["failure", "timeout", "blocked"] {
        let output = runtime.exec(&format!("try {{ await tools.read_tool_output({{outcome: '{outcome}'}}); }} catch {{ text('caught'); }}")).await;
        assert_eq!(
            output.outcome_for_logging(),
            if outcome == "blocked" {
                ToolOutputOutcome::Skipped
            } else {
                ToolOutputOutcome::Success
            }
        );
    }
    assert!(
        runtime
            .session
            .services
            .code_mode_service
            .packet_admission
            .lock()
            .unwrap()
            .cells
            .is_empty()
    );
    runtime.finish().await;
}

#[tokio::test]
async fn wait_propagates_real_nested_terminal_outcomes_and_keeps_owner_evidence() {
    for (outcome, typed) in [
        ("failure", ToolOutputOutcome::Failure),
        ("blocked", ToolOutputOutcome::Skipped),
        ("timeout", ToolOutputOutcome::Failure),
    ] {
        let runtime = PacketRuntime::new().await;
        let initial = runtime
            .exec(&format!(
                "await yield_control(); await tools.read_tool_output({}); text('JS completed');",
                serde_json::json!({"outcome": outcome}),
            ))
            .await;
        assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
        let cell = runtime.live_cell();
        let output = runtime.wait(&cell).await;
        assert_eq!(output.outcome_for_logging(), typed);
        let signal = output.sampling_request_signal().unwrap();
        assert_eq!(
            signal["outcome"],
            if outcome == "timeout" {
                "failure"
            } else {
                outcome
            }
        );
        assert_eq!(
            signal["nested_ordinal"],
            if outcome == "blocked" {
                serde_json::json!(0)
            } else {
                serde_json::Value::Null
            }
        );
        assert_eq!(
            signal["authoritative_wait_owner_v1"]["owner"],
            cell.as_str()
        );
        assert_eq!(
            signal["authoritative_wait_owner_v1"]["state_revision"],
            if outcome == "blocked" {
                "completed"
            } else {
                "failed"
            }
        );
        assert!(
            signal["semantic_evidence"]
                .to_string()
                .contains(if outcome == "blocked" {
                    "required nested tool"
                } else {
                    "Script error"
                })
        );
        assert!(
            signal["failure_signature"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            runtime
                .session
                .services
                .code_mode_service
                .packet_admission
                .lock()
                .unwrap()
                .cells
                .is_empty()
        );
        runtime.finish().await;
    }
}

#[tokio::test]
async fn cancelled_wait_retires_packet_created_by_real_nested_dispatch() {
    let runtime = PacketRuntime::new().await;
    let initial = runtime
        .exec(
            "await tools.read_tool_output({}); await yield_control(); await new Promise(() => {});",
        )
        .await;
    assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert!(initial.log_preview().contains("READ_RESULT_42"));
    let cell = runtime.live_cell();
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    let result = runtime
        .call(
            ToolPayload::Function {
                arguments: serde_json::json!({"cell_id": cell.as_str()}).to_string(),
            },
            cancellation,
        )
        .await;
    assert!(
        matches!(result, Err(crate::FunctionCallError::RespondToModel(ref message)) if message == "wait cancelled")
    );
    let service = &runtime.session.services.code_mode_service;
    assert!(service.packet_admission.lock().unwrap().cells.is_empty());
    assert_eq!(service.cell_parent_call_id(&cell), None);
    assert!(!service.dispatch_broker.has_waitable_cells());
    runtime.finish().await;
}

#[tokio::test]
async fn small_read_results_do_not_inject_batching_instructions() {
    let runtime = PacketRuntime::new().await;
    let output = runtime.exec("await tools.read_tool_output({});").await;
    let first = packet_output_text(output.as_ref());
    assert!(first.contains("READ_RESULT_42"));
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
    assert!(!first.contains("Low-density packet:"));
    assert!(!first.contains("batch only necessary independent reads"));
    for source in [
        "await tools.read_tool_output({});",
        "await tools.read_tool_output({}); await tools.read_tool_output({});",
        "await tools.read_tool_output({});",
    ] {
        let output = runtime.exec(source).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        let visible = packet_output_text(output.as_ref());
        assert!(visible.contains("READ_RESULT_42"));
        assert!(!visible.contains("Low-density packet:"));
    }
    runtime.finish().await;
}

#[tokio::test]
async fn real_nested_calls_keep_registration_order_across_yield_and_wait() {
    let runtime = PacketRuntime::new().await;
    let initial = runtime.exec(
        "await tools.read_tool_output({}); await yield_control(); await tools.read_tool_output({outcome: 'failure'});",
    ).await;
    assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert!(packet_output_text(initial.as_ref()).contains("READ_RESULT_42"));
    let output = runtime.wait(&runtime.live_cell()).await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert_eq!(
        output.sampling_request_signal().unwrap()["outcome"],
        "failure"
    );
    runtime.finish().await;
}

#[tokio::test]
async fn exec_mixed_output_keeps_the_actual_exception_after_a_printed_error_log() {
    let runtime = PacketRuntime::new().await;
    let output = runtime.exec(
        "// @exec: {\"max_output_tokens\": 40}\ntext('Script error:\\nOLD_LOG ' + 'x'.repeat(4000)); image('data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9ZlS8AAAAASUVORK5CYII='); throw new Error('ACTUAL_SCRIPT_FAILURE');",
    ).await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let visible = packet_output_text(output.as_ref());
    assert!(visible.contains("ACTUAL_SCRIPT_FAILURE"), "{visible}");
    assert!(codex_utils_string::approx_token_count(&visible) < 140);
    let canonical = output
        .canonical_result(&ToolPayload::Custom {
            input: String::new(),
        })
        .unwrap();
    let canonical = String::from_utf8(canonical.bytes).unwrap();
    assert!(canonical.contains("OLD_LOG"));
    assert!(canonical.contains("data:image/png;base64,"));
    assert!(canonical.contains("ACTUAL_SCRIPT_FAILURE"));
    runtime.finish().await;
}

#[tokio::test]
async fn nested_status_codes_remain_distinct_in_the_continuation_consumer() {
    use crate::session::turn_execution::SamplingRequestSettledState;
    use crate::session::turn_execution::TurnExecutionControl;
    let mut fingerprints = Vec::new();
    for status in [403, 404, 403] {
        let runtime = PacketRuntime::new().await;
        let source = format!(
            "try {{ await tools.read_tool_output({}); }} catch {{}}",
            serde_json::json!({"terminal_status": status}),
        );
        let payload = ToolPayload::Custom {
            input: source.clone(),
        };
        let registration = runtime.signals.register_deterministic_tool_call(
            &codex_tools::ToolName::plain("exec"),
            &payload,
            "packet-exec",
        );
        let output = runtime.exec(&source).await;
        runtime.signals.record_response_result(
            registration.ordinal,
            output.outcome_context(),
            output.sampling_request_signal(),
            &output.to_response_item("packet-exec", &payload),
            false,
        );
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        let control = TurnExecutionControl::new();
        let request = control.continuation_generation_request(
            &control.baselines(0),
            &runtime.signals,
            &SamplingRequestSettledState {
                mutation_revision: 0,
                tool_exposure_revision: 0,
            },
            false,
        );
        fingerprints.push(
            request
                .failure_fingerprint
                .expect("nested failure reaches the continuation consumer"),
        );
        runtime.finish().await;
    }
    assert_ne!(
        fingerprints[0], fingerprints[1],
        "403 and 404 require distinct failure identities"
    );
    assert_eq!(
        fingerprints[0], fingerprints[2],
        "repeating 403 retains its identity"
    );
}

use super::CodeModeNestedResultEvidence;
use super::FAILED_CELL_ERROR_TRUNCATION_MARKER;
use super::MAX_FAILED_CELL_ERROR_BYTES;
use super::failed_code_mode_cell_item;
use super::format_runtime_response;
use super::response_needs_retained_nested_results;

fn nested_result_evidence(output: &str) -> CodeModeNestedResultEvidence {
    CodeModeNestedResultEvidence {
        failed: false,
        command_state: None,
        ordinal: 0,
        call_id: "exec-cell-1-call-1".to_string(),
        parent_call_id: Some("outer-exec-call".to_string()),
        parent_cell_id: "cell-1".to_string(),
        runtime_tool_call_id: "call-1".to_string(),
        tool_name: "exec_command".to_string(),
        output: output.to_string(),
        output_truncated: false,
    }
}

#[test]
fn failed_runtime_response_builds_a_linkable_cell_item() {
    let item = failed_code_mode_cell_item(
        "exec-call-3",
        &RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("cell-7".to_string()),
            content_items: Vec::new(),
            error_text: Some("TypeError at line 4".to_string()),
        },
        Duration::from_millis(12),
    )
    .expect("failed result should emit a cell item");

    assert_eq!(item.id, "code-mode-cell:cell-7");
    assert_eq!(item.namespace.as_deref(), Some("codex.internal"));
    assert_eq!(item.tool, "code_mode_cell");
    assert_eq!(item.arguments["call_id"], "exec-call-3");
    assert_eq!(item.arguments["cell_id"], "cell-7");
    assert_eq!(item.success, Some(false));
    assert_eq!(item.error.as_deref(), Some("TypeError at line 4"));
}

#[test]
fn failed_cell_error_is_bounded_without_splitting_utf8() {
    let oversized_error = format!(
        "{}{}",
        "é".repeat(MAX_FAILED_CELL_ERROR_BYTES),
        "TAIL_MUST_NOT_SURVIVE"
    );
    let item = failed_code_mode_cell_item(
        "exec-call-3",
        &RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("cell-7".to_string()),
            content_items: Vec::new(),
            error_text: Some(oversized_error),
        },
        Duration::from_millis(12),
    )
    .expect("failed result should emit a cell item");
    let error = item.error.expect("failed cell error");

    assert!(error.len() <= MAX_FAILED_CELL_ERROR_BYTES);
    assert!(error.ends_with(FAILED_CELL_ERROR_TRUNCATION_MARKER));
    assert!(!error.contains("TAIL_MUST_NOT_SURVIVE"));
}

#[test]
fn runtime_response_paths_preserve_status_success_and_output_limits() {
    let cell_id = || CellId::new("cell-1".to_string());
    let content_items = || {
        vec![RuntimeContentItem::InputText {
            text: "x".repeat(400),
        }]
    };
    let cases = vec![
        (
            RuntimeResponse::Yielded {
                cell_id: cell_id(),
                content_items: content_items(),
            },
            None,
            "Script running with cell ID cell-1",
        ),
        (
            RuntimeResponse::Terminated {
                cell_id: cell_id(),
                content_items: content_items(),
            },
            Some(false),
            "Script terminated",
        ),
        (
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id(),
                content_items: content_items(),
                error_text: None,
            },
            Some(true),
            "Script completed",
        ),
        (
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell_id(),
                content_items: content_items(),
                error_text: Some("boom".to_string()),
            },
            Some(false),
            "Script failed",
        ),
    ];

    for (response, expected_success, expected_status) in cases {
        let output = format_runtime_response(
            response,
            Some(20),
            5,
            /*original_image_detail_supported*/ true,
            Instant::now(),
            Vec::new(),
            Vec::new(),
            None,
        );

        assert_eq!(output.success, expected_success);
        assert!(matches!(
            output.body.first(),
            Some(FunctionCallOutputContentItem::InputText { text })
                if text.starts_with(expected_status)
        ));
        assert!(output.body.iter().any(|item| matches!(
            item,
            FunctionCallOutputContentItem::InputText { text }
                if text.contains('…')
        )));
    }
}

#[test]
fn yielded_runtime_response_is_resumable_not_timed_out() {
    let output = format_runtime_response(
        RuntimeResponse::Yielded {
            cell_id: CellId::new("cell-live".to_string()),
            content_items: Vec::new(),
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
        None,
    );

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert_eq!(output.success, None);
}

#[test]
fn terminated_runtime_response_emits_failure_sampling_evidence() {
    let output = format_runtime_response(
        RuntimeResponse::Terminated {
            cell_id: CellId::new("cell-terminated".to_string()),
            content_items: Vec::new(),
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
        None,
    );

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert!(
        output
            .sampling_request_signal()
            .is_some_and(|signal| signal.to_string().contains("failure_signature"))
    );
}

#[test]
fn runtime_response_sampling_identity_excludes_wall_time() {
    let response = || RuntimeResponse::Result {
        output_loss: None,
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "same result".to_string(),
        }],
        error_text: None,
    };
    let recent = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
        None,
    );
    let older = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now() - Duration::from_secs(5),
        Vec::new(),
        Vec::new(),
        None,
    );

    assert_ne!(recent.body, older.body);
    assert_eq!(
        recent.sampling_request_signal(),
        older.sampling_request_signal(),
    );
}

#[test]
fn post_tool_feedback_survives_code_mode_projection() {
    let output = format_runtime_response(
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("cell-feedback".to_string()),
            content_items: Vec::new(),
            error_text: None,
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        vec![FunctionCallOutputContentItem::InputText {
            text: "hook feedback".to_string(),
        }],
        Vec::new(),
        None,
    );

    assert!(output.body.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::InputText { text } if text == "hook feedback"
    )));
    assert!(
        output
            .sampling_request_signal()
            .is_some_and(|signal| signal.to_string().contains("hook feedback"))
    );
}

#[test]
fn failed_script_keeps_successful_nested_result_and_linkage_visible() {
    let response = RuntimeResponse::Result {
        output_loss: None,
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "before failure".to_string(),
        }],
        error_text: Some("boom at line 7".to_string()),
    };
    assert!(response_needs_retained_nested_results(&response));

    let output = format_runtime_response(
        response,
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        vec![nested_result_evidence("COMMAND_SENTINEL")],
        None,
    )
    .into_text();

    assert!(output.contains("Nested tool result:"));
    assert!(output.contains("COMMAND_SENTINEL"));
    assert!(output.contains("\"parent_call_id\":\"outer-exec-call\""));
    assert!(output.contains("\"parent_cell_id\":\"cell-1\""));
    assert!(output.contains("\"runtime_tool_call_id\":\"call-1\""));
    assert!(output.contains("Script error:\nboom at line 7"));
}

#[test]
fn empty_successful_script_projects_retained_nested_result() {
    let response = RuntimeResponse::Result {
        output_loss: None,
        cell_id: CellId::new("cell-1".to_string()),
        content_items: Vec::new(),
        error_text: None,
    };
    assert!(response_needs_retained_nested_results(&response));

    let output = format_runtime_response(
        response,
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        vec![nested_result_evidence("NO_TEXT_SENTINEL")],
        None,
    )
    .into_text();

    assert!(output.contains("NO_TEXT_SENTINEL"));
}

#[test]
fn successful_script_output_suppresses_duplicate_retained_result_projection() {
    let response = RuntimeResponse::Result {
        output_loss: None,
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "already projected".to_string(),
        }],
        error_text: None,
    };

    assert!(!response_needs_retained_nested_results(&response));
}

#[tokio::test]
async fn terminated_packet_keeps_nested_results_and_omission_notice_after_printed_output() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: Arc::new(session),
        turn: Arc::new(turn),
    };
    for count in [1, 10] {
        let cell = CellId::new(format!("terminated-{count}"));
        let service = &exec.session.services.code_mode_service;
        service.record_cell_parent_call_id(&cell, "outer-exec");
        for _ in 0..count {
            let ordinal = service.begin_packet_call(&cell).unwrap();
            service.complete_packet_call(
                &cell,
                ordinal,
                false,
                0,
                Vec::new(),
                Some(nested_result_evidence("RETAINED_BEFORE_TERMINATION")),
                None,
            );
        }
        let output = super::handle_runtime_response(
            &exec,
            RuntimeResponse::Terminated {
                cell_id: cell,
                content_items: vec![RuntimeContentItem::InputText {
                    text: "progress log".to_string(),
                }],
            },
            Some(10_000),
            Instant::now(),
        )
        .unwrap();
        let visible = output.into_text();
        assert!(visible.contains("Script terminated"));
        assert!(visible.contains("progress log"));
        assert_eq!(
            visible.matches("RETAINED_BEFORE_TERMINATION").count(),
            count.min(8)
        );
        assert_eq!(
            visible.contains("2 additional nested tool results were omitted"),
            count == 10
        );
    }
}

#[tokio::test]
async fn packet_composition_budgets_required_diagnostics_and_preserves_canonical_failure() {
    use crate::tools::context::RequiredToolTerminalCause;
    use crate::tools::context::ToolPayload;
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: std::sync::Arc::new(session),
        turn: std::sync::Arc::new(turn),
    };
    let cell = CellId::new("large-required-error".to_string());
    let service = &exec.session.services.code_mode_service;
    service.record_cell_parent_call_id(&cell, "outer-exec");
    let ordinal = service.begin_packet_call(&cell).unwrap();
    let diagnostic = format!("REQUIRED_ROOT_CAUSE {} CANONICAL_TAIL", "é".repeat(40_000));
    service.complete_packet_call(
        &cell,
        ordinal,
        false,
        0,
        Vec::new(),
        Some(nested_result_evidence("RETAINED_RESULT")),
        Some((RequiredToolTerminalCause::Failure, diagnostic.clone())),
    );
    let output = super::handle_runtime_response(
        &exec,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell.clone(),
            content_items: Vec::new(),
            error_text: None,
        },
        Some(200),
        Instant::now(),
    )
    .unwrap();

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert_eq!(output.success, Some(false));
    let visible = super::code_mode_text_content(&output.body);
    assert!(visible.contains("Required nested tool outcome: REQUIRED_ROOT_CAUSE"));
    assert!(
        codex_utils_string::approx_token_count(&visible) < 300,
        "{visible}"
    );
    assert!(!visible.contains(&diagnostic));
    let canonical = output
        .canonical_result(&ToolPayload::Custom {
            input: "await tools.example({})".to_string(),
        })
        .unwrap();
    let canonical = String::from_utf8(canonical.bytes).unwrap();
    assert!(canonical.contains(&diagnostic));
    assert!(canonical.contains("RETAINED_RESULT"));
    let signal = output.sampling_request_signal().unwrap();
    assert_eq!(signal["outcome"], "failure");
    assert_eq!(signal["nested_ordinal"], ordinal);
    assert!(
        signal["failure_signature"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        signal["semantic_evidence"]
            .to_string()
            .contains("REQUIRED_ROOT_CAUSE")
    );
    assert!(
        service
            .finish_packet(cell.as_str(), false)
            .first_required_terminal
            .is_none()
    );
    service.finish_cell_dispatch(&cell);
}

#[test]
fn mixed_runtime_failure_uses_the_actual_error_after_a_printed_error_log() {
    let output = format_runtime_response(
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("mixed-errors".to_string()),
            content_items: vec![
                RuntimeContentItem::InputText {
                    text: format!("Script error:\nOLD_LOG {}", "x".repeat(4000)),
                },
                RuntimeContentItem::InputImage {
                    image_url: "data:image/png;base64,AA==".to_string(),
                    detail: None,
                },
            ],
            error_text: Some("ACTUAL_SCRIPT_FAILURE".to_string()),
        },
        Some(40),
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
        None,
    );
    assert!(output.body.iter().any(|item| matches!(item,
        FunctionCallOutputContentItem::InputText { text } if text == "Script error:\nACTUAL_SCRIPT_FAILURE"
    )));
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
}

#[tokio::test]
async fn live_packet_drain_preserves_ordinals_for_outstanding_calls() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: std::sync::Arc::new(session),
        turn: std::sync::Arc::new(turn),
    };
    let service = &exec.session.services.code_mode_service;
    let cell = CellId::new("live-packet".to_string());
    service.record_cell_parent_call_id(&cell, "outer-exec");
    let first = service.begin_packet_call(&cell).unwrap();
    let live = super::handle_runtime_response(
        &exec,
        RuntimeResponse::Yielded {
            cell_id: cell.clone(),
            content_items: Vec::new(),
        },
        Some(100),
        Instant::now(),
    )
    .unwrap();
    assert_eq!(live.outcome_for_logging(), ToolOutputOutcome::Yielded);
    let second = service.begin_packet_call(&cell).unwrap();
    assert_eq!((first, second), (0, 1));
    for (ordinal, message) in [(second, "second failure"), (first, "first failure")] {
        service.complete_packet_call(
            &cell,
            ordinal,
            false,
            0,
            Vec::new(),
            None,
            Some((
                crate::tools::context::RequiredToolTerminalCause::Failure,
                message.to_string(),
            )),
        );
    }
    let terminal = super::handle_runtime_response(
        &exec,
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: cell.clone(),
            content_items: Vec::new(),
            error_text: None,
        },
        Some(100),
        Instant::now(),
    )
    .unwrap();
    assert!(super::code_mode_text_content(&terminal.body).contains("first failure"));
    assert_eq!(
        terminal.sampling_request_signal().unwrap()["nested_ordinal"],
        0
    );
    service.finish_cell_dispatch(&cell);
}

#[tokio::test]
async fn command_receipts_share_the_script_budget_and_keep_latest_canonical_states() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: Arc::new(session),
        turn: Arc::new(turn),
    };
    let service = &exec.session.services.code_mode_service;
    let cell = CellId::new("receipt-budget".to_string());
    service.record_cell_parent_call_id(&cell, "outer-receipts");
    for index in 0..400 {
        let ordinal = service.begin_packet_call(&cell).unwrap();
        let mut result = nested_result_evidence("read");
        result.command_state = Some(serde_json::json!({
            "polled_session_id": 777, "process_exited": index == 399,
            "chunk_id": format!("POLL_{index}"), "output_complete": index == 399,
        }));
        service.complete_packet_call(&cell, ordinal, false, 0, Vec::new(), Some(result), None);
    }
    for index in 0..100 {
        let ordinal = service.begin_packet_call(&cell).unwrap();
        let mut result = nested_result_evidence("read");
        result.command_state = Some(serde_json::json!({
            "session_id": index, "process_exited": index != 99,
            "chunk_id": format!("DISTINCT_{index}"),
            "raw_output_artifact_error": "large diagnostic".repeat(100),
        }));
        service.complete_packet_call(&cell, ordinal, false, 0, Vec::new(), Some(result), None);
    }
    let output = super::handle_runtime_response(
        &exec,
        RuntimeResponse::Result {
            cell_id: cell.clone(),
            content_items: vec![RuntimeContentItem::InputText {
                text: "printed output".into(),
            }],
            error_text: Some("ACTUAL_RECEIPT_FAILURE".into()),
            output_loss: None,
        },
        Some(200),
        Instant::now(),
    )
    .unwrap();
    let visible = super::code_mode_text_content(&output.body);
    assert!(
        codex_utils_string::approx_token_count(&visible) < 300,
        "{visible}"
    );
    assert!(visible.contains("ACTUAL_RECEIPT_FAILURE"));
    let inline = output.essential_inline["nested_commands"]
        .as_array()
        .unwrap();
    assert_eq!(
        inline.len(),
        crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES
    );
    assert_eq!(inline[0]["session_id"], 99);
    assert!(
        inline
            .iter()
            .all(|state| state.get("raw_output_artifact_error").is_none())
    );
    let canonical = output.canonical_body.as_ref().unwrap();
    let receipts = canonical
        .iter()
        .find_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => {
                text.strip_prefix("Nested command states (independent of script completion):\n")
            }
            _ => None,
        })
        .unwrap();
    let states: Vec<serde_json::Value> = serde_json::from_str(receipts).unwrap();
    assert_eq!(states.len(), 101);
    let polled = states
        .iter()
        .find(|state| state["polled_session_id"] == 777)
        .unwrap();
    assert_eq!(polled["chunk_id"], "POLL_399");
    assert_eq!(polled["process_exited"], true);
    assert!(
        states
            .iter()
            .any(|state| state["chunk_id"] == "DISTINCT_99")
    );
    service.finish_cell_dispatch(&cell);
}
