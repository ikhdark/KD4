use crate::agent::task_capabilities::CapabilityPolicyError;
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::TypedToolRequest;
use crate::agent::task_capabilities::authorize_typed_tool;
use crate::agent::task_capabilities::classify_typed_tool;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec_plan::build_tool_router;
use codex_agent_task_store::AttemptState;
use codex_git_utils::get_git_repo_root;
use codex_mcp::ToolInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::SearchToolCallParams;
use codex_tools::DiscoverableTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

pub use crate::tools::context::ToolCallSource;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub tool_name: ToolName,
    pub call_id: String,
    pub payload: ToolPayload,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ToolCallBuildError {
    #[error("{message}")]
    ToolSearchArguments { call_id: String, message: String },
}

pub struct ToolRouter {
    registry: ToolRegistry,
    model_visible_specs: Vec<ToolSpec>,
    proven_read_only_external_tools: HashSet<ToolName>,
}

pub(crate) struct ToolRouterParams<'a> {
    pub(crate) mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) deferred_mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) tool_suggest_candidates: Option<ToolSuggestCandidates>,
    pub(crate) extension_tool_executors: Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    pub(crate) dynamic_tools: &'a [DynamicToolSpec],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolSuggestPresentation {
    ListTool,
    RecommendationContext,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSuggestCandidates {
    pub(crate) tools: Vec<DiscoverableTool>,
    pub(crate) presentation: ToolSuggestPresentation,
}

impl ToolRouter {
    pub(crate) fn from_context(
        step_context: &StepContext,
        params: ToolRouterParams<'_>,
        tool_search_handler_cache: &ToolSearchHandlerCache,
    ) -> Self {
        let proven_read_only_external_tools = collect_proven_read_only_external_tools(
            params.mcp_tools.as_deref(),
            params.deferred_mcp_tools.as_deref(),
        );
        let mut router = build_tool_router(step_context, params, tool_search_handler_cache);
        router.proven_read_only_external_tools = proven_read_only_external_tools;
        router
    }

    pub(crate) fn from_parts(registry: ToolRegistry, model_visible_specs: Vec<ToolSpec>) -> Self {
        Self {
            registry,
            model_visible_specs,
            proven_read_only_external_tools: HashSet::new(),
        }
    }

    pub fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.model_visible_specs.clone()
    }

    #[cfg(test)]
    pub(crate) fn registered_tool_names_for_test(&self) -> Vec<ToolName> {
        self.registry.tool_names_for_test()
    }

    #[cfg(test)]
    pub(crate) fn tool_exposure_for_test(
        &self,
        name: &ToolName,
    ) -> Option<crate::tools::registry::ToolExposure> {
        self.registry.tool_exposure(name)
    }

    pub(crate) fn create_diff_consumer(
        &self,
        tool_name: &ToolName,
    ) -> Option<Box<dyn ToolArgumentDiffConsumer>> {
        self.registry.create_diff_consumer(tool_name)
    }

    pub fn tool_supports_parallel(&self, call: &ToolCall) -> bool {
        self.registry
            .supports_parallel_tool_calls(&call.tool_name)
            .unwrap_or(false)
    }

    pub fn tool_waits_for_runtime_cancellation(&self, call: &ToolCall) -> bool {
        self.registry
            .waits_for_runtime_cancellation(&call.tool_name)
            .unwrap_or(false)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub fn build_tool_call(item: ResponseItem) -> Result<Option<ToolCall>, ToolCallBuildError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => {
                let tool_name = ToolName::new(namespace, name);
                Ok(Some(ToolCall {
                    tool_name,
                    call_id,
                    payload: ToolPayload::Function { arguments },
                }))
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                execution,
                arguments,
                ..
            } if execution == "client" => {
                let arguments: SearchToolCallParams = match serde_json::from_value(arguments) {
                    Ok(arguments) => arguments,
                    Err(err) => {
                        return Err(ToolCallBuildError::ToolSearchArguments {
                            call_id,
                            message: format!("failed to parse tool_search arguments: {err}"),
                        });
                    }
                };
                Ok(Some(ToolCall {
                    tool_name: ToolName::plain("tool_search"),
                    call_id,
                    payload: ToolPayload::ToolSearch { arguments },
                }))
            }
            ResponseItem::ToolSearchCall { .. } => Ok(None),
            ResponseItem::CustomToolCall {
                name,
                namespace,
                input,
                call_id,
                ..
            } => Ok(Some(ToolCall {
                tool_name: ToolName::new(namespace, name),
                call_id,
                payload: ToolPayload::Custom { input },
            })),
            _ => Ok(None),
        }
    }

    #[allow(dead_code)]
    #[instrument(level = "trace", skip_all, err)]
    pub async fn dispatch_tool_call_with_code_mode_result(
        &self,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
        source: ToolCallSource,
    ) -> Result<AnyToolResult, FunctionCallError> {
        self.dispatch_tool_call_with_code_mode_result_inner(
            session,
            step_context,
            cancellation_token,
            tracker,
            call,
            source,
            /*terminal_outcome_reached*/ None,
        )
        .await
    }

    #[instrument(level = "trace", skip_all, err)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_tool_call_with_terminal_outcome(
        &self,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
        source: ToolCallSource,
        terminal_outcome_reached: Arc<AtomicBool>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        self.dispatch_tool_call_with_code_mode_result_inner(
            session,
            step_context,
            cancellation_token,
            tracker,
            call,
            source,
            Some(terminal_outcome_reached),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_tool_call_with_code_mode_result_inner(
        &self,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        tracker: SharedTurnDiffTracker,
        call: ToolCall,
        source: ToolCallSource,
        terminal_outcome_reached: Option<Arc<AtomicBool>>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let external_mutation_intent = if self
            .proven_read_only_external_tools
            .contains(&call.tool_name)
        {
            ExternalMutationIntent::ProvenReadOnly
        } else {
            ExternalMutationIntent::MayMutate
        };
        authorize_bound_typed_tool_call(
            session.as_ref(),
            step_context.as_ref(),
            &call,
            external_mutation_intent,
        )
        .await?;
        let ToolCall {
            tool_name,
            call_id,
            payload,
        } = call;

        // Keep the legacy ToolInvocation.turn field tied to the same request state until handlers migrate.
        let turn = Arc::clone(&step_context.turn);
        let invocation = ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            tool_name,
            source,
            payload,
        };

        self.registry
            .dispatch_any_with_terminal_outcome(invocation, terminal_outcome_reached)
            .await
    }
}

