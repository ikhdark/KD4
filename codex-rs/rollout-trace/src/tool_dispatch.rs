//! Hot-path helpers for recording canonical tool dispatch boundaries.
//!
//! Core owns tool routing and result conversion. The trace crate owns the raw
//! event schema, payload shape, and no-op behavior, so core only adapts its
//! domain objects into the small request/result structs defined here.

use std::fmt::Display;
use std::sync::Arc;

use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::SandboxPermissions;
use codex_protocol::models::SearchToolCallParams;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::model::AgentThreadId;
use crate::model::CodeModeRuntimeToolId;
use crate::model::CodexTurnId;
use crate::model::ExecutionStatus;
use crate::model::ModelVisibleCallId;
use crate::model::ToolCallId;
use crate::model::ToolCallKind;
use crate::model::ToolCallSummary;
use crate::payload::RawPayloadKind;
use crate::payload::RawPayloadRef;
use crate::raw_event::RawToolCallRequester;
use crate::raw_event::RawTraceEventContext;
use crate::raw_event::RawTraceEventPayload;
use crate::writer::TraceWriter;

/// No-op capable trace handle for one resolved tool dispatch.
#[derive(Clone, Debug)]
pub struct ToolDispatchTraceContext {
    state: ToolDispatchTraceContextState,
}

#[derive(Clone, Debug)]
enum ToolDispatchTraceContextState {
    Disabled,
    Enabled(EnabledToolDispatchTraceContext),
}

#[derive(Clone, Debug)]
struct EnabledToolDispatchTraceContext {
    writer: Arc<TraceWriter>,
    thread_id: AgentThreadId,
    codex_turn_id: CodexTurnId,
    tool_call_id: ToolCallId,
}

/// Core-facing request data for the canonical Codex tool boundary.
pub struct ToolDispatchInvocation {
    pub thread_id: AgentThreadId,
    pub codex_turn_id: CodexTurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub tool_namespace: Option<String>,
    pub requester: ToolDispatchRequester,
    pub payload: ToolDispatchPayload,
}

/// Runtime source that caused a dispatch-level tool call.
pub enum ToolDispatchRequester {
    Model {
        model_visible_call_id: ModelVisibleCallId,
    },
    CodeCell {
        runtime_cell_id: String,
        runtime_tool_call_id: CodeModeRuntimeToolId,
    },
}

/// Tool input observed at the registry boundary.
#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolDispatchPayload {
    Function {
        arguments: String,
    },
    ToolSearch {
        arguments: SearchToolCallParams,
    },
    Custom {
        input: String,
    },
    LocalShell {
        command: Vec<String>,
        workdir: Option<String>,
        timeout_ms: Option<u64>,
        sandbox_permissions: Option<SandboxPermissions>,
        prefix_rule: Option<Vec<String>>,
        additional_permissions: Option<AdditionalPermissionProfile>,
        justification: Option<String>,
    },
}

/// Result data returned from a dispatch-level tool call.
#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolDispatchResult {
    DirectResponse { response_item: ResponseInputItem },
    CodeModeResponse { value: JsonValue },
}

/// Raw invocation payload for the canonical Codex tool boundary.
#[derive(Serialize)]
struct DispatchedToolTraceRequest<'a> {
    tool_name: &'a str,
    tool_namespace: Option<&'a str>,
    payload: &'a ToolDispatchPayload,
}

/// Raw response payload for dispatch-level tool trace events.
#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum DispatchedToolTraceResponse<'a> {
    DirectResponse {
        response_item: &'a ResponseInputItem,
    },
    CodeModeResponse {
        value: &'a JsonValue,
    },
    Error {
        error: String,
    },
}

impl ToolDispatchTraceContext {
    /// Builds a context that accepts trace calls and records nothing.
    pub(crate) fn disabled() -> Self {
        Self {
            state: ToolDispatchTraceContextState::Disabled,
        }
    }

    /// Returns whether caller-side result conversion would be recorded.
    ///
    /// Core uses this to avoid formatting or cloning tool outputs when the
    /// dispatch lifecycle is suppressed or tracing is disabled.
    pub fn is_enabled(&self) -> bool {
        matches!(self.state, ToolDispatchTraceContextState::Enabled(_))
    }

