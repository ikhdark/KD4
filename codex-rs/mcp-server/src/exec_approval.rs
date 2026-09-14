use std::path::PathBuf;
use std::sync::Arc;

use codex_core::CodexThread;
use codex_protocol::ThreadId;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tracing::error;

use crate::approval_response::review_decision_from_elicitation_response;

/// Conforms to the MCP elicitation request params shape, so it can be used as
/// the `params` field of an `elicitation/create` request.
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecApprovalElicitRequestParams {
    // These fields are required so that `params`
    // conforms to ElicitRequestParams.
    pub message: String,

    #[serde(rename = "requestedSchema")]
    pub requested_schema: Value,

    // These are additional fields the client can use to
    // correlate the request with the codex tool call.
    #[serde(rename = "threadId")]
    pub thread_id: ThreadId,
    pub codex_elicitation: String,
    pub codex_mcp_tool_call_id: String,
    pub codex_event_id: String,
    pub codex_call_id: String,
    pub codex_command: Vec<String>,
    pub codex_cwd: PathBuf,
    pub codex_parsed_cmd: Vec<ParsedCommand>,
}

/// Legacy Codex-specific approval response retained for existing clients.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExecApprovalResponse {
    pub decision: ReviewDecision,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_exec_approval_request(
    event: ExecApprovalRequestEvent,
    outgoing: Arc<crate::outgoing_message::OutgoingMessageSender>,
    codex: Arc<CodexThread>,
    tool_call_id: String,
    event_id: String,
    thread_id: ThreadId,
    cancellation: tokio_util::sync::CancellationToken,
) {
    let approval_id = event.effective_approval_id();
    let turn_id = event.turn_id.clone();
    let available_decisions = event.effective_available_decisions();
    if !outgoing.supports_form_elicitation() {
        let _ = codex
            .submit(Op::ExecApproval {
                id: approval_id,
                turn_id: Some(turn_id),
                decision: ReviewDecision::Denied,
            })
            .await;
        return;
    }
    let command = event.command;
    let cwd = event.cwd.to_path_buf();
    let escaped_command =
        shlex::try_join(command.iter().map(String::as_str)).unwrap_or_else(|_| command.join(" "));
    let mut message = format!(
        "Allow Codex to run `{escaped_command}` in `{cwd}`?",
        cwd = cwd.to_string_lossy()
    );

    if let Some(reason) = &event.reason {
        message.push_str(&format!("\nReason: {reason}"));
    }
    if let Some(cwd_uri) = &event.cwd_uri {
        message.push_str(&format!("\nWorking directory URI: {cwd_uri}"));
    }
    if let Some(permissions) = &event.additional_permissions {
        message.push_str(&format!("\nRequested permissions: {}", json!(permissions)));
    }
    if let Some(network) = &event.network_approval_context {
        message.push_str(&format!("\nNetwork request: {}", json!(network)));
    }
    message.push_str(&format!(
        "\nAllowed decisions: {}",
        json!(available_decisions)
    ));

    let params = ExecApprovalElicitRequestParams {
        message,
        requested_schema: json!({"type":"object","properties":{}}),
        thread_id,
        codex_elicitation: "exec-approval".to_string(),
        codex_mcp_tool_call_id: tool_call_id.clone(),
        codex_event_id: event_id.clone(),
        codex_call_id: event.call_id,
        codex_command: command,
        codex_cwd: cwd,
        codex_parsed_cmd: event.parsed_cmd,
    };
    let params_json = match serde_json::to_value(&params) {
        Ok(value) => value,
        Err(err) => {
            let message = format!("Failed to serialize ExecApprovalElicitRequestParams: {err}");
            error!("{message}");

            let _ = codex
                .submit(Op::ExecApproval {
                    id: approval_id,
                    turn_id: Some(turn_id),
                    decision: ReviewDecision::Denied,
                })
                .await;

            return;
        }
    };

    let pending = match outgoing
        .send_request("elicitation/create", Some(params_json))
        .await
    {
        Ok(pending) => pending,
        Err(err) => {
            error!("failed to request exec approval: {err:?}");
            let _ = codex
                .submit(Op::ExecApproval {
                    id: approval_id,
                    turn_id: Some(turn_id),
                    decision: ReviewDecision::Denied,
                })
                .await;
            return;
        }
    };

    // Listen for the response on a separate task so we don't block the main agent loop.
    {
        let codex = codex.clone();
        let approval_id = approval_id.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    outgoing.cancel_request(&pending.id).await;
                }
                _ = on_exec_approval_response(approval_id, turn_id, available_decisions, pending.receiver, codex) => {}
            }
        });
    }
}

