use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::ReadFileHandler;
use crate::tools::handlers::ReadToolOutputHandler;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ToolOutput;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn settled() -> SamplingRequestSettledState {
    SamplingRequestSettledState {
        mutation_revision: 0,
        tool_exposure_revision: 0,
    }
}

async fn invocation(name: &str, arguments: Value) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: "evidence-call".into(),
        tool_name: ToolName::plain(name),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn record(
    collector: &SamplingRequestSignalCollector,
    name: &str,
    payload: &ToolPayload,
    result: &dyn ToolOutput,
    call_id: &str,
) {
    let registration =
        collector.register_deterministic_tool_call(&ToolName::plain(name), payload, call_id);
    collector.record_response_result(
        registration.ordinal,
        result.outcome_context(),
        result.sampling_request_signal(),
        &result.to_response_item(call_id, payload),
        false,
    );
}

#[tokio::test]
async fn evidence_reuse_preserves_unscoped_commands_and_substantive_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.py");
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    for (text, novel) in [
        ("if ready:\n    act()\n", true),
        ("if ready:\n    act()\n", false),
        ("if ready:\nact()\n", true),
        ("timeout = 10ms\n", true),
        ("timeout = 20ms\n", true),
        ("diff --git a/a b/a\nold mode 100644\nnew mode 100755\n", true),
        ("diff --git a/b b/b\nold mode 100644\nnew mode 100755\n", true),
        ("diff --git a/b b/b\nold mode 100755\nnew mode 100644\n", true),
    ] {
        let mut result = command_result("exec_command", &path, text, 100).await;
        result.source_dependencies = None;
        let collector = control.collector(&baseline);
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"), &result.payload, &result.call_id,
        );
        assert!(result.sampling_request_signal().unwrap().get("semantic_evidence").is_some());
        collector.record_response_result(
            registration.ordinal, result.outcome_context(), result.sampling_request_signal(),
            &result.response(), false,
        );
        assert_eq!(
            control.observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence),
            novel, "{text}",
        );
    }
}

#[tokio::test]
async fn evidence_reuse_accumulates_native_and_artifact_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
    let mut call = invocation("read_file", json!({
        "path":path, "selectors":[{"kind":"lines","start":1,"end":1}]
    })).await;
    let initial = ReadFileHandler.handle(call.clone()).await.unwrap();
    let artifact = initial.code_mode_result(&call.payload)["artifact_id"].clone();
    assert!(artifact.is_string());
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    record(&collector, "read_file", &call.payload, initial.as_ref(), "initial");
    assert!(control.observe_progress(&baseline, &collector, &settled())
        .contains(&TurnTimingProgressKind::NewSourceEvidence));
    for (name, selector, novel) in [
        ("read_tool_output", json!({"kind":"lines","start":1,"end":1}), false),
        ("read_tool_output", json!({"kind":"lines","start":2,"end":2}), true),
        ("read_file", json!({"kind":"lines","start":1,"end":2}), false),
        ("read_file", json!({"kind":"lines","start":2,"end":3}), true),
        ("read_tool_output", json!({"kind":"bytes","start":0,"end":19}), false),
        ("read_file", json!({"kind":"bytes","start":7,"end":9}), false),
    ] {
        call.tool_name = ToolName::plain(name);
        call.payload = ToolPayload::Function {
            arguments: if name == "read_file" {
                json!({"path":path,"selectors":[selector]})
            } else {
                json!({"artifact_id":artifact,"selectors":[selector]})
            }.to_string(),
        };
        let output = if name == "read_file" {
            ReadFileHandler.handle(call.clone()).await.unwrap()
        } else {
            ReadToolOutputHandler.handle(call.clone()).await.unwrap()
        };
        assert!(output.success_for_logging());
        let collector = control.collector(&baseline);
        record(&collector, name, &call.payload, output.as_ref(), "coverage");
        assert_eq!(control.observe_progress(&baseline, &collector, &settled())
            .contains(&TurnTimingProgressKind::NewSourceEvidence), novel, "{name}: {selector}");
    }
    // A fresh hash must reopen coverage, even at an already-covered path/range.
    std::fs::write(&path, "changed\n").unwrap();
    call.payload = ToolPayload::Function {
        arguments: json!({"path":path,"force_fresh":true}).to_string(),
    };
    let output = ReadFileHandler.handle(call.clone()).await.unwrap();
    let collector = control.collector(&baseline);
    record(&collector, "read_file", &call.payload, output.as_ref(), "changed");
    assert!(control.observe_progress(&baseline, &collector, &settled())
        .contains(&TurnTimingProgressKind::NewSourceEvidence));
}

