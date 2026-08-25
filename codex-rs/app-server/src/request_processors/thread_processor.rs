use super::*;
use crate::error_code::method_not_found;
use crate::thread_state::OutOfBandElicitationLeaseKey;
use codex_app_server_protocol::SelectedCapabilityRoot;
use codex_app_server_protocol::ThreadErrorData;
use codex_app_server_protocol::ThreadErrorReason;
use codex_extension_api::ExtensionDataInit;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS;
use codex_protocol::models::BUILT_IN_PERMISSION_PROFILE_WORKSPACE;
use codex_protocol::persisted_thread_settings::PersistedThreadSettings;
use codex_protocol::persisted_thread_settings::PersistedThreadSettingsOverrideMask;
use codex_protocol::persisted_thread_settings::reduce_persisted_thread_settings;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_tools::validate_dynamic_tools;

const THREAD_PAGE_DEFAULT_LIMIT: usize = 25;
const THREAD_PAGE_MAX_LIMIT: usize = 100;
const CODEX_TUI_CLIENT_NAME: &str = "codex-tui";
const THREAD_ROLLBACK_DEPRECATION_SUMMARY: &str =
    "thread/rollback is deprecated and will be removed soon";

async fn reuse_or_capture_fork_snapshot<T, F, Fut>(captured: Option<T>, capture: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    match captured {
        Some(snapshot) => snapshot,
        None => capture().await,
    }
}

fn thread_page_size(limit: Option<u32>) -> usize {
    limit
        .map(|value| value as usize)
        .unwrap_or(THREAD_PAGE_DEFAULT_LIMIT)
        .clamp(1, THREAD_PAGE_MAX_LIMIT)
}

fn thread_store_sort_key(sort_key: Option<ThreadSortKey>) -> StoreThreadSortKey {
    match sort_key.unwrap_or(ThreadSortKey::CreatedAt) {
        ThreadSortKey::CreatedAt => StoreThreadSortKey::CreatedAt,
        ThreadSortKey::UpdatedAt => StoreThreadSortKey::UpdatedAt,
        ThreadSortKey::RecencyAt => StoreThreadSortKey::RecencyAt,
    }
}

fn thread_store_sort_direction(sort_direction: SortDirection) -> StoreSortDirection {
    match sort_direction {
        SortDirection::Asc => StoreSortDirection::Asc,
        SortDirection::Desc => StoreSortDirection::Desc,
    }
}

fn desktop_activation_unavailable_reason(
    error: codex_core::DesktopActivationVerificationError,
) -> DesktopActivationUnavailableReason {
    use codex_core::DesktopActivationVerificationError as Error;
    match error {
        Error::NoAuthenticatedHostTransport => {
            DesktopActivationUnavailableReason::NoAuthoritativeBootstrapEvidence
        }
        Error::InvalidAuthoritativeEvidence
        | Error::RunningProcessIdentityMissing
        | Error::ImplementationIdentityMismatch
        | Error::ChallengeIdentityMismatch
        | Error::AuthenticatedChannelMismatch
        | Error::InitializedProcessMismatch => {
            DesktopActivationUnavailableReason::BootstrapEvidenceMismatch
        }
        Error::AuthoritativeEvidenceStale => {
            DesktopActivationUnavailableReason::BootstrapEvidenceStale
        }
        Error::RunningExecutableMismatch => {
            DesktopActivationUnavailableReason::RunningExecutableMismatch
        }
        Error::ChallengeMissingOrConsumed => {
            DesktopActivationUnavailableReason::ChallengeMissingOrConsumed
        }
        Error::ChallengeExpired => DesktopActivationUnavailableReason::ChallengeExpired,
        Error::InvalidDesktopObservation => {
            DesktopActivationUnavailableReason::InvalidDesktopObservation
        }
        Error::ActivationObligationChanged => {
            DesktopActivationUnavailableReason::ActivationObligationChanged
        }
        Error::ChallengeAlreadyRecordedWithDifferentPayload => {
            DesktopActivationUnavailableReason::ReplayPayloadMismatch
        }
        Error::PersistenceFailed => DesktopActivationUnavailableReason::PersistenceFailed,
    }
}

struct ThreadListFilters {
    model_providers: Option<Vec<String>>,
    source_kinds: Option<Vec<ThreadSourceKind>>,
    archived: bool,
    cwd_filters: Option<Vec<PathBuf>>,
    search_term: Option<String>,
    use_state_db_only: Option<bool>,
    relation_filter: Option<StoreThreadRelationFilter>,
}

fn collect_resume_override_mismatches(
    request: &ThreadResumeParams,
    config_snapshot: &ThreadConfigSnapshot,
) -> Vec<String> {
    let mut mismatch_details = Vec::new();

    if let Some(requested_model) = request.model.as_deref()
        && requested_model != config_snapshot.model
    {
        mismatch_details.push(format!(
            "model requested={requested_model} active={}",
            config_snapshot.model
        ));
    }
    if let Some(requested_provider) = request.model_provider.as_deref()
        && requested_provider != config_snapshot.model_provider_id
    {
        mismatch_details.push(format!(
            "model_provider requested={requested_provider} active={}",
            config_snapshot.model_provider_id
        ));
    }
    if let Some(requested_service_tier) = request.service_tier.as_ref()
        && requested_service_tier != &config_snapshot.service_tier
    {
        mismatch_details.push(format!(
            "service_tier requested={requested_service_tier:?} active={:?}",
            config_snapshot.service_tier
        ));
    }
    if let Some(requested_cwd) = request.cwd.as_deref() {
        let requested_cwd_path = std::path::PathBuf::from(requested_cwd);
        if requested_cwd_path != config_snapshot.cwd().as_path() {
            mismatch_details.push(format!(
                "cwd requested={} active={}",
                requested_cwd_path.display(),
                config_snapshot.cwd().display()
            ));
        }
    }
    if let Some(requested_runtime_workspace_roots) = request.runtime_workspace_roots.as_ref() {
        let requested_runtime_workspace_roots = requested_runtime_workspace_roots.to_vec();
        if requested_runtime_workspace_roots != config_snapshot.workspace_roots {
            mismatch_details.push(format!(
                "runtime_workspace_roots requested={requested_runtime_workspace_roots:?} active={:?}",
                config_snapshot.workspace_roots
            ));
        }
    }
    if let Some(requested_approval) = request.approval_policy.as_ref() {
        let active_approval: AskForApproval = config_snapshot.approval_policy.into();
        if requested_approval != &active_approval {
            mismatch_details.push(format!(
                "approval_policy requested={requested_approval:?} active={active_approval:?}"
            ));
        }
    }
    if let Some(requested_review_policy) = request.approvals_reviewer.as_ref() {
        let active_review_policy: codex_app_server_protocol::ApprovalsReviewer =
            config_snapshot.approvals_reviewer.into();
        if requested_review_policy != &active_review_policy {
            mismatch_details.push(format!(
                "approvals_reviewer requested={requested_review_policy:?} active={active_review_policy:?}"
            ));
        }
    }
    if let Some(requested_sandbox) = request.sandbox.as_ref() {
        let active_sandbox = config_snapshot.sandbox_policy();
        let sandbox_matches = matches!(
            (requested_sandbox, &active_sandbox),
            (
                SandboxMode::ReadOnly,
                codex_protocol::protocol::SandboxPolicy::ReadOnly { .. }
            ) | (
                SandboxMode::WorkspaceWrite,
                codex_protocol::protocol::SandboxPolicy::WorkspaceWrite { .. }
            ) | (
                SandboxMode::DangerFullAccess,
                codex_protocol::protocol::SandboxPolicy::DangerFullAccess
            ) | (
                SandboxMode::DangerFullAccess,
                codex_protocol::protocol::SandboxPolicy::ExternalSandbox { .. }
            )
        );
        if !sandbox_matches {
            mismatch_details.push(format!(
                "sandbox requested={requested_sandbox:?} active={active_sandbox:?}"
            ));
        }
    }
    if request.permissions.is_some() {
        mismatch_details.push(format!(
            "permissions override was provided and ignored while running; active={:?}",
            config_snapshot.active_permission_profile
        ));
    }
    if let Some(requested_personality) = request.personality.as_ref()
        && config_snapshot.personality.as_ref() != Some(requested_personality)
    {
        mismatch_details.push(format!(
            "personality requested={requested_personality:?} active={:?}",
            config_snapshot.personality
        ));
    }

    if request.config.is_some() {
        mismatch_details
            .push("config overrides were provided and ignored while running".to_string());
    }
    if request.base_instructions.is_some() {
        mismatch_details
            .push("baseInstructions override was provided and ignored while running".to_string());
    }
    if request.developer_instructions.is_some() {
        mismatch_details.push(
            "developerInstructions override was provided and ignored while running".to_string(),
        );
    }
    mismatch_details
}

fn persisted_settings_fallback(stored_thread: &StoredThread) -> PersistedThreadSettings {
    let environments = AbsolutePathBuf::from_absolute_path(&stored_thread.cwd)
        .ok()
        .map(|cwd| TurnEnvironmentSelections::new(cwd, Vec::new()));
    PersistedThreadSettings {
        model: stored_thread.model.clone(),
        model_provider_id: (!stored_thread.model_provider.is_empty())
            .then(|| stored_thread.model_provider.clone()),
        approval_policy: Some(stored_thread.approval_mode),
        permission_profile: Some(stored_thread.permission_profile.clone()),
        environments,
        reasoning_effort: stored_thread.reasoning_effort.clone().map(Some),
        ..Default::default()
    }
}

fn raw_override_contains(
    overrides: Option<&HashMap<String, serde_json::Value>>,
    key: &str,
) -> bool {
    overrides.is_some_and(|overrides| {
        overrides.contains_key(key)
            || overrides.keys().any(|candidate| {
                candidate
                    .strip_prefix(key)
                    .is_some_and(|suffix| suffix.starts_with('.'))
            })
    })
}

fn persisted_settings_override_mask(
    request_overrides: Option<&HashMap<String, serde_json::Value>>,
    typesafe_overrides: &ConfigOverrides,
) -> PersistedThreadSettingsOverrideMask {
    let permission_override = typesafe_overrides.permission_profile.is_some()
        || typesafe_overrides.default_permissions.is_some()
        || typesafe_overrides.sandbox_mode.is_some()
        || raw_override_contains(request_overrides, "permission_profile")
        || raw_override_contains(request_overrides, "default_permissions")
        || raw_override_contains(request_overrides, "permissions")
        || raw_override_contains(request_overrides, "sandbox_mode");
    PersistedThreadSettingsOverrideMask {
        model: typesafe_overrides.model.is_some()
            || raw_override_contains(request_overrides, "model"),
        model_provider_id: typesafe_overrides.model_provider.is_some()
            || raw_override_contains(request_overrides, "model_provider"),
        service_tier: typesafe_overrides.service_tier.is_some()
            || raw_override_contains(request_overrides, "service_tier"),
        developer_instructions: typesafe_overrides.developer_instructions.is_some()
            || raw_override_contains(request_overrides, "developer_instructions"),
        approval_policy: typesafe_overrides.approval_policy.is_some()
            || raw_override_contains(request_overrides, "approval_policy"),
        approvals_reviewer: typesafe_overrides.approvals_reviewer.is_some()
            || raw_override_contains(request_overrides, "approvals_reviewer"),
        permission_profile: permission_override,
        active_permission_profile: permission_override,
        environments: typesafe_overrides.cwd.is_some()
            || raw_override_contains(request_overrides, "cwd"),
        workspace_roots: typesafe_overrides.workspace_roots.is_some()
            || raw_override_contains(request_overrides, "workspace_roots"),
        profile_workspace_roots: permission_override,
        sandbox_policy: permission_override,
        windows_sandbox_level: raw_override_contains(request_overrides, "windows.sandbox")
            || raw_override_contains(request_overrides, "windows"),
        reasoning_effort: raw_override_contains(request_overrides, "model_reasoning_effort"),
        reasoning_summary: raw_override_contains(request_overrides, "model_reasoning_summary"),
        personality: typesafe_overrides.personality.is_some()
            || raw_override_contains(request_overrides, "personality"),
        collaboration_mode: raw_override_contains(request_overrides, "collaboration_mode"),
    }
}

fn normalize_thread_list_cwd_filters(
    cwd: Option<ThreadListCwdFilter>,
) -> Result<Option<Vec<PathBuf>>, JSONRPCErrorError> {
    let Some(cwd) = cwd else {
        return Ok(None);
    };

    let cwds = match cwd {
        ThreadListCwdFilter::One(cwd) => vec![cwd],
        ThreadListCwdFilter::Many(cwds) => cwds,
    };
    let mut normalized_cwds = Vec::with_capacity(cwds.len());
    for cwd in cwds {
        let cwd = AbsolutePathBuf::relative_to_current_dir(cwd.as_str())
            .map(AbsolutePathBuf::into_path_buf)
            .map_err(|err| {
                invalid_params(format!("invalid thread/list cwd filter `{cwd}`: {err}"))
            })?;
        normalized_cwds.push(cwd);
    }

    Ok(Some(normalized_cwds))
}

fn should_finalize_failed_thread_setup(
    rollback_succeeded: bool,
    thread_id_still_loaded: bool,
) -> bool {
    rollback_succeeded || !thread_id_still_loaded
}

struct HandledThreadCreationInstances<T: ?Sized> {
    by_id: HashMap<ThreadId, std::sync::Weak<T>>,
}

impl<T: ?Sized> Default for HandledThreadCreationInstances<T> {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }
}

impl<T: ?Sized> HandledThreadCreationInstances<T> {
    /// Records that the creation event for this loaded instance is being handled.
    ///
    /// Returns whether this is the first handler for the current instance.
    fn mark_handled(&mut self, thread_id: ThreadId, thread: &Arc<T>) -> bool {
        let already_handled = self
            .by_id
            .get(&thread_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|handled| Arc::ptr_eq(&handled, thread));
        self.by_id.insert(thread_id, Arc::downgrade(thread));
        !already_handled
    }

    fn forget(&mut self, thread_id: ThreadId) {
        self.by_id.remove(&thread_id);
    }
}

#[derive(Clone)]
pub(crate) struct ThreadRequestProcessor {
    pub(super) auth_manager: Arc<AuthManager>,
    pub(super) thread_manager: Arc<ThreadManager>,
    pub(super) outgoing: Arc<OutgoingMessageSender>,
    pub(super) _arg0_paths: Arg0DispatchPaths,
    pub(super) config: Arc<Config>,
    pub(super) config_manager: ConfigManager,
    pub(super) thread_store: Arc<dyn ThreadStore>,
    pub(super) pending_thread_unloads: Arc<PendingThreadUnloads>,
    pub(crate) thread_state_manager: ThreadStateManager,
    pub(crate) thread_watch_manager: ThreadWatchManager,
    pub(super) thread_list_state_permit: Arc<Semaphore>,
    pub(super) thread_goal_processor: ThreadGoalRequestProcessor,
    pub(super) state_db: Option<StateDbHandle>,
    pub(super) log_db: Option<LogDbLayer>,
    pub(super) background_tasks: TaskTracker,
    pub(super) skills_watcher: Arc<SkillsWatcher>,
    pub(super) initial_config_warnings: Arc<Vec<ConfigWarningNotification>>,
    pub(super) desktop_activation_bootstrap:
        Arc<crate::desktop_activation::DesktopActivationBootstrap>,
    pub(super) desktop_activation_challenge_owners:
        Arc<Mutex<HashMap<String, (String, ConnectionId)>>>,
    handled_thread_creation_instances:
        Arc<std::sync::Mutex<HandledThreadCreationInstances<CodexThread>>>,
}

/// Outcome of trying to satisfy a resume request from an already loaded thread.
enum RunningThreadResumeResult {
    /// The request was delegated to the loaded thread.
    Handled,
    /// No loaded thread handled the request.
    ///
    /// The optional stored thread contains the history-bearing probe that cold
    /// resume can reuse instead of reading the rollout again.
    NotRunning(Option<Box<StoredThread>>),
}

/// Result of validating a thread ID at the request boundary.
///
/// Resume and fork may address a rollout by path, so an invalid ID is not
/// always an error. Keeping the validation result lets those flows defer the
/// error until the ID is required without parsing the same input again.
enum ParsedThreadId {
    Valid(ThreadId),
    Invalid(String),
}

impl ParsedThreadId {
    fn parse(thread_id: &str) -> Self {
        match ThreadId::from_string(thread_id) {
            Ok(thread_id) => Self::Valid(thread_id),
            Err(error) => Self::Invalid(error.to_string()),
        }
    }

    fn valid(&self) -> Option<ThreadId> {
        match self {
            Self::Valid(thread_id) => Some(*thread_id),
            Self::Invalid(_) => None,
        }
    }

    fn required(&self) -> Result<ThreadId, JSONRPCErrorError> {
        match self {
            Self::Valid(thread_id) => Ok(*thread_id),
            Self::Invalid(error) => Err(invalid_request(format!("invalid session id: {error}"))),
        }
    }
}

