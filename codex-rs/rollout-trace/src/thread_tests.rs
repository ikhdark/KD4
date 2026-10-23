use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::time::Duration;
use tempfile::TempDir;

use super::*;
use crate::AgentResultTracePayload;
use crate::CompactionCheckpointTracePayload;
use crate::ExecutionStatus;
use crate::RawPayloadKind;
use crate::RawTraceEventPayload;
use crate::RolloutStatus;
use crate::replay_bundle;

#[test]
fn trace_admission_skips_deltas_and_preserves_recorded_boundaries() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let trace = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    let delta = EventMsg::AgentMessageContentDelta(
        codex_protocol::protocol::AgentMessageContentDeltaEvent {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
            item_id: "item".into(),
            delta: "text".into(),
        },
    );
    let started = EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
        turn_id: "turn".into(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    });
    let tool = EventMsg::PatchApplyBegin(codex_protocol::protocol::PatchApplyBeginEvent {
        environment: None,
        call_id: "call".into(),
        turn_id: "turn".into(),
        auto_approved: true,
        changes: Default::default(),
    });
    let bundle = single_bundle_dir(temp.path())?;
    let before = fs::read(bundle.join("trace.jsonl"))?;
    for (event, protocol, runtime, tool_call) in [
        (&delta, false, false, false),
        (&started, true, true, false),
        (&tool, false, true, true),
        (&EventMsg::ShutdownComplete, true, false, false),
    ] {
        assert_eq!(trace.should_record_protocol_event(event), protocol);
        assert_eq!(trace.should_record_runtime_event("turn", event), runtime);
        assert_eq!(trace.should_record_tool_call_event(event), tool_call);
        let disabled = ThreadTraceContext::disabled();
        assert!(!disabled.should_record_protocol_event(event));
        assert!(!disabled.should_record_runtime_event("turn", event));
        assert!(!disabled.should_record_tool_call_event(event));
    }
    assert_eq!(
        fs::read(bundle.join("trace.jsonl"))?,
        before,
        "admission must not write"
    );
    Ok(())
}

#[test]
fn create_in_root_writes_replayable_lifecycle_events() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread_id = ThreadId::new();
    let thread_trace = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        ThreadStartedTraceMetadata {
            thread_id: thread_id.to_string(),
            agent_path: "/root".to_string(),
            task_name: None,
            nickname: None,
            agent_role: None,
            session_source: SessionSource::Exec,
            cwd: PathBuf::from("/workspace"),
            rollout_path: Some(PathBuf::from("/tmp/rollout.jsonl")),
            model: "gpt-test".to_string(),
            provider_name: "test-provider".to_string(),
            approval_policy: "never".to_string(),
            sandbox_policy: format!("{:?}", SandboxPolicy::DangerFullAccess),
        },
    )?;

    thread_trace.record_ended(RolloutStatus::Completed);

    let bundle_dir = single_bundle_dir(temp.path())?;
    let replayed = replay_bundle(&bundle_dir)?;

    assert_eq!(replayed.status, RolloutStatus::Completed);
    assert_eq!(replayed.root_thread_id, thread_id.to_string());
    assert_eq!(replayed.threads[&thread_id.to_string()].agent_path, "/root");
    assert_eq!(replayed.raw_payloads.len(), 1);

    Ok(())
}

#[test]
fn empty_trace_root_is_unset_instead_of_the_process_cwd() {
    assert_eq!(configured_trace_root(None), None);
    assert_eq!(configured_trace_root(Some(std::ffi::OsString::new())), None);
    assert_eq!(
        configured_trace_root(Some("traces".into())),
        Some(PathBuf::from("traces"))
    );
}

