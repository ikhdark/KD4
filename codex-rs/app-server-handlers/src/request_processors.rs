use crate::bespoke_event_handling::apply_bespoke_event_handling;
use crate::config_manager::ConfigManager;
use crate::error_code::INPUT_TOO_LARGE_ERROR_CODE;
use crate::error_code::invalid_params;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::ConnectionRequestId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::outgoing_message::RequestContext;
use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
use crate::skills_watcher::SkillsWatcher;
use crate::thread_status::ThreadWatchManager;
use crate::thread_status::resolve_thread_status;
use chrono::SecondsFormat;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::AnalyticsJsonRpcError;
use codex_analytics::InputError;
use codex_analytics::TurnSteerRequestError;
use codex_app_server_protocol_types::Account;
use codex_app_server_protocol_types::AccountLoginCompletedNotification;
use codex_app_server_protocol_types::AccountTokenUsageDailyBucket;
use codex_app_server_protocol_types::AccountTokenUsageSummary;
use codex_app_server_protocol_types::AccountUpdatedNotification;
use codex_app_server_protocol_types::AddCreditsNudgeCreditType;
use codex_app_server_protocol_types::AddCreditsNudgeEmailStatus;
use codex_app_server_protocol_types::AdditionalContextEntry;
use codex_app_server_protocol_types::AdditionalContextKind;
use codex_app_server_protocol_types::AppListUpdatedNotification;
use codex_app_server_protocol_types::AppSummary;
use codex_app_server_protocol_types::AppTemplateSummary;
use codex_app_server_protocol_types::AppTemplateUnavailableReason;
use codex_app_server_protocol_types::AppsListParams;
use codex_app_server_protocol_types::AppsListResponse;
use codex_app_server_protocol_types::AppsReadParams;
use codex_app_server_protocol_types::AppsReadResponse;
use codex_app_server_protocol_types::AskForApproval;
use codex_app_server_protocol_types::AuthMode;
use codex_app_server_protocol_types::BugCreateParams;
use codex_app_server_protocol_types::BugCreateResponse;
use codex_app_server_protocol_types::CancelLoginAccountParams;
use codex_app_server_protocol_types::CancelLoginAccountResponse;
use codex_app_server_protocol_types::CancelLoginAccountStatus;
use codex_app_server_protocol_types::ClientInfo;
use codex_app_server_protocol_types::ClientRequest;
use codex_app_server_protocol_types::ClientResponsePayload;
use codex_app_server_protocol_types::CodexErrorInfo;
use codex_app_server_protocol_types::CollaborationModeListParams;
use codex_app_server_protocol_types::CollaborationModeListResponse;
use codex_app_server_protocol_types::ConfigWarningNotification;
use codex_app_server_protocol_types::ConversationGitInfo;
use codex_app_server_protocol_types::ConversationSummary;
use codex_app_server_protocol_types::DeprecationNoticeNotification;
use codex_app_server_protocol_types::DynamicToolSpec;
use codex_app_server_protocol_types::EnvironmentInfoParams;
use codex_app_server_protocol_types::EnvironmentInfoResponse;
use codex_app_server_protocol_types::EnvironmentShellInfo;
use codex_app_server_protocol_types::ExperimentalFeature as ApiExperimentalFeature;
use codex_app_server_protocol_types::ExperimentalFeatureListParams;
use codex_app_server_protocol_types::ExperimentalFeatureListResponse;
use codex_app_server_protocol_types::ExperimentalFeatureStage as ApiExperimentalFeatureStage;
use codex_app_server_protocol_types::FeedbackUploadParams;
use codex_app_server_protocol_types::FeedbackUploadResponse;
use codex_app_server_protocol_types::GetAccountParams;
use codex_app_server_protocol_types::GetAccountRateLimitsResponse;
use codex_app_server_protocol_types::GetAccountResponse;
use codex_app_server_protocol_types::GetAccountTokenUsageResponse;
use codex_app_server_protocol_types::GetAuthStatusParams;
use codex_app_server_protocol_types::GetAuthStatusResponse;
use codex_app_server_protocol_types::GetConversationSummaryParams;
use codex_app_server_protocol_types::GetConversationSummaryResponse;
use codex_app_server_protocol_types::GetWorkspaceMessagesResponse;
use codex_app_server_protocol_types::GitDiffToRemoteParams;
use codex_app_server_protocol_types::GitDiffToRemoteResponse;
use codex_app_server_protocol_types::GitInfo as ApiGitInfo;
use codex_app_server_protocol_types::HookMetadata;
use codex_app_server_protocol_types::HooksListParams;
use codex_app_server_protocol_types::HooksListResponse;
use codex_app_server_protocol_types::InitializeParams;
use codex_app_server_protocol_types::InitializeResponse;
use codex_app_server_protocol_types::JSONRPCErrorError;
use codex_app_server_protocol_types::ListMcpServerStatusParams;
use codex_app_server_protocol_types::ListMcpServerStatusResponse;
use codex_app_server_protocol_types::LoginAccountParams;
use codex_app_server_protocol_types::LoginAccountResponse;
use codex_app_server_protocol_types::LoginApiKeyParams;
use codex_app_server_protocol_types::LoginAppBrand;
use codex_app_server_protocol_types::LogoutAccountResponse;
use codex_app_server_protocol_types::MarketplaceAddParams;
use codex_app_server_protocol_types::MarketplaceAddResponse;
use codex_app_server_protocol_types::MarketplaceInterface;
use codex_app_server_protocol_types::MarketplaceRemoveParams;
use codex_app_server_protocol_types::MarketplaceRemoveResponse;
use codex_app_server_protocol_types::MarketplaceUpgradeErrorInfo;
use codex_app_server_protocol_types::MarketplaceUpgradeParams;
use codex_app_server_protocol_types::MarketplaceUpgradeResponse;
use codex_app_server_protocol_types::McpResourceReadParams;
use codex_app_server_protocol_types::McpResourceReadResponse;
use codex_app_server_protocol_types::McpServerOauthLoginCompletedNotification;
use codex_app_server_protocol_types::McpServerOauthLoginParams;
use codex_app_server_protocol_types::McpServerOauthLoginResponse;
use codex_app_server_protocol_types::McpServerRefreshResponse;
use codex_app_server_protocol_types::McpServerStatus;
use codex_app_server_protocol_types::McpServerStatusDetail;
use codex_app_server_protocol_types::McpServerToolCallParams;
use codex_app_server_protocol_types::McpServerToolCallResponse;
use codex_app_server_protocol_types::MockExperimentalMethodParams;
use codex_app_server_protocol_types::MockExperimentalMethodResponse;
use codex_app_server_protocol_types::ModelListParams;
use codex_app_server_protocol_types::ModelListResponse;
use codex_app_server_protocol_types::PermissionProfileListParams;
use codex_app_server_protocol_types::PermissionProfileListResponse;
use codex_app_server_protocol_types::PermissionProfileSummary;
use codex_app_server_protocol_types::PluginDetail;
use codex_app_server_protocol_types::PluginInstallParams;
use codex_app_server_protocol_types::PluginInstallResponse;
use codex_app_server_protocol_types::PluginInstalledParams;
use codex_app_server_protocol_types::PluginInstalledResponse;
use codex_app_server_protocol_types::PluginInterface;
use codex_app_server_protocol_types::PluginListMarketplaceKind;
use codex_app_server_protocol_types::PluginListParams;
use codex_app_server_protocol_types::PluginListResponse;
use codex_app_server_protocol_types::PluginMarketplaceEntry;
use codex_app_server_protocol_types::PluginReadParams;
use codex_app_server_protocol_types::PluginReadResponse;
use codex_app_server_protocol_types::PluginShareCheckoutParams;
use codex_app_server_protocol_types::PluginShareCheckoutResponse;
use codex_app_server_protocol_types::PluginShareContext;
use codex_app_server_protocol_types::PluginShareDeleteParams;
use codex_app_server_protocol_types::PluginShareDeleteResponse;
use codex_app_server_protocol_types::PluginShareDiscoverability;
use codex_app_server_protocol_types::PluginShareListItem;
use codex_app_server_protocol_types::PluginShareListParams;
use codex_app_server_protocol_types::PluginShareListResponse;
use codex_app_server_protocol_types::PluginSharePrincipal;
use codex_app_server_protocol_types::PluginSharePrincipalType;
use codex_app_server_protocol_types::PluginShareSaveParams;
use codex_app_server_protocol_types::PluginShareSaveResponse;
use codex_app_server_protocol_types::PluginShareTarget;
use codex_app_server_protocol_types::PluginShareUpdateDiscoverability;
use codex_app_server_protocol_types::PluginShareUpdateTargetsParams;
use codex_app_server_protocol_types::PluginShareUpdateTargetsResponse;
use codex_app_server_protocol_types::PluginSkillReadParams;
use codex_app_server_protocol_types::PluginSkillReadResponse;
use codex_app_server_protocol_types::PluginSource;
use codex_app_server_protocol_types::PluginSummary;
use codex_app_server_protocol_types::PluginUninstallParams;
use codex_app_server_protocol_types::PluginUninstallResponse;
use codex_app_server_protocol_types::RateLimitResetCreditsSummary;
use codex_app_server_protocol_types::RequestId;
use codex_app_server_protocol_types::SandboxMode;
use codex_app_server_protocol_types::SendAddCreditsNudgeEmailParams;
use codex_app_server_protocol_types::SendAddCreditsNudgeEmailResponse;
use codex_app_server_protocol_types::ServerNotification;
use codex_app_server_protocol_types::ServerRequestResolvedNotification;
use codex_app_server_protocol_types::SkillSummary;
use codex_app_server_protocol_types::SkillsConfigWriteParams;
use codex_app_server_protocol_types::SkillsConfigWriteResponse;
use codex_app_server_protocol_types::SkillsExtraRootsSetParams;
use codex_app_server_protocol_types::SkillsExtraRootsSetResponse;
use codex_app_server_protocol_types::SkillsListParams;
use codex_app_server_protocol_types::SkillsListResponse;
use codex_app_server_protocol_types::SortDirection;
use codex_app_server_protocol_types::Thread;
use codex_app_server_protocol_types::ThreadArchiveParams;
use codex_app_server_protocol_types::ThreadArchiveResponse;
use codex_app_server_protocol_types::ThreadArchivedNotification;
use codex_app_server_protocol_types::ThreadBackgroundTerminal;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsCleanParams;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsCleanResponse;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsListParams;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsListResponse;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsTerminateParams;
use codex_app_server_protocol_types::ThreadBackgroundTerminalsTerminateResponse;
use codex_app_server_protocol_types::ThreadClosedNotification;
use codex_app_server_protocol_types::ThreadCompactStartParams;
use codex_app_server_protocol_types::ThreadCompactStartResponse;
use codex_app_server_protocol_types::ThreadDecrementElicitationParams;
use codex_app_server_protocol_types::ThreadDecrementElicitationResponse;
use codex_app_server_protocol_types::ThreadDeleteParams;
use codex_app_server_protocol_types::ThreadDeleteResponse;
use codex_app_server_protocol_types::ThreadDeletedNotification;
use codex_app_server_protocol_types::ThreadForkParams;
use codex_app_server_protocol_types::ThreadForkResponse;
use codex_app_server_protocol_projection::ThreadHistoryBuilder;
use codex_app_server_protocol_types::ThreadIncrementElicitationParams;
use codex_app_server_protocol_types::ThreadIncrementElicitationResponse;
use codex_app_server_protocol_types::ThreadInjectItemsParams;
use codex_app_server_protocol_types::ThreadInjectItemsResponse;
use codex_app_server_protocol_types::ThreadItem;
use codex_app_server_protocol_types::ThreadItemsListParams;
use codex_app_server_protocol_types::ThreadItemsListResponse;
use codex_app_server_protocol_types::ThreadListCwdFilter;
use codex_app_server_protocol_types::ThreadListParams;
use codex_app_server_protocol_types::ThreadListResponse;
use codex_app_server_protocol_types::ThreadLoadedListParams;
use codex_app_server_protocol_types::ThreadLoadedListResponse;
use codex_app_server_protocol_types::ThreadMetadataGitInfoUpdateParams;
use codex_app_server_protocol_types::ThreadMetadataUpdateParams;
use codex_app_server_protocol_types::ThreadMetadataUpdateResponse;
use codex_app_server_protocol_types::ThreadNameUpdatedNotification;
use codex_app_server_protocol_types::ThreadProjectUpdatedNotification;
use codex_app_server_protocol_types::ThreadReadParams;
use codex_app_server_protocol_types::ThreadReadResponse;
use codex_app_server_protocol_types::ThreadResumeInitialTurnsPageParams;
use codex_app_server_protocol_types::ThreadResumeParams;
use codex_app_server_protocol_types::ThreadResumeResponse;
use codex_app_server_protocol_types::ThreadRollbackParams;
use codex_app_server_protocol_types::ThreadSearchParams;
use codex_app_server_protocol_types::ThreadSearchResponse;
use codex_app_server_protocol_types::ThreadSearchResult;
use codex_app_server_protocol_types::ThreadSetNameParams;
use codex_app_server_protocol_types::ThreadSetNameResponse;
use codex_app_server_protocol_types::ThreadSettings;
use codex_app_server_protocol_types::ThreadSettingsUpdateParams;
use codex_app_server_protocol_types::ThreadSettingsUpdateResponse;
use codex_app_server_protocol_types::ThreadShellCommandParams;
use codex_app_server_protocol_types::ThreadShellCommandResponse;
use codex_app_server_protocol_types::ThreadSortKey;
use codex_app_server_protocol_types::ThreadSourceKind;
use codex_app_server_protocol_types::ThreadStartParams;
use codex_app_server_protocol_types::ThreadStartResponse;
use codex_app_server_protocol_types::ThreadStartedNotification;
use codex_app_server_protocol_types::ThreadStatus;
use codex_app_server_protocol_types::ThreadTurnsListParams;
use codex_app_server_protocol_types::ThreadTurnsListResponse;
use codex_app_server_protocol_types::ThreadUnarchiveParams;
use codex_app_server_protocol_types::ThreadUnarchiveResponse;
use codex_app_server_protocol_types::ThreadUnarchivedNotification;
use codex_app_server_protocol_types::ThreadUnsubscribeParams;
use codex_app_server_protocol_types::ThreadUnsubscribeResponse;
use codex_app_server_protocol_types::ThreadUnsubscribeStatus;
use codex_app_server_protocol_types::Turn;
use codex_app_server_protocol_types::TurnEnvironmentParams;
use codex_app_server_protocol_types::TurnError;
use codex_app_server_protocol_types::TurnInterruptParams;
use codex_app_server_protocol_types::TurnInterruptResponse;
use codex_app_server_protocol_types::TurnItemsView;
use codex_app_server_protocol_types::TurnStartParams;
use codex_app_server_protocol_types::TurnStartResponse;
use codex_app_server_protocol_types::TurnStatus;
use codex_app_server_protocol_types::TurnSteerParams;
use codex_app_server_protocol_types::TurnSteerResponse;
use codex_app_server_protocol_types::UserInput as V2UserInput;
use codex_app_server_protocol_types::WindowsSandboxGrantReadRootParams;
use codex_app_server_protocol_types::WindowsSandboxGrantReadRootResponse;
use codex_app_server_protocol_types::WindowsSandboxReadiness;
use codex_app_server_protocol_types::WindowsSandboxReadinessResponse;
use codex_app_server_protocol_types::WindowsSandboxSetupCompletedNotification;
use codex_app_server_protocol_types::WindowsSandboxSetupMode;
use codex_app_server_protocol_types::WindowsSandboxSetupStartParams;
use codex_app_server_protocol_types::WindowsSandboxSetupStartResponse;
use codex_app_server_protocol_types::WorkspaceMessage;
use codex_app_server_protocol_types::WorkspaceMessageType;
use codex_backend_client::AddCreditsNudgeCreditType as BackendAddCreditsNudgeCreditType;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::CodexWorkspaceMessage as BackendWorkspaceMessage;
use codex_backend_client::CodexWorkspaceMessageType as BackendWorkspaceMessageType;
use codex_backend_client::CodexWorkspaceMessagesResponse as BackendWorkspaceMessagesResponse;
use codex_backend_client::RequestError as BackendRequestError;
use codex_backend_client::TokenUsageProfile;
use codex_chatgpt::connectors;
use codex_chatgpt::workspace_settings;
use codex_chatgpt::workspace_settings::codex_plugins_enabled_for_workspace_or_default as workspace_codex_plugins_enabled;
use codex_config::CloudConfigBundleLoadError;
use codex_config::CloudConfigBundleLoadErrorCode;
use codex_config::ConfigLayerStack;
use codex_config::loader::project_trust_key;
use codex_config::types::McpServerTransportConfig;
use codex_connectors::AppInfo;
use codex_core::CodexThread;
use codex_core::CodexThreadSettingsOverrides;
use codex_core::ForkSnapshot;
use codex_core::McpManager;
use codex_core::NewThread;
use codex_core::StartThreadOptions;
use codex_core::SteerInputError;
use codex_core::ThreadConfigSnapshot;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::connectors::AccessibleConnectorsStatus;
use codex_core::truncate_rollout_after_turn_id;
use codex_core::windows_sandbox::WindowsSandboxSetupMode as CoreWindowsSandboxSetupMode;
use codex_core::windows_sandbox::WindowsSandboxSetupRequest;
use codex_core::windows_sandbox::sandbox_setup_is_complete;
use codex_core_plugins::PluginInstallError as CorePluginInstallError;
use codex_core_plugins::PluginInstallRequest;
use codex_core_plugins::PluginReadRequest;
use codex_core_plugins::PluginUninstallError as CorePluginUninstallError;
use codex_core_plugins::PluginsManager;
use codex_core_plugins::loader::load_plugin_apps;
use codex_core_plugins::loader::load_plugin_mcp_servers;
use codex_core_plugins::manifest::PluginManifestInterface;
use codex_core_plugins::marketplace::MarketplaceError;
use codex_core_plugins::marketplace::MarketplacePluginSource;
use codex_core_plugins::marketplace_add::MarketplaceAddError;
use codex_core_plugins::marketplace_add::MarketplaceAddRequest;
use codex_core_plugins::marketplace_add::add_marketplace as add_marketplace_to_codex_home;
use codex_core_plugins::marketplace_remove::MarketplaceRemoveError;
use codex_core_plugins::marketplace_remove::MarketplaceRemoveRequest as CoreMarketplaceRemoveRequest;
use codex_core_plugins::marketplace_remove::remove_marketplace;
use codex_core_plugins::remote::RemoteMarketplace;
use codex_core_plugins::remote::RemoteMarketplaceSource;
use codex_core_plugins::remote::RemotePluginCatalogError;
use codex_core_plugins::remote::RemotePluginDetail as RemoteCatalogPluginDetail;
use codex_core_plugins::remote::RemotePluginServiceConfig;
use codex_core_plugins::remote::RemotePluginShareContext as RemoteCatalogPluginShareContext;
use codex_core_plugins::remote::RemotePluginShareSummary as RemoteCatalogPluginShareSummary;
use codex_core_plugins::remote::RemotePluginSummary as RemoteCatalogPluginSummary;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::LOCAL_FS;
use codex_features::Feature;
use codex_features::Stage;
use codex_feedback::CodexFeedback;
use codex_feedback::FeedbackAttachmentPath;
use codex_feedback::FeedbackUploadOptions;
use codex_git_utils::git_diff_to_remote;
use codex_git_utils::resolve_root_git_project_for_trust;
use codex_login::AuthManager;
use codex_login::CODEX_OPEN_APP_URL;
use codex_login::CodexAuth;
use codex_login::LoginSuccessPage;
use codex_login::LoginSuccessPageBrand;
use codex_login::ServerOptions as LoginServerOptions;
use codex_login::ShutdownHandle;
use codex_login::login_with_api_key;
use codex_login::oauth_client_id;
use codex_login::prepare_device_code_login;
use codex_login::request_device_code;
use codex_login::run_login_server_async;
use codex_mcp::McpRuntimeContext;
use codex_mcp::McpServerStatusSnapshot;
use codex_mcp::McpSnapshotDetail;
use codex_mcp::collect_mcp_server_status_snapshot_for_servers_with_detail;
use codex_mcp::discover_supported_scopes_with_http_client;
use codex_mcp::read_mcp_resource as read_mcp_resource_without_thread;
use codex_mcp::resolve_oauth_scopes;
use codex_model_provider::create_model_provider;
use codex_models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use codex_protocol::ThreadId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ForcedLoginMethod;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::McpAuthStatus as CoreMcpAuthStatus;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::protocol::strip_user_message_prefix;
use codex_protocol::user_input::MAX_USER_INPUT_TEXT_CHARS;
use codex_protocol::user_input::UserInput as CoreInputItem;
use codex_rmcp_client::perform_oauth_login_return_url_with_http_client;
use codex_rollout::is_persisted_rollout_item;
use codex_rollout::state_integration::StateDbHandle;
use codex_state::log_db::LogDbLayer;
use codex_thread_store::ArchiveThreadParams as StoreArchiveThreadParams;
use codex_thread_store::ClearableField as StoreClearableField;
use codex_thread_store::DeleteThreadParams as StoreDeleteThreadParams;
use codex_thread_store::GitInfoPatch as StoreGitInfoPatch;
use codex_thread_store::ListItemsParams as StoreListItemsParams;
use codex_thread_store::ListThreadsParams as StoreListThreadsParams;
use codex_thread_store::ListTurnsParams as StoreListTurnsParams;
use codex_thread_store::ReadThreadByRolloutPathParams as StoreReadThreadByRolloutPathParams;
use codex_thread_store::ReadThreadParams as StoreReadThreadParams;
use codex_thread_store::SearchThreadsParams as StoreSearchThreadsParams;
use codex_thread_store::SortDirection as StoreSortDirection;
use codex_thread_store::StoredThread;
use codex_thread_store::StoredTurnItemsView as StoreStoredTurnItemsView;
use codex_thread_store::StoredTurnStatus as StoreStoredTurnStatus;
use codex_thread_store::ThreadListStorageMode as StoreThreadListStorageMode;
use codex_thread_store::ThreadMetadataPatch as StoreThreadMetadataPatch;
use codex_thread_store::ThreadRelationFilter as StoreThreadRelationFilter;
use codex_thread_store::ThreadSortKey as StoreThreadSortKey;
use codex_thread_store::ThreadStore;
use codex_thread_store::ThreadStoreError;
use codex_utils_absolute_path as path_utils;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
use std::result::Result;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tokio_util::task::TaskTracker;
use toml::Value as TomlValue;
use tracing::error;
use tracing::info;
use tracing::warn;
use uuid::Uuid;


