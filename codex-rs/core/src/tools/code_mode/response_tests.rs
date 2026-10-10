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
            if args["serial_retention"] == true
                && let crate::tools::router::ToolCallSource::CodeMode { cell_id, .. } = &invocation.source
            {
                invocation.session.services.code_mode_service
                    .flush_packet_retention(&CellId::new(cell_id.clone())).await;
            }
            if let Some(delay) = args["delay_ms"].as_u64() {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if args["partial_selection"] == true {
                return Ok(crate::tools::context::boxed_tool_output(
                    codex_tools::JsonToolOutput::with_success(serde_json::json!({
                        "complete":false, "results":[{"text":"EXACT_SIBLING"}],
                        "selector_errors":[{"status":"invalid_selector"}]
                    }), Some(false)).with_code_mode_failure_as_data(),
                ));
            }
            let process_exit_code = args["process_exit_code"]
                .as_i64()
                .or_else(|| (args["cmd"] == "fixture-exit-101").then_some(101));
            let process_running = args["process_running"] == true;
            if process_exit_code.is_some() || process_running {
                return Ok(crate::tools::context::boxed_tool_output(
                    crate::tools::context::ExecCommandToolOutput {
                        output_ranges: None,
                        process_output: None,
                        error: None,
                        validation: None,
                        event_call_id: invocation.call_id,
                        chunk_id: "failed-process".into(),
                        wall_time: Duration::ZERO,
                        raw_output: b"COMPILER_DIAGNOSTIC".to_vec(),
                        truncation_policy: codex_utils_output_truncation::TruncationPolicy::Tokens(
                            2000,
                        ),
                        max_output_tokens: None,
                        process_id: process_running.then_some(777),
                        session_capabilities: None,
                        exit_code: process_exit_code.map(|code| code as i32),
                        process_exited: !process_running,
                        search_no_match: false,
                        original_token_count: None,
                        hook_command: None,
                        raw_output_artifact: None,
                        repair_notice: None,
                        pending_deferred_completions: Vec::new(),
                    },
                ));
            }
            if args["patch_success"] == true {
                return Ok(crate::tools::context::boxed_tool_output(
                    crate::tools::context::ApplyPatchToolOutput {
                        text: "Success. Updated changed.rs".to_string(),
                        success: true,
                        changes: vec![
                            serde_json::json!({"path": "changed.rs", "kind": "update", "move_path": null}),
                        ],
                        changes_exact: true,
                        environment_id: Some("local".to_string()),
                        retry: None,
                        diagnostics: Vec::new(),
                    },
                ));
            }
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
                    Some("fixture-dispatch-rejected") => Some("dispatch rejected"),
                    Some("Remove-Item output.txt") => Some("write rejected"),
                    Some("git log -1") => Some("history unavailable"),
                    Some("Get-Content missing.txt") => Some("file missing"),
                    Some("git status --short") => Some("status unavailable"),
                    _ => None,
                })
            {
                return Err(crate::FunctionCallError::RespondToModel(error.to_string()));
            }
            let text = if args["benchmark_output"] == true {
                "src/first.rs:12:needle\nC:\\repo\\second.rs:3:needle\n".repeat(2_000)
                    + "FAILURE_TAIL\n"
            } else if args["retention_output"] == true {
                "EXACT_LARGE_RECOVERY\n".repeat(400)
            } else {
                "READ_RESULT_42".to_string()
            };
            let output = FunctionToolOutput::from_text(text, Some(true));
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

impl crate::tools::registry::CoreToolRuntime for PacketTestTool {
    fn command_argument_format(&self) -> Option<crate::tools::registry::CommandArgumentFormat> {
        (self.name == "exec_command").then_some(crate::tools::registry::CommandArgumentFormat::Exec)
    }
}

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
        Self::with_nested_runtime(Arc::new(PacketTestTool { name })).await
    }

    async fn with_nested_runtime(nested: Arc<dyn crate::tools::registry::CoreToolRuntime>) -> Self {
        let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
        session.services.code_mode_service = super::CodeModeService::new(Arc::new(
            codex_code_mode::InProcessCodeModeSessionProvider,
        ));
        turn.model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
        let session = Arc::new(session);
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
    assert!(!rendered.contains("Script completed"), "{rendered}");
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

/// No provider calls: measures the real JS/broker/router/projection round trip
/// with a deterministic nested producer. Process startup is measured separately.
#[tokio::test]
#[ignore = "wall-clock benchmark; run explicitly without competing benchmarks"]
#[expect(clippy::print_stdout, reason = "this opt-in benchmark emits its measured samples as JSON")]
async fn benchmark_tool_execution_projection() {
    let runtime = PacketRuntime::new().await;
    for limit in [100, 10_000] {
        let source = format!(
            "// @exec: {{\"max_output_tokens\":{limit}}}\ntext(await tools.read_tool_output({{benchmark_output:true}}));"
        );
        let mut elapsed_us = Vec::new();
        for sample in 0..8 {
            let start = Instant::now();
            let output = runtime.exec(&source).await;
            let visible = packet_output_text(output.as_ref());
            let elapsed = start.elapsed().as_micros();
            assert!(visible.contains("FAILURE_TAIL"), "{visible}");
            assert!(visible.contains("omitted lines"), "{visible}");
            if sample > 0 {
                elapsed_us.push(elapsed);
            }
        }
        let mut sorted = elapsed_us.clone();
        sorted.sort_unstable();
        println!("{}", serde_json::json!({
            "benchmark":"code_mode_nested_dispatch_to_model_packet",
            "limit":limit, "samples_us":elapsed_us, "median_us":sorted[3],
            "model_calls":0, "subprocesses":0,
        }));
    }
    runtime.finish().await;
}

#[tokio::test]
async fn packet_retention_flush_preserves_order_after_cancelled_wait_and_yield() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let service = &session.services.code_mode_service;
    let cell = CellId::new("retention-order".into());
    service.record_cell_parent_call_id(&cell, "outer");
    let (release, blocked) = tokio::sync::oneshot::channel();
    let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let first = Arc::clone(&completed);
    assert!(service.queue_packet_retention(&session, &cell, async move {
        blocked.await.unwrap();
        first.lock().unwrap().push(1);
    }).is_none());
    {
        let flush = service.flush_packet_retention(&cell);
        tokio::pin!(flush);
        assert!(futures::poll!(flush.as_mut()).is_pending());
        // Dropping this observer must not discard the chain's predecessor.
    }
    service.finish_packet(cell.as_str(), true);
    let second = Arc::clone(&completed);
    {
        let previous = service.queue_packet_retention(&session, &cell, async move {
            second.lock().unwrap().push(2);
        }).expect("the predecessor applies backpressure");
        tokio::pin!(previous);
        assert!(futures::poll!(previous.as_mut()).is_pending());
        // Cancelling the producer's wait must preserve both accepted writes.
    }
    let flush = service.flush_packet_retention(&cell);
    tokio::pin!(flush);
    assert!(futures::poll!(flush.as_mut()).is_pending());
    assert!(completed.lock().unwrap().is_empty());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), flush).await.unwrap();
    assert_eq!(*completed.lock().unwrap(), vec![1, 2]);
    service.finish_cell_dispatch(&cell);
}

