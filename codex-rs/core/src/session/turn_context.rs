use super::*;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::git_workspace::GitWorkspaceMetadataSource;
use crate::shell_snapshot::ShellSnapshotFile;
use codex_core_skills::HostSkillsSnapshot;
use codex_file_system::FileSystemSandboxContext;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_sandboxing::compatibility_sandbox_policy_for_permission_profile;
use codex_sandboxing::policy_transforms::effective_file_system_sandbox_policy;
use codex_sandboxing::policy_transforms::effective_network_sandbox_policy;
use codex_utils_path_uri::PathUri;
use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

#[derive(Clone, Debug)]
pub(crate) struct TurnSkillsContext {
    pub(crate) snapshot: HostSkillsSnapshot,
    pub(crate) implicit_invocation_seen_skills: Arc<Mutex<HashSet<String>>>,
}

impl TurnSkillsContext {
    pub(crate) fn new(snapshot: HostSkillsSnapshot) -> Self {
        Self {
            snapshot,
            implicit_invocation_seen_skills: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

pub(crate) type ShellSnapshotTask = Shared<BoxFuture<'static, Option<Arc<ShellSnapshotFile>>>>;

pub(crate) struct TurnEnvironmentLifecycle {
    cancellation: CancellationToken,
}

impl TurnEnvironmentLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for TurnEnvironmentLifecycle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeferredToolActivation {
    pub(crate) root_task_epoch: String,
    pub(crate) provenance: String,
    pub(crate) capability_revision: String,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DeferredToolActivationState {
    revision: u64,
    capability_revisions: Arc<HashMap<codex_tools::ToolName, String>>,
    activations: HashMap<codex_tools::ToolName, DeferredToolActivation>,
}

#[derive(Clone)]
pub(crate) struct TurnEnvironment {
    pub(crate) environment_id: String,
    pub(crate) environment: Arc<Environment>,
    cwd: PathUri,
    pub(crate) shell: Option<shell::Shell>,
    pub(crate) shell_snapshot: ShellSnapshotTask,
    lifecycle: Option<Arc<TurnEnvironmentLifecycle>>,
}

impl TurnEnvironment {
    pub(crate) fn new(
        environment_id: String,
        environment: Arc<Environment>,
        cwd: PathUri,
        shell: Option<shell::Shell>,
    ) -> Self {
        Self {
            environment_id,
            environment,
            cwd,
            shell,
            shell_snapshot: futures::future::ready(None).boxed().shared(),
            lifecycle: None,
        }
    }

    pub(crate) fn attach_lifecycle(&mut self, lifecycle: Arc<TurnEnvironmentLifecycle>) {
        self.lifecycle = Some(lifecycle);
    }

    pub(crate) fn lifecycle(&self) -> Option<Arc<TurnEnvironmentLifecycle>> {
        self.lifecycle.clone()
    }

    pub(crate) fn detach_lifecycle(&mut self) {
        self.lifecycle = None;
    }

    pub(crate) async fn shell_snapshot(&self, cwd: &PathUri) -> Option<Arc<ShellSnapshotFile>> {
        if self.cwd != *cwd {
            return None;
        }
        self.shell_snapshot.clone().await
    }

    pub(crate) fn cwd(&self) -> &PathUri {
        &self.cwd
    }

    pub(crate) fn selection(&self) -> TurnEnvironmentSelection {
        TurnEnvironmentSelection {
            environment_id: self.environment_id.clone(),
            cwd: self.cwd.clone(),
        }
    }
}

impl std::fmt::Debug for TurnEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnEnvironment")
            .field("environment_id", &self.environment_id)
            .field("environment", &self.environment)
            .field("cwd", &self.cwd)
            .field("shell", &self.shell)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct PendingPostToolContexts {
    by_call_id: Mutex<HashMap<String, Vec<ResponseItem>>>,
}

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) trace_id: Option<String>,
    pub config: Arc<Config>,
    /// Deduplicated workspace roots are immutable for the lifetime of a turn.
    /// Keeping one shared projection avoids rebuilding and cloning the list for
    /// every tool attempt and context projection.
    pub(crate) effective_workspace_roots: Arc<[AbsolutePathBuf]>,
    /// Fully resolved instructions for this turn. Keeping the rendered text on
    /// the turn avoids rebuilding it during token-estimate and retry paths.
    pub(crate) base_instructions: Arc<BaseInstructions>,
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    pub(crate) model_info: ModelInfo,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) provider: SharedModelProvider,
    /// The raw per-turn setting before model capability normalization. Policy
    /// telemetry reports this independently from the effective request effort.
    pub(crate) configured_reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
    pub(crate) reasoning_summary: ReasoningSummaryConfig,
    pub(crate) session_source: SessionSource,
    pub(crate) history_mode: ThreadHistoryMode,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) originator: String,
    pub(crate) environments: TurnEnvironmentSnapshot,
    pub(crate) current_date: Option<String>,
    pub(crate) timezone: Option<String>,
    pub(crate) app_server_client_name: Option<String>,
    pub(crate) developer_instructions: Option<String>,
    pub(crate) collaboration_mode: CollaborationMode,
    pub(crate) multi_agent_version: MultiAgentVersion,
    pub(crate) multi_agent_spawn_authorized: AtomicBool,
    pub(crate) personality: Option<Personality>,
    pub(crate) approval_policy: Constrained<AskForApproval>,
    pub(crate) permission_profile: PermissionProfile,
    pub(crate) network: Option<NetworkProxy>,
    pub(crate) windows_sandbox_level: WindowsSandboxLevel,
    pub(crate) available_models: Arc<Vec<ModelPreset>>,
    pub(crate) final_output_json_schema: Option<Value>,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    /// Deferred tools selected during this root task, bound to exact capability revisions.
    pub(crate) deferred_tool_activations: Arc<std::sync::RwLock<DeferredToolActivationState>>,
    pub(crate) validation_authorization: crate::validation_admission::SharedValidationAuthorization,
    pub(crate) turn_metadata_state: Arc<TurnMetadataState>,
    pub(crate) extension_data: Arc<codex_extension_api::ExtensionData>,
    pub(crate) turn_skills: TurnSkillsContext,
    pub(crate) turn_timing_state: Arc<TurnTimingState>,
    pub(crate) tool_call_acceptance: Arc<crate::state::ToolCallAcceptanceGate>,
    /// Remembers completed ordered-history commit retry keys for this turn only.
    pub(crate) durable_history_completed_commits: Arc<Mutex<HashSet<String>>>,
    pub(crate) terminal_error: Arc<Mutex<Option<ErrorEvent>>>,
    pub(crate) server_model_warning_emitted: AtomicBool,
    pub(crate) model_verification_emitted: AtomicBool,
    /// Ensures external-context producers signal memory pollution at most once
    /// during this turn, even when the same result has multiple projections.
    pub(crate) memory_pollution_signal_claimed: AtomicBool,
}