mod account_processor;
mod apps_processor;
mod catalog_processor;
mod config_processor;
mod environment_processor;
mod external_agent_config_processor;
mod feedback_doctor_report;
mod feedback_processor;
mod fs_processor;
mod git_processor;
mod initialize_processor;
mod marketplace_processor;
mod mcp_processor;
mod plugins;
mod projects;
mod remote_control_processor;
mod search;
mod thread_processor;
mod thread_queue;
mod token_usage_replay;
mod turn_processor;
mod windows_sandbox_processor;

pub(crate) use account_processor::AccountRequestProcessor;
pub(crate) use apps_processor::AppsRequestProcessor;
pub(crate) use catalog_processor::CatalogRequestProcessor;
pub(crate) use codex_app_server_exec::CommandExecRequestProcessor;
pub(crate) use config_processor::ConfigRequestProcessor;
pub(crate) use environment_processor::EnvironmentRequestProcessor;
pub(crate) use external_agent_config_processor::ExternalAgentConfigRequestProcessor;
pub(crate) use feedback_processor::FeedbackRequestProcessor;
pub(crate) use fs_processor::FsRequestProcessor;
pub(crate) use git_processor::git_diff_to_remote_response;
pub(crate) use initialize_processor::InitializeRequestProcessor;
pub(crate) use marketplace_processor::MarketplaceRequestProcessor;
pub(crate) use mcp_processor::McpRequestProcessor;
pub(crate) use plugins::PluginRequestProcessor;
pub(crate) use codex_app_server_exec::ProcessExecRequestProcessor;
pub(crate) use projects::ProjectRequestProcessor;
pub(crate) use remote_control_processor::RemoteControlRequestProcessor;
pub(crate) use search::SearchRequestProcessor;
pub(crate) use thread_processor::ThreadRequestProcessor;
mod thread_list_state;
pub(crate) use thread_list_state::ThreadListState;
pub(crate) use thread_queue::QueueOrigin;
pub(crate) use thread_queue::ThreadQueueRequestProcessor;
pub(crate) use turn_processor::TurnRequestProcessor;
pub(crate) use windows_sandbox_processor::WindowsSandboxRequestProcessor;

