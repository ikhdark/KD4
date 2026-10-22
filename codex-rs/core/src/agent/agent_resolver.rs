use crate::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::ThreadId;
use std::sync::Arc;

/// Resolves a single tool-facing agent target to a thread id.
pub(crate) async fn resolve_agent_target(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    target: &str,
) -> Result<ThreadId, FunctionCallError> {
    register_session_root(session, turn);
    // Compact UUIDs overlap the valid task-name grammar. Named aliases win in
    // the caller's scope; a hyphenated UUID always selects the concrete ID.
    if target.len() == 32
        && target
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        let control = &session.services.agent_control;
        let path = turn
            .session_source
            .get_agent_path()
            .unwrap_or_else(codex_protocol::AgentPath::root)
            .join(target)
            .map_err(FunctionCallError::RespondToModel)?;
        if control
            .task_coordinator()
            .binding_for_agent_path(&path)
            .is_some()
            || control.get_agent_id_for_path(&path).is_some()
        {
            return control
                .resolve_agent_reference(session.thread_id, &turn.session_source, target)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()));
        }
    }
    if let Ok(thread_id) = ThreadId::from_string(target) {
        if session
            .services
            .agent_control
            .ensure_agent_known(thread_id)
            .is_err()
        {
            session
                .services
                .agent_control
                .rehydrate_bound_agent(thread_id)
                .await
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string()))?;
        }
        return Ok(thread_id);
    }

    session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, target)
        .await
        .map_err(|err| match err {
            codex_protocol::error::CodexErr::UnsupportedOperation(message) => {
                FunctionCallError::RespondToModel(message)
            }
            other => FunctionCallError::RespondToModel(other.to_string()),
        })
}

fn register_session_root(session: &Arc<Session>, turn: &Arc<TurnContext>) {
    session
        .services
        .agent_control
        .register_session_root(session.thread_id, turn.parent_thread_id);
}
