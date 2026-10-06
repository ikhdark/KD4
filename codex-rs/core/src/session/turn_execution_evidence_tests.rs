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
async fn declared_lineage_projections_share_evidence_but_changed_sources_are_novel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let mut control = TurnExecutionControl::new();
    let baseline = control.baselines(0);
    // Commands and output bytes differ; only the declared lineage decides.
    for (command, scope, identity, novel) in [
        ("tool scan", Some("query-1"), "snapshot-1", true),
        ("tool render --retained", Some("query-1"), "snapshot-1", false),
        ("tool scan", Some("query-1"), "snapshot-2", true),
        ("tool scan", Some("query-2"), "snapshot-2", true),
        ("tool scan", None, "snapshot-3", true),
        ("tool render --retained", None, "snapshot-3", false),
    ] {
        let mut lineage = json!({"source": "example_index", "identity": identity});
        if let Some(scope) = scope {
            lineage["scope"] = json!(scope);
        }
        let command = command.to_string();
        let payload = ToolPayload::Function {
            arguments: json!({"cmd": command}).to_string(),
        };
        let result = ExecCommandToolOutput {
            process_output: None,
            error: None,
            validation: None,
            event_call_id: "lineage".into(),
            chunk_id: command.clone(),
            wall_time: std::time::Duration::ZERO,
            raw_output: serde_json::to_vec(&json!({
                "rendered_by": command,
                "evidence_lineage": lineage,
            })).unwrap(),
            truncation_policy: TruncationPolicy::Tokens(10_000),
            max_output_tokens: None,
            process_id: None,
            session_capabilities: None,
            exit_code: Some(0),
            process_exited: true,
            search_no_match: false,
            original_token_count: None,
            hook_command: Some(command.clone()),
            raw_output_artifact: None,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        let collector = control.collector(&baseline);
        record(&collector, "exec_command", &payload, &result, "lineage");
        assert_eq!(
            control.observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence),
            novel, "{command} {scope:?} {identity}",
        );
        for markdown in [false, true] {
            // A report file shares its producer's lineage only when it signs
            // the content that lineage describes.
            let mut signed = lineage.clone();
            let bytes = if markdown {
                let body = "# Report\n";
                signed["content_sha256"] = json!(crate::tool_history::sha256(body.as_bytes()));
                format!("<!-- codex-evidence: {} -->\n{body}",
                    json!({"evidence_lineage": signed})).into_bytes()
            } else {
                let content = serde_json::to_vec(&json!({"rendered_by": command})).unwrap();
                signed["content_sha256"] = json!(crate::tool_history::sha256(&content));
                serde_json::to_vec(&json!({"rendered_by": command, "evidence_lineage": signed}))
                    .unwrap()
            };
            std::fs::write(&path, bytes).unwrap();
            let call = invocation("read_file", json!({"path":path})).await;
            let read = ReadFileHandler.handle(call.clone()).await.unwrap();
            let collector = control.collector(&baseline);
            record(&collector, "read_file", &call.payload, read.as_ref(), "report");
            assert!(!control.observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence));
            // Even a selected projection retains attribution, not a new source
            // identity. Byte coverage still belongs to the ordinary read result.
            let call = invocation("read_file", json!({"path":path,
                "selectors":[{"kind":"lines","start":1,"end":1}]})).await;
            let read = ReadFileHandler.handle(call.clone()).await.unwrap();
            let collector = control.collector(&baseline);
            record(&collector, "read_file", &call.payload, read.as_ref(), "selection");
            assert!(!control.observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::NewSourceEvidence));
        }
    }
}