impl ThreadRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        config_manager: ConfigManager,
        thread_store: Arc<dyn ThreadStore>,
        pending_thread_unloads: Arc<PendingThreadUnloads>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_list_state_permit: Arc<Semaphore>,
        thread_goal_processor: ThreadGoalRequestProcessor,
        state_db: Option<StateDbHandle>,
        log_db: Option<LogDbLayer>,
        skills_watcher: Arc<SkillsWatcher>,
        initial_config_warnings: Vec<ConfigWarningNotification>,
        desktop_activation_bootstrap: Arc<crate::desktop_activation::DesktopActivationBootstrap>,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            _arg0_paths: arg0_paths,
            config,
            config_manager,
            thread_store,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            thread_goal_processor,
            state_db,
            log_db,
            background_tasks: TaskTracker::new(),
            skills_watcher,
            initial_config_warnings: Arc::new(initial_config_warnings),
            desktop_activation_bootstrap,
            desktop_activation_challenge_owners: Arc::new(Mutex::new(HashMap::new())),
            handled_thread_creation_instances: Arc::new(std::sync::Mutex::new(
                HandledThreadCreationInstances::default(),
            )),
        }
    }

    fn mark_thread_creation_handled(&self, thread_id: ThreadId, thread: &Arc<CodexThread>) -> bool {
        self.handled_thread_creation_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_handled(thread_id, thread)
    }

    fn forget_handled_thread_creation(&self, thread_id: ThreadId) {
        self.handled_thread_creation_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forget(thread_id);
    }

    pub(crate) async fn thread_archive(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadArchiveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        match self.thread_archive_inner(params).await {
            Ok((response, archived_thread_ids)) => {
                self.outgoing
                    .send_response(request_id.clone(), response)
                    .await;
                for thread_id in archived_thread_ids {
                    self.outgoing
                        .send_server_notification(ServerNotification::ThreadArchived(
                            ThreadArchivedNotification { thread_id },
                        ))
                        .await;
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn thread_set_name(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadSetNameParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        match self.thread_set_name_response_inner(params).await {
            Ok((response, notification)) => {
                self.outgoing
                    .send_response(request_id.clone(), response)
                    .await;
                if let Some(notification) = notification {
                    self.outgoing
                        .send_server_notification(ServerNotification::ThreadNameUpdated(
                            notification,
                        ))
                        .await;
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn thread_unarchive(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadUnarchiveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        match self.thread_unarchive_inner(params).await {
            Ok((response, notification)) => {
                self.outgoing
                    .send_response(request_id.clone(), response)
                    .await;
                self.outgoing
                    .send_server_notification(ServerNotification::ThreadUnarchived(notification))
                    .await;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn thread_rollback(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRollbackParams,
        app_server_client_name: Option<&str>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if app_server_client_name != Some(CODEX_TUI_CLIENT_NAME) {
            self.send_thread_rollback_deprecation_notice(request_id.connection_id)
                .await;
        }
        self.thread_rollback_start(request_id, params)
            .await
            .map(|()| None)
    }

    async fn send_thread_rollback_deprecation_notice(&self, connection_id: ConnectionId) {
        self.outgoing
            .send_server_notification_to_connections(
                &[connection_id],
                ServerNotification::DeprecationNotice(DeprecationNoticeNotification {
                    summary: THREAD_ROLLBACK_DEPRECATION_SUMMARY.to_string(),
                    details: None,
                }),
            )
            .await;
    }

    async fn load_thread(
        &self,
        thread_id: &str,
    ) -> Result<(ThreadId, Arc<CodexThread>), JSONRPCErrorError> {
        // Resolve the core conversation handle from a v2 thread id string.
        let thread_id = ThreadId::from_string(thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| invalid_request(format!("thread not found: {thread_id}")))?;

        Ok((thread_id, thread))
    }

    pub(crate) async fn desktop_activation_obligation(
        &self,
        params: ThreadDesktopActivationObligationParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let obligation = thread.desktop_activation_obligation().await.map(|value| {
            ApiDesktopActivationObligation {
                thread_id: value.thread_id,
                evidence_epoch: value.evidence_epoch,
                implementation_identity: value.implementation_identity,
                activation_obligation_identity: value.activation_obligation_identity,
                requiring_plan_step_ids: value.requiring_plan_step_ids,
            }
        });
        Ok(Some(
            ThreadDesktopActivationObligationResponse { obligation }.into(),
        ))
    }

    pub(crate) async fn desktop_activation_challenge(
        &self,
        connection_id: ConnectionId,
        params: ThreadDesktopActivationChallengeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        if thread.desktop_activation_obligation().await.is_none() {
            return Ok(Some(
                ThreadDesktopActivationChallengeResponse {
                    challenge: None,
                    unavailable_reason: Some(
                        DesktopActivationUnavailableReason::NoCurrentActivationObligation,
                    ),
                }
                .into(),
            ));
        }
        let (evidence, consumed_at) =
            match self.desktop_activation_bootstrap.as_ref() {
                crate::desktop_activation::DesktopActivationBootstrap::Absent => return Ok(Some(
                    ThreadDesktopActivationChallengeResponse {
                        challenge: None,
                        unavailable_reason: Some(
                            DesktopActivationUnavailableReason::NoAuthoritativeBootstrapEvidence,
                        ),
                    }
                    .into(),
                )),
                crate::desktop_activation::DesktopActivationBootstrap::Malformed => {
                    return Ok(Some(
                        ThreadDesktopActivationChallengeResponse {
                            challenge: None,
                            unavailable_reason: Some(
                                DesktopActivationUnavailableReason::BootstrapEvidenceMalformed,
                            ),
                        }
                        .into(),
                    ));
                }
                crate::desktop_activation::DesktopActivationBootstrap::Available {
                    evidence,
                    consumed_at,
                } => (evidence.as_ref().clone(), consumed_at.clone()),
            };
        match thread
            .issue_desktop_activation_challenge(evidence, consumed_at)
            .await
        {
            Ok(value) => {
                let mut owners = self.desktop_activation_challenge_owners.lock().await;
                match owners.get(&value.challenge_id) {
                    Some((_, owner)) if *owner != connection_id => {
                        return Err(invalid_request(
                            "Desktop activation challenge belongs to another connection",
                        ));
                    }
                    Some(_) => {}
                    None => {
                        owners.insert(
                            value.challenge_id.clone(),
                            (params.thread_id, connection_id),
                        );
                    }
                }
                drop(owners);
                Ok(Some(
                    ThreadDesktopActivationChallengeResponse {
                        challenge: Some(ApiDesktopActivationChallenge {
                            challenge_id: value.challenge_id,
                            thread_id: value.thread_id,
                            evidence_epoch: value.evidence_epoch,
                            implementation_identity: value.implementation_identity,
                            activation_obligation_identity: value.activation_obligation_identity,
                            publisher_evidence_id: value.publisher_evidence_id,
                            expected_installed_executable_path: value
                                .expected_installed_executable_path,
                            expected_installed_executable_sha256: value
                                .expected_installed_executable_sha256,
                            publish_id: value.publish_id,
                            issued_at: value.issued_at,
                            expires_at: value.expires_at,
                        }),
                        unavailable_reason: None,
                    }
                    .into(),
                ))
            }
            Err(error) => Ok(Some(
                ThreadDesktopActivationChallengeResponse {
                    challenge: None,
                    unavailable_reason: Some(desktop_activation_unavailable_reason(error)),
                }
                .into(),
            )),
        }
    }

    pub(crate) async fn desktop_activation_record(
        &self,
        connection_id: ConnectionId,
        params: ThreadDesktopActivationRecordParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let thread_id = self
            .desktop_activation_challenge_owners
            .lock()
            .await
            .get(&params.challenge_id)
            .filter(|(_, owner)| *owner == connection_id)
            .map(|(thread_id, _)| thread_id.clone())
            .ok_or_else(|| invalid_request("unknown Desktop activation challenge"))?;
        let (_, thread) = self.load_thread(&thread_id).await?;
        let result = thread
            .record_desktop_activation(codex_core::DesktopActivationRecordObservation {
                challenge_id: params.challenge_id,
                desktop_process_id: params.desktop_process_id,
                desktop_executable_path: params.desktop_executable_path,
                observation_timestamp: params.observation_timestamp,
                initialization_observation_identity: params.initialization_observation_identity,
            })
            .await
            .map_err(|error| {
                invalid_request(format!(
                    "Desktop activation record rejected: {:?}",
                    desktop_activation_unavailable_reason(error)
                ))
            })?;
        Ok(Some(
            ThreadDesktopActivationRecordResponse {
                challenge_id: result.challenge_id,
                recorded_at: result.recorded_at,
                already_recorded: result.already_recorded,
            }
            .into(),
        ))
    }

    pub(super) async fn acquire_thread_list_state_permit(
        &self,
    ) -> Result<SemaphorePermit<'_>, JSONRPCErrorError> {
        self.thread_list_state_permit
            .acquire()
            .await
            .map_err(|err| {
                internal_error(format!("failed to acquire thread list state permit: {err}"))
            })
    }

    async fn set_app_server_client_info(
        thread: &CodexThread,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        thread
            .set_app_server_client_info(
                app_server_client_name,
                app_server_client_version,
                MCP_ELICITATIONS_AUTO_DENY,
            )
            .await
            .map_err(|err| internal_error(format!("failed to set app server client info: {err}")))
    }

    async fn finalize_thread_teardown(&self, thread_id: ThreadId) {
        self.forget_handled_thread_creation(thread_id);
        self.pending_thread_unloads.finish(&thread_id).await;
        self.outgoing
            .cancel_requests_for_thread(thread_id, /*error*/ None)
            .await;
        self.thread_state_manager
            .remove_thread_state(thread_id)
            .await;
        self.thread_watch_manager
            .remove_thread(&thread_id.to_string())
            .await;
    }

    async fn rollback_failed_resumed_thread(
        &self,
        thread_id: ThreadId,
        thread: &Arc<CodexThread>,
        was_already_running: bool,
    ) {
        if was_already_running {
            warn!(
                "skipping rollback for failed resume of thread {thread_id}: the request reused an existing instance"
            );
            return;
        }

        let rollback_succeeded = self
            .thread_manager
            .rollback_resumed_thread_spawn(thread_id, thread)
            .await;
        let thread_id_still_loaded =
            !rollback_succeeded && self.thread_manager.get_thread(thread_id).await.is_ok();
        if should_finalize_failed_thread_setup(rollback_succeeded, thread_id_still_loaded) {
            self.finalize_thread_teardown(thread_id).await;
        } else {
            warn!(
                "skipping app-server cleanup for failed resume {thread_id}: a different thread instance is loaded under that id"
            );
        }
    }

    pub(crate) async fn thread_unsubscribe(
        &self,
        params: ThreadUnsubscribeParams,
        connection_id: ConnectionId,
    ) -> Result<ThreadUnsubscribeResponse, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        if self.thread_manager.get_thread(thread_id).await.is_err() {
            self.finalize_thread_teardown(thread_id).await;
            return Ok(ThreadUnsubscribeResponse {
                status: ThreadUnsubscribeStatus::NotLoaded,
            });
        };

        let was_subscribed = self
            .thread_state_manager
            .unsubscribe_connection_from_thread(thread_id, connection_id)
            .await;

        let status = if was_subscribed {
            ThreadUnsubscribeStatus::Unsubscribed
        } else {
            ThreadUnsubscribeStatus::NotSubscribed
        };
        Ok(ThreadUnsubscribeResponse { status })
    }

    async fn prepare_thread_for_archive(&self, thread_id: ThreadId) {
        self.prepare_thread_for_removal(thread_id, "archive").await;
    }

    pub(super) async fn prepare_thread_for_removal(&self, thread_id: ThreadId, operation: &str) {
        let removed_conversation = self.thread_manager.remove_thread(&thread_id).await;
        if let Some(conversation) = removed_conversation {
            info!("thread {thread_id} was active; shutting down");
            match wait_for_thread_shutdown(&conversation).await {
                ThreadShutdownResult::Complete => {}
                ThreadShutdownResult::SubmitFailed => {
                    error!(
                        "failed to submit Shutdown to thread {thread_id}; proceeding with {operation}"
                    );
                }
                ThreadShutdownResult::TimedOut => {
                    warn!("thread {thread_id} shutdown timed out; proceeding with {operation}");
                }
            }
        }
        self.finalize_thread_teardown(thread_id).await;
    }

    fn listener_task_context(&self) -> ListenerTaskContext {
        ListenerTaskContext {
            thread_manager: Arc::clone(&self.thread_manager),
            thread_state_manager: self.thread_state_manager.clone(),
            outgoing: Arc::clone(&self.outgoing),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch_manager: self.thread_watch_manager.clone(),
            thread_list_state_permit: self.thread_list_state_permit.clone(),
            fallback_model_provider: self.config.model_provider_id.clone(),
            codex_home: self.config.codex_home.to_path_buf(),
            skills_watcher: Arc::clone(&self.skills_watcher),
        }
    }

    async fn ensure_conversation_listener(
        &self,
        conversation_id: ThreadId,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
        super::thread_lifecycle::ensure_conversation_listener(
            self.listener_task_context(),
            conversation_id,
            connection_id,
            raw_events_enabled,
        )
        .await
    }

    async fn ensure_conversation_listener_for_instance(
        &self,
        conversation_id: ThreadId,
        conversation: Arc<CodexThread>,
        connection_id: ConnectionId,
        raw_events_enabled: bool,
    ) -> Result<EnsureConversationListenerResult, JSONRPCErrorError> {
        super::thread_lifecycle::ensure_conversation_listener_for_instance(
            self.listener_task_context(),
            conversation_id,
            conversation,
            connection_id,
            raw_events_enabled,
        )
        .await
    }

    async fn ensure_listener_task_running(
        &self,
        conversation_id: ThreadId,
        conversation: Arc<CodexThread>,
        thread_state: Arc<Mutex<ThreadState>>,
    ) -> Result<(), JSONRPCErrorError> {
        super::thread_lifecycle::ensure_listener_task_running(
            self.listener_task_context(),
            conversation_id,
            conversation,
            thread_state,
        )
        .await
    }

    pub(crate) async fn thread_start(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
        request_context: RequestContext,
    ) -> Result<(), JSONRPCErrorError> {
        let ThreadStartParams {
            model,
            model_provider,
            allow_provider_model_fallback,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            config,
            service_name,
            base_instructions,
            developer_instructions,
            dynamic_tools,
            selected_capability_roots,
            mock_experimental_field: _mock_experimental_field,
            experimental_raw_events,
            personality,
            ephemeral,
            history_mode,
            session_start_source,
            thread_source,
            environments,
        } = params;
        if sandbox.is_some() && permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandbox`",
            ));
        }
        let environment_selections =
            resolve_turn_environment_selections(self.thread_manager.as_ref(), environments)?;
        let runtime_workspace_roots = runtime_workspace_roots.map(resolve_runtime_workspace_roots);
        let mut typesafe_overrides = self.build_thread_config_overrides(
            model,
            model_provider,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            base_instructions,
            developer_instructions,
            personality,
        );
        typesafe_overrides.ephemeral = ephemeral;
        let listener_task_context = self.listener_task_context();
        let request_trace = request_context.request_trace();
        let config_manager = self.config_manager.clone();
        let initial_config_warnings = Arc::clone(&self.initial_config_warnings);
        let outgoing = Arc::clone(&listener_task_context.outgoing);
        let error_request_id = request_id.clone();
        let thread_start_task = async move {
            if let Err(error) = Self::thread_start_task(
                listener_task_context,
                config_manager,
                request_id,
                app_server_client_name,
                app_server_client_version,
                supports_openai_form_elicitation,
                config,
                typesafe_overrides,
                dynamic_tools,
                selected_capability_roots.unwrap_or_default(),
                history_mode,
                session_start_source,
                thread_source,
                environment_selections,
                service_name,
                allow_provider_model_fallback,
                experimental_raw_events,
                request_trace,
                initial_config_warnings,
            )
            .await
            {
                outgoing.send_error(error_request_id, error).await;
            }
        };
        thread_start_task.instrument(request_context.span()).await;
        Ok(())
    }

    pub(crate) async fn drain_background_tasks(&self) {
        self.background_tasks.close();
        if tokio::time::timeout(Duration::from_secs(10), self.background_tasks.wait())
            .await
            .is_err()
        {
            warn!("timed out waiting for background tasks to shut down; proceeding");
        }
    }

    pub(crate) async fn shutdown_threads(&self) {
        let report = self
            .thread_manager
            .shutdown_all_threads_bounded(Duration::from_secs(10))
            .await;
        self.thread_state_manager
            .clear_all_out_of_band_elicitation_leases()
            .await;
        for thread_id in report.submit_failed {
            warn!("failed to submit Shutdown to thread {thread_id}");
        }
        for thread_id in report.timed_out {
            warn!("timed out waiting for thread {thread_id} to shut down");
        }
    }

    async fn request_trace_context(
        &self,
        request_id: &ConnectionRequestId,
    ) -> Option<codex_protocol::protocol::W3cTraceContext> {
        self.outgoing.request_trace_context(request_id).await
    }

    async fn submit_core_op(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
        op: Op,
    ) -> CodexResult<String> {
        thread
            .submit_with_trace(op, self.request_trace_context(request_id).await)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn thread_start_task(
        listener_task_context: ListenerTaskContext,
        config_manager: ConfigManager,
        request_id: ConnectionRequestId,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
        config_overrides: Option<HashMap<String, serde_json::Value>>,
        typesafe_overrides: ConfigOverrides,
        dynamic_tools: Option<Vec<DynamicToolSpec>>,
        selected_capability_roots: Vec<SelectedCapabilityRoot>,
        history_mode: Option<ThreadHistoryMode>,
        session_start_source: Option<codex_app_server_protocol::ThreadStartSource>,
        thread_source: Option<codex_protocol::protocol::ThreadSource>,
        environments: Option<Vec<TurnEnvironmentSelection>>,
        service_name: Option<String>,
        allow_provider_model_fallback: bool,
        experimental_raw_events: bool,
        request_trace: Option<W3cTraceContext>,
        initial_config_warnings: Arc<Vec<ConfigWarningNotification>>,
    ) -> Result<(), JSONRPCErrorError> {
        let thread_start_started_at = std::time::Instant::now();
        let requested_cwd = typesafe_overrides.cwd.clone();
        let mut config = config_manager
            .load_with_overrides(config_overrides.clone(), typesafe_overrides.clone())
            .await
            .map_config_load_error()?;

        // The user may have requested WorkspaceWrite or DangerFullAccess via
        // the command line, though in the process of deriving the Config, it
        // could be downgraded to ReadOnly (perhaps there is no sandbox
        // available on Windows or the enterprise config disallows it). The cwd
        // should still be considered "trusted" in this case.
        let requested_permissions_trust_project =
            requested_permissions_trust_project(&typesafe_overrides, config.cwd.as_path());
        let effective_permissions_trust_project = permission_profile_trusts_project(
            &config.permissions.effective_permission_profile(),
            config.cwd.as_path(),
        );

        if requested_cwd.is_some()
            && config.active_project.trust_level.is_none()
            && (requested_permissions_trust_project || effective_permissions_trust_project)
        {
            let trust_target = resolve_root_git_project_for_trust(LOCAL_FS.as_ref(), &config.cwd)
                .await
                .unwrap_or_else(|| config.cwd.clone());
            let current_cli_overrides = config_manager.current_cli_overrides();
            let cli_overrides_with_trust;
            let cli_overrides_for_reload = if let Err(err) =
                codex_core::config::set_project_trust_level(
                    &listener_task_context.codex_home,
                    trust_target.as_path(),
                    TrustLevel::Trusted,
                ) {
                warn!(
                    "failed to persist trusted project state for {}; continuing with in-memory trust for this thread: {err}",
                    trust_target.display()
                );
                let mut project = toml::map::Map::new();
                project.insert(
                    "trust_level".to_string(),
                    TomlValue::String("trusted".to_string()),
                );
                let mut projects = toml::map::Map::new();
                projects.insert(
                    project_trust_key(trust_target.as_path()),
                    TomlValue::Table(project),
                );
                cli_overrides_with_trust = current_cli_overrides
                    .iter()
                    .cloned()
                    .chain(std::iter::once((
                        "projects".to_string(),
                        TomlValue::Table(projects),
                    )))
                    .collect::<Vec<_>>();
                cli_overrides_with_trust.as_slice()
            } else {
                current_cli_overrides.as_slice()
            };

            config = config_manager
                .load_with_cli_overrides(
                    cli_overrides_for_reload,
                    config_overrides,
                    typesafe_overrides,
                    /*fallback_cwd*/ None,
                )
                .await
                .map_config_load_error()?;
        }

        if let Ok(Some(err)) =
            codex_core::check_execpolicy_for_warnings(&config.config_layer_stack).await
        {
            let notification = crate::exec_policy_config_warning(&err);
            if !initial_config_warnings.contains(&notification) {
                listener_task_context
                    .outgoing
                    .send_server_notification_to_connections(
                        &[request_id.connection_id],
                        ServerNotification::ConfigWarning(notification),
                    )
                    .await;
            }
        }

        let environments = environments.unwrap_or_else(|| {
            listener_task_context
                .thread_manager
                .default_environment_selections(&config.cwd)
        });
        let dynamic_tools = dynamic_tools.unwrap_or_default();
        if !dynamic_tools.is_empty() {
            validate_dynamic_tools(&dynamic_tools).map_err(invalid_request)?;
        }
        // Count callable functions rather than top-level namespace containers.
        let dynamic_tool_count: usize = dynamic_tools
            .iter()
            .map(|tool| match tool {
                DynamicToolSpec::Function(_) => 1,
                DynamicToolSpec::Namespace(namespace) => namespace.tools.len(),
            })
            .sum();
        let mut thread_extension_init = ExtensionDataInit::new();
        if !selected_capability_roots.is_empty() {
            thread_extension_init.insert(selected_capability_roots);
        }
        let create_thread_started_at = std::time::Instant::now();
        let NewThread {
            thread_id,
            thread,
            session_configured,
            ..
        } = listener_task_context
            .thread_manager
            .start_thread_with_options(StartThreadOptions {
                config,
                allow_provider_model_fallback,
                initial_history: match session_start_source
                    .unwrap_or(codex_app_server_protocol::ThreadStartSource::Startup)
                {
                    codex_app_server_protocol::ThreadStartSource::Startup => InitialHistory::New,
                    codex_app_server_protocol::ThreadStartSource::Clear => InitialHistory::Cleared,
                },
                history_mode,
                session_source: None,
                thread_source,
                dynamic_tools,
                metrics_service_name: service_name,
                parent_trace: request_trace,
                environments,
                thread_extension_init,
                supports_openai_form_elicitation,
            })
            .instrument(tracing::info_span!(
                "app_server.thread_start.create_thread",
                otel.name = "app_server.thread_start.create_thread",
                thread_start.dynamic_tool_count = dynamic_tool_count,
            ))
            .await
            .map_err(|err| match err {
                CodexErr::InvalidRequest(message) => invalid_request(message),
                CodexErr::UnsupportedOperation(message) => method_not_found(message),
                err => internal_error(format!("error creating thread: {err}")),
            })?;
        let session_telemetry = thread.session_telemetry();
        session_telemetry.record_startup_phase(
            "thread_start_create_thread",
            create_thread_started_at.elapsed(),
            Some("ready"),
        );

        if let Err(err) = Self::set_app_server_client_info(
            thread.as_ref(),
            app_server_client_name,
            app_server_client_version,
        )
        .await
        {
            if !listener_task_context
                .thread_manager
                .rollback_thread_spawn(thread_id, &thread)
                .await
            {
                warn!("failed to roll back thread {thread_id} after thread/start setup failed");
            }
            return Err(err);
        }

        let instruction_sources = thread.legacy_instruction_sources().await;
        let config_snapshot = thread
            .config_snapshot()
            .instrument(tracing::info_span!(
                "app_server.thread_start.config_snapshot",
                otel.name = "app_server.thread_start.config_snapshot",
            ))
            .await;
        let mut thread = build_thread_from_snapshot(
            thread_id,
            session_configured.session_id.to_string(),
            &config_snapshot,
            session_configured.rollout_path.clone(),
        );

        // Auto-attach a thread listener when starting a thread.
        log_listener_attach_result(
            super::thread_lifecycle::ensure_conversation_listener(
                listener_task_context.clone(),
                thread_id,
                request_id.connection_id,
                experimental_raw_events,
            )
            .instrument(tracing::info_span!(
                "app_server.thread_start.attach_listener",
                otel.name = "app_server.thread_start.attach_listener",
                thread_start.experimental_raw_events = experimental_raw_events,
            ))
            .await,
            thread_id,
            request_id.connection_id,
            "thread",
        );

        listener_task_context
            .thread_watch_manager
            .upsert_thread_silently(thread.clone())
            .instrument(tracing::info_span!(
                "app_server.thread_start.upsert_thread",
                otel.name = "app_server.thread_start.upsert_thread",
            ))
            .await;

        thread.status = resolve_thread_status(
            listener_task_context
                .thread_watch_manager
                .loaded_status_for_thread(&thread.id)
                .instrument(tracing::info_span!(
                    "app_server.thread_start.resolve_status",
                    otel.name = "app_server.thread_start.resolve_status",
                ))
                .await,
            /*has_in_progress_turn*/ false,
        );

        let sandbox = thread_response_sandbox_policy(
            &config_snapshot.permission_profile,
            config_snapshot.cwd().as_path(),
        );
        let cwd = config_snapshot.cwd().clone();
        let active_permission_profile =
            thread_response_active_permission_profile(config_snapshot.active_permission_profile);
        let thread_originator = config_snapshot.originator.clone();

        let response = ThreadStartResponse {
            thread: thread.clone(),
            model: config_snapshot.model,
            model_provider: config_snapshot.model_provider_id,
            service_tier: config_snapshot.service_tier,
            cwd,
            runtime_workspace_roots: config_snapshot.workspace_roots,
            instruction_sources,
            approval_policy: config_snapshot.approval_policy.into(),
            approvals_reviewer: config_snapshot.approvals_reviewer.into(),
            sandbox,
            active_permission_profile,
            reasoning_effort: config_snapshot.reasoning_effort,
        };
        let notif = thread_started_notification(thread);
        listener_task_context
            .outgoing
            .send_response_with_thread_originator(request_id, response, thread_originator)
            .instrument(tracing::info_span!(
                "app_server.thread_start.send_response",
                otel.name = "app_server.thread_start.send_response",
            ))
            .await;

        listener_task_context
            .outgoing
            .send_server_notification(ServerNotification::ThreadStarted(notif))
            .instrument(tracing::info_span!(
                "app_server.thread_start.notify_started",
                otel.name = "app_server.thread_start.notify_started",
            ))
            .await;
        session_telemetry.record_startup_phase(
            "thread_start_total",
            thread_start_started_at.elapsed(),
            Some("ready"),
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn build_thread_config_overrides(
        &self,
        model: Option<String>,
        model_provider: Option<String>,
        service_tier: Option<Option<String>>,
        cwd: Option<String>,
        runtime_workspace_roots: Option<Vec<AbsolutePathBuf>>,
        approval_policy: Option<codex_app_server_protocol::AskForApproval>,
        approvals_reviewer: Option<codex_app_server_protocol::ApprovalsReviewer>,
        sandbox: Option<SandboxMode>,
        permissions: Option<String>,
        base_instructions: Option<String>,
        developer_instructions: Option<String>,
        personality: Option<Personality>,
    ) -> ConfigOverrides {
        ConfigOverrides {
            model,
            model_provider,
            service_tier,
            cwd: cwd.map(PathBuf::from),
            workspace_roots: runtime_workspace_roots,
            default_permissions: permissions,
            approval_policy: approval_policy
                .map(codex_app_server_protocol::AskForApproval::to_core),
            approvals_reviewer: approvals_reviewer
                .map(codex_app_server_protocol::ApprovalsReviewer::to_core),
            sandbox_mode: sandbox.map(SandboxMode::to_core),
            base_instructions,
            developer_instructions,
            personality,
            ..Default::default()
        }
    }

    async fn thread_archive_inner(
        &self,
        params: ThreadArchiveParams,
    ) -> Result<(ThreadArchiveResponse, Vec<String>), JSONRPCErrorError> {
        let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
        self.thread_archive_response(params).await
    }

    async fn thread_archive_response(
        &self,
        params: ThreadArchiveParams,
    ) -> Result<(ThreadArchiveResponse, Vec<String>), JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid session id: {err}")))?;

        let thread_ids = self.state_db_spawn_subtree_thread_ids(thread_id).await?;

        let mut archive_thread_ids = Vec::new();
        match self
            .thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
        {
            Ok(thread) => {
                if thread.archived_at.is_none() {
                    archive_thread_ids.push(thread_id);
                }
            }
            Err(err) => return Err(thread_store_archive_error("archive", err)),
        }
        for descendant_thread_id in thread_ids.into_iter().skip(1) {
            match self
                .thread_store
                .read_thread(StoreReadThreadParams {
                    thread_id: descendant_thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
            {
                Ok(thread) => {
                    if thread.archived_at.is_none() {
                        archive_thread_ids.push(descendant_thread_id);
                    }
                }
                Err(err) => {
                    warn!(
                        "failed to read spawned descendant thread {descendant_thread_id} while archiving {thread_id}: {err}"
                    );
                }
            }
        }

        let mut archived_thread_ids = Vec::new();
        let Some((parent_thread_id, descendant_thread_ids)) = archive_thread_ids.split_first()
        else {
            return Ok((ThreadArchiveResponse {}, archived_thread_ids));
        };

        match self
            .thread_store
            .archive_thread(StoreArchiveThreadParams {
                thread_id: *parent_thread_id,
            })
            .await
        {
            Ok(()) => {
                self.prepare_thread_for_archive(*parent_thread_id).await;
                archived_thread_ids.push(parent_thread_id.to_string());
            }
            Err(err) => return Err(thread_store_archive_error("archive", err)),
        }

        for descendant_thread_id in descendant_thread_ids.iter().rev().copied() {
            match self
                .thread_store
                .archive_thread(StoreArchiveThreadParams {
                    thread_id: descendant_thread_id,
                })
                .await
            {
                Ok(()) => {
                    self.prepare_thread_for_archive(descendant_thread_id).await;
                    archived_thread_ids.push(descendant_thread_id.to_string());
                }
                Err(err) => {
                    warn!(
                        "failed to archive spawned descendant thread {descendant_thread_id} while archiving {thread_id}: {err}"
                    );
                }
            }
        }

        Ok((ThreadArchiveResponse {}, archived_thread_ids))
    }

    pub(super) async fn state_db_spawn_subtree_thread_ids(
        &self,
        thread_id: ThreadId,
    ) -> Result<Vec<ThreadId>, JSONRPCErrorError> {
        self.thread_manager
            .list_agent_subtree_thread_ids(thread_id)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to list spawned descendants for thread id {thread_id}: {err}"
                ))
            })
    }

    pub(crate) async fn thread_increment_elicitation(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadIncrementElicitationParams,
    ) -> Result<ThreadIncrementElicitationResponse, JSONRPCErrorError> {
        let (thread_id, thread) = self.load_thread(&params.thread_id).await?;
        let lease_id = Uuid::now_v7().to_string();
        let lease = OutOfBandElicitationLeaseKey::new(request_id.connection_id, lease_id.clone());
        let count = self
            .thread_state_manager
            .acquire_out_of_band_elicitation_lease(thread_id, lease, &thread)
            .await
            .map_err(|err| match err {
                CodexErr::InvalidRequest(message) => invalid_request(message),
                err => internal_error(format!(
                    "failed to acquire out-of-band elicitation lease: {err}"
                )),
            })?;
        Ok(ThreadIncrementElicitationResponse {
            lease_id,
            count,
            paused: count > 0,
        })
    }

    pub(crate) async fn thread_decrement_elicitation(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadDecrementElicitationParams,
    ) -> Result<ThreadDecrementElicitationResponse, JSONRPCErrorError> {
        let ThreadDecrementElicitationParams {
            thread_id,
            lease_id,
        } = params;
        let thread_id = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let lease = OutOfBandElicitationLeaseKey::new(request_id.connection_id, lease_id);
        let released_count = self
            .thread_state_manager
            .release_out_of_band_elicitation_lease(thread_id, &lease)
            .await;
        let count = match released_count {
            Some(count) => count,
            None => self
                .thread_manager
                .get_thread(thread_id)
                .await
                .map_or(0, |thread| {
                    thread.active_out_of_band_elicitation_lease_count()
                }),
        };
        Ok(ThreadDecrementElicitationResponse {
            count,
            paused: count > 0,
        })
    }

    async fn thread_set_name_response_inner(
        &self,
        params: ThreadSetNameParams,
    ) -> Result<(ThreadSetNameResponse, Option<ThreadNameUpdatedNotification>), JSONRPCErrorError>
    {
        let ThreadSetNameParams { thread_id, name } = params;
        let thread_id = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let Some(name) = codex_thread_store::normalize_thread_name(&name) else {
            return Err(invalid_request("thread name must not be empty"));
        };

        let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
        self.thread_manager
            .update_thread_metadata(
                thread_id,
                StoreThreadMetadataPatch {
                    name: Some(Some(name.clone())),
                    ..Default::default()
                },
                /*include_archived*/ false,
            )
            .await
            .map_err(|err| core_thread_write_error("set thread name", err))?;

        Ok((
            ThreadSetNameResponse {},
            Some(ThreadNameUpdatedNotification {
                thread_id: thread_id.to_string(),
                thread_name: Some(name),
            }),
        ))
    }

    pub(crate) async fn thread_memory_mode_set(
        &self,
        params: ThreadMemoryModeSetParams,
    ) -> Result<ThreadMemoryModeSetResponse, JSONRPCErrorError> {
        let ThreadMemoryModeSetParams { thread_id, mode } = params;
        let thread_id = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        self.thread_manager
            .update_thread_metadata(
                thread_id,
                StoreThreadMetadataPatch {
                    memory_mode: Some(mode),
                    ..Default::default()
                },
                /*include_archived*/ false,
            )
            .await
            .map_err(|err| core_thread_write_error("set thread memory mode", err))?;

        Ok(ThreadMemoryModeSetResponse {})
    }

    pub(crate) async fn memory_reset(&self) -> Result<MemoryResetResponse, JSONRPCErrorError> {
        let state_db = self
            .state_db
            .clone()
            .ok_or_else(|| internal_error("sqlite state db unavailable for memory reset"))?;

        state_db
            .memories()
            .clear_memory_data()
            .await
            .map_err(|err| {
                internal_error(format!("failed to clear memory rows in memories db: {err}"))
            })?;

        clear_memory_roots_contents(&self.config.codex_home)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to clear memory directories under {}: {err}",
                    self.config.codex_home.display()
                ))
            })?;

        Ok(MemoryResetResponse {})
    }

    pub(crate) async fn thread_metadata_update(
        &self,
        params: ThreadMetadataUpdateParams,
    ) -> Result<ThreadMetadataUpdateResponse, JSONRPCErrorError> {
        let ThreadMetadataUpdateParams {
            thread_id,
            git_info,
        } = params;

        let thread_uuid = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let Some(ThreadMetadataGitInfoUpdateParams {
            sha,
            branch,
            origin_url,
        }) = git_info
        else {
            return Err(invalid_request("gitInfo must include at least one field"));
        };

        if sha.is_none() && branch.is_none() && origin_url.is_none() {
            return Err(invalid_request("gitInfo must include at least one field"));
        }

        let git_sha = Self::normalize_thread_metadata_git_field(sha, "gitInfo.sha")?;
        let git_branch = Self::normalize_thread_metadata_git_field(branch, "gitInfo.branch")?;
        let git_origin_url =
            Self::normalize_thread_metadata_git_field(origin_url, "gitInfo.originUrl")?;

        let patch = StoreThreadMetadataPatch {
            git_info: Some(StoreGitInfoPatch {
                sha: git_sha,
                branch: git_branch,
                origin_url: git_origin_url,
            }),
            ..Default::default()
        };

        let updated_thread = {
            let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
            self.thread_manager
                .update_thread_metadata(thread_uuid, patch, /*include_archived*/ true)
                .await
                .map_err(|err| core_thread_write_error("update thread metadata", err))?
        };
        let (mut thread, _) = thread_from_stored_thread(
            updated_thread,
            self.config.model_provider_id.as_str(),
            &self.config.cwd,
        );
        if let Ok(loaded_thread) = self.thread_manager.get_thread(thread_uuid).await {
            thread.session_id = loaded_thread.session_configured().session_id.to_string();
        }
        self.attach_thread_name(thread_uuid, &mut thread).await;
        thread.status = resolve_thread_status(
            self.thread_watch_manager
                .loaded_status_for_thread(&thread.id)
                .await,
            /*has_in_progress_turn*/ false,
        );

        Ok(ThreadMetadataUpdateResponse { thread })
    }

    fn normalize_thread_metadata_git_field(
        value: Option<Option<String>>,
        name: &str,
    ) -> Result<Option<Option<String>>, JSONRPCErrorError> {
        match value {
            Some(Some(value)) => {
                let value = value.trim().to_string();
                if value.is_empty() {
                    return Err(invalid_request(format!("{name} must not be empty")));
                }
                Ok(Some(Some(value)))
            }
            Some(None) => Ok(Some(None)),
            None => Ok(None),
        }
    }

    async fn thread_unarchive_inner(
        &self,
        params: ThreadUnarchiveParams,
    ) -> Result<(ThreadUnarchiveResponse, ThreadUnarchivedNotification), JSONRPCErrorError> {
        let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
        let (response, thread_id) = self.thread_unarchive_response(params).await?;
        Ok((response, ThreadUnarchivedNotification { thread_id }))
    }

    async fn thread_unarchive_response(
        &self,
        params: ThreadUnarchiveParams,
    ) -> Result<(ThreadUnarchiveResponse, String), JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|err| invalid_request(format!("invalid session id: {err}")))?;

        let fallback_provider = self.config.model_provider_id.clone();
        let stored_thread = self
            .thread_store
            .unarchive_thread(StoreArchiveThreadParams { thread_id })
            .await
            .map_err(|err| thread_store_archive_error("unarchive", err))?;
        let (mut thread, _) =
            thread_from_stored_thread(stored_thread, fallback_provider.as_str(), &self.config.cwd);

        thread.status = resolve_thread_status(
            self.thread_watch_manager
                .loaded_status_for_thread(&thread.id)
                .await,
            /*has_in_progress_turn*/ false,
        );
        self.attach_thread_name(thread_id, &mut thread).await;
        let thread_id = thread.id.clone();
        Ok((ThreadUnarchiveResponse { thread }, thread_id))
    }

    async fn thread_rollback_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRollbackParams,
    ) -> Result<(), JSONRPCErrorError> {
        let ThreadRollbackParams {
            thread_id,
            num_turns,
        } = params;

        if num_turns == 0 {
            return Err(invalid_request("numTurns must be >= 1"));
        }

        let (thread_id, thread) = self.load_thread(&thread_id).await?;

        let request = request_id.clone();

        let rollback_already_in_progress = {
            let thread_state = self.thread_state_manager.thread_state(thread_id).await;
            let mut thread_state = thread_state.lock().await;
            if thread_state.pending_rollbacks.is_some() {
                true
            } else {
                thread_state.pending_rollbacks = Some(request.clone());
                false
            }
        };
        if rollback_already_in_progress {
            return Err(invalid_request(
                "rollback already in progress for this thread",
            ));
        }

        if let Err(err) = self
            .submit_core_op(
                request_id,
                thread.as_ref(),
                Op::ThreadRollback { num_turns },
            )
            .await
        {
            // No ThreadRollback event will arrive if an error occurs.
            // Clean up and reply immediately.
            let thread_state = self.thread_state_manager.thread_state(thread_id).await;
            thread_state.lock().await.pending_rollbacks = None;

            return Err(internal_error(format!("failed to start rollback: {err}")));
        }
        Ok(())
    }

    pub(crate) async fn thread_compact_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadCompactStartParams,
    ) -> Result<ThreadCompactStartResponse, JSONRPCErrorError> {
        let ThreadCompactStartParams { thread_id } = params;

        let (_, thread) = self.load_thread(&thread_id).await?;
        self.submit_core_op(request_id, thread.as_ref(), Op::Compact)
            .await
            .map_err(|err| internal_error(format!("failed to start compaction: {err}")))?;
        Ok(ThreadCompactStartResponse {})
    }

    pub(crate) async fn thread_background_terminals_clean(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadBackgroundTerminalsCleanParams,
    ) -> Result<ThreadBackgroundTerminalsCleanResponse, JSONRPCErrorError> {
        let ThreadBackgroundTerminalsCleanParams { thread_id } = params;

        let (_, thread) = self.load_thread(&thread_id).await?;
        self.submit_core_op(request_id, thread.as_ref(), Op::CleanBackgroundTerminals)
            .await
            .map_err(|err| {
                internal_error(format!("failed to clean background terminals: {err}"))
            })?;
        Ok(ThreadBackgroundTerminalsCleanResponse {})
    }

    pub(crate) async fn thread_background_terminals_list(
        &self,
        params: ThreadBackgroundTerminalsListParams,
    ) -> Result<ThreadBackgroundTerminalsListResponse, JSONRPCErrorError> {
        let ThreadBackgroundTerminalsListParams {
            thread_id,
            cursor,
            limit,
        } = params;

        let (_, thread) = self.load_thread(&thread_id).await?;
        let terminals = thread
            .list_background_terminals()
            .await
            .into_iter()
            .map(|terminal| {
                // TODO(anp): Migrate ThreadBackgroundTerminal to PathUri.
                let cwd = terminal.cwd.to_abs_path().map_err(|err| {
                    internal_error(format!("background terminal has invalid cwd: {err}"))
                })?;
                Ok(ThreadBackgroundTerminal {
                    item_id: terminal.item_id,
                    process_id: terminal.process_id,
                    command: terminal.command,
                    cwd,
                    os_pid: None,
                    cpu_percent: None,
                    rss_kb: None,
                })
            })
            .collect::<Result<Vec<_>, JSONRPCErrorError>>()?;

        let (data, next_cursor) = paginate_background_terminals(&terminals, cursor, limit)?;

        Ok(ThreadBackgroundTerminalsListResponse { data, next_cursor })
    }

    pub(crate) async fn thread_background_terminals_terminate(
        &self,
        params: ThreadBackgroundTerminalsTerminateParams,
    ) -> Result<ThreadBackgroundTerminalsTerminateResponse, JSONRPCErrorError> {
        let ThreadBackgroundTerminalsTerminateParams {
            thread_id,
            process_id,
        } = params;
        let process_id = process_id.parse::<u32>().map_err(|err| {
            invalid_request(format!("invalid background terminal process id: {err}"))
        })?;

        let (_, thread) = self.load_thread(&thread_id).await?;
        let terminated = thread.terminate_background_terminal(process_id).await;
        Ok(ThreadBackgroundTerminalsTerminateResponse { terminated })
    }

    pub(crate) async fn thread_shell_command(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadShellCommandParams,
    ) -> Result<ThreadShellCommandResponse, JSONRPCErrorError> {
        let ThreadShellCommandParams { thread_id, command } = params;
        let command = command.trim().to_string();
        if command.is_empty() {
            return Err(invalid_request("command must not be empty"));
        }
        // `thread/shellCommand` is app-server's local-host shell escape hatch,
        // not the normal turn-selected shell tool path.
        if self
            .thread_manager
            .environment_manager()
            .try_local_environment()
            .is_none()
        {
            return Err(internal_error("local environment is not configured"));
        }

        let (_, thread) = self.load_thread(&thread_id).await?;
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RunUserShellCommand { command },
        )
        .await
        .map_err(|err| internal_error(format!("failed to start shell command: {err}")))?;
        Ok(ThreadShellCommandResponse {})
    }

    pub(crate) async fn thread_approve_guardian_denied_action(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadApproveGuardianDeniedActionParams,
    ) -> Result<ThreadApproveGuardianDeniedActionResponse, JSONRPCErrorError> {
        let ThreadApproveGuardianDeniedActionParams { thread_id, event } = params;
        let event = serde_json::from_value(event)
            .map_err(|err| invalid_request(format!("invalid Guardian denial event: {err}")))?;
        let (_, thread) = self.load_thread(&thread_id).await?;

        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::ApproveGuardianDeniedAction { event },
        )
        .await
        .map_err(|err| internal_error(format!("failed to approve Guardian denial: {err}")))?;
        Ok(ThreadApproveGuardianDeniedActionResponse {})
    }

    async fn refresh_loaded_thread_statuses(&self, threads: &mut [Thread]) {
        let thread_ids = threads.iter().map(|thread| thread.id.clone()).collect();
        let statuses = self
            .thread_watch_manager
            .loaded_statuses_for_threads(thread_ids)
            .await;
        for thread in threads {
            if let Some(status) = statuses.get(&thread.id) {
                thread.status = status.clone();
            }
        }
    }

    pub(crate) async fn thread_list(
        &self,
        params: ThreadListParams,
    ) -> Result<ThreadListResponse, JSONRPCErrorError> {
        let ThreadListParams {
            cursor,
            limit,
            sort_key,
            sort_direction,
            model_providers,
            source_kinds,
            archived,
            cwd,
            use_state_db_only,
            search_term,
            parent_thread_id,
            ancestor_thread_id,
        } = params;
        let cwd_filters = normalize_thread_list_cwd_filters(cwd)?;
        let relation_filter = match (parent_thread_id, ancestor_thread_id) {
            (Some(_), Some(_)) => {
                return Err(invalid_request(
                    "parentThreadId and ancestorThreadId are mutually exclusive",
                ));
            }
            (Some(parent_thread_id), None) => Some(StoreThreadRelationFilter::DirectChildrenOf(
                ThreadId::from_string(&parent_thread_id)
                    .map_err(|err| invalid_request(format!("invalid parent thread id: {err}")))?,
            )),
            (None, Some(ancestor_thread_id)) => Some(StoreThreadRelationFilter::DescendantsOf(
                ThreadId::from_string(&ancestor_thread_id)
                    .map_err(|err| invalid_request(format!("invalid ancestor thread id: {err}")))?,
            )),
            (None, None) => None,
        };
        if relation_filter.is_some() && use_state_db_only == Some(false) {
            return Err(invalid_request(
                "relationship-filtered thread listing does not support scan-and-repair storage",
            ));
        }

        let requested_page_size = thread_page_size(limit);
        let store_sort_key = thread_store_sort_key(sort_key);
        let sort_direction = sort_direction.unwrap_or(SortDirection::Desc);
        let (stored_threads, next_cursor, backwards_cursor) = self
            .list_threads_common(
                requested_page_size,
                cursor,
                store_sort_key,
                sort_direction,
                ThreadListFilters {
                    model_providers,
                    source_kinds,
                    archived: archived.unwrap_or(false),
                    cwd_filters,
                    search_term,
                    use_state_db_only,
                    relation_filter,
                },
            )
            .await?;
        let mut threads = Vec::with_capacity(stored_threads.len());
        let fallback_provider = self.config.model_provider_id.clone();

        for stored_thread in stored_threads {
            let (thread, _) = thread_from_stored_thread(
                stored_thread,
                fallback_provider.as_str(),
                &self.config.cwd,
            );
            threads.push(thread);
        }
        self.refresh_loaded_thread_statuses(&mut threads).await;
        Ok(ThreadListResponse {
            data: threads,
            next_cursor,
            backwards_cursor,
        })
    }

    pub(crate) async fn thread_search(
        &self,
        params: ThreadSearchParams,
    ) -> Result<ThreadSearchResponse, JSONRPCErrorError> {
        let ThreadSearchParams {
            cursor,
            limit,
            sort_key,
            sort_direction,
            source_kinds,
            archived,
            search_term,
        } = params;
        let search_term = search_term.trim().to_string();
        let search_term = (!search_term.is_empty())
            .then_some(search_term)
            .ok_or_else(|| invalid_request("thread/search requires a non-empty searchTerm"))?;
        let requested_page_size = thread_page_size(limit);
        let store_sort_key = thread_store_sort_key(sort_key);
        let store_sort_direction = sort_direction.unwrap_or(SortDirection::Desc);
        let (allowed_sources, source_kind_filter) = compute_source_filters(source_kinds);
        let mut cursor_obj = cursor;
        let mut last_cursor = cursor_obj.clone();
        let mut remaining = requested_page_size;
        let mut search_results = Vec::with_capacity(requested_page_size);
        let mut next_cursor = None;

        while remaining > 0 {
            let page = self
                .thread_store
                .search_threads(StoreSearchThreadsParams {
                    page_size: remaining.min(THREAD_PAGE_MAX_LIMIT),
                    cursor: cursor_obj.clone(),
                    sort_key: store_sort_key,
                    sort_direction: thread_store_sort_direction(store_sort_direction),
                    allowed_sources: allowed_sources.clone(),
                    archived: archived.unwrap_or(false),
                    search_term: search_term.clone(),
                })
                .await
                .map_err(thread_store_list_error)?;

            for result in page.items {
                let source = with_thread_spawn_agent_metadata(
                    result.thread.source.clone(),
                    result.thread.agent_nickname.clone(),
                    result.thread.agent_role.clone(),
                );
                if source_kind_filter
                    .as_ref()
                    .is_none_or(|filter| source_kind_matches(&source, filter))
                {
                    search_results.push(result);
                    if search_results.len() >= requested_page_size {
                        break;
                    }
                }
            }

            remaining = requested_page_size.saturating_sub(search_results.len());
            next_cursor = page.next_cursor;
            if remaining == 0 {
                break;
            }

            let Some(cursor_val) = next_cursor.clone() else {
                break;
            };
            if last_cursor.as_ref() == Some(&cursor_val) {
                next_cursor = None;
                break;
            }
            last_cursor = Some(cursor_val.clone());
            cursor_obj = Some(cursor_val);
        }

        let backwards_cursor = search_results.first().and_then(|result| {
            thread_backwards_cursor_for_sort_key(
                &result.thread,
                store_sort_key,
                store_sort_direction,
            )
        });
        let fallback_provider = self.config.model_provider_id.clone();
        let mut threads = Vec::with_capacity(search_results.len());
        let mut snippets = Vec::with_capacity(search_results.len());
        for result in search_results {
            let (thread, _) = thread_from_stored_thread(
                result.thread,
                fallback_provider.as_str(),
                &self.config.cwd,
            );
            threads.push(thread);
            snippets.push(result.snippet);
        }
        self.refresh_loaded_thread_statuses(&mut threads).await;
        let data = threads
            .into_iter()
            .zip(snippets)
            .map(|(thread, snippet)| ThreadSearchResult { thread, snippet })
            .collect();

        Ok(ThreadSearchResponse {
            data,
            next_cursor,
            backwards_cursor,
        })
    }

    pub(crate) async fn thread_loaded_list(
        &self,
        params: ThreadLoadedListParams,
    ) -> Result<ThreadLoadedListResponse, JSONRPCErrorError> {
        let ThreadLoadedListParams { cursor, limit } = params;
        let cursor = cursor
            .map(|cursor| {
                ThreadId::from_string(&cursor)
                    .map(|id| id.to_string())
                    .map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))
            })
            .transpose()?;
        let mut data: Vec<String> = self
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .map(|thread_id| thread_id.to_string())
            .collect();

        if data.is_empty() {
            return Ok(ThreadLoadedListResponse {
                data,
                next_cursor: None,
            });
        }

        data.sort();
        let total = data.len();
        let start = match cursor {
            Some(cursor) => match data.binary_search(&cursor) {
                Ok(idx) => idx + 1,
                Err(idx) => idx,
            },
            None => 0,
        };

        let effective_limit = limit.unwrap_or(total as u32).max(1) as usize;
        let end = start.saturating_add(effective_limit).min(total);
        let page = data[start..end].to_vec();
        let next_cursor = page.last().filter(|_| end < total).cloned();

        Ok(ThreadLoadedListResponse {
            data: page,
            next_cursor,
        })
    }

    pub(crate) async fn thread_read(
        &self,
        params: ThreadReadParams,
    ) -> Result<ThreadReadResponse, JSONRPCErrorError> {
        let ThreadReadParams {
            thread_id,
            include_turns,
        } = params;

        let thread_uuid = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let thread = self
            .read_thread_view(thread_uuid, include_turns)
            .await
            .map_err(thread_read_view_error)?;
        Ok(ThreadReadResponse { thread })
    }

    /// Builds the API view for `thread/read` from persisted metadata plus optional live state.
    async fn read_thread_view(
        &self,
        thread_id: ThreadId,
        include_turns: bool,
    ) -> Result<Thread, ThreadReadViewError> {
        let loaded_thread = self.thread_manager.get_thread(thread_id).await.ok();
        let mut thread = if include_turns {
            if let Some(loaded_thread) = loaded_thread.as_ref() {
                // Loaded thread with turns: use persisted metadata when it exists,
                // but reconstruct turns from the live ThreadStore history.
                let persisted_thread = self
                    .load_persisted_thread_for_read(thread_id, /*include_turns*/ false)
                    .await?;
                self.load_live_thread_view(
                    thread_id,
                    include_turns,
                    loaded_thread,
                    persisted_thread,
                )
                .await?
            } else if let Some(thread) = self
                .load_persisted_thread_for_read(thread_id, include_turns)
                .await?
            {
                // Unloaded thread with turns: load metadata and history together
                // from the ThreadStore.
                thread
            } else {
                return Err(ThreadReadViewError::ClassifiedInvalidRequest {
                    message: format!("thread not loaded: {thread_id}"),
                    reason: ThreadErrorReason::NotLoaded,
                });
            }
        } else if let Some(thread) = self
            .load_persisted_thread_for_read(thread_id, include_turns)
            .await?
        {
            // Persisted metadata-only read: no live thread state is needed.
            thread
        } else if let Some(loaded_thread) = loaded_thread.as_ref() {
            // Loaded metadata-only read before persistence is materialized: build
            // the response from the live thread snapshot.
            self.load_live_thread_view(
                thread_id,
                include_turns,
                loaded_thread,
                /*persisted_thread*/ None,
            )
            .await?
        } else {
            return Err(ThreadReadViewError::ClassifiedInvalidRequest {
                message: format!("thread not loaded: {thread_id}"),
                reason: ThreadErrorReason::NotLoaded,
            });
        };

        let has_live_in_progress_turn = if let Some(loaded_thread) = loaded_thread.as_ref() {
            matches!(loaded_thread.agent_status().await, AgentStatus::Running)
        } else {
            false
        };

        let thread_status = self
            .thread_watch_manager
            .loaded_status_for_thread(&thread.id)
            .await;

        set_thread_status_and_interrupt_stale_turns(
            &mut thread,
            thread_status,
            has_live_in_progress_turn,
        );
        Ok(thread)
    }

    async fn load_persisted_thread_for_read(
        &self,
        thread_id: ThreadId,
        include_turns: bool,
    ) -> Result<Option<Thread>, ThreadReadViewError> {
        let fallback_provider = self.config.model_provider_id.as_str();
        match self
            .thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: include_turns,
            })
            .await
        {
            Ok(stored_thread) => {
                let (mut thread, history) =
                    thread_from_stored_thread(stored_thread, fallback_provider, &self.config.cwd);
                if include_turns && let Some(history) = history {
                    thread.turns = build_legacy_api_turns_from_rollout_items(&history.items);
                }
                Ok(Some(thread))
            }
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: missing_thread_id,
            }) if missing_thread_id == thread_id => Ok(None),
            Err(ThreadStoreError::InvalidRequest { message }) => {
                Err(ThreadReadViewError::InvalidRequest(message))
            }
            Err(ThreadStoreError::Unsupported { operation }) => {
                Err(ThreadReadViewError::Unsupported(operation))
            }
            Err(err) => Err(ThreadReadViewError::Internal(format!(
                "failed to read thread: {err}"
            ))),
        }
    }

    /// Builds a `thread/read` view from a loaded thread plus optional persisted metadata.
    async fn load_live_thread_view(
        &self,
        thread_id: ThreadId,
        include_turns: bool,
        loaded_thread: &CodexThread,
        persisted_thread: Option<Thread>,
    ) -> Result<Thread, ThreadReadViewError> {
        let config_snapshot = loaded_thread.config_snapshot().await;
        if include_turns && config_snapshot.ephemeral {
            return Err(ThreadReadViewError::ClassifiedInvalidRequest {
                message: "ephemeral threads do not support includeTurns".to_string(),
                reason: ThreadErrorReason::EphemeralTurnsUnavailable,
            });
        }
        let fallback_thread =
            build_thread_from_loaded_snapshot(thread_id, &config_snapshot, loaded_thread);
        let mut thread = if let Some(mut thread) = persisted_thread {
            if thread.path.is_none() {
                thread.path = fallback_thread.path.clone();
            }
            thread.session_id.clone_from(&fallback_thread.session_id);
            thread.ephemeral = fallback_thread.ephemeral;
            thread
        } else {
            fallback_thread
        };
        self.apply_thread_read_store_fields(thread_id, &mut thread, include_turns, loaded_thread)
            .await?;
        Ok(thread)
    }

    async fn apply_thread_read_store_fields(
        &self,
        thread_id: ThreadId,
        thread: &mut Thread,
        include_turns: bool,
        loaded_thread: &CodexThread,
    ) -> Result<(), ThreadReadViewError> {
        self.attach_thread_name(thread_id, thread).await;

        if include_turns {
            let history = loaded_thread
                .load_history(/*include_archived*/ true)
                .await
                .map_err(|err| thread_read_history_load_error(thread_id, err))?;
            thread.turns = build_legacy_api_turns_from_rollout_items(&history.items);
        }

        Ok(())
    }

    pub(crate) async fn thread_turns_list(
        &self,
        params: ThreadTurnsListParams,
    ) -> Result<ThreadTurnsListResponse, JSONRPCErrorError> {
        let ThreadTurnsListParams {
            thread_id,
            cursor,
            limit,
            sort_direction,
            items_view,
        } = params;
        let thread_uuid = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;

        let items = self
            .load_thread_list_history(thread_uuid, "thread/turns/list")
            .await
            .map_err(thread_read_view_error)?;
        // This API optimizes network transfer by letting clients page through a
        // thread's turns incrementally, but it still replays the entire rollout on
        // every request. Rollback and compaction events can change earlier turns, so
        // the server has to rebuild the full turn list until turn metadata is indexed
        // separately.
        let loaded_thread = self.thread_manager.get_thread(thread_uuid).await.ok();
        let has_live_running_thread = match loaded_thread.as_ref() {
            Some(thread) => matches!(thread.agent_status().await, AgentStatus::Running),
            None => false,
        };
        let active_turn = if loaded_thread.is_some() {
            // Persisted history may not yet include the currently running turn. The
            // app-server listener has already projected live turn events into ThreadState,
            // so merge that in-memory snapshot before paginating.
            let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
            let state = thread_state.lock().await;
            state.active_turn_snapshot()
        } else {
            None
        };
        build_thread_turns_page_response(
            &items,
            self.thread_watch_manager
                .loaded_status_for_thread(&thread_uuid.to_string())
                .await,
            has_live_running_thread,
            active_turn,
            ThreadTurnsPageOptions {
                cursor: cursor.as_deref(),
                limit,
                sort_direction: sort_direction.unwrap_or(SortDirection::Desc),
                items_view: items_view.unwrap_or(TurnItemsView::Summary),
            },
        )
    }

    pub(crate) async fn thread_items_list(
        &self,
        params: ThreadItemsListParams,
    ) -> Result<ThreadItemsListResponse, JSONRPCErrorError> {
        let ThreadItemsListParams {
            thread_id,
            turn_id,
            cursor,
            limit,
            sort_direction,
        } = params;
        let thread_id = ThreadId::from_string(&thread_id)
            .map_err(|err| invalid_request(format!("invalid thread id: {err}")))?;
        let page_size = thread_page_size(limit);
        let sort_direction = sort_direction.unwrap_or(SortDirection::Asc);
        let page = match self
            .thread_store
            .list_items(StoreListItemsParams {
                thread_id,
                turn_id: turn_id.clone(),
                include_archived: true,
                cursor: cursor.clone(),
                page_size,
                sort_direction: match sort_direction {
                    SortDirection::Asc => StoreSortDirection::Asc,
                    SortDirection::Desc => StoreSortDirection::Desc,
                },
            })
            .await
        {
            Ok(page) => page,
            Err(ThreadStoreError::Unsupported { .. }) => {
                // Legacy/local stores persist rollout history but do not maintain the
                // projected item index used by `list_items`. Rebuild the same API
                // projection from history so callers can page items without a migration.
                let history = self
                    .load_thread_list_history(thread_id, "thread/items/list")
                    .await
                    .map_err(thread_read_view_error)?;
                return paginate_reconstructed_thread_items(
                    reconstruct_thread_items(&history, turn_id.as_deref()),
                    cursor.as_deref(),
                    page_size,
                    sort_direction,
                );
            }
            Err(ThreadStoreError::InvalidRequest { message }) => {
                return Err(invalid_request(message));
            }
            Err(ThreadStoreError::ThreadNotFound { thread_id }) => {
                return Err(thread_invalid_request(
                    format!("no rollout found for thread id {thread_id}"),
                    ThreadErrorReason::NotFound,
                ));
            }
            Err(err) => {
                return Err(internal_error(format!(
                    "failed to list thread items: {err}"
                )));
            }
        };
        let data =
            page.items
                .into_iter()
                .map(|item| {
                    serde_json::from_slice::<ThreadItem>(&item.materialized_thread_item_json)
                        .map_err(|err| {
                            internal_error(format!(
                                "failed to deserialize stored thread item {}: {err}",
                                item.item_key
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;

        Ok(ThreadItemsListResponse {
            data,
            next_cursor: page.next_cursor,
            backwards_cursor: page.backwards_cursor,
        })
    }

    async fn load_thread_list_history(
        &self,
        thread_id: ThreadId,
        operation: &'static str,
    ) -> Result<Vec<RolloutItem>, ThreadReadViewError> {
        match self
            .thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: true,
            })
            .await
        {
            Ok(stored_thread) => {
                let history = stored_thread.history.ok_or_else(|| {
                    ThreadReadViewError::Internal(format!(
                        "thread store did not return history for thread {thread_id}"
                    ))
                })?;
                return Ok(history.items);
            }
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: missing_thread_id,
            }) if missing_thread_id == thread_id => {}
            Err(ThreadStoreError::InvalidRequest { message }) => {
                return Err(ThreadReadViewError::InvalidRequest(message));
            }
            Err(ThreadStoreError::Unsupported { operation }) => {
                return Err(ThreadReadViewError::Unsupported(operation));
            }
            Err(err) => {
                return Err(ThreadReadViewError::Internal(format!(
                    "failed to read thread: {err}"
                )));
            }
        }

        let thread = self
            .thread_manager
            .get_thread(thread_id)
            .await
            .map_err(|_| ThreadReadViewError::ClassifiedInvalidRequest {
                message: format!("thread not loaded: {thread_id}"),
                reason: ThreadErrorReason::NotLoaded,
            })?;
        let config_snapshot = thread.config_snapshot().await;
        if config_snapshot.ephemeral {
            return Err(ThreadReadViewError::ClassifiedInvalidRequest {
                message: format!("ephemeral threads do not support {operation}"),
                reason: ThreadErrorReason::EphemeralTurnsUnavailable,
            });
        }

        thread
            .load_history(/*include_archived*/ true)
            .await
            .map(|history| history.items)
            .map_err(|err| thread_list_history_load_error(thread_id, operation, err))
    }

    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        {
            let mut owners = self.desktop_activation_challenge_owners.lock().await;
            remove_desktop_activation_challenge_owners_for_connection(&mut owners, connection_id);
        }
        let thread_ids = self
            .thread_state_manager
            .remove_connection(connection_id)
            .await;

        for thread_id in thread_ids {
            if self.thread_manager.get_thread(thread_id).await.is_err() {
                // Reconcile stale app-server bookkeeping when the thread has already been
                // removed from the core manager.
                self.finalize_thread_teardown(thread_id).await;
            }
        }
    }

    /// Best-effort: ensure initialized connections are subscribed to this thread.
    pub(crate) async fn try_attach_thread_listener(
        &self,
        thread_id: ThreadId,
        connection_ids: Vec<ConnectionId>,
    ) {
        let Some(thread_guard) = self
            .thread_manager
            .acquire_thread_created_thread(thread_id)
            .await
        else {
            return;
        };
        let thread = Arc::clone(thread_guard.thread());
        self.attach_created_thread_listeners(thread_id, thread, &connection_ids)
            .await;
        drop(thread_guard);
    }

    /// Handles one result from the thread-created broadcast receiver.
    ///
    /// Returns whether the receiver should remain active. Keeping the complete
    /// event policy here ensures every transport uses the same lag recovery and
    /// closed-channel behavior.
    pub(crate) async fn handle_thread_created_event(
        &self,
        event: Result<ThreadId, tokio::sync::broadcast::error::RecvError>,
        connection_ids: Vec<ConnectionId>,
    ) -> bool {
        match event {
            Ok(thread_id) => {
                self.try_attach_thread_listener(thread_id, connection_ids)
                    .await;
                true
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(
                    skipped,
                    "thread_created receiver lagged; resyncing listeners"
                );
                self.resync_thread_listeners(connection_ids).await;
                true
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
        }
    }

    /// Reconciles loaded thread instances whose creation broadcasts were missed.
    pub(crate) async fn resync_thread_listeners(&self, connection_ids: Vec<ConnectionId>) {
        for thread_id in self.thread_manager.list_thread_created_ids().await {
            let Some(thread_guard) = self
                .thread_manager
                .acquire_thread_created_thread(thread_id)
                .await
            else {
                continue;
            };
            let thread = Arc::clone(thread_guard.thread());
            self.attach_created_thread_listeners(thread_id, thread, &connection_ids)
                .await;
            drop(thread_guard);
        }
    }

    #[cfg(test)]
    pub(crate) async fn thread_created_ids_for_test(&self) -> Vec<ThreadId> {
        self.thread_manager.list_thread_created_ids().await
    }

    async fn attach_created_thread_listeners(
        &self,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
        connection_ids: &[ConnectionId],
    ) {
        if connection_ids.is_empty() {
            return;
        }
        // Listener attachment is idempotent and must be retried for the current
        // connection set after receiver lag or a duplicate creation event.
        self.mark_thread_creation_handled(thread_id, &thread);
        self.attach_thread_listeners(thread_id, thread, connection_ids)
            .await;
    }

    async fn attach_thread_listeners(
        &self,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
        connection_ids: &[ConnectionId],
    ) {
        let config_snapshot = thread.config_snapshot().await;
        let loaded_thread = build_thread_from_snapshot(
            thread_id,
            thread.session_configured().session_id.to_string(),
            &config_snapshot,
            thread.rollout_path(),
        );
        self.thread_watch_manager.upsert_thread(loaded_thread).await;
        let raw_events_enabled = if let Some(parent_thread_id) = config_snapshot.parent_thread_id {
            self.thread_state_manager
                .thread_state(parent_thread_id)
                .await
                .lock()
                .await
                .experimental_raw_events
        } else {
            false
        };

        for connection_id in connection_ids {
            log_listener_attach_result(
                self.ensure_conversation_listener_for_instance(
                    thread_id,
                    Arc::clone(&thread),
                    *connection_id,
                    raw_events_enabled,
                )
                .await,
                thread_id,
                *connection_id,
                "thread",
            );
        }
    }

    pub(crate) async fn thread_resume(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadResumeParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<(), JSONRPCErrorError> {
        let parsed_thread_id = ParsedThreadId::parse(&params.thread_id);
        if let Some(thread_id) = parsed_thread_id.valid() {
            self.pending_thread_unloads
                .wait_until_finished(&thread_id)
                .await;
        }

        if params.sandbox.is_some() && params.permissions.is_some() {
            self.outgoing
                .send_error(
                    request_id,
                    invalid_request("`permissions` cannot be combined with `sandbox`"),
                )
                .await;
            return Ok(());
        }
        let redact_resume_payloads =
            should_redact_thread_resume_payloads(app_server_client_name.as_deref());

        let _thread_list_state_permit = match self.acquire_thread_list_state_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                self.outgoing.send_error(request_id, error).await;
                return Ok(());
            }
        };
        let stored_thread_from_running_probe = match self
            .resume_running_thread(
                &request_id,
                &params,
                &parsed_thread_id,
                app_server_client_name.clone(),
                app_server_client_version.clone(),
            )
            .await
        {
            Ok(RunningThreadResumeResult::Handled) => return Ok(()),
            Ok(RunningThreadResumeResult::NotRunning(stored_thread)) => stored_thread,
            Err(error) => {
                self.outgoing.send_error(request_id, error).await;
                return Ok(());
            }
        };

        let ThreadResumeParams {
            thread_id: _,
            history,
            path,
            model,
            model_provider,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            config: request_overrides,
            base_instructions,
            developer_instructions,
            personality,
            exclude_turns,
            initial_turns_page,
        } = params;
        let include_turns = !exclude_turns;

        let resume_result = if let Some(history) = history {
            self.resume_thread_from_history(history.as_slice())
                .await
                .map(|thread_history| (thread_history, None))
        } else if let Some(mut stored_thread) = stored_thread_from_running_probe {
            self.stored_thread_to_initial_history(&mut stored_thread)
                .await
                .map(|thread_history| (thread_history, Some(*stored_thread)))
        } else {
            self.resume_thread_from_rollout(&parsed_thread_id, path.as_ref())
                .await
                .map(|(thread_history, stored_thread)| (thread_history, Some(stored_thread)))
        };
        let (thread_history, resume_source_thread) = match resume_result {
            Ok(value) => value,
            Err(error) => {
                self.outgoing.send_error(request_id, error).await;
                return Ok(());
            }
        };

        let persisted_fallback = resume_source_thread
            .as_ref()
            .map(persisted_settings_fallback)
            .unwrap_or_default();
        let reduced_settings = reduce_persisted_thread_settings(
            thread_history.get_rollout_items(),
            persisted_fallback.clone(),
        );
        let history_cwd = reduced_settings
            .environments
            .as_ref()
            .map(|environments| environments.legacy_fallback_cwd.to_path_buf())
            .or_else(|| thread_history.session_cwd());
        let runtime_workspace_roots = runtime_workspace_roots.map(resolve_runtime_workspace_roots);
        let typesafe_overrides = self.build_thread_config_overrides(
            model,
            model_provider,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            base_instructions,
            developer_instructions,
            personality,
        );
        let explicit_overrides =
            persisted_settings_override_mask(request_overrides.as_ref(), &typesafe_overrides);

        // Derive a Config using the same logic as new conversation, honoring overrides if provided.
        let config = match self
            .config_manager
            .load_for_cwd(request_overrides, typesafe_overrides, history_cwd)
            .await
            .map_config_load_error()
        {
            Ok(config) => config,
            Err(error) => {
                self.outgoing.send_error(request_id, error).await;
                return Ok(());
            }
        };

        let response_history = thread_history.clone();

        match self
            .thread_manager
            .resume_thread_with_history_and_settings(
                config,
                thread_history,
                self.auth_manager.clone(),
                self.request_trace_context(&request_id).await,
                supports_openai_form_elicitation,
                codex_core::ThreadSettingsReconstruction {
                    fallback: persisted_fallback,
                    explicit_overrides,
                },
            )
            .await
        {
            Ok(NewThread {
                thread_id,
                thread: codex_thread,
                session_configured,
                was_already_running,
            }) => {
                if let Err(err) = Self::set_app_server_client_info(
                    codex_thread.as_ref(),
                    app_server_client_name,
                    app_server_client_version,
                )
                .await
                {
                    self.rollback_failed_resumed_thread(
                        thread_id,
                        &codex_thread,
                        was_already_running,
                    )
                    .await;
                    self.outgoing.send_error(request_id, err).await;
                    return Ok(());
                }
                let instruction_sources = codex_thread.legacy_instruction_sources().await;
                let SessionConfiguredEvent { rollout_path, .. } = session_configured;
                let Some(rollout_path) = rollout_path else {
                    let error =
                        internal_error(format!("rollout path missing for thread {thread_id}"));
                    self.rollback_failed_resumed_thread(
                        thread_id,
                        &codex_thread,
                        was_already_running,
                    )
                    .await;
                    self.outgoing.send_error(request_id, error).await;
                    return Ok(());
                };
                // Auto-attach a thread listener when resuming a thread.
                log_listener_attach_result(
                    self.ensure_conversation_listener(
                        thread_id,
                        request_id.connection_id,
                        /*raw_events_enabled*/ false,
                    )
                    .await,
                    thread_id,
                    request_id.connection_id,
                    "thread",
                );

                let (mut thread, token_usage_turn_id) = match self
                    .load_thread_from_resume_source_or_send_internal(
                        thread_id,
                        codex_thread.as_ref(),
                        &response_history,
                        rollout_path.as_path(),
                        resume_source_thread,
                        include_turns,
                    )
                    .await
                {
                    Ok(thread) => thread,
                    Err(message) => {
                        self.rollback_failed_resumed_thread(
                            thread_id,
                            &codex_thread,
                            was_already_running,
                        )
                        .await;
                        self.outgoing
                            .send_error(request_id, internal_error(message))
                            .await;
                        return Ok(());
                    }
                };
                thread.thread_source = codex_thread.config_snapshot().await.thread_source;

                set_thread_status_and_interrupt_stale_turns(
                    &mut thread,
                    ThreadStatus::Idle,
                    /*has_live_in_progress_turn*/ false,
                );
                self.thread_watch_manager
                    .upsert_thread(thread.clone())
                    .await;
                let config_snapshot = codex_thread.config_snapshot().await;
                let sandbox = thread_response_sandbox_policy(
                    &config_snapshot.permission_profile,
                    config_snapshot.cwd().as_path(),
                );
                let active_permission_profile = thread_response_active_permission_profile(
                    config_snapshot.active_permission_profile,
                );
                let mut initial_turns_page = if let Some(params) = initial_turns_page.as_ref() {
                    match build_thread_resume_initial_turns_page(
                        response_history.get_rollout_items(),
                        thread.status.clone(),
                        /*has_live_running_thread*/ false,
                        /*active_turn*/ None,
                        params,
                    ) {
                        Ok(page) => Some(page),
                        Err(error) => {
                            self.rollback_failed_resumed_thread(
                                thread_id,
                                &codex_thread,
                                was_already_running,
                            )
                            .await;
                            self.outgoing.send_error(request_id, error).await;
                            return Ok(());
                        }
                    }
                } else {
                    None
                };
                if redact_resume_payloads {
                    redact_thread_resume_payloads(&mut thread.turns);
                    if let Some(initial_turns_page) = initial_turns_page.as_mut() {
                        redact_thread_resume_payloads(&mut initial_turns_page.data);
                    }
                }

                let thread_originator = config_snapshot.originator.clone();
                let response = ThreadResumeResponse {
                    thread,
                    model: session_configured.model,
                    model_provider: session_configured.model_provider_id,
                    service_tier: session_configured.service_tier,
                    cwd: session_configured.cwd,
                    runtime_workspace_roots: config_snapshot.workspace_roots,
                    instruction_sources,
                    approval_policy: session_configured.approval_policy.into(),
                    approvals_reviewer: session_configured.approvals_reviewer.into(),
                    sandbox,
                    active_permission_profile,
                    reasoning_effort: session_configured.reasoning_effort,
                    initial_turns_page,
                };

                let connection_id = request_id.connection_id;
                self.outgoing
                    .send_response_with_thread_originator(request_id, response, thread_originator)
                    .await;
                // `excludeTurns` is explicitly the cheap resume path, so avoid
                // rebuilding history only to attribute a replayed usage update.
                if let Some(token_usage_turn_id) = token_usage_turn_id {
                    // The client needs restored usage before it starts another turn.
                    // Sending after the response preserves JSON-RPC request ordering while
                    // still filling the status line before the next turn lifecycle begins.
                    send_thread_token_usage_update_to_connection(
                        &self.outgoing,
                        connection_id,
                        thread_id,
                        codex_thread.as_ref(),
                        token_usage_turn_id,
                    )
                    .await;
                }
                self.thread_goal_processor
                    .emit_resume_goal_snapshot_and_continue(thread_id, codex_thread.as_ref())
                    .await;
            }
            Err(err) => {
                let error = internal_error(format!("error resuming thread: {err}"));
                self.outgoing.send_error(request_id, error).await;
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn resume_running_thread(
        &self,
        request_id: &ConnectionRequestId,
        params: &ThreadResumeParams,
        parsed_thread_id: &ParsedThreadId,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<RunningThreadResumeResult, JSONRPCErrorError> {
        let running_thread = if params.history.is_some() {
            if let Some(existing_thread_id) = parsed_thread_id.valid()
                && self
                    .thread_manager
                    .get_thread(existing_thread_id)
                    .await
                    .is_ok()
            {
                return Err(invalid_request(format!(
                    "cannot resume thread {existing_thread_id} with history while it is already running"
                )));
            }
            None
        } else if let Some(existing_thread_id) = parsed_thread_id.valid()
            && let Ok(existing_thread) = self.thread_manager.get_thread(existing_thread_id).await
        {
            let source_thread = self
                .read_stored_thread_for_resume(
                    parsed_thread_id,
                    /*path*/ None,
                    /*include_history*/ true,
                )
                .await?;
            Some((existing_thread_id, existing_thread, source_thread))
        } else {
            let source_thread = self
                .read_stored_thread_for_resume(
                    parsed_thread_id,
                    params.path.as_ref(),
                    /*include_history*/ true,
                )
                .await?;
            let existing_thread_id = source_thread.thread_id;
            match self.thread_manager.get_thread(existing_thread_id).await {
                Ok(existing_thread) => Some((existing_thread_id, existing_thread, source_thread)),
                Err(_) => {
                    return Ok(RunningThreadResumeResult::NotRunning(Some(Box::new(
                        source_thread,
                    ))));
                }
            }
        };

        if let Some((existing_thread_id, existing_thread, mut source_thread)) = running_thread {
            let existing_thread_rollout_path = existing_thread.rollout_path();
            let active_path = existing_thread_rollout_path
                .as_ref()
                .or(source_thread.rollout_path.as_ref());
            if let (Some(requested_path), Some(active_path)) = (params.path.as_ref(), active_path)
                && !path_utils::paths_match_after_normalization(requested_path, active_path)
            {
                return Err(invalid_request(format!(
                    "cannot resume running thread {existing_thread_id} with stale path: requested `{}`, active `{}`",
                    requested_path.display(),
                    active_path.display()
                )));
            }
            let config_snapshot = existing_thread.config_snapshot().await;
            let mismatch_details = collect_resume_override_mismatches(params, &config_snapshot);
            if !mismatch_details.is_empty() {
                let has_subscribers = !self
                    .thread_state_manager
                    .subscribed_connection_ids(existing_thread_id)
                    .await
                    .is_empty();
                let loaded_status = self
                    .thread_watch_manager
                    .loaded_status_for_thread(&existing_thread_id.to_string())
                    .await;
                let is_running =
                    matches!(existing_thread.agent_status().await, AgentStatus::Running);

                if !has_subscribers && matches!(loaded_status, ThreadStatus::Idle) && !is_running {
                    // A loaded idle thread is only a cache entry. Shut it down
                    // before removing it so cold resume cannot duplicate a
                    // thread that timed out during shutdown.
                    match shutdown_idle_thread_for_resume(
                        &self.thread_manager,
                        &self.outgoing,
                        &self.pending_thread_unloads,
                        &self.thread_state_manager,
                        &self.thread_watch_manager,
                        existing_thread_id,
                        Arc::clone(&existing_thread),
                    )
                    .await
                    {
                        IdleThreadShutdownResult::ReadyForColdResume => {
                            // Shutdown can flush newer rollout items, so reload the
                            // stored thread before starting the replacement session.
                            return Ok(RunningThreadResumeResult::NotRunning(None));
                        }
                        IdleThreadShutdownResult::Closing => {
                            return Err(invalid_request(format!(
                                "thread {existing_thread_id} is closing; retry after the thread is closed"
                            )));
                        }
                        IdleThreadShutdownResult::RejoinLoaded => {}
                    }
                }

                return Err(invalid_request(format!(
                    "cannot apply thread/resume overrides to loaded thread {existing_thread_id}: {}",
                    mismatch_details.join("; ")
                )));
            }
            let redact_resume_payloads =
                should_redact_thread_resume_payloads(app_server_client_name.as_deref());
            let history_items = source_thread
                .history
                .take()
                .map(|history| history.items)
                .ok_or_else(|| {
                    internal_error(format!(
                        "thread {existing_thread_id} did not include persisted history"
                    ))
                })?;

            let thread_state = self
                .thread_state_manager
                .thread_state(existing_thread_id)
                .await;
            self.ensure_listener_task_running(
                existing_thread_id,
                existing_thread.clone(),
                thread_state.clone(),
            )
            .await?;
            Self::set_app_server_client_info(
                existing_thread.as_ref(),
                app_server_client_name,
                app_server_client_version,
            )
            .await?;

            let mut thread_summary = self.stored_thread_to_api_thread(
                source_thread,
                config_snapshot.model_provider_id.as_str(),
                /*include_turns*/ false,
            );
            thread_summary.session_id = existing_thread.session_configured().session_id.to_string();
            let instruction_sources = existing_thread.legacy_instruction_sources().await;

            let listener_command_tx = {
                let thread_state = thread_state.lock().await;
                thread_state.listener_command_tx()
            };
            let Some(listener_command_tx) = listener_command_tx else {
                return Err(internal_error(format!(
                    "failed to enqueue running thread resume for thread {existing_thread_id}: thread listener is not running"
                )));
            };

            let (emit_thread_goal_update, thread_goal_state_db) = self
                .thread_goal_processor
                .pending_resume_goal_state(existing_thread.as_ref())
                .await;

            let command = crate::thread_state::ThreadListenerCommand::SendThreadResumeResponse(
                Box::new(crate::thread_state::PendingThreadResumeRequest {
                    request_id: request_id.clone(),
                    history_items,
                    config_snapshot,
                    instruction_sources,
                    thread_summary,
                    emit_thread_goal_update,
                    thread_goal_state_db,
                    include_turns: !params.exclude_turns,
                    initial_turns_page: params.initial_turns_page.clone(),
                    redact_resume_payloads,
                }),
            );
            if listener_command_tx.send(command).is_err() {
                return Err(internal_error(format!(
                    "failed to enqueue running thread resume for thread {existing_thread_id}: thread listener command channel is closed"
                )));
            }
            return Ok(RunningThreadResumeResult::Handled);
        }
        Ok(RunningThreadResumeResult::NotRunning(None))
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn resume_thread_from_history(
        &self,
        history: &[ResponseItem],
    ) -> Result<InitialHistory, JSONRPCErrorError> {
        if history.is_empty() {
            return Err(invalid_request("history must not be empty"));
        }
        Ok(InitialHistory::Forked(
            history
                .iter()
                .cloned()
                .map(RolloutItem::ResponseItem)
                .collect(),
        ))
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn resume_thread_from_rollout(
        &self,
        thread_id: &ParsedThreadId,
        path: Option<&PathBuf>,
    ) -> Result<(InitialHistory, StoredThread), JSONRPCErrorError> {
        let mut stored_thread = self
            .read_stored_thread_for_resume(thread_id, path, /*include_history*/ true)
            .await?;
        let history = self
            .stored_thread_to_initial_history(&mut stored_thread)
            .await?;
        Ok((history, stored_thread))
    }

    async fn read_stored_thread_for_resume(
        &self,
        thread_id: &ParsedThreadId,
        path: Option<&PathBuf>,
        include_history: bool,
    ) -> Result<StoredThread, JSONRPCErrorError> {
        let result = if let Some(path) = path {
            self.thread_store
                .read_thread_by_rollout_path(StoreReadThreadByRolloutPathParams {
                    rollout_path: path.clone(),
                    include_archived: true,
                    include_history,
                })
                .await
        } else {
            let existing_thread_id = thread_id.required()?;
            let params = StoreReadThreadParams {
                thread_id: existing_thread_id,
                include_archived: true,
                include_history,
            };
            self.thread_store.read_thread(params).await
        };

        let stored_thread = result.map_err(thread_store_resume_read_error)?;
        if stored_thread.archived_at.is_some() {
            let thread_id = stored_thread.thread_id;
            return Err(invalid_request(format!(
                "session {thread_id} is archived. Run `codex unarchive {thread_id}` to unarchive it first."
            )));
        }

        Ok(stored_thread)
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn stored_thread_to_initial_history(
        &self,
        stored_thread: &mut StoredThread,
    ) -> Result<InitialHistory, JSONRPCErrorError> {
        let thread_id = stored_thread.thread_id;
        let history = stored_thread
            .history
            .take()
            .map(|history| history.items)
            .ok_or_else(|| {
                internal_error(format!(
                    "thread {thread_id} did not include persisted history"
                ))
            })?;
        Ok(InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(history),
            rollout_path: stored_thread.rollout_path.clone(),
        }))
    }

    fn stored_thread_to_api_thread(
        &self,
        stored_thread: StoredThread,
        fallback_provider: &str,
        include_turns: bool,
    ) -> Thread {
        self.stored_thread_to_api_thread_with_token_usage(
            stored_thread,
            fallback_provider,
            include_turns,
        )
        .0
    }

    fn stored_thread_to_api_thread_with_token_usage(
        &self,
        stored_thread: StoredThread,
        fallback_provider: &str,
        include_turns: bool,
    ) -> (Thread, Option<String>) {
        let (mut thread, history) =
            thread_from_stored_thread(stored_thread, fallback_provider, &self.config.cwd);
        let token_usage_turn_id = include_turns.then(|| {
            let items = history
                .as_ref()
                .map(|history| history.items.as_slice())
                .unwrap_or_default();
            super::thread_lifecycle::populate_thread_turns_from_history_with_token_usage(
                &mut thread,
                items,
                /*active_turn*/ None,
            )
        });
        (thread, token_usage_turn_id)
    }

    async fn read_stored_thread_for_new_fork(
        &self,
        thread_id: ThreadId,
        include_history: bool,
    ) -> Result<StoredThread, JSONRPCErrorError> {
        self.thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: true,
                include_history,
            })
            .await
            .map_err(thread_store_resume_read_error)
    }

    async fn load_thread_from_resume_source_or_send_internal(
        &self,
        thread_id: ThreadId,
        thread: &CodexThread,
        thread_history: &InitialHistory,
        rollout_path: &Path,
        resume_source_thread: Option<StoredThread>,
        include_turns: bool,
    ) -> std::result::Result<(Thread, Option<String>), String> {
        let config_snapshot = thread.config_snapshot().await;
        let session_id = thread.session_configured().session_id.to_string();
        let thread = match thread_history {
            InitialHistory::Resumed(resumed) => {
                let fallback_provider = config_snapshot.model_provider_id.as_str();
                if let Some(stored_thread) = resume_source_thread {
                    let source_updated_at = stored_thread.updated_at;
                    let source_recency_at = stored_thread.recency_at;
                    let mut stored_thread =
                        if let Some(rollout_path) = stored_thread.rollout_path.clone() {
                            self.thread_store
                                .read_thread_by_rollout_path(StoreReadThreadByRolloutPathParams {
                                    rollout_path,
                                    include_archived: true,
                                    include_history: false,
                                })
                                .await
                                .unwrap_or(StoredThread {
                                    history: None,
                                    ..stored_thread
                                })
                        } else {
                            self.thread_store
                                .read_thread(StoreReadThreadParams {
                                    thread_id: stored_thread.thread_id,
                                    include_archived: true,
                                    include_history: false,
                                })
                                .await
                                .unwrap_or(StoredThread {
                                    history: None,
                                    ..stored_thread
                                })
                        };
                    // Starting the resumed runtime can refresh the store before the response is
                    // assembled. Resume itself must not count as user activity, so preserve the
                    // timestamps observed by the initial read.
                    stored_thread.updated_at = source_updated_at;
                    stored_thread.recency_at = source_recency_at;
                    Ok(thread_from_stored_thread(
                        stored_thread,
                        fallback_provider,
                        &self.config.cwd,
                    )
                    .0)
                } else {
                    match self
                        .thread_store
                        .read_thread(StoreReadThreadParams {
                            thread_id: resumed.conversation_id,
                            include_archived: true,
                            include_history: false,
                        })
                        .await
                    {
                        Ok(stored_thread) => Ok(thread_from_stored_thread(
                            stored_thread,
                            fallback_provider,
                            &self.config.cwd,
                        )
                        .0),
                        Err(read_err) => {
                            Err(format!("failed to read thread from store: {read_err}"))
                        }
                    }
                }
            }
            InitialHistory::Forked(items) => {
                let mut thread = build_thread_from_snapshot(
                    thread_id,
                    session_id.clone(),
                    &config_snapshot,
                    Some(rollout_path.into()),
                );
                thread.preview = preview_from_rollout_items(items);
                Ok(thread)
            }
            InitialHistory::New | InitialHistory::Cleared => Err(format!(
                "failed to build resume response for thread {thread_id}: initial history missing"
            )),
        };
        let mut thread = thread?;
        thread.id = thread_id.to_string();
        thread.session_id = session_id;
        thread.path = Some(rollout_path.to_path_buf());
        let token_usage_turn_id = if include_turns {
            let history_items = thread_history.get_rollout_items();
            Some(
                super::thread_lifecycle::populate_thread_turns_from_history_with_token_usage(
                    &mut thread,
                    history_items,
                    /*active_turn*/ None,
                ),
            )
        } else {
            None
        };
        self.attach_thread_name(thread_id, &mut thread).await;
        Ok((thread, token_usage_turn_id))
    }

    async fn attach_thread_name(&self, thread_id: ThreadId, thread: &mut Thread) {
        if let Ok(stored_thread) = self
            .thread_store
            .read_thread(StoreReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
            && let Some(title) = stored_thread.name.as_deref().map(str::trim)
            && !title.is_empty()
            && stored_thread.preview.trim() != title
        {
            set_thread_name_from_title(thread, title.to_string());
        }
    }

    pub(crate) async fn thread_fork(
        &self,
        request_id: ConnectionRequestId,
        params: ThreadForkParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<(), JSONRPCErrorError> {
        let ThreadForkParams {
            thread_id,
            last_turn_id,
            path,
            model,
            model_provider,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            config: cli_overrides,
            base_instructions,
            developer_instructions,
            ephemeral,
            thread_source,
            exclude_turns,
        } = params;
        let include_turns = !exclude_turns;
        let parsed_thread_id = ParsedThreadId::parse(&thread_id);
        if sandbox.is_some() && permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandbox`",
            ));
        }
        let mut source_thread = self
            .read_stored_thread_for_resume(
                &parsed_thread_id,
                path.as_ref(),
                /*include_history*/ true,
            )
            .await?;
        let source_thread_id = source_thread.thread_id;
        let use_head_metadata_fallback = last_turn_id.is_none();
        let source_thread_name = source_thread
            .name
            .as_deref()
            .and_then(codex_thread_store::normalize_thread_name);
        let history_items = source_thread
            .history
            .take()
            .map(|history| history.items)
            .ok_or_else(|| {
                internal_error(format!(
                    "thread {source_thread_id} did not include persisted history"
                ))
            })?;
        let history_items = if let Some(last_turn_id) = last_turn_id.as_deref() {
            Arc::new(
                truncate_rollout_after_turn_id(&history_items, last_turn_id)
                    .map_err(|err| core_thread_write_error("truncate thread for fork", err))?,
            )
        } else {
            Arc::new(history_items)
        };
        let persisted_fallback = if use_head_metadata_fallback {
            persisted_settings_fallback(&source_thread)
        } else {
            Default::default()
        };
        let reduced_settings =
            reduce_persisted_thread_settings(&history_items, persisted_fallback.clone());
        let history_cwd = reduced_settings
            .environments
            .as_ref()
            .map(|environments| environments.legacy_fallback_cwd.to_path_buf())
            .or_else(|| use_head_metadata_fallback.then(|| source_thread.cwd.clone()));

        let request_overrides = cli_overrides;
        let runtime_workspace_roots = runtime_workspace_roots.map(resolve_runtime_workspace_roots);
        let mut typesafe_overrides = self.build_thread_config_overrides(
            model,
            model_provider,
            service_tier,
            cwd,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox,
            permissions,
            base_instructions,
            developer_instructions,
            /*personality*/ None,
        );
        typesafe_overrides.ephemeral = ephemeral.then_some(true);
        let explicit_overrides =
            persisted_settings_override_mask(request_overrides.as_ref(), &typesafe_overrides);
        // Derive a Config using the same logic as new conversation, honoring overrides if provided.
        let config = self
            .config_manager
            .load_for_cwd(request_overrides, typesafe_overrides, history_cwd)
            .await
            .map_config_load_error()?;

        let fallback_model_provider = config.model_provider_id.clone();

        let NewThread {
            thread_id,
            thread: forked_thread,
            session_configured,
            ..
        } = self
            .thread_manager
            .fork_thread_from_history_with_settings(
                ForkSnapshot::Interrupted,
                config,
                InitialHistory::Resumed(ResumedHistory {
                    conversation_id: source_thread_id,
                    history: Arc::clone(&history_items),
                    rollout_path: source_thread.rollout_path.clone(),
                }),
                thread_source,
                self.request_trace_context(&request_id).await,
                supports_openai_form_elicitation,
                codex_core::ThreadSettingsReconstruction {
                    fallback: persisted_fallback,
                    explicit_overrides,
                },
            )
            .await
            .map_err(|err| match err {
                CodexErr::Io(_) | CodexErr::Json(_) => {
                    invalid_request(format!("failed to load thread {source_thread_id}: {err}"))
                }
                CodexErr::InvalidRequest(message) => invalid_request(message),
                err => internal_error(format!("error forking thread: {err}")),
            })?;

        let fork_setup_result = async {
            Self::set_app_server_client_info(
                forked_thread.as_ref(),
                app_server_client_name,
                app_server_client_version,
            )
            .await?;
            if session_configured.rollout_path.is_some()
                && let Some(name) = source_thread_name.clone()
            {
                self.thread_manager
                    .update_thread_metadata(
                        thread_id,
                        StoreThreadMetadataPatch {
                            name: Some(Some(name)),
                            ..Default::default()
                        },
                        /*include_archived*/ true,
                    )
                    .await
                    .map_err(|err| core_thread_write_error("inherit source thread name", err))?;
            }

            let instruction_sources = forked_thread.legacy_instruction_sources().await;

            // Persistent forks materialize their own rollout immediately. Ephemeral forks stay
            // pathless, so they rebuild their visible history from the copied source history
            // instead.
            let (thread, token_usage_turn_id, config_snapshot) =
                if session_configured.rollout_path.is_some() {
                    let stored_thread = self
                        .read_stored_thread_for_new_fork(thread_id, include_turns)
                        .await?;
                    let (thread, token_usage_turn_id) = self
                        .stored_thread_to_api_thread_with_token_usage(
                            stored_thread,
                            fallback_model_provider.as_str(),
                            include_turns,
                        );
                    (thread, token_usage_turn_id, None)
                } else {
                    let config_snapshot = forked_thread.config_snapshot().await;
                    let mut thread = build_thread_from_snapshot(
                        thread_id,
                        session_configured.session_id.to_string(),
                        &config_snapshot,
                        /*path*/ None,
                    );
                    thread.preview = preview_from_rollout_items(&history_items);
                    thread.forked_from_id = Some(source_thread_id.to_string());
                    let token_usage_turn_id = include_turns.then(|| {
                    super::thread_lifecycle::populate_thread_turns_from_history_with_token_usage(
                        &mut thread,
                        &history_items,
                        /*active_turn*/ None,
                    )
                });
                    (thread, token_usage_turn_id, Some(config_snapshot))
                };
            Ok::<_, JSONRPCErrorError>((
                instruction_sources,
                thread,
                token_usage_turn_id,
                config_snapshot,
            ))
        }
        .await;
        let (instruction_sources, mut thread, token_usage_turn_id, config_snapshot) =
            match fork_setup_result {
                Ok(result) => result,
                Err(err) => {
                    let rollback_succeeded = self
                        .thread_manager
                        .rollback_thread_spawn(thread_id, &forked_thread)
                        .await;
                    let thread_id_still_loaded = !rollback_succeeded
                        && self.thread_manager.get_thread(thread_id).await.is_ok();
                    if should_finalize_failed_thread_setup(
                        rollback_succeeded,
                        thread_id_still_loaded,
                    ) {
                        self.finalize_thread_teardown(thread_id).await;
                        if let Some(state_db) = self.state_db.as_ref()
                            && let Err(cleanup_err) = state_db.delete_thread(thread_id).await
                        {
                            warn!(
                                "failed to remove app-server state for rolled-back fork {thread_id}: \
                             {cleanup_err}"
                            );
                        }
                    } else {
                        warn!(
                            "skipping app-server cleanup for failed fork {thread_id}: \
                         a different thread instance is loaded under that id"
                        );
                    }
                    return Err(err);
                }
            };
        let config_snapshot =
            reuse_or_capture_fork_snapshot(config_snapshot, || forked_thread.config_snapshot())
                .await;
        if let Some(name) = source_thread_name {
            set_thread_name_from_title(&mut thread, name);
        }
        thread.session_id = session_configured.session_id.to_string();
        thread.thread_source = config_snapshot.thread_source.clone();

        // Auto-attach a conversation listener only after all fallible fork setup has completed.
        log_listener_attach_result(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await,
            thread_id,
            request_id.connection_id,
            "thread",
        );

        self.thread_watch_manager
            .upsert_thread_silently(thread.clone())
            .await;

        thread.status = resolve_thread_status(
            self.thread_watch_manager
                .loaded_status_for_thread(&thread.id)
                .await,
            /*has_in_progress_turn*/ false,
        );
        let sandbox = thread_response_sandbox_policy(
            &config_snapshot.permission_profile,
            config_snapshot.cwd().as_path(),
        );
        let active_permission_profile =
            thread_response_active_permission_profile(config_snapshot.active_permission_profile);
        let thread_originator = config_snapshot.originator.clone();

        let response = ThreadForkResponse {
            thread: thread.clone(),
            model: session_configured.model,
            model_provider: session_configured.model_provider_id,
            service_tier: session_configured.service_tier,
            cwd: session_configured.cwd,
            runtime_workspace_roots: config_snapshot.workspace_roots,
            instruction_sources,
            approval_policy: session_configured.approval_policy.into(),
            approvals_reviewer: session_configured.approvals_reviewer.into(),
            sandbox,
            active_permission_profile,
            reasoning_effort: session_configured.reasoning_effort,
        };

        let notif = thread_started_notification(thread);
        let connection_id = request_id.connection_id;
        self.outgoing
            .send_response_with_thread_originator(request_id, response, thread_originator)
            .await;
        // `excludeTurns` is the cheap fork path, so skip restored usage replay
        // instead of rebuilding history only to attribute a historical update.
        if let Some(token_usage_turn_id) = token_usage_turn_id {
            // Mirror the resume contract for forks: the new thread is usable as soon
            // as the response arrives, so restored usage must follow immediately.
            send_thread_token_usage_update_to_connection(
                &self.outgoing,
                connection_id,
                thread_id,
                forked_thread.as_ref(),
                token_usage_turn_id,
            )
            .await;
        }

        self.outgoing
            .send_server_notification(ServerNotification::ThreadStarted(notif))
            .await;
        Ok(())
    }

    pub(crate) async fn conversation_summary(
        &self,
        params: GetConversationSummaryParams,
    ) -> Result<GetConversationSummaryResponse, JSONRPCErrorError> {
        let fallback_provider = self.config.model_provider_id.as_str();
        let read_result = match params {
            GetConversationSummaryParams::ThreadId { conversation_id } => self
                .thread_store
                .read_thread(StoreReadThreadParams {
                    thread_id: conversation_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                .map_err(|err| conversation_summary_thread_id_read_error(conversation_id, err)),
            GetConversationSummaryParams::RolloutPath { rollout_path } => {
                let Some(local_thread_store) = self
                    .thread_store
                    .as_any()
                    .downcast_ref::<LocalThreadStore>()
                else {
                    return Err(invalid_request(
                        "rollout path queries are only supported with the local thread store",
                    ));
                };

                local_thread_store
                    .read_thread_by_rollout_path(
                        rollout_path.clone(),
                        /*include_archived*/ true,
                        /*include_history*/ false,
                    )
                    .await
                    .map_err(|err| conversation_summary_rollout_path_read_error(&rollout_path, err))
            }
        };

        let stored_thread = read_result?;
        let summary = summary_from_stored_thread(stored_thread, fallback_provider);
        Ok(GetConversationSummaryResponse { summary })
    }

    async fn list_threads_common(
        &self,
        requested_page_size: usize,
        cursor: Option<String>,
        sort_key: StoreThreadSortKey,
        sort_direction: SortDirection,
        filters: ThreadListFilters,
    ) -> Result<(Vec<StoredThread>, Option<String>, Option<String>), JSONRPCErrorError> {
        let ThreadListFilters {
            model_providers,
            source_kinds,
            archived,
            cwd_filters,
            search_term,
            use_state_db_only,
            relation_filter,
        } = filters;
        let mut cursor_obj = cursor;
        let mut last_cursor = cursor_obj.clone();
        let mut remaining = requested_page_size;
        let mut items = Vec::with_capacity(requested_page_size);
        let mut next_cursor: Option<String> = None;
        let mut backwards_cursor: Option<String> = None;

        let model_provider_filter = match model_providers {
            Some(providers) => {
                if providers.is_empty() {
                    None
                } else {
                    Some(providers)
                }
            }
            None if relation_filter.is_some() => None,
            None => Some(vec![self.config.model_provider_id.clone()]),
        };
        let (allowed_sources_vec, source_kind_filter) =
            if relation_filter.is_some() && source_kinds.is_none() {
                (Vec::new(), None)
            } else {
                compute_source_filters(source_kinds)
            };
        let allowed_sources = allowed_sources_vec.as_slice();
        let store_sort_direction = thread_store_sort_direction(sort_direction);

        while remaining > 0 {
            let page_size = remaining.min(THREAD_PAGE_MAX_LIMIT);
            let page = self
                .thread_store
                .list_threads(StoreListThreadsParams {
                    page_size,
                    cursor: cursor_obj.clone(),
                    sort_key,
                    sort_direction: store_sort_direction,
                    allowed_sources: allowed_sources.to_vec(),
                    model_providers: model_provider_filter.clone(),
                    cwd_filters: cwd_filters.clone(),
                    archived,
                    search_term: search_term.clone(),
                    storage_mode: match use_state_db_only {
                        None => StoreThreadListStorageMode::PreferStateDb,
                        Some(true) => StoreThreadListStorageMode::StateDbOnly,
                        Some(false) => StoreThreadListStorageMode::ScanAndRepair,
                    },
                    relation_filter,
                })
                .await
                .map_err(thread_store_list_error)?;

            let mut filtered = Vec::with_capacity(page.items.len());
            for it in page.items {
                let source = with_thread_spawn_agent_metadata(
                    it.source.clone(),
                    it.agent_nickname.clone(),
                    it.agent_role.clone(),
                );
                if source_kind_filter
                    .as_ref()
                    .is_none_or(|filter| source_kind_matches(&source, filter))
                {
                    filtered.push(it);
                    if filtered.len() >= remaining {
                        break;
                    }
                }
            }
            items.extend(filtered);
            if backwards_cursor.is_none() && !items.is_empty() {
                backwards_cursor = page.backwards_cursor;
            }
            remaining = requested_page_size.saturating_sub(items.len());

            next_cursor = page.next_cursor;
            if remaining == 0 {
                break;
            }

            let Some(cursor_val) = next_cursor.clone() else {
                break;
            };
            // Break if our pagination would reuse the same cursor again; this avoids
            // an infinite loop when filtering drops everything on the page.
            if last_cursor.as_ref() == Some(&cursor_val) {
                next_cursor = None;
                break;
            }
            last_cursor = Some(cursor_val.clone());
            cursor_obj = Some(cursor_val);
        }

        Ok((items, next_cursor, backwards_cursor))
    }
}

const MCP_ELICITATIONS_AUTO_DENY: bool = false;

struct ReconstructedThreadItem {
    turn_id: String,
    item: ThreadItem,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReconstructedThreadItemsCursor {
    turn_id: String,
    item_id: String,
    include_anchor: bool,
}

fn reconstruct_thread_items(
    rollout_items: &[RolloutItem],
    turn_id_filter: Option<&str>,
) -> Vec<ReconstructedThreadItem> {
    build_legacy_api_turns_from_rollout_items(rollout_items)
        .into_iter()
        .filter(|turn| turn_id_filter.is_none_or(|turn_id| turn.id == turn_id))
        .flat_map(|turn| {
            let turn_id = turn.id;
            turn.items
                .into_iter()
                .map(move |item| ReconstructedThreadItem {
                    turn_id: turn_id.clone(),
                    item,
                })
        })
        .collect()
}

fn paginate_reconstructed_thread_items(
    items: Vec<ReconstructedThreadItem>,
    cursor: Option<&str>,
    page_size: usize,
    sort_direction: SortDirection,
) -> Result<ThreadItemsListResponse, JSONRPCErrorError> {
    let anchor = cursor
        .map(parse_reconstructed_thread_items_cursor)
        .transpose()?;
    let anchor_index = anchor.as_ref().and_then(|anchor| {
        items
            .iter()
            .position(|item| item.turn_id == anchor.turn_id && item.item.id() == anchor.item_id)
    });
    if anchor.is_some() && anchor_index.is_none() {
        return Err(invalid_request(
            "invalid cursor: anchor item is no longer present",
        ));
    }

    let mut keyed_items: Vec<_> = items.into_iter().enumerate().collect();
    match sort_direction {
        SortDirection::Asc => {
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_items.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index >= anchor_index
                    } else {
                        *index > anchor_index
                    }
                });
            }
        }
        SortDirection::Desc => {
            keyed_items.reverse();
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_items.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index <= anchor_index
                    } else {
                        *index < anchor_index
                    }
                });
            }
        }
    }

    let more_items_available = keyed_items.len() > page_size;
    keyed_items.truncate(page_size);
    let backwards_cursor = keyed_items
        .first()
        .map(|(_, item)| {
            serialize_reconstructed_thread_items_cursor(item, /*include_anchor*/ true)
        })
        .transpose()?;
    let next_cursor = if more_items_available {
        keyed_items
            .last()
            .map(|(_, item)| {
                serialize_reconstructed_thread_items_cursor(item, /*include_anchor*/ false)
            })
            .transpose()?
    } else {
        None
    };
    let data = keyed_items.into_iter().map(|(_, item)| item.item).collect();

    Ok(ThreadItemsListResponse {
        data,
        next_cursor,
        backwards_cursor,
    })
}