#[tokio::test]
async fn packet_retention_backpressure_preserves_overlap_and_cell_independence() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let service = &session.services.code_mode_service;
    let cell = CellId::new("slow-retention".into());
    let other = CellId::new("independent-retention".into());
    service.record_cell_parent_call_id(&cell, "outer");
    service.record_cell_parent_call_id(&other, "other-outer");
    let (release_first, first) = tokio::sync::oneshot::channel();
    let (release_second, second) = tokio::sync::oneshot::channel();
    assert!(service.queue_packet_retention(&session, &cell, async move {
        first.await.unwrap();
    }).is_none());
    let previous = service.queue_packet_retention(&session, &cell, async move {
        second.await.unwrap();
    }).expect("a producer cannot run arbitrarily far ahead of storage");
    tokio::pin!(previous);
    assert!(futures::poll!(previous.as_mut()).is_pending());

    // Storage backpressure is per cell, never a new cross-session execution lock.
    assert!(service.queue_packet_retention(&session, &other, async {}).is_none());
    tokio::time::timeout(Duration::from_secs(5), service.flush_packet_retention(&other))
        .await.unwrap();
    release_first.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), previous).await.unwrap();
    // Returning the second result waits only for the first write. The second
    // remains owned and can overlap the next tool until the response flush.
    let flush = service.flush_packet_retention(&cell);
    tokio::pin!(flush);
    assert!(futures::poll!(flush.as_mut()).is_pending());
    release_second.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), flush).await.unwrap();
    service.finish_cell_dispatch(&cell);
    service.finish_cell_dispatch(&other);
}

#[tokio::test]
async fn packet_retention_survives_cell_close_without_reopening_packet() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let service = &session.services.code_mode_service;
    let cell = CellId::new("closed-retention".into());
    service.record_cell_parent_call_id(&cell, "outer");
    let (release, blocked) = tokio::sync::oneshot::channel();
    let (done, finished) = tokio::sync::oneshot::channel();
    let task_session = Arc::clone(&session);
    let task_cell = cell.clone();
    assert!(service.queue_packet_retention(&session, &cell, async move {
        blocked.await.unwrap();
        task_session.services.code_mode_service.record_packet_recovery(
            &task_cell, 0, serde_json::json!({"result":"late"}),
        );
        done.send(()).unwrap();
    }).is_none());
    service.finish_cell_dispatch(&cell);
    session.terminal_tasks.close();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.terminal_tasks.wait()).await.unwrap();
    finished.await.unwrap();
    assert!(!service.packet_admission.lock().unwrap().cells.contains_key(cell.as_str()));
}

#[tokio::test]
async fn packet_retention_large_results_are_recoverable_at_exec_and_wait_boundaries() {
    for use_wait in [false, true] {
        let runtime = PacketRuntime::with_nested_tool("exec_command").await;
        let source = format!(
            "{} for (let i = 0; i < 3; i++) await tools.exec_command({{cmd:'Get-Content source.txt', retention_output:true}});",
            if use_wait { "await yield_control();" } else { "" },
        );
        let mut output = runtime.exec(&source).await;
        if use_wait {
            let cell = runtime.live_cell();
            output = runtime.call(ToolPayload::Function {
                arguments: serde_json::json!({"cell_id":cell.as_str(),"max_tokens":10_000}).to_string(),
            }, Default::default()).await.unwrap();
        }
        let visible = packet_output_text(output.as_ref());
        let directory = visible.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|row| row["nested_result_recovery_directory"].as_array().cloned())
            .expect("all three large results have recovery entries");
        assert_eq!(directory.len(), 3, "{visible}");
        let history = runtime.session.lock_history_state_for_test().await.tool_history_state();
        for entry in directory {
            let artifact_id = entry["outcome"]["artifact_id"].as_str().expect("retention drained");
            assert!(history.artifact_references().contains_key(artifact_id));
            let exact = crate::tools::command_output_artifact::read_complete_canonical_snapshot(
                &runtime.step.turn.config.codex_home, &runtime.session.thread_id.to_string(),
                artifact_id, 100_000,
            ).await.unwrap();
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&exact).unwrap(),
                serde_json::json!("EXACT_LARGE_RECOVERY\n".repeat(400)));
        }
        runtime.finish().await;
    }
}

/// Matched in-process barrier baseline, not an end-to-end model benchmark.
/// The serial fixture waits for the previous result before doing its own work.
#[tokio::test]
#[ignore = "wall-clock benchmark; run explicitly without competing benchmarks"]
#[expect(clippy::print_stdout, reason = "this opt-in benchmark emits matched retention timing samples as JSON")]
async fn benchmark_packet_retention_overlap() {
    let runtime = PacketRuntime::with_nested_tool("exec_command").await;
    for large in [false, true] {
        for serial in [true, false] {
            let source = format!(
                "for (let i=0; i<8; i++) await tools.exec_command({{cmd:'Get-Content source.txt',retention_output:{large},serial_retention:{serial},delay_ms:5}}); text('settled');"
            );
            let mut samples_us = Vec::new();
            for sample in 0..8 {
                let started = Instant::now();
                let output = runtime.exec(&source).await;
                assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
                assert!(packet_output_text(output.as_ref()).contains("settled"));
                if sample > 0 { samples_us.push(started.elapsed().as_micros()); }
            }
            let mut sorted = samples_us.clone();
            sorted.sort_unstable();
            println!("{}", serde_json::json!({
                "benchmark":"packet_retention_overlap", "large":large,
                "serial_barrier":serial, "samples_us":samples_us, "median_us":sorted[3],
                "nested_calls_per_cell":8, "model_calls":0,
            }));
        }
    }
    runtime.finish().await;
}

