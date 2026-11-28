use std::collections::BTreeMap;
use std::collections::HashSet;
use std::time::Duration;

use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::time::Instant;

use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::MAX_UNIFIED_EXEC_PROCESSES;
use crate::unified_exec::ProcessEventFilter;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProcessHandle {
    session_id: u32,
    incarnation: uuid::Uuid,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Any,
    #[default]
    All,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    processes: Vec<ProcessHandle>,
    #[serde(default)]
    mode: Mode,
    timeout_ms: Option<u64>,
}

pub(crate) struct WaitProcessesHandler {
    max_timeout_ms: u64,
}

impl WaitProcessesHandler {
    pub(crate) fn new(max_timeout_ms: u64) -> Self {
        Self { max_timeout_ms }
    }
}

fn outcome(handle: ProcessHandle, result: Result<Value, String>) -> Value {
    let mut value = json!({
        "session_id": handle.session_id,
        "incarnation": handle.incarnation,
    });
    match result {
        Ok(observation) => {
            let events = observation["events"].as_array();
            let exited = events.and_then(|events| events.iter().find(|e| e["kind"] == "exited"));
            let failed = events.and_then(|events| events.iter().find(|e| e["kind"] == "failed"));
            let expired = events.is_some_and(|events| events.iter().any(|e| {
                e["kind"] == "output_gap" && e["reason"] == "history_expired"
            }));
            value["status"] = json!(if failed.is_some() {
                "failed"
            } else if exited.is_some() {
                "exited"
            } else if expired {
                "unavailable"
            } else {
                "timed_out"
            });
            if let Some(exited) = exited {
                value["exit_code"] = exited["exit_code"].clone();
                value["succeeded"] = json!(
                    exited["exit_code"]
                        .as_i64()
                        .map(|code| failed.is_none() && code == 0)
                );
            }
            if let Some(failed) = failed {
                value["error"] = failed["message"].clone();
            } else if expired {
                value["error"] = json!("process event history expired; exit status is unavailable");
            }
            value["observation"] = observation;
        }
        Err(error) => {
            value["status"] = json!("unavailable");
            value["error"] = json!(error);
        }
    }
    value
}