fn serialize_reconstructed_thread_items_cursor(
    item: &ReconstructedThreadItem,
    include_anchor: bool,
) -> Result<String, JSONRPCErrorError> {
    serde_json::to_string(&ReconstructedThreadItemsCursor {
        turn_id: item.turn_id.clone(),
        item_id: item.item.id().to_string(),
        include_anchor,
    })
    .map_err(|err| internal_error(format!("failed to serialize cursor: {err}")))
}

fn parse_reconstructed_thread_items_cursor(
    cursor: &str,
) -> Result<ReconstructedThreadItemsCursor, JSONRPCErrorError> {
    serde_json::from_str(cursor).map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))
}

fn thread_backwards_cursor_for_sort_key(
    thread: &StoredThread,
    sort_key: StoreThreadSortKey,
    sort_direction: SortDirection,
) -> Option<String> {
    let timestamp = match sort_key {
        StoreThreadSortKey::CreatedAt => thread.created_at,
        StoreThreadSortKey::UpdatedAt => thread.updated_at,
        StoreThreadSortKey::RecencyAt => thread.recency_at,
    };
    // The state DB stores unique millisecond timestamps. Offset the reverse cursor by one
    // millisecond so the opposite-direction query includes the page anchor.
    let timestamp = match sort_direction {
        SortDirection::Asc => timestamp.checked_add_signed(chrono::Duration::milliseconds(1))?,
        SortDirection::Desc => timestamp.checked_sub_signed(chrono::Duration::milliseconds(1))?,
    };
    Some(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

struct ThreadTurnsPage {
    pub(super) turns: Vec<Turn>,
    pub(super) next_cursor: Option<String>,
    pub(super) backwards_cursor: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadTurnsCursor {
    turn_id: String,
    include_anchor: bool,
}

fn paginate_thread_turns(
    turns: Vec<Turn>,
    cursor: Option<&str>,
    limit: Option<u32>,
    sort_direction: SortDirection,
) -> Result<ThreadTurnsPage, JSONRPCErrorError> {
    let anchor = cursor.map(parse_thread_turns_cursor).transpose()?;
    let page_size = thread_page_size(limit);

    let anchor_index = anchor
        .as_ref()
        .and_then(|anchor| turns.iter().position(|turn| turn.id == anchor.turn_id));
    if anchor.is_some() && anchor_index.is_none() {
        return Err(invalid_request(
            "invalid cursor: anchor turn is no longer present",
        ));
    }

    let mut keyed_turns: Vec<_> = turns.into_iter().enumerate().collect();
    match sort_direction {
        SortDirection::Asc => {
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_turns.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index >= anchor_index
                    } else {
                        *index > anchor_index
                    }
                });
            }
        }
        SortDirection::Desc => {
            keyed_turns.reverse();
            if let (Some(anchor), Some(anchor_index)) = (anchor.as_ref(), anchor_index) {
                keyed_turns.retain(|(index, _)| {
                    if anchor.include_anchor {
                        *index <= anchor_index
                    } else {
                        *index < anchor_index
                    }
                });
            }
        }
    }

    let more_turns_available = keyed_turns.len() > page_size;
    keyed_turns.truncate(page_size);
    let backwards_cursor = keyed_turns
        .first()
        .map(|(_, turn)| serialize_thread_turns_cursor(&turn.id, /*include_anchor*/ true))
        .transpose()?;
    let next_cursor = if more_turns_available {
        keyed_turns
            .last()
            .map(|(_, turn)| serialize_thread_turns_cursor(&turn.id, /*include_anchor*/ false))
            .transpose()?
    } else {
        None
    };
    let turns = keyed_turns.into_iter().map(|(_, turn)| turn).collect();

    Ok(ThreadTurnsPage {
        turns,
        next_cursor,
        backwards_cursor,
    })
}