#[tokio::test]
async fn repository_runners_require_committed_declarations_and_supply_execution_proof() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git").current_dir(dir.path())
            .args(args).output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    };
    git(&["init", "-q"]);
    std::fs::create_dir(dir.path().join(".codex")).unwrap();
    let config = json!({"version":1,"runners":[
        {"programs":["just"],"prefixes":[["ci"]],"operations":["test"],"receipt_runner":"ci"},
        {"programs":["make"],"prefixes":[["verify"]],"operations":["test"],"receipt_runner":"ci"},
        {"programs":["npm"],"prefixes":[["run","e2e"]],"operations":["test"],"receipt_runner":"ci"}
    ]});
    let manifest = dir.path().join(".codex/test-runners.json");
    std::fs::write(&manifest, config.to_string()).unwrap();
    let resolve = |command: &str| CommandInvocation::Script(command.to_string());
    assert!(crate::validation::resolve_command_validation(&resolve("just ci"),
        Some(dir.path()), None).await.is_none());
    git(&["add", ".codex/test-runners.json"]);
    git(&["-c", "user.name=Test", "-c", "user.email=test@example.invalid",
        "commit", "-qm", "declare runners"]);
    // A worktree/index edit cannot introduce a trusted echo producer.
    std::fs::write(&manifest, json!({"version":1,"runners":[
        {"programs":["echo"],"prefixes":[["fake"]],"operations":["test"],"receipt_runner":"ci"}
    ]}).to_string()).unwrap();
    git(&["add", ".codex/test-runners.json"]);
    assert!(crate::validation::resolve_command_validation(&resolve("echo fake"),
        Some(dir.path()), None).await.is_none());

    let receipt = json!({"kind":"codex_test_execution_v1","runner":"ci","exit_code":0,
        "selected_targets":["suite"],"runner_input_fingerprint":"a".repeat(64),
        "completed_tests":{"suite":["case"]},"executed_tests":1});
    for command in ["just ci", "make verify", "npm run e2e"] {
        let validation = crate::validation::resolve_command_validation(&resolve(command),
            Some(dir.path()), None).await.unwrap();
        assert!(validation.is_test());
        for nested in [false, true] {
            let mut control = TurnExecutionControl::new();
            let baseline = control.baselines(0);
            let collector = control.collector(&baseline);
            let payload = ToolPayload::Function { arguments: json!({"cmd":command}).to_string() };
            let output = ExecCommandToolOutput {
                process_output: None, error: None, validation: Some(validation.clone()),
                event_call_id: "runner".into(), chunk_id: "chunk".into(),
                wall_time: std::time::Duration::ZERO, raw_output: receipt.to_string().into_bytes(),
                truncation_policy: TruncationPolicy::Tokens(1000), max_output_tokens: None,
                process_id: None, session_capabilities: None, exit_code: Some(0),
                process_exited: true, search_no_match: false, original_token_count: None,
                hook_command: Some(command.into()), raw_output_artifact: None, repair_notice: None,
                pending_deferred_completions: Vec::new(),
            };
            if nested {
                let signal = output.sampling_request_signal();
                let value = output.code_mode_result(&payload);
                collector.record_code_mode_result(CodeModeToolResult {
                    cell_id: "cell", tool_name: &ToolName::plain("exec_command"),
                    payload: &payload, source_dependencies: None,
                    outcome_context: output.outcome_context(), signal: signal.as_ref(),
                    result: &value, canonical_artifact_required: false,
                });
            } else {
                record(&collector, "exec_command", &payload, &output, "runner");
            }
            assert!(collector.fresh_successful_validation(), "{command} nested={nested}");
            assert!(control.observe_progress(&baseline, &collector, &settled())
                .contains(&TurnTimingProgressKind::ValidationResult));
        }
        for (field, value) in [
            ("executed_tests", json!(0)),
            ("runner", json!("undeclared")),
            ("completed_tests", json!({"suite":["case","case"]})),
            ("runner_input_fingerprint", json!("invalid")),
        ] {
            let mut bad = receipt.clone();
            bad[field] = value;
            let mut signal = json!({});
            crate::tools::context::attach_command_validation(&mut signal,
                bad.to_string().as_bytes(), Some(&validation), Some(0), true);
            assert!(signal.get("runner_execution_receipt").is_none());
        }
    }
    for command in ["echo fake", "just ci; echo fake", "just ci && echo fake",
        "just ci | cat", "just ci > receipt.json", "just ci --help"] {
        let validation = crate::validation::resolve_command_validation(&resolve(command),
            Some(dir.path()), None).await;
        let mut signal = json!({});
        crate::tools::context::attach_command_validation(&mut signal, receipt.to_string().as_bytes(),
            validation.as_ref(), Some(0), true);
        assert!(signal.get("runner_execution_receipt").is_none(), "{command}");
    }
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
            process_output: None,
            error: None,
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
async fn evidence_reuse_native_replay_tracks_paths_inputs_turns_and_freshness() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.txt");
    std::fs::write(&source, "source").unwrap();
    let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let observations = cache.begin_source_path_change_observations(
        root.path(), &[(source.clone(), false)],
    ).await.unwrap();
    let session_replays = Arc::new(SessionPathReplays::default());
    let mut control =
        TurnExecutionControl::new().with_session_path_replays(Arc::clone(&session_replays));
    let baseline = control.baselines(0);
    let payload = ToolPayload::Function { arguments: json!({"path":source}).to_string() };
    let collector = control.collector(&baseline);
    let registration = collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "initial",
    );
    collector.record_replay_dependencies(registration.ordinal, 0, observations.clone(), None);
    collector.record_replay_authorization(registration.ordinal, Some("a".repeat(64)));
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
    // Revisions are turn-local; the dispatcher re-proves path freshness, so
    // new input and later turns keep the path-scoped candidate.
    control.accepted_user_input();
    assert!(control.collector(&control.baselines(1)).register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "new-input",
    ).replayed_success.is_some());
    let next_turn = TurnExecutionControl::new().with_session_path_replays(session_replays);
    assert!(next_turn.collector(&next_turn.baselines(0)).register_deterministic_tool_call(
        &ToolName::plain("read_file"), &payload, "next-turn",
    ).replayed_success.is_some());
    // Equivalent legacy/default syntax must not turn a review-to-implementation
    // transition into another filesystem read. Never conflate environments,
    // selections, invalid calls, or an explicit freshness request.
    let collector = next_turn.collector(&next_turn.baselines(0));
    for (arguments, reusable) in [
        (json!({"file_path":source}), true),
        (json!({"path":source,"selectors":null,"environment_id":null,"force_fresh":false}), true),
        (json!({"path":source,"environment_id":"other"}), false),
        (json!({"path":source,"offset":1,"limit":1}), false),
        (json!({"path":source,"force_fresh":true}), false),
        (json!({"path":source,"file_path":source}), false),
        (json!({"path":source,"unknown":true}), false),
    ] {
        let payload = ToolPayload::Function { arguments: arguments.to_string() };
        assert_eq!(collector.register_deterministic_tool_call(
            &ToolName::plain("read_file"), &payload, "alias-follow-up",
        ).replayed_success.is_some(), reusable, "{arguments}");
    }
    // Exact canonical outputs and their persisted provenance can seed a new
    // turn owner; compaction prose must never become a replayable file result.
    use codex_protocol::models::ResponseItem;
    let ToolPayload::Function { arguments } = &payload else { unreachable!() };
    let mut items = vec![
        ResponseItem::FunctionCall {
            id: None, name: "read_file".into(), namespace: None,
            arguments: arguments.clone(), call_id: "initial".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None, call_id: "initial".into(),
            output: codex_protocol::models::FunctionCallOutputPayload::from_text("source".into()),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let mut history = crate::tool_history::ToolHistoryState::default();
    history.register(crate::tool_history::ToolHistoryCandidate {
        call_id: "initial".into(),
        tool_identity: "read_file".into(),
        semantic_class: "file_read".into(),
        successful: true,
        source_dependencies: BTreeSet::from([SourceDependencyV1::new(&source, false)]),
        source_dependencies_current: true,
        artifact_id: "source-artifact".into(),
        artifact_bytes: 6,
        artifact_sha256: format!("{:x}", Sha256::digest(b"source")),
        original_output_sha256: format!("{:x}", Sha256::digest(b"source")),
        original_tokens: 2,
        preserved_non_text_tokens: None,
        bounded_model_output: "source".into(),
        complete: true,
        projection_eligible: true,
        proof_identity: None,
        supersession_identity: Some(format!("authorized-v1:{}:read_file", "a".repeat(64))),
        consumed_by_generation: None,
        derived: Default::default(),
    });
    history.register_workspace_evidence(
        crate::tool_history::WorkspaceEvidenceObservation::from_response_item(
            None, &items[1], BTreeSet::from([SourceDependencyV1::new(&source, false)]),
        ).unwrap().with_source_path_observations(observations),
    );
    let restored = serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
    let replays = Arc::new(SessionPathReplays::default());
    replays.rehydrate(&items, &restored);
    let rehydrated = TurnExecutionControl::new().with_session_path_replays(replays);
    let restored_guard = rehydrated.collector(&rehydrated.baselines(0))
        .register_deterministic_tool_call(&ToolName::plain("read_file"), &payload, "rehydrated")
        .replayed_success.unwrap();
    assert!(restored_guard.is_fresh(1, &cache, None));
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut items[1] {
        *output = codex_protocol::models::FunctionCallOutputPayload::from_text("receipt".into());
    }
    let tampered = SessionPathReplays::default();
    tampered.rehydrate(&items, &restored);
    assert!(tampered.snapshot().is_empty());
    cache.note_host_workspace_mutation_paths(root.path(), &["source.txt".into()]).await;
    assert!(!guard.is_fresh(2, &cache, None), "a relevant edit invalidates the original proof");
    assert!(!restored_guard.is_fresh(2, &cache, None));
}

#[test_case::test_case(false; "explicit_ranges")]
#[test_case::test_case(true; "complete_default_read")]
#[tokio::test]
async fn native_selector_reuse_preserves_authority_coverage_and_changed_input_guards(default_read: bool) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    std::fs::write(&path, "first\nsecond\n").unwrap();
    let selectors = json!([
        {"kind":"lines","start":1,"end":1},
        {"kind":"lines","start":2,"end":2},
    ]);
    let arguments = if default_read { json!({"path":path}) } else { json!({"path":path,"selectors":selectors}) };
    let invocation = invocation("read_file", arguments).await;
    let result = ReadFileHandler.handle(invocation.clone()).await.unwrap();
    let cache = crate::git_workspace::GitWorkspaceCache::with_noop_watcher_for_tests();
    let observations = cache.begin_source_path_change_observations(root.path(), &[(path.clone(), false)])
        .await.unwrap();
    let shared = Arc::new(SessionPathReplays::default());
    let mut control = TurnExecutionControl::new().with_session_path_replays(Arc::clone(&shared));
    let baseline = control.baselines(0);
    let collector = control.collector(&baseline);
    let registration = collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &invocation.payload, "original",
    );
    collector.record_replay_dependencies(registration.ordinal, 0, observations, None);
    let authority = crate::tools::registry::authorized_tool_invocation_sha256(
        &invocation.step_context.turn, &invocation.payload, None,
    );
    collector.record_replay_authorization(registration.ordinal, authority.clone());
    collector.record_read_replay_output(registration.ordinal, result.code_mode_result(&invocation.payload));
    collector.record_response_result(
        registration.ordinal, result.outcome_context(), result.sampling_request_signal(),
        &result.to_response_item("original", &invocation.payload), false,
    );
    control.settle(&baseline, &collector, &settled());
    let next = TurnExecutionControl::new().with_session_path_replays(shared);
    let collector = next.collector(&next.baselines(0));
    let requested = ToolPayload::Function {
        arguments: json!({"path":path,"offset":2,"limit":1}).to_string(),
    };
    let ToolPayload::Function { arguments: original_args } = &invocation.payload else { unreachable!() };
    let ToolPayload::Function { arguments: requested_args } = &requested else { unreachable!() };
    let raw = result.code_mode_result(&invocation.payload);
    assert!(crate::tools::handlers::reselect_read_file_output(original_args, requested_args, raw.clone()).is_some(), "raw selector projection: {raw}");
    assert!(collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &invocation.payload, "original-again",
    ).replayed_success.is_some(), "original read must remain a candidate");
    let guard = collector.register_deterministic_tool_call(
        &ToolName::plain("read_file"), &requested, "subset",
    ).replayed_success.expect("a delivered selector survives a stage boundary");
    assert!(guard.is_fresh(0, &cache, None));
    assert!(guard.matches_authorization(crate::tools::registry::authorized_tool_invocation_sha256(
        &invocation.step_context.turn, guard.authorization_payload(&requested), None,
    ).as_deref()));
    assert!(!guard.matches_authorization(Some("changed-permissions")));
    assert!(!guard.matches_authorization(None));
    let replay = guard.response_for_call("subset").unwrap();
    let value: Value = serde_json::from_str(&response_output_text(&replay).unwrap()).unwrap();
    assert_eq!(value["results"].as_array().unwrap().len(), 1);
    assert_eq!(value["results"][0]["text"], "second\n");
    assert_eq!(value["complete"], true);
    assert_eq!(value["file_complete"], false);
    assert!(value.get("criterion_evidence").is_none());
    for args in [
        json!({"path":path,"offset":1,"limit":3}), // extends past the delivered text
        json!({"path":path,"offset":2,"limit":1,"force_fresh":true}),
        json!({"path":path,"offset":2,"limit":1,"environment_id":"other"}),
        json!({"path":path.with_file_name("other.txt"),"offset":2,"limit":1}),
        json!({"path":path,"selectors":[{"kind":"search","query":"second"}]}),
    ] {
        if default_read && (args.get("offset") == Some(&json!(1)) || args.get("selectors").is_some()) {
            // A full source can prove EOF-clamped lines and complete searches.
            assert!(collector.register_deterministic_tool_call(
                &ToolName::plain("read_file"),
                &ToolPayload::Function { arguments: args.to_string() }, "full-source",
            ).replayed_success.is_some(), "{args}");
            continue;
        }
        assert!(collector.register_deterministic_tool_call(
            &ToolName::plain("read_file"),
            &ToolPayload::Function { arguments: args.to_string() }, "miss",
        ).replayed_success.is_none(), "{args}");
    }
    std::fs::write(&path, "first\nCHANGED\n").unwrap();
    cache.note_host_workspace_mutation_paths(root.path(), &["source.txt".into()]).await;
    assert!(!guard.is_fresh(1, &cache, None));
}