use crate::error_code::internal_error;
use crate::error_code::invalid_request;
use crate::filters::compute_source_filters;
use crate::filters::source_kind_matches;
use crate::thread_state::ThreadListenerCommand;
use crate::thread_state::ThreadState;
use crate::thread_state::ThreadStateManager;
use token_usage_replay::send_thread_token_usage_update_to_connection;

fn resolve_request_cwd(cwd: Option<PathBuf>) -> Result<Option<AbsolutePathBuf>, JSONRPCErrorError> {
    cwd.map(|cwd| {
        AbsolutePathBuf::relative_to_current_dir(path_utils::normalize_for_native_workdir(cwd))
            .map_err(|err| invalid_request(format!("invalid cwd: {err}")))
    })
    .transpose()
}

fn resolve_turn_environment_selections(
    thread_manager: &ThreadManager,
    environments: Option<Vec<TurnEnvironmentParams>>,
) -> Result<Option<Vec<TurnEnvironmentSelection>>, JSONRPCErrorError> {
    let Some(environments) = environments else {
        return Ok(None);
    };
    let mut selections = Vec::with_capacity(environments.len());
    for environment in environments {
        let environment_id = environment.environment_id;
        let cwd = environment
            .cwd
            .to_inferred_path_uri()
            .ok_or_else(|| {
                invalid_request(format!(
                    "invalid cwd for environment `{environment_id}`: path `{}` does not use absolute Windows drive or UNC path syntax",
                    environment.cwd
                ))
            })?;
        selections.push(TurnEnvironmentSelection {
            environment_id,
            cwd,
        });
    }
    thread_manager
        .validate_environment_selections(&selections)
        .map_err(environment_selection_error)?;
    Ok(Some(selections))
}