fn serialize_thread_turns_cursor(
    turn_id: &str,
    include_anchor: bool,
) -> Result<String, JSONRPCErrorError> {
    serde_json::to_string(&ThreadTurnsCursor {
        turn_id: turn_id.to_string(),
        include_anchor,
    })
    .map_err(|err| internal_error(format!("failed to serialize cursor: {err}")))
}

fn parse_thread_turns_cursor(cursor: &str) -> Result<ThreadTurnsCursor, JSONRPCErrorError> {
    serde_json::from_str(cursor).map_err(|_| invalid_request(format!("invalid cursor: {cursor}")))
}

struct ThreadTurnsPageOptions<'a> {
    cursor: Option<&'a str>,
    limit: Option<u32>,
    sort_direction: SortDirection,
    items_view: TurnItemsView,
}

fn build_thread_turns_page_response(
    items: &[RolloutItem],
    loaded_status: ThreadStatus,
    has_live_running_thread: bool,
    active_turn: Option<Turn>,
    options: ThreadTurnsPageOptions<'_>,
) -> Result<ThreadTurnsListResponse, JSONRPCErrorError> {
    let mut turns = reconstruct_thread_turns_for_turns_list(
        items,
        loaded_status,
        has_live_running_thread,
        active_turn,
    );
    apply_thread_turns_items_view(&mut turns, options.items_view);
    let page = paginate_thread_turns(turns, options.cursor, options.limit, options.sort_direction)?;
    Ok(ThreadTurnsListResponse {
        data: page.turns,
        next_cursor: page.next_cursor,
        backwards_cursor: page.backwards_cursor,
    })
}