enum TurnMultiAgentRuntime {
    ResolveAndStore,
    Preview,
}

/// Only fields below are rewritten by [`Session::build_per_turn_config`].
/// Comparing this projection is sufficient to decide whether its clone still
/// represents the base config, without walking unrelated config maps and layer
/// data. The layer-stack identity guards the same projection when it is used to
/// reuse the prior turn's effective config after a config reload.
fn same_turn_config_projection(left: &Config, right: &Config) -> bool {
    Arc::ptr_eq(&left.config_layer_stack, &right.config_layer_stack)
        && left.cwd == right.cwd
        && left.model == right.model
        && left.workspace_roots == right.workspace_roots
        && left.permissions == right.permissions
        && left.model_reasoning_effort == right.model_reasoning_effort
        && left.model_reasoning_summary == right.model_reasoning_summary
        && left.service_tier == right.service_tier
        && left.personality == right.personality
        && left.approvals_reviewer == right.approvals_reviewer
        && left.web_search_mode == right.web_search_mode
        && left.features == right.features
}

impl TurnContext {
    pub(crate) async fn queue_post_tool_contexts(
        &self,
        call_id: &str,
        contexts: Vec<ResponseItem>,
    ) {
        if contexts.is_empty() {
            return;
        }
        let pending = self
            .extension_data
            .get_or_init(PendingPostToolContexts::default);
        pending
            .by_call_id
            .lock()
            .await
            .entry(call_id.to_string())
            .or_default()
            .extend(contexts);
    }

    pub(crate) async fn take_post_tool_contexts(&self, call_id: &str) -> Vec<ResponseItem> {
        let Some(pending) = self.extension_data.get::<PendingPostToolContexts>() else {
            return Vec::new();
        };
        pending
            .by_call_id
            .lock()
            .await
            .remove(call_id)
            .unwrap_or_default()
    }