#[test]
fn spawned_thread_start_appends_to_root_bundle() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let root_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let root_trace = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(root_thread_id),
    )?;

    let child_trace = root_trace.start_child_thread_trace_or_disabled(ThreadStartedTraceMetadata {
        thread_id: child_thread_id.to_string(),
        agent_path: "/root/repo_file_counter".to_string(),
        task_name: Some("repo_file_counter".to_string()),
        nickname: Some("Kepler".to_string()),
        agent_role: Some("worker".to_string()),
        session_source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_id,
            depth: 1,
            agent_path: Some(
                AgentPath::try_from("/root/repo_file_counter").map_err(anyhow::Error::msg)?,
            ),
            agent_nickname: Some("Kepler".to_string()),
            agent_role: Some("worker".to_string()),
        }),
        cwd: PathBuf::from("/workspace"),
        rollout_path: Some(PathBuf::from("/tmp/child-rollout.jsonl")),
        model: "gpt-test".to_string(),
        provider_name: "test-provider".to_string(),
        approval_policy: "never".to_string(),
        sandbox_policy: format!("{:?}", SandboxPolicy::DangerFullAccess),
    });
    child_trace.record_ended(RolloutStatus::Completed);
    let bundle_dir = single_bundle_dir(temp.path())?;
    let replayed = replay_bundle(&bundle_dir)?;

    assert_eq!(fs::read_dir(temp.path())?.count(), 1);
    assert_eq!(replayed.threads.len(), 2);
    assert_eq!(
        replayed.threads[&child_thread_id.to_string()].agent_path,
        "/root/repo_file_counter"
    );
    assert_eq!(replayed.status, RolloutStatus::Running);
    assert_eq!(
        replayed.threads[&child_thread_id.to_string()]
            .execution
            .status,
        ExecutionStatus::Completed
    );
    assert_eq!(replayed.raw_payloads.len(), 2);

    Ok(())
}

#[test]
fn disabled_thread_context_accepts_trace_calls_without_writing() -> anyhow::Result<()> {
    struct Unused;
    impl serde::Serialize for Unused {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            panic!("disabled tracing serialized a request")
        }
    }
    impl std::fmt::Display for Unused {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("disabled tracing formatted an error")
        }
    }
    let thread_trace = ThreadTraceContext::disabled();

    thread_trace.record_ended(RolloutStatus::Completed);
    thread_trace.record_protocol_event(&EventMsg::ShutdownComplete);
    thread_trace.record_codex_turn_event("turn-1", &EventMsg::ShutdownComplete);
    thread_trace.record_tool_call_event("turn-1", &EventMsg::ShutdownComplete);
    thread_trace.record_agent_result_interaction(
        "turn-1",
        ThreadId::new(),
        &AgentResultTracePayload {
            child_agent_path: "/root/child",
            message: "done",
            status: &AgentStatus::Completed(Some("done".to_string())),
        },
    );

    let inference_trace =
        thread_trace.inference_trace_context("turn-1", "gpt-test", "test-provider");
    let inference_attempt = inference_trace.start_attempt();
    inference_attempt.record_started(&Unused);
    let token_usage: Option<codex_protocol::protocol::TokenUsage> = None;
    inference_attempt.record_completed("response-1", Some("req-1"), &token_usage, &[]);
    inference_attempt.record_failed(Unused, /*upstream_request_id*/ None, &[]);

    let compaction_trace = thread_trace.compaction_trace_context(
        "turn-1",
        "compaction-1",
        "gpt-test",
        "test-provider",
    );
    let compaction_attempt = compaction_trace.start_attempt(&Unused);
    compaction_attempt.record_completed(&[]);
    compaction_attempt.record_failed(Unused);
    compaction_trace.record_installed(&CompactionCheckpointTracePayload {
        input_history: &[],
        replacement_history: &[],
    });

    let built_dispatch_invocation = Cell::new(false);
    let dispatch_trace = thread_trace.start_tool_dispatch_trace(|| {
        built_dispatch_invocation.set(true);
        None
    });
    assert!(!built_dispatch_invocation.get());
    assert!(!dispatch_trace.is_enabled());

    Ok(())
}