#[tokio::test]
async fn evidence_reuse_search_hydration_counts_as_delivered_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, "first\nneedle\nlast\n").unwrap();
    let mut call = invocation("read_file", json!({"path":path})).await;
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    for (selector, novel) in [
        (json!({"kind":"search","query":"needle","context_lines":1}), true),
        (json!({"kind":"lines","start":1,"end":3}), false),
        (json!({"kind":"search","query":"needle","context_lines":1}), false),
        (json!({"kind":"search","query":"absent"}), true),
        (json!({"kind":"search","query":"absent"}), false),
    ] {
        call.payload = ToolPayload::Function {
            arguments: json!({"path":path,"selectors":[selector]}).to_string(),
        };
        let output = ReadFileHandler.handle(call.clone()).await.unwrap();
        let collector = control.collector(&baseline);
        record(&collector, "read_file", &call.payload, output.as_ref(), "search");
        assert_eq!(control.observe_progress(&baseline, &collector, &settled())
            .contains(&TurnTimingProgressKind::NewSourceEvidence), novel, "{selector}");
    }
}

#[test]
fn evidence_reuse_live_stdout_resets_soft_pressure_without_source_credit() {
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    for text in ["", "working\n", "", "working\n"] {
        let output = ExecCommandToolOutput {
            validation: None, event_call_id: "poll".into(), chunk_id: "chunk".into(),
            wall_time: std::time::Duration::ZERO, raw_output: text.as_bytes().to_vec(),
            truncation_policy: TruncationPolicy::Tokens(100), max_output_tokens: Some(100),
            process_id: Some(42), session_capabilities: None, exit_code: None,
            process_exited: false, search_no_match: false, original_token_count: None,
            hook_command: None, raw_output_artifact: None, repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        let payload = ToolPayload::Function { arguments: json!({"session_id":42}).to_string() };
        let collector = control.collector(&baseline);
        record(&collector, "write_stdin", &payload, &output, "poll");
        assert!(control.observe_progress(&baseline, &collector, &settled()).is_empty());
        assert_eq!(control.continuations_without_progress, u32::from(text.is_empty()));
    }
}

#[tokio::test]
async fn evidence_reuse_native_replay_tracks_paths_authority_and_freshness() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    std::fs::write(&source, "source").unwrap();
    let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let observations = cache.begin_source_path_change_observations(
        root.path(), &[(source.clone(), false)],
    ).await.unwrap();
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let payload = ToolPayload::Function { arguments: json!({"path":source}).to_string() };
    let collector = control.collector(&baseline);
    let registration = collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "initial",
    );
    collector.record_replay_dependencies(registration.ordinal, 0, observations, None);
    collector.record_response_result(
        registration.ordinal, ToolOutputOutcomeContext::new(ToolOutputOutcome::Success), None,
        &ResponseInputItem::FunctionCallOutput {
            call_id: "initial".into(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text("source".into()),
        }, false,
    );
    control.settle(&baseline, &collector, &settled());
    cache.note_host_workspace_mutation_paths(root.path(), &["unrelated.txt".into()]).await;
    let collector = control.collector(&control.baselines(1));
    let guard = collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "reused",
    ).replayed_success.expect("unrelated edits preserve the replay candidate");
    assert!(guard.is_fresh(1, &cache, None));
    assert!(guard.matches_source_dependencies(&BTreeSet::from([SourceDependencyV1::new(&source, false)])));
    assert!(!guard.matches_source_dependencies(&BTreeSet::from([SourceDependencyV1::new(&root.path().join("other.txt"), false)])));
    assert!(!guard.matches_source_dependencies(&BTreeSet::new()));
    let fresh_payload = ToolPayload::Function {
        arguments: json!({"path":source,"force_fresh":true}).to_string(),
    };
    assert!(collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &fresh_payload, "fresh",
    ).replayed_success.is_none());
    control.input_revision += 1;
    assert!(control.collector(&control.baselines(1)).register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "new-input",
    ).replayed_success.is_none());
    cache.note_host_workspace_mutation_paths(root.path(), &["source.txt".into()]).await;
    assert!(!guard.is_fresh(2, &cache, None), "a relevant edit invalidates the original proof");
}

