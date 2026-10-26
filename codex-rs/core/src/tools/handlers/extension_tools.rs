use std::sync::Arc;
use std::sync::Weak;

use codex_protocol::items::TurnItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_tools::ConversationHistory;
use codex_tools::ConversationHistoryRequirement;
use codex_tools::ExtensionTurnItem;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolEnvironment;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::TurnItemEmissionFuture;
use codex_tools::TurnItemEmitter;

use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::apply_granted_turn_permissions_uri;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(crate) struct ExtensionToolAdapter(
    Arc<dyn codex_tools::ToolExecutor<ExtensionToolCall>>,
    ToolSpec,
);

impl ExtensionToolAdapter {

    pub(crate) fn with_registered_spec(
        executor: Arc<dyn codex_tools::ToolExecutor<ExtensionToolCall>>,
        spec: ToolSpec,
    ) -> Self {
        // Registration and payload dispatch must use the same validated contract,
        // even when a contributor can produce a different spec on its next call.
        Self(executor, spec)
    }
}

impl ToolExecutor<ToolInvocation> for ExtensionToolAdapter {
    fn tool_name(&self) -> ToolName {
        self.0.tool_name()
    }

    fn spec(&self) -> ToolSpec {
        self.1.clone()
    }

    fn exposure(&self) -> crate::tools::registry::ToolExposure {
        self.0.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.0.supports_parallel_tool_calls()
    }

    fn search_info_for_registered_spec(
        &self,
        registered_spec: &ToolSpec,
    ) -> Option<ToolSearchInfo> {
        let search_info = self.0.search_info()?;
        ToolSearchInfo::from_spec(
            search_info.entry.search_text,
            registered_spec,
            search_info.source_info,
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        let history_requirement = self.0.conversation_history_requirement(&invocation.payload);
        Box::pin(async move {
            let call = tokio::select! {
                _ = invocation.cancellation_token.cancelled() => {
                    return Err(crate::FunctionCallError::RespondToModel(
                        "extension tool call cancelled".to_string(),
                    ));
                }
                call = to_extension_call(&invocation, history_requirement) => call,
            };
            self.0.handle(call).await
        })
    }
}




impl CoreToolRuntime for ExtensionToolAdapter {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        match &self.1 {
            ToolSpec::Freeform(_) => matches!(payload, ToolPayload::Custom { .. }),
            _ => matches!(payload, ToolPayload::Function { .. }),
        }
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        self.0.handles_runtime_cancellation()
    }
}

struct CoreTurnItemEmitter {
    session: Weak<Session>,
    turn: Weak<TurnContext>,
}

async fn emit_legacy_events(session: &Session, turn: &TurnContext, legacy_events: Vec<EventMsg>) {
    for msg in legacy_events {
        session
            .send_event_raw(Event {
                id: turn.sub_id.clone(),
                msg,
            })
            .await;
    }
}

impl TurnItemEmitter for CoreTurnItemEmitter {
    fn emit_started<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }

    fn emit_completed<'a>(&'a self, item: ExtensionTurnItem) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let ExtensionTurnItem {
                item,
                legacy_events,
            } = item;
            let item = TurnItem::Extension(item);
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            emit_legacy_events(session.as_ref(), turn.as_ref(), legacy_events).await;
        })
    }
}

async fn to_extension_call(
    invocation: &ToolInvocation,
    history_requirement: ConversationHistoryRequirement,
) -> ExtensionToolCall {
    let conversation_history = match history_requirement {
        ConversationHistoryRequirement::Full => ConversationHistory::from_shared_items(
            invocation
                .session
                .clone_history()
                .await
                .into_shared_raw_items(),
        ),
        ConversationHistoryRequirement::None => ConversationHistory::default(),
    };
    let primary_environment_id = invocation
        .step_context
        .environments
        .primary()
        .map(|environment| environment.environment_id.clone());
    let mut environments =
        Vec::with_capacity(invocation.step_context.environments.turn_environments.len());
    for environment in &invocation.step_context.environments.turn_environments {
        let additional_permissions = apply_granted_turn_permissions_uri(
            invocation.session.as_ref(),
            environment.environment.approval_scope_id(),
            environment.cwd(),
            SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
        )
        .await
        .additional_permissions_uri;
        let mut file_system_sandbox_context = invocation
            .step_context
            .turn
            .file_system_sandbox_context(None, environment.cwd());
        file_system_sandbox_context.permissions =
            codex_sandboxing::policy_transforms::effective_permission_profile_uri(
                &invocation.step_context.turn.permission_profile(),
                additional_permissions.as_ref(),
            );
        environments.push(ToolEnvironment {
            environment_id: environment.environment_id.clone(),
            cwd: environment.cwd().clone(),
            file_system: environment.environment.get_filesystem(),
            file_system_sandbox_context,
        });
    }
    ExtensionToolCall {
        turn_id: invocation.step_context.turn.sub_id.clone(),
        call_id: invocation.call_id.clone(),
        tool_name: invocation.tool_name.clone(),
        model: invocation.step_context.turn.model_info.slug.clone(),
        truncation_policy: invocation
            .step_context
            .turn
            .model_info
            .truncation_policy
            .into(),
        source: crate::tools::lifecycle::extension_tool_call_source(invocation.source.clone()),
        conversation_history,
        turn_item_emitter: Arc::new(CoreTurnItemEmitter {
            session: Arc::downgrade(&invocation.session),
            turn: Arc::downgrade(&invocation.step_context.turn),
        }),
        cancellation_token: invocation.cancellation_token.clone(),
        primary_environment_id,
        environments,
        payload: invocation.payload.clone(),
    }
}