#[test]
fn compaction_contexts_share_identity_across_models() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread_id = ThreadId::new();
    let thread_trace =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(thread_id))?;
    thread_trace.record_codex_turn_started("turn-1");

    for model in ["gpt-previous", "gpt-selected"] {
        let compaction_trace =
            thread_trace.compaction_trace_context("turn-1", "compaction-1", model, "test-provider");
        compaction_trace
            .start_attempt(&serde_json::json!({ "model": model }))
            .record_failed("test failure");
    }

    let replayed = replay_bundle(&single_bundle_dir(temp.path())?)?;
    let mut attempts = replayed
        .compaction_requests
        .values()
        .map(|attempt| (attempt.model.clone(), attempt.compaction_id.clone()))
        .collect::<Vec<_>>();
    attempts.sort();
    assert_eq!(
        attempts,
        vec![
            ("gpt-previous".to_string(), "compaction-1".to_string()),
            ("gpt-selected".to_string(), "compaction-1".to_string()),
        ]
    );

    Ok(())
}

#[test]
fn protocol_wrapper_records_selected_events_as_raw_payloads() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread_id = ThreadId::new();
    let thread_trace =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(thread_id))?;

    let child_id = ThreadId::new();
    let child = thread_trace.start_child_thread_trace_or_disabled(minimal_metadata(child_id));
    thread_trace.record_protocol_event(&EventMsg::ShutdownComplete);
    child.record_protocol_event(&EventMsg::ShutdownComplete);

    let bundle = single_bundle_dir(temp.path())?;
    let event_log = fs::read_to_string(bundle.join("trace.jsonl"))?;
    let mut owners = Vec::new();
    for line in event_log.lines() {
        let event: crate::RawTraceEvent = serde_json::from_str(line)?;
        if let RawTraceEventPayload::ProtocolEventObserved {
            event_type,
            event_payload,
        } = event.payload
        {
            assert_eq!(event_type, "shutdown_complete");
            assert_eq!(event.codex_turn_id, None);
            assert_eq!(event_payload.kind, RawPayloadKind::ProtocolEvent);
            let contents: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(bundle.join(event_payload.path))?)?;
            assert_eq!(contents, serde_json::json!({"type": "shutdown_complete"}));
            owners.push(event.thread_id);
        }
    }
    assert_eq!(
        owners,
        vec![Some(thread_id.to_string()), Some(child_id.to_string())]
    );
    Ok(())
}

