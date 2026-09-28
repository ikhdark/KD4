//! Native-read progress regressions and opt-in mechanism timing.

use super::*;
use crate::session::turn_context::TurnEnvironment;
use crate::session::turn_execution::CodeModeToolResult;
use crate::tools::context::ToolCallSource;
use crate::tools::handlers::ListFilesHandler;
use crate::tools::handlers::ReadFileHandler;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutputOutcome;
use codex_tools::ToolOutputOutcomeContext;
use codex_tools::ToolPayload;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;

struct Observation {
    tool: ToolName,
    payload: ToolPayload,
    signal: Option<Value>,
    response: ResponseInputItem,
    value: Value,
}

fn assess(observations: &[&Observation], use_signal: bool, nested: bool) -> (usize, usize) {
    let mut control = TurnExecutionControl::new();
    let mut budget = LogicalGenerationBudget::default();
    let mut admitted = 0;
    let mut progress_count = 0;
    for observation in observations {
        if budget.admit(false) != LogicalGenerationAdmission::Regular {
            break;
        }
        admitted += 1;
        let baselines = control.baselines(0);
        let collector = control.collector(&baselines);
        let signal = if use_signal {
            observation.signal.clone()
        } else {
            None
        };
        let success = ToolOutputOutcomeContext::new(ToolOutputOutcome::Success);
        if nested {
            let outer = collector.register_deterministic_tool_call(
                &ToolName::plain("exec"),
                &ToolPayload::Custom {
                    input: "text(await native_read());".into(),
                },
                "outer",
            );
            collector.record_code_mode_result(CodeModeToolResult {
                cell_id: "benchmark-cell",
                tool_name: &observation.tool,
                payload: &observation.payload,
                source_dependencies: None,
                outcome_context: success,
                signal: signal.as_ref(),
                result: &observation.value,
                canonical_artifact_required: false,
            });
            collector.record_response_result(
                outer.ordinal,
                success,
                None,
                &observation.response,
                false,
            );
        } else {
            let registration = collector.register_deterministic_tool_call(
                &observation.tool,
                &observation.payload,
                "read",
            );
            collector.record_response_result(
                registration.ordinal,
                success,
                signal,
                &observation.response,
                false,
            );
        }
        let settled = SamplingRequestSettledState {
            mutation_revision: 0,
            tool_exposure_revision: 0,
        };
        control.settle(&baselines, &collector, &settled);
        let progress = control.observe_budget_progress(&baselines, &collector, &settled);
        progress_count += usize::from(progress);
        budget.observe_progress(progress, false);
        control.evaluate_convergence(&baselines, &collector, &settled);
    }
    (admitted, progress_count)
}

