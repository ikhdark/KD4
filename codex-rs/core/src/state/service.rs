use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use super::SessionState;
use crate::SkillsService;
use crate::agent::AgentControl;
use crate::agents_md_manager::AgentsMdManager;
use crate::attestation::AttestationProvider;
use crate::client::ModelClient;
use crate::config::NetworkProxyAuditMetadata;
use crate::config::StartedNetworkProxy;
use crate::current_time::TimeProvider;
use crate::elicitation::ElicitationService;
use crate::environment_selection::ThreadEnvironments;
use crate::exec_policy::ExecPolicyManager;
use crate::git_workspace::GitWorkspaceCache;
use crate::guardian::GuardianRejection;
use crate::guardian::GuardianRejectionCircuitBreaker;
use crate::mcp::McpManager;
use crate::session::McpRuntimeSnapshot;
use crate::task_evidence::TaskEvidenceLedger;
use crate::tools::code_mode::CodeModeService;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use anyhow::Result;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_analytics::AnalyticsEventsClient;
use codex_core_plugins::PluginsManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_hooks::Hooks;
use codex_login::AuthManager;
use codex_mcp::McpConfig;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpRuntimeContext;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_rollout::state_db::StateDbHandle;
use codex_rollout_trace::ThreadTraceContext;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use std::path::PathBuf;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub(crate) struct SessionServices {
    /// The sole atomically published MCP runtime generation.
    pub(crate) mcp_runtime: Arc<ArcSwapOption<McpRuntimeSnapshot>>,
    /// Aggregate generation for model-visible planning state. Every invalidating
    /// publication must advance this value before a pending turn may replan.
    pub(crate) planning_generation: AtomicU64,
    /// Serializes environment-driven runtime rebuilds.
    pub(crate) mcp_projection_lock: Mutex<()>,
    pub(crate) mcp_startup_cancellation_token: Mutex<CancellationToken>,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    pub(crate) command_execution: CommandExecutionLedger,
    pub(crate) task_evidence: TaskEvidenceLedger,
    pub(crate) elicitations: ElicitationService,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) shell_zsh_path: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) main_execve_wrapper_exe: Option<PathBuf>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) hooks: ArcSwap<Hooks>,
    pub(crate) rollout_thread_trace: ThreadTraceContext,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) exec_policy: Arc<ExecPolicyManager>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) guardian_rejections: Mutex<HashMap<String, GuardianRejection>>,
    pub(crate) guardian_rejection_circuit_breaker: Mutex<GuardianRejectionCircuitBreaker>,
    pub(crate) runtime_handle: Handle,
    pub(crate) skills_service: Arc<SkillsService>,
    pub(crate) agents_md_manager: Arc<AgentsMdManager>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<ExtensionRegistry<crate::config::Config>>,
    pub(crate) session_extension_data: ExtensionData,
    pub(crate) thread_extension_data: ExtensionData,
    pub(crate) supports_openai_form_elicitation: AtomicBool,
    /// Raw capability selections for this thread. Each model step resolves them against its
    /// current executor environments before using them.
    pub(crate) selected_capability_roots: Vec<SelectedCapabilityRoot>,
    pub(crate) mcp_thread_init: ExtensionDataInit,
    pub(crate) agent_control: AgentControl,
    pub(crate) network_proxy: ArcSwapOption<StartedNetworkProxy>,
    pub(crate) network_proxy_audit_metadata: NetworkProxyAuditMetadata,
    pub(crate) managed_network_requirements_configured: bool,
    pub(crate) network_approval: Arc<NetworkApprovalService>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) live_thread: Option<LiveThread>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) time_provider: Arc<dyn TimeProvider>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) code_mode_service: CodeModeService,
    pub(crate) tool_search_handler_cache: ToolSearchHandlerCache,
    pub(crate) turn_environments: Arc<ThreadEnvironments>,
    pub(crate) git_workspace: Arc<GitWorkspaceCache>,
    pub(crate) source_reads: crate::tools::handlers::source_tools::SourceReadCoordinator,
}

impl SessionServices {
    /// Installs the manager before validating required servers so startup-time elicitation can
    /// resolve through the session's manager while validation waits.
    pub(crate) async fn install_mcp_connection_manager(
        &self,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: McpConnectionManager,
    ) -> Result<()> {
        // Session construction has not published a `Session` yet, so no pending
        // turn can race this initial runtime installation.
        let runtime = self.publish_mcp_runtime_unowned(
            config,
            plugins_available,
            runtime_context,
            available_environment_ids,
            manager,
        );
        runtime.manager().validate_required_servers().await
    }

    pub(crate) fn publish_mcp_runtime(
        &self,
        state_owner: &mut SessionState,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: McpConnectionManager,
    ) -> Arc<McpRuntimeSnapshot> {
        self.publish_mcp_runtime_update(
            state_owner,
            config,
            plugins_available,
            runtime_context,
            available_environment_ids,
            Arc::new(manager),
        )
    }

    pub(crate) fn publish_mcp_runtime_reusing_manager(
        &self,
        state_owner: &mut SessionState,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: Arc<McpConnectionManager>,
    ) -> Arc<McpRuntimeSnapshot> {
        self.publish_mcp_runtime_update(
            state_owner,
            config,
            plugins_available,
            runtime_context,
            available_environment_ids,
            manager,
        )
    }

    fn publish_mcp_runtime_update(
        &self,
        state_owner: &mut SessionState,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: Arc<McpConnectionManager>,
    ) -> Arc<McpRuntimeSnapshot> {
        let next_generation = self.planning_generation().saturating_add(1);
        let runtime = self.publish_mcp_runtime_with_generation(
            next_generation,
            config,
            plugins_available,
            runtime_context,
            available_environment_ids,
            manager,
        );
        let published_generation = self.advance_planning_generation(state_owner);
        debug_assert_eq!(published_generation, next_generation);
        runtime
    }

    fn publish_mcp_runtime_unowned(
        &self,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: McpConnectionManager,
    ) -> Arc<McpRuntimeSnapshot> {
        let generation = self.bump_planning_generation_unowned();
        self.publish_mcp_runtime_with_generation(
            generation,
            config,
            plugins_available,
            runtime_context,
            available_environment_ids,
            Arc::new(manager),
        )
    }

    fn publish_mcp_runtime_with_generation(
        &self,
        generation: u64,
        config: Arc<McpConfig>,
        plugins_available: bool,
        runtime_context: McpRuntimeContext,
        available_environment_ids: Vec<String>,
        manager: Arc<McpConnectionManager>,
    ) -> Arc<McpRuntimeSnapshot> {
        let runtime = Arc::new(McpRuntimeSnapshot::new(
            generation,
            config,
            plugins_available,
            manager,
            runtime_context,
            available_environment_ids,
        ));
        self.mcp_runtime.store(Some(Arc::clone(&runtime)));
        runtime
    }

    pub(crate) fn planning_generation(&self) -> u64 {
        self.planning_generation.load(Ordering::Acquire)
    }

    /// Advances planning state while the caller owns the session persistence
    /// mutex. Requiring the mutable state owner makes invalidation mutually
    /// exclusive with pending-plan compare-and-commit.
    pub(crate) fn advance_planning_generation(&self, _state_owner: &mut SessionState) -> u64 {
        self.bump_planning_generation_unowned()
    }

    fn bump_planning_generation_unowned(&self) -> u64 {
        self.planning_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    pub(crate) fn latest_mcp_runtime(&self) -> Arc<McpRuntimeSnapshot> {
        let Some(runtime) = self.mcp_runtime.load_full() else {
            unreachable!("MCP runtime must be installed before handling requests");
        };
        runtime
    }
}