pub(super) fn build_thread_resume_initial_turns_page(
    items: &[RolloutItem],
    loaded_status: ThreadStatus,
    has_live_running_thread: bool,
    active_turn: Option<Turn>,
    params: &ThreadResumeInitialTurnsPageParams,
) -> Result<codex_app_server_protocol::TurnsPage, JSONRPCErrorError> {
    build_thread_turns_page_response(
        items,
        loaded_status,
        has_live_running_thread,
        active_turn,
        ThreadTurnsPageOptions {
            cursor: None,
            limit: params.limit,
            sort_direction: params.sort_direction.unwrap_or(SortDirection::Desc),
            items_view: params.items_view.unwrap_or(TurnItemsView::Summary),
        },
    )
    .map(Into::into)
}

fn apply_thread_turns_items_view(turns: &mut [Turn], items_view: TurnItemsView) {
    for turn in turns {
        match items_view {
            TurnItemsView::NotLoaded => {
                turn.items.clear();
                turn.items_view = TurnItemsView::NotLoaded;
            }
            TurnItemsView::Summary => {
                let first_user_message = turn
                    .items
                    .iter()
                    .find(|item| matches!(item, ThreadItem::UserMessage { .. }))
                    .cloned();
                let final_agent_message = turn
                    .items
                    .iter()
                    .rev()
                    .find(|item| matches!(item, ThreadItem::AgentMessage { .. }))
                    .cloned();
                turn.items = match (first_user_message, final_agent_message) {
                    (Some(user_message), Some(agent_message))
                        if user_message.id() != agent_message.id() =>
                    {
                        vec![user_message, agent_message]
                    }
                    (Some(user_message), _) => vec![user_message],
                    (None, Some(agent_message)) => vec![agent_message],
                    (None, None) => Vec::new(),
                };
                turn.items_view = TurnItemsView::Summary;
            }
            TurnItemsView::Full => {
                turn.items_view = TurnItemsView::Full;
            }
        }
    }
}