    /// Starts one dispatch-level lifecycle and returns the handle for its result.
    pub(crate) fn start(writer: Arc<TraceWriter>, invocation: ToolDispatchInvocation) -> Self {
        if suppresses_tool_dispatch_trace(&invocation) {
            return Self::disabled();
        }

        let context = EnabledToolDispatchTraceContext {
            writer,
            thread_id: invocation.thread_id.clone(),
            codex_turn_id: invocation.codex_turn_id.clone(),
            tool_call_id: invocation.tool_call_id.clone(),
        };
        record_started(&context, invocation);
        Self {
            state: ToolDispatchTraceContextState::Enabled(context),
        }
    }

    /// Records the caller-facing successful or failed tool result.
    pub fn record_completed(&self, status: ExecutionStatus, result: ToolDispatchResult) {
        let ToolDispatchTraceContextState::Enabled(context) = &self.state else {
            return;
        };
        let response = match &result {
            ToolDispatchResult::DirectResponse { response_item } => {
                DispatchedToolTraceResponse::DirectResponse { response_item }
            }
            ToolDispatchResult::CodeModeResponse { value } => {
                DispatchedToolTraceResponse::CodeModeResponse { value }
            }
        };
        append_tool_call_ended(context, status, &response);
    }

    /// Records a dispatch failure before the tool produced a normal result payload.
    pub fn record_failed(&self, error: impl Display) {
        self.record_error(ExecutionStatus::Failed, error);
    }

    /// Records cancellation after the owning runtime has completed cleanup.
    pub fn record_cancelled(&self, error: impl Display) {
        self.record_error(ExecutionStatus::Cancelled, error);
    }

    fn record_error(&self, status: ExecutionStatus, error: impl Display) {
        let ToolDispatchTraceContextState::Enabled(context) = &self.state else {
            return;
        };
        append_tool_call_ended(
            context,
            status,
            &DispatchedToolTraceResponse::Error {
                error: error.to_string(),
            },
        );
    }
}

fn suppresses_tool_dispatch_trace(invocation: &ToolDispatchInvocation) -> bool {
    matches!(invocation.payload, ToolDispatchPayload::Custom { .. })
        && invocation.tool_namespace.is_none()
        && invocation.tool_name == codex_code_mode::PUBLIC_TOOL_NAME
}

fn record_started(context: &EnabledToolDispatchTraceContext, invocation: ToolDispatchInvocation) {
    let tool_name = invocation.tool_name;
    let tool_namespace = invocation.tool_namespace;
    let kind = dispatched_tool_kind(&tool_name, tool_namespace.as_deref());
    let label = dispatched_tool_label(&tool_name, tool_namespace.as_deref(), &invocation.payload);
    let input_preview = Some(invocation.payload.log_payload_preview());
    let request = DispatchedToolTraceRequest {
        tool_name: tool_name.as_str(),
        tool_namespace: tool_namespace.as_deref(),
        payload: &invocation.payload,
    };
    let request_payload =
        write_json_payload_best_effort(&context.writer, RawPayloadKind::ToolInvocation, &request);
    let (model_visible_call_id, code_mode_runtime_tool_id, requester) =
        requester_fields(invocation.requester);

    append_with_context_best_effort(
        context,
        RawTraceEventPayload::ToolCallStarted {
            tool_call_id: context.tool_call_id.clone(),
            model_visible_call_id,
            code_mode_runtime_tool_id,
            requester,
            kind,
            summary: ToolCallSummary::Generic {
                label,
                input_preview,
                output_preview: None,
            },
            invocation_payload: request_payload,
        },
    );
}

fn requester_fields(
    requester: ToolDispatchRequester,
) -> (
    Option<ModelVisibleCallId>,
    Option<CodeModeRuntimeToolId>,
    RawToolCallRequester,
) {
    match requester {
        ToolDispatchRequester::Model {
            model_visible_call_id,
        } => (
            Some(model_visible_call_id),
            None,
            RawToolCallRequester::Model,
        ),
        ToolDispatchRequester::CodeCell {
            runtime_cell_id,
            runtime_tool_call_id,
        } => (
            None,
            Some(runtime_tool_call_id),
            RawToolCallRequester::CodeCell { runtime_cell_id },
        ),
    }
}

