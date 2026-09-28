//! Native-progress regression and opt-in mechanism benchmarks.
//! The replay comparison uses the existing direct path as a proxy, not a nested-path fix.

use super::*;
use crate::session::turn_context::TurnEnvironment;
use crate::session::turn_execution::CodeModeToolResult;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::ListFilesHandler;
use crate::tools::handlers::ReadFileHandler;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolPayload;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

#[expect(clippy::print_stdout, reason = "emits the opt-in benchmark report")]
fn save_report(name: &str, report: &Value) {
    let root = std::env::var_os("KD4_TURN_REVIEW_BENCH_DIR")
        .expect("set KD4_TURN_REVIEW_BENCH_DIR for these opt-in benchmarks");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        PathBuf::from(root).join(name),
        serde_json::to_vec_pretty(report).unwrap(),
    )
    .unwrap();
    println!("{name}: {report}");
}

async fn fixture(workspace: &Path) -> (Arc<Session>, Arc<TurnContext>) {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd = AbsolutePathBuf::from_absolute_path(workspace).unwrap();
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(workspace).unwrap(),
        None,
    )];
    (Arc::new(session), Arc::new(turn))
}

async fn native_result(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    tool: &str,
    args: &Value,
) -> (Value, Option<Value>) {
    let payload = ToolPayload::Function {
        arguments: args.to_string(),
    };
    let invocation = ToolInvocation {
        session: Arc::clone(session),
        step_context: StepContext::for_test(Arc::clone(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: "native-read".into(),
        tool_name: ToolName::plain(tool),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    };
    let result = if tool == "read_file" {
        ReadFileHandler.handle(invocation).await.unwrap()
    } else {
        ListFilesHandler.handle(invocation).await.unwrap()
    };
    assert_eq!(result.outcome_for_logging(), ToolOutputOutcome::Success);
    let signal = result.sampling_request_signal();
    let value = result.code_mode_result(&payload);
    assert_eq!(value["complete"], true);
    (value, signal)
}

fn text_response(call_id: &str, text: String) -> ResponseInputItem {
    ResponseInputItem::FunctionCallOutput {
        call_id: call_id.into(),
        output: codex_protocol::models::FunctionCallOutputPayload::from_text(text),
    }
}

fn observe_native(
    control: &mut TurnExecutionControl,
    nested: bool,
    tool: &str,
    args: &Value,
    value: &Value,
    signal: Option<Value>,
) -> bool {
    let baselines = control.baselines(0);
    let collector = control.collector(&baselines);
    let tool_name = ToolName::plain(tool);
    let payload = ToolPayload::Function {
        arguments: args.to_string(),
    };
    let success = ToolOutputOutcomeContext::new(ToolOutputOutcome::Success);
    if nested {
        let outer = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &ToolPayload::Custom {
                input: "await tools.read_file(args)".into(),
            },
            "outer",
        );
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell",
            tool_name: &tool_name,
            payload: &payload,
            source_dependencies: None,
            outcome_context: success,
            signal: signal.as_ref(),
            result: value,
            canonical_artifact_required: false,
        });
        collector.record_response_result(
            outer.ordinal,
            success,
            None,
            &text_response("outer", value.to_string()),
            false,
        );
    } else {
        let call = collector.register_deterministic_tool_call(&tool_name, &payload, "read");
        collector.record_response_result(
            call.ordinal,
            success,
            signal,
            &text_response("read", value.to_string()),
            false,
        );
    }
    let settled = SamplingRequestSettledState {
        mutation_revision: 0,
        tool_exposure_revision: 0,
    };
    control.settle(&baselines, &collector, &settled);
    let progressed = control.observe_budget_progress(&baselines, &collector, &settled);
    control.evaluate_convergence(&baselines, &collector, &settled);
    progressed
}

#[tokio::test]
#[ignore = "opt-in turn-review benchmark"]
async fn native_read_progress_benchmark() {
    save_report("native-progress.json", &native_read_progress_report().await);
}

#[tokio::test]
async fn native_read_progress_preserves_generation_capacity() {
    native_read_progress_report().await;
}