#[tokio::test]
async fn nested_failures_preserve_only_observed_workspace_dependencies() {
    use crate::tool_history::SourceDependencyV1;
    use std::collections::BTreeSet;

    for (input, added_dependency, observes_directory) in [
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
            Some("fixture-dispatch-rejected") => "dispatch rejected",
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
        if observes_directory {
            expected.insert(SourceDependencyV1::new(
                runtime.step.turn.config.cwd.as_path(),
                true,
            ));
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
    for (count, omitted) in [(20, 18), (2, 0)] {
        let source = format!(
            "await Promise.all(Array.from({{length: {count}}}, () => tools.read_tool_output({{}})));"
        );
        let output = runtime.exec(&source).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        let visible = packet_output_text(output.as_ref());
        let rows = visible.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).collect::<Vec<_>>();
        assert_eq!(rows.iter().filter(|row| row.get("result").is_some() && row.get("tool_name").is_some()).count(), 2, "{visible}");
        if omitted > 0 {
            let directory = rows.iter().find(|row| row.get("nested_result_recovery_directory").is_some()).expect("all settled results remain recoverable");
            assert_eq!(directory["omitted_inline_result_count"], omitted);
            assert_eq!(directory["nested_result_recovery_directory"].as_array().unwrap().len(), count);
        } else {
            assert!(!visible.contains("nested_result_recovery_directory"), "{visible}");
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
            "await tools.read_tool_output({}); await yield_control(); await new Promise(resolve => setTimeout(resolve, 60_000));",
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
    let output = result.expect("cancelled wait returns the retained terminal packet");
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(output.log_preview().contains("Script terminated"));
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
async fn nested_process_state_remains_available_to_the_resumed_script() {
    let runtime = PacketRuntime::with_nested_tool("write_stdin").await;
    let initial = runtime.exec(
        "const running = await tools.write_stdin({process_running: true}); \
         if (running.execution_state !== 'running' || running.process_exited !== false \
             || running.exit_code !== null || running.session_id !== 777) \
             throw new Error('lost running state'); \
         await yield_control(); \
         const done = await tools.write_stdin({session_id: running.session_id, process_exit_code: 0}); \
         if (done.execution_state !== 'exited' || !done.process_exited \
             || done.exit_code !== 0 || !done.output_complete || done.output_reduced) \
             throw new Error('lost terminal state'); \
         text('observed running and terminal results in one cell');",
    ).await;
    assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
    let completed = runtime.wait(&runtime.live_cell()).await;
    assert_eq!(completed.outcome_for_logging(), ToolOutputOutcome::Success);
    assert!(
        packet_output_text(completed.as_ref())
            .contains("observed running and terminal results in one cell")
    );
    runtime.finish().await;
}

#[tokio::test]
async fn nested_patch_result_remains_structured_across_explicit_yield() {
    let runtime = PacketRuntime::with_nested_tool("apply_patch").await;
    let initial = runtime
        .exec(
            "const patch = await tools.apply_patch({patch_success: true}); \
         if (!patch.success || !patch.changes_exact || patch.changes[0].path !== 'changed.rs') \
             throw new Error('lost patch metadata'); \
         await yield_control(); \
         text({file: patch.changes[0].path, environment: patch.environment_id});",
        )
        .await;
    assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
    let completed = runtime.wait(&runtime.live_cell()).await;
    assert_eq!(completed.outcome_for_logging(), ToolOutputOutcome::Success);
    let text = packet_output_text(completed.as_ref());
    assert!(text.contains(r#""file":"changed.rs""#), "{text}");
    assert!(text.contains(r#""environment":"local""#), "{text}");
    runtime.finish().await;
}

#[tokio::test]
async fn command_failure_remains_inspectable_without_another_cell() {
    for name in ["exec_command", "write_stdin"] {
        let runtime = PacketRuntime::with_nested_tool(name).await;
        let source = format!(
            "const r = await tools.{name}({{cmd: 'fixture-exit-101'}}); \
             if (r.exit_code !== 101 || !r.process_exited) throw new Error('lost exit state'); \
             text(r.output); text(await tools.{name}({{cmd: 'fixture-read'}}));"
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
        let visible = packet_output_text(output.as_ref());
        assert_eq!(
            output.outcome_for_logging(),
            ToolOutputOutcome::Success,
            "{visible}"
        );
        assert!(
            visible.contains("READ_RESULT_42"),
            "the next tool must execute in the same cell: {visible}"
        );
        assert!(visible.contains("COMPILER_DIAGNOSTIC"), "{visible}");
        assert!(visible.contains("nested_command_failure"), "{visible}");
        assert!(visible.contains("\"exit_code\":101"), "{visible}");
        assert!(
            !visible.contains("Script error:"),
            "a nonzero exit is not a JS exception: {visible}"
        );
        let control = crate::session::turn_execution::TurnExecutionControl::new();
        let request = control.continuation_generation_request(
            &control.baselines(0),
            &runtime.signals,
            &crate::session::turn_execution::SamplingRequestSettledState {
                mutation_revision: 0,
                attributed_mutation_revision: 0,
                tool_exposure_revision: 0,
            },
            false,
        );
        assert_eq!(
            request.purpose,
            Some(codex_protocol::protocol::TurnTimingGenerationPurpose::Repair),
            "failure of a potentially mutating command must reach the repair consumer"
        );
        assert!(
            request.failure_fingerprint.is_some(),
            "command failure evidence must retain its diagnostic identity"
        );
        assert_eq!(
            request.sampling,
            crate::session::turn_execution::SamplingGenerationDisposition::DecisionBearing,
            "a command failure must still permit the next repair decision"
        );
        assert!(!request.terminal_completion_only);
        runtime.finish().await;
    }
}

#[tokio::test]
async fn command_failure_state_survives_explicit_yield_before_recovery() {
    let runtime = PacketRuntime::with_nested_tool("write_stdin").await;
    let initial = runtime.exec(
        "const result = await tools.write_stdin({process_exit_code: 7}); await yield_control(); \
         if (result.exit_code !== 7) throw new Error('lost failure'); text(await tools.write_stdin({}));",
    ).await;
    assert_eq!(initial.outcome_for_logging(), ToolOutputOutcome::Yielded);
    let output = runtime.wait(&runtime.live_cell()).await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
    assert!(packet_output_text(output.as_ref()).contains("READ_RESULT_42"));
    runtime.finish().await;
}

#[tokio::test]
async fn command_dispatch_errors_still_stop_dependent_work() {
    let runtime = PacketRuntime::with_nested_tool("exec_command").await;
    let output = runtime.exec(
        "await tools.exec_command({cmd: 'fixture-dispatch-rejected'}); text(await tools.exec_command({}));",
    ).await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    let visible = packet_output_text(output.as_ref());
    assert!(visible.contains("dispatch rejected"));
    assert!(!visible.contains("READ_RESULT_42"));
    runtime.finish().await;
}

#[tokio::test]
async fn partial_selection_data_survives_nested_dispatch() {
    let runtime = PacketRuntime::new().await;
    let output = runtime.exec(
        "const r = await tools.read_tool_output({partial_selection:true}); \
         if (r.complete || r.selector_errors.length !== 1) throw new Error('lost failure'); \
         text(r.results[0].text);",
    ).await;
    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
    assert!(packet_output_text(output.as_ref()).contains("EXACT_SIBLING"));
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
                attributed_mutation_revision: 0,
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
use super::bounded_serialized_json;
use super::failed_code_mode_cell_item;
use super::format_runtime_response;
use super::nested_result_already_emitted;
use super::response_needs_retained_nested_results;
use super::retained_nested_output;
use super::shows_session_handle;

fn nested_result_evidence(output: &str) -> CodeModeNestedResultEvidence {
    CodeModeNestedResultEvidence {
        failed: false,
        command_state: None,
        output_fingerprints: Vec::new(),
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
    assert_eq!(item.status, codex_protocol::items::DynamicToolCallStatus::Failed);
    assert_eq!(item.duration, Some(Duration::from_millis(12)));
    assert_eq!(item.success, Some(false));
    assert_eq!(item.error.as_deref(), Some("TypeError at line 4"));
}

#[test]
fn failed_cell_error_is_bounded_without_splitting_utf8() {
    // The leading byte shifts every two-byte character so the cut point falls
    // inside one; an aligned fixture never needs to back off to a boundary.
    let oversized_error = format!(
        "a{}{}",
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

    assert!(error.len() < MAX_FAILED_CELL_ERROR_BYTES);
    assert!(error.ends_with(FAILED_CELL_ERROR_TRUNCATION_MARKER));
    assert!(!error.contains("TAIL_MUST_NOT_SURVIVE"));
}

#[test]
fn cell_projection_preserves_running_yielded_and_successful_boundaries() {
    let cell = CellId::new("cell-live-process".to_string());
    for (response, state, success) in [
        (RuntimeResponse::Yielded { cell_id: cell.clone(), content_items: Vec::new() }, "in_progress", None),
        (RuntimeResponse::ExplicitYield { cell_id: cell.clone(), content_items: Vec::new() }, "yielded", None),
        (RuntimeResponse::Result {
            cell_id: cell, content_items: vec![RuntimeContentItem::InputText {
                text: r#"{"session_id":42,"process_exited":false}"#.into(),
            }], error_text: None, output_loss: None,
        }, "completed", Some(true)),
    ] {
        let item = failed_code_mode_cell_item("exec-parent", &response, Duration::ZERO).unwrap();
        assert_eq!(item.id, "code-mode-cell:cell-live-process");
        assert_eq!(item.arguments["state"], state);
        assert_eq!(item.success, success);
        assert!(item.error.is_none());
    }
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
            "",
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

#[tokio::test]
async fn truncated_cell_recovery_covers_the_entire_omitted_gap() {
    let source = (0..600).map(|line| format!("line {line} λ😀\r\n")).collect::<String>();
    let output = format_runtime_response(
        RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("full-gap".into()),
            content_items: vec![RuntimeContentItem::InputText { text: source.clone() }],
            error_text: None,
        },
        Some(300), usize::MAX, true, Instant::now(), Vec::new(), Vec::new(), None,
    );
    let visible = super::code_mode_text_content(&output.body);
    let gap = visible.split("[omitted lines ").nth(1).expect("omission marker");
    let coordinates = gap.split(" of ").next().unwrap();
    let (start, end) = coordinates.split_once('-').unwrap();
    let start = start.parse::<usize>().unwrap();
    let end = end.parse::<usize>().unwrap();
    assert!(end - start > 200, "fixture must exercise more than the old prefix");
    let selector = output.essential_inline["cell_output_recovery_selector"].clone();
    assert_eq!(selector, serde_json::json!({"kind":"lines", "start":start, "end":end}));
    let canonical = output.canonical_result(&ToolPayload::Custom { input: "fixture".into() }).unwrap();
    let canonical_text = std::str::from_utf8(&canonical.bytes).unwrap();
    assert!(canonical_text.ends_with(&source), "canonical retention must preserve every authored Unicode and CRLF byte");
    let expected = canonical_text
        .split_inclusive('\n').skip(start - 1).take(end - start + 1).collect::<String>();
    let home = tempfile::tempdir().unwrap();
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
        home.path(), "full-gap", &canonical,
    ).await;
    let (recovered, _) = crate::tools::handlers::execute_recovery_transaction(
        home.path(), "full-gap", &artifact.artifact_id().unwrap(),
        vec![serde_json::from_value(selector).unwrap()], true,
    ).await.unwrap();
    assert!(recovered.complete);
    assert_eq!(recovered.results[0].text.as_deref(), Some(expected.as_str()));
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

    assert_eq!(recent.body, older.body);
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

    assert!(!output.contains("Nested tool result:"));
    assert!(output.contains("COMMAND_SENTINEL"));
    assert!(output.contains("exec-cell-1-call-1"));
    assert!(output.contains("\"tool_name\":\"exec_command\""));
    assert!(!output.contains("parent_call_id"));
    assert!(!output.contains("parent_cell_id"));
    assert!(!output.contains("runtime_tool_call_id"));
    assert!(output.contains("Script error:\nboom at line 7"));
}

#[test]
fn rethrown_nested_failure_is_projected_once_through_the_script_error() {
    let rejection = "Command rejected: `rg -n needle 'src/run*'`\nReason: literal glob path";
    let response = RuntimeResponse::Result {
        output_loss: None,
        cell_id: CellId::new("cell-1".to_string()),
        content_items: Vec::new(),
        error_text: Some(format!("Error: {rejection}")),
    };
    let failed = CodeModeNestedResultEvidence {
        failed: true,
        ..nested_result_evidence(rejection)
    };
    let caught = CodeModeNestedResultEvidence {
        failed: true,
        ..nested_result_evidence("CAUGHT_FAILURE_SENTINEL")
    };

    let output = format_runtime_response(
        response,
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        vec![failed, caught],
        None,
    )
    .into_text();

    assert_eq!(
        output.matches("Reason: literal glob path").count(),
        1,
        "{output}"
    );
    assert!(output.contains(&format!("Script error:\nError: {rejection}")));
    assert!(output.contains("CAUGHT_FAILURE_SENTINEL"));
}

#[test]
fn failed_script_does_not_repeat_nested_results_it_already_printed() {
    let lines = "    let value = parse(\"field\")?;\r\n".repeat(40);
    let command = |output: String| {
        serde_json::json!({
            "chunk_id": "chunk",
            "exit_code": 0,
            "process_exited": true,
            "output": output,
        })
    };
    let printed = command(format!("SHARED_HEAD\r\n{lines}FIRST_RESULT_END\r\n"));
    // Shares the printed prefix, as build progress does, but was never printed.
    let unprinted = command(format!("SHARED_HEAD\r\n{lines}SECOND_RESULT_END\r\n"));
    let plan = serde_json::json!({
        "current_plan": {"plan": [{"step": "Validate the focused change", "status": "in_progress"}]},
        "message": "Plan updated",
    });
    let retained = |tool: &str, value: &serde_json::Value| CodeModeNestedResultEvidence {
        tool_name: tool.to_string(),
        output_fingerprints: super::nested_output_fingerprints(&codex_tools::ToolName::plain(tool), value),
        output: retained_nested_output(
            &codex_tools::ToolName::plain(tool),
            value,
            bounded_serialized_json(value).0,
        ),
        ..nested_result_evidence("")
    };
    // `text(result)` prints the escaped JSON; `text(result.output)` the text.
    let printed_forms = [
        printed.to_string(),
        printed["output"].as_str().unwrap().to_string(),
    ];
    for printed_command in printed_forms {
        let response = RuntimeResponse::Result {
            output_loss: None,
            cell_id: CellId::new("cell-1".to_string()),
            content_items: vec![
                RuntimeContentItem::InputText {
                    text: printed_command,
                },
                RuntimeContentItem::InputText {
                    text: plan.to_string(),
                },
            ],
            error_text: Some("Error: cargo test failed".to_string()),
        };

        let output = format_runtime_response(
            response,
            None,
            usize::MAX,
            true,
            Instant::now(),
            Vec::new(),
            vec![
                retained("exec_command", &printed),
                retained("update_plan", &plan),
                retained("exec_command", &unprinted),
            ],
            None,
        )
        .into_text();

        assert_eq!(output.matches("FIRST_RESULT_END").count(), 1, "{output}");
        assert_eq!(
            output.matches("Validate the focused change").count(),
            1,
            "{output}"
        );
        assert_eq!(output.matches("SECOND_RESULT_END").count(), 1, "{output}");
        assert!(output.contains("Script error:\nError: cargo test failed"));
    }
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
fn verified10_failed_siblings_keep_authoritative_identity_and_truncation() {
    let first = nested_result_evidence("same shaped output");
    let mut second = first.clone();
    second.call_id = "second-call".into();
    second.tool_name = "read_file".into();
    second.output_truncated = true;
    let output = format_runtime_response(RuntimeResponse::Result {
        output_loss: None, cell_id: CellId::new("cell-1".into()), content_items: Vec::new(),
        error_text: Some("exception".into()),
    }, None, usize::MAX, true, Instant::now(), Vec::new(), vec![first, second], None);
    let envelopes = output.body.iter().filter_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } => serde_json::from_str::<serde_json::Value>(text).ok(),
        _ => None,
    }).filter(|value| value.get("call_id").is_some()).collect::<Vec<_>>();
    assert_eq!(envelopes.len(), 2);
    assert_eq!(envelopes[0]["call_id"], "exec-cell-1-call-1");
    assert_eq!(envelopes[0]["output_truncated"], false);
    assert_eq!(envelopes[1]["call_id"], "second-call");
    assert_eq!(envelopes[1]["tool_name"], "read_file");
    assert_eq!(envelopes[1]["output_truncated"], true);
}

#[test]
fn verified_evidence_matching_ends_do_not_hide_an_unprinted_middle() {
    let prefix = "shared wrapper ".repeat(30);
    let suffix = "shared trailer ".repeat(30);
    let first = format!("{prefix}actual: 7{suffix}");
    let second = format!("{prefix}actual: 9{suffix}");
    let retained = nested_result_evidence(&second);
    assert!(!nested_result_already_emitted(&retained, &first));
    assert!(!nested_result_already_emitted(&retained, &serde_json::json!({"output": first}).to_string()));
    assert!(nested_result_already_emitted(&retained, &second));
    assert!(nested_result_already_emitted(&retained, &serde_json::json!({"output": second}).to_string()));
    let object = serde_json::json!({"diagnostic":"important middle error", "tail":"end"}).to_string();
    let retained = nested_result_evidence(&object);
    assert!(!nested_result_already_emitted(&retained, &object[1..]));
    assert!(nested_result_already_emitted(&retained, &object));
}

#[test]
fn verified_evidence_projection_preserves_batch_positions() {
    let value = serde_json::json!({"results": [
        {"payload": "a"}, {"payload": "b"}, {"payload": "c"}, {"payload": "d"},
        {"status": "failed", "payload": "diagnostic"}
    ]});
    let projection = codex_tools::ToolOutputProjectionMetadata::from_json(&value, true, None).essential_inline;
    assert_eq!(projection["results"], serde_json::json!([null, null, null, null, {"status":"failed"}]));
    let nested = serde_json::json!({"results":["ordinary", [null, {"payload":1},
        [{"payload":2}, {"status":"failed"}]], {"error":"last"}]});
    let projection = codex_tools::ToolOutputProjectionMetadata::from_json(&nested, true, None).essential_inline;
    assert_eq!(projection["results"], serde_json::json!([null, [null, null,
        [null, {"status":"failed"}]], {"error":"last"}]));
}

#[test]
fn verified_evidence_quoted_handles_do_not_replace_live_receipts() {
    let state = serde_json::json!({"session_id":12, "execution_state":"running", "session_capabilities":{"polling":true}});
    for visible in [r#"const example = {"session_id":12};"#.to_string(),
        serde_json::json!({"source":state.to_string()}).to_string(),
        "Historical example: Running command session_id: 12".into(),
        serde_json::json!({"session_id":12}).to_string(),
        serde_json::json!({"session_id":12, "execution_state":"exited", "session_capabilities":{"polling":true}}).to_string()] {
        assert!(!shows_session_handle(&visible, &state), "{visible}");
    }
    assert!(shows_session_handle(&serde_json::json!({"results":[{"value":state}]}).to_string(), &state));
}

#[test]
fn verified_evidence_large_state_remains_spillable_beside_cursor() {
    let value = serde_json::json!({"state": {"status":"x".repeat(100_000)},
        "action":"y".repeat(100_000), "application_id":["z".repeat(100_000)], "nextCursor":"page-2"});
    let projection = codex_tools::ToolOutputProjectionMetadata::from_json(&value, true, None).essential_inline;
    assert_eq!(projection, serde_json::json!({"nextCursor":"page-2"}));
}

#[test]
fn printed_compact_results_are_not_repeated_on_failure() {
    for (tool, raw) in [
        ("read_file", serde_json::json!({"source_sha256":"rev", "canonical_sha256":"rev",
            "complete":true,"delivered_selection_complete":true,"artifact_id":null,
            "results":[{"text":"COMPACT_RESULT_SENTINEL"}]})),
        ("mcp__test__mirror", serde_json::json!({"structuredContent":{"text":"COMPACT_RESULT_SENTINEL"},
            "content":[{"type":"text","text":"{\"text\":\"COMPACT_RESULT_SENTINEL\"}"}]})),
    ] {
        let compact = codex_code_mode::model_visible_tool_result(
            &codex_tools::ToolName::plain(tool), &raw,
        ).unwrap();
        let response = RuntimeResponse::Result {
            cell_id: CellId::new("cell-1".to_string()),
            content_items: vec![RuntimeContentItem::InputText {
                text: serde_json::json!({"result":compact}).to_string(),
            }],
            error_text: Some("later failure".to_string()),
            output_loss: None,
        };
        let evidence = CodeModeNestedResultEvidence {
            tool_name: tool.to_string(), output: raw.to_string(), ..nested_result_evidence("")
        };
        let output = format_runtime_response(response, None, usize::MAX, true, Instant::now(),
            Vec::new(), vec![evidence], None).into_text();
        assert_eq!(output.matches("COMPACT_RESULT_SENTINEL").count(), 1, "{output}");
        assert!(output.contains("later failure"));
    }
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
        let rows = visible.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).collect::<Vec<_>>();
        assert_eq!(rows.iter().filter(|row| row.get("result").is_some() && row.get("tool_name").is_some()).count(), count.min(2));
        let directory = rows.iter().find(|row| row.get("nested_result_recovery_directory").is_some());
        assert_eq!(directory.is_some(), count == 10);
        if let Some(directory) = directory {
            assert_eq!(directory["omitted_inline_result_count"], 8);
            assert_eq!(directory["nested_result_recovery_directory"].as_array().unwrap().len(), count);
        }
    }
}

#[tokio::test]
async fn explicit_exec_budget_above_the_default_is_honored_to_the_ceiling() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: Arc::new(session),
        turn: Arc::new(turn),
    };
    // Middle marker: middle truncation keeps head and tail, so only an intact
    // packet retains it.
    let body = format!(
        "{} MIDDLE_EVIDENCE_MARKER {}",
        "head evidence line\n".repeat(2_500),
        "tail evidence line\n".repeat(2_500),
    );
    let body_tokens = codex_utils_string::approx_token_count(&body);
    assert!(body_tokens > codex_code_mode::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    assert!(body_tokens < codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL);
    for (requested, retained) in [
        (Some(codex_code_mode::MAX_OUTPUT_TOKENS_PER_EXEC_CALL), true),
        (None, false),
    ] {
        let cell = CellId::new(format!("bulk-evidence-{}", requested.is_some()));
        let service = &exec.session.services.code_mode_service;
        service.record_cell_parent_call_id(&cell, "outer-exec");
        let output = super::handle_runtime_response(
            &exec,
            RuntimeResponse::Result {
                output_loss: None,
                cell_id: cell.clone(),
                content_items: vec![RuntimeContentItem::InputText { text: body.clone() }],
                error_text: None,
            },
            requested,
            Instant::now(),
        )
        .unwrap();
        let visible = output.into_text();
        assert_eq!(
            visible.contains("MIDDLE_EVIDENCE_MARKER"),
            retained,
            "requested={requested:?}"
        );
        exec.session
            .services
            .code_mode_service
            .finish_cell_dispatch(&cell);
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
        if index == 99 {
            let state = result.command_state.as_mut().unwrap();
            state["output_reduced"] = true.into();
            state["raw_output_artifact_id"] = "retained-tail".into();
            state["raw_output_artifact_retention_limit_hit"] = true.into();
            state["recovery"] = serde_json::json!({
                "tool": "read_tool_output",
                "arguments": {
                    "artifact_id": "retained-tail",
                    "selectors": [{"kind": "lines", "start": 12, "end": 24}],
                },
            });
        }
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
    assert_eq!(inline.len(), 101);
    assert!(!visible.contains("DISTINCT_"));
    assert!(visible.contains("Running command session_id: 99"));
    let receipt = output.body.iter().find_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } => {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .filter(|value| value["artifact_id"] == "retained-tail")
        }
        _ => None,
    }).expect("text-only output must retain actionable recovery metadata");
    assert_eq!(receipt["recovery"]["arguments"]["selectors"],
        serde_json::json!([{"kind": "lines", "start": 12, "end": 24}]));
    assert_eq!(receipt["raw_output_artifact_retention_limit_hit"], true);
    let canonical = output.canonical_body.as_ref().unwrap();
    let states = canonical
        .iter()
        .find_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => {
                serde_json::from_str::<serde_json::Value>(text)
                    .ok()?
                    .get("nested_commands")
                    .cloned()
            }
            _ => None,
        })
        .unwrap();
    let states = states.as_array().unwrap();
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

#[tokio::test]
async fn live_session_receipt_is_added_only_when_its_handle_is_not_visible() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext {
        session: Arc::new(session),
        turn: Arc::new(turn),
    };
    let service = &exec.session.services.code_mode_service;
    for (name, printed, receipts) in [
        ("envelope", r#"{"session_id":12,"execution_state":"running","session_capabilities":{"polling":true}}"#, 0),
        ("output-only", "building", 1),
        ("longer-id", r#"{"session_id":123,"execution_state":"running"}"#, 1),
    ] {
        let cell = CellId::new(name.to_string());
        service.record_cell_parent_call_id(&cell, &format!("outer-{name}"));
        let ordinal = service.begin_packet_call(&cell).unwrap();
        let mut result = nested_result_evidence("building");
        result.command_state = super::nested_command_state(
            Some(&codex_tools::ToolName::plain("shell_command")), "forwarded",
            &ToolPayload::Function { arguments: "{}".into() },
            &serde_json::json!({"session_id": 12, "process_exited": false,
                "execution_state": "running", "session_capabilities": {"polling": true}}),
        );
        service.complete_packet_call(&cell, ordinal, false, 0, Vec::new(), Some(result), None);
        let output = super::handle_runtime_response(
            &exec,
            RuntimeResponse::Result {
                cell_id: cell.clone(),
                content_items: vec![RuntimeContentItem::InputText {
                    text: printed.into(),
                }],
                error_text: None,
                output_loss: None,
            },
            None,
            Instant::now(),
        )
        .unwrap();
        let visible = super::code_mode_text_content(&output.body);
        assert_eq!(
            visible.matches("Running command session_id: 12").count(),
            receipts,
            "{name}: {visible}"
        );
        service.finish_cell_dispatch(&cell);
    }
}

async fn command_receipt_fixture(
    arguments: serde_json::Value,
    raw: serde_json::Value,
    printed: &str,
    error_text: Option<String>,
) -> FunctionToolOutput {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let exec = super::ExecContext { session: Arc::new(session), turn: Arc::new(turn) };
    let service = &exec.session.services.code_mode_service;
    let cell = CellId::new("recovery-audit".to_string());
    service.record_cell_parent_call_id(&cell, "outer-recovery-audit");
    let ordinal = service.begin_packet_call(&cell).unwrap();
    let mut result = nested_result_evidence("output");
    result.command_state = super::nested_command_state(
        Some(&codex_tools::ToolName::plain("exec_command")), "command",
        &ToolPayload::Function { arguments: arguments.to_string() }, &raw,
    );
    service.complete_packet_call(&cell, ordinal, false, 0, Vec::new(), Some(result), None);
    let output = super::handle_runtime_response(
        &exec,
        RuntimeResponse::Result {
            cell_id: cell.clone(),
            content_items: vec![RuntimeContentItem::InputText { text: printed.into() }],
            error_text,
            output_loss: None,
        },
        None,
        Instant::now(),
    ).unwrap();
    service.finish_cell_dispatch(&cell);
    output
}

#[tokio::test]
async fn recovery_audit_silent_exact_success_does_not_request_recovery() {
    let raw = serde_json::json!({
        "process_exited": true, "exit_code": 0, "execution_state": "exited",
        "streams_complete": true, "output_reduced": true,
        "raw_output_artifact_id": "retained",
        "recovery_selector": {"kind": "lines", "start": 1, "end": 20},
    });
    for (arguments, exact, error, expected) in [
        (serde_json::json!({"max_output_tokens": 0}), true, None, false),
        (serde_json::json!({"max_output_tokens": 0}), false, None, true),
        (serde_json::json!({}), true, None, true),
        (serde_json::json!({"max_output_tokens": 0}), true, Some("script failed".into()), true),
    ] {
        let mut raw = raw.clone();
        raw["streams_complete"] = exact.into();
        let output = command_receipt_fixture(arguments, raw, "summary", error).await;
        let visible = super::code_mode_text_content(&output.body);
        assert_eq!(visible.contains("nested_command_display_reduced"), expected);
        assert!(!visible.contains("\"output_truncated\":true"));
        if expected {
            assert!(visible.contains(&format!("\"cumulative_streams_complete\":{exact}")));
        }
        assert_eq!(output.essential_inline["nested_commands"][0]["raw_output_artifact_id"], "retained");
    }
}

#[tokio::test]
async fn recovery_audit_exited_but_undrained_process_keeps_handle() {
    for (session_id, expected) in [(serde_json::json!(12), true), (serde_json::Value::Null, false)] {
        let output = command_receipt_fixture(serde_json::json!({}), serde_json::json!({
            "process_exited": true, "exit_code": 0, "execution_state": "exited",
            "session_id": session_id, "output_complete": false,
            "session_capabilities": {"incarnation": "exact-creation"},
        }), "last output", None).await;
        assert_eq!(super::code_mode_text_content(&output.body).contains("Running command session_id: 12"), expected);
        assert_eq!(output.essential_inline["nested_commands"][0].get("continuation").is_some(), expected);
    }
}

#[tokio::test]
async fn fork91_live_fallback_preserves_capabilities_without_inventing_them() {
    let capabilities = serde_json::json!({"stdin":false,"interrupt":false,"polling":true,"cancellation":true,"incarnation":"exact-creation"});
    for caps in [capabilities.clone(), serde_json::Value::Null] {
        let raw = serde_json::json!({
            "process_exited":false, "exit_code":null, "execution_state":"running",
            "session_id":12, "session_capabilities":caps, "output_complete":false,
        });
        let output = command_receipt_fixture(serde_json::json!({}), raw.clone(), "progress", None).await;
        let visible = super::code_mode_text_content(&output.body);
        assert!(visible.contains("Running command session_id: 12"));
        assert_eq!(visible.contains("session_capabilities"), caps.is_object());
        if caps.is_object() {
            let receipt = visible.lines().find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()).unwrap();
            assert_eq!(receipt["session_capabilities"], caps);
            assert_eq!(receipt["continuation"]["arguments"]["session_id"], 12);
            assert_eq!(receipt["continuation"]["arguments"]["incarnation"], "exact-creation");
        }
        let output = command_receipt_fixture(serde_json::json!({}), raw.clone(), &raw.to_string(), None).await;
        assert_eq!(super::code_mode_text_content(&output.body).contains("Running command session_id"),
            !caps.is_object(), "a numeric-only historical handle is not a usable capability");
    }
}

#[tokio::test]
async fn fork91_no_match_receipt_is_not_a_failure_but_errors_are() {
    for (exit, no_match, error, failed) in [
        (0, false, None, false), (1, true, None, false), (1, false, None, true),
        (2, true, None, true), (1, true, Some("stderr error"), true),
    ] {
        let output = command_receipt_fixture(serde_json::json!({}), serde_json::json!({
            "process_exited":true, "exit_code":exit, "execution_state":"exited",
            "search_no_match":no_match, "error":error,
        }), "search complete", None).await;
        assert_eq!(super::code_mode_text_content(&output.body).contains("nested_command_failure"), failed);
    }
}

#[tokio::test]
async fn recovery_audit_failure_keeps_bounded_diagnostic_without_changing_success() {
    for error in [None, Some("launch denied: ".to_string() + &"x".repeat(4_000))] {
        let output = command_receipt_fixture(serde_json::json!({}), serde_json::json!({
            "process_exited": error.is_none(), "exit_code": if error.is_none() { Some(0) } else { None },
            "execution_state": if error.is_none() { "exited" } else { "unknown" }, "error": error,
        }), "", None).await;
        assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Success);
        assert_eq!(output.success, Some(true));
        let visible = super::code_mode_text_content(&output.body);
        assert_eq!(visible.contains("launch denied"), error.is_some());
        assert_eq!(visible.contains("nested_command_failure"), error.is_some());
        assert!(visible.len() < 2_000, "diagnostics must stay bounded");
        if let Some(error) = error {
            assert_eq!(output.essential_inline["nested_commands"][0]["error"], error);
        }
    }
}

#[tokio::test]
async fn recovery_audit_missing_artifact_is_explicit_and_not_retryable() {
    for reduced in [false, true] {
        let output = command_receipt_fixture(serde_json::json!({}), serde_json::json!({
            "process_exited": true, "exit_code": 0, "execution_state": "exited",
            "output_reduced": reduced, "raw_output_artifact_error": "disk full",
        }), "partial output", None).await;
        let visible = super::code_mode_text_content(&output.body);
        assert_eq!(visible.contains("\"recovery_available\":false"), reduced);
        assert_eq!(visible.contains("disk full"), reduced);
        assert!(!visible.contains("recovery_tool"));
    }
}

#[tokio::test]
async fn recovery_audit_bare_locator_does_not_hide_exact_selector() {
    let raw = serde_json::json!({
        "process_exited": true, "exit_code": 0, "execution_state": "exited",
        "output_reduced": true, "raw_output_artifact_id": "retained",
        "recovery_selector": {"kind": "lines", "start": 12, "end": 24},
    });
    for (printed, appended) in [
        ("retained".to_string(), true),
        (raw.to_string(), false),
        (serde_json::json!({"results":[{"status":"fulfilled", "value":raw}]}).to_string(), false),
        (serde_json::json!({"source":raw.to_string()}).to_string(), true),
    ] {
        let output = command_receipt_fixture(serde_json::json!({}), raw.clone(), &printed, None).await;
        let visible = super::code_mode_text_content(&output.body);
        assert_eq!(visible.contains("\"recovery\":"), appended);
        assert!(visible.contains("\"start\":12"));
        assert_eq!(output.essential_inline["nested_commands"][0]["recovery"]["arguments"]["selectors"],
            serde_json::json!([raw["recovery_selector"]]));
        assert!(super::code_mode_text_content(output.canonical_body.as_ref().unwrap())
            .contains("retained"));
    }
}
#[tokio::test]
async fn verified10_paginated_recovery_delivers_in_one_cell_without_erasing_failure() {
    for failed_sibling in [false, true] {
        let runtime = PacketRuntime::with_nested_runtime(Arc::new(
            crate::tools::handlers::ReadToolOutputHandler,
        )).await;
        let raw = "λ exact source evidence\n".repeat(1000);
        let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
            &runtime.step.turn.config.codex_home, &runtime.session.thread_id.to_string(),
            &codex_tools::CanonicalToolResult::text(raw.clone()),
        ).await;
        let source = format!(r#"// @exec: {{"deliver":true}}
            const artifact_id = {};
            if ({failed_sibling}) await tools.read_tool_output({{artifact_id, selectors:[{{kind:'lines',start:0,end:0}}]}});
            let selector = {{kind:'bytes',start:0,end:{}}};
            let parts = [], end = 0, pages = 0;
            while (selector) {{
                // A text-only consumer chooses pages aligned to these 25-byte lines.
                // Arbitrary byte boundaries may correctly return data_base64 instead.
                const r = await tools.read_tool_output({{artifact_id, selectors:[selector], max_bytes:4000}});
                for (const row of r.results) {{
                    if (row.status !== 'ok' || typeof row.text !== 'string') continue;
                    if (row.canonical_range.start !== end) throw Error('noncontiguous recovery');
                    parts.push(row.text); end = row.canonical_range.end;
                }}
                pages++;
                if (pages > 64) throw Error('recovery did not progress');
                if (r.complete) break;
                selector = r.continuation_stop?.selector;
                if (!selector) throw Error('missing continuation');
            }}
            if (parts.join('') !== {} || pages < 2) throw Error('incomplete evidence');
            text('verified recovered selection');
        "#, serde_json::to_string(&artifact.artifact_id().unwrap()).unwrap(), raw.len(), serde_json::to_string(&raw).unwrap());
        let output = runtime.exec(&source).await;
        let signal = output.sampling_request_signal().unwrap();
        assert_eq!(signal.get("explicit_completion_message").and_then(serde_json::Value::as_str),
            (!failed_sibling).then_some("verified recovered selection"), "{}", packet_output_text(output.as_ref()));
        if failed_sibling {
            assert!(serde_json::to_string(&output.to_response_item("exec", &ToolPayload::Custom { input: source }))
                .unwrap().contains("nested_work_failed_or_incomplete"));
        }
        runtime.finish().await;
    }
}

#[tokio::test]
async fn completion_audit_retained_intent_requires_authoritative_terminal_commands() {
    for scenario in ["complete", "empty-yield", "partial-yield", "input-changed",
                     "schema-changed", "running", "forwarded-shell", "unknown", "failed", "missing-exit", "deferred",
                     "no-match", "no-match-error", "invalid-no-match"] {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let service = &session.services.code_mode_service;
        let cell = CellId::new(format!("completion-{scenario}"));
        service.record_cell_parent_call_id(&cell, "parent");
        let (activity, receiver) = tokio::sync::watch::channel(crate::session::InputQueueActivity::Mailbox);
        service.record_delivery_intent(&cell, &turn, receiver);
        let mut command = serde_json::json!({
            "execution_state":"exited", "process_exited":true, "exit_code":0
        });
        match scenario {
            "forwarded-shell" => {
                command = super::nested_command_state(
                    Some(&codex_tools::ToolName::plain("shell_command")), "forwarded-call",
                    &ToolPayload::Function { arguments: "{}".into() },
                    &serde_json::json!({"execution_state":"running", "process_exited":false,
                        "session_id":12, "session_capabilities":{"polling":true}}),
                ).expect("forwarded shell command state");
            }
            "running" => { command["execution_state"] = "running".into(); command["process_exited"] = false.into(); }
            "unknown" => command["execution_state"] = "unknown".into(),
            "failed" => command["exit_code"] = 1.into(),
            "no-match" | "no-match-error" | "invalid-no-match" => {
                command["exit_code"] = if scenario == "invalid-no-match" { 2 } else { 1 }.into();
                command["search_no_match"] = true.into();
                if scenario == "no-match-error" { command["error"] = "error".into(); }
            }
            "missing-exit" => { command.as_object_mut().unwrap().remove("exit_code"); }
            "deferred" => command["pending_deferred_completions"] = serde_json::json!(["required-job"]),
            "input-changed" => { activity.send_replace(crate::session::InputQueueActivity::Steer); }
            "schema-changed" => turn.final_output_json_schema = Some(serde_json::json!({"type":"string"})),
            _ => {}
        }
        service.packet_admission.lock().unwrap().cells.get_mut(cell.as_str()).unwrap()
            .command_states.push(command);
        if matches!(scenario, "empty-yield" | "partial-yield") {
            let yielded = RuntimeResponse::ExplicitYield {
                cell_id: cell.clone(), content_items: if scenario == "partial-yield" {
                    vec![RuntimeContentItem::InputText { text: "partial".into() }]
                } else { Vec::new() },
            };
            let refusal = service.delivery_for_response(&cell, &turn, &yielded).unwrap_err();
            assert_eq!(refusal["category"], if scenario == "partial-yield" { "partial_output" } else { "cell_running" });
            assert_eq!(refusal["cell_id"], cell.as_str());
            service.finish_packet(cell.as_str(), true);
        }
        let response = RuntimeResponse::Result {
            cell_id: cell.clone(),
            content_items: vec![RuntimeContentItem::InputText { text: "verified result".into() }],
            error_text: None, output_loss: None,
        };
        let decision = service.delivery_for_response(&cell, &turn, &response);
        if !matches!(scenario, "complete" | "empty-yield" | "no-match" | "partial-yield") {
            assert!(decision.as_ref().unwrap_err()["category"].is_string(), "{scenario}");
        }
        assert_eq!(decision.ok().flatten().as_deref(),
            matches!(scenario, "complete" | "empty-yield" | "no-match").then_some("verified result"), "{scenario}");
        assert!(service.delivery_for_response(&cell, &turn, &response).unwrap().is_none(), "intent is consumed once");
        service.finish_cell_dispatch(&cell);
    }
}