fn environment_selection_error(err: CodexErr) -> JSONRPCErrorError {
    match err {
        CodexErr::InvalidRequest(message) => invalid_request(message),
        err => internal_error(format!("failed to validate environment selections: {err}")),
    }
}


fn resolve_runtime_workspace_roots(workspace_roots: Vec<AbsolutePathBuf>) -> Vec<AbsolutePathBuf> {
    let mut resolved_roots = Vec::new();
    for root in workspace_roots {
        if !resolved_roots.iter().any(|existing| existing == &root) {
            resolved_roots.push(root);
        }
    }
    resolved_roots
}

mod config_errors;
mod thread_delete;
mod thread_lifecycle;
mod thread_summary;

use self::config_errors::*;
use self::thread_lifecycle::*;
use self::thread_summary::*;

pub(crate) use self::thread_lifecycle::PendingThreadUnloads;
pub(crate) use self::thread_lifecycle::populate_thread_turns_from_history;
pub(crate) use self::thread_processor::thread_from_stored_thread;
pub(crate) use self::thread_summary::thread_settings_from_config_snapshot;
pub(crate) use self::thread_summary::thread_settings_from_core_snapshot;

pub(crate) fn build_legacy_api_turns_from_rollout_items(items: &[RolloutItem]) -> Vec<Turn> {
    let mut builder = ThreadHistoryBuilder::new();
    for item in items {
        if is_persisted_rollout_item(item, codex_protocol::protocol::ThreadHistoryMode::Legacy) {
            builder.handle_rollout_item(item);
        }
    }
    builder.finish()
}