fn dispatched_tool_kind(tool_name: &str, tool_namespace: Option<&str>) -> ToolCallKind {
    // These are the built-in identities registered by core's spec_plan.
    match (tool_namespace, tool_name) {
        (Some("web"), "run") => return ToolCallKind::Web,
        (Some("image_gen"), "imagegen") => return ToolCallKind::ImageGeneration,
        (
            Some("multi_agent_v1"),
            "spawn_agent" | "send_message" | "followup_task" | "assign_task" | "wait_agent"
            | "close_agent" | "interrupt_agent",
        )
        | (None, _) => {}
        (Some(namespace), name) => {
            return ToolCallKind::Other {
                name: format!("{namespace}.{name}"),
            };
        }
    }
    match tool_name {
        "exec_command" | "local_shell" | "shell" | "shell_command" => ToolCallKind::ExecCommand,
        "write_stdin" => ToolCallKind::WriteStdin,
        "apply_patch" => ToolCallKind::ApplyPatch,
        "web_search" | "web_search_preview" => ToolCallKind::Web,
        "image_generation" | "image_query" | "imagegen" => ToolCallKind::ImageGeneration,
        "spawn_agent" => ToolCallKind::SpawnAgent,
        "send_message" => ToolCallKind::SendMessage,
        "followup_task" | "assign_task" => ToolCallKind::AssignAgentTask,
        "wait_agent" => ToolCallKind::WaitAgent,
        "close_agent" | "interrupt_agent" => ToolCallKind::CloseAgent,
        other => ToolCallKind::Other {
            name: other.to_string(),
        },
    }
}

fn dispatched_tool_label(
    tool_name: &str,
    tool_namespace: Option<&str>,
    _payload: &ToolDispatchPayload,
) -> String {
    match tool_namespace {
        Some(namespace) => format!("{namespace}.{tool_name}"),
        None => tool_name.to_string(),
    }
}

impl ToolDispatchPayload {
    fn log_payload_preview(&self) -> String {
        match self {
            ToolDispatchPayload::Function { arguments } => truncate_preview(arguments),
            ToolDispatchPayload::ToolSearch { arguments } => truncate_preview(&arguments.query),
            ToolDispatchPayload::Custom { input } => truncate_preview(input),
            ToolDispatchPayload::LocalShell { command, .. } => {
                truncate_preview_chars(command.iter().enumerate().flat_map(|(index, argument)| {
                    (index > 0)
                        .then_some(' ')
                        .into_iter()
                        .chain(argument.chars())
                }))
            }
        }
    }
}

fn truncate_preview(value: &str) -> String {
    truncate_preview_chars(value.chars())
}