#[test]
fn terminal_runtime_payloads_use_terminal_runtime_payload_kind() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread_id = ThreadId::new();
    let thread_trace =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(thread_id))?;
    let begin = EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
        environment: None,
        call_id: "call-terminal".to_string(),
        process_id: Some("process-1".to_string()),
        turn_id: "turn-1".to_string(),
        started_at_ms: 1234,
        command: vec!["pwd".to_string()],
        cwd: "file:///workspace".parse()?,
        parsed_cmd: Vec::new(),
        source: ExecCommandSource::Agent,
        interaction_input: None,
    });
    let end = EventMsg::ExecCommandEnd(ExecCommandEndEvent {
        environment: None,
        output_metadata: None,
        call_id: "call-terminal".to_string(),
        process_id: Some("process-1".to_string()),
        turn_id: "turn-1".to_string(),
        completed_at_ms: 2345,
        command: vec!["pwd".to_string()],
        cwd: "file:///workspace".parse()?,
        parsed_cmd: Vec::new(),
        source: ExecCommandSource::Agent,
        interaction_input: None,
        stdout: "/workspace".to_string(),
        stderr: String::new(),
        aggregated_output: "/workspace".to_string(),
        exit_code: Some(0),
        duration: Duration::from_millis(10),
        formatted_output: "/workspace".to_string(),
        status: ExecCommandStatus::Completed,
    });

    thread_trace.record_tool_call_event("turn-1", &begin);
    thread_trace.record_tool_call_event("turn-1", &end);

    let event_log = fs::read_to_string(single_bundle_dir(temp.path())?.join("trace.jsonl"))?;
    let runtime_payload_kinds = event_log
        .lines()
        .filter_map(|line| {
            let event: crate::RawTraceEvent = serde_json::from_str(line).expect("raw trace event");
            match event.payload {
                RawTraceEventPayload::ToolCallRuntimeStarted {
                    runtime_payload, ..
                }
                | RawTraceEventPayload::ToolCallRuntimeEnded {
                    runtime_payload, ..
                } => Some(runtime_payload.kind),
                _ => None,
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(
        runtime_payload_kinds,
        vec![
            RawPayloadKind::TerminalRuntimeEvent,
            RawPayloadKind::TerminalRuntimeEvent,
        ]
    );
    Ok(())
}

#[test]
fn terminal_protocol_replay_preserves_absent_and_nonzero_exit_status() -> anyhow::Result<()> {
    for exit_code in [None, Some(0), Some(7)] {
        let temp = TempDir::new()?;
        let thread_id = ThreadId::new();
        let trace = ThreadTraceContext::start_root_in_root_for_test(
            temp.path(),
            minimal_metadata(thread_id),
        )?;
        trace.record_codex_turn_started("turn-1");
        let dispatch = trace.start_tool_dispatch_trace(|| {
            Some(ToolDispatchInvocation {
                thread_id: thread_id.to_string(),
                codex_turn_id: "turn-1".into(),
                tool_call_id: "terminal".into(),
                tool_name: "exec_command".into(),
                tool_namespace: None,
                requester: crate::ToolDispatchRequester::Model {
                    model_visible_call_id: "call-1".into(),
                },
                payload: crate::ToolDispatchPayload::Function {
                    arguments: r#"{"cmd":"test command"}"#.into(),
                },
            })
        });
        trace.record_tool_call_event(
            "turn-1",
            &EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                environment: None,
                call_id: "terminal".into(),
                process_id: Some("process-1".into()),
                turn_id: "turn-1".into(),
                started_at_ms: 1,
                command: vec!["test command".into()],
                cwd: "file:///workspace".parse()?,
                parsed_cmd: vec![],
                source: ExecCommandSource::Agent,
                interaction_input: None,
            }),
        );
        trace.record_tool_call_event(
            "turn-1",
            &EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                environment: None,
                output_metadata: None,
                call_id: "terminal".into(),
                process_id: Some("process-1".into()),
                turn_id: "turn-1".into(),
                completed_at_ms: 2,
                command: vec!["test command".into()],
                cwd: "file:///workspace".parse()?,
                parsed_cmd: vec![],
                source: ExecCommandSource::Agent,
                interaction_input: None,
                stdout: "captured stdout".into(),
                stderr: "captured stderr".into(),
                aggregated_output: "captured stdoutcaptured stderr".into(),
                exit_code,
                duration: Duration::from_millis(1),
                formatted_output: "display".into(),
                status: ExecCommandStatus::Completed,
            }),
        );
        dispatch.record_completed(
            ExecutionStatus::Completed,
            crate::ToolDispatchResult::CodeModeResponse {
                value: serde_json::json!({"output":"display"}),
            },
        );
        let replayed = replay_bundle(single_bundle_dir(temp.path())?)?;
        let operation = replayed
            .terminal_operations
            .values()
            .next()
            .expect("terminal operation");
        let result = operation.result.as_ref().expect("runtime result");
        assert_eq!(result.exit_code, exit_code);
        assert_eq!(result.stdout, "captured stdout");
        assert_eq!(result.stderr, "captured stderr");
        assert_eq!(operation.execution.status, ExecutionStatus::Completed);
        // An operation returning without an exit code is not evidence of process exit.
        assert_eq!(
            replayed.terminal_sessions["process-1"].execution.status,
            ExecutionStatus::Running
        );
    }
    Ok(())
}

