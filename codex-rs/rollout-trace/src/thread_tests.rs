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
        exit_code: 0,
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
fn failed_request_capture_does_not_emit_unmatched_terminal_events() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let thread = ThreadTraceContext::start_root_in_root_for_test(
        temp.path(),
        minimal_metadata(ThreadId::new()),
    )?;
    thread.record_codex_turn_started("turn-1");
    let bundle = single_bundle_dir(temp.path())?;
    fs::create_dir(bundle.join("payloads/2.json"))?;
    fs::create_dir(bundle.join("payloads/3.json"))?;
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
    let replayed = replay_bundle(&bundle)?;
    assert!(replayed.inference_calls.is_empty());
    assert!(replayed.compaction_requests.is_empty());
    assert_eq!(
        fs::read_to_string(bundle.join("trace.jsonl"))?
            .lines()
            .count(),
        3
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
