use crate::FunctionCallError;
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::authorize_typed_tool;
use crate::agent::task_capabilities::is_independent_review_source;
use crate::client_common::ToolSchemaArtifact;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::AnyToolResult;
use crate::tools::registry::RegisteredTool;
use crate::tools::registry::ToolArgumentDiffConsumer;
use crate::tools::registry::ToolRegistry;
use crate::tools::spec_plan::build_tool_router;
use crate::tools::tool_dispatch_trace::record_authorization_state_coordination;
use codex_agent_task_store::AssignmentAdmissionOrigin;
use codex_agent_task_store::AttemptState;
use codex_config::schema::canonicalize as canonicalize_json;
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
use std::sync::OnceLock;
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

// Advisory only: schemas carry executable contracts and must not be silently cut.
const TOOL_SCHEMA_WARNING_BYTES: usize = 128 * 1024;

fn tool_schema_size_warning(schemas: &ToolSchemaArtifact) -> Option<String> {
    let bytes = schemas.serialized().len();
    (bytes > TOOL_SCHEMA_WARNING_BYTES).then(|| format!(
        "The model-visible tool schemas use {bytes} bytes (advisory budget: {TOOL_SCHEMA_WARNING_BYTES}). Disable unused tools or enable deferred tool discovery to reduce request overhead."
    ))
}

pub struct ToolRouter {
    registry: ToolRegistry,
    planning_warnings: Vec<String>,
    exposure_identity: ToolExposureIdentity,
    resolved_planning_identity: Option<ResolvedToolPlanningIdentity>,
    pub(crate) tool_suggest_candidates: Option<ToolSuggestCandidates>,
    pub(crate) tool_search_sources: String,
    schema_cache: Mutex<Option<(HashSet<ToolName>, Arc<ToolSchemaArtifact>)>>,
    manifest_cache: Mutex<ToolManifestCache>,
    deferred_tool_capability_revisions: OnceLock<Arc<HashMap<ToolName, String>>>,
}

/// Retain the resolved inputs used by the canonical planner, not just the
/// requested model slug. Catalog refreshes can change either under equal config.
struct ResolvedToolPlanningIdentity {
    config: Arc<crate::config::Config>,
    model_info: codex_protocol::openai_models::ModelInfo,
    available_models: Arc<Vec<codex_protocol::openai_models::ModelPreset>>,
    code_mode_available: bool,
    multi_agent_version: codex_protocol::protocol::MultiAgentVersion,
}

#[derive(Default)]
struct ToolManifestCache {
    base: Option<ToolManifestItem>,
    activated: Option<(HashSet<ToolName>, ToolManifestItem)>,
}

impl ToolManifestCache {
    fn get(&self, activated: &HashSet<ToolName>) -> Option<&ToolManifestItem> {
        if activated.is_empty() {
            self.base.as_ref()
        } else {
            self.activated
                .as_ref()
                .filter(|(cached_activated, _)| cached_activated == activated)
                .map(|(_, manifest)| manifest)
        }
    }

    fn insert(&mut self, activated: HashSet<ToolName>, manifest: ToolManifestItem) {
        if activated.is_empty() {
            self.base = Some(manifest);
        } else {
            self.activated = Some((activated, manifest));
        }
    }
}

pub(crate) struct ToolRouterParams<'a> {
    pub(crate) mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) deferred_mcp_tools: Option<Vec<ToolInfo>>,
    pub(crate) tool_suggest_candidates: Option<ToolSuggestCandidates>,
    pub(crate) extension_tool_executors: Vec<Arc<dyn ToolExecutor<ExtensionToolCall>>>,
    pub(crate) dynamic_tools: &'a [DynamicToolSpec],
    pub(crate) exposure_identity: ToolExposureIdentity,
}

/// Completes an admitted dispatch even when router-side validation returns
/// before the registry can transfer terminal ownership to a handler.
struct ToolDispatchCompletionGuard(Arc<ToolDispatchState>);