#[tokio::test]
async fn default_read_reselection_matches_fresh_selector_engine_without_io() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    std::fs::write(&path, "λ\r\n日本語 second\r\nSECOND\nlast").unwrap();
    let original = invocation("read_file", json!({"path":path})).await;
    let raw = ReadFileHandler.handle(original.clone()).await.unwrap().code_mode_result(&original.payload);
    let ToolPayload::Function { arguments: previous } = &original.payload else { unreachable!() };
    let mut cases = Vec::new();
    for selectors in [
        json!([{"kind":"lines","start":2,"end":3}]),
        json!([{"kind":"lines","start":1,"end":99}]),
        json!([{"kind":"search","query":"second","case_insensitive":true,"context_lines":1}]),
        json!([{"kind":"search","query":"absent"}]),
        json!([{"kind":"lines","start":1,"end":2},{"kind":"search","query":"second","context_lines":2}]),
    ] {
        let current = invocation("read_file", json!({"path":path,"selectors":selectors})).await;
        let fresh = ReadFileHandler.handle(current.clone()).await.unwrap().code_mode_result(&current.payload);
        let mut script_call = current.clone();
        script_call.source = ToolCallSource::CodeMode {
            cell_id: "reselect-script".into(), parent_call_id: None,
            runtime_tool_call_id: "reselect-script-read".into(),
            nested_deadline: None, cancellation_cause: None,
        };
        let script = ReadFileHandler.handle(script_call.clone()).await.unwrap().code_mode_result(&script_call.payload);
        let ToolPayload::Function { arguments } = current.payload else { unreachable!() };
        cases.push((arguments, fresh, script));
    }
    // Pure projection must not reread, even if the source is unavailable now.
    // Production freshness/authorization remains covered by the collector test.
    std::fs::remove_file(&path).unwrap();
    for (arguments, fresh, script) in cases {
        let replay = crate::tools::handlers::reselect_read_file_output(previous, &arguments, raw.clone());
        if fresh["results"] != script["results"] {
            assert!(replay.is_none(), "consumer-specific ordering/hydration must not be replayed: {arguments}");
            continue;
        }
        let replay = replay.unwrap();
        for key in ["results", "complete", "file_complete", "source_sha256", "canonical_bytes"] {
            assert_eq!(replay[key], fresh[key], "{key}: {arguments}");
        }
        for key in ["file_complete", "canonical_sha256"] {
            let mut invalid = raw.clone();
            invalid[key] = json!(false);
            assert!(crate::tools::handlers::reselect_read_file_output(previous, &arguments, invalid).is_none());
        }
    }
}

