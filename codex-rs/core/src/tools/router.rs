use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ResolvedTool;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolRegistry;
use crate::tools::schema_bundle::ToolSchemaBundle;
use crate::tools::spec_plan::build_tool_router;
use codex_mcp::ToolInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::SearchToolCallParams;
use codex_tools::DiscoverableTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::borrow::Cow;
use std::ops::Deref;
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

/// Immutable call data shared by admission, dispatch, cancellation, telemetry,
/// and response projection. Cloning this envelope never clones a potentially
/// large tool payload.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SharedToolCall(Arc<ToolCall>);

impl SharedToolCall {
    pub(crate) fn new(call: ToolCall) -> Self {
        Self(Arc::new(call))
    }

    #[cfg(test)]
    pub(crate) fn shares_allocation_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for SharedToolCall {
    type Target = ToolCall;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

pub(crate) enum ToolCallPreflight {
    Function {
        tool_name: ToolName,
    },
    ToolSearch {
        tool_name: ToolName,
        arguments: SearchToolCallParams,
    },
    Custom {
        tool_name: ToolName,
    },
}

impl ToolCallPreflight {
    pub(crate) fn tool_name(&self) -> &ToolName {
        match self {
            Self::Function { tool_name }
            | Self::ToolSearch { tool_name, .. }
            | Self::Custom { tool_name } => tool_name,
        }
    }

    pub(crate) fn log_payload<'a>(&'a self, item: &'a ResponseItem) -> Cow<'a, str> {
        match (self, item) {
            (Self::Function { .. }, ResponseItem::FunctionCall { arguments, .. }) => {
                Cow::Borrowed(arguments)
            }
            (Self::ToolSearch { arguments, .. }, ResponseItem::ToolSearchCall { .. }) => {
                Cow::Borrowed(&arguments.query)
            }
            (Self::Custom { .. }, ResponseItem::CustomToolCall { input, .. }) => {
                Cow::Borrowed(input)
            }
            _ => unreachable!("tool preflight must be consumed with its source item"),
        }
    }

    pub(crate) fn into_tool_call(self, item: ResponseItem) -> ToolCall {
        match (self, item) {
            (
                Self::Function { tool_name },
                ResponseItem::FunctionCall {
                    arguments, call_id, ..
                },
            ) => ToolCall {
                tool_name,
                call_id,
                payload: ToolPayload::Function { arguments },
            },
            (
                Self::ToolSearch {
                    tool_name,
                    arguments,
                },
                ResponseItem::ToolSearchCall {
                    call_id: Some(call_id),
                    execution,
                    ..
                },
            ) => {
                debug_assert_eq!(execution, "client");
                ToolCall {
                    tool_name,
                    call_id,
                    payload: ToolPayload::ToolSearch { arguments },
                }
            }
            (Self::Custom { tool_name }, ResponseItem::CustomToolCall { input, call_id, .. }) => {
                ToolCall {
                    tool_name,
                    call_id,
                    payload: ToolPayload::Custom { input },
                }
            }
            _ => unreachable!("tool preflight must be consumed with its source item"),
        }
    }
}

pub struct ToolRouter {
    registry: ToolRegistry,
    model_visible_schema_bundle: Arc<ToolSchemaBundle>,
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
        build_tool_router(step_context, params, tool_search_handler_cache)
    }

    pub(crate) fn from_parts(registry: ToolRegistry, model_visible_specs: Vec<ToolSpec>) -> Self {
        Self {
            registry,
            model_visible_schema_bundle: Arc::new(ToolSchemaBundle::new(model_visible_specs)),
        }
    }

    #[allow(dead_code)]
    pub fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.model_visible_schema_bundle.canonical().to_vec()
    }

    pub(crate) fn model_visible_schema_bundle(&self) -> &Arc<ToolSchemaBundle> {
        &self.model_visible_schema_bundle
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

    pub(crate) fn resolve_tool_call(&self, call: &SharedToolCall) -> ResolvedTool {
        self.registry.resolve_tool(&call.tool_name, &call.payload)
    }

    #[instrument(level = "trace", skip_all, err)]
    #[allow(dead_code)]
    pub fn build_tool_call(item: ResponseItem) -> Result<Option<ToolCall>, FunctionCallError> {
        let Some(preflight) = Self::preflight_tool_call(&item)? else {
            return Ok(None);
        };
        Ok(Some(preflight.into_tool_call(item)))
    }

    pub(crate) fn preflight_tool_call(
        item: &ResponseItem,
    ) -> Result<Option<ToolCallPreflight>, FunctionCallError> {
        match item {
            ResponseItem::FunctionCall {
                name, namespace, ..
            } => Ok(Some(ToolCallPreflight::Function {
                tool_name: ToolName::new(namespace.clone(), name.clone()),
            })),
            ResponseItem::ToolSearchCall {
                call_id: Some(_),
                execution,
                arguments,
                ..
            } if execution == "client" => {
                let arguments = SearchToolCallParams::deserialize(arguments).map_err(|err| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to parse tool_search arguments: {err}"
                    ))
                })?;
                Ok(Some(ToolCallPreflight::ToolSearch {
                    tool_name: ToolName::plain("tool_search"),
                    arguments,
                }))
            }
            ResponseItem::ToolSearchCall { .. } => Ok(None),
            ResponseItem::CustomToolCall {
                name, namespace, ..
            } => Ok(Some(ToolCallPreflight::Custom {
                tool_name: ToolName::new(namespace.clone(), name.clone()),
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
        let call = SharedToolCall::new(call);
        let resolved = self.resolve_tool_call(&call);
        self.dispatch_tool_call_with_code_mode_result_inner(
            session,
            step_context,
            cancellation_token,
            tracker,
            call,
            source,
            resolved,
            /*terminal_outcome_reached*/ None,
        )
        .await
    }

    #[instrument(level = "trace", skip_all, err)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_resolved_tool_call_with_terminal_outcome(
        &self,
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        tracker: SharedTurnDiffTracker,
        call: SharedToolCall,
        source: ToolCallSource,
        resolved: ResolvedTool,
        terminal_outcome_reached: Arc<AtomicBool>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        self.dispatch_tool_call_with_code_mode_result_inner(
            session,
            step_context,
            cancellation_token,
            tracker,
            call,
            source,
            resolved,
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
        call: SharedToolCall,
        source: ToolCallSource,
        resolved: ResolvedTool,
        terminal_outcome_reached: Option<Arc<AtomicBool>>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        // Keep the legacy ToolInvocation.turn field tied to the same request state until handlers migrate.
        let turn = Arc::clone(&step_context.turn);
        let invocation = ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id: call.call_id.clone(),
            tool_name: call.tool_name.clone(),
            source,
            payload: call.payload.clone(),
        };

        self.registry
            .dispatch_resolved_with_terminal_outcome(
                resolved,
                call,
                invocation,
                terminal_outcome_reached,
            )
            .await
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
