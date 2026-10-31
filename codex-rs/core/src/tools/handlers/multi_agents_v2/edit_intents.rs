use super::*;
use crate::agent_communication::{AgentCommunicationContext, AgentCommunicationKind};
use codex_agent_task_store::{AssignmentId, AttemptId, EditIntent, TaskActor};
use codex_protocol::ThreadId;
use codex_tools::JsonToolOutput;
use codex_tools::{JsonSchema, ResponsesApiTool, ToolSpec};
use std::collections::BTreeMap;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("declare_edit_intents")
    }
    fn spec(&self) -> ToolSpec {
        let region = JsonSchema::object(
            BTreeMap::from([
                (
                    "start".into(),
                    JsonSchema {
                        minimum: Some(1.into()),
                        ..JsonSchema::integer(None)
                    },
                ),
                (
                    "end".into(),
                    JsonSchema {
                        minimum: Some(1.into()),
                        ..JsonSchema::integer(None)
                    },
                ),
            ]),
            Some(vec!["start".into(), "end".into()]),
            Some(false.into()),
        );
        let intent = JsonSchema::object(BTreeMap::from([
            ("path".into(), JsonSchema::string(Some("Repository-relative declared edit path, not authority to edit.".into()))),
            ("base_revision".into(), JsonSchema::string(Some("Exact declared source hash or immutable revision; not authenticated by this declaration.".into()))),
            ("region".into(), region),
        ]), Some(vec!["path".into(), "base_revision".into()]), Some(false.into()));
        ToolSpec::Function(ResponsesApiTool {
            name: "declare_edit_intents".into(),
            description: "Replace advisory edit intentions for an active assignment/attempt. Declare exact paths, optional inclusive line regions (omit for whole file), and base revisions before editing; use intents:[] to retire them. Overlapping active declarations identify competing assignments and notify reachable peers. Different bases have incomparable regions and warn conservatively. Terminal/superseded attempts are excluded. Declarations are not locks, freshness checks, scope grants or overwrite permission; revalidate actual edit guards before publication.".into(),
            strict: false, defer_loading: None,
            parameters: JsonSchema::object(BTreeMap::from([
                ("assignment_id".into(), JsonSchema::string(None)),
                ("expected_attempt_id".into(), JsonSchema::string(None)),
                ("intents".into(), JsonSchema { max_items: Some(128), ..JsonSchema::array(intent, None) }),
            ]), Some(vec!["assignment_id".into(), "expected_attempt_id".into(), "intents".into()]), Some(false.into())),
            output_schema: None,
        })
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                assignment_id: AssignmentId,
                expected_attempt_id: AttemptId,
                intents: Vec<EditIntent>,
            }
            let args: Args = parse_arguments(&function_arguments(invocation.payload.clone())?)?;
            let control = &invocation.session.services.agent_control;
            let coordinator = control.task_coordinator();
            let actor = if invocation
                .step_context
                .turn
                .session_source
                .is_non_root_agent()
            {
                let binding = coordinator
                    .binding_for_turn(&invocation.step_context.turn)
                    .filter(|b| {
                        b.thread_id.as_deref()
                            == Some(invocation.session.thread_id.to_string().as_str())
                    })
                    .ok_or_else(|| {
                        FunctionCallError::RespondToModel(
                            "edit intents require the caller's active task binding".into(),
                        )
                    })?;
                TaskActor::Attempt(binding.attempt_id)
            } else {
                TaskActor::RootSession(invocation.session.thread_id.to_string())
            };
            let store = coordinator.store().ok_or_else(|| {
                FunctionCallError::RespondToModel("task store unavailable".into())
            })?;
            let warnings = store
                .declare_edit_intents(
                    actor,
                    args.assignment_id,
                    args.expected_attempt_id,
                    args.intents,
                )
                .await
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "declare_edit_intents: {}",
                        task_store_error_detail("declare_edit_intents", error)
                    ))
                })?;
            let author = invocation
                .step_context
                .turn
                .session_source
                .get_agent_path()
                .unwrap_or_else(AgentPath::root);
            let mut notifications = Vec::new();
            // Queue-only; no new turn or worker is started by an advisory.
            let mut recipients = std::collections::BTreeSet::new();
            let mut targets = warnings.iter().filter_map(|warning|
                warning.thread_id.clone().zip(warning.agent_path.clone())).collect::<Vec<_>>();
            // A root may declare on a worker's behalf. That worker does not see
            // this call's returned warning, so notify it as well as competitors.
            if !warnings.is_empty() {
                match coordinator.get_agent_task_binding(args.assignment_id).await {
                    Ok(Some(binding)) => {
                        if let Some(thread) = binding.thread_id
                            && thread != invocation.session.thread_id.to_string()
                        {
                            targets.push((thread, binding.agent_path));
                        }
                    }
                    Ok(None) => {},
                    Err(error) => notifications.push(serde_json::json!({
                        "assignment_id":args.assignment_id, "queued":false, "error":error.to_string()})),
                }
            }
            for (thread, path) in targets {
                if !recipients.insert(thread.to_string()) {
                    continue;
                }
                let result = async {
                    let thread_id = ThreadId::from_string(&thread).map_err(|e| e.to_string())?;
                    let recipient = path.parse::<AgentPath>().map_err(|e| e.to_string())?;
                    control.ensure_agent_in_tree(thread_id).await.map_err(|e| e.to_string())?;
                    let text = format!("Advisory edit-intent overlap with assignment {}, attempt {}: {}. Declarations are not locks or overwrite permission; reconcile current bytes before publishing.",
                        args.assignment_id, args.expected_attempt_id, serde_json::to_string(&warnings).map_err(|e| e.to_string())?);
                    let communication = communication_from_plaintext_message(author.clone(), recipient, text);
                    control.send_inter_agent_communication(thread_id, communication,
                        AgentCommunicationContext::new(AgentCommunicationKind::Message, invocation.session.thread_id)).await.map_err(|e| e.to_string())
                }.await;
                notifications.push(serde_json::json!({"thread_id":thread, "queued":result.is_ok(), "error":result.err()}));
            }
            Ok(boxed_tool_output(JsonToolOutput::new(
                serde_json::json!({"advisory_only":true,
                "assignment_id":args.assignment_id, "attempt_id":args.expected_attempt_id,
                "overlap_warnings":warnings, "notifications":notifications}),
            )))
        })
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
