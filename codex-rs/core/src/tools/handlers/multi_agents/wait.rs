use super::*;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::session::session::Session;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v1;
use codex_features::MULTI_AGENT_MAX_WAIT_TIMEOUT_MS;
use codex_features::MULTI_AGENT_MIN_WAIT_TIMEOUT_MS;
use codex_protocol::error::CodexErr;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v1(self.options)
    }

    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "wait_agent wait agent subagent status final result complete timeout targets",
            registered_spec.clone(),
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            step_context,
            payload,
            call_id,
            cancellation_token,
            ..
        } = invocation;
        let turn = Arc::clone(&step_context.turn);
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let mut receiver_thread_ids = parse_agent_id_targets(args.targets)?;
        let mut seen_thread_ids = HashSet::with_capacity(receiver_thread_ids.len());
        receiver_thread_ids.retain(|thread_id| seen_thread_ids.insert(*thread_id));
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        let mut target_by_thread_id = HashMap::with_capacity(receiver_thread_ids.len());
        for receiver_thread_id in &receiver_thread_ids {
            let agent_metadata = session
                .services
                .agent_control
                .get_agent_metadata(*receiver_thread_id)
                .unwrap_or_default();
            target_by_thread_id.insert(
                *receiver_thread_id,
                agent_metadata
                    .agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| receiver_thread_id.to_string()),
            );
            receiver_agents.push(CollabAgentRef {
                thread_id: *receiver_thread_id,
                agent_nickname: agent_metadata.agent_nickname,
                agent_role: agent_metadata.agent_role,
            });
        }

        let timeout_ms = match args.timeout_ms {
            Some(ms) if ms < MULTI_AGENT_MIN_WAIT_TIMEOUT_MS => {
                return Err(FunctionCallError::RespondToModel(
                    "Omit timeout_ms for the normal wait. wait_agent returns immediately when a target has already completed.".to_owned(),
                ));
            }
            Some(ms) => Some(ms.clamp(
                MULTI_AGENT_MIN_WAIT_TIMEOUT_MS,
                MULTI_AGENT_MAX_WAIT_TIMEOUT_MS,
            )),
            None => None,
        };

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, mut pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let mut status_rxs = Vec::with_capacity(receiver_thread_ids.len());
        let mut initial_final_statuses = Vec::new();
        for id in &receiver_thread_ids {
            match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => {
                    let status = rx.borrow().clone();
                    if is_final(&status) {
                        initial_final_statuses.push((*id, status));
                    }
                    status_rxs.push((*id, rx));
                }
                Err(CodexErr::ThreadNotFound(_)) => {
                    initial_final_statuses.push((*id, AgentStatus::NotFound));
                }
                Err(err) => {
                    let mut statuses = HashMap::with_capacity(1);
                    statuses.insert(*id, session.services.agent_control.get_status(*id).await);
                    session
                        .emit_turn_item_completed(
                            &turn,
                            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                                id: call_id.clone(),
                                tool: CollabAgentTool::Wait,
                                status: CollabAgentToolCallStatus::Failed,
                                sender_thread_id: session.thread_id,
                                receiver_thread_ids: statuses.keys().copied().collect(),
                                receiver_agents: wait_receiver_agents(&statuses, &receiver_agents),
                                prompt: None,
                                model: None,
                                reasoning_effort: None,
                                agents_states: statuses,
                            }),
                        )
                        .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let partial_statuses = initial_final_statuses.clone();
        let receiver_count = receiver_thread_ids.len();
        let status_session = session.clone();
        let status_wait = async move {
            match args.return_when {
                WaitReturnWhen::First if !initial_final_statuses.is_empty() => {
                    Ok(initial_final_statuses)
                }
                WaitReturnWhen::First => {
                    let mut futures = FuturesUnordered::new();
                    for (id, rx) in status_rxs {
                        let session = status_session.clone();
                        futures.push(wait_for_final_status(session, id, rx));
                    }
                    while let Some(result) = futures.next().await {
                        if let Some(result) = result {
                            let mut results = vec![result];
                            loop {
                                match futures.next().now_or_never() {
                                    Some(Some(Some(result))) => results.push(result),
                                    Some(Some(None)) => continue,
                                    Some(None) | None => break,
                                }
                            }
                            return Ok(results);
                        }
                    }
                    Err("wait_agent status subscriptions ended before a target reached a final state"
                        .to_string())
                }
                WaitReturnWhen::All => {
                    let mut results = initial_final_statuses;
                    let mut futures = FuturesUnordered::new();
                    for (id, rx) in status_rxs {
                        if results.iter().any(|(final_id, _)| *final_id == id) {
                            continue;
                        }
                        let session = status_session.clone();
                        futures.push(wait_for_final_status(session, id, rx));
                    }
                    while results.len() < receiver_count {
                        match futures.next().await {
                            Some(Some(result)) => results.push(result),
                            Some(None) => continue,
                            None => {
                                return Err(
                                    "wait_agent status subscriptions ended before all targets reached a final state"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    Ok(results)
                }
            }
        };
        tokio::pin!(status_wait);
        let deadline =
            timeout_ms.map(|timeout_ms| Instant::now() + Duration::from_millis(timeout_ms as u64));
        let completion = if let Some(deadline) = deadline {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                activity = next_input_activity(&mut activity_rx, &mut pending_activity) => {
                    LegacyWaitCompletion::InputActivity(activity)
                }
                statuses = &mut status_wait => {
                    LegacyWaitCompletion::Statuses(
                        statuses.map_err(FunctionCallError::RespondToModel)?,
                    )
                }
                _ = tokio::time::sleep_until(deadline) => LegacyWaitCompletion::TimedOut,
            }
        } else {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {
                    return Err(FunctionCallError::RespondToModel(
                        "wait_agent cancelled".to_string(),
                    ));
                }
                activity = next_input_activity(&mut activity_rx, &mut pending_activity) => {
                    LegacyWaitCompletion::InputActivity(activity)
                }
                statuses = &mut status_wait => {
                    LegacyWaitCompletion::Statuses(
                        statuses.map_err(FunctionCallError::RespondToModel)?,
                    )
                }
            }
        };
        let (statuses, timed_out, interruption) = match completion {
            LegacyWaitCompletion::Statuses(statuses) => (statuses, false, None),
            LegacyWaitCompletion::TimedOut => (partial_statuses, true, None),
            LegacyWaitCompletion::InputActivity(activity) => {
                let message = match activity {
                    InputQueueActivity::Mailbox => "wait_agent interrupted by mailbox activity",
                    InputQueueActivity::Steer => "wait_agent interrupted by new user input",
                };
                (partial_statuses, false, Some(message.to_string()))
            }
        };
        let statuses_by_id = statuses.clone().into_iter().collect::<HashMap<_, _>>();
        let result = WaitAgentResult {
            status: statuses
                .into_iter()
                .filter_map(|(thread_id, status)| {
                    target_by_thread_id
                        .get(&thread_id)
                        .cloned()
                        .map(|target| (target, status))
                })
                .collect(),
            timed_out,
        };

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: statuses_by_id.keys().copied().collect(),
                    receiver_agents: wait_receiver_agents(&statuses_by_id, &receiver_agents),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses_by_id,
                }),
            )
            .await;

        if let Some(message) = interruption {
            return Err(FunctionCallError::RespondToModel(message));
        }

        Ok(boxed_tool_output(result))
    }
}