#[tokio::test]
async fn native_read_progress_uses_delivered_hash_and_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, "first\nsecond\n").unwrap();
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let base = invocation("read_file", json!({"path":path})).await;
    for (selectors, expected) in [
        (json!([{"kind":"lines","start":1,"end":1}]), true),
        (json!([{"kind":"bytes","start":0,"end":6}]), false),
        (json!([{"kind":"lines","start":2,"end":2}]), true),
        (json!([{"kind":"lines","start":2,"end":2}]), false),
        (json!([{"kind":"lines","start":0,"end":0}]), false),
    ] {
        let mut call = base.clone();
        call.payload = ToolPayload::Function {
            arguments: json!({"path":path,"selectors":selectors}).to_string(),
        };
        let result = ReadFileHandler.handle(call.clone()).await.unwrap();
        let collector = control.collector(&baseline);
        record(
            &collector,
            "read_file",
            &call.payload,
            result.as_ref(),
            "read",
        );
        assert_eq!(
            control
                .observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence),
            expected,
            "{selectors}"
        );
    }
    // Identical bytes at a different path are independent evidence.
    let other = dir.path().join("other.txt");
    std::fs::write(&other, "first\nsecond\n").unwrap();
    let mut call = base.clone();
    call.payload = ToolPayload::Function {
        arguments: json!({"path":other,"selectors":[{"kind":"lines","start":1,"end":1}]})
            .to_string(),
    };
    let result = ReadFileHandler.handle(call.clone()).await.unwrap();
    let collector = control.collector(&baseline);
    record(
        &collector,
        "read_file",
        &call.payload,
        result.as_ref(),
        "other",
    );
    assert!(
        control
            .observe_progress(&baseline, &collector, &settled())
            .contains(&TurnTimingProgressKind::NewSourceEvidence)
    );
}

#[tokio::test]
async fn recovery_failure_and_partial_success_reach_progress_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, "first\nsecond\n").unwrap();
    let base = invocation(
        "read_file",
        json!({"path":path,"selectors":[{"kind":"lines","start":1,"end":1}]}),
    )
    .await;
    let initial = ReadFileHandler
        .handle(base.clone())
        .await
        .unwrap()
        .code_mode_result(&base.payload);
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    for (selectors, success, novel) in [
        (json!([{"kind":"lines","start":0,"end":0}]), false, false),
        (
            json!([{"kind":"json_pointer","pointer":"/missing"}]),
            false,
            false,
        ),
        (
            json!([{"kind":"lines","start":1,"end":1},{"kind":"json_pointer","pointer":"/missing"}]),
            true,
            true,
        ),
        (json!([{"kind":"bytes","start":0,"end":6}]), true, false),
        (json!([{"kind":"lines","start":2,"end":2}]), true, true),
    ] {
        let mut call = base.clone();
        call.tool_name = ToolName::plain("read_tool_output");
        call.payload = ToolPayload::Function {
            arguments: json!({"artifact_id":initial["artifact_id"],"selectors":selectors})
                .to_string(),
        };
        let result = ReadToolOutputHandler.handle(call.clone()).await.unwrap();
        assert_eq!(result.success_for_logging(), success, "{selectors}");
        assert_eq!(
            result.outcome_for_logging(),
            if success {
                ToolOutputOutcome::Success
            } else {
                ToolOutputOutcome::Failure
            }
        );
        let collector = control.collector(&baseline);
        record(
            &collector,
            "read_tool_output",
            &call.payload,
            result.as_ref(),
            "recover",
        );
        assert_eq!(
            control
                .observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence),
            novel,
            "{selectors}"
        );
        assert!(result.code_mode_result(&call.payload)["results"].is_array());
    }
}