fn reconstruct_thread_turns_for_turns_list(
    items: &[RolloutItem],
    loaded_status: ThreadStatus,
    has_live_running_thread: bool,
    active_turn: Option<Turn>,
) -> Vec<Turn> {
    let has_live_in_progress_turn = has_live_running_thread
        || active_turn
            .as_ref()
            .is_some_and(|turn| matches!(turn.status, TurnStatus::InProgress));
    let mut turns = build_legacy_api_turns_from_rollout_items(items);
    normalize_thread_turns_status(&mut turns, loaded_status, has_live_in_progress_turn);
    if let Some(active_turn) = active_turn {
        merge_turn_history_with_active_turn(&mut turns, active_turn);
    }
    turns
}

fn normalize_thread_turns_status(
    turns: &mut [Turn],
    loaded_status: ThreadStatus,
    has_live_in_progress_turn: bool,
) {
    let status = resolve_thread_status(loaded_status, has_live_in_progress_turn);
    if matches!(status, ThreadStatus::Active { .. }) {
        return;
    }
    for turn in turns {
        if matches!(turn.status, TurnStatus::InProgress) {
            turn.status = TurnStatus::Interrupted;
        }
    }
}

enum ThreadReadViewError {
    InvalidRequest(String),
    ClassifiedInvalidRequest {
        message: String,
        reason: ThreadErrorReason,
    },
    Unsupported(&'static str),
    Internal(String),
}

fn thread_read_view_error(err: ThreadReadViewError) -> JSONRPCErrorError {
    match err {
        ThreadReadViewError::InvalidRequest(message) => invalid_request(message),
        ThreadReadViewError::ClassifiedInvalidRequest { message, reason } => {
            thread_invalid_request(message, reason)
        }
        ThreadReadViewError::Unsupported(operation) => {
            unsupported_thread_store_operation(operation)
        }
        ThreadReadViewError::Internal(message) => internal_error(message),
    }
}

fn thread_invalid_request(
    message: impl Into<String>,
    reason: ThreadErrorReason,
) -> JSONRPCErrorError {
    let mut error = invalid_request(message);
    match serde_json::to_value(ThreadErrorData { reason }) {
        Ok(data) => error.data = Some(data),
        Err(err) => warn!("failed to serialize thread error data: {err}"),
    }
    error
}

pub(super) fn unsupported_thread_store_operation(operation: &'static str) -> JSONRPCErrorError {
    method_not_found(format!("{operation} is not supported yet"))
}

fn thread_store_list_error(err: ThreadStoreError) -> JSONRPCErrorError {
    match err {
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        ThreadStoreError::Unsupported { operation } => {
            unsupported_thread_store_operation(operation)
        }
        err => internal_error(format!("failed to list threads: {err}")),
    }
}

fn thread_store_resume_read_error(err: ThreadStoreError) -> JSONRPCErrorError {
    match err {
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        ThreadStoreError::Unsupported { operation } => {
            unsupported_thread_store_operation(operation)
        }
        ThreadStoreError::ThreadNotFound { thread_id } => thread_invalid_request(
            format!("no rollout found for thread id {thread_id}"),
            ThreadErrorReason::NotMaterialized,
        ),
        err @ ThreadStoreError::RolloutNotMaterialized { .. } => {
            thread_invalid_request(err.to_string(), ThreadErrorReason::NotMaterialized)
        }
        err => internal_error(format!("failed to read thread: {err}")),
    }
}

fn thread_list_history_load_error(
    thread_id: ThreadId,
    operation: &'static str,
    err: ThreadStoreError,
) -> ThreadReadViewError {
    match err {
        ThreadStoreError::RolloutNotMaterialized { .. } => {
            ThreadReadViewError::ClassifiedInvalidRequest {
                message: format!(
                    "thread {thread_id} is not materialized yet; {operation} is unavailable before first user message"
                ),
                reason: ThreadErrorReason::NotMaterialized,
            }
        }
        ThreadStoreError::ThreadNotFound { .. } => ThreadReadViewError::ClassifiedInvalidRequest {
            message: format!(
                "thread {thread_id} is not materialized yet; {operation} is unavailable before first user message"
            ),
            reason: ThreadErrorReason::NotMaterialized,
        },
        ThreadStoreError::InvalidRequest { message } => {
            ThreadReadViewError::InvalidRequest(message)
        }
        ThreadStoreError::Unsupported { operation } => ThreadReadViewError::Unsupported(operation),
        err => ThreadReadViewError::Internal(format!(
            "failed to load thread history for thread {thread_id}: {err}"
        )),
    }
}

fn thread_read_history_load_error(
    thread_id: ThreadId,
    err: ThreadStoreError,
) -> ThreadReadViewError {
    match err {
        ThreadStoreError::RolloutNotMaterialized { .. } => {
            ThreadReadViewError::ClassifiedInvalidRequest {
                message: format!(
                    "thread {thread_id} is not materialized yet; includeTurns is unavailable before first user message"
                ),
                reason: ThreadErrorReason::NotMaterialized,
            }
        }
        ThreadStoreError::ThreadNotFound {
            thread_id: missing_thread_id,
        } if missing_thread_id == thread_id => ThreadReadViewError::ClassifiedInvalidRequest {
            message: format!(
                "thread {thread_id} is not materialized yet; includeTurns is unavailable before first user message"
            ),
            reason: ThreadErrorReason::NotMaterialized,
        },
        ThreadStoreError::InvalidRequest { message } => {
            ThreadReadViewError::InvalidRequest(message)
        }
        ThreadStoreError::Unsupported { operation } => ThreadReadViewError::Unsupported(operation),
        err => ThreadReadViewError::Internal(format!(
            "failed to load thread history for thread {thread_id}: {err}"
        )),
    }
}

fn conversation_summary_thread_id_read_error(
    conversation_id: ThreadId,
    err: ThreadStoreError,
) -> JSONRPCErrorError {
    match err {
        ThreadStoreError::Unsupported { operation } => {
            unsupported_thread_store_operation(operation)
        }
        ThreadStoreError::ThreadNotFound { thread_id } if thread_id == conversation_id => {
            conversation_summary_not_found_error(conversation_id)
        }
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        err => internal_error(format!(
            "failed to load conversation summary for {conversation_id}: {err}"
        )),
    }
}

fn conversation_summary_not_found_error(conversation_id: ThreadId) -> JSONRPCErrorError {
    thread_invalid_request(
        format!("no rollout found for conversation id {conversation_id}"),
        ThreadErrorReason::NotFound,
    )
}

fn conversation_summary_rollout_path_read_error(
    path: &Path,
    err: ThreadStoreError,
) -> JSONRPCErrorError {
    match err {
        err @ ThreadStoreError::RolloutNotMaterialized { .. } => {
            thread_invalid_request(err.to_string(), ThreadErrorReason::NotMaterialized)
        }
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        ThreadStoreError::Unsupported { operation } => {
            unsupported_thread_store_operation(operation)
        }
        err => internal_error(format!(
            "failed to load conversation summary from {}: {}",
            path.display(),
            err
        )),
    }
}

pub(super) fn core_thread_write_error(operation: &str, err: CodexErr) -> JSONRPCErrorError {
    match err {
        CodexErr::ThreadNotFound(thread_id) => thread_invalid_request(
            format!("thread not found: {thread_id}"),
            ThreadErrorReason::NotFound,
        ),
        CodexErr::InvalidRequest(message) => invalid_request(message),
        CodexErr::UnsupportedOperation(message) => method_not_found(message),
        err => internal_error(format!("failed to {operation}: {err}")),
    }
}

fn thread_store_archive_error(operation: &str, err: ThreadStoreError) -> JSONRPCErrorError {
    match err {
        ThreadStoreError::ThreadNotFound { thread_id } => thread_invalid_request(
            format!("no rollout found for thread id {thread_id}"),
            ThreadErrorReason::NotFound,
        ),
        ThreadStoreError::InvalidRequest { message } => invalid_request(message),
        ThreadStoreError::Unsupported {
            operation: unsupported_operation,
        } => unsupported_thread_store_operation(unsupported_operation),
        err => internal_error(format!("failed to {operation} session: {err}")),
    }
}

fn set_thread_name_from_title(thread: &mut Thread, title: String) {
    if title.trim().is_empty() || thread.preview.trim() == title.trim() {
        return;
    }
    thread.name = Some(title);
}

pub(crate) fn thread_from_stored_thread(
    thread: StoredThread,
    fallback_provider: &str,
    fallback_cwd: &AbsolutePathBuf,
) -> (Thread, Option<codex_thread_store::StoredThreadHistory>) {
    let path = thread.rollout_path;
    let git_info = thread.git_info.map(|info| ApiGitInfo {
        sha: info.commit_hash.map(|sha| sha.0),
        branch: info.branch,
        origin_url: info.repository_url,
    });
    let cwd = AbsolutePathBuf::relative_to_current_dir(path_utils::normalize_for_native_workdir(
        thread.cwd,
    ))
    .unwrap_or_else(|err| {
        warn!("failed to normalize thread cwd while reading stored thread: {err}");
        fallback_cwd.clone()
    });
    let source = with_thread_spawn_agent_metadata(
        thread.source,
        thread.agent_nickname.clone(),
        thread.agent_role.clone(),
    );
    let history = thread.history;
    let thread_id = thread.thread_id.to_string();
    let thread = Thread {
        id: thread_id.clone(),
        extra: None,
        session_id: thread_id,
        forked_from_id: thread.forked_from_id.map(|id| id.to_string()),
        parent_thread_id: thread.parent_thread_id.map(|id| id.to_string()),
        preview: thread.preview,
        ephemeral: false,
        history_mode: thread.history_mode,
        model_provider: if thread.model_provider.is_empty() {
            fallback_provider.to_string()
        } else {
            thread.model_provider
        },
        created_at: thread.created_at.timestamp(),
        updated_at: thread.updated_at.timestamp(),
        recency_at: Some(thread.recency_at.timestamp()),
        status: ThreadStatus::NotLoaded,
        path,
        cwd,
        cli_version: thread.cli_version,
        agent_nickname: source.get_nickname(),
        agent_role: source.get_agent_role(),
        source: source.into(),
        thread_source: thread.thread_source,
        git_info,
        name: thread.name,
        turns: Vec::new(),
    };
    (thread, history)
}

fn summary_from_stored_thread(
    thread: StoredThread,
    fallback_provider: &str,
) -> ConversationSummary {
    let path = thread.rollout_path.unwrap_or_default();
    let source = with_thread_spawn_agent_metadata(
        thread.source,
        thread.agent_nickname.clone(),
        thread.agent_role.clone(),
    );
    let git_info = thread.git_info.map(|git| ConversationGitInfo {
        sha: git.commit_hash.map(|sha| sha.0),
        branch: git.branch,
        origin_url: git.repository_url,
    });
    ConversationSummary {
        conversation_id: thread.thread_id,
        path,
        preview: thread.preview,
        // Preserve millisecond precision from the thread store so thread/list cursors
        // round-trip the same ordering key used by pagination queries.
        timestamp: Some(
            thread
                .created_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
        updated_at: Some(
            thread
                .updated_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
        model_provider: if thread.model_provider.is_empty() {
            fallback_provider.to_string()
        } else {
            thread.model_provider
        },
        cwd: path_utils::normalize_for_native_workdir(thread.cwd.as_path()),
        cli_version: thread.cli_version,
        source,
        git_info,
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn summary_from_state_db_metadata(
    conversation_id: ThreadId,
    path: PathBuf,
    first_user_message: Option<String>,
    preview: Option<String>,
    timestamp: String,
    updated_at: String,
    model_provider: String,
    cwd: PathBuf,
    cli_version: String,
    source: String,
    _thread_source: Option<codex_protocol::protocol::ThreadSource>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    git_sha: Option<String>,
    git_branch: Option<String>,
    git_origin_url: Option<String>,
) -> ConversationSummary {
    let preview = preview.or(first_user_message).unwrap_or_default();
    let source = serde_json::from_str(&source)
        .or_else(|_| serde_json::from_value(serde_json::Value::String(source.clone())))
        .unwrap_or(codex_protocol::protocol::SessionSource::Unknown);
    let source = with_thread_spawn_agent_metadata(source, agent_nickname, agent_role);
    let git_info = if git_sha.is_none() && git_branch.is_none() && git_origin_url.is_none() {
        None
    } else {
        Some(ConversationGitInfo {
            sha: git_sha,
            branch: git_branch,
            origin_url: git_origin_url,
        })
    };
    ConversationSummary {
        conversation_id,
        path,
        preview,
        timestamp: Some(timestamp),
        updated_at: Some(updated_at),
        model_provider,
        cwd,
        cli_version,
        source,
        git_info,
    }
}

#[cfg(test)]
fn summary_from_thread_metadata(metadata: &ThreadMetadata) -> ConversationSummary {
    summary_from_state_db_metadata(
        metadata.id,
        metadata.rollout_path.clone(),
        metadata.first_user_message.clone(),
        metadata.preview.clone(),
        metadata
            .created_at
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        metadata
            .updated_at
            .to_rfc3339_opts(SecondsFormat::Secs, true),
        metadata.model_provider.clone(),
        metadata.cwd.clone(),
        metadata.cli_version.clone(),
        metadata.source.clone(),
        metadata.thread_source.clone(),
        metadata.agent_nickname.clone(),
        metadata.agent_role.clone(),
        metadata.git_sha.clone(),
        metadata.git_branch.clone(),
        metadata.git_origin_url.clone(),
    )
}

fn preview_from_rollout_items(items: &[RolloutItem]) -> String {
    items
        .iter()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(item) => match codex_core::parse_turn_item(item) {
                Some(codex_protocol::items::TurnItem::UserMessage(user)) => Some(user.message()),
                _ => None,
            },
            _ => None,
        })
        .map(|preview| strip_user_message_prefix(preview.as_str()).to_string())
        .unwrap_or_default()
}

fn requested_permissions_trust_project(overrides: &ConfigOverrides, cwd: &Path) -> bool {
    if matches!(
        overrides.sandbox_mode,
        Some(
            codex_protocol::config_types::SandboxMode::WorkspaceWrite
                | codex_protocol::config_types::SandboxMode::DangerFullAccess
        )
    ) {
        return true;
    }

    if matches!(
        overrides.default_permissions.as_deref(),
        Some(
            BUILT_IN_PERMISSION_PROFILE_WORKSPACE | BUILT_IN_PERMISSION_PROFILE_DANGER_FULL_ACCESS
        )
    ) {
        return true;
    }

    overrides
        .permission_profile
        .as_ref()
        .is_some_and(|profile| permission_profile_trusts_project(profile, cwd))
}

fn permission_profile_trusts_project(
    profile: &codex_protocol::models::PermissionProfile,
    cwd: &Path,
) -> bool {
    match profile {
        codex_protocol::models::PermissionProfile::Disabled
        | codex_protocol::models::PermissionProfile::External { .. } => true,
        codex_protocol::models::PermissionProfile::Managed { .. } => profile
            .file_system_sandbox_policy()
            .can_write_path_with_cwd(cwd, cwd),
    }
}

fn build_thread_from_snapshot(
    thread_id: ThreadId,
    session_id: String,
    config_snapshot: &ThreadConfigSnapshot,
    path: Option<PathBuf>,
) -> Thread {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Thread {
        id: thread_id.to_string(),
        extra: None,
        session_id,
        forked_from_id: None,
        parent_thread_id: config_snapshot.parent_thread_id.map(|id| id.to_string()),
        preview: String::new(),
        ephemeral: config_snapshot.ephemeral,
        history_mode: config_snapshot.history_mode,
        model_provider: config_snapshot.model_provider_id.clone(),
        created_at: now,
        updated_at: now,
        recency_at: Some(now),
        status: ThreadStatus::NotLoaded,
        path,
        cwd: config_snapshot.cwd().clone(),
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
        agent_nickname: config_snapshot.session_source.get_nickname(),
        agent_role: config_snapshot.session_source.get_agent_role(),
        source: config_snapshot.session_source.clone().into(),
        thread_source: config_snapshot.thread_source.clone(),
        git_info: None,
        name: None,
        turns: Vec::new(),
    }
}

fn paginate_background_terminals(
    terminals: &[ThreadBackgroundTerminal],
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<(Vec<ThreadBackgroundTerminal>, Option<String>), JSONRPCErrorError> {
    let start = match cursor {
        Some(cursor) => {
            let cursor = cursor
                .parse::<u32>()
                .map_err(|err| invalid_request(format!("invalid cursor: {err}")))?;
            terminals
                .iter()
                .position(|terminal| {
                    terminal
                        .process_id
                        .parse::<u32>()
                        .is_ok_and(|process_id| process_id > cursor)
                })
                .unwrap_or(terminals.len())
        }
        None => 0,
    };
    let effective_limit = limit.unwrap_or(terminals.len() as u32).max(1) as usize;
    let end = start.saturating_add(effective_limit).min(terminals.len());
    let next_cursor = (end < terminals.len()).then(|| terminals[end - 1].process_id.clone());
    Ok((terminals[start..end].to_vec(), next_cursor))
}

fn remove_desktop_activation_challenge_owners_for_connection(
    owners: &mut HashMap<String, (String, ConnectionId)>,
    connection_id: ConnectionId,
) {
    owners.retain(|_, (_, owner)| *owner != connection_id);
}

fn build_thread_from_loaded_snapshot(
    thread_id: ThreadId,
    config_snapshot: &ThreadConfigSnapshot,
    loaded_thread: &CodexThread,
) -> Thread {
    build_thread_from_snapshot(
        thread_id,
        loaded_thread.session_configured().session_id.to_string(),
        config_snapshot,
        loaded_thread.rollout_path(),
    )
}

#[cfg(test)]
mod thread_api_policy_tests {
    use super::ParsedThreadId;
    use super::SortDirection;
    use super::StoreSortDirection;
    use super::StoreThreadSortKey;
    use super::ThreadErrorData;
    use super::ThreadErrorReason;
    use super::ThreadSortKey;
    use super::thread_page_size;
    use super::thread_store_resume_read_error;
    use super::thread_store_sort_direction;
    use super::thread_store_sort_key;
    use codex_protocol::ThreadId;
    use codex_thread_store::ThreadStoreError;

    #[test]
    fn parsed_thread_id_reuses_boundary_validation_result() {
        let thread_id = "00000000-0000-0000-0000-000000000403";
        let expected = ThreadId::from_string(thread_id).expect("valid thread id");
        let parsed = ParsedThreadId::parse(thread_id);

        assert_eq!(parsed.valid(), Some(expected));
        assert_eq!(
            parsed.required().expect("thread id remains valid"),
            expected
        );

        let invalid = ParsedThreadId::parse("not-a-thread-id");
        assert_eq!(invalid.valid(), None);
        let error = invalid
            .required()
            .expect_err("invalid thread id is rejected");
        assert_eq!(error.code, -32600);
        assert!(error.message.starts_with("invalid session id: "));
    }

    #[test]
    fn one_policy_resolves_all_thread_page_limits() {
        assert_eq!(thread_page_size(None), 25);
        assert_eq!(thread_page_size(Some(0)), 1);
        assert_eq!(thread_page_size(Some(40)), 40);
        assert_eq!(thread_page_size(Some(200)), 100);
    }

    #[test]
    fn one_policy_maps_all_thread_sort_options() {
        assert_eq!(thread_store_sort_key(None), StoreThreadSortKey::CreatedAt);
        assert_eq!(
            thread_store_sort_key(Some(ThreadSortKey::UpdatedAt)),
            StoreThreadSortKey::UpdatedAt
        );
        assert_eq!(
            thread_store_sort_key(Some(ThreadSortKey::RecencyAt)),
            StoreThreadSortKey::RecencyAt
        );
        assert_eq!(
            thread_store_sort_direction(SortDirection::Asc),
            StoreSortDirection::Asc
        );
        assert_eq!(
            thread_store_sort_direction(SortDirection::Desc),
            StoreSortDirection::Desc
        );
    }

    #[test]
    fn resume_read_uses_typed_store_error_for_wire_classification() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000403").expect("valid thread id");
        let error = thread_store_resume_read_error(ThreadStoreError::ThreadNotFound { thread_id });

        assert_eq!(error.code, -32600);
        assert_eq!(
            error.message,
            format!("no rollout found for thread id {thread_id}")
        );
        assert_eq!(
            serde_json::from_value::<ThreadErrorData>(error.data.expect("classified error data"))
                .expect("valid thread error data")
                .reason,
            ThreadErrorReason::NotMaterialized
        );

        let internal = thread_store_resume_read_error(ThreadStoreError::Internal {
            message: "lookup unavailable".to_string(),
        });
        assert_eq!(internal.code, -32603);
    }
}

#[cfg(test)]
mod thread_created_lag_tests {
    use super::*;

    fn thread_id(value: &str) -> ThreadId {
        ThreadId::from_string(value).expect("valid thread id")
    }

    #[test]
    fn thread_creation_handling_tracks_loaded_instance_identity() {
        let id = thread_id("00000000-0000-0000-0000-000000000001");
        let first_instance = Arc::new(());
        let mut handled = HandledThreadCreationInstances::default();

        assert!(handled.mark_handled(id, &first_instance));
        assert!(!handled.mark_handled(id, &first_instance));

        let replacement_instance = Arc::new(());
        assert!(handled.mark_handled(id, &replacement_instance));
        assert!(!handled.mark_handled(id, &replacement_instance));
    }
}

#[cfg(test)]
#[path = "thread_processor_tests.rs"]
mod thread_processor_tests;