    pub(crate) fn claim_memory_pollution_signal(&self) -> bool {
        self.memory_pollution_signal_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns the host-native cwd resolved for this turn.
    ///
    /// Per-turn config construction derives this value from the primary selected
    /// environment when its URI uses the host path convention, and otherwise
    /// preserves the session's explicit legacy fallback.
    pub(crate) fn cwd(&self) -> &AbsolutePathBuf {
        &self.config.cwd
    }

    pub(crate) fn effective_workspace_roots(&self) -> &[AbsolutePathBuf] {
        &self.effective_workspace_roots
    }

    /// Returns the selected environment cwd without projecting foreign paths
    /// onto the host. Falls back to the resolved host cwd only when no ready
    /// environment is selected.
    pub(crate) fn cwd_uri(&self) -> PathUri {
        self.environments
            .primary()
            .map(|environment| environment.cwd().clone())
            .unwrap_or_else(|| PathUri::from_abs_path(self.cwd()))
    }

    pub(crate) async fn update_validation_authorization(
        &self,
        input: &[codex_protocol::user_input::UserInput],
    ) {
        let mut authorization = self.validation_authorization.write().await;
        for item in input {
            if let codex_protocol::user_input::UserInput::Text { text, .. } = item {
                authorization.update_from_user_input(text);
            }
        }
    }

    pub(crate) fn update_multi_agent_spawn_authorization(
        &self,
        input: &[codex_protocol::user_input::UserInput],
    ) {
        for item in input {
            if let codex_protocol::user_input::UserInput::Text { text, .. } = item {
                crate::session::multi_agents::update_spawn_authorization_from_text(self, text);
            }
        }
    }
    pub(crate) fn activate_deferred_tools(
        &self,
        tools: impl IntoIterator<Item = codex_tools::ToolName>,
    ) {
        let mut state = self
            .deferred_tool_activations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        for tool_name in tools {
            let Some(capability_revision) = state.capability_revisions.get(&tool_name).cloned()
            else {
                continue;
            };
            let activation = DeferredToolActivation {
                root_task_epoch: self.sub_id.clone(),
                provenance: tool_name.to_string(),
                capability_revision,
            };
            changed |= state.activations.get(&tool_name) != Some(&activation);
            state.activations.insert(tool_name, activation);
        }
        if changed {
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub(crate) fn deferred_tool_is_activated(&self, tool_name: &codex_tools::ToolName) -> bool {
        let state = self
            .deferred_tool_activations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(activation) = state.activations.get(tool_name) else {
            return false;
        };
        activation.root_task_epoch == self.sub_id
            && activation.provenance == tool_name.to_string()
            && state.capability_revisions.get(tool_name) == Some(&activation.capability_revision)
    }

    pub(crate) fn deferred_tool_activation_revision(&self) -> u64 {
        self.deferred_tool_activations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision
    }

    pub(crate) fn deferred_tool_activation_snapshot(
        &self,
    ) -> (u64, HashSet<codex_tools::ToolName>) {
        let state = self
            .deferred_tool_activations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let activated = state
            .activations
            .iter()
            .filter(|(tool_name, activation)| {
                activation.root_task_epoch == self.sub_id
                    && activation.provenance == tool_name.to_string()
                    && state.capability_revisions.get(*tool_name)
                        == Some(&activation.capability_revision)
            })
            .map(|(tool_name, _)| tool_name)
            .cloned()
            .collect();
        (state.revision, activated)
    }

    pub(crate) fn activated_deferred_tools(&self) -> HashSet<codex_tools::ToolName> {
        self.deferred_tool_activation_snapshot().1
    }

    /// Settles a sampling request that advertised deferred capabilities.
    ///
    /// Activations deliberately remain live for the rest of this turn. A model may need an
    /// intervening reasoning or tool step before it can call a discovered tool, so request-scoped
    /// release can erase the only callable schema before use. Capability refresh still removes
    /// stale revisions, and dropping this turn context removes all remaining activations.
    pub(crate) fn release_advertised_deferred_tools(
        &self,
        _advertised: &HashSet<codex_tools::ToolName>,
    ) {
    }

    pub(crate) fn refresh_deferred_tool_capabilities(
        &self,
        capability_revisions: Arc<HashMap<codex_tools::ToolName, String>>,
    ) {
        let mut state = self
            .deferred_tool_activations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let capability_changed =
            state.capability_revisions.as_ref() != capability_revisions.as_ref();
        state.capability_revisions = capability_revisions;
        let current_epoch = self.sub_id.as_str();
        let current_revisions = Arc::clone(&state.capability_revisions);
        let before = state.activations.len();
        state.activations.retain(|tool_name, activation| {
            activation.root_task_epoch == current_epoch
                && activation.provenance == tool_name.to_string()
                && current_revisions.get(tool_name) == Some(&activation.capability_revision)
        });
        if capability_changed || state.activations.len() != before {
            state.revision = state.revision.saturating_add(1);
        }
    }

    pub(crate) fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile.clone()
    }

    pub(crate) fn file_system_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        self.permission_profile.file_system_sandbox_policy()
    }

    pub(crate) fn network_sandbox_policy(&self) -> NetworkSandboxPolicy {
        self.permission_profile.network_sandbox_policy()
    }

    pub(crate) fn sandbox_policy(&self) -> SandboxPolicy {
        compatibility_sandbox_policy_for_permission_profile(&self.permission_profile, self.cwd())
    }

    pub(crate) fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if self.model_info.supports_reasoning_summaries {
            self.reasoning_effort
                .clone()
                .or_else(|| self.model_info.default_reasoning_level.clone())
        } else {
            None
        }
    }

    pub(crate) fn effective_reasoning_effort_for_tracing(&self) -> String {
        self.effective_reasoning_effort()
            .map(|effort| effort.to_string())
            .unwrap_or_else(|| "default".to_string())
    }

    pub(crate) fn model_context_window(&self) -> Option<i64> {
        let effective_context_window_percent = self.model_info.effective_context_window_percent;
        self.model_info
            .resolved_context_window()
            .map(|context_window| {
                context_window.saturating_mul(effective_context_window_percent) / 100
            })
    }

    pub(crate) fn apps_enabled(&self) -> bool {
        let uses_codex_backend = self
            .auth_manager
            .as_deref()
            .is_some_and(AuthManager::current_auth_uses_codex_backend);
        self.config
            .features
            .apps_enabled_for_auth(uses_codex_backend)
            && self.config.orchestrator_mcp_enabled
    }