fn wait_receiver_agents(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_agents: &[CollabAgentRef],
) -> Vec<CollabAgentRef> {
    if statuses.is_empty() {
        return Vec::new();
    }

    let mut agents = Vec::with_capacity(statuses.len());
    let mut seen = HashMap::with_capacity(receiver_agents.len());
    for receiver_agent in receiver_agents {
        seen.insert(receiver_agent.thread_id, ());
        if statuses.contains_key(&receiver_agent.thread_id) {
            agents.push(receiver_agent.clone());
        }
    }

    let mut extras = statuses
        .keys()
        .filter(|thread_id| !seen.contains_key(thread_id))
        .map(|thread_id| CollabAgentRef {
            thread_id: *thread_id,
            agent_nickname: None,
            agent_role: None,
        })
        .collect::<Vec<_>>();
    extras.sort_by_key(|agent| agent.thread_id.to_string());
    agents.extend(extras);
    agents
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct WaitArgs {
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    return_when: WaitReturnWhen,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WaitReturnWhen {
    #[default]
    First,
    All,
}

enum LegacyWaitCompletion {
    Statuses(Vec<(ThreadId, AgentStatus)>),
    TimedOut,
    InputActivity(InputQueueActivity),
}

async fn next_input_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: &mut Option<InputQueueActivity>,
) -> InputQueueActivity {
    if let Some(activity) = pending_activity.take() {
        return activity;
    }
    loop {
        if activity_rx.changed().await.is_ok() {
            return *activity_rx.borrow_and_update();
        }
        std::future::pending::<()>().await;
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) status: HashMap<String, AgentStatus>,
    pub(crate) timed_out: bool,
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn projection_metadata(&self) -> Option<codex_tools::ToolOutputProjectionMetadata> {
        crate::tools::handlers::multi_agents_common::tool_output_projection_metadata(self, true)
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

async fn wait_for_final_status(
    session: Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, AgentStatus)> {
    let mut status = status_rx.borrow().clone();
    if is_final(&status) {
        return Some((thread_id, status));
    }

    loop {
        if status_rx.changed().await.is_err() {
            let latest = session.services.agent_control.get_status(thread_id).await;
            return is_final(&latest).then_some((thread_id, latest));
        }
        status = status_rx.borrow().clone();
        if is_final(&status) {
            return Some((thread_id, status));
        }
    }
}
