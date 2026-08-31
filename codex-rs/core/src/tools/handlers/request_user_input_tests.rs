use super::*;
use crate::session::Session;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

fn request_invocation(
    session: Arc<Session>,
    turn: Arc<crate::session::turn_context::TurnContext>,
) -> ToolInvocation {
    ToolInvocation {
        session,
        step_context: StepContext::for_test(turn),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({
                "questions": [{
                    "header": "Hdr",
                    "question": "Pick one",
                    "id": "pick_one",
                    "options": [
                        {
                            "label": "A",
                            "description": "A"
                        },
                        {
                            "label": "B",
                            "description": "B"
                        }
                    ]
                }]
            })
            .to_string(),
        },
    }
}

#[tokio::test]
async fn multi_agent_v2_request_user_input_rejects_subagent_threads() {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let turn = Arc::new(turn);

    let result = RequestUserInputHandler {
        available_modes: Vec::new(),
    }
    .handle(request_invocation(Arc::new(session), Arc::clone(&turn)))
    .await;

    let Err(err) = result else {
        panic!("sub-agent request_user_input should fail");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "request_user_input can only be used by the root thread".to_string(),
        )
    );
}

#[tokio::test]
async fn never_approval_request_user_input_returns_recoverable_error() {
    let (session, mut turn) = make_session_and_context().await;
    turn.approval_policy = codex_config::Constrained::allow_any(AskForApproval::Never);
    let turn = Arc::new(turn);

    let result = RequestUserInputHandler {
        available_modes: Vec::new(),
    }
    .handle(request_invocation(Arc::new(session), turn))
    .await;

    let Err(err) = result else {
        panic!("never-approval request_user_input should fail recoverably");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "request_user_input is unavailable when approval policy is `never`; continue without interactive input or return a final response explaining the missing information"
                .to_string(),
        )
    );
}