    pub(crate) async fn with_model(
        &self,
        model: String,
        models_manager: &SharedModelsManager,
    ) -> Self {
        let mut config = (*self.config).clone();
        config.model = Some(model.clone());
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let supported_reasoning_levels = model_info
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort.clone())
            .collect::<Vec<_>>();
        let reasoning_effort = if let Some(current_reasoning_effort) = self.reasoning_effort.clone()
        {
            if supported_reasoning_levels.contains(&current_reasoning_effort) {
                Some(current_reasoning_effort)
            } else {
                supported_reasoning_levels
                    .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                    .cloned()
                    .or_else(|| model_info.default_reasoning_level.clone())
            }
        } else {
            supported_reasoning_levels
                .get(supported_reasoning_levels.len().saturating_sub(1) / 2)
                .cloned()
                .or_else(|| model_info.default_reasoning_level.clone())
        };
        config.model_reasoning_effort = reasoning_effort.clone();

        let collaboration_mode = self.collaboration_mode.with_updates(
            Some(model.clone()),
            Some(reasoning_effort.clone()),
            /*developer_instructions*/ None,
        );
        let available_models = models_manager
            .list_models_shared(
                RefreshStrategy::OnlineIfUncached,
                config.http_client_factory(),
            )
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "model refresh failed; retaining the current turn catalog");
                Arc::clone(&self.available_models)
            });
        let effective_workspace_roots = config.effective_workspace_roots().into();

        Self {
            sub_id: self.sub_id.clone(),
            trace_id: self.trace_id.clone(),
            config: Arc::new(config),
            effective_workspace_roots,
            base_instructions: Arc::new(BaseInstructions {
                text: model_info.base_instructions.clone(),
            }),
            auth_manager: self.auth_manager.clone(),
            model_info: model_info.clone(),
            session_telemetry: self
                .session_telemetry
                .clone()
                .with_model(model.as_str(), model_info.slug.as_str()),
            provider: self.provider.clone(),
            configured_reasoning_effort: self.configured_reasoning_effort.clone(),
            reasoning_effort,
            reasoning_summary: self.reasoning_summary,
            session_source: self.session_source.clone(),
            history_mode: self.history_mode,
            parent_thread_id: self.parent_thread_id,
            originator: self.originator.clone(),
            environments: self.environments.clone(),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            app_server_client_name: self.app_server_client_name.clone(),
            developer_instructions: self.developer_instructions.clone(),
            collaboration_mode,
            multi_agent_version: self.multi_agent_version,
            multi_agent_spawn_authorized: AtomicBool::new(
                self.multi_agent_spawn_authorized.load(Ordering::Relaxed),
            ),
            personality: self.personality,
            approval_policy: self.approval_policy.clone(),
            permission_profile: self.permission_profile.clone(),
            network: self.network.clone(),
            windows_sandbox_level: self.windows_sandbox_level,
            available_models,
            final_output_json_schema: self.final_output_json_schema.clone(),
            dynamic_tools: self.dynamic_tools.clone(),
            deferred_tool_activations: Arc::clone(&self.deferred_tool_activations),
            validation_authorization: Arc::clone(&self.validation_authorization),
            turn_metadata_state: self.turn_metadata_state.clone(),
            extension_data: Arc::clone(&self.extension_data),
            turn_skills: self.turn_skills.clone(),
            turn_timing_state: Arc::clone(&self.turn_timing_state),
            tool_call_acceptance: Arc::clone(&self.tool_call_acceptance),
            durable_history_completed_commits: Arc::clone(&self.durable_history_completed_commits),
            terminal_error: Arc::clone(&self.terminal_error),
            server_model_warning_emitted: AtomicBool::new(
                self.server_model_warning_emitted.load(Ordering::Relaxed),
            ),
            model_verification_emitted: AtomicBool::new(
                self.model_verification_emitted.load(Ordering::Relaxed),
            ),
            memory_pollution_signal_claimed: AtomicBool::new(
                self.memory_pollution_signal_claimed.load(Ordering::Relaxed),
            ),
        }
    }

    pub(crate) fn file_system_sandbox_context(
        &self,
        additional_permissions: Option<AdditionalPermissionProfile>,
        cwd: &PathUri,
    ) -> FileSystemSandboxContext {
        let (base_file_system_sandbox_policy, base_network_sandbox_policy) =
            self.permission_profile.to_runtime_permissions();
        let file_system_sandbox_policy = effective_file_system_sandbox_policy(
            &base_file_system_sandbox_policy,
            additional_permissions.as_ref(),
        );
        let network_sandbox_policy = effective_network_sandbox_policy(
            base_network_sandbox_policy,
            additional_permissions.as_ref(),
        );
        let permissions = PermissionProfile::from_runtime_permissions_with_enforcement(
            self.permission_profile.enforcement(),
            &file_system_sandbox_policy,
            network_sandbox_policy,
        );
        FileSystemSandboxContext {
            permissions: permissions.into(),
            cwd: Some(cwd.clone()),
            workspace_roots: self
                .effective_workspace_roots()
                .iter()
                .map(PathUri::from_abs_path)
                .collect(),
            windows_sandbox_level: self.windows_sandbox_level,
            windows_sandbox_private_desktop: self
                .config
                .permissions
                .windows_sandbox_private_desktop,
        }
    }

    fn non_legacy_file_system_sandbox_policy(&self) -> Option<FileSystemSandboxPolicy> {
        // Omit the derived split filesystem policy when it is equivalent to
        // the legacy sandbox policy. This keeps turn-context payloads stable
        // while both fields exist; once callers consume only the split policy,
        // this comparison and the legacy projection should go away.
        let legacy_file_system_sandbox_policy =
            FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                &self.sandbox_policy(),
                self.cwd(),
            );
        let file_system_sandbox_policy = self.file_system_sandbox_policy();
        (file_system_sandbox_policy != legacy_file_system_sandbox_policy)
            .then_some(file_system_sandbox_policy)
    }

    pub(crate) fn to_turn_context_item(&self) -> TurnContextItem {
        let workspace_roots = self.effective_workspace_roots();
        let cwd = self.cwd().clone();
        TurnContextItem {
            turn_id: Some(self.sub_id.clone()),
            cwd,
            workspace_roots: (!workspace_roots.is_empty()).then(|| workspace_roots.to_vec()),
            current_date: self.current_date.clone(),
            timezone: self.timezone.clone(),
            approval_policy: self.approval_policy.value(),
            approvals_reviewer: Some(self.config.approvals_reviewer),
            sandbox_policy: self.sandbox_policy(),
            permission_profile: Some(self.permission_profile()),
            network: self.turn_context_network_item(),
            file_system_sandbox_policy: self.non_legacy_file_system_sandbox_policy(),
            model: self.model_info.slug.clone(),
            comp_hash: self.model_info.comp_hash.clone(),
            personality: self.personality,
            collaboration_mode: Some(self.collaboration_mode.clone()),
            multi_agent_version: Some(self.multi_agent_version),
            multi_agent_mode: super::multi_agents::effective_multi_agent_mode(self)
                .map(|mode| mode.to_persisted_mode()),
            effort: self.reasoning_effort.clone(),
            context_provenance: None,
        }
    }

    fn turn_context_network_item(&self) -> Option<TurnContextNetworkItem> {
        let network = self
            .config
            .config_layer_stack
            .requirements()
            .network
            .as_ref()?;
        Some(TurnContextNetworkItem {
            allowed_domains: network
                .domains
                .as_ref()
                .and_then(codex_config::NetworkDomainPermissionsToml::allowed_domains)
                .unwrap_or_default(),
            denied_domains: network
                .domains
                .as_ref()
                .and_then(codex_config::NetworkDomainPermissionsToml::denied_domains)
                .unwrap_or_default(),
        })
    }
}