#[test]
fn cold_child_resume_preserves_identity_and_captures_each_activation() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let parent_id = ThreadId::new();
    let child_id = ThreadId::new();
    let parent =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(parent_id))?;
    let metadata = || child_metadata(parent_id, child_id);
    let child = parent.start_child_thread_trace_or_disabled(metadata());
    record_child_work(&child, "first");
    child.record_ended(RolloutStatus::Completed);
    let bundle = single_bundle_dir(temp.path())?;
    let first = replay_bundle(&bundle)?.threads[&child_id.to_string()]
        .execution
        .clone();

    let resumed = parent.resume_child_thread_trace_or_disabled(metadata());
    assert!(resumed.is_enabled());
    let live = replay_bundle(&bundle)?;
    let child = &live.threads[&child_id.to_string()];
    assert_eq!(child.previous_executions, vec![first.clone()]);
    assert_eq!(child.execution.status, ExecutionStatus::Running);
    assert!(child.execution.started_seq > first.ended_seq.unwrap());
    record_child_work(&resumed, "second");
    resumed.record_ended(RolloutStatus::Completed);
    let replayed = replay_bundle(&bundle)?;
    assert_eq!(replayed.threads.len(), 2);
    assert_eq!(replayed.inference_calls.len(), 2);
    assert_eq!(replayed.code_cells.len(), 2);
    assert_eq!(replayed.codex_turns.len(), 2);
    let child = &replayed.threads[&child_id.to_string()];
    assert!(!child.history_before_trace);
    assert_eq!(child.previous_executions, vec![first]);
    assert_eq!(child.execution.status, ExecutionStatus::Completed);
    assert!(
        matches!(&child.origin, crate::AgentOrigin::Spawned { parent_thread_id, .. } if parent_thread_id == &parent_id.to_string())
    );
    assert_eq!(replayed.status, RolloutStatus::Running);
    assert!(
        replayed.interaction_edges.is_empty(),
        "resume is not a new spawn or delivery"
    );
    Ok(())
}

#[test]
fn historical_child_import_marks_unobserved_history_without_inventing_an_interval()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let parent_id = ThreadId::new();
    let child_id = ThreadId::new();
    let parent =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(parent_id))?;
    let child = parent.resume_child_thread_trace_or_disabled(child_metadata(parent_id, child_id));
    record_child_work(&child, "imported");
    child.record_ended(RolloutStatus::Completed);
    let replayed = replay_bundle(&single_bundle_dir(temp.path())?)?;
    let child = &replayed.threads[&child_id.to_string()];
    assert!(child.history_before_trace);
    assert!(child.previous_executions.is_empty());
    assert!(
        child.execution.started_seq
            > replayed.threads[&parent_id.to_string()]
                .execution
                .started_seq
    );
    assert_eq!(replayed.inference_calls.len(), 1);
    assert_eq!(replayed.code_cells.len(), 1);
    assert!(replayed.interaction_edges.is_empty());
    Ok(())
}

#[test]
fn resumed_trace_rejects_overlapping_runtime_owners_and_respects_trace_off() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let parent_id = ThreadId::new();
    let child_id = ThreadId::new();
    assert!(
        !ThreadTraceContext::disabled()
            .resume_child_thread_trace_or_disabled(child_metadata(parent_id, child_id))
            .is_enabled()
    );
    let parent =
        ThreadTraceContext::start_root_in_root_for_test(temp.path(), minimal_metadata(parent_id))?;
    parent.start_child_thread_trace_or_disabled(child_metadata(parent_id, child_id));
    parent.resume_child_thread_trace_or_disabled(child_metadata(parent_id, child_id));
    let error =
        replay_bundle(&single_bundle_dir(temp.path())?).expect_err("overlapping activations");
    assert!(format!("{error:#}").contains("still active"));
    Ok(())
}

fn child_metadata(parent_id: ThreadId, child_id: ThreadId) -> ThreadStartedTraceMetadata {
    ThreadStartedTraceMetadata {
        agent_path: "/root/worker".into(),
        task_name: Some("worker".into()),
        session_source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: parent_id,
            depth: 1,
            agent_path: Some(AgentPath::try_from("/root/worker").unwrap()),
            agent_nickname: None,
            agent_role: Some("worker".into()),
        }),
        ..minimal_metadata(child_id)
    }
}