async fn command_result(
    name: &str,
    path: &std::path::Path,
    text: &str,
    budget: usize,
) -> AnyToolResult {
    let key = if name == "shell_command" {
        "command"
    } else {
        "cmd"
    };
    let payload = ToolPayload::Function {
        arguments: json!({key:format!("Get-Content -LiteralPath '{}'", path.display()),
            "max_output_tokens":budget,"yield_time_ms":budget})
        .to_string(),
    };
    let (classification, _) = crate::tool_history::classify_workspace_tool_call_at_admission(
        name.to_string(),
        payload.clone(),
        path.parent().unwrap().to_path_buf(),
    )
    .await
    .unwrap();
    assert!(!classification.source_dependencies.is_empty());
    AnyToolResult {
        call_id: "command".into(),
        payload,
        result: Box::new(ExecCommandToolOutput {
            validation: None,
            event_call_id: "command".into(),
            chunk_id: "chunk".into(),
            wall_time: std::time::Duration::from_millis(budget as u64),
            raw_output: text.as_bytes().to_vec(),
            truncation_policy: TruncationPolicy::Tokens(10_000),
            max_output_tokens: Some(budget),
            process_id: None,
            session_capabilities: None,
            exit_code: Some(0),
            process_exited: true,
            search_no_match: false,
            original_token_count: None,
            hook_command: None,
            raw_output_artifact: None,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        }),
        model_projection: None,
        source_dependencies: Some(classification.source_dependencies),
        code_mode_feedback: Vec::new(),
    }
}

#[tokio::test]
async fn command_provenance_converges_across_routes_without_merging_sources() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    let other = dir.path().join("b.txt");
    std::fs::write(&path, "same\n").unwrap();
    std::fs::write(&other, "same\n").unwrap();
    for text in ["same\n", ""] {
        let mut control = TurnExecutionControl::new();
        let baseline = control.baselines(0);
        let mut cycles = Vec::new();
        for (name, path, budget, novel) in [
            ("exec_command", &path, 100, true),
            ("shell_command", &path, 500, false),
            ("exec_command", &path, 1000, false),
            ("exec_command", &other, 1000, true),
        ] {
            let result = command_result(name, path, text, budget).await;
            let collector = control.collector(&baseline);
            let registration = collector.register_deterministic_tool_call(
                &ToolName::plain(name),
                &result.payload,
                &result.call_id,
            );
            collector.record_response_result(
                registration.ordinal,
                result.outcome_context(),
                result.sampling_request_signal(),
                &result.response(),
                false,
            );
            cycles.push(collector.deterministic_cycle_key().unwrap());
            assert_eq!(
                control
                    .observe_progress(&baseline, &collector, &settled())
                    .contains(&TurnTimingProgressKind::NewSourceEvidence),
                novel
            );
        }
        assert_eq!(cycles[0], cycles[1]);
        assert_eq!(cycles[0], cycles[2]);
        assert_ne!(cycles[0], cycles[3]);

        let original = command_result("exec_command", &path, text, 100).await;
        let mut selected = command_result("exec_command", &path, text, 100).await;
        let ToolPayload::Function { arguments } = &mut selected.payload else {
            unreachable!();
        };
        let mut value: Value = serde_json::from_str(arguments).unwrap();
        value["cmd"] = json!(format!(
            "Get-Content -LiteralPath '{}' | Select-Object -First 1",
            path.display()
        ));
        *arguments = value.to_string();
        assert_ne!(
            original.sampling_request_signal().unwrap()["semantic_evidence"],
            selected.sampling_request_signal().unwrap()["semantic_evidence"],
            "equal text is not proof of equal query coverage"
        );
        selected.source_dependencies = None;
        assert!(
            selected
                .sampling_request_signal()
                .unwrap()
                .get("semantic_evidence")
                .is_some_and(|evidence| evidence.get("source").is_none()),
            "unknown provenance retains action-scoped evidence, not cross-tool evidence"
        );
    }
}

