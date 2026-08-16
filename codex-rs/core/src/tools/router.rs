use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::authorize_typed_tool;
use crate::agent::task_capabilities::classify_typed_tool;
use crate::agent::task_capabilities::is_independent_review_source;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec_plan::build_tool_router;
use crate::tools::tool_dispatch_trace::record_authorization_state_coordination;
use codex_agent_task_store::AttemptState;
use codex_mcp::ToolInfo;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::SearchToolCallParams;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ToolManifestItem;
use codex_tools::DiscoverableTool;
use codex_tools::ToolCall as ExtensionToolCall;
use codex_tools::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
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

pub(crate) fn build_function_tool_payload(
    tool_name: &ToolName,
    arguments: String,
) -> Result<ToolPayload, String> {
    if tool_name == &ToolName::plain("tool_search") {
        let arguments: SearchToolCallParams = serde_json::from_str(&arguments)
            .map_err(|err| format!("failed to parse tool_search arguments: {err}"))?;
        return Ok(ToolPayload::ToolSearch { arguments });
    }

    Ok(ToolPayload::Function { arguments })
}

pub struct ToolRouter {
    registry: ToolRegistry,
    model_visible_specs: Vec<ToolSpec>,
    planning_warnings: Vec<String>,
    proven_read_only_external_tools: HashSet<ToolName>,
    exposure_identity: ToolExposureIdentity,
    manifest_cache: Mutex<Option<(String, u64, ToolManifestItem)>>,
}

pub(crate) struct ToolRouterParams<'a> {
    pub(crate) mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) deferred_mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) tool_suggest_candidates: Option<ToolSuggestCandidates>,
    pub(crate) extension_tool_executors: Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    pub(crate) dynamic_tools: &'a [DynamicToolSpec],
    pub(crate) exposure_identity: ToolExposureIdentity,
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

    #[cfg(test)]
    pub(crate) fn from_parts(registry: ToolRegistry, model_visible_specs: Vec<ToolSpec>) -> Self {
        Self::from_parts_with_warnings(registry, model_visible_specs, Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn from_parts_with_warnings(
        registry: ToolRegistry,
        model_visible_specs: Vec<ToolSpec>,
        planning_warnings: Vec<String>,
    ) -> Self {
        Self::from_parts_with_warnings_and_identity(
            registry,
            model_visible_specs,
            planning_warnings,
            ToolExposureIdentity::default(),
        )
    }

    pub(crate) fn from_parts_with_warnings_and_identity(
        registry: ToolRegistry,
        model_visible_specs: Vec<ToolSpec>,
        planning_warnings: Vec<String>,
        exposure_identity: ToolExposureIdentity,
    ) -> Self {
        Self {
            registry,
            model_visible_specs,
            planning_warnings,
            proven_read_only_external_tools: HashSet::new(),
            exposure_identity,
            manifest_cache: Mutex::new(None),
        }
    }

    pub(crate) fn exposure_identity(&self) -> &ToolExposureIdentity {
        &self.exposure_identity
    }

    pub fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.model_visible_specs.clone()
    }

    pub(crate) fn deferred_tool_capability_revisions(&self) -> HashMap<ToolName, String> {
        let exposure_identity = serde_json::to_value(&self.exposure_identity).unwrap_or_default();
        self.registry
            .manifest_entries()
            .into_iter()
            .filter(|(_, exposure, _)| *exposure == crate::tools::registry::ToolExposure::Deferred)
            .map(|(name, _, spec)| {
                let encoded = serde_json::to_vec(&serde_json::json!({
                    "tool_exposure_identity": exposure_identity,
                    "provenance": name.to_string(),
                    "spec": spec,
                }))
                .unwrap_or_default();
                (name, format!("{:x}", Sha256::digest(encoded)))
            })
            .collect()
    }

    pub(crate) fn tool_manifest(
        &self,
        turn: &crate::session::turn_context::TurnContext,
    ) -> ToolManifestItem {
        let activation_revision = turn.deferred_tool_activation_revision();
        let turn_id = turn.sub_id.clone();
        if let Some((_, _, manifest)) = self
            .manifest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|(cached_turn, cached_revision, _)| {
                cached_turn == &turn_id && *cached_revision == activation_revision
            })
        {
            return manifest.clone();
        }
        let registered = self
            .registry
            .manifest_entries()
            .into_iter()
            .map(|(name, exposure, spec)| {
                let canonical_spec =
                    canonicalize_json(serde_json::to_value(spec).unwrap_or_default());
                let spec_sha256 = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&canonical_spec).unwrap_or_default())
                );
                let exposure_name = match exposure {
                    crate::tools::registry::ToolExposure::Direct => "direct",
                    crate::tools::registry::ToolExposure::Deferred => "deferred",
                    crate::tools::registry::ToolExposure::DirectModelOnly => "direct_model_only",
                    crate::tools::registry::ToolExposure::Hidden => "hidden",
                };
                serde_json::json!({
                    "name": name.to_string(),
                    "exposure": exposure_name,
                    "activated": exposure != crate::tools::registry::ToolExposure::Deferred
                        || turn.deferred_tool_is_activated(&name),
                    // Full schemas already live in `model_visible` or the deferred
                    // discovery index. Persist only their canonical identity here so
                    // the rollout manifest does not scale with every registered schema.
                    "spec_sha256": spec_sha256,
                })
            })
            .collect::<Vec<_>>();
        let manifest = canonicalize_json(serde_json::json!({
            "model_visible": self.model_visible_specs,
            "registered": registered,
        }));
        let fingerprint_input = serde_json::json!({
            "manifest": &manifest,
            "tool_exposure_identity": &self.exposure_identity,
        });
        let encoded = serde_json::to_vec(&fingerprint_input).unwrap_or_default();
        let item = ToolManifestItem {
            hash: format!("{:x}", Sha256::digest(encoded)),
            manifest,
        };
        *self
            .manifest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((turn_id, activation_revision, item.clone()));
        item
    }

    pub(crate) fn planning_warnings(&self) -> &[String] {
        &self.planning_warnings
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
                let payload =
                    build_function_tool_payload(&tool_name, arguments).map_err(|message| {
                        ToolCallBuildError::ToolSearchArguments {
                            call_id: call_id.clone(),
                            message,
                        }
                    })?;
                Ok(Some(ToolCall {
                    tool_name,
                    call_id,
                    payload,
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
        if self.registry.tool_exposure(&call.tool_name)
            == Some(crate::tools::registry::ToolExposure::Deferred)
            && !step_context
                .turn
                .deferred_tool_is_activated(&call.tool_name)
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "tool `{}` is deferred; select it with tool_search during this turn before calling it",
                call.tool_name
            )));
        }
        let external_mutation_intent = if self
            .proven_read_only_external_tools
            .contains(&call.tool_name)
        {
            ExternalMutationIntent::ProvenReadOnly
        } else {
            ExternalMutationIntent::MayMutate
        };
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
        authorize_independent_review_tool_call(
            &step_context.turn.session_source,
            collaboration_namespace,
            &call,
            external_mutation_intent,
        )?;
        let authorization_state_started = Instant::now();
        let authorization_result = authorize_bound_typed_tool_call(
            session.as_ref(),
            step_context.as_ref(),
            &call,
            external_mutation_intent,
        )
        .await;
        record_authorization_state_coordination(authorization_state_started.elapsed());
        authorization_result?;
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

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json(value)))
                    .collect(),
            )
        }
        value => value,
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
            .unwrap_or_else(|| is_allowlisted_read_only_external_tool(&name));
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