fn record_child_work(thread: &ThreadTraceContext, turn: &str) {
    thread.record_codex_turn_started(turn);
    let inference = thread
        .inference_trace_context(turn, "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    let call_id = format!("call-{turn}");
    let output = serde_json::from_value(serde_json::json!({
        "type": "custom_tool_call", "name": "exec", "call_id": call_id, "input": "text(1)"
    }))
    .unwrap();
    inference.record_completed(&format!("response-{turn}"), None, &None, &[output]);
    let cell_id = format!("cell-{turn}");
    let cell = thread.start_code_cell_trace(turn, cell_id.clone(), call_id, "text(1)");
    cell.record_initial_response(
        &codex_code_mode::RuntimeResponse::Result {
            output_loss: None,
            cell_id: codex_code_mode::CellId::new(cell_id),
            content_items: vec![],
            error_text: None,
        },
        true,
    );
}

fn minimal_metadata(thread_id: ThreadId) -> ThreadStartedTraceMetadata {
    ThreadStartedTraceMetadata {
        thread_id: thread_id.to_string(),
        agent_path: "/root".to_string(),
        task_name: None,
        nickname: None,
        agent_role: None,
        session_source: SessionSource::Exec,
        cwd: PathBuf::from("/workspace"),
        rollout_path: None,
        model: "gpt-test".to_string(),
        provider_name: "test-provider".to_string(),
        approval_policy: "never".to_string(),
        sandbox_policy: "danger-full-access".to_string(),
    }
}

fn single_bundle_dir(root: &Path) -> anyhow::Result<PathBuf> {
    let mut entries = fs::read_dir(root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    assert_eq!(entries.len(), 1);
    Ok(entries.remove(0))
}

#[test]
fn provider_completion_survives_response_capture_failure() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let inference = thread
        .inference_trace_context("turn-1", "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    let compaction = thread
        .compaction_trace_context("turn-1", "compact-1", "model", "provider")
        .start_attempt(&serde_json::json!({"input": []}));
    let bundle = single_bundle_dir(temp.path())?;
    // Keep request evidence readable while making the next payload writes fail.
    fs::create_dir(bundle.join("payloads/4.json"))?;
    fs::create_dir(bundle.join("payloads/5.json"))?;
    inference.record_completed("response-1", Some("request-1"), &None, &[]);
    inference.record_failed("late duplicate", None, &[]);
    compaction.record_completed(&[]);
    let replayed = replay_bundle(&bundle)?;
    let call = replayed
        .inference_calls
        .values()
        .next()
        .expect("inference start");
    assert_eq!(call.execution.status, ExecutionStatus::Completed);
    assert_eq!(call.response_id.as_deref(), Some("response-1"));
    assert_eq!(call.upstream_request_id.as_deref(), Some("request-1"));
    assert_eq!(call.raw_response_payload_id, None);
    let compact = replayed
        .compaction_requests
        .values()
        .next()
        .expect("compaction start");
    assert_eq!(compact.execution.status, ExecutionStatus::Completed);
    assert_eq!(compact.raw_response_payload_id, None);
    let events = fs::read_to_string(bundle.join("trace.jsonl"))?;
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("inference_completed"))
            .count(),
        1
    );
    assert!(!events.contains("inference_failed"));
    Ok(())
}

#[test]
fn failed_response_identity_survives_payload_storage_failure() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let inference = thread
        .inference_trace_context("turn-1", "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    let bundle = single_bundle_dir(temp.path())?;
    // Metadata and request payloads already exist; only the response write fails.
    fs::create_dir(bundle.join("payloads/3.json"))?;
    let usage = codex_protocol::protocol::TokenUsage {
        input_tokens: 100,
        total_tokens: 100,
        ..Default::default()
    };
    inference.record_failed_with_usage(
        "failed",
        Some("request"),
        &[],
        Some("response"),
        Some(&usage),
    );
    inference.record_completed("late-duplicate", None, &None, &[]);
    let replayed = replay_bundle(&bundle)?;
    let call = replayed.inference_calls.values().next().expect("inference");
    assert_eq!(call.execution.status, ExecutionStatus::Failed);
    assert_eq!(call.response_id.as_deref(), Some("response"));
    assert_eq!(call.upstream_request_id.as_deref(), Some("request"));
    assert_eq!(call.raw_response_payload_id, None);
    assert_eq!(call.usage, None); // Storage loss must not fabricate a receipt.
    Ok(())
}