#[tokio::test]
async fn registered_shell_dispatch_scopes_evidence_for_direct_and_nested_routes() {
    use crate::session::turn_context::TurnEnvironment;
    use crate::tools::handlers::ShellCommandHandler;
    use crate::tools::handlers::ShellCommandHandlerOptions;
    use crate::tools::parallel::ToolCallRuntime;
    use crate::tools::registry::CoreToolRuntime;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolCall;
    use crate::tools::router::ToolRouter;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;

    let workspace = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(workspace.path().join("source.txt"), "registered evidence\n").unwrap();
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config).cwd =
        AbsolutePathBuf::from_absolute_path(workspace.path()).unwrap();
    turn.permission_profile = PermissionProfile::Disabled;
    turn.environments.turn_environments = vec![TurnEnvironment::new(
        codex_exec_server::LOCAL_ENVIRONMENT_ID.into(),
        Arc::new(codex_exec_server::Environment::default_for_tests()),
        PathUri::from_host_native_path(workspace.path()).unwrap(),
        None,
    )];
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([
            Arc::new(ShellCommandHandler::new(ShellCommandHandlerOptions {
                foreign_environment: false,
                allow_login_shell: true,
                allow_escalated_sandbox_permissions: false,
                exec_permission_approvals_enabled: false,
            })) as Arc<dyn CoreToolRuntime>,
        ]),
        Vec::new(),
    ));
    let runtime = ToolCallRuntime::new(
        Arc::new(session),
        StepContext::for_test(Arc::new(turn)).with_tool_router_for_test(router),
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let mut identities = Vec::new();
    for source in [
        ToolCallSource::Direct,
        ToolCallSource::CodeMode {
            cell_id: "cell".into(),
            parent_call_id: Some("outer".into()),
            runtime_tool_call_id: "nested".into(),
            nested_deadline: None,
            cancellation_cause: None,
        },
    ] {
        let arguments = if cfg!(windows) {
            json!({"script_body":"Get-Content -LiteralPath source.txt","max_output_tokens":1000})
        } else {
            json!({"command":"cat source.txt","max_output_tokens":1000})
        };
        let result = runtime
            .clone()
            .handle_tool_call_with_source(
                ToolCall {
                    tool_name: ToolName::plain("shell_command"),
                    call_id: format!("route-{}", identities.len()),
                    payload: ToolPayload::Function {
                        arguments: arguments.to_string(),
                    },
                },
                source,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.success_for_logging());
        let signal = result.sampling_request_signal().unwrap();
        let evidence = signal["semantic_evidence"].clone();
        assert_eq!(evidence["source"], "workspace-command");
        assert!(
            !evidence["scope"]["dependencies"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        identities.push(evidence);
    }
    assert_eq!(identities[0], identities[1]);
}

#[tokio::test]
async fn mixed_direct_and_nested_work_is_counted_once_per_owned_call() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "nested\n").unwrap();
    std::fs::write(&b, "direct\n").unwrap();
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    let mut cycles = Vec::new();
    for (direct_text, novel) in [
        ("direct\n", true),
        ("direct\n", false),
        ("new direct\n", true),
    ] {
        let collector = control.collector(&baseline);
        let outer = ToolPayload::Custom {
            input: "await tools.exec_command({cmd:'read'});".into(),
        };
        let outer_registration =
            collector.register_deterministic_tool_call(&ToolName::plain("exec"), &outer, "outer");
        collector.record_code_mode_parent("cell", Some("outer"));
        let nested = command_result("exec_command", &a, "nested\n", 100).await;
        let nested_signal = nested.sampling_request_signal();
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "cell",
            tool_name: &ToolName::plain("exec_command"),
            payload: &nested.payload,
            source_dependencies: nested.source_dependencies.clone(),
            outcome_context: nested.outcome_context(),
            signal: nested_signal.as_ref(),
            result: &json!({"output":"nested\n"}),
            canonical_artifact_required: false,
        });
        let other_outer = collector.register_deterministic_tool_call(
            &ToolName::plain("exec"),
            &outer,
            "other-outer",
        );
        collector.record_code_mode_parent("other-cell", Some("other-outer"));
        collector.record_code_mode_result(CodeModeToolResult {
            cell_id: "other-cell",
            tool_name: &ToolName::plain("exec_command"),
            payload: &nested.payload,
            source_dependencies: nested.source_dependencies.clone(),
            outcome_context: nested.outcome_context(),
            signal: nested_signal.as_ref(),
            result: &json!({"output":"nested\n"}),
            canonical_artifact_required: false,
        });
        collector.push(SamplingToolOutcome::plain(
            other_outer.ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));
        let direct = command_result("exec_command", &b, direct_text, 100).await;
        let registration = collector.register_deterministic_tool_call(
            &ToolName::plain("exec_command"),
            &direct.payload,
            "direct",
        );
        collector.record_response_result(
            registration.ordinal,
            direct.outcome_context(),
            direct.sampling_request_signal(),
            &direct.response(),
            false,
        );
        collector.push(SamplingToolOutcome::plain(
            outer_registration.ordinal,
            SamplingToolOutcomeKind::Success,
            None,
        ));
        cycles.push(
            collector
                .deterministic_cycle_key()
                .expect("mixed cycle is classifiable"),
        );
        assert_eq!(
            control
                .observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence),
            novel
        );
    }
    assert_eq!(cycles[0], cycles[1]);
    assert_ne!(cycles[1], cycles[2]);
}
