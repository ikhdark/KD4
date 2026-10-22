use serde_json::json;
use tempfile::TempDir;

use crate::model::ExecutionStatus;
use crate::model::ToolCallKind;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawToolCallRequester;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::generic_summary;
use crate::reducer::test_support::start_turn;
use crate::reducer::test_support::trace_context;
use crate::replay_bundle;

#[test]
fn runtime_before_generic_start_preserves_sequence() -> anyhow::Result<()> {
    for delayed_start in [false, true] {
        let temp = TempDir::new()?;
        let writer = create_started_writer(&temp)?;
        start_turn(&writer, "turn-1")?;
        let start = RawTraceEventPayload::ToolCallStarted {
            tool_call_id: "tool-1".into(),
            model_visible_call_id: None,
            code_mode_runtime_tool_id: None,
            requester: RawToolCallRequester::Model,
            kind: ToolCallKind::ExecCommand,
            summary: generic_summary("exec_command"),
            invocation_payload: None,
        };
        if !delayed_start {
            writer.append_with_context(trace_context("turn-1"), start.clone())?;
        }
        let runtime = writer.write_json_payload(
            RawPayloadKind::ToolRuntimeEvent,
            &json!({
                "call_id": "tool-1", "turn_id": "turn-1", "command": ["echo", "ok"],
                "cwd": "/repo", "process_id": "process-1", "stdout": "ok\n",
                "stderr": "", "formatted_output": "ok\n", "exit_code": 0
            }),
        )?;
        let begin = writer.append_with_context(
            trace_context("turn-1"),
            RawTraceEventPayload::ToolCallRuntimeStarted {
                tool_call_id: "tool-1".into(),
                runtime_payload: runtime.clone(),
            },
        )?;
        let end = writer.append_with_context(
            trace_context("turn-1"),
            RawTraceEventPayload::ToolCallRuntimeEnded {
                tool_call_id: "tool-1".into(),
                status: ExecutionStatus::Completed,
                runtime_payload: runtime,
            },
        )?;
        if delayed_start {
            writer.append_with_context(trace_context("turn-1"), start)?;
        }
        writer.append_with_context(
            trace_context("turn-1"),
            RawTraceEventPayload::ToolCallEnded {
                tool_call_id: "tool-1".into(),
                status: ExecutionStatus::Completed,
                result_payload: None,
            },
        )?;
        let rollout = replay_bundle(temp.path())?;
        let tool = &rollout.tool_calls["tool-1"];
        assert_eq!(rollout.tool_calls.len(), 1);
        assert_eq!(tool.execution.status, ExecutionStatus::Completed);
        let operation = &rollout.terminal_operations[tool.terminal_operation_id.as_ref().unwrap()];
        assert_eq!(operation.execution.started_seq, begin.seq);
        assert_eq!(operation.execution.ended_seq, Some(end.seq));
        assert_eq!(operation.execution.status, ExecutionStatus::Completed);
    }
    Ok(())
}

#[test]
fn runtime_evidence_without_a_generic_start_remains_an_error() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;
    let runtime = writer.write_json_payload(RawPayloadKind::ToolRuntimeEvent, &json!({}))?;
    writer.append_with_context(
        trace_context("turn-1"),
        RawTraceEventPayload::ToolCallRuntimeStarted {
            tool_call_id: "never-started".into(),
            runtime_payload: runtime,
        },
    )?;
    assert!(
        replay_bundle(temp.path())
            .unwrap_err()
            .to_string()
            .contains("never-started")
    );
    Ok(())
}