fn local_time_context() -> (String, String) {
    match iana_time_zone::get_timezone() {
        Ok(timezone) => (Local::now().format("%Y-%m-%d").to_string(), timezone),
        Err(_) => (
            Utc::now().format("%Y-%m-%d").to_string(),
            "Etc/UTC".to_string(),
        ),
    }
}

impl Session {
    /// Don't expand the number of mutated arguments on config. We are in the process of getting rid of it.
    pub(crate) fn build_per_turn_config(
        session_configuration: &SessionConfiguration,
        cwd: AbsolutePathBuf,
    ) -> Arc<Config> {
        // todo(aibrahim): store this state somewhere else so we don't need to mut config
        let config = session_configuration.original_config_do_not_use.clone();
        let mut per_turn_config = (*config).clone();
        per_turn_config.cwd = cwd;
        per_turn_config.model = Some(session_configuration.collaboration_mode.model().to_string());
        per_turn_config.workspace_roots = session_configuration.workspace_roots.clone();
        per_turn_config
            .permissions
            .set_workspace_roots(session_configuration.workspace_roots.clone());
        per_turn_config.model_reasoning_effort =
            session_configuration.collaboration_mode.reasoning_effort();
        per_turn_config.model_reasoning_summary = session_configuration.model_reasoning_summary;
        per_turn_config.service_tier = session_configuration.service_tier.clone();
        per_turn_config.personality = session_configuration.personality;
        per_turn_config.approvals_reviewer = session_configuration.approvals_reviewer;
        session_configuration
            .apply_permission_profile_to_permissions(&mut per_turn_config.permissions);
        let permission_profile = session_configuration.permission_profile();
        let resolved_web_search_mode =
            resolve_web_search_mode_for_turn(&per_turn_config.web_search_mode, &permission_profile);
        if let Err(err) = per_turn_config
            .web_search_mode
            .set(resolved_web_search_mode)
        {
            let fallback_value = per_turn_config.web_search_mode.value();
            tracing::warn!(
                error = %err,
                ?resolved_web_search_mode,
                ?fallback_value,
                "resolved web_search_mode is disallowed by requirements; keeping constrained value"
            );
        }
        per_turn_config.features = config.features.clone();
        if same_turn_config_projection(&per_turn_config, &config) {
            config
        } else {
            Arc::new(per_turn_config)
        }
    }