#[test]
fn failed_request_capture_does_not_emit_unmatched_terminal_events() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let bundle = single_bundle_dir(temp.path())?;
    // Payload 1 is the session metadata. Fail both request captures: 2 is the
    // inference request and 4 the compaction request (3 is the inference response).
    fs::create_dir(bundle.join("payloads/2.json"))?;
    fs::create_dir(bundle.join("payloads/4.json"))?;
    let inference = thread
        .inference_trace_context("turn-1", "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    inference.record_completed("response-1", None, &None, &[]);
    inference.record_failed("failure", None, &[]);
    let compaction = thread
        .compaction_trace_context("turn-1", "compact-1", "model", "provider")
        .start_attempt(&serde_json::json!({"input": []}));
    compaction.record_completed(&[]);
    compaction.record_failed("failure");
    // A lost request body is an explicit capture gap: each attempt keeps a
    // started-without-payload identity, so its single terminal is never unmatched.
    let replayed = replay_bundle(&bundle)?;
    assert_eq!(replayed.inference_calls.len(), 1);
    let call = replayed
        .inference_calls
        .values()
        .next()
        .expect("inference call");
    assert_eq!(call.raw_request_payload_id, None);
    assert!(call.raw_response_payload_id.is_some());
    assert_eq!(call.execution.status, ExecutionStatus::Completed);
    assert_eq!(call.response_id.as_deref(), Some("response-1"));
    assert_eq!(replayed.compaction_requests.len(), 1);
    let request = replayed
        .compaction_requests
        .values()
        .next()
        .expect("compaction request");
    assert_eq!(request.raw_request_payload_id, None);
    assert!(request.raw_response_payload_id.is_some());
    assert_eq!(request.execution.status, ExecutionStatus::Completed);
    assert_eq!(replayed.payload_capture_gap_count, Some(2));
    // Rollout/thread/turn starts plus one start and one terminal per attempt;
    // the later failures after completion append nothing.
    assert_eq!(
        fs::read_to_string(bundle.join("trace.jsonl"))?
            .lines()
            .count(),
        7
    );
    Ok(())
}

