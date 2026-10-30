use super::*;
use crate::agent::resource_locks::AgentLockResult;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;
use uuid::Uuid;

pub(crate) struct Handler;

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Args {
    Acquire {
        resource: String,
        #[serde(default)]
        wait: bool,
    },
    Release {
        resource: String,
        token: Uuid,
    },
    Recover {
        resource: String,
        token: Uuid,
    },
    List,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("agent_lock")
    }

    fn spec(&self) -> ToolSpec {
        let mut variants = Vec::new();
        for operation in ["acquire", "release", "recover", "list"] {
            let mut properties = BTreeMap::from([(
                "operation".into(),
                JsonSchema {
                    enum_values: Some(vec![JsonValue::String(operation.into())]),
                    ..JsonSchema::string(None)
                },
            )]);
            let mut required = vec!["operation".into()];
            if operation != "list" {
                properties.insert("resource".into(), JsonSchema {
                    min_length: Some(1), max_length: Some(256),
                    ..JsonSchema::string(Some("Exact opaque name, 1..256 UTF-8 bytes, no surrounding whitespace or control characters. Case-sensitive; not a filesystem path or write permission.".into()))
                });
                required.push("resource".into());
            }
            if matches!(operation, "release" | "recover") {
                properties.insert("token".into(), JsonSchema::string(Some("Per-acquisition UUID returned by acquire/list. Required to fence delayed release/recovery.".into())));
                required.push("token".into());
            }
            if operation == "acquire" {
                properties.insert("wait".into(), JsonSchema::boolean(Some("Default false (nonblocking). If true, register with the resource owner and wait for a targeted release wakeup, then retry atomic acquisition. A wakeup does not grant ownership. Until it returns, this agent's acquisitions of other resources return order_violation; same-resource calls join it. Cancellation or the enclosing call deadline removes the waiter and returns cancelled/timed_out without ownership; never wait while holding a resource the owner needs.".into())));
            }
            variants.push(JsonSchema::object(
                properties,
                Some(required),
                Some(false.into()),
            ));
        }
        ToolSpec::Function(ResponsesApiTool {
            name: "agent_lock".into(),
            description: format!(
                "{}\n\n{}",
                "Cooperative named locks within this live root-agent tree, not filesystem isolation: ordinary write tools remain unrestricted by these locks. Acquire is atomic and defaults to nonblocking (busy queues nothing). Set wait:true to register for targeted release wakeups and retry acquisition without polling; wakeups are not ownership grants. Acquire multiple resources in strictly increasing case-sensitive UTF-8 order, or release held locks first. Same-runtime reacquire is idempotent, not counted. Release requires both the runtime-verified acquiring identity and current token. Shutdown fences acquisition; confirmed drained termination automatically cleans that runtime's locks and wakes waiters. Recover is root-only, token-fenced and never forces shutdown: active holders are refused, uncertain/stalled shutdown returns recovery_blocked and retains ownership. A completed/error/interrupted turn alone is not termination. List recovers lock/token state after a lost response. Process-local only; no cross-process or restart isolation.",
                include_str!("../../../../templates/agents/resource_contention.md").trim()
            ),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema {
                one_of: Some(variants),
                ..JsonSchema::object(BTreeMap::new(), None, None)
            },
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let args: Args = parse_arguments(&function_arguments(invocation.payload)?)?;
            let session = &invocation.session;
            let control = &session.services.agent_control;
            let locks = &control.registry().resource_locks;
            let validate = |resource: &str| {
                if resource.is_empty()
                    || resource.len() > 256
                    || resource.trim() != resource
                    || resource.chars().any(char::is_control)
                {
                    Err(FunctionCallError::RespondToModel("resource must be 1..256 UTF-8 bytes without surrounding whitespace or control characters".into()))
                } else {
                    Ok(())
                }
            };
            let result = match args {
                Args::Acquire { resource, wait } => {
                    validate(&resource)?;
                    if wait {
                        // A nested deadline settles as a timed_out result, not
                        // an error that would reject sibling Code Mode promises.
                        locks
                            .acquire_wait(
                                session,
                                resource,
                                &invocation.cancellation_token,
                                crate::unified_exec::nested_poll_bound(
                                    invocation.source.nested_deadline(),
                                ),
                            )
                            .await
                    } else {
                        locks.acquire(session, resource, &invocation.cancellation_token)
                    }
                }
                Args::Release { resource, token } => {
                    validate(&resource)?;
                    locks.release(session, &resource, token)
                }
                Args::Recover { resource, token } => {
                    validate(&resource)?;
                    if session.thread_id != codex_protocol::ThreadId::from(control.session_id()) {
                        return Err(FunctionCallError::RespondToModel(
                            "only the runtime-verified root orchestrator may recover locks".into(),
                        ));
                    }
                    locks.recover(&resource, token)
                }
                Args::List => locks.list(),
            };
            Ok(boxed_tool_output(result))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn delegates_workspace_admission(&self) -> bool {
        // Admitted by AgentResourceLocks, not by a workspace lease. A wait
        // must not stall this agent's other calls, including the one that
        // leads the holder to release.
        true
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        // An interrupted waiter must observe cancellation under the owner
        // mutex instead of committing a queued acquisition later.
        true
    }

    fn cancellation_requires_commit_barrier(&self) -> bool {
        // Report the settled receipt (including a token acquired just before
        // cancellation) in the aborted response, not after it.
        true
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl ToolOutput for AgentLockResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "agent_lock")
    }

    fn success_for_logging(&self) -> bool {
        // Busy, stale-token and blocked-recovery are explicit protocol states,
        // not failed invocations. Code Mode must return them, not throw away
        // their structured status in an exception. Invalid/unauthorized calls
        // still use FunctionCallError before constructing this result.
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(
            self,
            self.success_for_logging(),
        )
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(
            call_id,
            payload,
            self,
            Some(self.success_for_logging()),
            "agent_lock",
        )
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "agent_lock")
    }
}