#[test]
fn native_selector_reuse_rejects_incomplete_and_shared_only_results() {
    let previous = json!({"path":"file","selectors":[
        {"kind":"lines","start":1,"end":1}, {"kind":"lines","start":2,"end":2}
    ]}).to_string();
    let requested = json!({"path":"file","offset":2,"limit":1}).to_string();
    for result in [
        json!({"selector":{"kind":"lines","start":2,"end":2},"status":"ok","complete":false,"text":"partial"}),
        json!({"selector":{"kind":"lines","start":2,"end":2},"status":"ok","complete":true,"value":{"shared":true}}),
        json!({"selector":{"kind":"lines","start":2,"end":2},"status":"aggregate_omitted","complete":false}),
    ] {
        assert!(crate::tools::handlers::reselect_read_file_output(
            &previous, &requested, json!({"results":[result]}),
        ).is_none());
    }
}

#[test]
fn native_selector_reuse_slices_utf8_crlf_without_inventing_bytes() {
    let text = "λ\r\n日本語\r\n";
    let output = json!({
        "artifact_id":null,"canonical_sha256":"source","canonical_bytes":text.len(),
        "retained_bytes":0,"complete":true,"results":[{
            "selector":{"kind":"bytes","start":0,"end":text.len()},
            "status":"ok","complete":true,"text":text,
            "canonical_range":{"start":0,"end":text.len()}
        }]
    });
    let original = json!({"path":"file"}).to_string();
    let requested = json!({"path":"file","selectors":[{"kind":"bytes","start":4,"end":13}]}).to_string();
    let result = crate::tools::handlers::reselect_read_file_output(&original, &requested, output.clone()).unwrap();
    assert_eq!(result["results"][0]["text"], "日本語");
    assert_eq!(result["results"][0]["exact_bytes"], 9);
    assert_eq!(result["file_complete"], false);
    let split_codepoint = json!({"path":"file","selectors":[{"kind":"bytes","start":1,"end":2}]}).to_string();
    assert!(crate::tools::handlers::reselect_read_file_output(&original, &split_codepoint, output).is_none());
}