impl ToolExecutor<ToolInvocation> for WaitProcessesHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_processes")
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        let handle = JsonSchema::object(
            BTreeMap::from([
                (
                    "session_id".into(),
                    JsonSchema::integer(Some(
                        "Originating exec_command session_id, not an OS PID.".into(),
                    )),
                ),
                (
                    "incarnation".into(),
                    JsonSchema::string(Some(
                        "Creation identity from the originating session_capabilities.".into(),
                    )),
                ),
            ]),
            Some(vec!["session_id".into(), "incarnation".into()]),
            Some(false.into()),
        );
        ToolSpec::Function(ResponsesApiTool {
            name: "wait_processes".into(),
            description: "Observe process exit/failure for 1–64 distinct existing exec_command handles. mode any returns after the first settled outcome (including unavailable handles); all (default) waits for every outcome. Nonzero exits and stale/missing handles are independent outcomes, never fail-fast errors that erase siblings. timeout_ms is one total monotonic observation budget for the group, not per process; nested caller deadlines can shorten it. A synchronous history lookup already in progress may overrun the deadline, but late results are not accepted. After a timely any result, ready siblings are included only within the same deadline; unobserved siblings remain pending. Returns status completed, timed_out, deadline_exceeded or cancelled and ordered per-process outcomes (exited, failed, unavailable, pending, timed_out or cancelled). completed describes the wait, not command success; inspect each exit_code/succeeded. This tool never writes input, terminates processes, acknowledges/consumes output, or retires handles. Cancelling or dropping the wait closes only its subscriptions. Collect unread output afterward with ordinary write_stdin using each original handle.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("processes".into(), JsonSchema {
                    min_items: Some(1),
                    max_items: Some(MAX_UNIFIED_EXEC_PROCESSES as u64),
                    ..JsonSchema::array(handle, Some("Existing process handles in result order; duplicate session_id values are rejected.".into()))
                }),
                ("mode".into(), JsonSchema {
                    enum_values: Some(vec![json!("any"), json!("all")]),
                    ..JsonSchema::string(Some("Wait for any or all independent outcomes; default all.".into()))
                }),
                ("timeout_ms".into(), JsonSchema {
                    minimum: Some(1.into()),
                    maximum: Some(self.max_timeout_ms.into()),
                    ..JsonSchema::integer(Some(format!("Total group wait budget; default {}, maximum {} milliseconds.", 30_000_u64.min(self.max_timeout_ms), self.max_timeout_ms)))
                }),
            ]), Some(vec!["processes".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let started = Instant::now();
            // Argument refusals below precede every subscription: settled, no effects.
            let settled = super::read_tool_output::settled_recovery_failure;
            let ToolPayload::Function { ref arguments } = invocation.payload else {
                return Err(settled(FunctionCallError::RespondToModel(
                    "wait_processes requires function arguments".into(),
                )));
            };
            let args: Arguments = parse_arguments(arguments).map_err(settled)?;
            let timeout_ms = args
                .timeout_ms
                .unwrap_or(30_000_u64.min(self.max_timeout_ms));
            if args.processes.is_empty()
                || args.processes.len() > MAX_UNIFIED_EXEC_PROCESSES
                || !(1..=self.max_timeout_ms).contains(&timeout_ms)
            {
                return Err(settled(FunctionCallError::RespondToModel(format!(
                    "wait_processes requires 1–{MAX_UNIFIED_EXEC_PROCESSES} handles and timeout_ms from 1 to {}",
                    self.max_timeout_ms
                ))));
            }
            let mut ids = HashSet::new();
            if args
                .processes
                .iter()
                .any(|handle| !ids.insert(handle.session_id))
            {
                return Err(settled(FunctionCallError::RespondToModel(
                    "wait_processes requires distinct session_id values; no subscriptions created"
                        .into(),
                )));
            }
            let requested = started + Duration::from_millis(timeout_ms);
            let deadline =
                crate::unified_exec::nested_poll_bound(invocation.source.nested_deadline())
                    .map_or(requested, |bound| bound.min(requested));
            let timeout_status = if deadline < requested {
                "deadline_exceeded"
            } else {
                "timed_out"
            };
            let manager = &invocation.session.services.unified_exec_manager;
            let session = &invocation.session;
            let durable_records = session.durable_process_records().shared();
            let filter = ProcessEventFilter::exit_only(timeout_ms);
            let mut pending = FuturesUnordered::new();
            for (index, handle) in args.processes.iter().copied().enumerate() {
                let filter = &filter;
                let durable_records = durable_records.clone();
                pending.push(async move {
                    let mut durable_error = None;
                    if manager.process_incarnation(handle.session_id).await.is_none() {
                        let receipt = match session.services.command_execution
                            .completed_process_result(handle.session_id, handle.incarnation).await
                        {
                            Some(receipt) => Some(receipt),
                            None => {
                                let lookup = match durable_records.await {
                                    Ok(Some(records)) => records.lookup(handle.session_id, handle.incarnation),
                                    Ok(None) => Ok(None),
                                    Err(cause) => Err(cause),
                                };
                                match lookup {
                                    Ok(receipt) => receipt,
                                    // Incomplete or unreadable history cannot prove
                                    // absence; keep the fallback observation.
                                    Err(cause) => {
                                        durable_error = Some(cause);
                                        None
                                    }
                                }
                            }
                        };
                        if let Some(receipt) = receipt {
                            return (index, json!({
                                "session_id": handle.session_id, "incarnation": handle.incarnation,
                                "status": if receipt["process_exited"] == true { "exited" } else { "unavailable" },
                                "exit_code": receipt["exit_code"], "succeeded": receipt["success"],
                                "error": receipt["error"], "completion_provenance": receipt["completion_provenance"],
                                "output_drained": receipt["output_drained"], "output_reference": receipt["output_reference"],
                            }));
                        }
                    }
                    let mut value = outcome(
                        handle,
                        manager
                            .filtered_process_events(
                                handle.session_id,
                                handle.incarnation,
                                filter,
                                Some(deadline),
                            )
                            .await,
                    );
                    if let Some(cause) = durable_error {
                        value["durable_evidence_error"] = json!(cause);
                    }
                    (index, value)
                });
            }
            // Reuse call-scoped observers, never consuming write_stdin/racing
            // collectors. No spawned task, interaction lock, or output cursor
            // acknowledgment survives cancellation or an early any return.
            let mut outcomes = vec![None; args.processes.len()];
            let status = loop {
                tokio::select! {
                    biased;
                    _ = invocation.cancellation_token.cancelled() => break "cancelled",
                    _ = tokio::time::sleep_until(deadline) => break timeout_status,
                    next = pending.next() => {
                        // A ready poll can spend the remaining budget scanning
                        // cold history synchronously, without yielding to the timer.
                        if Instant::now() >= deadline {
                            break timeout_status;
                        }
                        let Some((index, value)) = next else { break "completed" };
                        let settled = value["status"] != "timed_out";
                        outcomes[index] = Some(value);
                        if !settled {
                            break timeout_status;
                        }
                        if pending.is_empty() {
                            break "completed";
                        }
                        if matches!(args.mode, Mode::Any) {
                            // Keep the timely winner and any timely ready siblings,
                            // but do not start another poll or accept a late result
                            // after the original group deadline.
                            while Instant::now() < deadline {
                                let Some(Some((index, value))) = pending.next().now_or_never() else {
                                    break;
                                };
                                if Instant::now() >= deadline {
                                    break;
                                }
                                outcomes[index] = Some(value);
                            }
                            break "completed";
                        }
                    }
                }
            };
            drop(pending);
            let outcomes: Vec<Value> = args
                .processes
                .into_iter()
                .zip(outcomes)
                .map(|(handle, value)| {
                    value.unwrap_or_else(|| {
                        json!({
                            "session_id": handle.session_id,
                            "incarnation": handle.incarnation,
                            "status": match status {
                                "completed" => "pending",
                                "cancelled" => "cancelled",
                                _ => "timed_out",
                            },
                        })
                    })
                })
                .collect();
            Ok(boxed_tool_output(JsonToolOutput::new(json!({
                "mode": args.mode,
                "status": status,
                "elapsed_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "outcomes": outcomes,
            }))))
        })
    }
}

impl CoreToolRuntime for WaitProcessesHandler {
    fn is_workspace_independent(&self) -> bool {
        // A process can hold the workspace lease while this observer awaits exit.
        true
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        // Let the handler return per-handle cancellation outcomes. This observer
        // deliberately does not own processes or request process cleanup.
        true
    }
}
