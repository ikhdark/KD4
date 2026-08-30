use crate::FunctionCallError;
use crate::agent::task_capabilities::ExternalMutationIntent;
use crate::agent::task_capabilities::TypedToolClass;
use crate::agent::task_capabilities::authorize_typed_tool;
use crate::agent::task_capabilities::is_independent_review_source;
use crate::client_common::ToolSchemaArtifact;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolDispatchState;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::registry::AnyToolResult;
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
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
use std::sync::atomic::Ordering;
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

pub struct ToolRouter {
    registry: ToolRegistry,
    planning_warnings: Vec<String>,
    exposure_identity: ToolExposureIdentity,
    schema_cache: Mutex<Option<(HashSet<ToolName>, Arc<ToolSchemaArtifact>)>>,
    manifest_cache: Mutex<ToolManifestCache>,
    deferred_tool_capability_revisions: OnceLock<Arc<HashMap<ToolName, String>>>,
    #[cfg(test)]
    schema_snapshot_build_count: AtomicUsize,
    #[cfg(test)]
    manifest_snapshot_build_count: AtomicUsize,
    #[cfg(test)]
    deferred_tool_capability_revision_build_count: AtomicUsize,
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
        let _ = self.0.try_complete();
    }
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
    #[cfg(test)]
    pub(crate) fn from_context(
        step_context: &StepContext,
        params: ToolRouterParams<'_>,
        tool_search_handler_cache: &ToolSearchHandlerCache,
    ) -> Self {
        Self::try_from_context(step_context, params, tool_search_handler_cache)
            .unwrap_or_else(|error| panic!("failed to build tool router: {error}"))
    }

    pub(crate) fn try_from_context(
        step_context: &StepContext,
        params: ToolRouterParams<'_>,
        tool_search_handler_cache: &ToolSearchHandlerCache,
    ) -> Result<Self, String> {
        build_tool_router(step_context, params, tool_search_handler_cache)
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
        mut registry: ToolRegistry,
        model_visible_specs: Vec<ToolSpec>,
        planning_warnings: Vec<String>,
        exposure_identity: ToolExposureIdentity,
    ) -> Self {
        registry.set_model_visible_specs(model_visible_specs);
        Self {
            registry,
            planning_warnings,
            exposure_identity,
            schema_cache: Mutex::new(None),
            manifest_cache: Mutex::new(ToolManifestCache::default()),
            deferred_tool_capability_revisions: OnceLock::new(),
            #[cfg(test)]
            schema_snapshot_build_count: AtomicUsize::new(0),
            #[cfg(test)]
            manifest_snapshot_build_count: AtomicUsize::new(0),
            #[cfg(test)]
            deferred_tool_capability_revision_build_count: AtomicUsize::new(0),
        }
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
        let (_, activated) = turn.deferred_tool_activation_snapshot();
        self.tool_schema_snapshot(&activated)
    }

    pub(crate) fn deferred_tool_capability_revisions(&self) -> Arc<HashMap<ToolName, String>> {
        Arc::clone(self.deferred_tool_capability_revisions.get_or_init(|| {
            #[cfg(test)]
            self.deferred_tool_capability_revision_build_count
                .fetch_add(1, Ordering::Relaxed);
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
                            "spec_sha256": tool.canonical_spec_sha256(),
                        }))
                        .unwrap_or_default();
                        (name.clone(), format!("{:x}", Sha256::digest(encoded)))
                    })
                    .collect(),
            )
        }))
    }

    #[cfg(test)]
    pub(crate) fn tool_manifest(
        &self,
        turn: &crate::session::turn_context::TurnContext,
    ) -> ToolManifestItem {
        let (_, activated) = turn.deferred_tool_activation_snapshot();
        self.tool_manifest_snapshot(&activated)
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

        let manifest = self.tool_manifest_snapshot(&activated);
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
                if tool.exposure() != crate::tools::registry::ToolExposure::Deferred {
                    continue;
                }
                let Some(spec) = filter_activated_deferred_spec(tool.spec(), activated) else {
                    continue;
                };
                merge_visible_tool_spec(&mut visible, spec);
            }
            Arc::new(ToolSchemaArtifact::new(visible))
        };
        #[cfg(test)]
        self.schema_snapshot_build_count
            .fetch_add(1, Ordering::Relaxed);
        *cache = Some((activated.clone(), Arc::clone(&schemas)));
        schemas
    }

    fn tool_manifest_snapshot(&self, activated: &HashSet<ToolName>) -> ToolManifestItem {
        let mut cache = self
            .manifest_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(manifest) = cache.get(activated) {
            return manifest.clone();
        }

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
        let manifest = canonicalize_json(&serde_json::json!({
            "model_visible": schemas.specs(),
            "registered": registered,
        }));
        let fingerprint_input = serde_json::json!({
            "manifest": &manifest,
            "tool_exposure_identity": &self.exposure_identity,
        });
        let encoded = serde_json::to_vec(&fingerprint_input).unwrap_or_default();
        let manifest = ToolManifestItem::full(format!("{:x}", Sha256::digest(encoded)), manifest);
        #[cfg(test)]
        self.manifest_snapshot_build_count
            .fetch_add(1, Ordering::Relaxed);
        cache.insert(activated.clone(), manifest.clone());
        manifest
    }

    #[cfg(test)]
    pub(crate) fn schema_snapshot_build_count(&self) -> usize {
        self.schema_snapshot_build_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn manifest_snapshot_build_count(&self) -> usize {
        self.manifest_snapshot_build_count.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn deferred_tool_capability_revision_build_count(&self) -> usize {
        self.deferred_tool_capability_revision_build_count
            .load(Ordering::Relaxed)
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

    #[cfg(test)]
    pub(crate) fn tool_authorization_class_for_test(
        &self,
        name: &ToolName,
    ) -> Option<TypedToolClass> {
        self.registry.tool_authorization_class(name)
    }

    #[cfg(test)]
    pub(crate) fn tool_external_mutation_intent_for_test(
        &self,
        name: &ToolName,
    ) -> Option<ExternalMutationIntent> {
        self.registry.tool_external_mutation_intent(name)
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

    pub fn tool_owns_unified_exec_processes(&self, call: &ToolCall) -> bool {
        self.registry
            .owns_unified_exec_processes(&call.tool_name)
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
        if self.registry.tool_exposure(&call.tool_name)
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
        let external_mutation_intent = self
            .registry
            .tool_external_mutation_intent(&call.tool_name)
            .unwrap_or(ExternalMutationIntent::MayMutate);
        let tool_class = self.classify_tool_name(step_context.turn.as_ref(), &call.tool_name);
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
        Err(FunctionCallError::DeniedToModel(format!(
            "{}: independent review capability denied: only read-only repository inspection tools are available",
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
    let Some(binding) = coordinator.binding_for_source(&step_context.turn.session_source) else {
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
        return Err(FunctionCallError::DeniedToModel(format!(
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
        return Err(FunctionCallError::DeniedToModel(format!(
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

#[cfg(test)]
#[path = "router_tests.rs"]
mod tests;