fn truncate_preview_chars(mut chars: impl Iterator<Item = char>) -> String {
    const MAX_PREVIEW_CHARS: usize = 160;
    let mut preview = chars.by_ref().take(MAX_PREVIEW_CHARS).collect::<String>();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn append_tool_call_ended(
    context: &EnabledToolDispatchTraceContext,
    status: ExecutionStatus,
    response: &DispatchedToolTraceResponse<'_>,
) {
    let response_payload =
        write_json_payload_best_effort(&context.writer, RawPayloadKind::ToolResult, response);
    append_with_context_best_effort(
        context,
        RawTraceEventPayload::ToolCallEnded {
            tool_call_id: context.tool_call_id.clone(),
            status,
            result_payload: response_payload,
        },
    );
}

fn write_json_payload_best_effort(
    writer: &TraceWriter,
    kind: RawPayloadKind,
    payload: &impl Serialize,
) -> Option<RawPayloadRef> {
    writer.write_json_payload_best_effort(kind, payload)
}

fn append_with_context_best_effort(
    context: &EnabledToolDispatchTraceContext,
    payload: RawTraceEventPayload,
) {
    let event_context = RawTraceEventContext {
        thread_id: Some(context.thread_id.clone()),
        codex_turn_id: Some(context.codex_turn_id.clone()),
    };
    context
        .writer
        .append_with_context_best_effort(event_context, payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_records_full_tool_identity_and_typed_payload() -> anyhow::Result<()> {
        for (namespace, name, expected_kind) in [
            (
                Some("mcp__external"),
                "apply_patch",
                ToolCallKind::Other {
                    name: "mcp__external.apply_patch".into(),
                },
            ),
            (
                Some("mcp__external"),
                "spawn_agent",
                ToolCallKind::Other {
                    name: "mcp__external.spawn_agent".into(),
                },
            ),
            (
                Some("multi_agent_v1"),
                "spawn_agent",
                ToolCallKind::SpawnAgent,
            ),
            (
                Some("multi_agent_v1"),
                "shell",
                ToolCallKind::Other {
                    name: "multi_agent_v1.shell".into(),
                },
            ),
            (Some("image_gen"), "imagegen", ToolCallKind::ImageGeneration),
            (Some("web"), "run", ToolCallKind::Web),
            (None, "apply_patch", ToolCallKind::ApplyPatch),
        ] {
            let temp = tempfile::TempDir::new()?;
            let writer = Arc::new(TraceWriter::create(
                temp.path(),
                "trace".into(),
                "rollout".into(),
                "thread-1".into(),
            )?);
            ToolDispatchTraceContext::start(
                writer,
                invocation(
                    name,
                    namespace.map(str::to_string),
                    ToolDispatchRequester::Model {
                        model_visible_call_id: "call-1".into(),
                    },
                    ToolDispatchPayload::Function {
                        arguments: "{\"x\":1}".into(),
                    },
                ),
            );
            let event: crate::RawTraceEvent =
                serde_json::from_str(&std::fs::read_to_string(temp.path().join("trace.jsonl"))?)?;
            let RawTraceEventPayload::ToolCallStarted {
                kind,
                invocation_payload: Some(payload),
                ..
            } = event.payload
            else {
                panic!("dispatch start missing")
            };
            assert_eq!(kind, expected_kind);
            let value: JsonValue =
                serde_json::from_str(&std::fs::read_to_string(temp.path().join(payload.path))?)?;
            assert_eq!(
                value,
                serde_json::json!({"tool_name": name, "tool_namespace": namespace, "payload": {"type":"function", "arguments":"{\"x\":1}"}})
            );
        }
        Ok(())
    }

    #[test]
    fn local_shell_payload_preserves_nulls_and_bounded_unicode_preview() -> anyhow::Result<()> {
        let payload = ToolDispatchPayload::LocalShell {
            command: vec!["é".repeat(159), "z".repeat(1000)],
            workdir: None,
            timeout_ms: None,
            sandbox_permissions: None,
            prefix_rule: None,
            additional_permissions: None,
            justification: None,
        };
        assert_eq!(
            payload.log_payload_preview(),
            format!("{} ...", "é".repeat(159))
        );
        assert_eq!(
            serde_json::to_value(&payload)?,
            serde_json::json!({
                "type":"local_shell", "command":["é".repeat(159), "z".repeat(1000)],
                "workdir":null, "timeout_ms":null, "sandbox_permissions":null,
                "prefix_rule":null, "additional_permissions":null, "justification":null
            })
        );
        assert_eq!(truncate_preview(&"é".repeat(160)), "é".repeat(160));
        Ok(())
    }

    #[test]
    fn suppresses_only_noncanonical_dispatch_boundaries() {
        assert!(suppresses_tool_dispatch_trace(&invocation(
            codex_code_mode::PUBLIC_TOOL_NAME,
            /*tool_namespace*/ None,
            ToolDispatchRequester::Model {
                model_visible_call_id: "call-exec".to_string(),
            },
            ToolDispatchPayload::Custom {
                input: "1 + 1".to_string(),
            },
        )));
        assert!(!suppresses_tool_dispatch_trace(&invocation(
            "custom_tool",
            /*tool_namespace*/ None,
            ToolDispatchRequester::Model {
                model_visible_call_id: "call-custom".to_string(),
            },
            ToolDispatchPayload::Custom {
                input: "payload".to_string(),
            },
        )));
        assert!(!suppresses_tool_dispatch_trace(&invocation(
            codex_code_mode::PUBLIC_TOOL_NAME,
            Some("mcp__server".to_string()),
            ToolDispatchRequester::Model {
                model_visible_call_id: "call-namespaced".to_string(),
            },
            ToolDispatchPayload::Custom {
                input: "payload".to_string(),
            },
        )));
    }

    #[test]
    fn classifies_interrupt_agent_as_close_agent() {
        assert_eq!(
            dispatched_tool_kind("interrupt_agent", None),
            ToolCallKind::CloseAgent
        );
    }

    #[test]
    fn classifies_imagegen_as_image_generation() {
        assert_eq!(
            dispatched_tool_kind("imagegen", None),
            ToolCallKind::ImageGeneration
        );
    }

    fn invocation(
        tool_name: &str,
        tool_namespace: Option<String>,
        requester: ToolDispatchRequester,
        payload: ToolDispatchPayload,
    ) -> ToolDispatchInvocation {
        ToolDispatchInvocation {
            thread_id: "thread-1".to_string(),
            codex_turn_id: "turn-1".to_string(),
            tool_call_id: "tool-call-1".to_string(),
            tool_name: tool_name.to_string(),
            tool_namespace,
            requester,
            payload,
        }
    }
}