async fn observe(runtime: &ToolCallRuntime, tool: &str, arguments: Value, id: &str) -> Observation {
    let tool = ToolName::plain(tool);
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let result = runtime
        .clone()
        .handle_tool_call_with_source(
            ToolCall {
                tool_name: tool.clone(),
                call_id: id.into(),
                payload: payload.clone(),
            },
            ToolCallSource::Direct,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.outcome_for_logging(), ToolOutputOutcome::Success);
    let signal = result.sampling_request_signal();
    let response = result.response();
    let value = result.code_mode_result();
    assert_eq!(value["complete"], true);
    Observation {
        tool,
        payload,
        signal,
        response,
        value,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_reads_renew_generation_capacity_only_for_new_evidence() {
    native_read_progress(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in filesystem and generation-admission benchmark"]
async fn native_read_progress_efficiency_benchmark() {
    native_read_progress(true).await;
}

async fn native_read_progress(measure: bool) {
    let workspace = tempfile::tempdir().unwrap();
    for index in 0..80 {
        let directory = workspace.path().join(format!("dir-{index:03}"));
        std::fs::create_dir(&directory).unwrap();
        // Identical content at different paths is deliberately distinct coverage.
        std::fs::write(directory.join("source.txt"), "first\nsecond\n").unwrap();
    }
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd =
        AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
    turn.permission_profile = codex_protocol::models::PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(workspace.path()).unwrap(),
        None,
    )];
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([
            Arc::new(ReadFileHandler) as Arc<dyn CoreToolRuntime>,
            Arc::new(ListFilesHandler) as Arc<dyn CoreToolRuntime>,
        ]),
        Vec::new(),
    ));
    let step = StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router);
    let runtime = ToolCallRuntime::new(
        Arc::new(session),
        step,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let mut report = Vec::new();
    for tool in ["read_file", "list_files"] {
        let started = Instant::now();
        let mut observations = Vec::new();
        for index in 0..80 {
            let path = if tool == "read_file" {
                format!("dir-{index:03}/source.txt")
            } else {
                format!("dir-{index:03}")
            };
            let observation = observe(
                &runtime,
                tool,
                json!({"path": path}),
                &format!("{tool}-{index}"),
            )
            .await;
            assert!(
                observation.signal.is_some(),
                "native read must deliver evidence to the production controller"
            );
            if tool == "read_file" {
                assert_eq!(observation.value["results"][0]["text"], "first\nsecond\n");
            } else {
                assert_eq!(observation.value["entries"].as_array().unwrap().len(), 1);
            }
            observations.push(observation);
        }
        let dispatch_ms = started.elapsed().as_secs_f64() * 1000.0;
        let unique = observations.iter().collect::<Vec<_>>();
        let repeated = vec![&observations[0]; 80];
        let limit = MAX_REGULAR_LOGICAL_GENERATIONS as usize;
        for nested in [false, true] {
            assert_eq!(assess(&unique, false, nested), (limit, 0));
            assert_eq!(assess(&unique, true, nested), (80, 80));
            assert_eq!(assess(&repeated, true, nested), (limit + 1, 1));
            let mut samples = Vec::new();
            for use_signal in [false, true].into_iter().filter(|_| measure) {
                let mut elapsed = Vec::new();
                // Equal admitted workloads: do not time early-stop vs full completion.
                for _ in 0..20 {
                    let started = Instant::now();
                    std::hint::black_box(assess(&unique[..limit], use_signal, nested));
                    elapsed.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                elapsed.sort_by(f64::total_cmp);
                samples.push(json!({"production_signal": use_signal, "median_ms": elapsed[10], "min_ms": elapsed[0], "max_ms": elapsed[19]}));
            }
            report.push(json!({
                "tool": tool, "nested_collector": nested, "real_dispatches": 80,
                "dispatch_ms_shared_by_both_cases": dispatch_ms,
                "baseline_admitted_of_80": limit, "candidate_admitted_of_80": 80,
                "baseline_new_evidence": 0, "candidate_new_evidence": 80,
                "repeated_admitted": limit + 1, "repeated_new_evidence": 1,
                "equal_workload_observations": limit,
                "equal_workload_reducer_samples": samples,
            }));
        }
    }
    let first = observe(
        &runtime,
        "read_file",
        json!({"path":"dir-000/source.txt", "selectors":[{"kind":"lines","start":1,"end":1}]}),
        "range-1",
    )
    .await;
    let repeated = observe(
        &runtime,
        "read_file",
        json!({"path":"dir-000/source.txt", "selectors":[{"kind":"lines","start":1,"end":1}]}),
        "range-1-again",
    )
    .await;
    let second = observe(
        &runtime,
        "read_file",
        json!({"path":"dir-000/source.txt", "selectors":[{"kind":"lines","start":2,"end":2}]}),
        "range-2",
    )
    .await;
    assert_ne!(first.value["artifact_id"], repeated.value["artifact_id"]);
    assert_eq!(first.signal, repeated.signal);
    assert_eq!(assess(&[&first, &repeated, &second], true, false), (3, 2));
    std::fs::write(
        workspace.path().join("dir-000/source.txt"),
        "first\nunobserved change\n",
    )
    .unwrap();
    let unobserved = observe(
        &runtime,
        "read_file",
        json!({"path":"dir-000/source.txt", "selectors":[{"kind":"lines","start":1,"end":1}]}),
        "unobserved-change",
    )
    .await;
    assert_eq!(first.signal, unobserved.signal);
    assert_eq!(assess(&[&first, &unobserved], true, false), (2, 1));
    std::fs::write(
        workspace.path().join("dir-000/source.txt"),
        "changed\nsecond\n",
    )
    .unwrap();
    let changed = observe(
        &runtime,
        "read_file",
        json!({"path":"dir-000/source.txt", "selectors":[{"kind":"lines","start":1,"end":1}]}),
        "changed-range",
    )
    .await;
    assert_eq!(assess(&[&first, &changed], true, false), (2, 2));
    if !measure {
        return;
    }
    let output = std::path::PathBuf::from(
        std::env::var_os("NATIVE_READ_BENCH_OUTPUT").expect("benchmark output path"),
    );
    assert!(
        output.is_absolute(),
        "benchmark output must be absolute: {output:?}"
    );
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(
        output,
        serde_json::to_vec_pretty(
            &json!({"cases": report, "range_and_mutation_controls": "passed"}),
        )
        .unwrap(),
    )
    .unwrap();
}