#[test]
fn checkpoint_and_responses_preserve_reasoning_evidence() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let history: Vec<codex_protocol::models::ResponseItem> =
        serde_json::from_value(serde_json::json!([{
            "type": "reasoning", "id": "rs_1", "summary": [],
            "content": [{"type": "text", "text": "captured reasoning"}]
        }]))?;
    let inference = thread
        .inference_trace_context("turn-1", "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    inference.record_completed("response-1", None, &None, &history);
    let compaction = thread.compaction_trace_context("turn-1", "compact-1", "model", "provider");
    compaction
        .start_attempt(&serde_json::json!({"input": []}))
        .record_completed(&history);
    compaction.record_installed(&CompactionCheckpointTracePayload {
        input_history: &history,
        replacement_history: &history,
    });
    let bundle = single_bundle_dir(temp.path())?;
    let replayed = replay_bundle(&bundle)?;
    let mut checked = 0;
    for payload in replayed.raw_payloads.values() {
        let fields: &[&str] = match payload.kind {
            RawPayloadKind::InferenceResponse | RawPayloadKind::CompactionResponse => {
                &["output_items"]
            }
            RawPayloadKind::CompactionCheckpoint => &["input_history", "replacement_history"],
            _ => continue,
        };
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(bundle.join(&payload.path))?)?;
        for field in fields {
            assert_eq!(
                json[*field][0]["content"],
                serde_json::json!([{"type": "text", "text": "captured reasoning"}])
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 4);
    Ok(())
}

#[test]
fn immediate_code_cell_completion_reuses_captured_payload() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let inference = thread
        .inference_trace_context("turn-1", "model", "provider")
        .start_attempt();
    inference.record_started(&serde_json::json!({"input": []}));
    let output = serde_json::from_value(
        serde_json::json!({"type":"custom_tool_call", "name":"exec", "call_id":"call-1", "input":"text(1)"}),
    )?;
    inference.record_completed("response-1", None, &None, &[output]);
    let cell = thread.start_code_cell_trace("turn-1", "cell-1", "call-1", "text(1)");
    cell.record_initial_response(
        &codex_code_mode::RuntimeResponse::Result {
            output_loss: None,
            cell_id: codex_code_mode::CellId::new("cell-1".into()),
            content_items: vec![],
            error_text: None,
        },
        true,
    );
    let bundle = single_bundle_dir(temp.path())?;
    let mut payloads = Vec::new();
    for line in fs::read_to_string(bundle.join("trace.jsonl"))?.lines() {
        let event: crate::RawTraceEvent = serde_json::from_str(line)?;
        match event.payload {
            RawTraceEventPayload::CodeCellInitialResponse {
                response_payload, ..
            }
            | RawTraceEventPayload::CodeCellEnded {
                response_payload, ..
            } => payloads.push(response_payload.expect("response evidence")),
            _ => {}
        }
    }
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0], payloads[1]);
    let replayed = replay_bundle(&bundle)?;
    let cell = replayed.code_cells.values().next().expect("code cell");
    assert_eq!(cell.runtime_status, crate::CodeCellRuntimeStatus::Completed);
    assert!(
        cell.initial_response_seq.expect("initial response")
            < cell.execution.ended_seq.expect("runtime ended")
    );
    assert_eq!(replayed.raw_payloads.len(), 4);
    Ok(())
}
#[test]
fn compaction_receipts_survive_failure_empty_output_and_reducer_replay() -> anyhow::Result<()> {
    use crate::CompactionResponseReceipt;
    use codex_protocol::protocol::TokenUsage;
    use codex_protocol::protocol::TokenUsageCoverage;
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    for completed in [false, true] {
        let context = thread.compaction_trace_context(
            "turn-1",
            if completed { "ok" } else { "failed" },
            "model",
            "provider",
        );
        let attempt = context.start_attempt(&serde_json::json!({"input": []}));
        let receipt = CompactionResponseReceipt {
            response_id: Some("provider-response".into()),
            sampling_request_id: Some("sampling".into()),
            physical_attempt_id: Some(format!("attempt-{completed}")),
            provider_completed: completed,
            token_usage: Some(TokenUsage {
                output_tokens: 100,
                reasoning_output_tokens: 60,
                coverage: Some(TokenUsageCoverage::fully_reported()),
                ..Default::default()
            }),
        };
        let result: Result<&[codex_protocol::models::ResponseItem], &str> = if completed {
            Ok(&[])
        } else {
            Err("rejected checkpoint")
        };
        attempt.record_result_with_receipt(result, Some(&receipt));
        attempt.record_failed("duplicate terminal must not overwrite receipt");
    }
    let bundle = single_bundle_dir(temp.path())?;
    let replayed = replay_bundle(&bundle)?;
    assert_eq!(replayed.compaction_requests.len(), 2);
    for request in replayed.compaction_requests.values() {
        let payload = &replayed.raw_payloads[request
            .raw_response_payload_id
            .as_ref()
            .expect("receipt retained without output")];
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(bundle.join(&payload.path))?)?;
        assert_eq!(json["receipt"]["response_id"], "provider-response");
        assert_eq!(json["receipt"]["token_usage"]["output_tokens"], 100);
        assert_eq!(
            json["receipt"]["token_usage"]["reasoning_output_tokens"],
            60
        );
        assert_eq!(json["output_items"], serde_json::json!([]));
    }
    Ok(())
}