fn collect_proven_read_only_external_tools(
    mcp_tools: Option<&[ToolInfo]>,
    deferred_mcp_tools: Option<&[ToolInfo]>,
) -> HashSet<ToolName> {
    let mut external_tool_read_only = HashMap::new();
    for tool in mcp_tools
        .into_iter()
        .flatten()
        .chain(deferred_mcp_tools.into_iter().flatten())
    {
        let name = ToolName::new(
            Some(tool.callable_namespace.clone()),
            tool.callable_name.clone(),
        );
        let read_only = tool
            .tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            == Some(true);
        external_tool_read_only
            .entry(name)
            .and_modify(|all_read_only| *all_read_only &= read_only)
            .or_insert(read_only);
    }
    external_tool_read_only
        .into_iter()
        .filter_map(|(name, read_only)| read_only.then_some(name))
        .collect()
}

async fn authorize_bound_typed_tool_call(
    session: &Session,
    step_context: &StepContext,
    call: &ToolCall,
    external_mutation_intent: ExternalMutationIntent,
) -> Result<(), FunctionCallError> {
    let coordinator = session.services.agent_control.task_coordinator();
    let Some(binding) = coordinator.binding_for_source(&step_context.turn.session_source) else {
        return Ok(());
    };
    let task = coordinator
        .get_agent_task(binding.assignment_id, Some(0))
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{}: typed assignment state is unavailable: {error}",
                call.tool_name.name
            ))
        })?;
    if task.current_attempt.attempt_id != binding.attempt_id
        || task.current_attempt.state != AttemptState::Active
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "{}: the bound typed assignment attempt is no longer active",
            call.tool_name.name
        )));
    }

    let cwd = match step_context.environments.primary() {
        Some(environment) => environment
            .cwd()
            .to_abs_path()
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "{}: typed assignments require a local filesystem environment: {error}",
                    call.tool_name.name
                ))
            })?
            .to_path_buf(),
        None => step_context.turn.config.cwd.to_path_buf(),
    };
    let repo_root = get_git_repo_root(&cwd).unwrap_or(cwd);
    let collaboration_namespace = step_context
        .turn
        .provider
        .capabilities()
        .namespace_tools
        .then_some(
            step_context
                .turn
                .config
                .multi_agent_v2
                .tool_namespace
                .as_deref(),
        )
        .flatten();
    let class = classify_typed_tool(
        call.tool_name.namespace.as_deref(),
        &call.tool_name.name,
        collaboration_namespace,
    );
    let authorization = authorize_typed_tool(
        &task.assignment,
        &repo_root,
        TypedToolRequest {
            class,
            external_mutation_intent,
            repo_paths: &[],
        },
    );
    match authorization {
        Ok(_) => Ok(()),
        Err(CapabilityPolicyError::MissingStructuredEditPaths)
            if class == TypedToolClass::StructuredEdit =>
        {
            // The verified apply-patch runtime owns complete path extraction and repeats
            // authorization with that closed path set immediately before execution.
            Ok(())
        }
        Err(error) => Err(FunctionCallError::RespondToModel(format!(
            "{}: typed assignment capability denied: {error}",
            call.tool_name.name
        ))),
    }
}

#[instrument(level = "trace", skip_all)]
pub(crate) fn extension_tool_executors(
    session: &Session,
) -> Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>> {
    session
        .services
        .extensions
        .tool_contributors()
        .iter()
        .flat_map(|contributor| {
            contributor.tools(
                &session.services.session_extension_data,
                &session.services.thread_extension_data,
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