#[test]
fn restored_failure_memory_requires_current_syntax_failure_and_runtime_opt_in() {
    use codex_protocol::models::ResponseItem;
    use crate::tools::registry::TerminalFailureReuse;
    let mut output = codex_protocol::models::FunctionCallOutputPayload::from_text(
        "failed to parse function arguments".into(),
    );
    output.success = Some(false);
    let items = vec![
        ResponseItem::FunctionCall {
            id: None, name: "update_plan".into(), namespace: None,
            arguments: "{".into(), call_id: "bad".into(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None, call_id: "bad".into(), output,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    let mut control = TurnExecutionControl::new();
    control.rehydrate_argument_failures(&items);
    control.accepted_user_input();
    let collector = control.collector(&control.baselines(9));
    for (arguments, capability, suppressed) in [
        ("{", TerminalFailureReuse::RequestRevisionAndJsonSyntax, true),
        ("{}", TerminalFailureReuse::RequestRevisionAndJsonSyntax, false),
        ("{", TerminalFailureReuse::RequestRevision, false),
        ("{", TerminalFailureReuse::Never, false),
    ] {
        let registration = collector.register_deterministic_tool_call_with_reuse(
            &ToolName::plain("update_plan"),
            &ToolPayload::Function { arguments: arguments.into() },
            "repeat",
            capability,
        );
        assert_eq!(registration.suppressed_failure.is_some(), suppressed);
    }
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
            process_output: None,
            error: None,
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