async fn native_read_progress_report() -> Value {
    let workspace = tempfile::tempdir().unwrap();
    for index in 0..70 {
        let dir = workspace.path().join(format!("scope-{index}"));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("evidence.txt"),
            format!("fact-{index}\nsecond line\n"),
        )
        .unwrap();
    }
    let (session, turn) = fixture(workspace.path()).await;
    let mut states = (0..4)
        .map(|_| {
            (
                TurnExecutionControl::new(),
                LogicalGenerationBudget::default(),
                0usize,
                None,
                None,
            )
        })
        .collect::<Vec<_>>();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    let started = Instant::now();
    for index in 0..70 {
        let (tool, args) = if index % 2 == 0 {
            (
                "read_file",
                json!({"path": format!("scope-{index}/evidence.txt")}),
            )
        } else {
            (
                "list_files",
                json!({"path": format!("scope-{index}"), "max_depth": 0}),
            )
        };
        let (value, actual_signal) = native_result(&session, &turn, tool, &args).await;
        assert!(
            actual_signal.is_some(),
            "successful native reads must supply evidence"
        );
        if tool == "read_file" {
            assert_eq!(
                value["results"][0]["text"],
                format!("fact-{index}\nsecond line\n")
            );
        } else {
            assert_eq!(value["entries"].as_array().unwrap().len(), 1);
        }
        for (variant, (control, budget, progress, terminal, warning)) in
            states.iter_mut().enumerate()
        {
            if control.take_soft_convergence_directive(index > 0).is_some() {
                *warning = Some(index + 1);
            }
            if matches!(
                budget.admit(false),
                LogicalGenerationAdmission::Terminal { forced: true }
            ) {
                *terminal = Some(index + 1);
            }
            // Removing the production signal reproduces the old missing-wire baseline.
            let signal = (variant % 2 == 1).then(|| actual_signal.clone()).flatten();
            let progressed = observe_native(control, variant >= 2, tool, &args, &value, signal);
            *progress += usize::from(progressed);
            budget.observe_progress(progressed, false);
        }
    }
    let native_io_and_four_collectors_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut variants = Vec::new();
    for (index, (_, _, progress, terminal, warning)) in states.iter().enumerate() {
        let candidate = index % 2 == 1;
        assert_eq!(*progress, if candidate { 70 } else { 0 });
        assert_eq!(
            *terminal,
            if candidate {
                None
            } else {
                Some(MAX_REGULAR_LOGICAL_GENERATIONS as usize + 1)
            }
        );
        assert_eq!(*warning, if candidate { None } else { Some(4) });
        variants.push(json!({"nested": index >= 2, "production_signal": candidate,
            "new_evidence_generations": progress, "forced_terminal_at": terminal, "soft_warning_at": warning}));
    }
    let mut control = TurnExecutionControl::new();
    let args =
        json!({"path": "scope-0/evidence.txt", "selectors": [{"kind":"lines", "start":1,"end":1}]});
    let (first, first_signal) = native_result(&session, &turn, "read_file", &args).await;
    let (repeat, repeat_signal) = native_result(&session, &turn, "read_file", &args).await;
    assert_ne!(first["artifact_id"], repeat["artifact_id"]);
    for (value, signal, expected) in [
        (&first, first_signal, true),
        (&repeat, repeat_signal, false),
    ] {
        assert_eq!(
            observe_native(&mut control, true, "read_file", &args, value, signal),
            expected
        );
    }
    std::fs::write(
        workspace.path().join("scope-0/evidence.txt"),
        "fact-0\nunread edit\n",
    )
    .unwrap();
    let (unread_edit, unread_signal) = native_result(&session, &turn, "read_file", &args).await;
    assert_ne!(first["source_sha256"], unread_edit["source_sha256"]);
    assert_eq!(unread_edit["results"][0]["text"], "fact-0\n");
    assert!(!observe_native(
        &mut control,
        true,
        "read_file",
        &args,
        &unread_edit,
        unread_signal
    ));
    let next_range =
        json!({"path": "scope-0/evidence.txt", "selectors": [{"kind":"lines", "start":2,"end":2}]});
    let (second, second_signal) = native_result(&session, &turn, "read_file", &next_range).await;
    assert_eq!(second["results"][0]["text"], "unread edit\n");
    assert!(observe_native(
        &mut control,
        true,
        "read_file",
        &next_range,
        &second,
        second_signal
    ));
    std::fs::write(
        workspace.path().join("scope-0/evidence.txt"),
        "changed\nsecond line\n",
    )
    .unwrap();
    let (changed, changed_signal) = native_result(&session, &turn, "read_file", &args).await;
    assert_eq!(changed["results"][0]["text"], "changed\n");
    assert!(observe_native(
        &mut control,
        true,
        "read_file",
        &args,
        &changed,
        changed_signal
    ));
    let (other_session, mut other_turn) = fixture(workspace.path()).await;
    Arc::get_mut(&mut other_turn)
        .unwrap()
        .environments
        .turn_environments[0]
        .environment_id = "other-local-view".into();
    let (other, other_signal) =
        native_result(&other_session, &other_turn, "read_file", &args).await;
    assert_eq!(other["results"][0]["text"], changed["results"][0]["text"]);
    assert!(observe_native(
        &mut control,
        true,
        "read_file",
        &args,
        &other,
        other_signal
    ));

    std::fs::create_dir(workspace.path().join("empty")).unwrap();
    let listing_args = json!({"path": "empty"});
    for expected in [true, false] {
        let (listing, signal) = native_result(&session, &turn, "list_files", &listing_args).await;
        assert_eq!(listing["entries"], json!([]));
        assert_eq!(
            observe_native(
                &mut control,
                true,
                "list_files",
                &listing_args,
                &listing,
                signal
            ),
            expected
        );
    }
    std::fs::write(workspace.path().join("empty/new.txt"), "first").unwrap();
    let (listing, signal) = native_result(&session, &turn, "list_files", &listing_args).await;
    assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
    assert!(observe_native(
        &mut control,
        true,
        "list_files",
        &listing_args,
        &listing,
        signal
    ));
    std::fs::write(
        workspace.path().join("empty/new.txt"),
        "contents are not listed",
    )
    .unwrap();
    let (listing, signal) = native_result(&session, &turn, "list_files", &listing_args).await;
    assert!(!observe_native(
        &mut control,
        true,
        "list_files",
        &listing_args,
        &listing,
        signal
    ));
    tokio::time::resume();
    json!({
        "scope":"real native handlers + production collectors/budget; no model requests",
        "clock":"120-second soft-warning threshold advanced, not wall-clock measured",
        "variants": variants, "native_io_and_four_collectors_ms":native_io_and_four_collectors_ms,
        "controls":{"repeated_bytes_no_progress":true,"artifact_id_noise_ignored":true,
            "new_range_progress":true,"external_edit_progress":true,"unread_edit_no_progress":true,
            "environment_change_progress":true,"empty_listing_evidence":true,"listing_change_progress":true},
    })
}