    pub(crate) fn build_effective_session_config(
        session_configuration: &SessionConfiguration,
    ) -> Config {
        let mut config = (*Self::build_per_turn_config(
            session_configuration,
            session_configuration.cwd().clone(),
        ))
        .clone();
        config.permissions.approval_policy = session_configuration.approval_policy.clone();
        config.workspace_roots = session_configuration.workspace_roots.clone();
        config
            .permissions
            .set_workspace_roots(session_configuration.workspace_roots.clone());
        config
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn make_turn_context(
        thread_id: ThreadId,
        session_id: SessionId,
        auth_manager: Option<Arc<AuthManager>>,
        session_telemetry: &SessionTelemetry,
        provider: ModelProviderInfo,
        session_configuration: &SessionConfiguration,
        multi_agent_version: MultiAgentVersion,
        per_turn_config: Arc<Config>,
        model_info: ModelInfo,
        available_models: Arc<Vec<ModelPreset>>,
        network: Option<NetworkProxy>,
        environments: TurnEnvironmentSnapshot,
        git_metadata_source: Option<GitWorkspaceMetadataSource>,
        sub_id: String,
        skills_snapshot: HostSkillsSnapshot,
    ) -> TurnContext {
        let configured_reasoning_effort =
            session_configuration.collaboration_mode.reasoning_effort();
        let reasoning_effort = configured_reasoning_effort.clone();
        let reasoning_summary = session_configuration
            .model_reasoning_summary
            .unwrap_or(model_info.default_reasoning_summary);
        let session_telemetry = session_telemetry.clone().with_model(
            session_configuration.collaboration_mode.model(),
            model_info.slug.as_str(),
        );
        let session_source = session_configuration.session_source.clone();
        let auth_manager_for_context = auth_manager.clone();
        let provider_for_context = create_model_provider(provider, auth_manager);
        let session_telemetry_for_context = session_telemetry;
        let resolved_service_tier = get_service_tier(
            per_turn_config.service_tier.clone(),
            per_turn_config.features.enabled(Feature::FastMode),
            &model_info,
        );
        let mut per_turn_config = per_turn_config;
        if per_turn_config.service_tier != resolved_service_tier {
            Arc::make_mut(&mut per_turn_config).service_tier = resolved_service_tier;
        }
        let turn_metadata_state = Arc::new(TurnMetadataState::new_with_git_metadata_source(
            session_id.to_string(),
            thread_id.to_string(),
            session_configuration.forked_from_thread_id,
            session_configuration.parent_thread_id,
            &session_configuration.session_source,
            session_configuration.thread_source.clone(),
            sub_id.clone(),
            &session_configuration.permission_profile(),
            session_configuration.windows_sandbox_level,
            network.is_some(),
            git_metadata_source,
        ));
        let (current_date, timezone) = local_time_context();
        let extension_data = Arc::new(codex_extension_api::ExtensionData::new(sub_id.clone()));
        extension_data.insert(skills_snapshot.clone());
        let base_instructions = Arc::new(BaseInstructions {
            text: model_info.base_instructions.clone(),
        });
        let effective_workspace_roots = per_turn_config.effective_workspace_roots().into();
        TurnContext {
            sub_id,
            trace_id: current_span_trace_id(),
            config: per_turn_config,
            effective_workspace_roots,
            base_instructions,
            auth_manager: auth_manager_for_context,
            model_info,
            session_telemetry: session_telemetry_for_context,
            provider: provider_for_context,
            configured_reasoning_effort,
            reasoning_effort,
            reasoning_summary,
            session_source,
            history_mode: session_configuration.history_mode,
            parent_thread_id: session_configuration.parent_thread_id,
            originator: session_configuration.originator.clone(),
            environments,
            current_date: Some(current_date),
            timezone: Some(timezone),
            app_server_client_name: session_configuration.app_server_client_name.clone(),
            developer_instructions: session_configuration.developer_instructions.clone(),
            collaboration_mode: session_configuration.collaboration_mode.clone(),
            multi_agent_version,
            multi_agent_spawn_authorized: AtomicBool::new(false),
            personality: session_configuration.personality,
            approval_policy: session_configuration.approval_policy.clone(),
            permission_profile: session_configuration.permission_profile(),
            network,
            windows_sandbox_level: session_configuration.windows_sandbox_level,
            available_models,
            final_output_json_schema: None,
            dynamic_tools: session_configuration.dynamic_tools.clone(),
            deferred_tool_activations: Arc::new(std::sync::RwLock::new(
                DeferredToolActivationState::default(),
            )),
            validation_authorization: Arc::new(tokio::sync::RwLock::new(
                crate::validation_admission::ValidationAuthorization::default(),
            )),
            turn_metadata_state,
            extension_data,
            turn_skills: TurnSkillsContext::new(skills_snapshot),
            turn_timing_state: Arc::new(TurnTimingState::default()),
            tool_call_acceptance: Arc::new(crate::state::ToolCallAcceptanceGate::default()),
            durable_history_completed_commits: Arc::new(Mutex::new(HashSet::new())),
            terminal_error: Arc::new(Mutex::new(None)),
            server_model_warning_emitted: AtomicBool::new(false),
            model_verification_emitted: AtomicBool::new(false),
            memory_pollution_signal_claimed: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) async fn new_turn_with_sub_id(
        &self,
        sub_id: String,
        updates: SessionSettingsUpdate,
    ) -> CodexResult<Arc<TurnContext>> {
        let session_configuration = self.apply_turn_settings(&sub_id, &updates).await?;

        Ok(self
            .new_turn_from_configuration(
                sub_id,
                session_configuration,
                /*final_output_json_schema*/ None,
            )
            .await)
    }

    pub(crate) async fn apply_turn_settings(
        &self,
        sub_id: &str,
        updates: &SessionSettingsUpdate,
    ) -> CodexResult<SessionConfiguration> {
        if updates.is_empty() {
            return Ok(self.state.lock().await.session_configuration.clone());
        }
        let Ok(_refresh_guard) = self.managed_network_proxy_refresh_lock.acquire().await else {
            unreachable!("managed network proxy refresh semaphore is never closed");
        };
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let update_result: CodexResult<_> = {
            let mut state = self.state.lock().await;
            match state.session_configuration.clone().apply(updates) {
                Ok(next) => {
                    let previous_session_configuration = state.session_configuration.clone();
                    let previous_permission_profile =
                        state.session_configuration.permission_profile();
                    let next_permission_profile = next.permission_profile();
                    let permission_profile_changed =
                        previous_permission_profile != next_permission_profile;
                    let previous_config = notify_config_contributors.then(|| {
                        Self::build_effective_session_config(&state.session_configuration)
                    });
                    let new_config = notify_config_contributors
                        .then(|| Self::build_effective_session_config(&next));
                    state.session_configuration = next.clone();
                    Ok((
                        next,
                        permission_profile_changed,
                        previous_config,
                        new_config,
                        previous_session_configuration,
                        updates.environments.is_some(),
                    ))
                }
                Err(err) => Err(CodexErr::InvalidRequest(err.to_string())),
            }
        };

        let (
            session_configuration,
            permission_profile_changed,
            previous_config,
            new_config,
            previous_session_configuration,
            environments_changed,
        ) = match update_result {
            Ok(update) => update,
            Err(err) => {
                let message = err.to_string();
                self.send_event_raw(Event {
                    id: sub_id.to_string(),
                    msg: EventMsg::Error(ErrorEvent {
                        message: message.clone(),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                })
                .await;
                return Err(CodexErr::InvalidRequest(message));
            }
        };

        if permission_profile_changed
            && let Err(error) = self
                .refresh_managed_network_proxy_for_current_permission_profile()
                .await
        {
            self.state.lock().await.session_configuration = previous_session_configuration;
            let message = format!("managed network policy update failed: {error}");
            self.send_event_raw(Event {
                id: sub_id.to_string(),
                msg: EventMsg::Error(ErrorEvent {
                    message: message.clone(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
            })
            .await;
            return Err(CodexErr::InvalidRequest(message));
        }
        if environments_changed {
            let mut state_owner = self.state.lock().await;
            self.services
                .turn_environments
                .update_selections(session_configuration.environment_selections());
            self.services.advance_planning_generation(&mut state_owner);
        }
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());

        Ok(session_configuration)
    }

    pub(crate) async fn new_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
    ) -> Arc<TurnContext> {
        self.new_turn_context_from_configuration(
            sub_id,
            session_configuration,
            final_output_json_schema,
            TurnMultiAgentRuntime::ResolveAndStore,
        )
        .await
    }

    async fn new_startup_prewarm_turn_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
    ) -> Arc<TurnContext> {
        self.new_turn_context_from_configuration(
            sub_id,
            session_configuration,
            /*final_output_json_schema*/ None,
            TurnMultiAgentRuntime::Preview,
        )
        .await
    }

    #[instrument(name = "turn_context.build", level = "trace", skip_all)]
    async fn new_turn_context_from_configuration(
        &self,
        sub_id: String,
        session_configuration: SessionConfiguration,
        final_output_json_schema: Option<Option<Value>>,
        multi_agent_runtime: TurnMultiAgentRuntime,
    ) -> Arc<TurnContext> {
        let turn_environments = self.services.turn_environments.snapshot().await;
        let git_metadata_source = self
            .services
            .git_workspace
            .snapshot(&turn_environments)
            .await
            .primary_local_metadata_source();
        let primary_turn_environment = turn_environments.primary().cloned();
        // TODO(anp): Migrate per-turn config and legacy TurnContext cwd consumers to PathUri so
        // a foreign primary environment does not fall back to the session's host cwd.
        let cwd = primary_turn_environment
            .as_ref()
            .and_then(|turn_environment| turn_environment.cwd().to_abs_path().ok())
            .unwrap_or_else(|| session_configuration.cwd().clone());
        let per_turn_config = Self::build_per_turn_config(&session_configuration, cwd.clone());
        {
            let mcp_runtime = self.services.latest_mcp_runtime();
            let mcp_connection_manager = mcp_runtime.manager();
            mcp_connection_manager.set_approval_policy(&session_configuration.approval_policy);
            mcp_connection_manager
                .set_permission_profile(session_configuration.permission_profile());
        }

        let model_info = self
            .services
            .models_manager
            .get_model_info(
                session_configuration.collaboration_mode.model(),
                &per_turn_config.to_models_manager_config(),
            )
            .await;
        self.services
            .thread_extension_data
            .insert(model_info.clone());

        let multi_agent_version = match multi_agent_runtime {
            TurnMultiAgentRuntime::ResolveAndStore => {
                self.resolve_multi_agent_version_for_model(&model_info, &per_turn_config)
            }
            TurnMultiAgentRuntime::Preview => self
                .multi_agent_version()
                .or(model_info.multi_agent_version)
                .unwrap_or_else(|| per_turn_config.multi_agent_version_from_features()),
        };
        let plugins_input = per_turn_config.plugins_config_input();
        let plugin_outcome = self
            .services
            .plugins_manager
            .plugins_for_config(&plugins_input)
            .await;
        let effective_skill_roots = plugin_outcome.effective_plugin_skill_roots();
        let plugin_skill_snapshots = self
            .services
            .plugins_manager
            .plugin_skill_snapshots_for_config(&plugins_input);
        let skills_input = skills_load_input_from_config(&per_turn_config, effective_skill_roots)
            .with_plugin_skill_snapshots(plugin_skill_snapshots);
        let fs = primary_turn_environment
            .map(|turn_environment| turn_environment.environment.get_filesystem());
        let skills_snapshot = self
            .services
            .skills_service
            .snapshot_for_config(&skills_input, fs)
            .await;
        let available_models = self
            .services
            .models_manager
            .list_models_shared(
                RefreshStrategy::Offline,
                per_turn_config.http_client_factory(),
            )
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to load the current turn model catalog");
                Arc::new(Vec::new())
            });
        let resolved_service_tier = get_service_tier(
            per_turn_config.service_tier.clone(),
            per_turn_config.features.enabled(Feature::FastMode),
            &model_info,
        );
        let mut per_turn_config = per_turn_config;
        if per_turn_config.service_tier != resolved_service_tier {
            Arc::make_mut(&mut per_turn_config).service_tier = resolved_service_tier;
        }
        let per_turn_config = {
            let mut cached = self
                .turn_config_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match cached.as_ref() {
                Some(previous)
                    if same_turn_config_projection(previous.as_ref(), per_turn_config.as_ref()) =>
                {
                    Arc::clone(previous)
                }
                _ => {
                    *cached = Some(Arc::clone(&per_turn_config));
                    per_turn_config
                }
            }
        };
        let mut turn_context: TurnContext = Self::make_turn_context(
            self.thread_id(),
            self.session_id(),
            Some(Arc::clone(&self.services.auth_manager)),
            &self.services.session_telemetry,
            session_configuration.provider.clone(),
            &session_configuration,
            multi_agent_version,
            per_turn_config,
            model_info,
            available_models,
            self.services
                .network_proxy
                .load_full()
                .as_ref()
                .and_then(|started_proxy| {
                    Self::managed_network_proxy_active_for_permission_profile(
                        &session_configuration.permission_profile(),
                    )
                    .then(|| started_proxy.proxy())
                }),
            turn_environments,
            git_metadata_source,
            sub_id,
            skills_snapshot,
        );
        if let Some(final_schema) = final_output_json_schema {
            turn_context.final_output_json_schema = final_schema;
        }
        let turn_context = Arc::new(turn_context);
        if turn_context
            .environments
            .single_local_environment_cwd()
            .is_some()
        {
            turn_context.turn_metadata_state.spawn_git_enrichment_task();
        }
        turn_context
    }

    pub(crate) async fn maybe_emit_model_warnings_for_turn(&self, tc: &TurnContext) {
        if tc.model_info.used_fallback_model_metadata {
            self.send_event(
                tc,
                EventMsg::Warning(WarningEvent {
                    message: format!(
                        "Model metadata for `{}` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.",
                        tc.model_info.slug
                    ),
                }),
            )
            .await;
        }

        if let Some(message) =
            unsupported_code_mode_warning(&tc.model_info, tc.config.features.get())
        {
            self.send_event(tc, EventMsg::Warning(WarningEvent { message }))
                .await;
        }
    }

    pub(crate) async fn new_default_turn(&self) -> Arc<TurnContext> {
        self.new_default_turn_with_sub_id(self.next_internal_sub_id())
            .await
    }

    pub(crate) async fn new_default_turn_with_sub_id(&self, sub_id: String) -> Arc<TurnContext> {
        let session_configuration = self.default_turn_configuration().await;
        self.new_turn_from_configuration(
            sub_id,
            session_configuration,
            /*final_output_json_schema*/ None,
        )
        .await
    }

    pub(crate) async fn new_startup_prewarm_turn_with_sub_id(
        &self,
        sub_id: String,
    ) -> Arc<TurnContext> {
        let session_configuration = self.default_turn_configuration().await;
        self.new_startup_prewarm_turn_from_configuration(sub_id, session_configuration)
            .await
    }

    async fn default_turn_configuration(&self) -> SessionConfiguration {
        let state = self.state.lock().await;
        state.session_configuration.clone()
    }
}
