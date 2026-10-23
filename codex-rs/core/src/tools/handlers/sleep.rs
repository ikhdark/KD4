use crate::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::items::SleepItem;
use codex_protocol::items::TurnItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

const NAMESPACE: &str = "clock";
const TOOL_NAME: &str = "sleep";
const MAX_SLEEP_DURATION_MS: u64 = 12 * 60 * 60 * 1000;

pub struct SleepHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SleepArgs {
    duration_ms: u64,
}

fn create_sleep_tool() -> ToolSpec {
    let duration_ms = JsonSchema {
        minimum: Some(serde_json::Number::from(1_u64)),
        maximum: Some(serde_json::Number::from(MAX_SLEEP_DURATION_MS)),
        ..JsonSchema::integer(Some(format!(
            "How long to sleep in milliseconds. Must be between 1 and {MAX_SLEEP_DURATION_MS}."
        )))
    };
    let properties = BTreeMap::from([("duration_ms".to_string(), duration_ms)]);

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: NAMESPACE.to_string(),
        description: "Tools for reading and waiting on time.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Pause execution for a specified duration. The sleep ends early when new input arrives for the active turn. Returns the elapsed wall-clock time."
                .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["duration_ms".to_string()]),
                /*additional_properties*/ Some(false.into()),
            ),
            output_schema: None,
        })],
    })
}