fn is_allowlisted_read_only_external_tool(name: &ToolName) -> bool {
    matches!(
        (name.namespace.as_deref(), name.name.as_str()),
        (
            Some("mcp__repo_atlas"),
            "batch"
                | "cochange"
                | "context_for"
                | "contract"
                | "crate_graph"
                | "crate_summary"
                | "find_def"
                | "find_refs"
                | "impact"
                | "index_status"
                | "outline"
                | "repo_facts"
                | "select_root"
                | "slice"
                | "trace"
                | "where_belongs",
        ) | (
            Some("mcp__codex_apps__github"),
            "fetch"
                | "fetch_blob"
                | "fetch_commit"
                | "fetch_commit_workflow_runs"
                | "fetch_file"
                | "fetch_issue"
                | "fetch_issue_comments"
                | "fetch_pr"
                | "fetch_pr_comments"
                | "fetch_pr_file_patch"
                | "fetch_pr_patch"
                | "fetch_workflow_job_logs"
                | "fetch_workflow_job_steps"
                | "fetch_workflow_run_artifacts"
                | "fetch_workflow_run_jobs",
        )
    )
}

fn authorize_independent_review_tool_call(
    session_source: &SessionSource,
    collaboration_namespace: Option<&str>,
    call: &ToolCall,
    external_mutation_intent: ExternalMutationIntent,
) -> Result<(), FunctionCallError> {
    if !is_independent_review_source(session_source) {
        return Ok(());
    }
    let class = classify_typed_tool(
        call.tool_name.namespace.as_deref(),
        &call.tool_name.name,
        collaboration_namespace,
    );
    let allowed = matches!(
        class,
        TypedToolClass::AgentCommunication
            | TypedToolClass::OwnTask
            | TypedToolClass::ReadSearch
            | TypedToolClass::CodeModeControl
            | TypedToolClass::Diff
            | TypedToolClass::Shell
    ) || (class == TypedToolClass::DynamicExternal
        && external_mutation_intent == ExternalMutationIntent::ProvenReadOnly);
    if allowed {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "{}: independent review capability denied: only read-only repository inspection tools are available",
            call.tool_name.name
        )))
    }
}

async fn authorize_bound_typed_tool_call(
    session: &Session,
    step_context: &StepContext,
    call: &ToolCall,
    _external_mutation_intent: ExternalMutationIntent,
) -> Result<(), FunctionCallError> {
    let coordinator = session.services.agent_control.task_coordinator();
    let Some(binding) = coordinator.binding_for_source(&step_context.turn.session_source) else {
        return Ok(());
    };
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

    authorize_typed_tool(class).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "{}: typed assignment capability denied: {error}",
            call.tool_name.name
        ))
    })?;
    let heartbeated = coordinator
        .heartbeat_typed_actor_binding(&binding)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{}: typed assignment heartbeat failed: {error}",
                call.tool_name.name
            ))
        })?;
    if !heartbeated {
        return Err(FunctionCallError::RespondToModel(format!(
            "{}: the bound typed assignment attempt is no longer active",
            call.tool_name.name
        )));
    }
    Ok(())
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