impl Drop for ToolDispatchCompletionGuard {
    fn drop(&mut self) {
        // A panic is not a validation return. Leave the election to the
        // dispatch owner that joins the panic and records its failed trace.
        if !std::thread::panicking() {
            let _ = self.0.try_complete();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolSuggestPresentation {
    ListTool,
    RecommendationContext,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolSuggestCandidates {
    pub(crate) tools: Vec<DiscoverableTool>,
    pub(crate) presentation: ToolSuggestPresentation,
}

impl ToolRouter {

    pub(crate) fn try_from_context(
        step_context: &StepContext,
        params: ToolRouterParams<'_>,
        tool_search_handler_cache: &ToolSearchHandlerCache,
    ) -> Result<Self, String> {
        build_tool_router(step_context, params, tool_search_handler_cache)
    }

    pub(crate) fn with_planning_context(mut self, turn: &TurnContext) -> Self {
        self.resolved_planning_identity = Some(ResolvedToolPlanningIdentity {
            config: Arc::clone(&turn.config),
            model_info: turn.model_info.clone(),
            available_models: Arc::clone(&turn.available_models),
            code_mode_available: turn.code_mode_available,
            multi_agent_version: turn.multi_agent_version,
        });
        self
    }

    pub(crate) fn matches_planning_context(&self, turn: &TurnContext) -> bool {
        self.resolved_planning_identity
            .as_ref()
            .is_some_and(|identity| {
                (Arc::ptr_eq(&identity.config, &turn.config)
                    || identity.config.as_ref() == turn.config.as_ref())
                    && identity.model_info == turn.model_info
                    && (Arc::ptr_eq(&identity.available_models, &turn.available_models)
                        || identity.available_models.as_ref() == turn.available_models.as_ref())
                    && identity.code_mode_available == turn.code_mode_available
                    && identity.multi_agent_version == turn.multi_agent_version
            })
    }




    pub(crate) fn from_parts_with_prepared_search_sources(
        mut registry: ToolRegistry,
        model_visible_specs: Vec<ToolSpec>,
        planning_warnings: Vec<String>,
        exposure_identity: ToolExposureIdentity,
        search_sources: Option<String>,
    ) -> Self {
        registry.set_model_visible_specs(model_visible_specs);
        let mut tool_search_sources = search_sources.unwrap_or_else(|| {
            let search_infos = registry
                .manifest_entries()
                .into_iter()
                .filter(|tool| tool.exposure() == codex_tools::ToolExposure::Deferred)
                .filter_map(RegisteredTool::search_info_ref)
                .collect::<Vec<_>>();
            if search_infos.is_empty() {
                String::new()
            } else {
                crate::tools::handlers::tool_search_spec::render_tool_search_sources(
                    search_infos.iter().filter_map(|info| info.source_info.as_ref()),
                    search_infos.iter().any(|info| info.source_info.is_none()),
                    search_infos.iter().copied(),
                )
            }
        });
        // Keep installed-but-rejected capabilities visible through the existing
        // source notice even when no callable survived registration.
        for warning in planning_warnings
            .iter()
            .filter(|warning| warning.starts_with(super::spec_plan::REJECTED_TOOL_WARNING_PREFIX))
        {
            if !tool_search_sources.is_empty() {
                tool_search_sources.push('\n');
            }
            tool_search_sources.push_str(warning);
        }
        let mut planning_warnings = planning_warnings;
        if let Some(warning) = tool_schema_size_warning(&registry.model_visible_schemas()) {
            tracing::warn!("{warning}");
            planning_warnings.push(warning);
        }
        Self {
            registry,
            planning_warnings,
            exposure_identity,
            resolved_planning_identity: None,
            tool_suggest_candidates: None,
            tool_search_sources,
            schema_cache: Mutex::new(None),
            manifest_cache: Mutex::new(ToolManifestCache::default()),
            deferred_tool_capability_revisions: OnceLock::new(),
        }
    }

    pub(crate) fn has_registered_tool(&self, name: &ToolName) -> bool {
        self.registry.tool_exposure(name).is_some()
    }

    pub(crate) fn tool_search_sources(&self) -> &str {
        &self.tool_search_sources
    }

    /// Tools the deferred Desktop guidance names that this inventory, including
    /// deferred tools, lacks. Routers can be reused across turns, so the caller
    /// passes the current instructions rather than relying on the cache identity.
    pub(crate) fn desktop_guidance_unavailable_tools(&self, instructions: &str) -> String {
        let entries = self.registry.manifest_entries();
        let available = entries
            .iter()
            .map(|tool| tool.tool_name().name.as_str())
            .collect();
        crate::context::desktop_instructions::unavailable_tools(instructions, &available)
    }

    pub(crate) fn exposure_identity(&self) -> &ToolExposureIdentity {
        &self.exposure_identity
    }

    pub(crate) fn classify_tool_name(
        &self,
        _turn: &crate::session::turn_context::TurnContext,
        tool_name: &ToolName,
    ) -> TypedToolClass {
        self.registry
            .tool_authorization_class(tool_name)
            .unwrap_or(TypedToolClass::Unknown)
    }

    pub fn model_visible_specs(&self) -> Vec<ToolSpec> {
        self.registry.model_visible_specs()
    }

    pub(crate) fn model_visible_schemas(&self) -> Arc<ToolSchemaArtifact> {
        self.registry.model_visible_schemas()
    }

    pub(crate) fn model_visible_schemas_for_turn(
        &self,
        turn: &crate::session::turn_context::TurnContext,
    ) -> Arc<ToolSchemaArtifact> {
        if crate::tools::effective_tool_mode(turn)
            != codex_protocol::openai_models::ToolMode::CodeModeOnly
            || !turn.config.code_mode.excluded_tool_namespaces.is_empty()
        {
            // Direct calls need activated schemas in the API tools array.
            return self.tool_schema_snapshot(&turn.activated_deferred_tools());
        }
        // Activated schemas travel in appended history, never ahead of it.
        // A rebuilt router can have new eager capabilities within this same
        // turn (for example, an environment becoming ready). Reuse only an
        // identical authoritative base schema, not an arbitrary older router.
        let base_schemas = self.registry.model_visible_schemas();
        let mut state = turn
            .deferred_tool_activations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(schemas) = state.model_visible_schemas.as_ref()
            && schemas.digest() == base_schemas.digest()
        {
            return Arc::clone(schemas);
        }
        let schemas = self.tool_schema_snapshot(&HashSet::new());
        state.model_visible_schemas = Some(Arc::clone(&schemas));
        schemas
    }

    pub(crate) fn deferred_tool_capability_revisions(&self) -> Arc<HashMap<ToolName, String>> {
        Arc::clone(self.deferred_tool_capability_revisions.get_or_init(|| {
            Arc::new(
                self.registry
                    .manifest_entries()
                    .into_iter()
                    .filter(|tool| {
                        tool.exposure() == crate::tools::registry::ToolExposure::Deferred
                    })
                    .map(|tool| {
                        let name = tool.tool_name();
                        let encoded = serde_json::to_vec(&serde_json::json!({
                            "provenance": name.to_string(),
                            "provider_authority": tool.provider_authority(),
                            "spec_sha256": tool.canonical_spec_sha256(),
                            "callable_contract": tool.spec().callable_contract(),
                        }))
                        .unwrap_or_default();
                        (name.clone(), hex::encode(Sha256::digest(encoded)))
                    })
                    .collect(),
            )
        }))
    }


    /// Return the manifest record for this request without cloning the full schema tree when the
    /// same hash was already queued for this rollout. The recorder still owns canonical
    /// definition/delta encoding; this only avoids repeatedly moving an unchanged definition
    /// through the request-preparation path.
    pub(crate) fn tool_manifest_for_rollout(
        &self,
        turn: &crate::session::turn_context::TurnContext,
        previous_hash: Option<&str>,
    ) -> ToolManifestItem {
        let (_, activated) = turn.deferred_tool_activation_snapshot();
        {
            let cache = self
                .manifest_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(manifest) = cache.get(&activated)
                && previous_hash == Some(manifest.hash.as_str())
            {
                return ToolManifestItem::reference(manifest.hash.clone());
            }
        }

        let manifest = self.tool_manifest_snapshot(&activated, turn);
        if previous_hash == Some(manifest.hash.as_str()) {
            ToolManifestItem::reference(manifest.hash)
        } else {
            manifest
        }
    }

    fn tool_schema_snapshot(&self, activated: &HashSet<ToolName>) -> Arc<ToolSchemaArtifact> {
        let mut cache = self
            .schema_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, schemas)) = cache
            .as_ref()
            .filter(|(cached_activated, _)| cached_activated == activated)
        {
            return Arc::clone(schemas);
        }

        let schemas = if activated.is_empty() {
            self.registry.model_visible_schemas()
        } else {
            let mut visible = self.model_visible_specs();
            for tool in self.registry.manifest_entries() {
                if tool.exposure() != crate::tools::registry::ToolExposure::Deferred
                    || !activated.contains(tool.tool_name())
                {
                    continue;
                }
                let Some(spec) = filter_activated_deferred_spec(tool.spec(), activated) else {
                    continue;
                };
                merge_visible_tool_spec(&mut visible, spec);
            }
            // Registration keeps full namespace guidance for lazy resolution; an
            // activated namespace joins the final projection and shares its budget.
            apply_visible_namespace_description_budget(&mut visible);
            Arc::new(ToolSchemaArtifact::new(visible))
        };
        if !activated.is_empty()
            && let Some(warning) = tool_schema_size_warning(&schemas)
        {
            tracing::warn!("{warning}");
        }
        *cache = Some((activated.clone(), Arc::clone(&schemas)));
        schemas
    }

    fn tool_manifest_snapshot(
        &self,
        activated: &HashSet<ToolName>,
        turn: &crate::session::turn_context::TurnContext,
    ) -> ToolManifestItem {
        let mut cache = self
            .manifest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(manifest) = cache.get(activated) {
            return manifest.clone();
        }

        let model_visible = self.model_visible_schemas_for_turn(turn);
        let schemas = self.tool_schema_snapshot(activated);
        let registered = self
            .registry
            .manifest_entries()
            .into_iter()
            .map(|tool| {
                let name = tool.tool_name();
                let exposure = tool.exposure();
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
                        || activated.contains(name),
                    // Full schemas already live in `model_visible` or the deferred
                    // discovery index. Persist only their canonical identity here so
                    // the rollout manifest does not scale with every registered schema.
                    "spec_sha256": tool.canonical_spec_sha256(),
                })
            })
            .collect::<Vec<_>>();
        let model_visible = serde_json::to_value(model_visible.specs()).unwrap_or_default();
        // The activated surface is `model_visible` plus activated deferred tools.
        // Persist only those additions; storing the full activated snapshot
        // repeated every model-visible schema in each manifest definition.
        let activated_additional_schemas = match serde_json::to_value(schemas.specs()) {
            Ok(serde_json::Value::Array(specs)) => specs
                .into_iter()
                .filter(|spec| {
                    !model_visible
                        .as_array()
                        .is_some_and(|visible| visible.contains(spec))
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let manifest = canonicalize_json(&serde_json::json!({
            "model_visible": model_visible,
            "activated_additional_schemas": activated_additional_schemas,
            "registered": registered,
        }));
        let fingerprint_input = serde_json::json!({
            "manifest": &manifest,
            "tool_exposure_identity": &self.exposure_identity,
        });
        let encoded = serde_json::to_vec(&fingerprint_input).unwrap_or_default();
        let manifest = ToolManifestItem::full(hex::encode(Sha256::digest(encoded)), manifest);
        cache.insert(activated.clone(), manifest.clone());
        manifest
    }




    pub(crate) fn planning_warnings(&self) -> &[String] {
        &self.planning_warnings
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

    pub(crate) fn pre_tool_use_hook_may_run(
        &self,
        hooks: &codex_hooks::Hooks,
        call: &ToolCall,
    ) -> bool {
        self.registry
            .pre_tool_use_hook_may_run(hooks, &call.tool_name, &call.payload)
    }

    pub fn tool_waits_for_runtime_cancellation(&self, call: &ToolCall) -> bool {
        self.registry
            .waits_for_runtime_cancellation(&call.tool_name)
            .unwrap_or(false)
    }

    pub fn tool_owns_unified_exec_processes(&self, call: &ToolCall) -> bool {
        self.registry
            .owns_unified_exec_processes(&call.tool_name)
            .unwrap_or(false)
    }

    pub fn tool_cancellation_requires_commit_barrier(&self, call: &ToolCall) -> bool {
        self.registry
            .cancellation_requires_commit_barrier(&call.tool_name)
            .unwrap_or(false)
    }

    pub(crate) fn tool_cancellation_cleanup_deadline(
        &self,
        call: &ToolCall,
    ) -> std::time::Duration {
        self.registry.cancellation_cleanup_deadline(&call.tool_name)
    }

    pub(crate) fn cancellation_recovery(
        &self,
        name: &ToolName,
        result: Option<&serde_json::Value>,
        error: Option<&str>,
    ) -> crate::tools::context::ToolEffectRecovery {
        self.registry.cancellation_recovery(name, result, error)
    }

    pub(crate) fn semantic_tool_name(&self, name: &ToolName) -> ToolName {
        self.registry.semantic_tool_name(name)
    }

    pub(crate) fn semantic_capabilities(
        &self,
        name: &ToolName,
    ) -> crate::tools::registry::ToolSemanticCapabilities {
        self.registry.semantic_capabilities(name)
    }

    pub(crate) fn terminal_failure_reuse(
        &self,
        name: &ToolName,
    ) -> crate::tools::registry::TerminalFailureReuse {
        self.registry.terminal_failure_reuse(name)
    }

    pub(crate) fn terminal_result_adapter(&self, name: &ToolName) -> Option<&'static str> {
        self.registry.terminal_result_adapter(name)
    }

    pub(crate) fn permits_shared_workspace_observation(&self, call: &ToolCall) -> bool {
        self.registry
            .permits_shared_workspace_observation(&call.tool_name, &call.payload)
    }

    pub(crate) fn prepares_during_workspace_baseline(&self, call: &ToolCall) -> bool {
        self.registry
            .prepares_during_workspace_baseline(&call.tool_name)
    }

    pub(crate) fn delegates_workspace_admission(&self, call: &ToolCall) -> bool {
        self.registry
            .delegates_workspace_admission(&call.tool_name, &call.payload)
    }

    pub(crate) fn native_addition_paths(&self, call: &ToolCall) -> Option<Vec<std::path::PathBuf>> {
        self.registry
            .native_addition_paths(&call.tool_name, &call.payload)
    }

    #[instrument(level = "trace", skip_all, err)]
    pub fn build_tool_call(item: &ResponseItem) -> Result<Option<ToolCall>, ToolCallBuildError> {
        match item {
            ResponseItem::FunctionCall {
                name,
                namespace,
                arguments,
                call_id,
                ..
            } => {
                let tool_name = ToolName::new(namespace.clone(), name.clone());
                Ok(Some(ToolCall {
                    tool_name,
                    call_id: call_id.clone(),
                    payload: ToolPayload::Function {
                        arguments: arguments.clone(),
                    },
                }))
            }
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                execution,
                arguments,
                ..
            } if execution == "client" => {
                let arguments: SearchToolCallParams =
                    match serde::Deserialize::deserialize(arguments) {
                        Ok(arguments) => arguments,
                        Err(err) => {
                            return Err(ToolCallBuildError::ToolSearchArguments {
                                call_id: call_id.clone(),
                                message: format!("failed to parse tool_search arguments: {err}"),
                            });
                        }
                    };
                Ok(Some(ToolCall {
                    tool_name: ToolName::plain("tool_search"),
                    call_id: call_id.clone(),
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
                tool_name: ToolName::new(namespace.clone(), name.clone()),
                call_id: call_id.clone(),
                payload: ToolPayload::Custom {
                    input: input.clone(),
                },
            })),
            _ => Ok(None),
        }
    }

    pub(crate) async fn prepare_hook_input(
        &self,
        invocation: ToolInvocation,
    ) -> Result<(ToolCall, Option<String>), FunctionCallError> {
        let (invocation, notice) = self.registry.prepare_hook_input(invocation).await?;
        Ok((
            ToolCall {
                tool_name: invocation.tool_name,
                call_id: invocation.call_id,
                payload: invocation.payload,
            },
            notice,
        ))
    }

    pub(crate) fn post_tool_use_hook_may_run(&self, invocation: &ToolInvocation) -> bool {
        self.registry.post_tool_use_hook_may_run(invocation)
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
        dispatch_state: Arc<ToolDispatchState>,
    ) -> Result<AnyToolResult, FunctionCallError> {
        let _completion_guard = ToolDispatchCompletionGuard(Arc::clone(&dispatch_state));
        let registered = self.registry.registered_tool(&call.tool_name);
        if registered.map(RegisteredTool::exposure)
            == Some(crate::tools::registry::ToolExposure::Deferred)
            && !step_context
                .turn
                .deferred_tool_is_activated(&call.tool_name)
        {
            // An exact router match is already unambiguous. Activate it in the
            // same dispatch transaction so callers avoid a separate discovery
            // round trip. TurnContext only records the activation
            // when the current capability revision is still registered.
            step_context
                .turn
                .activate_deferred_tools(std::iter::once(call.tool_name.clone()));
            if !step_context
                .turn
                .deferred_tool_is_activated(&call.tool_name)
            {
                return Err(FunctionCallError::RespondToModel(format!(
                    "tool `{}` is deferred but its current capability revision is unavailable; refresh tool discovery",
                    call.tool_name
                )));
            }
        }
        let external_mutation_intent = registered
            .map(RegisteredTool::external_mutation_intent)
            .unwrap_or(ExternalMutationIntent::MayMutate);
        let tool_class = registered
            .map(RegisteredTool::authorization_class)
            .unwrap_or(TypedToolClass::Unknown);
        authorize_independent_review_tool_call(
            &step_context.turn.session_source,
            tool_class,
            &call,
            external_mutation_intent,
        )?;
        let authorization_state_started = Instant::now();
        let authorization_result = authorize_bound_typed_tool_call(
            session.as_ref(),
            step_context.as_ref(),
            tool_class,
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

        let invocation = ToolInvocation {
            session,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            tool_name,
            source,
            payload,
        };

        self.registry
            .dispatch_any_with_terminal_outcome(invocation, Arc::clone(&dispatch_state))
            .await
    }
}

fn filter_activated_deferred_spec(
    spec: &ToolSpec,
    activated: &HashSet<ToolName>,
) -> Option<ToolSpec> {
    match spec {
        ToolSpec::Namespace(namespace) => {
            let mut namespace = namespace.clone();
            let namespace_name = namespace.name.clone();
            namespace.tools.retain(|tool| match tool {
                codex_tools::ResponsesApiNamespaceTool::Function(tool) => activated.contains(
                    &ToolName::namespaced(namespace_name.clone(), tool.name.clone()),
                ),
            });
            (!namespace.tools.is_empty()).then_some(ToolSpec::Namespace(namespace))
        }
        ToolSpec::Function(tool) => activated
            .contains(&ToolName::plain(tool.name.clone()))
            .then(|| spec.clone()),
        ToolSpec::Freeform(tool) => activated
            .contains(&ToolName::plain(tool.name.clone()))
            .then(|| spec.clone()),
        ToolSpec::ToolSearch { .. } | ToolSpec::WebSearch { .. } => activated
            .contains(&ToolName::plain(spec.name()))
            .then(|| spec.clone()),
    }
}

fn merge_visible_tool_spec(visible: &mut Vec<ToolSpec>, spec: ToolSpec) {
    let mut namespace = match spec {
        ToolSpec::Namespace(namespace) => namespace,
        spec => {
            if !visible
                .iter()
                .any(|existing| existing.name() == spec.name())
            {
                visible.push(spec);
            }
            return;
        }
    };

    if let Some(ToolSpec::Namespace(existing)) = visible
        .iter_mut()
        .find(|existing| existing.name() == namespace.name.as_str())
    {
        for tool in namespace.tools.drain(..) {
            let codex_tools::ResponsesApiNamespaceTool::Function(candidate) = &tool;
            let duplicate = existing.tools.iter().any(|existing_tool| {
                let codex_tools::ResponsesApiNamespaceTool::Function(existing_tool) = existing_tool;
                existing_tool.name == candidate.name
            });
            if !duplicate {
                existing.tools.push(tool);
            }
        }
    } else {
        visible.push(ToolSpec::Namespace(namespace));
    }
}

/// Apply the shared namespace-description budget to an activated projection,
/// as `spec_plan::merge_into_namespaces` does for the base projection.
fn apply_visible_namespace_description_budget(visible: &mut [ToolSpec]) {
    let mut descriptions = visible
        .iter()
        .filter_map(|spec| match spec {
            ToolSpec::Namespace(namespace) => Some(namespace.description.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    super::spec_plan::apply_fair_description_budget(&mut descriptions);
    let namespaces = visible.iter_mut().filter_map(|spec| match spec {
        ToolSpec::Namespace(namespace) => Some(namespace),
        _ => None,
    });
    for (namespace, description) in namespaces.zip(descriptions) {
        namespace.description = description;
    }
}

fn authorize_independent_review_tool_call(
    session_source: &SessionSource,
    class: TypedToolClass,
    call: &ToolCall,
    external_mutation_intent: ExternalMutationIntent,
) -> Result<(), FunctionCallError> {
    if !is_independent_review_source(session_source) {
        return Ok(());
    }
    let allowed = matches!(
        class,
        TypedToolClass::AgentCommunication
            | TypedToolClass::OwnTask
            | TypedToolClass::ReadSearch
            | TypedToolClass::CodeModeControl
            | TypedToolClass::Shell
    ) || (class == TypedToolClass::DynamicExternal
        && external_mutation_intent == ExternalMutationIntent::ProvenReadOnly);
    if allowed {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "{}: independent review capability denied: no tool was run; only read-only repository inspection tools are available. Do not retry the mutation under the current permissions",
            call.tool_name.name
        )))
    }
}

async fn authorize_bound_typed_tool_call(
    session: &Session,
    step_context: &StepContext,
    class: TypedToolClass,
    call: &ToolCall,
    _external_mutation_intent: ExternalMutationIntent,
) -> Result<(), FunctionCallError> {
    let coordinator = session.services.agent_control.task_coordinator();
    let Some(binding) = coordinator.binding_for_turn(&step_context.turn) else {
        // A deliberately unbound turn cannot acquire authority from a task
        // attached later (or bound to another thread at turn entry).
        if coordinator
            .binding_for_source(&step_context.turn.session_source)
            .is_some()
        {
            return Err(FunctionCallError::RequiredOperationBlocked(format!(
                "{}: this turn has no bound typed assignment attempt",
                call.tool_name.name
            )));
        }
        return Ok(());
    };
    let authorization = coordinator
        .get_agent_task_authorization(binding.assignment_id)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{}: typed assignment state is unavailable: {error}",
                call.tool_name.name
            ))
        })?;
    if authorization.current_attempt.attempt_id != binding.attempt_id
        || authorization.current_attempt.state != AttemptState::Active
    {
        return Err(FunctionCallError::RequiredOperationBlocked(format!(
            "{}: the bound typed assignment attempt is no longer active",
            call.tool_name.name
        )));
    }

