use crate::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::get_context_remaining_spec::GET_CONTEXT_REMAINING_TOOL_NAME;
use crate::tools::handlers::get_context_remaining_spec::create_get_context_remaining_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::JsonToolOutput;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde_json::json;

pub struct GetContextRemainingHandler;

impl ToolExecutor<ToolInvocation> for GetContextRemainingHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(GET_CONTEXT_REMAINING_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_get_context_remaining_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            if !matches!(invocation.payload, ToolPayload::Function { .. }) {
                return Err(FunctionCallError::RespondToModel(
                    "get_context_remaining handler received unsupported payload".to_string(),
                ));
            }

            let token_status = crate::session::context_window::context_window_token_status(
                invocation.session.as_ref(),
                invocation.step_context.turn.as_ref(),
            )
            .await;

            Ok(boxed_tool_output(JsonToolOutput::new(
                json!({"tokens_left": token_status.base_window_tokens_remaining}),
            )))
        })
    }
}

impl CoreToolRuntime for GetContextRemainingHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::ResponseInputItem;
    use std::sync::Arc;

    #[tokio::test]
    async fn direct_and_code_mode_results_obey_the_same_schema() {
        // Remaining means the smaller of the configured soft budget and physical
        // context capacity, less usage, clamped at zero; absent limits yield null.
        for (context_window, soft_limit, effective_percent, used, expected) in [
            (Some(1_000), Some(800), 100, 100, Some(700)),
            (Some(1_000), Some(800), 100, 900, Some(0)),
            (Some(1_000), Some(800), 50, 100, Some(400)),
            (None, None, 100, 100, None),
        ] {
            let (session, mut turn) = crate::session::tests::make_session_and_context().await;
            let config = Arc::make_mut(&mut turn.config);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.features.enable(codex_features::Feature::TokenBudget).unwrap();
            turn.model_info.context_window = context_window;
            turn.model_info.max_context_window = None;
            turn.model_info.auto_compact_token_limit = soft_limit;
            turn.model_info.effective_context_window_percent = effective_percent;
            assert!(session.clone_history().await.raw_items().is_empty());
            session.lock_history_state_for_test().await.set_token_info(Some(
                codex_protocol::protocol::TokenUsageInfo {
                    total_token_usage: Default::default(),
                    last_token_usage: codex_protocol::protocol::TokenUsage {
                        input_tokens: used,
                        total_tokens: used,
                        ..Default::default()
                    },
                    model_context_window: context_window,
                },
            ));
            let payload = ToolPayload::Function {
                arguments: "{}".into(),
            };
            let output = GetContextRemainingHandler
                .handle(ToolInvocation {
                    session: Arc::new(session),
                    step_context: crate::session::step_context::StepContext::for_test(Arc::new(turn)),
                    cancellation_token: Default::default(),
                    tracker: Arc::new(tokio::sync::Mutex::new(
                        crate::turn_diff_tracker::TurnDiffTracker::new(),
                    )),
                    call_id: "remaining".into(),
                    tool_name: ToolName::plain(GET_CONTEXT_REMAINING_TOOL_NAME),
                    source: crate::tools::router::ToolCallSource::Direct,
                    payload: payload.clone(),
                })
                .await
                .unwrap();
            let ResponseInputItem::FunctionCallOutput { output: direct, .. } =
                output.to_response_item("remaining", &payload)
            else {
                panic!("expected function output");
            };
            let FunctionCallOutputBody::Text(text) = direct.body else {
                panic!("expected structured JSON text, not rendered content fragments");
            };
            let direct: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(direct, json!({"tokens_left": expected}));
            assert_eq!(direct, output.code_mode_result(&payload));
            let ToolSpec::Function(spec) = GetContextRemainingHandler.spec() else {
                panic!("expected function spec");
            };
            let validator = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
            assert!(validator.is_valid(&direct), "{direct}");
        }
    }
}