struct SearchHandler {
    cwd: PathBuf,
    executions: Arc<AtomicUsize>,
}

impl ToolExecutor<ToolInvocation> for SearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("exec_command")
    }
    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "exec_command".into(),
            description: "Benchmark real rg read".into(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }
    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }
    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let mut command = tokio::process::Command::new("rg");
            #[cfg(windows)]
            command.creation_flags(0x08000000);
            let output = command
                .args(["-n", "needle", "source.txt"])
                .current_dir(&self.cwd)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(Box::new(FunctionToolOutput::from_text(
                String::from_utf8(output.stdout).unwrap(),
                Some(true),
            )) as Box<dyn ToolOutput>)
        })
    }
}
impl CoreToolRuntime for SearchHandler {}

async fn replay_trial(bytes: usize, nested: bool, edit: bool, force_fresh: bool) -> Value {
    let workspace = tempfile::tempdir().unwrap();
    assert!(
        tokio::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .await
            .unwrap()
            .success()
    );
    let contents = format!("needle\n{}", "padding\n".repeat(bytes / 8));
    std::fs::write(workspace.path().join("source.txt"), contents).unwrap();
    let (session, turn) = fixture(workspace.path()).await;
    let executions = Arc::new(AtomicUsize::new(0));
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([Arc::new(SearchHandler {
            cwd: workspace.path().to_path_buf(),
            executions: Arc::clone(&executions),
        }) as Arc<dyn CoreToolRuntime>]),
        Vec::new(),
    ));
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let mut control = TurnExecutionControl::new();
    let mut per_call_ms = Vec::new();
    for index in 0..4 {
        if edit && index == 1 {
            std::fs::write(
                workspace.path().join("source.txt"),
                "needle changed\npadding\n",
            )
            .unwrap();
        }
        let baseline = control.baselines(0);
        let collector = control.collector(&baseline);
        let step =
            StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(Arc::clone(&router));
        let runtime = ToolCallRuntime::new(Arc::clone(&session), step, Arc::clone(&tracker))
            .with_sampling_request_signals(collector.clone());
        let payload = ToolPayload::Function {
            arguments: if force_fresh {
                json!({"cmd":"rg -n needle source.txt", "force_fresh":true})
            } else {
                json!({"cmd":"rg -n needle source.txt"})
            }
            .to_string(),
        };
        let call = ToolCall {
            tool_name: ToolName::plain("exec_command"),
            call_id: format!("read-{index}"),
            payload: payload.clone(),
        };
        let start = Instant::now();
        let response = if nested {
            let outer = collector.register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Custom {
                    input: "await tools.exec_command(args)".into(),
                },
                "outer",
            );
            let result = runtime
                .clone()
                .handle_tool_call_with_source(
                    call,
                    ToolCallSource::CodeMode {
                        cell_id: format!("cell-{index}"),
                        parent_call_id: Some(format!("outer-{index}")),
                        runtime_tool_call_id: format!("nested-{index}"),
                        nested_deadline: None,
                        cancellation_cause: None,
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            let response = result.response();
            let signal = result.sampling_request_signal();
            let dependencies = result.projected_source_dependencies().cloned();
            let outcome = result.outcome_context();
            let value = result.code_mode_result();
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "cell",
                tool_name: &ToolName::plain("exec_command"),
                payload: &payload,
                source_dependencies: dependencies,
                outcome_context: outcome,
                signal: signal.as_ref(),
                result: &value,
                canonical_artifact_required: false,
            });
            collector.record_response_result(
                outer.ordinal,
                ToolOutputOutcomeContext::new(ToolOutputOutcome::Success),
                None,
                &text_response("outer", value.to_string()),
                false,
            );
            response
        } else {
            runtime
                .clone()
                .handle_tool_call(call, CancellationToken::new())
                .await
                .unwrap()
        };
        runtime.flush_workspace_evidence_generation().await.unwrap();
        let settled = SamplingRequestSettledState {
            mutation_revision: tracker.lock().await.current_mutation_revision(),
            tool_exposure_revision: 0,
        };
        assert_eq!(settled.mutation_revision, 0, "read must not mutate");
        control.settle(&baseline, &collector, &settled);
        per_call_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            panic!("read output");
        };
        assert_eq!(
            output.text_content(),
            Some(if edit && index >= 1 {
                "1:needle changed\n"
            } else {
                "1:needle\n"
            })
        );
    }
    let count = executions.load(Ordering::SeqCst);
    assert_eq!(
        count,
        if nested || force_fresh {
            4
        } else if edit {
            2
        } else {
            1
        }
    );
    json!({"nested":nested,"edit":edit,"force_fresh":force_fresh,"fixture_bytes":bytes + 7,
        "producer_executions":count,"per_call_ms":per_call_ms,"total_ms":per_call_ms.iter().sum::<f64>()})
}

#[tokio::test]
#[ignore = "opt-in turn-review benchmark"]
async fn successful_read_replay_benchmark() {
    let mut samples = Vec::new();
    for bytes in [64 * 1024, 4 * 1024 * 1024] {
        for pair in 0..6 {
            for nested in if pair % 2 == 0 {
                [true, false]
            } else {
                [false, true]
            } {
                let mut sample = replay_trial(bytes, nested, false, false).await;
                sample["pair"] = json!(pair);
                samples.push(sample);
            }
        }
    }
    let changed = replay_trial(64 * 1024, false, true, false).await;
    let fresh = replay_trial(64 * 1024, false, false, true).await;
    save_report(
        "replay-paths.json",
        &json!({
            "scope":"production dispatch and freshness; real rg child; existing direct replay is a proxy for proposed nested reuse",
            "excluded":"session setup, JS runtime, provider requests, model tokens, hooks; not an integrated nested implementation",
            "samples":samples,"controls":{"external_edit":changed,"force_fresh":fresh},
        }),
    );
}