    let is_legacy_nested_spawn = class == TypedToolClass::RootTaskControl
        && call.tool_name.name == "spawn_agent"
        && matches!(
            authorization.admission_origin,
            AssignmentAdmissionOrigin::LegacyMessage { .. }
        );
    if !is_legacy_nested_spawn {
        authorize_typed_tool(class).map_err(|error| {
            FunctionCallError::DeniedToModel(format!(
                "{}: typed assignment capability denied: {error}",
                call.tool_name.name
            ))
        })?;
    }
    // An authorized tool call is meaningful progress, not just liveness.
    let heartbeated = coordinator
        .heartbeat_typed_actor_binding(&binding, /*progress*/ true)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "{}: typed assignment heartbeat failed: {error}",
                call.tool_name.name
            ))
        })?;
    if !heartbeated {
        return Err(FunctionCallError::RequiredOperationBlocked(format!(
            "{}: the bound typed assignment attempt is no longer active",
            call.tool_name.name
        )));
    }
    Ok(())
}


pub(crate) fn extension_tool_surface_snapshot(
    session: &Session,
) -> codex_extension_api::ToolSurfaceSnapshot {
    let mut snapshot = codex_extension_api::ToolSurfaceSnapshot {
        revision: 0,
        tools: Vec::new(),
    };
    for contributor in session.services.extensions.tool_contributors() {
        let contribution = contributor.surface_snapshot(
            &session.services.session_extension_data,
            &session.services.thread_extension_data,
        );
        snapshot.revision = snapshot
            .revision
            .rotate_left(7)
            .wrapping_add(contribution.revision);
        snapshot.tools.extend(contribution.tools);
    }
    snapshot
}

pub(crate) fn extension_tool_surface_revision(session: &Session) -> u64 {
    session
        .services
        .extensions
        .tool_contributors()
        .iter()
        .fold(0_u64, |revision, contributor| {
            revision
                .rotate_left(7)
                .wrapping_add(contributor.surface_revision(
                    &session.services.session_extension_data,
                    &session.services.thread_extension_data,
                ))
        })
}