impl ToolExecutor<ToolInvocation> for SleepHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(NAMESPACE, TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_sleep_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                session,
                step_context,
                call_id,
                payload,
                source,
                ..
            } = invocation;
            let turn = Arc::clone(&step_context.turn);
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{TOOL_NAME} handler received unsupported payload"
                )));
            };
            let args: SleepArgs = parse_arguments(&arguments)?;
            if !(1..=MAX_SLEEP_DURATION_MS).contains(&args.duration_ms) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "duration_ms must be between 1 and {MAX_SLEEP_DURATION_MS}"
                )));
            }

            let started = Instant::now();
            let requested = Duration::from_millis(args.duration_ms);
            // A nested Code Mode call must return before its wrapper deadline
            // drops it. Stop there and report the remainder instead.
            let sleep_duration = crate::unified_exec::nested_poll_bound(source.nested_deadline())
                .map(|bound| bound.saturating_duration_since(tokio::time::Instant::now()))
                .map_or(requested, |allowed| allowed.min(requested));
            let stopped_at_deadline = sleep_duration < requested;
            let item = TurnItem::Sleep(SleepItem {
                id: call_id,
                duration_ms: args.duration_ms,
            });
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            let turn_state = session
                .input_queue
                .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                .await;
            let (mut activity_rx, pending_activity) = session
                .input_queue
                .subscribe_activity(turn_state.as_deref(), false)
                .await;
            let sleep_result: Result<bool, FunctionCallError> = if pending_activity.is_some() {
                Ok(true)
            } else {
                let sleep = session
                    .services
                    .time_provider
                    .sleep(session.thread_id, sleep_duration);
                tokio::pin!(sleep);
                tokio::select! {
                    result = &mut sleep => result
                        .map(|()| false)
                        .map_err(|err| {
                            FunctionCallError::Fatal(format!("failed to sleep: {err:#}"))
                        }),
                    result = activity_rx.changed() => {
                        if result.is_ok() {
                            Ok(true)
                        } else {
                            sleep
                                .await
                                .map(|()| false)
                                .map_err(|err| {
                                    FunctionCallError::Fatal(format!("failed to sleep: {err:#}"))
                                })
                        }
                    }
                }
            };
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            let interrupted = sleep_result?;

            let message = if interrupted {
                "Sleep interrupted by new input.".to_string()
            } else if stopped_at_deadline {
                format!(
                    "Sleep stopped at the Code Mode nested-call deadline with {} ms of the requested duration remaining; sleep again for the rest, or pass a larger {{ timeout_ms }} call option.",
                    requested.saturating_sub(started.elapsed()).as_millis()
                )
            } else {
                "Sleep completed.".to_string()
            };
            let wall_time_seconds = started.elapsed().as_secs_f64();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                format!("Wall time: {wall_time_seconds:.4} seconds\n{message}"),
                /*success*/ Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for SleepHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sleep_rejects_invalid_arguments() {
        use crate::session::step_context::StepContext;
        use crate::session::tests::make_session_and_context;
        use crate::tools::context::ToolCallSource;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use serde_json::json;
        use tokio::sync::Mutex;
        use tokio_util::sync::CancellationToken;

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let step_context = StepContext::for_test(Arc::new(turn));
        for (arguments, expected_error) in [
            (
                json!({"duration_ms": 0}),
                "duration_ms must be between 1 and 43200000",
            ),
            (
                json!({"duration_ms": 43_200_001}),
                "duration_ms must be between 1 and 43200000",
            ),
            (
                json!({"duration_ms": -1}),
                "failed to parse function arguments:",
            ),
            (
                json!({"duration_ms": 1.5}),
                "failed to parse function arguments:",
            ),
            (json!({}), "failed to parse function arguments:"),
            (
                json!({"duration_ms": 1, "typo": true}),
                "failed to parse function arguments:",
            ),
        ] {
            let invocation = ToolInvocation {
                session: Arc::clone(&session),
                step_context: Arc::clone(&step_context),
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                call_id: "invalid-sleep".to_string(),
                tool_name: ToolName::namespaced("clock", "sleep"),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            };
            // A broken upper-bound guard must fail rather than wait twelve hours.
            // Dropping the handler also drops the provider's cancellable wait.
            let result =
                tokio::time::timeout(Duration::from_secs(1), SleepHandler.handle(invocation))
                    .await
                    .expect("invalid sleep arguments must not leave a long-running wait");
            let Err(FunctionCallError::RespondToModel(message)) = result else {
                panic!("expected argument rejection for {arguments}");
            };
            assert!(
                message.starts_with(expected_error),
                "{arguments}: {message}"
            );
        }
    }

    #[test]
    fn sleep_schema_enforces_runtime_integer_range() {
        let ToolSpec::Namespace(namespace) = create_sleep_tool() else {
            panic!("sleep must remain a namespace tool");
        };
        let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0];
        let schema = serde_json::to_value(&tool.parameters).expect("serialize sleep schema");
        let validator = jsonschema::validator_for(&schema).expect("compile sleep schema");

        assert!(validator.is_valid(&serde_json::json!({ "duration_ms": 1 })));
        assert!(validator.is_valid(&serde_json::json!({ "duration_ms": MAX_SLEEP_DURATION_MS })));
        assert!(!validator.is_valid(&serde_json::json!({ "duration_ms": 0 })));
        assert!(
            !validator.is_valid(&serde_json::json!({ "duration_ms": MAX_SLEEP_DURATION_MS + 1 }))
        );
        assert!(!validator.is_valid(&serde_json::json!({ "duration_ms": 1.5 })));
    }

    #[tokio::test]
    async fn nested_sleep_returns_before_its_deadline() {
        use crate::session::step_context::StepContext;
        use crate::session::tests::make_session_and_context;
        use crate::tools::context::ToolCallSource;
        use crate::tools::context::ToolOutput;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use serde_json::json;
        use tokio::sync::Mutex;
        use tokio_util::sync::CancellationToken;

        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let step_context = StepContext::for_test(Arc::new(turn));
        let payload = |duration_ms: u64| ToolPayload::Function {
            arguments: json!({ "duration_ms": duration_ms }).to_string(),
        };
        let invocation = |duration_ms: u64, source: ToolCallSource| ToolInvocation {
            session: Arc::clone(&session),
            step_context: Arc::clone(&step_context),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "nested-sleep".to_string(),
            tool_name: ToolName::namespaced("clock", "sleep"),
            source,
            payload: payload(duration_ms),
        };
        // An exhausted wrapper budget must end the sleep as a normal result
        // that names the stop, not leave the call for the wrapper to drop.
        let nested = invocation(
            120_000,
            ToolCallSource::CodeMode {
                cell_id: "sleep-cell".to_string(),
                parent_call_id: None,
                runtime_tool_call_id: "sleep-call".to_string(),
                nested_deadline: Some(std::time::Instant::now()),
                cancellation_cause: None,
            },
        );
        let output = tokio::time::timeout(Duration::from_secs(5), SleepHandler.handle(nested))
            .await
            .expect("a nested sleep must return before its wrapper deadline")
            .expect("stopping at the deadline is a normal result");
        let text = output.code_mode_result(&payload(120_000)).to_string();
        assert!(text.contains("nested-call deadline"), "{text}");
        // Negative control: a direct sleep is not shortened or relabelled.
        let direct = invocation(1, ToolCallSource::Direct);
        let output = tokio::time::timeout(Duration::from_secs(5), SleepHandler.handle(direct))
            .await
            .expect("a 1 ms sleep returns")
            .expect("sleep succeeds");
        let text = output.code_mode_result(&payload(1)).to_string();
        assert!(text.contains("Sleep completed."), "{text}");
        assert!(!text.contains("nested-call deadline"), "{text}");
    }
}
