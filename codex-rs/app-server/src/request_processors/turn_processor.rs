use super::*;
use codex_core::CodexResponsesMetadata;
use codex_core::ModelClient;
use codex_core::Prompt;
use codex_core::ResponseEvent;
use codex_features::Feature;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider_info::ModelProviderInfo;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::AdditionalContextEntry as CoreAdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind as CoreAdditionalContextKind;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::InferenceTraceContext;
use codex_state::BugClaim;
use codex_state::BugClassification;
use codex_state::BugCreateParams as StateBugCreateParams;
use codex_state::BugFailureCategory;
use codex_state::BugStore;
use futures::StreamExt;
use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use crate::image_url::REMOTE_IMAGE_URL_ERROR;
use crate::image_url::is_remote_image_url;

const DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR: &str =
    "direct app-server input is not allowed for multi-agent v2 sub-agents";
const MAX_ADDITIONAL_CONTEXT_ENTRIES: usize = 64;
const MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES: usize = 256;
const MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES: usize = 128 * 1_024;
// Context fragments cap each escaped value at approximately 4,000 tokens.
const MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES: usize = 16 * 1_024;
const ESTIMATED_ADDITIONAL_CONTEXT_WRAPPER_BYTES: usize = 96;

fn validate_user_input_image_urls(input: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
    if input.iter().any(|item| {
        matches!(
            item,
            V2UserInput::Image { url, .. } if is_remote_image_url(url)
        )
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

fn validate_response_item_image_urls(items: &[ResponseItem]) -> Result<(), JSONRPCErrorError> {
    if items.iter().any(|item| match item {
        ResponseItem::Message { content, .. } => content.iter().any(|item| {
            matches!(
                item,
                ContentItem::InputImage { image_url, .. } if is_remote_image_url(image_url)
            )
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.content_items().is_some_and(|content| {
                content.iter().any(|item| {
                    matches!(
                        item,
                        FunctionCallOutputContentItem::InputImage { image_url, .. }
                            if is_remote_image_url(image_url)
                    )
                })
            })
        }
        ResponseItem::Reasoning { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::AdditionalTools { .. }
        | ResponseItem::Other => false,
    }) {
        return Err(invalid_request(REMOTE_IMAGE_URL_ERROR));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct TurnRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    analytics_events_client: AnalyticsEventsClient,
    arg0_paths: Arg0DispatchPaths,
    config: Arc<Config>,
    config_manager: ConfigManager,
    pending_thread_unloads: Arc<PendingThreadUnloads>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    thread_list_state_permit: Arc<Semaphore>,
    skills_watcher: Arc<SkillsWatcher>,
    bug_worker_shutdown: CancellationToken,
}

fn map_additional_context(
    additional_context: Option<IndexMap<String, AdditionalContextEntry>>,
) -> Result<IndexMap<String, CoreAdditionalContextEntry>, JSONRPCErrorError> {
    let additional_context = additional_context.unwrap_or_default();
    if additional_context.len() > MAX_ADDITIONAL_CONTEXT_ENTRIES {
        return Err(invalid_request(format!(
            "additionalContext may contain at most {MAX_ADDITIONAL_CONTEXT_ENTRIES} entries (received {})",
            additional_context.len()
        )));
    }

    let longest_source_bytes = additional_context
        .keys()
        .map(String::len)
        .max()
        .unwrap_or_default();
    if longest_source_bytes > MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES {
        return Err(invalid_request(format!(
            "additionalContext source identifiers may contain at most {MAX_ADDITIONAL_CONTEXT_SOURCE_BYTES} bytes (longest was {longest_source_bytes} bytes)"
        )));
    }

    let estimated_rendered_bytes = additional_context
        .iter()
        .map(|(source, entry)| estimated_additional_context_rendered_bytes(source, entry))
        .fold(0usize, usize::saturating_add);
    if estimated_rendered_bytes > MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES {
        return Err(invalid_request(format!(
            "additionalContext may render to at most {MAX_ADDITIONAL_CONTEXT_AGGREGATE_RENDERED_BYTES} bytes (estimated {estimated_rendered_bytes} bytes)"
        )));
    }

    Ok(additional_context
        .into_iter()
        .map(|(key, entry)| {
            (
                key,
                CoreAdditionalContextEntry {
                    value: entry.value,
                    kind: match entry.kind {
                        AdditionalContextKind::Untrusted => CoreAdditionalContextKind::Untrusted,
                        AdditionalContextKind::Application => {
                            CoreAdditionalContextKind::Application
                        }
                    },
                },
            )
        })
        .collect())
}

fn estimated_additional_context_rendered_bytes(
    source: &str,
    entry: &AdditionalContextEntry,
) -> usize {
    let source_bytes = source.chars().fold(0usize, |total, ch| {
        let escaped = match ch {
            '&' => "&amp;".len(),
            '<' => "&lt;".len(),
            '>' => "&gt;".len(),
            '"' => "&quot;".len(),
            '\'' => "&#39;".len(),
            _ => ch.len_utf8(),
        };
        total.saturating_add(escaped)
    });
    let value_bytes = entry.value.chars().fold(0usize, |total, ch| {
        let escaped = match ch {
            '&' => "&amp;".len(),
            '<' => "&lt;".len(),
            '>' => "&gt;".len(),
            _ => ch.len_utf8(),
        };
        total.saturating_add(escaped)
    });

    source_bytes
        .saturating_add(value_bytes.min(MAX_ADDITIONAL_CONTEXT_VALUE_RENDERED_BYTES))
        .saturating_add(ESTIMATED_ADDITIONAL_CONTEXT_WRAPPER_BYTES)
}

struct ThreadSettingsBuildParams {
    method: &'static str,
    environments: Option<TurnEnvironmentSelections>,
    runtime_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    approval_policy: Option<codex_app_server_protocol::AskForApproval>,
    approvals_reviewer: Option<codex_app_server_protocol::ApprovalsReviewer>,
    sandbox_policy: Option<codex_app_server_protocol::SandboxPolicy>,
    permissions: Option<String>,
    model: Option<String>,
    service_tier: Option<Option<String>>,
    effort: Option<ReasoningEffort>,
    summary: Option<ReasoningSummary>,
    collaboration_mode: Option<CollaborationMode>,
    personality: Option<Personality>,
}

impl TurnRequestProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        analytics_events_client: AnalyticsEventsClient,
        arg0_paths: Arg0DispatchPaths,
        config: Arc<Config>,
        config_manager: ConfigManager,
        pending_thread_unloads: Arc<PendingThreadUnloads>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        thread_list_state_permit: Arc<Semaphore>,
        skills_watcher: Arc<SkillsWatcher>,
        bug_worker_shutdown: CancellationToken,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager,
            outgoing,
            analytics_events_client,
            arg0_paths,
            config,
            config_manager,
            pending_thread_unloads,
            thread_state_manager,
            thread_watch_manager,
            thread_list_state_permit,
            skills_watcher,
            bug_worker_shutdown,
        }
    }

    pub(crate) async fn turn_start(
        &self,
        request_id: ConnectionRequestId,
        mut params: TurnStartParams,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        let additional_context = map_additional_context(params.additional_context.take())?;
        self.turn_start_inner(
            request_id,
            params,
            additional_context,
            app_server_client_name,
            app_server_client_version,
            /*supports_openai_form_elicitation*/ supports_openai_form_elicitation,
        )
        .await
        .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_inject_items(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_inject_items_response_inner(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn thread_settings_update(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_settings_update_inner(request_id, params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_steer(
        &self,
        request_id: &ConnectionRequestId,
        mut params: TurnSteerParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        validate_user_input_image_urls(&params.input)?;
        let additional_context = map_additional_context(params.additional_context.take())?;
        self.turn_steer_inner(request_id, params, additional_context)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn turn_interrupt(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.turn_interrupt_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_start(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_start_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_audio(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_audio_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_text(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_text_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_append_speech(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_append_speech_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_stop(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.thread_realtime_stop_inner(request_id, params)
            .await
            .map(|response| response.map(Into::into))
    }

    pub(crate) async fn thread_realtime_list_voices(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        Ok(Some(
            ThreadRealtimeListVoicesResponse {
                voices: RealtimeVoicesList::builtin(),
            }
            .into(),
        ))
    }

    pub(crate) async fn bug_create(
        &self,
        params: BugCreateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if params.raw_text.trim().is_empty() {
            return Err(invalid_request("bug report text must not be empty"));
        }
        // Loading is validation only: no turn is created and the thread is not
        // changed or waited on. The report's lifecycle belongs to SQLite.
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let snapshot = thread.config_snapshot().await;
        let models_manager = self.thread_manager.get_models_manager();
        let model_info = models_manager
            .get_model_info(
                snapshot.model.as_str(),
                &self.config.to_models_manager_config(),
            )
            .await;
        let provider = self
            .config
            .model_providers
            .get(&snapshot.model_provider_id)
            .cloned()
            .ok_or_else(|| internal_error("thread model provider is not configured"))?;
        let classifier = BugClassifierContext {
            auth_manager: Arc::clone(&self.auth_manager),
            config: Arc::clone(&self.config),
            provider,
            provider_id: snapshot.model_provider_id.clone(),
            requested_model: snapshot.model.clone(),
            model_info,
            session_source: snapshot.session_source.clone(),
            originator: snapshot.originator.clone(),
            shutdown: self.bug_worker_shutdown.clone(),
        };
        let cwd_path = self.config.cwd.to_path_buf();
        let cwd = cwd_path.to_string_lossy().into_owned();
        let repository_root = codex_git_utils::get_git_repo_root(cwd_path.as_path())
            .map(|path| path.to_string_lossy().into_owned());
        let git_commit = codex_git_utils::get_head_commit_hash(cwd_path.as_path())
            .await
            .map(|sha| sha.0);
        let store = BugStore::open(self.config.sqlite_home.as_path())
            .await
            .map_err(|_| internal_error("failed to persist bug report"))?;
        let created = store
            .create(StateBugCreateParams {
                raw_text: &params.raw_text,
                thread_id: &params.thread_id,
                cwd: Some(&cwd),
                repository_root: repository_root.as_deref(),
                git_commit: git_commit.as_deref(),
            })
            .await
            .map_err(|_| internal_error("failed to persist bug report"))?;
        let response = BugCreateResponse {
            id: created.id,
            display_id: created.display_id,
            status: "pending".to_string(),
            durable_save_result: true,
        };
        let created_id = created.id;
        tokio::spawn(async move {
            let Ok(Some(new_claim)) = store.claim_by_id(created_id).await else {
                return;
            };
            classify_bug_claim(&store, &classifier, new_claim).await;
            if let Ok(Some(older_claim)) = store.claim_next_older(created_id).await {
                classify_bug_claim(&store, &classifier, older_claim).await;
            }
        });
        Ok(Some(response.into()))
    }

    fn track_error_response(
        &self,
        request_id: &ConnectionRequestId,
        error: &JSONRPCErrorError,
        error_type: Option<AnalyticsJsonRpcError>,
    ) {
        self.analytics_events_client.track_error_response(
            request_id.connection_id.0,
            request_id.request_id.clone(),
            error.clone(),
            error_type,
        );
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

    async fn ensure_direct_input_allowed(
        &self,
        request_id: &ConnectionRequestId,
        thread: &CodexThread,
    ) -> Result<(), JSONRPCErrorError> {
        if thread.multi_agent_version() == Some(MultiAgentVersion::V2)
            && matches!(
                thread.config_snapshot().await.session_source,
                SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
            )
        {
            let error = invalid_request(DIRECT_INPUT_TO_MULTI_AGENT_V2_SUBAGENT_ERROR);
            self.track_error_response(request_id, &error, /*error_type*/ None);
            return Err(error);
        }

        Ok(())
    }

    fn normalize_collaboration_mode(
        &self,
        mut collaboration_mode: CollaborationMode,
    ) -> CollaborationMode {
        if collaboration_mode.settings.developer_instructions.is_none()
            && let Some(instructions) = builtin_collaboration_mode_presets()
                .into_iter()
                .find(|preset| preset.mode == Some(collaboration_mode.mode))
                .and_then(|preset| preset.developer_instructions.flatten())
                .filter(|instructions| !instructions.is_empty())
        {
            collaboration_mode.settings.developer_instructions = Some(instructions);
        }

        collaboration_mode
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

    fn input_too_large_error(actual_chars: usize) -> JSONRPCErrorError {
        let mut error = invalid_params(format!(
            "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
        ));
        error.data = Some(serde_json::json!({
            "input_error_code": INPUT_TOO_LARGE_ERROR_CODE,
            "max_chars": MAX_USER_INPUT_TEXT_CHARS,
            "actual_chars": actual_chars,
        }));
        error
    }

    fn validate_v2_input_limit(items: &[V2UserInput]) -> Result<(), JSONRPCErrorError> {
        let actual_chars: usize = items.iter().map(V2UserInput::text_char_count).sum();
        if actual_chars > MAX_USER_INPUT_TEXT_CHARS {
            return Err(Self::input_too_large_error(actual_chars));
        }
        Ok(())
    }

    async fn turn_start_inner(
        &self,
        request_id: ConnectionRequestId,
        params: TurnStartParams,
        additional_context: IndexMap<String, CoreAdditionalContextEntry>,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        supports_openai_form_elicitation: bool,
    ) -> Result<TurnStartResponse, JSONRPCErrorError> {
        let (thread_id, thread) =
            self.load_thread(&params.thread_id)
                .await
                .inspect_err(|error| {
                    self.track_error_response(&request_id, error, /*error_type*/ None);
                })?;
        self.ensure_direct_input_allowed(&request_id, thread.as_ref())
            .await?;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                &request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }
        let environment_selections =
            resolve_turn_environment_selections(self.thread_manager.as_ref(), params.environments)?;

        // Map v2 input items to core input items.
        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let client_user_message_id = params.client_user_message_id;
        let turn_has_input = !mapped_items.is_empty();
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(thread.as_ref(), cwd, environment_selections)
            .await;
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "turn/start",
                    environments,
                    runtime_workspace_roots: params.runtime_workspace_roots,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;

        // Finish fallible request preflight before applying connection capabilities to shared
        // thread and MCP state. A rejected turn must leave the active thread unchanged.
        Self::set_app_server_client_info(
            thread.as_ref(),
            app_server_client_name,
            app_server_client_version,
        )
        .await
        .inspect_err(|error| {
            self.track_error_response(&request_id, error, /*error_type*/ None);
        })?;
        thread
            .set_openai_form_elicitation_support(supports_openai_form_elicitation)
            .await
            .map_err(|err| {
                internal_error(format!(
                    "failed to update OpenAI form elicitation support: {err}"
                ))
            })?;

        // Start the turn by submitting the user input. Return its submission id as turn_id.
        let turn_op = Op::UserInput {
            items: mapped_items,
            final_output_json_schema: params.output_schema,
            responsesapi_client_metadata: params.responsesapi_client_metadata,
            additional_context,
            thread_settings,
        };
        let turn_id = thread.reserve_turn_id();
        let turn_origin_tracker = {
            let thread_state = self.thread_state_manager.thread_state(thread_id).await;
            thread_state.lock().await.turn_origin_tracker()
        };
        let origin_reservation =
            turn_origin_tracker.reserve(turn_id.clone(), request_id.connection_id);
        let request_trace_context = self.request_trace_context(&request_id).await;
        thread
            .submit_user_input_with_reserved_turn_id(
                turn_id.clone(),
                turn_op,
                request_trace_context,
                client_user_message_id,
            )
            .await
            .map_err(|err| {
                let error = internal_error(format!("failed to start turn: {err}"));
                self.track_error_response(&request_id, &error, /*error_type*/ None);
                error
            })?;
        origin_reservation.commit();

        if turn_has_input {
            let config_snapshot = thread.config_snapshot().await;
            codex_memories_write::start_memories_startup_task(
                Arc::clone(&self.thread_manager),
                Arc::clone(&self.auth_manager),
                thread_id,
                Arc::clone(&thread),
                thread.config().await,
                &config_snapshot.session_source,
            );
        }

        self.outgoing
            .record_request_turn_id(&request_id, &turn_id)
            .await;
        let turn = Turn {
            id: turn_id,
            items: vec![],
            items_view: TurnItemsView::NotLoaded,
            error: None,
            status: TurnStatus::InProgress,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            completion: None,
            timing: None,
            surfaced_result: None,
            reasoning_policy_history: None,
        };

        Ok(TurnStartResponse { turn })
    }

    async fn build_environment_override(
        &self,
        thread: &CodexThread,
        cwd: Option<AbsolutePathBuf>,
        environment_selections: Option<Vec<TurnEnvironmentSelection>>,
    ) -> Option<TurnEnvironmentSelections> {
        match (cwd, environment_selections) {
            (None, None) => None,
            (Some(cwd), None) => {
                let environment_selections =
                    self.thread_manager.default_environment_selections(&cwd);
                Some(TurnEnvironmentSelections::new(cwd, environment_selections))
            }
            (cwd, Some(environment_selections)) => {
                let legacy_fallback_cwd = match cwd {
                    Some(cwd) => cwd,
                    None => {
                        let snapshot = thread.config_snapshot().await;
                        environment_selections
                            .iter()
                            .find(|selection| selection.environment_id == LOCAL_ENVIRONMENT_ID)
                            .and_then(|selection| selection.cwd.to_abs_path().ok())
                            .unwrap_or_else(|| snapshot.cwd().clone())
                    }
                };
                Some(TurnEnvironmentSelections::new(
                    legacy_fallback_cwd,
                    environment_selections,
                ))
            }
        }
    }

    async fn build_thread_settings_overrides(
        &self,
        thread: &CodexThread,
        params: ThreadSettingsBuildParams,
    ) -> Result<codex_protocol::protocol::ThreadSettingsOverrides, JSONRPCErrorError> {
        let ThreadSettingsBuildParams {
            method,
            environments,
            runtime_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permissions,
            model,
            service_tier,
            effort,
            summary,
            collaboration_mode,
            personality,
        } = params;

        if sandbox_policy.is_some() && permissions.is_some() {
            return Err(invalid_request(
                "`permissions` cannot be combined with `sandboxPolicy`",
            ));
        }

        let collaboration_mode =
            collaboration_mode.map(|mode| self.normalize_collaboration_mode(mode));
        let runtime_workspace_roots_request = runtime_workspace_roots;
        let has_environment_override = environments.is_some();
        // `thread/settings/update` only acknowledges that the update was queued.
        // Clients that send dependent partial updates should wait for
        // `thread/settings/updated` or combine the fields in one request.
        let snapshot = if permissions.is_some() {
            Some(thread.config_snapshot().await)
        } else {
            None
        };

        let has_any_overrides = has_environment_override
            || runtime_workspace_roots_request.is_some()
            || approval_policy.is_some()
            || approvals_reviewer.is_some()
            || sandbox_policy.is_some()
            || permissions.is_some()
            || model.is_some()
            || service_tier.is_some()
            || effort.is_some()
            || summary.is_some()
            || collaboration_mode.is_some()
            || personality.is_some();

        let runtime_workspace_roots =
            runtime_workspace_roots_request.map(resolve_runtime_workspace_roots);
        let approval_policy =
            approval_policy.map(codex_app_server_protocol::AskForApproval::to_core);
        let approvals_reviewer =
            approvals_reviewer.map(codex_app_server_protocol::ApprovalsReviewer::to_core);
        let sandbox_policy = sandbox_policy.map(|policy| policy.to_core());
        let (permission_profile, active_permission_profile, profile_workspace_roots) =
            if let Some(permissions) = permissions {
                let Some(snapshot) = snapshot.as_ref() else {
                    return Err(internal_error(format!(
                        "{method} permission selection missing thread snapshot"
                    )));
                };
                let overrides = ConfigOverrides {
                    cwd: environments
                        .as_ref()
                        .map(|environments| environments.legacy_fallback_cwd.to_path_buf()),
                    workspace_roots: Some(
                        runtime_workspace_roots
                            .clone()
                            .unwrap_or_else(|| snapshot.workspace_roots.clone()),
                    ),
                    default_permissions: Some(permissions),
                    codex_linux_sandbox_exe: self.arg0_paths.codex_linux_sandbox_exe.clone(),
                    main_execve_wrapper_exe: self.arg0_paths.main_execve_wrapper_exe.clone(),
                    ..Default::default()
                };
                let config = self
                    .config_manager
                    .load_for_cwd(
                        /*request_overrides*/ None,
                        overrides,
                        Some(snapshot.cwd().to_path_buf()),
                    )
                    .await
                    .map_err(|err| config_load_error(&err))?;
                // Startup config is allowed to fall back when requirements
                // disallow a configured profile. An explicit settings update
                // is different: reject it before accepting the request.
                if let Some(warning) = config.startup_warnings.iter().find(|warning| {
                    warning.contains("Configured value for `permission_profile` is disallowed")
                }) {
                    return Err(invalid_request(format!(
                        "invalid thread settings override: {warning}"
                    )));
                }
                (
                    Some(config.permissions.permission_profile().clone()),
                    config.permissions.active_permission_profile(),
                    Some(config.permissions.profile_workspace_roots().to_vec()),
                )
            } else {
                (None, None, None)
            };
        let effort = effort.map(Some);

        if has_any_overrides {
            thread
                .preview_thread_settings_overrides(CodexThreadSettingsOverrides {
                    environments: environments.clone(),
                    workspace_roots: runtime_workspace_roots.clone(),
                    approval_policy,
                    approvals_reviewer,
                    sandbox_policy: sandbox_policy.clone(),
                    permission_profile: permission_profile.clone(),
                    active_permission_profile: active_permission_profile.clone(),
                    profile_workspace_roots: profile_workspace_roots.clone(),
                    windows_sandbox_level: None,
                    model: model.clone(),
                    effort: effort.clone(),
                    summary,
                    service_tier: service_tier.clone(),
                    collaboration_mode: collaboration_mode.clone(),
                    personality,
                })
                .await
                .map_err(|err| {
                    invalid_request(format!("invalid thread settings override: {err}"))
                })?;
        }

        Ok(codex_protocol::protocol::ThreadSettingsOverrides {
            environments,
            workspace_roots: runtime_workspace_roots,
            profile_workspace_roots,
            approval_policy,
            approvals_reviewer,
            sandbox_policy,
            permission_profile,
            active_permission_profile,
            windows_sandbox_level: None,
            model,
            effort,
            summary,
            service_tier,
            collaboration_mode,
            personality,
        })
    }

    async fn thread_settings_update_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadSettingsUpdateParams,
    ) -> Result<ThreadSettingsUpdateResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let cwd = resolve_request_cwd(params.cwd)?;
        let environments = self
            .build_environment_override(thread.as_ref(), cwd, /*environment_selections*/ None)
            .await;
        let thread_settings = self
            .build_thread_settings_overrides(
                thread.as_ref(),
                ThreadSettingsBuildParams {
                    method: "thread/settings/update",
                    environments,
                    runtime_workspace_roots: None,
                    approval_policy: params.approval_policy,
                    approvals_reviewer: params.approvals_reviewer,
                    sandbox_policy: params.sandbox_policy,
                    permissions: params.permissions,
                    model: params.model,
                    service_tier: params.service_tier,
                    effort: params.effort,
                    summary: params.summary,
                    collaboration_mode: params.collaboration_mode,
                    personality: params.personality,
                },
            )
            .await?;

        if thread_settings != codex_protocol::protocol::ThreadSettingsOverrides::default() {
            self.submit_core_op(
                request_id,
                thread.as_ref(),
                Op::ThreadSettings { thread_settings },
            )
            .await
            .map_err(|err| internal_error(format!("failed to update thread settings: {err}")))?;
        }

        Ok(ThreadSettingsUpdateResponse {})
    }

    async fn thread_inject_items_response_inner(
        &self,
        params: ThreadInjectItemsParams,
    ) -> Result<ThreadInjectItemsResponse, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;

        let items = params
            .items
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                serde_json::from_value::<ResponseItem>(value)
                    .map_err(|err| format!("items[{index}] is not a valid response item: {err}"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(invalid_request)?;
        validate_response_item_image_urls(&items)?;

        thread
            .inject_response_items(items)
            .await
            .map_err(|err| match err {
                CodexErr::InvalidRequest(message) => invalid_request(message),
                err => internal_error(format!("failed to inject response items: {err}")),
            })?;
        Ok(ThreadInjectItemsResponse {})
    }

    async fn set_app_server_client_info(
        thread: &CodexThread,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let mcp_elicitations_auto_deny = xcode_26_4_mcp_elicitations_auto_deny(
            app_server_client_name.as_deref(),
            app_server_client_version.as_deref(),
        );
        thread
            .set_app_server_client_info(
                app_server_client_name,
                app_server_client_version,
                mcp_elicitations_auto_deny,
            )
            .await
            .map_err(|err| internal_error(format!("failed to set app server client info: {err}")))
    }

    async fn turn_steer_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnSteerParams,
        additional_context: IndexMap<String, CoreAdditionalContextEntry>,
    ) -> Result<TurnSteerResponse, JSONRPCErrorError> {
        let (_, thread) = self
            .load_thread(&params.thread_id)
            .await
            .inspect_err(|error| {
                self.track_error_response(request_id, error, /*error_type*/ None);
            })?;
        self.ensure_direct_input_allowed(request_id, thread.as_ref())
            .await?;

        if params.expected_turn_id.is_empty() {
            return Err(invalid_request("expectedTurnId must not be empty"));
        }
        self.outgoing
            .record_request_turn_id(request_id, &params.expected_turn_id)
            .await;
        if let Err(error) = Self::validate_v2_input_limit(&params.input) {
            self.track_error_response(
                request_id,
                &error,
                Some(AnalyticsJsonRpcError::Input(InputError::TooLarge)),
            );
            return Err(error);
        }

        let mapped_items: Vec<CoreInputItem> = params
            .input
            .into_iter()
            .map(V2UserInput::into_core)
            .collect();
        let turn_id = thread
            .steer_input(
                mapped_items,
                additional_context,
                Some(&params.expected_turn_id),
                params.client_user_message_id,
                params.responsesapi_client_metadata,
            )
            .await
            .map_err(|err| {
                let (message, data, error_type) = match err {
                    SteerInputError::NoActiveTurn(_) => (
                        "no active turn to steer".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::NoActiveTurn,
                        )),
                    ),
                    SteerInputError::ExpectedTurnMismatch { expected, actual } => (
                        format!("expected active turn id `{expected}` but found `{actual}`"),
                        None,
                        Some(AnalyticsJsonRpcError::TurnSteer(
                            TurnSteerRequestError::ExpectedTurnMismatch,
                        )),
                    ),
                    SteerInputError::ActiveTurnNotSteerable { turn_kind } => {
                        let (message, turn_steer_error) = match turn_kind {
                            codex_protocol::protocol::NonSteerableTurnKind::Review => (
                                "cannot steer a review turn".to_string(),
                                TurnSteerRequestError::NonSteerableReview,
                            ),
                            codex_protocol::protocol::NonSteerableTurnKind::Compact => (
                                "cannot steer a compact turn".to_string(),
                                TurnSteerRequestError::NonSteerableCompact,
                            ),
                        };
                        let error = TurnError {
                            message: message.clone(),
                            codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                                turn_kind: turn_kind.into(),
                            }),
                            additional_details: None,
                        };
                        let data = match serde_json::to_value(error) {
                            Ok(data) => Some(data),
                            Err(error) => {
                                tracing::error!(
                                    ?error,
                                    "failed to serialize active-turn-not-steerable turn error"
                                );
                                None
                            }
                        };
                        (
                            message,
                            data,
                            Some(AnalyticsJsonRpcError::TurnSteer(turn_steer_error)),
                        )
                    }
                    SteerInputError::EmptyInput => (
                        "input must not be empty".to_string(),
                        None,
                        Some(AnalyticsJsonRpcError::Input(InputError::Empty)),
                    ),
                };
                let mut error = invalid_request(message);
                error.data = data;
                self.track_error_response(request_id, &error, error_type);
                error
            })?;
        Ok(TurnSteerResponse { turn_id })
    }

    async fn prepare_realtime_conversation_thread(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: &str,
    ) -> Result<Option<(ThreadId, Arc<CodexThread>)>, JSONRPCErrorError> {
        let (thread_id, thread) = self.load_thread(thread_id).await?;

        match self
            .ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await
        {
            Ok(EnsureConversationListenerResult::Attached) => {}
            Ok(EnsureConversationListenerResult::ConnectionClosed) => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }

        if !thread.enabled(Feature::RealtimeConversation) {
            return Err(invalid_request(format!(
                "thread {thread_id} does not support realtime conversation"
            )));
        }

        Ok(Some((thread_id, thread)))
    }

    async fn thread_realtime_start_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStartParams,
    ) -> Result<Option<ThreadRealtimeStartResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationStart(ConversationStartParams {
                client_managed_handoffs: params.client_managed_handoffs.unwrap_or(false),
                flush_transcript_tail_on_session_end: params
                    .flush_transcript_tail_on_session_end
                    .unwrap_or(false),
                codex_responses_as_items: params.codex_responses_as_items.unwrap_or(false),
                codex_response_item_prefix: params.codex_response_item_prefix,
                codex_response_handoff_prefix: params.codex_response_handoff_prefix,
                model: params.model,
                output_modality: params.output_modality,
                include_startup_context: params.include_startup_context.unwrap_or(true),
                prompt: params.prompt,
                realtime_session_id: params.realtime_session_id,
                transport: params.transport.map(|transport| match transport {
                    ThreadRealtimeStartTransport::Websocket => {
                        ConversationStartTransport::Websocket
                    }
                    ThreadRealtimeStartTransport::Webrtc { sdp } => {
                        ConversationStartTransport::Webrtc { sdp }
                    }
                }),
                version: params.version,
                voice: params.voice,
            }),
        )
        .await
        .map_err(|err| internal_error(format!("failed to start realtime conversation: {err}")))?;
        Ok(Some(ThreadRealtimeStartResponse::default()))
    }

    async fn thread_realtime_append_audio_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendAudioParams,
    ) -> Result<Option<ThreadRealtimeAppendAudioResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationAudio(ConversationAudioParams {
                frame: params.audio.into(),
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation audio: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendAudioResponse::default()))
    }

    async fn thread_realtime_append_text_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendTextParams,
    ) -> Result<Option<ThreadRealtimeAppendTextResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationText(ConversationTextParams {
                text: params.text,
                role: params.role,
            }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation text: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendTextResponse::default()))
    }

    async fn thread_realtime_append_speech_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeAppendSpeechParams,
    ) -> Result<Option<ThreadRealtimeAppendSpeechResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(
            request_id,
            thread.as_ref(),
            Op::RealtimeConversationSpeech(ConversationSpeechParams { text: params.text }),
        )
        .await
        .map_err(|err| {
            internal_error(format!(
                "failed to append realtime conversation speech: {err}"
            ))
        })?;
        Ok(Some(ThreadRealtimeAppendSpeechResponse::default()))
    }

    async fn thread_realtime_stop_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: ThreadRealtimeStopParams,
    ) -> Result<Option<ThreadRealtimeStopResponse>, JSONRPCErrorError> {
        let Some((_, thread)) = self
            .prepare_realtime_conversation_thread(request_id, &params.thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.submit_core_op(request_id, thread.as_ref(), Op::RealtimeConversationClose)
            .await
            .map_err(|err| {
                internal_error(format!("failed to stop realtime conversation: {err}"))
            })?;
        Ok(Some(ThreadRealtimeStopResponse::default()))
    }

    async fn turn_interrupt_inner(
        &self,
        request_id: &ConnectionRequestId,
        params: TurnInterruptParams,
    ) -> Result<Option<TurnInterruptResponse>, JSONRPCErrorError> {
        let TurnInterruptParams { thread_id, turn_id } = params;
        let is_startup_interrupt = turn_id.is_empty();

        let (thread_uuid, thread) = self.load_thread(&thread_id).await?;

        // Record turn interrupts so we can reply when TurnAborted arrives. Startup
        // interrupts do not have a turn and are acknowledged after submission.
        if !is_startup_interrupt {
            let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
            let is_running = matches!(thread.agent_status().await, AgentStatus::Running);
            {
                let mut thread_state = thread_state.lock().await;
                if let Some(active_turn) = thread_state.active_turn_snapshot() {
                    if active_turn.id != turn_id {
                        return Err(invalid_request(format!(
                            "expected active turn id {turn_id} but found {}",
                            active_turn.id
                        )));
                    }
                } else if thread_state.last_terminal_turn_id.as_deref() == Some(turn_id.as_str())
                    || !is_running
                {
                    return Err(invalid_request("no active turn to interrupt"));
                }
                thread_state.pending_interrupts.push(request_id.clone());
            }

            self.outgoing
                .record_request_turn_id(request_id, &turn_id)
                .await;
        }

        // Submit the interrupt. Turn interrupts respond upon TurnAborted; startup
        // interrupts respond here because startup cancellation has no turn event.
        match self
            .submit_core_op(request_id, thread.as_ref(), Op::Interrupt)
            .await
        {
            Ok(_) if is_startup_interrupt => Ok(Some(TurnInterruptResponse {})),
            Ok(_) => Ok(None),
            Err(err) => {
                if !is_startup_interrupt {
                    let thread_state = self.thread_state_manager.thread_state(thread_uuid).await;
                    let mut thread_state = thread_state.lock().await;
                    thread_state
                        .pending_interrupts
                        .retain(|pending_request_id| pending_request_id != request_id);
                }
                let interrupt_target = if is_startup_interrupt {
                    "startup"
                } else {
                    "turn"
                };
                Err(internal_error(format!(
                    "failed to interrupt {interrupt_target}: {err}"
                )))
            }
        }
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
}

const BUG_CLASSIFIER_SCHEMA_VERSION: &str = "bug-classification-v1";
const BUG_CLASSIFIER_PROMPT_VERSION: &str = "bug-classifier-prompt-v1";
const BUG_CLASSIFIER_INSTRUCTIONS: &str = "Classify the supplied bug report. Return only the requested JSON object. The summary may be abstractive. Every other populated field must quote exact source text through its evidence range. Do not infer facts that are not stated in the report.";

#[derive(Clone)]
struct BugClassifierContext {
    auth_manager: Arc<AuthManager>,
    config: Arc<Config>,
    provider: ModelProviderInfo,
    provider_id: String,
    requested_model: String,
    model_info: ModelInfo,
    session_source: SessionSource,
    originator: String,
    shutdown: CancellationToken,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BugClassifierResult {
    summary: String,
    severity: Option<CitedFact>,
    failure_mechanism: Option<CitedFact>,
    affected_components: Vec<CitedFact>,
    stated_cause: Option<CitedFact>,
    required_repair: Option<CitedFact>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CitedFact {
    value: String,
    evidence: ByteEvidence,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ByteEvidence {
    start_byte: usize,
    end_byte: usize,
    text: String,
}

struct ValidatedBugClassification {
    summary: String,
    severity: Option<String>,
    failure_mechanism: Option<String>,
    affected_components_json: String,
    stated_cause: Option<String>,
    required_repair: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum BugClassificationFailure {
    Cancelled,
    Provider,
    MalformedOutput,
    Schema,
    Grounding,
}

impl BugClassificationFailure {
    fn category(self) -> BugFailureCategory {
        match self {
            Self::Cancelled => BugFailureCategory::Cancelled,
            Self::Provider => BugFailureCategory::Provider,
            Self::MalformedOutput => BugFailureCategory::MalformedOutput,
            Self::Schema => BugFailureCategory::Schema,
            Self::Grounding => BugFailureCategory::Grounding,
        }
    }
}

async fn classify_bug_claim(store: &BugStore, context: &BugClassifierContext, claim: BugClaim) {
    match classify_bug_report(context, &claim.raw_text).await {
        Ok(classification) => {
            let normalized = BugClassification {
                summary: &classification.summary,
                severity: classification.severity.as_deref(),
                failure_mechanism: classification.failure_mechanism.as_deref(),
                affected_components_json: &classification.affected_components_json,
                stated_cause: classification.stated_cause.as_deref(),
                required_repair: classification.required_repair.as_deref(),
                classifier_provider_id: &context.provider_id,
                classifier_requested_model: &context.requested_model,
                classifier_resolved_model: Some(context.model_info.slug.as_str()),
                classifier_reasoning_effort: "low",
                classifier_schema_version: BUG_CLASSIFIER_SCHEMA_VERSION,
                classifier_prompt_version: BUG_CLASSIFIER_PROMPT_VERSION,
            };
            // A failed result commit deliberately leaves the claim in place until its lease
            // expires; the consumed attempt must not be replayed immediately.
            let _ = store
                .commit_classification(claim.id, &claim.claim_token, normalized)
                .await;
        }
        Err(failure) => {
            let _ = store
                .release_failure(claim.id, &claim.claim_token, failure.category())
                .await;
        }
    }
}

async fn classify_bug_report(
    context: &BugClassifierContext,
    raw_text: &str,
) -> Result<ValidatedBugClassification, BugClassificationFailure> {
    let agent_identity_policy = if context.config.features.enabled(Feature::UseAgentIdentity) {
        AgentIdentityAuthPolicy::ChatGptAuth
    } else {
        AgentIdentityAuthPolicy::JwtOnly
    };
    let model_client = ModelClient::new(
        Some(Arc::clone(&context.auth_manager)),
        agent_identity_policy,
        ThreadId::new(),
        context.provider.clone(),
        context.session_source.clone(),
        context.originator.clone(),
        context.config.model_verbosity,
        context
            .config
            .features
            .enabled(Feature::EnableRequestCompression),
        context.config.features.enabled(Feature::RuntimeMetrics),
        None,
        context.config.features.enabled(Feature::ItemIds),
        context
            .config
            .features
            .enabled(Feature::ConcurrentReasoningSummaries),
        None,
        context.config.http_client_factory(),
    );

    let mut prompt = Prompt::default();
    prompt.input = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: raw_text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }]
    .into();
    prompt.base_instructions.text = BUG_CLASSIFIER_INSTRUCTIONS.to_string();
    prompt.output_schema = Some(bug_classifier_schema());

    let classifier_thread_id = ThreadId::new();
    let telemetry = SessionTelemetry::new(
        classifier_thread_id,
        context.requested_model.as_str(),
        context.model_info.slug.as_str(),
        None,
        None,
        None,
        "bug-classifier".to_string(),
        false,
        "bug-classifier".to_string(),
        context.session_source.clone(),
    );
    let responses_metadata = CodexResponsesMetadata::new(
        String::new(),
        ThreadId::new().to_string(),
        ThreadId::new().to_string(),
        String::new(),
    );
    let inference_trace = InferenceTraceContext::disabled();
    let mut session = model_client.new_session();
    let mut stream = tokio::select! {
        _ = context.shutdown.cancelled() => return Err(BugClassificationFailure::Cancelled),
        result = session.stream(
            &prompt,
            &context.model_info,
            &telemetry,
            Some(ReasoningEffort::Low),
            ReasoningSummary::None,
            None,
            &responses_metadata,
            &inference_trace,
        ) => result.map_err(|_| BugClassificationFailure::Provider)?,
    };

    let mut completed = false;
    let mut output_count = 0usize;
    let mut output = None;
    loop {
        let event = tokio::select! {
            _ = context.shutdown.cancelled() => return Err(BugClassificationFailure::Cancelled),
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event.map_err(|_| BugClassificationFailure::Provider)? {
            ResponseEvent::OutputItemDone(item) => {
                output_count += 1;
                if let ResponseItem::Message { role, content, .. } = item
                    && role == "assistant"
                    && let [ContentItem::OutputText { text }] = content.as_slice()
                {
                    output = Some(text.clone());
                }
            }
            ResponseEvent::Completed { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }

    if !completed || output_count != 1 {
        return Err(BugClassificationFailure::MalformedOutput);
    }
    let output = output.ok_or(BugClassificationFailure::MalformedOutput)?;
    parse_bug_classification(&output, raw_text)
}

fn parse_bug_classification(
    output: &str,
    raw_text: &str,
) -> Result<ValidatedBugClassification, BugClassificationFailure> {
    let value: Value =
        serde_json::from_str(output).map_err(|_| BugClassificationFailure::MalformedOutput)?;
    let object = value.as_object().ok_or(BugClassificationFailure::Schema)?;
    let expected = [
        "summary",
        "severity",
        "failureMechanism",
        "affectedComponents",
        "statedCause",
        "requiredRepair",
    ];
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(BugClassificationFailure::Schema);
    }
    let result: BugClassifierResult =
        serde_json::from_value(value).map_err(|_| BugClassificationFailure::Schema)?;
    if result.summary.trim().is_empty() {
        return Err(BugClassificationFailure::Schema);
    }

    let severity = result
        .severity
        .map(|fact| normalize_severity(&fact, raw_text))
        .transpose()?;
    let failure_mechanism = result
        .failure_mechanism
        .map(|fact| validate_free_text_fact(&fact, raw_text))
        .transpose()?;
    let affected_components = result
        .affected_components
        .iter()
        .map(|fact| validate_free_text_fact(fact, raw_text))
        .collect::<Result<Vec<_>, _>>()?;
    let stated_cause = result
        .stated_cause
        .map(|fact| validate_free_text_fact(&fact, raw_text))
        .transpose()?;
    let required_repair = result
        .required_repair
        .map(|fact| validate_free_text_fact(&fact, raw_text))
        .transpose()?;
    let affected_components_json = serde_json::to_string(&affected_components)
        .map_err(|_| BugClassificationFailure::Schema)?;

    Ok(ValidatedBugClassification {
        summary: result.summary,
        severity,
        failure_mechanism,
        affected_components_json,
        stated_cause,
        required_repair,
    })
}

fn validate_free_text_fact(
    fact: &CitedFact,
    raw_text: &str,
) -> Result<String, BugClassificationFailure> {
    validate_evidence(&fact.evidence, raw_text)?;
    if fact.value != fact.evidence.text {
        return Err(BugClassificationFailure::Grounding);
    }
    Ok(fact.value.clone())
}

fn normalize_severity(
    fact: &CitedFact,
    raw_text: &str,
) -> Result<String, BugClassificationFailure> {
    validate_evidence(&fact.evidence, raw_text)?;
    let normalized = fact.evidence.text.trim().to_ascii_lowercase();
    if !matches!(normalized.as_str(), "critical" | "high" | "medium" | "low")
        || fact.value != normalized
    {
        return Err(BugClassificationFailure::Grounding);
    }
    Ok(normalized)
}

fn validate_evidence(
    evidence: &ByteEvidence,
    raw_text: &str,
) -> Result<(), BugClassificationFailure> {
    if evidence.start_byte >= evidence.end_byte
        || evidence.end_byte > raw_text.len()
        || !raw_text.is_char_boundary(evidence.start_byte)
        || !raw_text.is_char_boundary(evidence.end_byte)
        || raw_text.get(evidence.start_byte..evidence.end_byte) != Some(evidence.text.as_str())
    {
        return Err(BugClassificationFailure::Grounding);
    }
    Ok(())
}

fn bug_classifier_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "severity", "failureMechanism", "affectedComponents", "statedCause", "requiredRepair"],
        "properties": {
            "summary": { "type": "string" },
            "severity": { "anyOf": [{ "type": "null" }, cited_fact_schema()] },
            "failureMechanism": { "anyOf": [{ "type": "null" }, cited_fact_schema()] },
            "affectedComponents": { "type": "array", "items": cited_fact_schema() },
            "statedCause": { "anyOf": [{ "type": "null" }, cited_fact_schema()] },
            "requiredRepair": { "anyOf": [{ "type": "null" }, cited_fact_schema()] }
        }
    })
}

fn cited_fact_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["value", "evidence"],
        "properties": {
            "value": { "type": "string" },
            "evidence": {
                "type": "object",
                "additionalProperties": false,
                "required": ["startByte", "endByte", "text"],
                "properties": {
                    "startByte": { "type": "integer", "minimum": 0 },
                    "endByte": { "type": "integer", "minimum": 0 },
                    "text": { "type": "string" }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "turn_processor_tests.rs"]
mod tests;

fn xcode_26_4_mcp_elicitations_auto_deny(
    client_name: Option<&str>,
    client_version: Option<&str>,
) -> bool {
    // Xcode 26.4 shipped before app-server MCP elicitation requests were
    // client-visible. Keep elicitations auto-denied for that client line.
    // TODO: Remove this compatibility hack once Xcode 26.4 ages out.
    client_name == Some("Xcode")
        && client_version.is_some_and(|version| version.starts_with("26.4"))
}
