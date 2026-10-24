use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolCallSource as ExtensionToolCallSource;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolStartInput;
use codex_tools::ToolName;
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;

pub(crate) async fn notify_tool_start(
    invocation: &ToolInvocation,
) -> Result<(), crate::FunctionCallError> {
    let mut first_panic = None;
    for contributor in invocation
        .session
        .services
        .extensions
        .tool_lifecycle_contributors()
    {
        let result = AssertUnwindSafe(async {
            contributor
                .on_tool_start(ToolStartInput {
                    session_store: &invocation.session.services.session_extension_data,
                    thread_store: &invocation.session.services.thread_extension_data,
                    turn_store: invocation.step_context.turn.extension_data.as_ref(),
                    turn_id: invocation.step_context.turn.sub_id.as_str(),
                    call_id: invocation.call_id.as_str(),
                    tool_name: &invocation.tool_name,
                    source: extension_tool_call_source(invocation.source.clone()),
                })
                .await;
        })
        .catch_unwind()
        .await;
        if let Err(panic) = result {
            first_panic.get_or_insert_with(|| {
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        panic
                            .downcast_ref::<&str>()
                            .map(|message| (*message).to_string())
                    })
                    .unwrap_or_else(|| "non-string panic".to_string())
            });
        }
    }
    match first_panic {
        Some(message) => Err(crate::FunctionCallError::Fatal(format!(
            "tool lifecycle start callback panicked: {message}"
        ))),
        None => Ok(()),
    }
}

pub(crate) async fn notify_tool_finish(invocation: &ToolInvocation, outcome: ToolCallOutcome) {
    let _ = notify_tool_finish_parts(
        invocation.session.as_ref(),
        invocation.step_context.turn.as_ref(),
        invocation.call_id.as_str(),
        &invocation.tool_name,
        invocation.source.clone(),
        outcome,
        false,
    )
    .await;
}

pub(crate) async fn notify_required_tool_finish(
    invocation: &ToolInvocation,
    outcome: ToolCallOutcome,
) -> Result<(), crate::FunctionCallError> {
    notify_tool_finish_parts(
        invocation.session.as_ref(),
        invocation.step_context.turn.as_ref(),
        invocation.call_id.as_str(),
        &invocation.tool_name,
        invocation.source.clone(),
        outcome,
        true,
    )
    .await
}

pub(crate) async fn notify_tool_aborted(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    tool_name: &ToolName,
    source: ToolCallSource,
) {
    // Cancellation cannot claim successful accounting when its dependency is
    // stalled. Bound this observation too and retain an explicit diagnostic;
    // ordinary terminal success still waits for required accounting above.
    match tokio::time::timeout(
        std::time::Duration::from_secs(1),
        notify_tool_finish_parts(
            session,
            turn,
            call_id,
            tool_name,
            source.clone(),
            ToolCallOutcome::Aborted,
            true,
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            call_id, %error,
            "cancelled tool required finish accounting is unconfirmed after callback failure"
        ),
        Err(_) => tracing::warn!(
            call_id,
            "cancelled tool required finish accounting is unconfirmed after its deadline"
        ),
    }
    let _ = notify_tool_finish_parts(
        session,
        turn,
        call_id,
        tool_name,
        source,
        ToolCallOutcome::Aborted,
        false,
    )
    .await;
}

async fn notify_tool_finish_parts(
    session: &Session,
    turn: &TurnContext,
    call_id: &str,
    tool_name: &ToolName,
    source: ToolCallSource,
    outcome: ToolCallOutcome,
    required: bool,
) -> Result<(), crate::FunctionCallError> {
    // One absolute observation budget, not a fresh timeout per contributor.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut required_panicked = false;
    for contributor in session.services.extensions.tool_lifecycle_contributors() {
        if contributor.requires_finish_before_completion() != required {
            continue;
        }
        let callback = AssertUnwindSafe(async {
            contributor
                .on_tool_finish(ToolFinishInput {
                    session_store: &session.services.session_extension_data,
                    thread_store: &session.services.thread_extension_data,
                    turn_store: turn.extension_data.as_ref(),
                    turn_id: turn.sub_id.as_str(),
                    call_id,
                    tool_name,
                    source: extension_tool_call_source(source.clone()),
                    outcome,
                })
                .await;
        })
        .catch_unwind();
        let result = if required {
            callback.await
        } else {
            match tokio::time::timeout_at(deadline, callback).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        call_id,
                        "tool finish observation deadline exceeded; observer cancelled, committed tool result preserved"
                    );
                    continue;
                }
            }
        };
        if result.is_err() {
            tracing::warn!(call_id, required, "tool lifecycle finish callback panicked");
            required_panicked |= required;
        }
    }
    if required_panicked {
        Err(crate::FunctionCallError::RespondToModel(
            "Required tool finish accounting is unconfirmed because a lifecycle callback panicked. Tool effects have not been rolled back; inspect retained results and accounting state, and do not repeat the tool automatically.".into(),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn extension_tool_call_source(source: ToolCallSource) -> ExtensionToolCallSource {
    match source {
        ToolCallSource::Direct => ExtensionToolCallSource::Direct,
        ToolCallSource::CodeMode {
            cell_id,
            parent_call_id,
            runtime_tool_call_id,
            nested_deadline,
            ..
        } => ExtensionToolCallSource::CodeMode {
            cell_id,
            parent_call_id,
            runtime_tool_call_id,
            nested_deadline,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn extension_source_preserves_correlation_and_deadline() {
        assert_eq!(
            extension_tool_call_source(ToolCallSource::Direct),
            ExtensionToolCallSource::Direct
        );
        for nested_deadline in [None, Some(std::time::Instant::now())] {
            for parent_call_id in [None, Some("exec-parent".to_string())] {
                assert_eq!(
                    extension_tool_call_source(ToolCallSource::CodeMode {
                        cell_id: "cell".to_string(),
                        parent_call_id: parent_call_id.clone(),
                        runtime_tool_call_id: "runtime-call".to_string(),
                        nested_deadline,
                        cancellation_cause: None,
                    }),
                    ExtensionToolCallSource::CodeMode {
                        cell_id: "cell".to_string(),
                        parent_call_id,
                        runtime_tool_call_id: "runtime-call".to_string(),
                        nested_deadline,
                    }
                );
            }
        }
    }
}