async fn on_exec_approval_response(
    approval_id: String,
    turn_id: String,
    available_decisions: Vec<ReviewDecision>,
    receiver: tokio::sync::oneshot::Receiver<serde_json::Value>,
    codex: Arc<CodexThread>,
) {
    let op = exec_approval_op(approval_id, turn_id, available_decisions, receiver).await;

    if let Err(err) = codex.submit(op).await {
        error!("failed to submit ExecApproval: {err}");
    }
}

async fn exec_approval_op(
    approval_id: String,
    turn_id: String,
    available_decisions: Vec<ReviewDecision>,
    receiver: tokio::sync::oneshot::Receiver<serde_json::Value>,
) -> Op {
    let decision = exec_approval_decision(receiver).await;
    let decision = if matches!(decision, ReviewDecision::Denied | ReviewDecision::Abort)
        || available_decisions.contains(&decision)
    {
        decision
    } else {
        ReviewDecision::Denied
    };
    Op::ExecApproval {
        id: approval_id,
        turn_id: Some(turn_id),
        decision,
    }
}

async fn exec_approval_decision(
    receiver: tokio::sync::oneshot::Receiver<serde_json::Value>,
) -> ReviewDecision {
    let value = match receiver.await {
        Ok(value) => value,
        Err(err) => {
            error!("request failed: {err:?}");
            return ReviewDecision::Denied;
        }
    };

    review_decision_from_elicitation_response(value).unwrap_or_else(|err| {
        error!("failed to deserialize ExecApprovalResponse: {err}");
        // If we cannot deserialize the response, we deny the request to be
        // conservative.
        ReviewDecision::Denied
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_cannot_expand_the_available_scope() {
        for (decision, expected) in [
            (ReviewDecision::Approved, ReviewDecision::Approved),
            (ReviewDecision::ApprovedForSession, ReviewDecision::Denied),
            (ReviewDecision::Abort, ReviewDecision::Abort),
        ] {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            sender.send(json!({"decision": decision})).unwrap();
            let op = exec_approval_op(
                "approval-id".into(),
                "explicit-turn-id".into(),
                vec![ReviewDecision::Approved],
                receiver,
            )
            .await;
            let Op::ExecApproval {
                id,
                turn_id,
                decision,
            } = op
            else {
                panic!("expected exec approval");
            };
            assert_eq!(id, "approval-id");
            assert_eq!(turn_id.as_deref(), Some("explicit-turn-id"));
            assert_eq!(decision, expected);
        }
    }

    #[tokio::test]
    async fn cancelled_exec_approval_receiver_is_denied() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        drop(sender);

        let op = exec_approval_op(
            "approval-id".to_string(),
            "turn-id".to_string(),
            vec![ReviewDecision::Approved],
            receiver,
        )
        .await;

        let Op::ExecApproval {
            id,
            turn_id,
            decision,
        } = op
        else {
            panic!("cancelled receiver should produce an ExecApproval operation");
        };
        assert_eq!(id, "approval-id");
        assert_eq!(turn_id.as_deref(), Some("turn-id"));
        assert_eq!(decision, ReviewDecision::Denied);
    }
}
