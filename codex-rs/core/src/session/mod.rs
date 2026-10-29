use indexmap::IndexMap;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::agent_status_from_event;
use crate::agent::agent_status_from_task;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::attestation::AttestationProvider;
use crate::compact;
use crate::config::ManagedFeatures;
use crate::config::resolve_tool_suggest_config_from_layer_stack;
use crate::context::ApprovalPromptContext;
use crate::context::ApprovedCommandPrefixSaved;
use crate::context::AvailableSkillsInstructions;
use crate::context::CollaborationModeInstructions;
use crate::context::ContextualUserFragment;
use crate::context::MultiAgentModeInstructions;
use crate::context::PermissionsInstructions;
use crate::context::PersonalitySpecInstructions;
use crate::context::SkillsUsageInstructions;
use crate::context::world_state::WorldState;
use crate::context::world_state::WorldStateSnapshot;
use crate::current_time::TimeProvider;
use crate::default_skill_metadata_budget;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::exec_policy::ExecPolicyManager;
use crate::image_preparation::prepare_response_items;
use crate::image_preparation::response_items_need_preparation;
use crate::parse_turn_item;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnEnvironment;
use crate::skills::SkillRenderSideEffects;
use crate::skills_load_input_from_config;
use crate::turn_metadata::TurnMetadataState;
use crate::turn_timing::now_unix_timestamp_ms;
use async_channel::Receiver;
use async_channel::Sender;
use chrono::Local;
use chrono::Utc;
use codex_analytics::AnalyticsEventsClient;
use codex_analytics::SubAgentThreadStartedInput;
use codex_analytics::TurnCodexErrorFact;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_context_fragments::ModelContextBudget;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::PromptFragment;
use codex_extension_api::PromptSlot;
use codex_extension_api::TurnContextContributionInput;
use codex_features::FEATURES;
use codex_features::Feature;
use codex_features::unstable_features_warning_event;
use codex_hooks::Hooks;
use codex_hooks::HooksConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpResourceClient;
use codex_mcp::McpRuntimeContext;
use codex_mcp::codex_apps_tools_cache_key;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::current_span_trace_id;
use codex_otel::current_span_w3c_trace_context;
use codex_otel::set_parent_from_w3c_trace_context;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyAmendment;
use codex_protocol::approvals::NetworkPolicyRuleAction;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::EnteredReviewModeItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::models::format_allow_prefixes;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AcceptedAttemptProvenance;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::ContextFragmentDigest;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnContextNetworkItem;
use codex_protocol::protocol::TurnContextProvenance;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_protocol::protocol::W3cTraceContext;
use codex_protocol::protocol::WorldStateItem;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsArgs;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use codex_protocol::request_permissions::UriAdditionalPermissionProfile;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_rollout::state_integration;
use codex_sandboxing::policy_transforms::intersect_uri_permission_profiles;
use codex_sandboxing::policy_transforms::merge_uri_permission_profiles;
use codex_shell_command::parse_command::parse_command;
use codex_terminal_detection::user_agent;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::LiveThread;
use codex_thread_store::LiveThreadInitGuard;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::ResumeThreadParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use futures::future::Shared;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use rmcp::model::RequestId;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) struct DurableHistoryCommitInFlight {
    session: Arc<Session>,
}

impl Drop for DurableHistoryCommitInFlight {
    fn drop(&mut self) {
        self.session
            .durable_history_commits_in_flight
            .fetch_sub(1, Ordering::AcqRel);
        self.session
            .durable_history_commits_changed
            .notify_waiters();
    }
}
use toml::Value as TomlValue;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::warn;
use uuid::Uuid;

use crate::client::ModelClient;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ConstraintResult;
use crate::config::PermissionProfileSnapshot;
use crate::config::PermissionProfileState;
use crate::config::resolve_web_search_mode_for_turn;
use crate::context_manager::ContextManager;
use crate::context_manager::missing_call_outputs;
use crate::thread_rollout_truncation::PriorUserTurnTracker;
use codex_config::CONFIG_TOML_FILE;
use codex_config::ConfigLayerSource;
use codex_config::ConfigLayerStackOrdering;
use codex_config::types::McpServerConfig;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;

mod code_mode_warning;
mod config_lock;
pub(crate) mod context_window;
mod handlers;
mod inject;
mod input_queue;
mod mcp;
mod mcp_runtime;
pub(crate) mod multi_agents;
mod review;
mod rollout_reconstruction;
#[allow(clippy::module_inception)]
pub(crate) mod session;
pub(crate) mod step_context;
pub(crate) mod time_reminder;
pub(crate) mod turn;
pub(crate) mod turn_context;
pub(crate) mod turn_execution;
mod user_decision;
mod input_receipts;
mod world_state;
use self::code_mode_warning::unsupported_code_mode_warning;
use self::config_lock::export_config_lock_if_configured;
use self::config_lock::validate_config_lock_if_configured;
use self::handlers::submission_loop;
pub(crate) use self::input_queue::InputQueue;
pub(crate) use self::input_queue::InputQueueActivity;
pub(crate) use self::input_queue::TurnInput;
pub(crate) use self::input_queue::TurnInputQueue;
pub(crate) use self::mcp_runtime::McpManagerLifecycle;
pub use self::mcp_runtime::McpRuntimeSnapshot;
use self::review::spawn_review_thread;
pub(crate) use self::rollout_reconstruction::is_unified_exec_resume_invalidation;
use self::session::AppServerClientMetadata;
pub(crate) use self::session::Session;
use self::session::SessionConfiguration;
pub(crate) use self::session::SessionSettingsUpdate;
use self::turn_context::TurnContext;
use self::turn_context::TurnSkillsContext;

pub(super) const EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT: Duration = Duration::from_secs(5);

const CONTEXT_FRAGMENT_DIGEST_DOMAIN: &[u8] = b"codex.context-fragment.v1";

fn digest_context_fragments(items: &[ResponseItem]) -> Vec<ContextFragmentDigest> {
    fn canonicalize(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let sorted = object
                    .into_iter()
                    .filter(|(key, _)| key != "id" && key != "turn_id")
                    .map(|(key, value)| (key, canonicalize(value)))
                    .collect::<BTreeMap<_, _>>();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(canonicalize).collect())
            }
            value => value,
        }
    }

    let mut occurrences = BTreeMap::<String, usize>::new();
    items
        .iter()
        .map(|item| {
            let mut value = serde_json::to_value(item).unwrap_or(serde_json::Value::Null);
            if let Some(object) = value.as_object_mut() {
                object.remove("id");
                object.remove("turn_id");
            }
            let category = value
                .get("type")
                .or_else(|| value.get("role"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    value
                        .as_object()
                        .and_then(|object| object.keys().next().cloned())
                })
                .unwrap_or_else(|| "item".to_string());
            let occurrence = occurrences.entry(category.clone()).or_default();
            let key = format!("{category}/{occurrence}");
            *occurrence += 1;
            let mut hasher = Sha256::new();
            hasher.update(CONTEXT_FRAGMENT_DIGEST_DOMAIN);
            hasher.update(key.as_bytes());
            if let Ok(encoded) = serde_json::to_vec(&canonicalize(value)) {
                hasher.update(encoded);
            }
            ContextFragmentDigest {
                key,
                digest: hex::encode(hasher.finalize()),
            }
        })
        .collect()
}

struct InitialContextBuild {
    full_context_required: bool,
    items: Vec<ResponseItem>,
    turn_context_items: Vec<ResponseItem>,
    world_state_snapshot: WorldStateSnapshot,
    fragment_digests: Vec<ContextFragmentDigest>,
}

/// One immutable context projection prepared for both pre-turn accounting and the eventual
/// history commit. The planning generation is validated while this value is committed, so a
/// stale projection can never advance history or a realized context baseline.
pub(crate) struct PreparedContextUpdate {
    turn_context: Arc<TurnContext>,
    world_state: Arc<WorldState>,
    context_items: Vec<ResponseItem>,
    baseline: ContextBaselineCandidate,
}

impl PreparedContextUpdate {
    pub(crate) fn context_items(&self) -> &[ResponseItem] {
        &self.context_items
    }
}

pub(crate) fn active_turn_has_other_id(active: &crate::state::ActiveTurn, turn_id: &str) -> bool {
    active
        .terminal
        .as_ref()
        .map(|terminal| terminal.turn_id())
        .or_else(|| {
            active
                .task
                .as_ref()
                .map(|task| task.turn_context.sub_id.as_str())
        })
        .is_some_and(|active_id| active_id != turn_id)
}

fn context_snapshot_matches(
    realized: Option<&TurnContextItem>,
    candidate: &TurnContextItem,
) -> bool {
    let Some(realized) = realized else {
        return false;
    };
    let mut realized = realized.clone();
    realized.context_provenance = None;
    &realized == candidate
}


#[derive(Debug, PartialEq)]
pub enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ExpectedTurnMismatch { expected: String, actual: String },
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    EmptyInput,
    IdempotencyConflict,
    ReceiptUnavailable { message: String },
    AdditionalContextUnavailable { message: String },
    PendingInputLimitExceeded { max_items: usize, max_bytes: usize },
}

impl SteerInputError {
    fn to_error_event(&self) -> ErrorEvent {
        match self {
            Self::IdempotencyConflict => ErrorEvent {
                message: "clientUserMessageId was already used with different steering contents".into(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            },
            Self::ReceiptUnavailable { message } => ErrorEvent {
                message: message.clone(),
                codex_error_info: Some(CodexErrorInfo::InternalServerError),
            },
            Self::AdditionalContextUnavailable { message } => ErrorEvent {
                message: message.clone(),
                codex_error_info: Some(CodexErrorInfo::InternalServerError),
            },
            Self::NoActiveTurn(_) => ErrorEvent {
                message: "no active turn to steer".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            },
            Self::ExpectedTurnMismatch { expected, actual } => ErrorEvent {
                message: format!("expected active turn id `{expected}` but found `{actual}`"),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            },
            Self::ActiveTurnNotSteerable { turn_kind } => {
                let turn_kind_label = match turn_kind {
                    NonSteerableTurnKind::Review => "review",
                    NonSteerableTurnKind::Compact => "compact",
                };
                ErrorEvent {
                    message: format!("cannot steer a {turn_kind_label} turn"),
                    codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                        turn_kind: *turn_kind,
                    }),
                }
            }
            Self::EmptyInput => ErrorEvent {
                message: "input must not be empty".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            },
            Self::PendingInputLimitExceeded {
                max_items,
                max_bytes,
            } => ErrorEvent {
                message: format!(
                    "pending input limit exceeded (maximum {max_items} items or {max_bytes} bytes)"
                ),
                codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
            },
        }
    }
}

/// Notes from the previous real user turn.
///
/// Conceptually this is the same role that `previous_model` used to fill, but
/// it can carry other prior-turn settings that matter when constructing
/// sensible state-change diffs or full-context reinjection, such as model
/// switches or compaction compatibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviousTurnSettings {
    pub(crate) model: String,
    pub(crate) comp_hash: Option<String>,
}

fn retarget_previous_turn_settings_for_provider(
    previous_turn_settings: &mut Option<PreviousTurnSettings>,
    reconstructed_provider_id: Option<&str>,
    current_provider_id: &str,
    current_model: &str,
) {
    if reconstructed_provider_id != Some(current_provider_id)
        && let Some(settings) = previous_turn_settings.as_mut()
    {
        settings.model = current_model.to_string();
    }
}

use crate::SkillsService;
use crate::exec_policy::ExecPolicyUpdateError;
use crate::mcp::McpManager;
use crate::rollout::map_session_init_error;
use crate::session_startup_prewarm::SessionStartupPrewarmHandle;
use crate::session_startup_prewarm::SessionStartupTransportHandle;
use crate::shell;
use crate::state::AutoCompactWindowIds;
use crate::state::AutoCompactWindowSnapshot;
use crate::state::ContextBaselineCandidate;
use crate::state::PendingRequestPermissions;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::tasks::ReviewTask;
use crate::tools::sandboxing::ApprovalStore;
use crate::turn_timing::InteractiveWaitKind;
use crate::turn_timing::TurnLocalPhase;
use crate::turn_timing::TurnTimingState;
use crate::turn_timing::record_turn_ttfm_metric;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_core_plugins::PluginsManager;
use codex_mcp::McpConfig;
use codex_mcp::effective_mcp_servers;
use codex_otel::SessionTelemetry;
use codex_otel::THREAD_STARTED_METRIC;
use codex_otel::TelemetryAuthMode;
use codex_protocol::ResponseItemId;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::LocalImagePreparation;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::persisted_thread_settings::PersistedThreadSettings;
use codex_protocol::persisted_thread_settings::PersistedThreadSettingsReducer;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::McpServerRefreshConfig;
use codex_protocol::protocol::ModelRerouteEvent;
use codex_protocol::protocol::ModelRerouteReason;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::ModelVerificationEvent;
use codex_protocol::protocol::NetworkApprovalContext;
use codex_protocol::protocol::NonSteerableTurnKind;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnModerationMetadataEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;

/// The high-level interface to the Codex system.
/// It operates as a queue pair where you send submissions and receive events.
pub struct Codex {
    pub(crate) tx_sub: Sender<QueuedSubmission>,
    pub(crate) rx_event: Receiver<Event>,
    // Last known status of the agent.
    pub(crate) agent_status: watch::Receiver<AgentStatus>,
    pub(crate) session: Arc<Session>,
    // Shared future for the background submission loop completion so multiple
    // callers can wait for shutdown.
    pub(crate) session_loop_termination: SessionLoopTermination,
}

pub(crate) type SessionLoopTermination = Shared<BoxFuture<'static, Result<(), &'static str>>>;

/// Internal acknowledgement stays out of the public submission protocol.
#[derive(Debug)]
pub(crate) struct QueuedSubmission {
    pub(crate) submission: Submission,
    pub(crate) mailbox_admission: Option<oneshot::Sender<CodexResult<()>>>,
    // Held through forwarding, deferred preparation, and dispatch. Dropping any
    // cancelled or rejected submission releases its admission automatically.
    pub(crate) preparation_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

fn is_submission_control(op: &Op) -> bool {
    matches!(
        op,
        Op::Interrupt
            | Op::Shutdown
            | Op::ExecApproval { .. }
            | Op::PatchApproval { .. }
            | Op::UserInputAnswer { .. }
            | Op::RequestPermissionsResponse { .. }
            | Op::DynamicToolResponse { .. }
            | Op::ResolveElicitation { .. }
    )
}

/// Wrapper returned by [`Codex::spawn`] containing the spawned [`Codex`] and
/// the unique session id.
pub struct CodexSpawnOk {
    pub codex: Codex,
    pub thread_id: ThreadId,
}

pub(crate) struct CodexSpawnArgs {
    pub(crate) config: Config,
    pub(crate) allow_provider_model_fallback: bool,
    pub(crate) user_instructions: LoadedUserInstructions,
    pub(crate) installation_id: String,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) environment_manager: Arc<EnvironmentManager>,
    pub(crate) skills_service: Arc<SkillsService>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) code_mode_session_provider: Arc<dyn codex_code_mode::CodeModeSessionProvider>,
    pub(crate) extensions: Arc<codex_extension_api::ExtensionRegistry<crate::config::Config>>,
    pub(crate) conversation_history: InitialHistory,
    pub(crate) requested_history_mode: Option<ThreadHistoryMode>,
    pub(crate) session_source: SessionSource,
    pub(crate) forked_from_thread_id: Option<ThreadId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) originator: String,
    pub(crate) agent_control: AgentControl,
    pub(crate) dynamic_tools: Vec<DynamicToolSpec>,
    pub(crate) metrics_service_name: Option<String>,
    pub(crate) inherited_exec_policy: Option<Arc<ExecPolicyManager>>,
    pub(crate) inherited_environments: Option<TurnEnvironmentSnapshot>,
    pub(crate) user_shell_override: Option<shell::Shell>,
    pub(crate) parent_trace: Option<W3cTraceContext>,
    pub(crate) environment_selections: Vec<TurnEnvironmentSelection>,
    pub(crate) thread_extension_init: ExtensionDataInit,
    pub(crate) supports_openai_form_elicitation: bool,
    pub(crate) analytics_events_client: Option<AnalyticsEventsClient>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) external_time_provider: Option<Arc<dyn TimeProvider>>,
    pub(crate) inherited_multi_agent_version: Option<MultiAgentVersion>,
}

pub(crate) fn resolve_multi_agent_version(
    conversation_history: &InitialHistory,
    inherited_multi_agent_version: Option<MultiAgentVersion>,
) -> Option<MultiAgentVersion> {
    if inherited_multi_agent_version == Some(MultiAgentVersion::Disabled) {
        return Some(MultiAgentVersion::Disabled);
    }

    conversation_history
        .get_multi_agent_version()
        .or(inherited_multi_agent_version)
        .or(match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => None,
            // Threads created before runtime metadata existed join the current V2 tool surface.
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => Some(MultiAgentVersion::V2),
        })
        .map(MultiAgentVersion::runtime_version)
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 512;
const CYBER_VERIFY_URL: &str = "https://chatgpt.com/cyber";
const CYBER_SAFETY_URL: &str = "https://developers.openai.com/codex/concepts/cyber-safety";

fn event_channel() -> (async_channel::Sender<Event>, async_channel::Receiver<Event>) {
    // The shared producer admission caps each serialized event as well, giving
    // the live queue a finite 256 MiB envelope without lossy terminal success.
    async_channel::bounded(codex_mcp::event_queue::EVENT_QUEUE_CAPACITY)
}

impl Codex {
    /// Spawn a new [`Codex`] and initialize the session.
    pub(crate) async fn spawn(args: CodexSpawnArgs) -> CodexResult<CodexSpawnOk> {
        let parent_trace = match args.parent_trace {
            Some(trace) => {
                if codex_otel::context_from_w3c_trace_context(&trace).is_some() {
                    Some(trace)
                } else {
                    warn!("ignoring invalid thread spawn trace carrier");
                    None
                }
            }
            None => None,
        };
        let thread_spawn_span = info_span!("thread_spawn", otel.name = "thread_spawn");
        if let Some(trace) = parent_trace.as_ref() {
            let _ = set_parent_from_w3c_trace_context(&thread_spawn_span, trace);
        }
        Self::spawn_internal(CodexSpawnArgs {
            parent_trace,
            ..args
        })
        .instrument(thread_spawn_span)
        .await
    }

    async fn spawn_internal(args: CodexSpawnArgs) -> CodexResult<CodexSpawnOk> {
        let CodexSpawnArgs {
            mut config,
            allow_provider_model_fallback,
            user_instructions,
            installation_id,
            auth_manager,
            models_manager,
            environment_manager,
            skills_service,
            plugins_manager,
            mcp_manager,
            code_mode_session_provider,
            extensions,
            conversation_history,
            requested_history_mode,
            session_source,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            agent_control,
            dynamic_tools,
            metrics_service_name,
            user_shell_override,
            inherited_exec_policy,
            inherited_environments,
            parent_trace: _,
            environment_selections,
            thread_extension_init,
            supports_openai_form_elicitation,
            analytics_events_client,
            thread_store,
            attestation_provider,
            external_time_provider,
            inherited_multi_agent_version,
        } = args;
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = event_channel();

        let LoadedUserInstructions {
            instructions: user_instructions,
            warnings: user_instruction_provider_warnings,
        } = user_instructions;
        // TODO(anp) pull startup_warnings out of Config
        config
            .startup_warnings
            .extend(user_instruction_provider_warnings);
        let exec_policy = if let Some(exec_policy) = &inherited_exec_policy {
            Arc::clone(exec_policy)
        } else {
            Arc::new(
                ExecPolicyManager::load(&config.config_layer_stack)
                    .await
                    .map_err(|err| CodexErr::Fatal(format!("failed to load rules: {err}")))?,
            )
        };

        // New registrations remain strict. On restore, quarantine only invalid
        // capabilities so a descriptor defect cannot make history inaccessible.
        let dynamic_tools = if dynamic_tools.is_empty() {
            let mut restored = Vec::new();
            let mut validator = codex_tools::DynamicToolRegistrationValidator::default();
            for tool in conversation_history.get_dynamic_tools().unwrap_or_default() {
                if let Err(error) = validator.validate(&tool) {
                    config
                        .startup_warnings
                        .push(format!("A restored dynamic tool is unavailable: {error}"));
                } else {
                    restored.push(tool);
                }
            }
            restored
        } else {
            codex_tools::validate_dynamic_tools(&dynamic_tools)
                .map_err(CodexErr::InvalidRequest)?;
            dynamic_tools
        };
        let config = Arc::new(config);
        let refresh_strategy = if session_source.is_non_root_agent() {
            codex_models_manager::manager::RefreshStrategy::Offline
        } else {
            codex_models_manager::manager::RefreshStrategy::OnlineIfUncached
        };
        if should_prefetch_models_before_default(config.model.is_some(), refresh_strategy)
            && let Err(error) = models_manager
                .list_models(refresh_strategy, config.http_client_factory())
                .await
        {
            warn!(%error, "failed to prefetch model metadata before resolving the session model");
        }
        let model = models_manager
            .get_default_model(
                &config.model,
                allow_provider_model_fallback,
                refresh_strategy,
                config.http_client_factory(),
            )
            .await?;
        if allow_provider_model_fallback
            && let Some(requested_model) = config.model.as_ref()
            && model != *requested_model
        {
            info!(
                model_provider = %config.model_provider_id,
                requested_model,
                fallback_model = %model,
                "replaced unavailable requested model with provider default"
            );
        }

        // Resolve the neutral base instructions from the active model. Config overrides have
        // already been applied by the models manager. Persisted instructions describe the old
        // request and must not pin resumed threads to a stale model prompt.
        let model_info = models_manager
            .get_model_info(model.as_str(), &config.to_models_manager_config())
            .await;
        let multi_agent_version =
            resolve_multi_agent_version(&conversation_history, inherited_multi_agent_version);
        let history_mode = conversation_history.get_history_mode(
            requested_history_mode.unwrap_or_else(|| thread_store.default_history_mode()),
        );
        config
            .validate_multi_agent_v2_config()
            .map_err(|err| CodexErr::InvalidRequest(err.to_string()))?;
        let base_instructions = model_info.base_instructions.clone();

        // TODO (aibrahim): Consolidate config.model and config.model_reasoning_effort into config.collaboration_mode
        // to avoid extracting these fields separately and constructing CollaborationMode here.
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: model.clone(),
                reasoning_effort: config.model_reasoning_effort.clone(),
                developer_instructions: None,
            },
        };
        let session_reasoning_effort = crate::client::request_effort_for_model(
            &model_info,
            collaboration_mode.reasoning_effort(),
        );
        let fast_mode_enabled = config.features.enabled(Feature::FastMode);
        let initial_service_tier_warning = unsupported_service_tier_warning(
            config.service_tier.as_deref(),
            fast_mode_enabled,
            &model_info,
        );
        // Preserve the requested preference across metadata refreshes. Sampling
        // still filters unsupported tiers using the current model metadata.
        let service_tier = config.service_tier.clone();
        let persisted_validation = conversation_history
            .get_rollout_items()
            .iter()
            .rev()
            .find_map(|item| match item {
                RolloutItem::TurnContext(context) if context.validation_mode.is_some() => {
                    Some(context)
                }
                _ => None,
            });
        let inherited_validation = agent_control.inherited_validation_context.as_ref();
        let session_configuration = SessionConfiguration {
            provider: config.model_provider.clone(),
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            service_tier,
            validation_mode: inherited_validation
                .map(|context| context.mode)
                .or_else(|| persisted_validation.and_then(|context| context.validation_mode))
                .unwrap_or(config.validation_mode),
            validation_root_turn: inherited_validation
                .map(|context| context.scope.clone())
                .or_else(|| persisted_validation.and_then(|context| context.validation_root_turn.clone())),
            validation_attempts: persisted_validation
                .filter(|context| inherited_validation.is_none_or(|inherited| {
                    context.validation_root_turn.as_ref() == Some(&inherited.scope)
                }))
                .map(|context| context.validation_attempts.clone())
                .unwrap_or_default(),
            developer_instructions: config.developer_instructions.clone(),
            personality: config.personality,
            base_instructions,
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.permissions.approval_policy.clone(),
            permission_profile_state: session_permission_profile_state_from_config(&config)?,
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            environments: TurnEnvironmentSelections::new(
                config.cwd.clone(),
                environment_selections,
            ),
            workspace_roots: config.workspace_roots.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            metrics_service_name,
            app_server_client_name: None,
            app_server_client_version: None,
            session_source,
            history_mode,
            forked_from_thread_id,
            parent_thread_id,
            thread_source,
            originator,
            dynamic_tools,
            user_shell_override,
        };

        // Generate a unique ID for the lifetime of this Codex session.
        let session_source_clone = session_configuration.session_source.clone();
        let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);

        let session = Box::pin(Session::new(
            session_configuration,
            session_reasoning_effort,
            config.clone(),
            user_instructions,
            installation_id,
            auth_manager.clone(),
            models_manager.clone(),
            exec_policy,
            tx_event.clone(),
            agent_status_tx.clone(),
            conversation_history,
            session_source_clone,
            skills_service,
            plugins_manager,
            mcp_manager.clone(),
            code_mode_session_provider,
            extensions,
            thread_extension_init,
            supports_openai_form_elicitation,
            agent_control,
            environment_manager,
            inherited_environments,
            analytics_events_client,
            thread_store,
            attestation_provider,
            external_time_provider,
            multi_agent_version,
        ))
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        if let Some(message) = initial_service_tier_warning {
            // The ordered writer retains this even before the lazy rollout
            // exists; the normal persistence barrier will write it later.
            session
                .send_event_raw(Event {
                    id: INITIAL_SUBMIT_ID.to_owned(),
                    msg: EventMsg::Warning(WarningEvent { message }),
                })
                .await;
        }
        let thread_id = session.thread_id;

        // This task will run until Op::Shutdown is received.
        let session_for_loop = Arc::clone(&session);
        let session_loop_handle = tokio::spawn(async move {
            submission_loop(session_for_loop, config, rx_sub)
                .instrument(info_span!("session_loop", thread_id = %thread_id))
                .await;
        });
        let codex = Codex {
            tx_sub,
            rx_event,
            agent_status: agent_status_rx,
            session,
            session_loop_termination: session_loop_termination_from_handle(session_loop_handle),
        };

        Ok(CodexSpawnOk { codex, thread_id })
    }

    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        self.submit_with_trace(op, /*trace*/ None).await
    }

    pub async fn submit_with_trace(
        &self,
        op: Op,
        trace: Option<W3cTraceContext>,
    ) -> CodexResult<String> {
        let id = new_submission_id();
        let sub = Submission {
            id: id.clone(),
            op,
            client_user_message_id: None,
            trace,
        };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    pub fn reserve_turn_id(&self) -> String {
        new_submission_id()
    }

    /// Use sparingly: prefer `submit()` so Codex is responsible for generating
    /// unique IDs for each submission.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        Self::await_mailbox_admission(self.enqueue_submission(sub).await?).await
    }

    /// A one-shot caller must observe rejection before waiting for a terminal
    /// that will never exist for an unaccepted turn.
    pub(crate) async fn submit_with_start_admission(
        &self,
        sub: Submission,
        execution_guard: Option<crate::agent::control::AgentExecutionGuard>,
        cancellation: CancellationToken,
    ) -> CodexResult<()> {
        if cancellation.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        let turn_id = sub.id.clone();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        {
            let mut state = self.session.task_start_state.lock().await;
            match state.pending_start_only_admissions.entry(turn_id.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((tx, cancellation));
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    return Err(CodexErr::InvalidRequest(format!(
                        "turn id {turn_id} already has a pending start admission"
                    )));
                }
            }
        }
        let submitted = async {
            let registration = self
                .session
                .services
                .agent_control
                .register_execution_permit(self.session.thread_id, &turn_id, execution_guard)?;
            let admission = self.enqueue_submission(sub).await?;
            registration.commit();
            Self::await_mailbox_admission(admission).await
        }
        .await;
        if let Err(error) = submitted {
            self.session
                .task_start_state
                .lock()
                .await
                .pending_start_only_admissions
                .remove(&turn_id);
            return Err(error);
        }
        // A loop resolves every admission it receives before it terminates. Input
        // lost on the way (a delegate proxy drops it once the child loop is gone)
        // or a loop that ended abnormally would leave this sender parked in the
        // session, so stop waiting once intake is closed and the loop has ended.
        let loop_gone = async {
            self.tx_sub.closed().await;
            let _ = self.session_loop_termination.clone().await;
        };
        tokio::select! {
            biased;
            admitted = &mut rx => return admitted.unwrap_or_else(|_| Err(CodexErr::Fatal(
                format!("turn start admission acknowledgement was dropped for {turn_id}")))),
            () = loop_gone => {}
        }
        if self
            .session
            .task_start_state
            .lock()
            .await
            .pending_start_only_admissions
            .remove(&turn_id)
            .is_some()
        {
            return Err(CodexErr::InternalAgentDied);
        }
        rx.await.unwrap_or_else(|_| {
            Err(CodexErr::Fatal(format!(
                "turn start admission acknowledgement was dropped for {turn_id}"
            )))
        })
    }

    pub(crate) async fn enqueue_submission(
        &self,
        mut sub: Submission,
    ) -> CodexResult<Option<oneshot::Receiver<CodexResult<()>>>> {
        if self.rx_event.is_closed() {
            // Required stream failure is fatal to this runtime. Closing intake
            // runs the existing submission-loop teardown, retaining its owners.
            self.tx_sub.close();
            return Err(CodexErr::InternalAgentDied);
        }
        let preparation_permit = if is_submission_control(&sub.op) {
            None
        } else {
            Some(tokio::select! {
                biased;
                _ = self.tx_sub.closed() => return Err(CodexErr::InternalAgentDied),
                permit = Arc::clone(&self.session.submission_preparation_slots).acquire_owned() => {
                    permit.map_err(|_| CodexErr::InternalAgentDied)?
                }
            })
        };
        if sub.trace.is_none() {
            sub.trace = current_span_w3c_trace_context();
        }
        let (mailbox_admission, admission_rx) =
            if matches!(&sub.op, Op::InterAgentCommunication { .. }) {
                let (tx, rx) = oneshot::channel();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
        self.tx_sub
            .send(QueuedSubmission {
                submission: sub,
                mailbox_admission,
                preparation_permit,
            })
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(admission_rx)
    }

    pub(crate) async fn await_mailbox_admission(
        admission_rx: Option<oneshot::Receiver<CodexResult<()>>>,
    ) -> CodexResult<()> {
        if let Some(admission_rx) = admission_rx {
            admission_rx
                .await
                .map_err(|_| CodexErr::InternalAgentDied)??;
        }
        Ok(())
    }

    pub(crate) async fn request_shutdown(&self) -> CodexResult<()> {
        match self.submit(Op::Shutdown).await {
            Ok(_) => {}
            Err(CodexErr::InternalAgentDied) => {}
            Err(err) => return Err(err),
        }
        Ok(())
    }

    pub async fn shutdown_and_wait(&self) -> CodexResult<()> {
        let session_loop_termination = self.session_loop_termination.clone();
        self.request_shutdown().await?;
        session_loop_termination
            .await
            .map_err(|error| CodexErr::Fatal(error.to_string()))?;
        match self.session.rollout_durability_error.try_lock() {
            Ok(outcome) => {
                if let Some(error) = outcome.as_ref() {
                    return Err(CodexErr::Fatal(format!(
                        "session persistence unconfirmed: {error}"
                    )));
                }
            }
            Err(_) => {
                return Err(CodexErr::Fatal(
                    "session durable barrier remains pending".to_string(),
                ));
            }
        }
        if let Some(Err(error)) = self
            .session
            .persistence_shutdown_result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Err(CodexErr::Fatal(error));
        }
        match self.session.persistence_retirement_result.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
            Some(result) => result.map_err(CodexErr::Fatal),
            None if self.session.live_thread().is_some() || self.session.persistence_tasks.is_closed() => Err(CodexErr::Fatal(
                "session persistence shutdown was not confirmed".to_string(),
            )),
            None => Ok(()),
        }
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        if self.rx_event.is_closed() {
            self.tx_sub.close();
        }
        let event = self
            .rx_event
            .recv()
            .await
            .inspect_err(|_| { self.tx_sub.close(); })
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub async fn steer_input(
        &self,
        input: Vec<UserInput>,
        additional_context: IndexMap<String, AdditionalContextEntry>,
        expected_turn_id: Option<&str>,
        client_user_message_id: Option<String>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
    ) -> Result<String, SteerInputError> {
        self.session
            .steer_input(
                input,
                additional_context,
                expected_turn_id,
                client_user_message_id,
                responsesapi_client_metadata,
            )
            .await
    }

    pub(crate) async fn set_app_server_client_info(
        &self,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
        mcp_elicitations_auto_deny: bool,
    ) -> ConstraintResult<()> {
        self.session
            .update_settings(SessionSettingsUpdate {
                app_server_client_name,
                app_server_client_version,
                ..Default::default()
            })
            .await?;
        self.session
            .services
            .latest_mcp_runtime()
            .manager()
            .set_elicitations_auto_deny(mcp_elicitations_auto_deny);
        Ok(())
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        self.agent_status.borrow().clone()
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        self.session.thread_config_snapshot().await
    }

    pub(crate) async fn instruction_sources(&self) -> Vec<PathUri> {
        self.session
            .services
            .agents_md_manager
            .get_loaded()
            .await
            .as_ref()
            .map_or_else(Vec::new, |instructions| instructions.sources().collect())
    }

    pub(crate) async fn thread_environment_selections(&self) -> Vec<TurnEnvironmentSelection> {
        let state = self.session.state.lock().await;
        state
            .session_configuration
            .environment_selections()
            .to_vec()
    }

    pub(crate) async fn thread_primary_environment_has_shell(&self) -> Option<bool> {
        self.session
            .services
            .turn_environments
            .snapshot()
            .await
            .primary()
            .map(|environment| environment.shell.is_some())
    }

    pub(crate) fn state_db(&self) -> Option<state_integration::StateDbHandle> {
        self.session.state_db()
    }

    pub(crate) fn enabled(&self, feature: Feature) -> bool {
        self.session.enabled(feature)
    }
}

fn should_prefetch_models_before_default(
    model_provided: bool,
    refresh_strategy: codex_models_manager::manager::RefreshStrategy,
) -> bool {
    model_provided
        && !matches!(
            refresh_strategy,
            codex_models_manager::manager::RefreshStrategy::Offline
        )
}

/// Generate a core submission ID. App-server exposes submission IDs that
/// create turns as a public-facing turn ID.
///
/// Some use cases take advantage of the fact that these are UUID7 which
/// encodes a timestamp, so think carefully before changing this.
fn new_submission_id() -> String {
    Uuid::now_v7().to_string()
}

pub(crate) fn get_service_tier(
    configured_service_tier: Option<String>,
    fast_mode_enabled: bool,
    model_info: &ModelInfo,
) -> Option<String> {
    // Flex is an explicit routing choice, not a FastMode or catalog capability.
    if configured_service_tier.as_deref() == Some(ServiceTier::Flex.request_value()) {
        return configured_service_tier;
    }
    if !fast_mode_enabled {
        return None;
    }
    configured_service_tier.filter(|service_tier| {
        service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE
            || model_info.supports_service_tier(service_tier)
    })
}

fn unsupported_service_tier_warning(
    configured_service_tier: Option<&str>,
    fast_mode_enabled: bool,
    model_info: &ModelInfo,
) -> Option<String> {
    let service_tier = configured_service_tier.filter(|service_tier| {
        *service_tier != SERVICE_TIER_DEFAULT_REQUEST_VALUE
            && *service_tier != ServiceTier::Flex.request_value()
    })?;
    if !fast_mode_enabled {
        return Some(format!(
            "Configured service tier `{service_tier}` will be omitted from requests because FastMode is disabled (`features.fast_mode = false`)."
        ));
    }
    if model_info.supports_service_tier(service_tier) {
        return None;
    }
    Some(format!(
        "Configured service tier `{service_tier}` is not advertised as supported for model `{}` and will be omitted from requests.",
        model_info.slug
    ))
}

fn session_permission_profile_state_from_config(
    config: &Config,
) -> CodexResult<PermissionProfileState> {
    Ok(config.permissions.permission_profile_state().clone())
}


pub(crate) fn session_loop_termination_from_handle(
    handle: JoinHandle<()>,
) -> SessionLoopTermination {
    async move {
        match handle.await {
            Ok(()) => Ok(()),
            Err(err) => {
                error!(%err, "session loop terminated unexpectedly");
                Err(if err.is_panic() {
                    "session loop panicked before orderly shutdown"
                } else {
                    "session loop was cancelled before orderly shutdown"
                })
            }
        }
    }
    .boxed()
    .shared()
}

async fn thread_title_from_thread_store(
    live_thread: Option<&LiveThread>,
    thread_store: &Arc<dyn ThreadStore>,
    conversation_id: ThreadId,
) -> Option<String> {
    let thread = match live_thread {
        Some(live_thread) => {
            live_thread
                .read_thread(
                    /*include_archived*/ true, /*include_history*/ false,
                )
                .await
        }
        None => {
            thread_store
                .read_thread(ReadThreadParams {
                    thread_id: conversation_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
        }
    }
    .ok()?;

    let title = thread.name.as_deref()?.trim();
    (!title.is_empty() && thread.preview.trim() != title).then(|| title.to_string())
}


#[expect(
    clippy::expect_used,
    reason = "context source identities must fit usize; overflow must fail rather than alias a different contributor"
)]
fn context_contribution_index(producer: usize, fragment: usize, thread: bool) -> usize {
    // Cantor pairing is injective; source identity cannot shift when another
    // contributor times out or an earlier fragment fails admission.
    let sum = producer
        .checked_add(fragment)
        .expect("context source count fits usize");
    let pair = sum
        .checked_mul(sum + 1)
        .expect("context source count fits usize")
        / 2
        + fragment;
    pair.checked_mul(2)
        .and_then(|value| value.checked_add(usize::from(thread)))
        .expect("context source identity fits usize")
}

fn take_prompt_fragment_with_identity(
    fragment: PromptFragment,
    budget: &mut ModelContextBudget,
    turn_id: &str,
    contribution_index: Option<usize>,
) -> Option<(PromptSlot, String)> {
    let rendered = fragment.text().to_string();
    let slot = fragment.slot();
    if slot == PromptSlot::SeparateDeveloper && rendered.is_empty() {
        return None;
    }
    // Charge each fragment as its own normal message, including its envelope and turn ID.
    // Grouping fragments into fewer messages later can only reduce this conservative charge.
    let identify = |text: &str| {
        contribution_index.map_or_else(
            || text.to_string(),
            |index| crate::stable_context::turn_contribution_text(index, text),
        )
    };
    let message_bytes = |text: &str| {
        let mut items = Session::build_context_contribution_items_from_rendered_fragments(vec![(
            slot,
            identify(text),
        )]);
        items.iter_mut().fold(0usize, |total, item| {
            item.set_turn_id_if_missing(turn_id);
            let token_bytes = codex_utils_string::approx_bytes_for_tokens(
                usize::try_from(crate::context_manager::estimate_item_token_count(item).max(1))
                    .unwrap_or(usize::MAX),
            );
            let serialized_bytes = serde_json::to_vec(item)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX);
            total.saturating_add(token_bytes.max(serialized_bytes))
        })
    };
    if slot == PromptSlot::DeveloperPolicy {
        return budget
            .try_take_bytes(message_bytes(&rendered))
            .then(|| (slot, identify(&rendered)));
    }
    let text = budget.clone().take(&rendered)?;
    let charge = message_bytes(&text);
    if budget.try_take_bytes(charge) {
        return Some((slot, identify(&text)));
    }

    // Preserve the existing bounded head/tail truncation when the envelope leaves less room.
    let mut lower = 1;
    let mut upper = budget.remaining_bytes().saturating_sub(1);
    let mut admitted = None;
    while lower <= upper {
        let middle = lower + (upper - lower) / 2;
        let Some(text) = budget.clone().take_up_to(&rendered, middle) else {
            lower = middle + 1;
            continue;
        };
        if slot == PromptSlot::SeparateDeveloper && text.is_empty() {
            lower = middle + 1;
            continue;
        }
        let charge = message_bytes(&text);
        if charge <= budget.remaining_bytes() {
            admitted = Some((text, charge));
            lower = middle + 1;
        } else {
            upper = middle - 1;
        }
    }
    let (text, charge) = admitted?;
    budget
        .try_take_bytes(charge)
        .then(|| (slot, identify(&text)))
}

fn push_rendered_prompt_fragment(
    slot: PromptSlot,
    text: String,
    developer_sections: &mut Vec<String>,
    contextual_user_sections: &mut Vec<String>,
    separate_developer_sections: &mut Vec<String>,
) {
    match slot {
        PromptSlot::DeveloperPolicy | PromptSlot::DeveloperCapabilities => {
            developer_sections.push(text);
        }
        PromptSlot::ContextualUser => {
            contextual_user_sections.push(text);
        }
        PromptSlot::SeparateDeveloper => {
            separate_developer_sections.push(text);
        }
    }
}

struct PreparedEventDispatch {
    terminal_source: Option<EventMsg>,
    legacy_events: Vec<EventMsg>,
}

impl PreparedEventDispatch {
    fn new(source: &EventMsg, show_raw_agent_reasoning: bool) -> Self {
        let terminal_source =
            matches!(source, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_))
                .then(|| source.clone());
        Self {
            terminal_source,
            legacy_events: source.as_legacy_events(show_raw_agent_reasoning),
        }
    }
}

fn response_item_needs_turn_id(item: &ResponseItem) -> bool {
    !matches!(
        item,
        ResponseItem::AdditionalTools { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::Other
    ) && item.turn_id().is_none()
}

struct StartupRolloutFacts {
    initial_messages: Option<Vec<EventMsg>>,
    mailbox_communication_ids: Vec<codex_protocol::ResponseItemId>,
    has_prior_user_turns: bool,
    last_token_info: Option<TokenUsageInfo>,
    reconstructed_provider_id: Option<String>,
}

impl StartupRolloutFacts {
    fn reduce(history: &InitialHistory) -> Self {
        let items = history.get_rollout_items();
        let mut initial_messages =
            (!matches!(history, InitialHistory::New | InitialHistory::Cleared)).then(Vec::new);
        let mut mailbox_communication_ids = Vec::new();
        let mut prior_user_turns = PriorUserTurnTracker::default();
        let mut last_token_info = None;
        let mut persisted_settings =
            PersistedThreadSettingsReducer::new(PersistedThreadSettings::default());

        for (idx, item) in items.iter().enumerate() {
            persisted_settings.apply_item(item);
            prior_user_turns.observe(items, idx, item);
            match item {
                RolloutItem::EventMsg(event) => {
                    if let Some(initial_messages) = initial_messages.as_mut() {
                        initial_messages.push(event.clone());
                    }
                    if let EventMsg::TokenCount(event) = event
                        && let Some(info) = event.info.as_ref()
                    {
                        last_token_info = Some(info);
                    }
                }
                RolloutItem::InterAgentCommunication(communication) => {
                    if let Some(id) = communication.id.as_ref() {
                        mailbox_communication_ids.push(id.clone());
                    }
                }
                RolloutItem::ResponseItem(ResponseItem::AgentMessage { id: Some(id), .. }) => {
                    mailbox_communication_ids.push(id.clone());
                }
                _ => {}
            }
        }

        Self {
            initial_messages,
            mailbox_communication_ids,
            has_prior_user_turns: prior_user_turns.has_prior_user_turns(),
            last_token_info: last_token_info.cloned(),
            reconstructed_provider_id: persisted_settings.into_settings().model_provider_id,
        }
    }
}

/// Boundary items for a resumed rollout whose last turn never completed:
/// the interrupted-turn marker followed by the abort event the live path records.
fn unfinished_turn_boundary_items(
    rollout_items: &[RolloutItem],
    turn_context: &TurnContext,
) -> Vec<RolloutItem> {
    let Some(turn_id) = crate::thread_manager::resumed_rollout_unfinished_turn(rollout_items)
    else {
        return Vec::new();
    };
    let start = rollout_items
        .iter()
        .rposition(|item| matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))))
        .unwrap_or(0);
    let mut calls = std::collections::HashSet::new();
    let mut results = std::collections::HashSet::new();
    for item in &rollout_items[start..] {
        match item {
            RolloutItem::ResponseItem(
                ResponseItem::FunctionCall { call_id, .. }
                | ResponseItem::CustomToolCall { call_id, .. },
            ) => {
                calls.insert(call_id);
            }
            RolloutItem::ResponseItem(
                ResponseItem::LocalShellCall {
                    call_id: Some(call_id),
                    ..
                }
                | ResponseItem::ToolSearchCall {
                    call_id: Some(call_id),
                    ..
                },
            ) => {
                calls.insert(call_id);
            }
            RolloutItem::ResponseItem(
                ResponseItem::FunctionCallOutput { call_id, .. }
                | ResponseItem::CustomToolCallOutput { call_id, .. }
                | ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    ..
                },
            ) => {
                results.insert(call_id);
            }
            _ => {}
        }
    }
    let (recovery, mut timing) = crate::context::lost_turn_recovery(rollout_items);
    timing.counters.tool_call_count = calls.len() as u32;
    let mut items = Vec::new();
    if let Some(mut marker) = crate::tasks::unfinished_turn_history_marker(
        crate::tasks::InterruptedTurnHistoryMarker::from_config_and_version(
            turn_context.config.as_ref(),
            turn_context.multi_agent_version,
        ),
        calls.len(),
        results.len(),
    ) {
        if let ResponseItem::Message { content, .. } = &mut marker {
            content.push(codex_protocol::models::ContentItem::InputText { text: recovery });
        }
        items.push(RolloutItem::ResponseItem(marker));
    }
    items.push(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id,
            reason: TurnAbortReason::ProcessLost,
            completed_at: None,
            duration_ms: None,
            timing: Some(timing),
        },
    )));
    items
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ThreadSettingsUpdateError {
    #[error("invalid thread settings override: {0}")]
    Invalid(#[from] codex_config::ConstraintError),
    #[error("failed to persist thread settings override: {0}")]
    Persistence(#[from] std::io::Error),
}

struct StepContextTimer {
    step: &'static str,
    started: std::time::Instant,
}

impl StepContextTimer {
    fn start(step: &'static str) -> Self {
        tracing::info!(step, "step context phase started");
        Self {
            step,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for StepContextTimer {
    fn drop(&mut self) {
        tracing::info!(
            step = self.step,
            duration_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            "step context phase finished"
        );
    }
}

impl Session {
    pub(crate) async fn app_server_client_metadata(&self) -> AppServerClientMetadata {
        let state = self.state.lock().await;
        AppServerClientMetadata {
            client_name: state.session_configuration.app_server_client_name.clone(),
            client_version: state
                .session_configuration
                .app_server_client_version
                .clone(),
        }
    }


    /// Builds the `x-codex-beta-features` header value for this session.
    ///
    /// `ModelClient` is session-scoped and intentionally does not depend on the full `Config`, so
    /// we precompute the comma-separated list of enabled experimental feature keys at session
    /// creation time and thread it into the client.
    fn build_model_client_beta_features_header(config: &Config) -> Option<String> {
        let beta_features_header = FEATURES
            .iter()
            .filter_map(|spec| {
                let advertise_in_model_client_header =
                    spec.stage.experimental_menu_description().is_some();
                if advertise_in_model_client_header && config.features.enabled(spec.id) {
                    Some(spec.key)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(",");

        if beta_features_header.is_empty() {
            None
        } else {
            Some(beta_features_header)
        }
    }



    pub(crate) async fn codex_home(&self) -> AbsolutePathBuf {
        let state = self.state.lock().await;
        state.session_configuration.codex_home().clone()
    }

    pub(crate) fn subscribe_elicitation_pause_state(&self) -> watch::Receiver<bool> {
        self.services.elicitations.subscribe()
    }

    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    pub(crate) fn state_db(&self) -> Option<state_integration::StateDbHandle> {
        self.services.state_db.clone()
    }

    pub(crate) fn live_thread_for_persistence(
        &self,
        operation: &str,
    ) -> anyhow::Result<&LiveThread> {
        self.live_thread()
            .ok_or_else(|| anyhow::anyhow!("Session persistence is disabled; cannot {operation}."))
    }

    pub(crate) fn live_thread(&self) -> Option<&LiveThread> {
        self.services.live_thread.as_ref()
    }

    /// Flush rollout writes and return the final durability-barrier result.
    #[instrument(name = "session.flush_rollout", level = "trace", skip_all)]
    pub(crate) async fn flush_rollout(&self) -> std::io::Result<()> {
        self.flush_rollout_with_durability(/*durable*/ false).await
    }

    /// [`Self::flush_rollout`] plus a stable-storage barrier for the rollout itself, so a
    /// terminal turn record survives an operating-system crash. Used once per turn.
    #[instrument(name = "session.flush_rollout_durable", level = "trace", skip_all)]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The held receipt is the pending-barrier witness that shutdown try-locks and keeps newer failures from being erased"
    )]
    pub(crate) async fn flush_rollout_durable(&self) -> std::io::Result<()> {
        // Serialize the barrier and its receipt together. An older successful
        // waiter must never erase a newer failure after releasing the store lock.
        // Order: receipt -> existing artifact/store locks; shutdown only try-locks.
        let mut outcome = self.rollout_durability_error.lock().await;
        let result = self.flush_rollout_with_durability(/*durable*/ true).await;
        *outcome = result.as_ref().err().map(ToString::to_string);
        result
    }

    async fn flush_rollout_with_durability(&self, durable: bool) -> std::io::Result<()> {
        crate::tools::command_output_artifact::sync_tool_output_artifacts(
            self.codex_home().await.as_path(),
            &self.thread_id.to_string(),
        )
        .await?;
        let Some(live_thread) = self.live_thread() else {
            return Ok(());
        };
        if durable {
            live_thread.flush_durable().await
        } else {
            live_thread.flush().await
        }
        .map_err(std::io::Error::other)
    }


    /// Keep accepted tool commits ahead of turn terminalization even if their
    /// caller disappears before cooperative cancellation finishes.
    pub(crate) fn retain_tool_dispatch_commit(self: &Arc<Self>) -> DurableHistoryCommitInFlight {
        self.durable_history_commits_in_flight
            .fetch_add(1, Ordering::AcqRel);
        DurableHistoryCommitInFlight {
            session: Arc::clone(self),
        }
    }

    /// Retain generation evidence until invalidation and publication finish,
    /// including when the sampling worker is aborted during a capture.
    pub(crate) async fn flush_workspace_evidence_generation(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        batch: &Arc<crate::tools::parallel::WorkspaceEvidenceGenerationBatch>,
        tracker: &crate::tools::context::SharedTurnDiffTracker,
    ) -> CodexResult<crate::tools::parallel::WorkspaceEvidenceGenerationFlush> {
        let in_flight = self.retain_tool_dispatch_commit();
        let session = Arc::clone(self);
        let turn_context = Arc::clone(turn_context);
        let batch = Arc::clone(batch);
        let tracker = Arc::clone(tracker);
        tokio::spawn(async move {
            let _in_flight = in_flight;
            batch.flush(&session, &turn_context, &tracker).await
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!("workspace evidence flush task failed: {error}"))
        })?
    }

    pub(crate) async fn wait_for_ordered_history_commits(&self) {
        loop {
            let changed = self.durable_history_commits_changed.notified();
            if self
                .durable_history_commits_in_flight
                .load(Ordering::Acquire)
                == 0
            {
                break;
            }
            changed.await;
        }
    }

    /// Wait for every cancellation-shielded history append accepted before terminalization, then
    /// flush that complete prefix and attest its queued tool results.
    pub(crate) async fn flush_rollout_after_ordered_commits(
        &self,
        turn_context: &TurnContext,
    ) -> std::io::Result<()> {
        self.wait_for_ordered_history_commits().await;
        let executions = turn_context
            .turn_timing_state
            .queued_tool_result_executions();
        if let Err(error) = self.flush_rollout().await {
            turn_context
                .turn_timing_state
                .record_tool_result_persistence_barrier_failed();
            return Err(error);
        }
        turn_context
            .turn_timing_state
            .record_tool_result_executions_persisted(&executions);
        Ok(())
    }

    pub(crate) async fn thread_settings_applied_event(&self) -> EventMsg {
        let snapshot = {
            let state = self.state.lock().await;
            state.session_configuration.thread_config_snapshot()
        };
        Self::thread_settings_event_from_snapshot(snapshot)
    }

    fn thread_settings_event_from_snapshot(snapshot: ThreadConfigSnapshot) -> EventMsg {
        let cwd = snapshot.cwd().clone();
        let sandbox_policy = snapshot.sandbox_policy();
        EventMsg::ThreadSettingsApplied(ThreadSettingsAppliedEvent {
            thread_settings: ThreadSettingsSnapshot {
                model: snapshot.model,
                model_provider_id: snapshot.model_provider_id,
                service_tier: Some(snapshot.service_tier),
                developer_instructions: Some(snapshot.developer_instructions),
                approval_policy: snapshot.approval_policy,
                permission_profile: snapshot.permission_profile,
                active_permission_profile: Some(snapshot.active_permission_profile),
                environments: Some(snapshot.environments),
                workspace_roots: Some(snapshot.workspace_roots),
                profile_workspace_roots: Some(snapshot.profile_workspace_roots),
                sandbox_policy: Some(sandbox_policy),
                windows_sandbox_level: Some(snapshot.windows_sandbox_level),
                cwd,
                reasoning_effort: Some(snapshot.reasoning_effort),
                reasoning_summary: Some(snapshot.reasoning_summary),
                personality: Some(snapshot.personality),
                collaboration_mode: snapshot.collaboration_mode,
            },
        })
    }

    pub(crate) async fn persist_thread_settings_snapshot(&self) -> std::io::Result<()> {
        let event = self.thread_settings_applied_event().await;
        self.persist_rollout_items_durable(&[RolloutItem::EventMsg(event)])
            .await
    }

    pub(crate) async fn persist_thread_settings_snapshot_if_unmaterialized(
        &self,
    ) -> std::io::Result<bool> {
        let Some(path) = self
            .current_rollout_path()
            .await
            .map_err(std::io::Error::other)?
        else {
            return Ok(false);
        };
        if codex_rollout::existing_rollout_path(&path).await.is_some() {
            return Ok(false);
        }
        self.persist_thread_settings_snapshot().await?;
        Ok(true)
    }

    pub(crate) async fn try_ensure_rollout_materialized(&self) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread.persist().await.map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    pub(crate) async fn ensure_rollout_materialized(&self) {
        if let Err(e) = self.try_ensure_rollout_materialized().await {
            warn!("failed to materialize thread persistence: {e}");
        }
    }

    fn next_internal_sub_id(&self) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("auto-compact-{id}")
    }

    pub(crate) async fn get_total_token_usage(&self) -> i64 {
        let (history, base_instructions) = {
            let state = self.state.lock().await;
            (
                state.history.clone(),
                BaseInstructions {
                    text: state.session_configuration.base_instructions.clone(),
                },
            )
        };
        history.get_total_token_usage(false, &base_instructions)
    }

    pub(crate) async fn get_context_tokens_for_turn(&self, turn: &TurnContext) -> i64 {
        let history = self.state.lock().await.history.clone();
        history.get_total_token_usage_for_modalities(
            &turn.base_instructions,
            &turn.model_info.input_modalities,
        )
    }

    pub(crate) async fn projected_prompt_state(
        &self,
        turn: &TurnContext,
        pending_user_boundary: bool,
        body_after_prefix: bool,
    ) -> (i64, Option<i64>, Option<AutoCompactWindowSnapshot>) {
        let (history, window) = {
            let state = self.state.lock().await;
            (state.history.clone(), body_after_prefix.then(|| state.auto_compact_window_snapshot()))
        };
        let (active, committed) = history.projected_token_estimates(
            &turn.base_instructions, &turn.model_info.input_modalities, pending_user_boundary,
        );
        (active, committed, window)
    }

    pub(crate) async fn auto_compact_window_snapshot(&self) -> AutoCompactWindowSnapshot {
        let state = self.state.lock().await;
        state.auto_compact_window_snapshot()
    }

    pub(crate) async fn estimated_tokens_after_last_model_generated_item(&self) -> i64 {
        let state = self.state.lock().await;
        state
            .history
            .estimated_tokens_after_last_model_generated_item()
    }

    pub(crate) async fn total_token_usage(&self) -> Option<TokenUsage> {
        let state = self.state.lock().await;
        state.token_info().map(|info| info.total_token_usage)
    }

    /// Returns the complete token usage snapshot currently cached for this session.
    ///
    /// Resume and fork reconstruction seed this state from the last persisted rollout
    /// `TokenCount` event. Callers that need to replay restored usage to a client
    /// should use this accessor instead of `total_token_usage`, because the app-server
    /// notification includes both total and last-turn usage.
    pub(crate) async fn token_usage_info(&self) -> Option<TokenUsageInfo> {
        let state = self.state.lock().await;
        state.token_info()
    }

    pub(crate) async fn get_estimated_token_count(
        &self,
        turn_context: &TurnContext,
    ) -> Option<i64> {
        let history = self.state.lock().await.history.clone();
        history.estimate_token_count(turn_context)
    }

    pub(crate) async fn get_base_instructions(&self) -> BaseInstructions {
        let state = self.state.lock().await;
        BaseInstructions {
            text: state.session_configuration.base_instructions.clone(),
        }
    }

    // Merges connector IDs into the session-level explicit connector selection.
    #[tracing::instrument(
        level = "trace",
        skip_all,
        fields(connector_count = connector_ids.len())
    )]
    pub(crate) async fn merge_connector_selection(
        &self,
        connector_ids: HashSet<String>,
    ) -> HashSet<String> {
        let mut state = self.state.lock().await;
        state.merge_connector_selection(connector_ids)
    }

    // Returns the connector IDs currently selected for this session.
    pub(crate) async fn get_connector_selection(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.get_connector_selection()
    }

    // Clears connector IDs that were accumulated for explicit selection.
    pub(crate) async fn clear_connector_selection(&self) {
        let mut state = self.state.lock().await;
        state.clear_connector_selection();
    }


    async fn record_initial_history_with_facts(
        &self,
        conversation_history: InitialHistory,
        startup_rollout_facts: StartupRolloutFacts,
    ) -> std::io::Result<()> {
        let is_subagent = {
            let state = self.state.lock().await;
            state
                .session_configuration
                .session_source
                .is_non_root_agent()
        };
        let has_prior_user_turns = startup_rollout_facts.has_prior_user_turns;
        let last_token_info = startup_rollout_facts.last_token_info;
        let reconstructed_provider_id = startup_rollout_facts.reconstructed_provider_id;
        {
            let mut state = self.state.lock().await;
            state.set_next_turn_is_first(!has_prior_user_turns);
        }
        match conversation_history {
            InitialHistory::New | InitialHistory::Cleared => {
                // Defer initial context insertion until the first real turn starts so
                // turn/start overrides can be merged before we write model-visible context.
                self.set_previous_turn_settings(/*previous_turn_settings*/ None)
                    .await;
            }
            InitialHistory::Resumed(resumed_history) => {
                let turn_context = self.new_default_turn().await;
                let mut rollout_items = Arc::unwrap_or_clone(resumed_history.history);
                // The process can stop inside a turn before any completion or
                // interruption is recorded. Append the same boundary the live
                // interrupt path persists, so the model does not treat that
                // request as worked on when the user continues.
                let unfinished_turn_boundary =
                    unfinished_turn_boundary_items(&rollout_items, &turn_context);
                rollout_items.extend(unfinished_turn_boundary.iter().cloned());
                let previous_turn_settings = self
                    .apply_rollout_reconstruction_with_provider(
                        &turn_context,
                        &rollout_items,
                        /*invalidate_unified_exec_sessions*/ true,
                        reconstructed_provider_id.as_deref(),
                    )
                    .await;
                if !unfinished_turn_boundary.is_empty() {
                    self.persist_rollout_items(&unfinished_turn_boundary).await;
                }
                self.reconcile_input_receipts(&rollout_items, &turn_context).await?;

                // If resuming, warn when the last recorded model differs from the current one.
                let curr: &str = turn_context.model_info.slug.as_str();
                if let Some(prev) = previous_turn_settings
                    .as_ref()
                    .map(|settings| settings.model.as_str())
                    .filter(|model| *model != curr)
                {
                    warn!("resuming session with different model: previous={prev}, current={curr}");
                    // A resume-time configuration warning is live feedback, not a
                    // new turn: leave persisted history and its mtime untouched.
                    self.send_event_raw_with_persistence(
                        Event {
                            id: turn_context.sub_id.clone(),
                            msg: EventMsg::Warning(WarningEvent {
                                message: format!(
                                    "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                                ),
                            }),
                        },
                        /*persist*/ false,
                    )
                    .await;
                }

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = last_token_info {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // Defer seeding the session's initial context until the first turn starts so
                // turn/start overrides can be merged before we write to the rollout.
                if !is_subagent && let Err(err) = self.flush_rollout().await {
                    warn!(%err, "failed to flush resumed rollout");
                    self.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!("Could not save the resumed session history: {err}"),
                        }),
                    )
                    .await;
                }
            }
            InitialHistory::Forked(mut rollout_items) => {
                let turn_context = self.new_default_turn().await;
                for rollout_item in &mut rollout_items {
                    if let RolloutItem::ResponseItem(response_item) = rollout_item {
                        Self::assign_missing_response_item_id(response_item);
                    }
                    // Rewrite only the child's copied observations. This keeps
                    // their turn attribution and makes every inherited prefix
                    // safe to resume without billing the parent's work again.
                    if let RolloutItem::EventMsg(EventMsg::TokenCount(event)) = rollout_item
                        && let Some(info) = event.info.as_mut()
                    {
                        *info = info.for_fork();
                    }
                }
                self.apply_rollout_reconstruction_with_provider(
                    &turn_context,
                    &rollout_items,
                    /*invalidate_unified_exec_sessions*/ false,
                    reconstructed_provider_id.as_deref(),
                )
                .await;
                // Parent usage informs inherited context pressure, never child cost.
                if let Some(info) = last_token_info {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info.for_fork()));
                }

                // Persist the fork history with any missing item IDs assigned above
                // (the store filters which rollout items are retained).
                if !rollout_items.is_empty() {
                    self.persist_rollout_items_ordered(&rollout_items).await?;
                }

                // Root forks finish restoring reconstructed runtime-only settings in
                // `ThreadManager` after session construction. Let that owner append the
                // single authoritative child snapshot instead of recording a provisional
                // one here as well. Non-root forks have no later reconstruction phase, so
                // they still persist their resolved settings at this boundary.
                if is_subagent {
                    self.persist_thread_settings_snapshot().await?;
                }

                // Forked threads should remain file-backed immediately after startup.
                self.try_ensure_rollout_materialized().await?;

                // Flush after seeding history and any persisted rollout copy.
                if !is_subagent {
                    self.flush_rollout().await?;
                }
            }
        }
        Ok(())
    }

    #[instrument(
        level = "trace",
        skip_all,
        fields(
            thread_id = %self.thread_id(),
            rollout_item_count = rollout_items.len()
        )
    )]
    async fn apply_rollout_reconstruction(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
        invalidate_unified_exec_sessions: bool,
    ) -> Option<PreviousTurnSettings> {
        let reconstructed_provider_id =
            codex_protocol::persisted_thread_settings::reduce_persisted_thread_settings(
                rollout_items,
                PersistedThreadSettings::default(),
            )
            .model_provider_id;
        self.apply_rollout_reconstruction_with_provider(
            turn_context,
            rollout_items,
            invalidate_unified_exec_sessions,
            reconstructed_provider_id.as_deref(),
        )
        .await
    }

    async fn apply_rollout_reconstruction_with_provider(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
        invalidate_unified_exec_sessions: bool,
        reconstructed_provider_id: Option<&str>,
    ) -> Option<PreviousTurnSettings> {
        let rollout_reconstruction::RolloutReconstruction {
            mut history,
            plan,
            plan_lineage,
            mut previous_turn_settings,
            reference_context_item,
            world_state_baseline,
            additional_context,
            repository_sources,
            window_number,
            first_window_id,
            previous_window_id,
            window_id,
        } = self
            .reconstruct_history_from_rollout(turn_context, rollout_items)
            .await;
        // A prior model slug is meaningful only with the provider catalog that selected it.
        // Preserve the old compaction hash so an incompatibility still compacts, but make that
        // compaction use the current provider's current model.
        retarget_previous_turn_settings_for_provider(
            &mut previous_turn_settings,
            reconstructed_provider_id,
            &turn_context.config.model_provider_id,
            &turn_context.model_info.slug,
        );
        if invalidate_unified_exec_sessions {
            self.services
                .unified_exec_manager
                .exclude_process_ids(rollout_reconstruction::historical_process_ids(
                    rollout_items,
                ))
                .await;
            rollout_reconstruction::append_unified_exec_resume_invalidation_with_rollout(
                &mut history,
                rollout_items,
            );
            rollout_reconstruction::append_unsettled_tool_recovery(&mut history, rollout_items);
        }
        // Keep the recorded rollout unchanged. Prepare its reconstructed history before
        // installing it, so legacy images are processed once for this resume or fork and
        // will be processed again if the rollout is reconstructed in a future session.
        // This meets image resizing requirements without modifying persisted rollouts.
        prepare_response_items(&mut history);
        self.services
            .plan_store
            .restore_reconstructed(plan, plan_lineage, &history)
            .await;
        let prefix_tokens = if matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            let base_instructions = self.get_base_instructions().await;
            ContextManager::estimate_items_token_count_with_base_instructions(
                &history,
                &base_instructions,
            )
        } else {
            None
        };
        if let Some(source_snapshot) = world_state_baseline
            .as_ref()
            .and_then(|snapshot| snapshot.section("agents_md"))
            .or(repository_sources.as_ref())
        {
            self.services
                .agents_md_manager
                .restore_repository_snapshot(
                    &turn_context.config,
                    &turn_context.environments,
                    source_snapshot,
                )
                .await;
        }
        {
            let mut state = self.state.lock().await;
            state.additional_context = additional_context;
            state.replace_history(history, reference_context_item);
            if let Some(world_state) = world_state_baseline {
                state.history.set_world_state_baseline(world_state);
            }
            let fallback_ids = state.auto_compact_window_ids();
            let window_id = window_id.unwrap_or(fallback_ids.window_id);
            state.restore_auto_compact_window(
                window_number,
                AutoCompactWindowIds {
                    first_window_id: first_window_id.unwrap_or(window_id),
                    previous_window_id,
                    window_id,
                },
            );
            state.set_previous_turn_settings(previous_turn_settings.clone());
        }
        if let Some(prefix_tokens) = prefix_tokens {
            self.set_auto_compact_window_estimated_prefill_for_scope(turn_context, prefix_tokens)
                .await;
        }
        previous_turn_settings
    }

    async fn set_auto_compact_window_estimated_prefill_for_scope(
        &self,
        turn_context: &TurnContext,
        tokens: i64,
    ) {
        if !matches!(
            turn_context.config.model_auto_compact_token_limit_scope,
            AutoCompactTokenLimitScope::BodyAfterPrefix
        ) {
            return;
        }

        let mut state = self.state.lock().await;
        state.set_auto_compact_window_estimated_prefill(tokens);
    }

    async fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        let state = self.state.lock().await;
        state.previous_turn_settings()
    }

    #[tracing::instrument(level = "trace", skip_all)]
    pub(crate) async fn set_previous_turn_settings(
        &self,
        previous_turn_settings: Option<PreviousTurnSettings>,
    ) {
        let mut state = self.state.lock().await;
        state.set_previous_turn_settings(previous_turn_settings);
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        self.update_settings_and_get(&updates, /*serialize_turn*/ false)
            .await
            .map(|_| ())
    }

    async fn update_settings_and_get(
        &self,
        updates: &SessionSettingsUpdate,
        serialize_turn: bool,
    ) -> ConstraintResult<SessionConfiguration> {
        match self
            .update_settings_and_get_inner(updates, serialize_turn, false)
            .await
        {
            Ok(configuration) => Ok(configuration),
            Err(ThreadSettingsUpdateError::Invalid(error)) => Err(error),
            Err(ThreadSettingsUpdateError::Persistence(_)) => {
                unreachable!("non-durable settings updates do not write persistence")
            }
        }
    }

    pub(crate) async fn update_settings_durable(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> Result<EventMsg, ThreadSettingsUpdateError> {
        let configuration = self
            .update_settings_and_get_inner(updates, true, true)
            .await?;
        Ok(Self::thread_settings_event_from_snapshot(
            configuration.thread_config_snapshot(),
        ))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Serialize settings projection, persistence and commit against concurrent accepted updates"
    )]
    async fn update_settings_and_get_inner(
        &self,
        updates: &SessionSettingsUpdate,
        serialize_turn: bool,
        persist: bool,
    ) -> Result<SessionConfiguration, ThreadSettingsUpdateError> {
        // Serialize accepted permission, environment and durable settings updates.
        let _refresh_guard = if serialize_turn
            || updates.permission_profile.is_some()
            || updates.sandbox_policy.is_some()
            || updates.environments.is_some()
            || updates.workspace_roots.is_some()
        {
            let Ok(refresh_guard) = self.configuration_update_lock.acquire().await else {
                unreachable!("configuration update semaphore is never closed");
            };
            Some(refresh_guard)
        } else {
            None
        };
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let (previous_config, new_config, updated) = {
            let mut state = self.state.lock().await;
            let updated = match state.session_configuration.apply_async(updates).await {
                Ok(updated) => updated,
                Err(err) => {
                    warn!("rejected session settings update: {err}");
                    return Err(ThreadSettingsUpdateError::Invalid(err));
                }
            };

            let previous_config = notify_config_contributors
                .then(|| Self::build_effective_session_config(&state.session_configuration));
            let new_config =
                notify_config_contributors.then(|| Self::build_effective_session_config(&updated));
            if persist {
                let event =
                    Self::thread_settings_event_from_snapshot(updated.thread_config_snapshot());
                self.persist_rollout_items_durable(&[RolloutItem::EventMsg(event)])
                    .await?;
            }
            // Publish the new configuration only after its durable write succeeds.
            state.session_configuration = updated.clone();
            // Publish live policy at the accepted settings boundary, never from
            // an async context builder (including speculative startup previews).
            let mcp_runtime = self.services.latest_mcp_runtime();
            mcp_runtime
                .manager()
                .set_approval_policy(&updated.approval_policy);
            mcp_runtime
                .manager()
                .set_permission_profile(updated.permission_profile());
            if updates.environments.is_some() {
                self.services
                    .turn_environments
                    .update_selections(updated.environment_selections());
                self.services.advance_planning_generation(&mut state);
            }
            (previous_config, new_config, updated)
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());

        Ok(updated)
    }

    pub(crate) async fn preview_settings(
        &self,
        updates: &SessionSettingsUpdate,
    ) -> ConstraintResult<ThreadConfigSnapshot> {
        let configuration = { self.state.lock().await.session_configuration.clone() };
        configuration
            .apply_async(updates)
            .await
            .map(|configuration| configuration.thread_config_snapshot())
    }

    pub(crate) async fn set_session_startup_prewarm(
        &self,
        startup_prewarm: SessionStartupPrewarmHandle,
    ) {
        let mut state = self.state.lock().await;
        state.set_session_startup_prewarm(startup_prewarm);
    }

    pub(crate) async fn take_session_startup_prewarm(&self) -> Option<SessionStartupPrewarmHandle> {
        let mut state = self.state.lock().await;
        state.take_session_startup_prewarm()
    }

    pub(crate) async fn set_session_startup_transport(
        &self,
        startup_transport: SessionStartupTransportHandle,
    ) {
        let mut state = self.state.lock().await;
        state.set_session_startup_transport(startup_transport);
    }

    pub(crate) async fn take_session_startup_transport(
        &self,
    ) -> Option<SessionStartupTransportHandle> {
        let mut state = self.state.lock().await;
        state.take_session_startup_transport()
    }

    pub(crate) async fn get_config(&self) -> std::sync::Arc<Config> {
        let state = self.state.lock().await;
        state
            .session_configuration
            .original_config_do_not_use
            .clone()
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        let state = self.state.lock().await;
        state.session_configuration.thread_config_snapshot()
    }

    pub(crate) async fn user_instructions(&self) -> Option<codex_extension_api::UserInstructions> {
        self.services.agents_md_manager.user_instructions()
    }

    pub(crate) async fn refresh_runtime_config(&self, next_config: Config) {
        self.refresh_runtime_config_features(next_config, &[]).await;
    }

    pub(crate) async fn refresh_runtime_config_features(
        &self,
        next_config: Config,
        refreshed_features: &[Feature],
    ) {
        self.refresh_runtime_config_features_inner(next_config, refreshed_features, true)
            .await;
    }

    pub(crate) async fn refresh_runtime_config_features_after_discovery_invalidation(
        &self,
        next_config: Config,
        refreshed_features: &[Feature],
    ) {
        self.refresh_runtime_config_features_inner(next_config, refreshed_features, false)
            .await;
    }

    async fn refresh_runtime_config_features_inner(
        &self,
        next_config: Config,
        refreshed_features: &[Feature],
        invalidate_discovery: bool,
    ) {
        let refresh_started = std::time::Instant::now();
        // Serialize refreshes with environment/profile updates while preparing
        // the hooks that will be published with this configuration.
        let Ok(_refresh_guard) = self.configuration_update_lock.acquire().await else {
            unreachable!("configuration update semaphore is never closed");
        };
        // Refresh only the user layer from the incoming snapshot. Preserve thread-local
        // layers such as request/session overrides that were present when this session
        // was created.
        let notify_config_contributors = !self.services.extensions.config_contributors().is_empty();
        let config = {
            let state = self.state.lock().await;
            let mut config = (*state.session_configuration.original_config_do_not_use).clone();
            config.config_layer_stack = config
                .config_layer_stack
                .with_user_layer_from(&next_config.config_layer_stack)
                .into();
            for feature in refreshed_features {
                let enabled = next_config.features.enabled(*feature);
                if let Err(err) = config.features.set_enabled(*feature, enabled) {
                    tracing::warn!(
                        feature = feature.key(),
                        error = %err,
                        "failed to refresh runtime feature enablement"
                    );
                }
            }
            config.tool_suggest =
                resolve_tool_suggest_config_from_layer_stack(&config.config_layer_stack);
            Arc::new(config)
        };
        if invalidate_discovery {
            self.services.skills_service.clear_cache();
            self.services.plugins_manager.clear_cache();
        }
        let environments = self.services.turn_environments.snapshot().await;
        let hooks = build_hooks_for_config(
            config.as_ref(),
            self.services.plugins_manager.as_ref(),
            environments.single_local_environment(),
        )
        .await;

        let (previous_config, new_config) = {
            let mut state = self.state.lock().await;
            let previous_config = notify_config_contributors
                .then(|| Self::build_effective_session_config(&state.session_configuration));
            // Cancellation during hook discovery leaves both accepted values intact.
            // Preserve ordinary session updates accepted while the build was pending.
            state.session_configuration.original_config_do_not_use = config;
            self.services
                .latest_mcp_runtime()
                .manager()
                .invalidate_resource_caches();
            let hooks = hooks.with_run_history_from(&self.services.hooks.load());
            self.services.hooks.store(Arc::new(hooks));
            let new_config = notify_config_contributors
                .then(|| Self::build_effective_session_config(&state.session_configuration));
            (previous_config, new_config)
        };
        self.emit_config_changed_contributors(previous_config.as_ref(), new_config.as_ref());
        if plugin_refresh_trace_enabled() {
            tracing::info!(target: "codex_plugin_refresh", phase = "user_layer_published", thread_id = %self.thread_id, elapsed_us = refresh_started.elapsed().as_micros() as u64);
        }
    }

    fn emit_config_changed_contributors(
        &self,
        previous_config: Option<&Config>,
        new_config: Option<&Config>,
    ) {
        let (Some(previous_config), Some(new_config)) = (previous_config, new_config) else {
            return;
        };
        if previous_config == new_config {
            return;
        }
        for contributor in self.services.extensions.config_contributors() {
            contributor.on_config_changed(
                &self.services.session_extension_data,
                &self.services.thread_extension_data,
                previous_config,
                new_config,
            );
        }
    }

    pub(crate) async fn reload_user_config_layer(&self) {
        // Refresh layer-backed runtime state for an existing session, including enabled plugin,
        // skill, and hook state. Derived config fields such as feature gates and legacy notify
        // settings remain session-static.
        //
        // Prefer `refresh_runtime_config()` when the host can already provide a materialized
        // config snapshot. This file-based path exists for legacy local reload flows.
        let config_toml_paths = {
            let state = self.state.lock().await;
            let config = &state.session_configuration.original_config_do_not_use;
            let user_layers = config.config_layer_stack.get_user_layers(
                ConfigLayerStackOrdering::LowestPrecedenceFirst,
                /*include_disabled*/ true,
            );
            if user_layers.is_empty() {
                vec![
                    state
                        .session_configuration
                        .codex_home
                        .join(CONFIG_TOML_FILE),
                ]
            } else {
                user_layers
                    .into_iter()
                    .filter(|layer| !layer.is_disabled())
                    .filter_map(|layer| match &layer.name {
                        ConfigLayerSource::User { file, .. } => Some(file.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            }
        };

        let mut reloaded_user_configs = Vec::with_capacity(config_toml_paths.len());
        for config_toml_path in config_toml_paths {
            let user_config = match tokio::fs::read_to_string(&config_toml_path).await {
                Ok(contents) => match toml::from_str::<toml::Value>(&contents) {
                    Ok(config) => config,
                    Err(err) => {
                        warn!("failed to parse user config while reloading layer: {err}");
                        return;
                    }
                },
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    toml::Value::Table(Default::default())
                }
                Err(err) => {
                    warn!("failed to read user config while reloading layer: {err}");
                    return;
                }
            };
            reloaded_user_configs.push((config_toml_path, user_config));
        }

        let next_config = {
            let state = self.state.lock().await;
            let mut config = (*state.session_configuration.original_config_do_not_use).clone();
            for (config_toml_path, user_config) in reloaded_user_configs {
                config.config_layer_stack = config
                    .config_layer_stack
                    .with_user_config(&config_toml_path, user_config)
                    .into();
            }
            if let Some(value) = config
                .config_layer_stack
                .effective_config()
                .get("tool_suggest")
                && let Err(err) = value
                    .clone()
                    .try_into::<codex_config::types::ToolSuggestConfig>()
            {
                warn!("failed to parse tool_suggest while reloading layer: {err}");
                return;
            }
            config.tool_suggest =
                resolve_tool_suggest_config_from_layer_stack(&config.config_layer_stack);
            config
        };
        self.refresh_runtime_config(next_config).await;
    }

    async fn build_settings_update_items(
        &self,
        reference_context_item: Option<&TurnContextItem>,
        current_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        // TODO: Make context updates a pure diff of persisted previous/current TurnContextItem
        // state so replay/backtracking is deterministic. Runtime inputs that affect model-visible
        // context (exec policy, feature gates, previous-turn bridge) should be persisted
        // state or explicit non-state replay events.
        let previous_turn_settings = {
            let state = self.state.lock().await;
            state.previous_turn_settings()
        };
        let exec_policy = self.services.exec_policy.current();
        crate::context_manager::updates::build_settings_update_items(
            reference_context_item,
            previous_turn_settings.as_ref(),
            current_context,
            exec_policy.as_ref(),
            self.features.enabled(Feature::Personality),
        )
    }

    /// Record a terminal CodexErr before the app-server completion notification is reduced.
    pub(crate) fn track_turn_codex_error(&self, turn_context: &TurnContext, error: &CodexErr) {
        self.services
            .analytics_events_client
            .track_turn_codex_error(TurnCodexErrorFact::from_codex_err(
                self.thread_id.to_string(),
                turn_context.sub_id.clone(),
                error,
            ));
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) fn send_event<'a>(
        &'a self,
        turn_context: &'a TurnContext,
        msg: EventMsg,
    ) -> impl std::future::Future<Output = ()> + 'a {
        // Event persistence has a large state machine. Return heap storage so
        // every caller does not reserve construction and await copies for each
        // event in its polling frame. All work remains on the caller's task.
        Box::pin(async move {
            let post_dispatch = self
                .send_event_before_post_dispatch(turn_context, msg, true, None)
                .await;
            self.finish_prepared_event_dispatch(turn_context, post_dispatch)
                .await;
        })
    }

    /// Publish the original terminal outcome once, retaining its actual live-delivery receipt.
    /// Parent notification is a separate finalizer phase so recovery can finish it independently.
    pub(crate) async fn publish_terminal_event(
        &self,
        turn_context: &TurnContext,
        msg: EventMsg,
        published: &mut Option<EventMsg>,
        recovering_publication: bool,
    ) -> EventMsg {
        self.publish_terminal_event_with_barrier(
            turn_context,
            msg,
            published,
            recovering_publication,
            async || self.flush_rollout_durable().await,
        )
        .await
    }

    #[expect(
        clippy::expect_used,
        reason = "Terminal dispatch retains its original source"
    )]
    pub(crate) async fn publish_terminal_event_with_barrier(
        &self,
        turn_context: &TurnContext,
        msg: EventMsg,
        published: &mut Option<EventMsg>,
        recovering_publication: bool,
        durable_barrier: impl AsyncFnOnce() -> std::io::Result<()>,
    ) -> EventMsg {
        if let Some(event) = published.as_ref() {
            return event.clone();
        }
        debug_assert!(matches!(
            &msg,
            EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
        ));
        // An append can have succeeded before a panic prevented live delivery. Reuse that
        // accepted outcome and do not append a contradictory recovery terminal after it.
        let (mut msg, persist, mut persistence_error) = if recovering_publication
            && let Some(live_thread) = self.live_thread()
        {
            match tokio::time::timeout(
                Duration::from_secs(5),
                live_thread.terminal_event(&turn_context.sub_id, false),
            )
            .await
            {
                Ok(Ok(Some(event))) => (event, false, None),
                Ok(Ok(None)) => (msg, true, None),
                Ok(Err(error)) => {
                    warn!(
                        "terminal persistence is ambiguous; suppressing another append: {error:#}"
                    );
                    (
                        msg,
                        false,
                        Some(format!("terminal append could not be confirmed ({error})")),
                    )
                }
                Err(_) => (
                    msg,
                    false,
                    Some(
                        "terminal append recovery timed out; no duplicate was appended".to_string(),
                    ),
                ),
            }
        } else {
            (msg, true, None)
        };
        // Stage the canonical terminal before the stable-storage barrier. Live
        // consumers may stop as soon as they see this event, so its persistence
        // outcome must already be known (or explicitly uncertain) at delivery.
        if let Some(live_thread) = self.live_thread()
            && live_thread.should_persist_event(&msg)
        {
            // A blocked recorder must not strand interruption or successor
            // admission. Timeout means unknown, not a successful durable write;
            // a queued recorder operation may still finish after this waiter.
            match tokio::time::timeout(Duration::from_secs(5), async {
                if persist {
                    self.persist_rollout_items_ordered(&[RolloutItem::EventMsg(msg.clone())])
                        .await?;
                }
                durable_barrier().await
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    persistence_error.get_or_insert_with(|| format!("terminal record ({error})"));
                }
                Err(_) => {
                    persistence_error.get_or_insert_with(|| {
                        "terminal durability was not acknowledged within 5 seconds".to_string()
                    });
                }
            }
        }
        if let Some(error) = persistence_error {
            let warning = crate::tasks::unsaved_turn_record_warning(&turn_context.sub_id, &error);
            warn!(turn_id = %turn_context.sub_id, "{warning}");
            let timing = match &mut msg {
                EventMsg::TurnComplete(event) => &mut event.timing,
                EventMsg::TurnAborted(event) => &mut event.timing,
                _ => unreachable!("terminal publication only accepts terminal events"),
            };
            let assessment = timing
                .get_or_insert_with(Default::default)
                .completion_assessment
                .get_or_insert_with(Default::default);
            let already_reported = assessment
                .verification_gaps
                .iter()
                .any(|gap| gap.contains("not durably saved"));
            if !assessment.verification_gaps.contains(&warning) {
                assessment.verification_gaps.push(warning.clone());
            }
            // Legacy clients still receive their diagnostic before terminal;
            // required terminal metadata survives optional-notification loss.
            if !already_reported {
                // Do not re-enter the failed/stalled recorder just to report
                // its failure. The required terminal carries the same caveat.
                self.deliver_event_raw_with_receipt(
                    Event {
                        id: turn_context.sub_id.clone(),
                        msg: EventMsg::Warning(WarningEvent { message: warning }),
                    },
                    None,
                )
                .await;
            }
        }
        let post_dispatch = self
            .send_event_before_post_dispatch(turn_context, msg, false, Some(published))
            .await;
        post_dispatch
            .terminal_source
            .expect("terminal publication retains its original outcome")
    }

    pub(crate) async fn finish_terminal_event_dispatch(
        &self,
        turn_context: &TurnContext,
        event: &EventMsg,
    ) -> bool {
        self.finish_prepared_event_dispatch(
            turn_context,
            PreparedEventDispatch::new(event, self.show_raw_agent_reasoning()),
        )
        .await
    }

    async fn send_event_before_post_dispatch(
        &self,
        turn_context: &TurnContext,
        msg: EventMsg,
        persist: bool,
        publication_receipt: Option<&mut Option<EventMsg>>,
    ) -> PreparedEventDispatch {
        let visible_candidate = match &msg {
            EventMsg::AgentMessageContentDelta(event) => !event.delta.is_empty(),
            EventMsg::ReasoningContentDelta(event) => !event.delta.is_empty(),
            EventMsg::ReasoningRawContentDelta(event) => self.show_raw_agent_reasoning() && !event.delta.is_empty(),
            EventMsg::AgentMessage(event) => !event.message.is_empty(),
            EventMsg::ItemStarted(event) => match &event.item {
                codex_protocol::items::TurnItem::AgentMessage(message) => message.content.iter().any(|part| match part {
                    codex_protocol::items::AgentMessageContent::Text { text } => !text.is_empty(),
                }),
                codex_protocol::items::TurnItem::Reasoning(reasoning) => reasoning.summary_text.iter().any(|text| !text.is_empty()),
                _ => false,
            },
            EventMsg::ItemCompleted(event) => match &event.item {
                codex_protocol::items::TurnItem::AgentMessage(message) => message.content.iter().any(|part| match part {
                    codex_protocol::items::AgentMessageContent::Text { text } => !text.is_empty(),
                }),
                codex_protocol::items::TurnItem::Reasoning(reasoning) => reasoning.summary_text.iter().any(|text| !text.is_empty()),
                _ => false,
            },
            _ => false,
        };
        let post_dispatch = PreparedEventDispatch::new(&msg, self.show_raw_agent_reasoning());
        if let EventMsg::Error(error) = &msg
            && error.affects_turn_status()
        {
            turn_context
                .terminal_error
                .lock()
                .await
                .replace(error.clone());
        }
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        if visible_candidate {
            let mut local_receipt = None;
            let receipt = publication_receipt.unwrap_or(&mut local_receipt);
            self.send_event_raw_with_receipt(event, persist, Some(&mut *receipt)).await;
            if let Some(published) = receipt.as_ref() {
                turn_context.record_first_visible_publication();
                turn_context.turn_timing_state.record_assistant_text_publication(published);
            }
        } else {
            self.send_event_raw_with_receipt(event, persist, publication_receipt).await;
        }
        post_dispatch
    }

    async fn finish_prepared_event_dispatch(
        &self,
        turn_context: &TurnContext,
        post_dispatch: PreparedEventDispatch,
    ) -> bool {
        let parent_notification_completed = match post_dispatch.terminal_source.as_ref() {
            Some(terminal_source) => {
                self.maybe_notify_parent_of_terminal_turn(turn_context, terminal_source)
                    .await
            }
            None => true,
        };
        for legacy in post_dispatch.legacy_events {
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
        parent_notification_completed
    }

    /// Forwards terminal turn events from spawned MultiAgentV2 children to their direct parent.
    pub(crate) async fn maybe_notify_parent_of_terminal_turn(
        &self,
        turn_context: &TurnContext,
        msg: &EventMsg,
    ) -> bool {
        if turn_context.multi_agent_version != MultiAgentVersion::V2 {
            return true;
        }

        if !matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) {
            return true;
        }

        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &turn_context.session_source
        else {
            return true;
        };

        let mut status = match turn_context.terminal_error.lock().await.clone() {
            Some(error) => {
                let status = AgentStatus::Errored(error.message);
                self.update_agent_status(&turn_context.sub_id, msg, status.clone());
                status
            }
            None => {
                let Some(status) = agent_status_from_event(msg) else {
                    return true;
                };
                status
            }
        };
        if !is_final(&status) {
            if let Some(binding) = turn_context.agent_task_binding.get().and_then(Option::as_ref)
                && let Err(error) = self.services.agent_control.task_coordinator()
                    .record_host_completion_observation(binding, self.thread_id, &turn_context.sub_id,
                        format!("Host terminal boundary for turn {}: turn interrupted; partial effects may remain.", turn_context.sub_id), None).await
            {
                warn!(%error, "failed to retain interrupted host terminal boundary");
                return false;
            }
            return true;
        }

        let mut protected_host_summary = match msg {
            EventMsg::TurnComplete(event) => Some(
                event
                    .timing
                    .as_ref()
                    .and_then(|timing| timing.completion_assessment.as_ref())
                    .and_then(|assessment| assessment.bounded_summary(&event.turn_id))
                    .unwrap_or_else(|| crate::agent::status::NO_COMPLETION_DIAGNOSTICS.to_string()),
            ),
            EventMsg::TurnAborted(_) => Some(format!(
                "Host terminal boundary for turn {}: turn aborted; partial effects may remain.",
                turn_context.sub_id
            )),
            _ => None,
        };
        let task_coordinator = self.services.agent_control.task_coordinator();
        if let Some(binding) = turn_context
            .agent_task_binding
            .get()
            .and_then(Option::as_ref)
        {
            task_coordinator.mark_host_terminal(binding.assignment_id, binding.attempt_id);
            match task_coordinator
                .retain_terminal_outcome(binding.clone(), turn_context.sub_id.clone(), &status, msg)
                .await
            {
                Ok(false) => return true, // A successor attempt owns this path now.
                Ok(true) => {}
                Err(error) => {
                    warn!(%error, "failed to retain terminal reconciliation obligation");
                    return false;
                }
            }
            if let Some(summary) = protected_host_summary.clone()
                && let Err(error) = task_coordinator
                    .record_host_completion_and_seal(
                        binding,
                        child_agent_path,
                        self.thread_id,
                        &turn_context.sub_id,
                        summary,
                        &status,
                    )
                    .await
            {
                warn!(%error, "failed to retain host completion observations");
                return false;
            }
            let assignment_id = binding.assignment_id;
            match task_coordinator.get_agent_task(assignment_id, None).await {
                Ok(task) => {
                    if task.current_attempt.attempt_id != binding.attempt_id {
                        return true;
                    }
                    protected_host_summary =
                        crate::agent::status::completion_diagnostics_from_task(&task);
                    if let Some(durable_status) =
                        agent_status_from_task(&task, Some((binding.attempt_id, &status)))
                    {
                        status = durable_status;
                        self.update_agent_status(&turn_context.sub_id, msg, status.clone());
                    }
                    task_coordinator.emit_terminal_metrics_from_snapshot(
                        &task,
                        &turn_context.session_telemetry,
                    );
                }
                Err(error) => {
                    warn!(%assignment_id, %error, "failed to load durable typed-agent outcome for parent notification");
                    return false;
                }
            }
        }

        let delivered = self
            .forward_child_completion_to_parent(
                turn_context,
                *parent_thread_id,
                child_agent_path,
                status,
                // The clean-terminal marker only supersedes older durable observations
                // (see `completion_diagnostics_from_task`); it is not a parent-visible fact.
                protected_host_summary
                    .as_deref()
                    .filter(|summary| *summary != crate::agent::status::NO_COMPLETION_DIAGNOSTICS),
            )
            .await;
        if delivered
            && let Some(binding) = turn_context
                .agent_task_binding
                .get()
                .and_then(Option::as_ref)
            && let Err(error) = task_coordinator
                .acknowledge_terminal_outcome(binding.attempt_id, &turn_context.sub_id)
                .await
        {
            warn!(%error, "terminal reconciliation acknowledgment remains pending");
        }
        delivered
    }

    /// Sends the standard completion envelope from a spawned MultiAgentV2 child to its parent.
    async fn forward_child_completion_to_parent(
        &self,
        turn_context: &TurnContext,
        parent_thread_id: ThreadId,
        child_agent_path: &codex_protocol::AgentPath,
        status: AgentStatus,
        protected_host_summary: Option<&str>,
    ) -> bool {
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| codex_protocol::AgentPath::try_from(parent).ok())
        else {
            return true;
        };

        let receipt = turn_context
            .agent_task_binding
            .get()
            .and_then(Option::as_ref)
            .map(|binding| {
                format!(
                    "Receipt: get_agent_task({{\"assignment_id\":\"{}\"}})\nProducer attempt: {}",
                    binding.assignment_id, binding.attempt_id,
                )
            });
        let Some(mut communication) =
            crate::session_prefix::inter_agent_completion_communication_with_diagnostics(
                parent_agent_path,
                child_agent_path.clone(),
                self.thread_id,
                &status,
                receipt.as_deref(),
                protected_host_summary,
            )
        else {
            return true;
        };
        // Reuse mailbox identity deduplication for repeated terminal dispatch
        // of this turn. Preserve the existing stronger error identity, which
        // also coalesces repeated identical failed attempts.
        if communication.id.is_none() {
            communication.id = Some(codex_protocol::ResponseItemId::from_server(format!(
                "agent-result-{}-{}", self.thread_id, turn_context.sub_id,
            )));
        }
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, self.thread_id);
        if let Err(err) = self
            .services
            .agent_control
            .send_inter_agent_communication(parent_thread_id, communication, context)
            .await
        {
            debug!("failed to notify parent thread {parent_thread_id}: {err}");
            // The execution is finished. Retain a parent-visible recovery fact
            // in the existing typed receipt owner instead of resampling the child
            // or relying on this finalizer being entered again. A separate key
            // keeps this idempotent without overwriting completion diagnostics.
            if let Some(binding) = turn_context
                .agent_task_binding
                .get()
                .and_then(Option::as_ref)
            {
                let summary = format!(
                    "Automatic completion delivery to parent {parent_thread_id} failed. Read get_agent_task({{\"assignment_id\":\"{}\"}}) to recover the retained outcome; do not re-execute the child.",
                    binding.assignment_id,
                );
                if let Err(error) = self
                    .services
                    .agent_control
                    .task_coordinator()
                    .record_host_completion_observation(
                        binding,
                        self.thread_id,
                        &format!("{}:parent-delivery", turn_context.sub_id),
                        summary,
                        None,
                    )
                    .await
                {
                    warn!(%error, "failed to retain parent completion delivery recovery state");
                }
            }
            return false;
        }
        true
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        self.send_event_raw_with_persistence(event, /*persist*/ true)
            .await;
    }

    /// Delivers an event without creating a local rollout for a thread that has not materialized.
    pub(crate) async fn send_event_raw_without_materializing_rollout(&self, event: Event) {
        let persist = match self.current_rollout_path().await {
            Ok(Some(path)) => codex_rollout::existing_rollout_path(&path).await.is_some(),
            Ok(None) => true,
            Err(err) => {
                warn!("failed to check whether thread persistence is materialized: {err}");
                true
            }
        };
        self.send_event_raw_with_persistence(event, persist).await;
    }

    async fn send_event_raw_with_persistence(&self, event: Event, persist: bool) {
        self.send_event_raw_with_receipt(event, persist, None).await;
    }

    async fn send_event_raw_with_receipt(
        &self,
        event: Event,
        persist: bool,
        publication_receipt: Option<&mut Option<EventMsg>>,
    ) {
        // Normal history and opt-in activity share the ordered writer, but never append the
        // same event twice. Activity remains observational and cannot enter model history.
        if persist
            && self
                .live_thread()
                .is_some_and(|live_thread| live_thread.should_persist_event(&event.msg))
        {
            let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
            if matches!(&event.msg, EventMsg::TurnStarted(_)) {
                // TurnStarted is an ordered lifecycle boundary, not a durability boundary. Keep
                // live delivery immediate and let terminal finalization flush the complete turn
                // prefix once, before the matching terminal event is dispatched.
                if let Err(error) = self.persist_rollout_items_ordered(&rollout_items).await {
                    error!("failed to queue turn-start rollout item: {error:#}");
                }
            } else {
                self.persist_rollout_items(&rollout_items).await;
            }
        } else if persist
            && self.features.enabled(Feature::RolloutActivity)
            && codex_rollout::is_rollout_activity_event(&event.msg)
            && let Some(live_thread) = self.live_thread()
            && let Err(error) = live_thread.append_activity_event_ordered(&event.msg).await
        {
            error!("failed to queue rollout activity event: {error:#}");
        }
        self.deliver_event_raw_with_receipt(event, publication_receipt)
            .await;
    }

    async fn deliver_event_raw(&self, event: Event) {
        self.deliver_event_raw_with_receipt(event, None).await;
    }

    fn update_agent_status(&self, event_id: &str, msg: &EventMsg, status: AgentStatus) {
        let mut owner = self
            .agent_status_turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*self.agent_status.borrow(), AgentStatus::Shutdown) {
            return;
        }
        let turn_id = match msg {
            EventMsg::TurnStarted(event) => {
                *owner = Some(event.turn_id.clone());
                event.turn_id.as_str()
            }
            EventMsg::TurnComplete(event) => event.turn_id.as_str(),
            EventMsg::TurnAborted(event) => event.turn_id.as_deref().unwrap_or(event_id),
            EventMsg::ShutdownComplete => {
                self.agent_status.send_replace(status);
                return;
            }
            _ => event_id,
        };
        if owner.as_deref().is_none_or(|owner| owner == turn_id) {
            self.agent_status.send_replace(status);
        }
    }

    async fn deliver_event_raw_with_receipt(
        &self,
        event: Event,
        publication_receipt: Option<&mut Option<EventMsg>>,
    ) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.update_agent_status(&event.id, &event.msg, status);
        }
        let published_msg = publication_receipt.as_ref().map(|_| event.msg.clone());
        let publishes_visible_content = match &event.msg {
            EventMsg::AgentMessageContentDelta(delta) => !delta.delta.is_empty(),
            EventMsg::ReasoningContentDelta(delta) => !delta.delta.is_empty(),
            EventMsg::ReasoningRawContentDelta(delta) => !delta.delta.is_empty(),
            EventMsg::AgentMessage(message) => !message.message.is_empty(),
            _ => false,
        };
        let publication_turn_id = publishes_visible_content.then(|| event.id.clone());
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.histogram(
                "codex.event_channel.occupancy",
                self.tx_event.len() as i64,
                &[],
            );
        }
        // Keep the timer alive until after the publication receipt is stored:
        // telemetry must not run between channel acceptance and receipt ownership.
        let _send_wait_timer =
            codex_otel::start_global_timer("codex.event_channel.send_wait.duration_ms", &[]);
        let result = codex_mcp::event_queue::send_event(&self.tx_event, event).await;
        match result {
            Ok(()) => {
                // No await separates channel acceptance from recording the receipt in the
                // existing finalizer owner. A later post-dispatch panic must not publish again.
                if let (Some(receipt), Some(msg)) = (publication_receipt, published_msg) {
                    *receipt = Some(msg);
                }
                if let Some(turn_id) = publication_turn_id {
                    tracing::info!(event.name = "codex.model_output_published", %turn_id,
                        endpoint = "post_filter_session_channel_acceptance", client_visible = false,
                        clock_id = codex_protocol::timing::process_clock_domain_id(),
                        server_publication_ns = u64::try_from(codex_protocol::timing::process_monotonic_ns()).ok(),
                        at_unix_ms = chrono::Utc::now().timestamp_millis(), clock = "unix_wall_ms",
                        "visible-content publication is separate from provider arrival and client rendering");
                }
            }
            Err(e) => debug!("dropping event because channel is closed: {e}"),
        }
    }

    /// Offers a live-only event without waiting for a consumer or persisting it.
    pub(crate) fn try_send_live_event(&self, event: Event) {
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.update_agent_status(&event.id, &event.msg, status);
        }
        // Live-only output is best-effort: a saturated or oversized offer drops this
        // event; it must not close the stream that required events still use.
        if !codex_mcp::event_queue::event_fits_budget(&event) {
            debug!("dropping live event that exceeds the event budget");
            return;
        }
        if let Err(err) = self.tx_event.try_send(event) {
            debug!("dropping live event because channel is unavailable: {err}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.thread_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
                started_at_ms: now_unix_timestamp_ms(),
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        self.emit_turn_item_completed_with_legacy_events(turn_context, item, true)
            .await;
    }

    /// Closing a failed compaction attempt is not a legacy ContextCompacted success.
    pub(crate) async fn emit_turn_item_completed_with_legacy_events(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
        emit_legacy: bool,
    ) {
        record_turn_ttfm_metric(turn_context, &item);
        let event = EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: self.thread_id,
            turn_id: turn_context.sub_id.clone(),
            item,
            completed_at_ms: now_unix_timestamp_ms(),
        });
        let mut dispatch = self
            .send_event_before_post_dispatch(turn_context, event, true, None)
            .await;
        if !emit_legacy {
            dispatch.legacy_events.clear();
        }
        self.finish_prepared_event_dispatch(turn_context, dispatch)
            .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .codex_home()
            .clone();

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    pub(crate) async fn turn_context_for_sub_id(&self, sub_id: &str) -> Option<Arc<TurnContext>> {
        let active = self.active_turn.lock().await;
        active
            .as_ref()
            .and_then(|turn| turn.task.as_ref())
            .filter(|task| task.turn_context.sub_id == sub_id)
            .map(|task| Arc::clone(&task.turn_context))
    }

    pub(crate) async fn record_execpolicy_amendment_message(
        &self,
        sub_id: &str,
        amendment: &ExecPolicyAmendment,
    ) {
        let Some(prefixes) = format_allow_prefixes(vec![amendment.command.clone()]) else {
            warn!("execpolicy amendment for {sub_id} had no command prefix");
            return;
        };
        let message: ResponseItem =
            ContextualUserFragment::into(ApprovedCommandPrefixSaved::new(prefixes));
        let turn_context = self.turn_context_for_sub_id(sub_id).await;
        if let Err(error) = self
            .inject_no_new_turn(vec![message], turn_context.as_deref())
            .await
        {
            self.send_event_raw(Event {
                id: sub_id.to_string(),
                msg: EventMsg::Warning(codex_protocol::protocol::WarningEvent {
                    message: format!("Saved policy notification was not delivered: {error}"),
                }),
            })
            .await;
        }
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `call_id` + `approval_id` so matching responses
    /// are delivered to the correct in-flight turn. If the pending approval is
    /// cleared before a response arrives, treat it as an abort so interrupted
    /// turns do not continue on a synthetic denial.
    ///
    /// Note that if `available_decisions` is `None`, then the other fields will
    /// be used to derive the available decisions via
    /// [ExecApprovalRequestEvent::default_available_decisions].
    #[allow(clippy::too_many_arguments)]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_command_approval<Cwd>(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        approval_id: Option<String>,
        environment_id: Option<String>,
        command: Vec<String>,
        cwd: Cwd,
        reason: Option<String>,
        network_approval_context: Option<NetworkApprovalContext>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
        additional_permissions: Option<AdditionalPermissionProfile>,
        available_decisions: Option<Vec<ReviewDecision>>,
    ) -> ReviewDecision
    where
        Cwd: Into<PathUri>,
    {
        let _elicitation = self.services.elicitations.register();
        //  command-level approvals use `call_id`.
        // `approval_id` is only present for subcommand callbacks (execve intercept)
        let effective_approval_id = approval_id.clone().unwrap_or_else(|| call_id.clone());
        // Add the tx_approve callback to the map before sending the request.
        let cleanup = CancellationToken::new();
        let _cleanup_on_drop = cleanup.clone().drop_guard();
        let (tx_approve, rx_approve) = oneshot::channel();
        let (originating_turn_state, prev_entry) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    if active_turn_has_other_id(at, &turn_context.sub_id) {
                        return ReviewDecision::Abort;
                    }
                    let mut ts = at.turn_state.lock().await;
                    let previous =
                        ts.insert_pending_approval(effective_approval_id.clone(), tx_approve);
                    (Some(Arc::clone(&at.turn_state)), previous)
                }
                None => (None, None),
            }
        };
        if let Some(turn_state) = originating_turn_state {
            let key = effective_approval_id.clone();
            self.terminal_tasks.spawn(async move {
                cleanup.cancelled().await;
                // A stale requester must preserve a replacement with a live receiver.
                turn_state.lock().await.remove_closed_pending_approval(&key);
            });
        }
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for call_id: {effective_approval_id}");
        }
        drop(prev_entry);

        let parsed_cmd = parse_command(&command);
        let proposed_network_policy_amendments = network_approval_context.as_ref().map(|context| {
            vec![
                NetworkPolicyAmendment {
                    host: context.host.clone(),
                    action: NetworkPolicyRuleAction::Allow,
                },
                NetworkPolicyAmendment {
                    host: context.host.clone(),
                    action: NetworkPolicyRuleAction::Deny,
                },
            ]
        });
        let available_decisions = available_decisions.unwrap_or_else(|| {
            ExecApprovalRequestEvent::default_available_decisions(
                network_approval_context.as_ref(),
                proposed_execpolicy_amendment.as_ref(),
                proposed_network_policy_amendments.as_deref(),
                additional_permissions.as_ref(),
            )
        });
        let cwd_uri = cwd.into();
        let cwd = cwd_uri
            .to_abs_path()
            .unwrap_or_else(|_| turn_context.cwd().clone());
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            approval_id,
            turn_id: turn_context.sub_id.clone(),
            environment_id,
            started_at_ms: now_unix_timestamp_ms(),
            command,
            cwd,
            cwd_uri: Some(cwd_uri),
            reason,
            network_approval_context,
            proposed_execpolicy_amendment,
            proposed_network_policy_amendments,
            additional_permissions,
            available_decisions: Some(available_decisions),
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        let _interactive_wait_guard = turn_context
            .turn_timing_state
            .begin_interactive_wait(InteractiveWaitKind::Approval);
        rx_approve.await.unwrap_or(ReviewDecision::Abort)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> ReviewDecision {
        let _elicitation = self.services.elicitations.register();
        // Add the tx_approve callback to the map before sending the request.
        let approval_id = call_id.clone();
        let cleanup = CancellationToken::new();
        let _cleanup_on_drop = cleanup.clone().drop_guard();
        let (tx_approve, rx_approve) = oneshot::channel();
        let (originating_turn_state, prev_entry) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    if active_turn_has_other_id(at, &turn_context.sub_id) {
                        return ReviewDecision::Abort;
                    }
                    let mut ts = at.turn_state.lock().await;
                    let previous = ts.insert_pending_approval(approval_id.clone(), tx_approve);
                    (Some(Arc::clone(&at.turn_state)), previous)
                }
                None => (None, None),
            }
        };
        if let Some(turn_state) = originating_turn_state {
            let key = approval_id.clone();
            self.terminal_tasks.spawn(async move {
                cleanup.cancelled().await;
                // A stale requester must preserve a replacement with a live receiver.
                turn_state.lock().await.remove_closed_pending_approval(&key);
            });
        }
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for call_id: {approval_id}");
        }
        drop(prev_entry);

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            started_at_ms: now_unix_timestamp_ms(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        let _interactive_wait_guard = turn_context
            .turn_timing_state
            .begin_interactive_wait(InteractiveWaitKind::Approval);
        rx_approve.await.unwrap_or(ReviewDecision::Abort)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn request_permissions_for_environment(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        call_id: String,
        args: RequestPermissionsArgs,
        environment: TurnEnvironmentSelection,
        cancellation_token: CancellationToken,
    ) -> Option<RequestPermissionsResponse> {
        // A dropped requester must make any late response inert, including while the
        // notification is waiting to acquire the grant storage lock.
        let cancellation_token = Arc::new(cancellation_token.child_token());
        let _cancel_on_drop = cancellation_token.as_ref().clone().drop_guard();
        match turn_context.as_ref().approval_policy.value() {
            AskForApproval::Never => {
                return Some(RequestPermissionsResponse {
                    permissions: RequestPermissionProfile::default(),
                    scope: PermissionGrantScope::Turn,
                });
            }
            AskForApproval::Granular(granular_config)
                if !granular_config.allows_request_permissions() =>
            {
                return Some(RequestPermissionsResponse {
                    permissions: RequestPermissionProfile::default(),
                    scope: PermissionGrantScope::Turn,
                });
            }
            AskForApproval::OnRequest
            | AskForApproval::UnlessTrusted
            | AskForApproval::Granular(_) => {}
        }

        let requested_permissions = args.permissions;
        let native_environment_cwd = environment.cwd.to_abs_path().ok();
        let approval_scope_id = turn_context
            .environments
            .turn_environments
            .iter()
            .find(|turn_environment| turn_environment.environment_id == environment.environment_id)
            .map(|turn_environment| turn_environment.environment.approval_scope_id().to_string());
        let approval_scope_id = if approval_scope_id.is_none()
            && turn_context
                .config
                .features
                .enabled(Feature::DeferredExecutor)
        {
            self.services
                .turn_environments
                .snapshot()
                .await
                .turn_environments
                .iter()
                .find(|turn_environment| {
                    turn_environment.environment_id == environment.environment_id
                })
                .map(|turn_environment| {
                    turn_environment.environment.approval_scope_id().to_string()
                })
        } else {
            approval_scope_id
        };
        let Some(approval_scope_id) = approval_scope_id else {
            warn!(
                environment_id = %environment.environment_id,
                "request_permissions targeted an environment outside the active turn"
            );
            return Some(RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
            });
        };

        let _elicitation = self.services.elicitations.register();
        let (tx_response, rx_response) = oneshot::channel();
        let (originating_turn_state, prev_entry) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    if active_turn_has_other_id(at, &turn_context.sub_id) {
                        return None;
                    }
                    let mut ts = at.turn_state.lock().await;
                    let previous = ts.insert_pending_request_permissions(
                        call_id.clone(),
                        PendingRequestPermissions {
                            tx_response,
                            cancellation_token: cancellation_token.clone(),
                            requested_permissions: requested_permissions.clone(),
                            environment: environment.clone(),
                            approval_scope_id: approval_scope_id.clone(),
                        },
                    );
                    (Some(Arc::clone(&at.turn_state)), previous)
                }
                None => (None, None),
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending request_permissions for call_id: {call_id}");
        }
        drop(prev_entry);
        if let Some(turn_state) = originating_turn_state.as_ref() {
            let turn_state = Arc::clone(turn_state);
            let call_id = call_id.clone();
            let cancellation_token = Arc::clone(&cancellation_token);
            // The existing requester DropGuard cancels this token even while
            // event delivery is blocked, before the local cancellation select.
            // Retain only this registration's identity in an existing owner.
            self.terminal_tasks.spawn(async move {
                cancellation_token.cancelled().await;
                turn_state
                    .lock()
                    .await
                    .remove_pending_request_permissions_if_same(&call_id, &cancellation_token);
            });
        }

        let event = EventMsg::RequestPermissions(RequestPermissionsEvent {
            call_id: call_id.clone(),
            turn_id: turn_context.sub_id.clone(),
            environment_id: Some(environment.environment_id.clone()),
            started_at_ms: now_unix_timestamp_ms(),
            reason: args.reason,
            permissions: requested_permissions,
            cwd: native_environment_cwd,
            cwd_uri: Some(environment.cwd.clone()),
        });
        self.send_event(turn_context.as_ref(), event).await;
        let _interactive_wait_guard = turn_context
            .turn_timing_state
            .begin_interactive_wait(InteractiveWaitKind::Permission);
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                if let Some(turn_state) = originating_turn_state {
                    turn_state.lock().await.remove_pending_request_permissions_if_same(
                        &call_id,
                        &cancellation_token,
                    );
                }
                None
            }
            response = rx_response => response.ok(),
        }
    }

    pub(crate) async fn request_permissions_for_cwd(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        call_id: String,
        args: RequestPermissionsArgs,
        cwd: AbsolutePathBuf,
        cancellation_token: CancellationToken,
    ) -> Option<RequestPermissionsResponse> {
        let turn_environment = match args.environment_id.as_deref() {
            Some(environment_id) => turn_context
                .environments
                .turn_environments
                .iter()
                .find(|environment| environment.environment_id == environment_id),
            None => turn_context.environments.primary(),
        };
        let Some(turn_environment) = turn_environment else {
            return Some(RequestPermissionsResponse {
                permissions: RequestPermissionProfile::default(),
                scope: PermissionGrantScope::Turn,
            });
        };
        let mut environment = turn_environment.selection();
        environment.cwd = PathUri::from_abs_path(&cwd);
        self.request_permissions_for_environment(
            turn_context,
            call_id,
            args,
            environment,
            cancellation_token,
        )
        .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn request_user_input(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let _elicitation = self.services.elicitations.register();
        let sub_id = turn_context.sub_id.clone();
        let cleanup = CancellationToken::new();
        let _cleanup_on_drop = cleanup.clone().drop_guard();
        let (tx_response, rx_response) = oneshot::channel();
        let event_id = sub_id.clone();
        let (originating_turn_state, prev_entry) = {
            let mut active = self.active_turn.lock().await;
            let at = active.as_mut()?;
            // A late tool/delegate must not register an old turn's response key
            // in whichever turn happens to be active now.
            let active_id = at
                .terminal
                .as_ref()
                .map(|terminal| terminal.turn_id())
                .or_else(|| {
                    at.task
                        .as_ref()
                        .map(|task| task.turn_context.sub_id.as_str())
                });
            if active_id != Some(sub_id.as_str()) {
                return None;
            }
            let mut ts = at.turn_state.lock().await;
            let previous = ts.insert_pending_user_input(sub_id, call_id.clone(), tx_response);
            (Arc::clone(&at.turn_state), previous)
        };
        self.terminal_tasks.spawn(async move {
            cleanup.cancelled().await;
            // The old request cannot remove a same-key replacement whose
            // response receiver is still live, even across active-turn changes.
            originating_turn_state
                .lock()
                .await
                .remove_closed_pending_user_input(&event_id);
        });
        if prev_entry.is_some() {
            warn!(
                "Overwriting existing pending user input for sub_id: {}",
                turn_context.sub_id
            );
        }
        // Replacing the keyed request must release its old waiter immediately,
        // rather than retaining its sender until the replacement also finishes.
        drop(prev_entry);

        let event = EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            questions: args.questions,
            auto_resolution_ms: args.auto_resolution_ms,
        });
        turn_context
            .turn_metadata_state
            .mark_user_input_requested_during_turn();
        self.send_event(turn_context, event).await;
        let _interactive_wait_guard = turn_context
            .turn_timing_state
            .begin_interactive_wait(InteractiveWaitKind::UserInput);
        rx_response.await.ok()
    }


    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_user_input_response_for_request(
        &self,
        sub_id: &str,
        call_id: Option<&str>,
        response: RequestUserInputResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_matching_user_input(sub_id, call_id)
                        .map(|tx_response| (tx_response, at.terminal.clone()))
                }
                None => None,
            }
        };
        match entry {
            Some((tx_response, terminal)) => {
                // Delivery and fencing share admission locks so a canceled
                // waiter is inert and an accepted consumer cannot finish first.
                if response.interrupted
                    && let Some(terminal) = terminal
                {
                    terminal
                        .mark_interrupt_pending_if(|| tx_response.send(response).is_ok())
                        .await;
                } else {
                    tx_response.send(response).ok();
                }
            }
            None => {
                warn!("No pending user input found for sub_id: {sub_id}");
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_request_permissions_response(
        &self,
        call_id: &str,
        response: RequestPermissionsResponse,
    ) {
        let (entry, originating_turn_state) = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    let entry = ts.remove_pending_request_permissions(call_id);
                    let originating_turn_state = entry.as_ref().map(|_| Arc::clone(&at.turn_state));
                    (entry, originating_turn_state)
                }
                None => (None, None),
            }
        };
        match entry {
            Some(entry) => {
                if entry.cancellation_token.is_cancelled() {
                    return;
                }
                let response = Self::normalize_request_permissions_response(
                    entry.requested_permissions,
                    response,
                    &entry.environment.cwd,
                );
                if !self
                    .record_granted_request_permissions_for_turn(
                        &response,
                        &entry.approval_scope_id,
                        originating_turn_state.as_ref(),
                        &entry.cancellation_token,
                    )
                    .await
                {
                    return;
                }
                if entry.tx_response.send(response).is_err() {
                    warn!(
                        call_id,
                        "request permissions response arrived after its receiver closed"
                    );
                }
            }
            None => {
                warn!("No pending request_permissions found for call_id: {call_id}");
            }
        }
    }

    fn normalize_request_permissions_response(
        requested_permissions: RequestPermissionProfile,
        response: RequestPermissionsResponse,
        cwd: &PathUri,
    ) -> RequestPermissionsResponse {
        if response.permissions.is_empty() {
            return response;
        }

        RequestPermissionsResponse {
            permissions: intersect_uri_permission_profiles(
                requested_permissions.into(),
                response.permissions.into(),
                cwd,
            )
            .into(),
            scope: response.scope,
        }
    }

    async fn record_granted_request_permissions_for_turn(
        &self,
        response: &RequestPermissionsResponse,
        approval_scope_id: &str,
        originating_turn_state: Option<&Arc<Mutex<crate::state::TurnState>>>,
        cancellation_token: &CancellationToken,
    ) -> bool {
        if cancellation_token.is_cancelled() {
            return false;
        }
        if response.permissions.is_empty() {
            return true;
        }
        match response.scope {
            PermissionGrantScope::Turn => {
                if let Some(turn_state) = originating_turn_state {
                    let mut ts = turn_state.lock().await;
                    if cancellation_token.is_cancelled() {
                        return false;
                    }
                    let permissions: UriAdditionalPermissionProfile =
                        response.permissions.clone().into();
                    ts.record_granted_permissions(approval_scope_id, permissions);
                }
            }
            PermissionGrantScope::Session => {
                let mut state = self.state.lock().await;
                if cancellation_token.is_cancelled() {
                    return false;
                }
                state.record_granted_permissions(
                    approval_scope_id,
                    response.permissions.clone().into(),
                );
            }
        }
        true
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn reads must stay consistent with the matching turn state"
    )]
    pub(crate) async fn granted_turn_permissions(
        &self,
        approval_scope_id: &str,
    ) -> Option<UriAdditionalPermissionProfile> {
        let (local, root_turn) = {
            let active = self.active_turn.lock().await;
            let active = active.as_ref()?;
            let ts = active.turn_state.lock().await;
            (
                ts.granted_permissions(approval_scope_id),
                active
                    .task
                    .as_ref()
                    .map(|task| task.turn_context.validation_root_turn.clone()),
            )
        };
        let Some(root_turn) = root_turn else {
            return local;
        };
        // Release child locks before inspecting root. Never carry a turn grant
        // into a different root turn or promote it to a session grant.
        let Some(root) = self
            .services
            .agent_control
            .root_session_for_permissions(self.thread_id)
            .await
        else {
            return local;
        };
        if root.thread_id.to_string() != root_turn.0 {
            return local;
        }
        let inherited = {
            let active = root.active_turn.lock().await;
            let Some(active) = active.as_ref() else {
                return local;
            };
            if active.task.as_ref().is_none_or(|task| {
                task.turn_context.sub_id != root_turn.1
                    || task.turn_context.validation_root_turn != root_turn
            }) {
                return local;
            }
            let ts = active.turn_state.lock().await;
            ts.granted_permissions(approval_scope_id)
        };
        merge_uri_permission_profiles(local.as_ref(), inherited.as_ref())
    }

    pub(crate) async fn granted_session_permissions(
        &self,
        approval_scope_id: &str,
    ) -> Option<UriAdditionalPermissionProfile> {
        let local = {
            let state = self.state.lock().await;
            state.granted_permissions(approval_scope_id)
        };
        let Some(root) = self
            .services
            .agent_control
            .root_session_for_permissions(self.thread_id)
            .await
        else {
            return local;
        };
        let inherited = {
            let state = root.state.lock().await;
            state.granted_permissions(approval_scope_id)
        };
        merge_uri_permission_profiles(local.as_ref(), inherited.as_ref())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_dynamic_tool_response(
        &self,
        turn_id: &str,
        call_id: &str,
        response: DynamicToolResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_dynamic_tool(call_id, turn_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                if tx_response.send(response).is_err() {
                    warn!(
                        call_id,
                        "dynamic tool response arrived after its receiver closed"
                    );
                }
            }
            None => {
                warn!("No pending dynamic tool call found for call_id: {call_id}");
            }
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn notify_approval(&self, approval_id: &str, decision: ReviewDecision) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(approval_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_approve) => {
                if tx_approve.send(decision).is_err() {
                    warn!(approval_id, "approval arrived after its receiver closed");
                }
            }
            None => {
                warn!("No pending approval found for call_id: {approval_id}");
            }
        }
    }

    /// Records conversation items: append to history, persist to rollout, and
    /// notify clients observing raw response items.
    pub(crate) fn prepare_conversation_items_for_history<'a>(
        &self,
        turn_context: &TurnContext,
        items: &'a [ResponseItem],
    ) -> Cow<'a, [ResponseItem]> {
        let mut items = Cow::Borrowed(items);
        let needs_image_preparation = response_items_need_preparation(&items);
        let needs_turn_id = items.iter().any(response_item_needs_turn_id);
        if needs_image_preparation || needs_turn_id {
            let items = items.to_mut();
            if needs_image_preparation {
                prepare_response_items(items);
            }
            // Most response items get their passthrough turn ID at the durable history boundary.
            if needs_turn_id {
                for item in items {
                    item.set_turn_id_if_missing(&turn_context.sub_id);
                }
            }
        }
        Self::assign_missing_response_item_ids(items)
    }

    fn assign_missing_response_item_ids(items: Cow<'_, [ResponseItem]>) -> Cow<'_, [ResponseItem]> {
        if items
            .iter()
            .all(|item| item.id().is_some_and(|id| !id.is_empty()))
        {
            return items;
        }
        let mut items = items;
        for item in items.to_mut() {
            Self::assign_missing_response_item_id(item);
        }
        items
    }

    fn prepare_owned_conversation_items_for_history(
        turn_id: &str,
        mut items: Vec<ResponseItem>,
    ) -> Vec<ResponseItem> {
        if response_items_need_preparation(&items) {
            prepare_response_items(&mut items);
        }
        for item in &mut items {
            item.set_turn_id_if_missing(turn_id);
        }
        Self::assign_missing_response_item_ids(Cow::Owned(items)).into_owned()
    }

    fn assign_missing_response_item_id(item: &mut ResponseItem) {
        if item.id().is_some_and(|id| !id.is_empty()) {
            return;
        }
        let Some(prefix) = item.id_prefix() else {
            return;
        };
        item.set_id(Some(ResponseItemId::new(prefix)));
    }

    fn record_model_visible_tool_outputs(turn_context: &TurnContext, items: &[ResponseItem]) {
        for call_id in items.iter().filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
            ResponseItem::ToolSearchOutput {
                call_id: Some(call_id),
                ..
            } => Some(call_id.as_str()),
            _ => None,
        }) {
            turn_context
                .turn_timing_state
                .record_tool_output_model_visible(call_id);
        }
    }

    pub(crate) async fn response_item_from_user_input(
        &self,
        input: Vec<UserInput>,
    ) -> std::io::Result<ResponseItem> {
        use codex_utils_image::MAX_PROMPT_IMAGE_SOURCE_BYTES;
        let mut images = std::collections::VecDeque::new();
        let mut image_index = 0;
        for item in &input {
            if matches!(item, UserInput::Image { .. } | UserInput::LocalImage { .. }) {
                image_index += 1;
            }
            if let UserInput::LocalImage { path, detail } = item {
                let acquired = async {
                    let absolute = codex_utils_absolute_path::AbsolutePathBuf::try_from(
                        std::path::absolute(path)?,
                    )
                    .map_err(std::io::Error::other)?;
                    let uri = codex_utils_path_uri::PathUri::from(absolute);
                    let fs = codex_exec_server::LOCAL_FS.as_ref();
                    let metadata = fs.get_metadata(&uri, None).await?;
                    if !metadata.is_file || metadata.size > MAX_PROMPT_IMAGE_SOURCE_BYTES as u64 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "image source must be a regular file no larger than 64 MiB",
                        ));
                    }
                    fs.read_file_bounded(&uri, MAX_PROMPT_IMAGE_SOURCE_BYTES, None)
                        .await?
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "image source changed or exceeded its size limit",
                            )
                        })
                }
                .await;
                let acquired = acquired.map_err(|error| std::io::Error::new(error.kind(),
                    format!("Required image attachment {image_index} at {} could not be read: {error}", path.display())))?;
                let path = path.clone();
                let detail = detail.unwrap_or(codex_protocol::models::DEFAULT_IMAGE_DETAIL);
                let content = tokio::task::spawn_blocking(move || {
                    codex_protocol::models::local_image_content_items_from_acquired_bytes(
                        &path,
                        Ok(acquired),
                        image_index,
                        detail,
                        LocalImagePreparation::Defer,
                    )
                })
                .await
                .map_err(std::io::Error::other)?;
                images.push_back(content);
            }
        }
        let mut response_item = if images.is_empty() {
            ResponseItem::from(ResponseInputItem::from_user_input(
                input,
                LocalImagePreparation::Defer,
            ))
        } else {
        // Only one raw file snapshot coexists with its base64 conversion.
        // Keep the original source/detail labels while assembling pure content.
        ResponseItem::from(
            ResponseInputItem::from_user_input_with_local_image_content(
                input,
                |path, index, detail| {
                    // The loop above acquires one snapshot per local image, in input
                    // order; render a missing one as that image's read failure.
                    images.pop_front().unwrap_or_else(|| {
                        codex_protocol::models::local_image_content_items_from_acquired_bytes(
                            path,
                            Err(std::io::Error::other("local image snapshot is unavailable")),
                            index,
                            detail,
                            LocalImagePreparation::Defer,
                        )
                    })
                },
            ),
        )
        };
        crate::image_preparation::prepare_required_user_images(&mut response_item)?;
        Ok(response_item)
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) -> std::io::Result<()> {
        let items = self.prepare_conversation_items_for_history(turn_context, items);
        let items = items.as_ref();
        let commit_permit = match self.durable_history_commit_gate.acquire().await {
            Ok(permit) => permit,
            Err(_) => {
                return Err(std::io::Error::other("history commit gate is closed"));
            }
        };
        let persistence_timing_guard = turn_context
            .turn_timing_state
            .begin_local_phase(TurnLocalPhase::Persistence);
        // Ordinary and durability-sensitive commits share one serialization
        // point and both append before mutating live history. That keeps rollout
        // and in-memory order identical under concurrency.
        let rollout_items = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect::<Vec<_>>();
        self.persist_rollout_items_ordered(&rollout_items).await?;
        {
            let mut state = self.state.lock().await;
            state.current_time_reminder.note_recorded_items(items);
            state.record_items(
                items.iter(),
                turn_context.model_info.truncation_policy.into(),
            );
        }
        drop(commit_permit);
        Self::record_model_visible_tool_outputs(turn_context, items);
        drop(persistence_timing_guard);
        self.send_raw_response_items(turn_context, items).await;
        Ok(())
    }

    /// Source ownership advances only with its exact admitted messages. This
    /// ordered owner survives caller cancellation, like other input commits.
    pub(crate) async fn record_additional_context_snapshot(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        values: IndexMap<String, AdditionalContextEntry>,
    ) -> std::io::Result<()> {
        let session = Arc::clone(self);
        let turn = Arc::clone(turn_context);
        session
            .durable_history_commits_in_flight
            .fetch_add(1, Ordering::AcqRel);
        let in_flight = DurableHistoryCommitInFlight {
            session: Arc::clone(&session),
        };
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let _permit = session
                .durable_history_commit_gate
                .acquire()
                .await
                .map_err(|_| std::io::Error::other("history commit gate is closed"))?;
            let mut staged = {
                let state = session.state.lock().await;
                if state.additional_context.is_current(&values) {
                    return Ok(());
                }
                state.additional_context.clone()
            };
            let inputs = staged.merge(values);
            let items = inputs
                .into_iter()
                .map(ResponseItem::from)
                .collect::<Vec<_>>();
            let items = session
                .prepare_conversation_items_for_history(&turn, &items)
                .into_owned();
            let mut records = items
                .iter()
                .cloned()
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>();
            records.push(RolloutItem::AdditionalContextState {
                version: 1,
                turn_id: turn.sub_id.clone(),
                state: serde_json::to_value(&staged).map_err(std::io::Error::other)?,
                repository_sources: None,
            });
            session.persist_rollout_items_ordered(&records).await?;
            {
                let mut state = session.state.lock().await;
                state.record_items(items.iter(), turn.model_info.truncation_policy.into());
                state.additional_context = staged;
            }
            session.send_raw_response_items(&turn, &items).await;
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub(crate) async fn record_conversation_items_durable(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        items: &[ResponseItem],
    ) -> std::io::Result<()> {
        self.record_conversation_items_committed(turn_context, items, true, None, None)
            .await
    }

    /// Append tool relay output in canonical order without forcing a durability barrier.
    /// Terminal finalization flushes the rollout before it seals tool closure, so normal
    /// continuations do not need to pay for the same barrier after every tool batch.
    pub(crate) async fn record_conversation_items_ordered(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        items: &[ResponseItem],
    ) -> std::io::Result<()> {
        self.record_conversation_items_committed(turn_context, items, false, None, None)
            .await
    }

    /// Own a completed tool response and its pending hook context before any await.
    /// Terminalization observes the same in-flight commit owner as ordered history.
    pub(crate) async fn record_tool_completion_ordered(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        call_id: &str,
        items: &[ResponseItem],
        pending_continuation_ack: Option<crate::tools::code_mode::PendingContinuationAcknowledgment>,
    ) -> std::io::Result<()> {
        self.record_conversation_items_committed(
            turn_context,
            items,
            false,
            Some(call_id.to_owned()),
            pending_continuation_ack,
        )
        .await
    }

    async fn record_conversation_items_committed(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        items: &[ResponseItem],
        wait_for_durability: bool,
        post_tool_context_call_id: Option<String>,
        pending_continuation_ack: Option<crate::tools::code_mode::PendingContinuationAcknowledgment>,
    ) -> std::io::Result<()> {
        if self.live_thread().is_some() {
            codex_rollout::RolloutRecorder::validate_response_items(items)?;
        }
        let executions = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCallOutput { call_id, .. }
                | ResponseItem::CustomToolCallOutput { call_id, .. }
                | ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    ..
                } => turn_context
                    .turn_timing_state
                    .tool_result_execution_id(call_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let persistence_timing = Arc::clone(&turn_context.turn_timing_state);
        let publish_search_context = items.iter().any(|item| {
            matches!(
                item,
                ResponseItem::ToolSearchOutput { call_id: Some(call_id), status, .. }
                    if Some(call_id) == post_tool_context_call_id.as_ref()
                        && matches!(status.as_str(), "completed" | "incomplete")
            )
        });
        let mut raw_items = items.to_vec();
        let session = Arc::clone(self);
        let turn_context = Arc::clone(turn_context);
        session
            .durable_history_commits_in_flight
            .fetch_add(1, Ordering::AcqRel);
        let in_flight = DurableHistoryCommitInFlight {
            session: Arc::clone(&session),
        };
        let owner_timing = Arc::clone(&persistence_timing);
        let commit = tokio::spawn(async move {
            let _in_flight = in_flight;
            let result = async {
                let commit_permit = session
                    .durable_history_commit_gate
                    .acquire()
                    .await
                    .map_err(|_| std::io::Error::other("durable history commit gate is closed"))?;
                let turn_id = turn_context.sub_id.clone();
                let (commit_key, recovered_items) =
                    if conversation_items_need_blocking_history_identity(&raw_items) {
                        tokio::task::spawn_blocking(move || {
                            durable_history_commit_key(&turn_id, &raw_items)
                                .map(|key| (key, raw_items))
                        })
                        .await
                        .map_err(std::io::Error::other)??
                    } else {
                        (durable_history_commit_key(&turn_id, &raw_items)?, raw_items)
                    };
                raw_items = recovered_items;
                if turn_context
                    .durable_history_completed_commits
                    .lock()
                    .await
                    .contains(&commit_key)
                {
                    if let Some(ack) = pending_continuation_ack.as_ref() {
                        session.services.code_mode_service
                            .acknowledge_owner_drained_continuations(&ack.owner_key, &ack.receipts);
                    }
                    if wait_for_durability {
                        let queued = turn_context
                            .turn_timing_state
                            .queued_tool_result_executions();
                        session.flush_rollout().await?;
                        turn_context
                            .turn_timing_state
                            .record_tool_result_executions_persisted(&queued);
                    }
                    return Ok(());
                }
                if let Some(call_id) = post_tool_context_call_id.as_ref() {
                    // Keep pending context available if append fails. The commit gate
                    // serializes retries; remove it only after accepted publication.
                    raw_items.extend(
                        turn_context
                            .pending_post_tool_contexts
                            .lock()
                            .await
                            .get(call_id)
                            .map(|context| {
                                let mut items = context.items.clone();
                                if publish_search_context {
                                    items.extend(context.search_items.iter().cloned());
                                }
                                items
                            })
                            .unwrap_or_default(),
                    );
                }
                let turn_id = turn_context.sub_id.clone();
                if session.live_thread().is_some() && post_tool_context_call_id.is_some() {
                    codex_rollout::RolloutRecorder::validate_response_items(&raw_items)?;
                }
                let items = if conversation_items_need_blocking_history_identity(&raw_items) {
                    tokio::task::spawn_blocking(move || {
                        Self::prepare_owned_conversation_items_for_history(&turn_id, raw_items)
                    })
                    .await
                    .map_err(|err| {
                        std::io::Error::other(format!("history identity task failed: {err}"))
                    })?
                } else {
                    Self::prepare_owned_conversation_items_for_history(&turn_id, raw_items)
                };
                let persistence_timing_guard = turn_context
                    .turn_timing_state
                    .begin_local_phase(TurnLocalPhase::Persistence);
                let rollout_items = items
                    .iter()
                    .cloned()
                    .map(RolloutItem::ResponseItem)
                    .collect::<Vec<_>>();
                // Record append acceptance before a separate flush can fail. A retry
                // can then retry durability without appending the same batch again.
                session
                    .persist_rollout_items_ordered(&rollout_items)
                    .await?;
                {
                    let mut state = session.state.lock().await;
                    state.current_time_reminder.note_recorded_items(&items);
                    state.record_items(
                        items.iter(),
                        turn_context.model_info.truncation_policy.into(),
                    );
                }
                turn_context
                    .durable_history_completed_commits
                    .lock()
                    .await
                    .insert(commit_key);
                if let Some(ack) = pending_continuation_ack.as_ref() {
                    session.services.code_mode_service
                        .acknowledge_owner_drained_continuations(&ack.owner_key, &ack.receipts);
                }
                if let Some(call_id) = post_tool_context_call_id.as_ref() {
                    let context = turn_context
                        .pending_post_tool_contexts
                        .lock()
                        .await
                        .remove(call_id);
                    if let Some(context) = context
                        && publish_search_context
                    {
                        turn_context.commit_tool_search_activations(context);
                    }
                }
                let durability_result = if wait_for_durability {
                    session.flush_rollout().await
                } else {
                    Ok(())
                };
                drop(commit_permit);
                Self::record_model_visible_tool_outputs(&turn_context, &items);
                if wait_for_durability && durability_result.is_ok() {
                    turn_context
                        .turn_timing_state
                        .record_tool_result_executions_persisted(&executions);
                } else {
                    turn_context
                        .turn_timing_state
                        .record_tool_result_executions_queued(&executions);
                }
                session.send_raw_response_items(&turn_context, &items).await;
                drop(persistence_timing_guard);
                durability_result
            }
            .await;
            // The write owner survives a dropped caller, so it must also own
            // failure notification before releasing terminalization's barrier.
            if result.is_err() {
                owner_timing.record_tool_result_persistence_barrier_failed();
            }
            result
        });
        let result = commit
            .await
            .map_err(|err| std::io::Error::other(format!("durable history commit failed: {err}")))
            .and_then(|result| result);
        if result.is_err() {
            persistence_timing.record_tool_result_persistence_barrier_failed();
        }
        result
    }

    pub(crate) async fn persist_missing_call_outputs_durable(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
    ) -> std::io::Result<usize> {
        let history = self.clone_history().await;
        let current_turn_items = history
            .raw_items()
            .iter()
            .filter(|item| item.turn_id() == Some(turn_context.sub_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let missing_outputs = missing_call_outputs(&current_turn_items);
        let output_count = missing_outputs.len();
        if output_count == 0 {
            return Ok(0);
        }

        let missing_call_ids = missing_outputs
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCallOutput { call_id, .. }
                | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.clone()),
                ResponseItem::ToolSearchOutput {
                    call_id: Some(call_id),
                    ..
                } => Some(call_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.services
            .unified_exec_manager
            .terminate_unpublished_processes_for_call_ids(&missing_call_ids)
            .await
            .map_err(std::io::Error::other)?;

        // A synthetic terminal output satisfies provider call/output pairing,
        // not result availability. Reuse the host receipt recognized on resume,
        // adjacent to each output in the same durable ordered batch; never infer
        // unknown effects from tool text or synthetic response-item IDs.
        let mut missing_items = Vec::with_capacity(output_count * 2);
        for output in missing_outputs {
            let receipt = match &output {
                ResponseItem::FunctionCallOutput { call_id, .. }
                | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                    Some(crate::tools::registry::model_delivery_unavailable_receipt(call_id))
                }
                _ => None,
            };
            missing_items.push(output);
            missing_items.extend(receipt);
        }
        self.record_conversation_items_durable(turn_context, &missing_items)
            .await?;
        Ok(output_count)
    }

    pub(crate) async fn record_step_world_state_if_changed(
        self: &Arc<Self>,
        previous_world_state: &Arc<WorldState>,
        step_context: &step_context::StepContext,
    ) -> std::io::Result<Arc<WorldState>> {
        let _timer = StepContextTimer::start("record_step_world_state_if_changed");
        let turn_context = step_context.turn.as_ref();
        let recovered_agents =
            if step_context.agents_md_freshness != crate::agents_md::AgentsMdFreshness::Refreshed {
                self.services.agents_md_manager.recovered_snapshot().await
            } else {
                None
            };
        // Render model-visible state from the same step used to build and run tools.
        let world_state = Arc::new(
            if turn_context
                .config
                .features
                .enabled(Feature::DeferredExecutor)
            {
                self.build_world_state_for_step(step_context).await
            } else {
                previous_world_state
                    .replacing_section(
                        crate::context::world_state::AgentsMdState::new_cached(
                            step_context.loaded_agents_md.as_deref(),
                            step_context.agents_md_stable_context.as_ref(),
                            step_context.agents_md_freshness,
                        )
                        .with_unavailable_fallback(|| {
                            previous_world_state
                                .snapshot()
                                .section("agents_md")
                                .cloned()
                                .or(recovered_agents)
                        }),
                    )
            },
        );
        // Desired-state equality is not delivery equality: a previous optional update may
        // have been omitted, or only its compact invalidation may have been admitted.
        // Reconcile captured state against history without rediscovering any sources.
        // Diff against the realized state without advancing it until the next
        // exact physical attempt accepts this update. A staged candidate is not
        // provider-realized yet, but its context is already in history: diff against
        // it, as context preparation does, so the first step after a context commit or
        // compaction does not resend every retained section as unknown.
        let staged_world_state = self
            .state
            .lock()
            .await
            .pending_context_baseline_ref()
            .map(|candidate| candidate.world_state_snapshot.clone());
        let (fragments, world_state_item, world_state_snapshot) = {
            let mut history = self.clone_history().await;
            if let Some(snapshot) = staged_world_state.as_ref() {
                history.set_world_state_baseline(snapshot.clone());
            }
            let (fragments, world_state_item) = history.update_world_state(world_state.as_ref());
            (
                fragments,
                world_state_item,
                history.world_state_baseline().unwrap_or_default(),
            )
        };
        let items = crate::context_manager::updates::merge_contextual_fragments(fragments);
        // Replacing an unaccepted candidate keeps a complete receipt; a patch against
        // its snapshot would reference state that no accepted request persisted.
        let world_state_item =
            if staged_world_state.is_some() && (world_state_item.is_some() || !items.is_empty()) {
                Some(WorldStateItem::full(
                    world_state_snapshot.clone().into_value(),
                ))
            } else {
                world_state_item
            };
        if world_state_item.is_some() || !items.is_empty() {
            // A world-state-only update must preserve the fragment receipts that were actually
            // accepted by the previous request. Rebuilding every contributor here both repeats
            // expensive context discovery and can falsely mark newly discovered fragments as
            // realized even though only the world-state delta is being recorded.
            let fragment_digests = {
                let state = self.state.lock().await;
                state
                    .pending_context_baseline()
                    .map(|candidate| candidate.fragment_digests)
                    .or_else(|| {
                        state
                            .reference_context_item()
                            .and_then(|item| item.context_provenance)
                            .map(|provenance| provenance.fragment_digests)
                    })
            };
            let fragment_digests = match fragment_digests {
                Some(fragment_digests) => fragment_digests,
                None => {
                    self.build_initial_context_with_world_state_and_mcp(
                        turn_context,
                        world_state.as_ref(),
                        false,
                    )
                    .await
                    .fragment_digests
                }
            };
            let turn_context_item = step_context.turn.to_turn_context_item_async().await;
            self.commit_prepared_context_update(
                PreparedContextUpdate {
                    turn_context: Arc::clone(&step_context.turn),
                    world_state: Arc::clone(&world_state),
                    context_items: items,
                    baseline: ContextBaselineCandidate {
                        turn_context_item,
                        world_state_snapshot,
                        world_state_item,
                        fragment_digests,
                        bound_sampling_request_id: None,
                        bound_physical_attempt_id: None,
                    },
                },
                None,
            )
            .await?;
        }
        Ok(world_state)
    }

    /// Captures one request-scoped view of dynamic state.
    ///
    /// This may refresh filesystem-derived state. Normal turns should call it only from
    /// `run_turn` and pass the result down; standalone request or history boundaries may capture
    /// their own step.
    #[tracing::instrument(name = "step_context.capture", level = "info", skip_all)]
    pub(crate) async fn capture_step_context(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
    ) -> CodexResult<Arc<StepContext>> {
        let _timer = StepContextTimer::start("capture_step_context");
        let user_intent_epoch = self.input_queue.incorporated_user_intent_epoch();
        let deferred_executor_enabled = turn_context
            .config
            .features
            .enabled(Feature::DeferredExecutor);
        let turn = &turn_context;
        let prepare_tools = |environments: TurnEnvironmentSnapshot| async move {
            let selected_capability_roots = self
                .resolve_selected_capability_roots_for_step(&environments)
                .await;
            let mcp = self
                .mcp_runtime_for_step(turn.as_ref(), &environments, &selected_capability_roots)
                .await?;
            Ok::<_, CodexErr>((selected_capability_roots, mcp))
        };
        // Refresh project instructions before each sampling step, including
        // same-scope file changes. Capture deferred environments inside instruction
        // serialization so instruction discovery and tools use the same scope.
        let (environments, agents_md_observation, prepared_tools) = if deferred_executor_enabled {
            self.services
                .agents_md_manager
                .refresh_for_step(
                    &turn_context.config,
                    self.services.turn_environments.as_ref(),
                    prepare_tools,
                )
                .await
        } else {
            let refresh = self
                .services
                .agents_md_manager
                .refresh_and_observe_shared(&turn_context.config, &turn_context.environments);
            let (observation, prepared) = tokio::join!(
                Box::pin(refresh),
                Box::pin(prepare_tools(turn_context.environments.clone())),
            );
            (turn_context.environments.clone(), observation, prepared)
        };
        let (selected_capability_roots, mcp) = prepared_tools?;
        let mut step_context = StepContext::new_with_agents_md_freshness(
            turn_context,
            environments,
            selected_capability_roots,
            mcp,
            agents_md_observation.loaded,
            agents_md_observation.stable_context,
            agents_md_observation.freshness,
        );
        step_context.user_intent_epoch = user_intent_epoch;
        step_context.plan_sampling_revision =
            Some(self.services.plan_store.sampling_revision().await);
        Ok(Arc::new(step_context))
    }

    pub(crate) async fn record_inter_agent_communication(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        mut communication: InterAgentCommunication,
    ) -> std::io::Result<()> {
        communication.set_turn_id_if_missing(&turn_context.sub_id);
        let session = Arc::clone(self);
        let turn_context = Arc::clone(turn_context);
        let in_flight = self.retain_tool_dispatch_commit();
        let commit = tokio::spawn(async move {
            let _in_flight = in_flight;
            let _permit = session
                .durable_history_commit_gate
                .acquire()
                .await
                .map_err(|_| std::io::Error::other("durable history commit gate is closed"))?;
            let response_item = communication.to_model_input_item();
            let items = session.prepare_conversation_items_for_history(
                &turn_context,
                std::slice::from_ref(&response_item),
            );
            session
                .persist_rollout_items_ordered(&[
                    RolloutItem::InterAgentCommunicationMetadata {
                        trigger_turn: communication.trigger_turn,
                    },
                    RolloutItem::ResponseItem(items[0].clone()),
                ])
                .await?;
            {
                let mut state = session.state.lock().await;
                state.current_time_reminder.note_recorded_items(&items);
                state.record_items(
                    items.iter(),
                    turn_context.model_info.truncation_policy.into(),
                );
            }
            drop(_permit);
            session.send_raw_response_items(&turn_context, &items).await;
            Ok::<_, std::io::Error>(())
        });
        commit.await.map_err(|error| {
            std::io::Error::other(format!("inter-agent history commit failed: {error}"))
        })?
    }

    async fn maybe_warn_on_server_model_mismatch(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        server_model: String,
    ) -> bool {
        let requested_model = turn_context.model_info.slug.clone();
        let server_model_normalized = server_model.to_ascii_lowercase();
        let requested_model_normalized = requested_model.to_ascii_lowercase();
        if server_model_normalized == requested_model_normalized {
            info!("server reported model {server_model} (matches requested model)");
            return false;
        }

        warn!("server reported model {server_model} while requested model was {requested_model}");

        let warning_message = format!(
            "Your account was flagged for potentially high-risk cyber activity and this request for {requested_model} was routed to {server_model} as a fallback. To regain access to {requested_model}, apply for trusted access: {CYBER_VERIFY_URL} or learn more: {CYBER_SAFETY_URL}"
        );

        self.send_event(
            turn_context,
            EventMsg::ModelReroute(ModelRerouteEvent {
                from_model: requested_model.clone(),
                to_model: server_model.clone(),
                reason: ModelRerouteReason::HighRiskCyberActivity,
            }),
        )
        .await;

        self.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: warning_message.clone(),
            }),
        )
        .await;
        true
    }

    pub(crate) async fn emit_model_verification(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        verifications: Vec<ModelVerification>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ModelVerification(ModelVerificationEvent { verifications }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_moderation_metadata(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        metadata: TurnModerationMetadataEvent,
    ) {
        self.send_event(turn_context, EventMsg::TurnModerationMetadata(metadata))
            .await;
    }


    pub(crate) async fn commit_rollback_history(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        replay_items: Vec<RolloutItem>,
        rollback_msg: EventMsg,
        task_start_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> std::io::Result<()> {
        let session = Arc::clone(self);
        let turn_context = Arc::clone(turn_context);
        session
            .durable_history_commits_in_flight
            .fetch_add(1, Ordering::AcqRel);
        let in_flight = DurableHistoryCommitInFlight {
            session: Arc::clone(&session),
        };
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let _commit_permit = session.durable_history_commit_gate.acquire().await
                .map_err(|_| std::io::Error::other("durable history commit gate is closed"))?;
            let persistence = async {
                session.persist_rollout_items_ordered(&[RolloutItem::EventMsg(rollback_msg)]).await?;
                if let Some(live_thread) = session.live_thread() {
                    live_thread.flush_durable().await.map_err(std::io::Error::other)?;
                }
                Ok::<_, std::io::Error>(())
            }.await;
            if let Err(error) = persistence {
                // The marker may have reached storage despite a lost/failed
                // barrier. Fence later history commits until reopen reconciles
                // the authoritative rollout; never append another rollback.
                session.durable_history_commit_gate.close();
                return Err(std::io::Error::other(format!(
                    "rollback persistence is unconfirmed; live history was not published; reopen the thread before retrying: {error}"
                )));
            }
            // Publish only after the marker and its referenced payloads are durable.
            session.apply_rollout_reconstruction(
                turn_context.as_ref(), &replay_items, false,
            ).await;
            let token_count = session.recompute_token_usage_state(turn_context.as_ref()).await;
            // State publication is complete. A stalled event consumer must not
            // retain task admission (which shutdown also needs).
            drop(task_start_permit);
            if let Some(event) = token_count {
                session.send_token_count(turn_context.as_ref(), event).await;
            }
            Ok(())
        }).await.map_err(std::io::Error::other)?
    }

    pub(crate) async fn replace_compacted_history(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
        world_state_baseline: Option<WorldStateSnapshot>,
        fragment_digests: Vec<ContextFragmentDigest>,
        compacted_item: CompactedItem,
    ) -> CodexResult<()> {
        let session = Arc::clone(self);
        let turn_context = Arc::clone(turn_context);
        session
            .durable_history_commits_in_flight
            .fetch_add(1, Ordering::AcqRel);
        let in_flight = DurableHistoryCommitInFlight {
            session: Arc::clone(&session),
        };
        // Like ordinary ordered history commits, compaction owns its append and
        // publication even if the requesting future is dropped.
        tokio::spawn(async move {
            let _in_flight = in_flight;
            let _commit_permit = session
                .durable_history_commit_gate
                .acquire()
                .await
                .map_err(|_| {
                    CodexErr::Fatal("durable history commit gate is closed".to_string())
                })?;
            let _tool_history_io_permit = if turn_context.config.completed_tool_history_projection {
                Some(session.acquire_tool_history_io_permit().await?)
            } else {
                None
            };
            // Reserve the next window under the ordered commit gate, but leave
            // live metadata unchanged until its rollout record is durable.
            let (window_number, window_ids) = session.state.lock().await.next_auto_compact_window();
            let mut items = items;
            let application_context = session
                .state
                .lock()
                .await
                .additional_context
                .application_context_for_replacement(&items);
            // Keep the handoff last. The durable replacement and live history
            // must contain the same policy before delivery is reconciled.
            let insertion_index = items.len().saturating_sub(1);
            items.splice(insertion_index..insertion_index, application_context);
            let items = Self::assign_missing_response_item_ids(Cow::Owned(items)).into_owned();
            let compacted_item = CompactedItem {
                replacement_history: Some(items.clone()),
                window_number: Some(window_number),
                first_window_id: Some(window_ids.first_window_id.to_string()),
                previous_window_id: window_ids.previous_window_id.map(|id| id.to_string()),
                window_id: Some(window_ids.window_id.to_string()),
                ..compacted_item
            };
            let snapshot = if turn_context.config.completed_tool_history_projection {
                let mut snapshot = session.state.lock().await.tool_history_state();
                snapshot.retain_for_history(&items);
                Some(snapshot)
            } else {
                None
            };
            // acquire_tool_history_io_permit drained the queue while excluding
            // producers, so writer admission cannot block behind this I/O lease.
            let mut persistence_writer = if snapshot.is_some() {
                Some(session.tool_history_persistence.writer().await)
            } else {
                None
            };
            // The old durable ledger remains a safe superset until the rollout
            // commits. Keep the worker behind this writer; rollback the reserved
            // prune if append fails, including unwinding before publication.
            let snapshot_reservation = match (persistence_writer.as_mut(), snapshot) {
                (Some(writer), Some(snapshot)) => Some(
                    writer
                        .reserve_snapshot(snapshot, session.live_thread().cloned())
                        .map_err(|error| {
                            CodexErr::Fatal(format!(
                                "failed to reserve compacted tool history: {error}"
                            ))
                        })?,
                ),
                _ => None,
            };
            let source_state = serde_json::to_value(&session.state.lock().await.additional_context)
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to encode additional context: {error}"))
                })?;
            let repository_sources = world_state_baseline
                .as_ref()
                .and_then(|snapshot| snapshot.section("agents_md"))
                .cloned();
            session
                .persist_rollout_items_ordered(&[
                    RolloutItem::Compacted(compacted_item),
                    RolloutItem::AdditionalContextState {
                        version: 1,
                        turn_id: turn_context.sub_id.clone(),
                        state: source_state,
                        repository_sources,
                    },
                ])
                .await
                .map_err(|error| {
                    CodexErr::Fatal(format!("failed to commit compacted history: {error}"))
                })?;
            {
                let mut state = session.state.lock().await;
                state.replace_history(items, None);
                state.restore_auto_compact_window(window_number, window_ids);
                if let (Some(turn_context_item), Some(snapshot)) =
                    (reference_context_item, world_state_baseline)
                {
                    state.stage_context_baseline(ContextBaselineCandidate {
                        turn_context_item,
                        world_state_snapshot: snapshot.clone(),
                        world_state_item: Some(WorldStateItem::full(snapshot.into_value())),
                        fragment_digests,
                        bound_sampling_request_id: None,
                        bound_physical_attempt_id: None,
                    });
                } else {
                    state.clear_pending_context_baseline();
                }
                session.services.advance_planning_generation(&mut state);
                state.queue_pending_session_start_source(codex_hooks::SessionStartSource::Compact);
            }
            if let Some(reservation) = snapshot_reservation {
                reservation.commit();
            }
            // A failed later prune remains a dirty queue obligation. The old
            // persisted superset and protection markers still cover this commit.
            Ok(())
        })
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!("compacted history commit task failed: {error}"))
        })?
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> ManagedFeatures {
        self.features.clone()
    }

    pub(crate) async fn collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state.session_configuration.collaboration_mode.clone()
    }

    pub(crate) fn multi_agent_version(&self) -> Option<MultiAgentVersion> {
        self.multi_agent_version.get().copied()
    }

    pub(crate) fn set_multi_agent_version_if_unset(
        &self,
        multi_agent_version: MultiAgentVersion,
    ) -> MultiAgentVersion {
        *self.multi_agent_version.get_or_init(|| multi_agent_version.runtime_version())
    }

    pub(crate) fn resolve_multi_agent_version_for_model(
        &self,
        model_info: &ModelInfo,
        config: &Config,
    ) -> MultiAgentVersion {
        if let Some(multi_agent_version) = self.multi_agent_version() {
            return multi_agent_version;
        }

        let selected = model_info
            .multi_agent_version
            .unwrap_or_else(|| config.multi_agent_version_from_features());

        self.set_multi_agent_version_if_unset(selected)
    }

    /// Records that a consumer asked for `RawResponseItem` events on this thread.
    ///
    /// The flag is monotonic: once any subscriber opts in, the producer keeps emitting raw
    /// events for the rest of the thread's life, matching the app-server's per-thread
    /// `experimental_raw_events` contract.
    pub(crate) fn request_raw_response_items(&self) {
        self.raw_response_items_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn raw_response_items_requested(&self) -> bool {
        self.raw_response_items_requested
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn request_raw_user_turn_boundaries(&self) {
        self.raw_user_turn_boundaries_requested
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        // Raw events clone every recorded item, including full tool outputs, and occupy a
        // bounded event-channel slot. Skip construction entirely when no consumer opted in.
        let all_items = self.raw_response_items_requested();
        if !all_items
            && !self
                .raw_user_turn_boundaries_requested
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        for item in items {
            if !all_items && !codex_context_fragments::is_user_turn_boundary(item) {
                continue;
            }
            let msg = EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() });
            self.send_event_raw_with_persistence(
                Event {
                    id: turn_context.sub_id.clone(),
                    msg,
                },
                /*persist*/ false,
            )
            .await;
        }
    }

    async fn poll_thread_context_contributors(
        &self,
        estimate: bool,
    ) -> Vec<Option<Vec<PromptFragment>>> {
        // Poll contributors concurrently, but yield their fragments in registration order.
        let mut pending = FuturesOrdered::new();
        let deadline = tokio::time::Instant::now() + EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
        for (contributor_index, contributor) in self
            .services
            .extensions
            .context_contributors()
            .iter()
            .enumerate()
        {
            let contribution = if estimate {
                contributor.estimate_thread_context(
                    &self.services.session_extension_data,
                    &self.services.thread_extension_data,
                )
            } else {
                contributor.contribute_thread_context(
                    &self.services.session_extension_data,
                    &self.services.thread_extension_data,
                )
            };
            pending.push_back(async move {
                match tokio::time::timeout_at(deadline, contribution).await {
                    Ok(fragments) if fragments.iter().any(PromptFragment::is_unavailable) => None,
                    Ok(fragments) => Some(fragments),
                    Err(_) => {
                        warn!(
                            contributor_index,
                            scope = "thread",
                            timeout = ?EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT,
                            "extension context contributor timed out; omitting its fragments"
                        );
                        None
                    }
                }
            });
        }

        let mut fragments = Vec::new();
        while let Some(contributed_fragments) = pending.next().await {
            fragments.push(contributed_fragments);
        }
        fragments
    }

    async fn poll_turn_context_contributors(
        &self,
        turn_context: &TurnContext,
        estimate: bool,
    ) -> Vec<Option<Vec<PromptFragment>>> {
        // Poll contributors concurrently, but yield their fragments in registration order.
        let mut pending = FuturesOrdered::new();
        let deadline = tokio::time::Instant::now() + EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT;
        for (contributor_index, contributor) in self
            .services
            .extensions
            .context_contributors()
            .iter()
            .enumerate()
        {
            let input = TurnContextContributionInput {
                thread_id: self.thread_id(),
                turn_id: turn_context.sub_id.as_str(),
                session_store: &self.services.session_extension_data,
                thread_store: &self.services.thread_extension_data,
                turn_store: turn_context.extension_data.as_ref(),
                model_context_window: turn_context.model_context_window(),
            };
            let contribution = if estimate {
                contributor.estimate_turn_context(input)
            } else {
                contributor.contribute_turn_context(input)
            };
            pending.push_back(async move {
                match tokio::time::timeout_at(deadline, contribution).await {
                    Ok(fragments) if fragments.iter().any(PromptFragment::is_unavailable) => None,
                    Ok(fragments) => Some(fragments),
                    Err(_) => {
                        warn!(
                            contributor_index,
                            scope = "turn",
                            timeout = ?EXTENSION_CONTEXT_CONTRIBUTOR_TIMEOUT,
                            "extension context contributor timed out; omitting its fragments"
                        );
                        None
                    }
                }
            });
        }

        let mut fragments = Vec::new();
        while let Some(contributed_fragments) = pending.next().await {
            fragments.push(contributed_fragments);
        }
        fragments
    }


    fn build_context_contribution_items_from_rendered_fragments(
        rendered_fragments: Vec<(PromptSlot, String)>,
    ) -> Vec<ResponseItem> {
        let mut developer_sections = Vec::new();
        let mut contextual_user_sections = Vec::new();
        let mut separate_developer_sections = Vec::new();
        for (slot, text) in rendered_fragments {
            push_rendered_prompt_fragment(
                slot,
                text,
                &mut developer_sections,
                &mut contextual_user_sections,
                &mut separate_developer_sections,
            );
        }

        let mut items = Vec::with_capacity(3);
        if let Some(developer_message) =
            crate::context_manager::updates::build_developer_update_item(developer_sections)
        {
            items.push(developer_message);
        }
        for section in separate_developer_sections {
            if let Some(developer_message) =
                crate::context_manager::updates::build_developer_update_item(vec![section])
            {
                items.push(developer_message);
            }
        }
        if let Some(contextual_user_message) =
            crate::context_manager::updates::build_contextual_user_message(contextual_user_sections)
        {
            items.push(contextual_user_message);
        }
        items
    }



    pub(crate) async fn build_initial_context_with_world_state_and_provenance(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
    ) -> (
        Vec<ResponseItem>,
        WorldStateSnapshot,
        Vec<ContextFragmentDigest>,
    ) {
        let built = self
            .build_initial_context_with_world_state_and_mcp(turn_context, world_state, false)
            .await;
        (
            built.items,
            built.world_state_snapshot,
            built.fragment_digests,
        )
    }

    async fn build_initial_context_with_world_state_and_mcp(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
        estimate: bool,
    ) -> InitialContextBuild {
        self.build_context_candidate(turn_context, world_state, estimate, None, None)
            .await
    }

    async fn build_context_candidate(
        &self,
        turn_context: &TurnContext,
        world_state: &WorldState,
        estimate: bool,
        represented_context: Option<&TurnContextItem>,
        represented_digests: Option<&[ContextFragmentDigest]>,
    ) -> InitialContextBuild {
        let mut developer_sections = Vec::<String>::with_capacity(8);
        let mut contextual_user_sections = Vec::<String>::with_capacity(2);
        let separate_developer_sections = Vec::<String>::new();
        // Settings and world state have typed baselines and dedicated diff protocols. Fragment
        // receipts intentionally cover only unstructured prompt material that cannot otherwise be
        // compared safely.
        let mut stable_developer_sections = Vec::<String>::with_capacity(4);
        let mut stable_contextual_user_sections = Vec::<String>::with_capacity(2);
        let stable_separate_developer_sections = Vec::<String>::new();
        let (previous_turn_settings, session_source, retractable) = {
            let state = self.state.lock().await;
            (
                state.previous_turn_settings(),
                state.session_configuration.session_source.clone(),
                crate::stable_context::retractable_markers(state.history.raw_items()),
            )
        };
        if let Some(model_switch_message) =
            crate::context_manager::updates::build_model_switch_update_item(
                previous_turn_settings.as_ref(),
                turn_context,
            )
        {
            developer_sections.push(model_switch_message);
        }
        let permissions_index = developer_sections.len();

        let projected_instructions = turn_context
            .developer_instructions
            .as_deref()
            .map(crate::context::desktop_instructions::project);
        let configured_developer_instructions =
            crate::stable_context::configured_developer_instructions_sections(
                projected_instructions
                    .as_deref()
                    .filter(|instructions| !instructions.is_empty()),
            );

        // A lone removal marker is injected only when history still shows the slot
        // present; otherwise it retracts nothing. Fragment digests still include
        // it, which keeps change detection on later turns exact.
        if !crate::stable_context::is_redundant_retraction(
            &configured_developer_instructions,
            &retractable,
        ) {
            developer_sections.extend(configured_developer_instructions.iter().cloned());
        }
        stable_developer_sections.extend(configured_developer_instructions.iter().cloned());

        // Use the captured turn, not settings published for a future turn. A full
        // refresh appends to existing history, so it must also retract cleared slots.
        if turn_context.config.include_collaboration_mode_instructions
            && let Some(collab_instructions) =
                CollaborationModeInstructions::from_collaboration_mode(
                    &turn_context.collaboration_mode,
                )
                .map(|instructions| instructions.render())
                .or_else(|| {
                    crate::context_manager::updates::build_collaboration_mode_update_item(
                        represented_context,
                        turn_context,
                    )
                })
        {
            developer_sections.push(collab_instructions);
        }
        // No root-orchestration developer block: every clause it carried is
        // already stated once by the base instructions (batching and dependency
        // sequencing, shared-state serialization, resuming yielded commands,
        // caller updates without unrelated refactors, validation reuse, scoped
        // clippy) or by the exec tool description that the model reads while
        // authoring the call. A second, differently worded copy at a different
        // authority level is work the model has to redo every generation.
        if self.features.enabled(Feature::Personality) {
            let rendered_personality = turn_context
                .personality
                .and_then(|personality| {
                    crate::context_manager::updates::personality_message_for(
                        &turn_context.model_info,
                        personality,
                    )
                })
                .map(|message| PersonalitySpecInstructions::new(message).render());
            // Catalog text can change under the same model slug and personality.
            // Track effective text, not a one-time reset, in the accepted receipt.
            stable_developer_sections.extend(rendered_personality.iter().cloned());
            if let Some(personality_message) = rendered_personality.or_else(|| {
                crate::context_manager::updates::build_personality_update_item(
                    represented_context,
                    turn_context,
                    true,
                )
            }) {
                developer_sections.push(personality_message);
            }
        }
        let mut skill_catalog_present = false;
        // An empty host catalog does not mean executor or orchestrator skills
        // are unavailable. Shared usage guidance follows the model/config gates.
        let skill_usage_present = turn_context.config.include_skill_instructions
            && turn_context.model_info.include_skills_usage_instructions;
        if skill_usage_present {
            let skills_usage = SkillsUsageInstructions.render();
            developer_sections.push(skills_usage.clone());
            stable_developer_sections.push(skills_usage);
        }
        let mut skill_source_unavailable = turn_context.config.include_skill_instructions
            && !turn_context
                .turn_skills
                .snapshot
                .outcome()
                .errors
                .is_empty();
        if turn_context.config.include_skill_instructions {
            let available_skills = turn_context.turn_skills.snapshot.available_skills(
                default_skill_metadata_budget(turn_context.model_info.context_window),
                if estimate {
                    SkillRenderSideEffects::None
                } else {
                    SkillRenderSideEffects::ThreadStart {
                        session_telemetry: &self.services.session_telemetry,
                    }
                },
            );
            if available_skills.is_none()
                && !turn_context
                    .turn_skills
                    .snapshot
                    .outcome()
                    .allowed_skills_for_implicit_invocation()
                    .is_empty()
            {
                // A rendering-budget omission is not authoritative source removal.
                skill_source_unavailable = true;
            }
            if let Some(available_skills) = available_skills {
                skill_catalog_present = true;
                let warning_message = available_skills.warning_message.clone();
                let skills_instructions =
                    AvailableSkillsInstructions::from_available_skills(&available_skills);
                if !estimate && let Some(warning_message) = warning_message {
                    self.send_event_raw(Event {
                        id: String::new(),
                        msg: EventMsg::Warning(WarningEvent {
                            message: warning_message,
                        }),
                    })
                    .await;
                }
                let skills_instructions = skills_instructions.render();
                contextual_user_sections.push(skills_instructions.clone());
                stable_contextual_user_sections.push(skills_instructions);
            }
        }
        let mut extension_context_budget = ModelContextBudget::default();
        for (present, unavailable, removal) in [
            (
                skill_catalog_present,
                skill_source_unavailable,
                crate::stable_context::SKILL_CATALOG_REMOVED,
            ),
            (
                skill_usage_present,
                false,
                crate::stable_context::SKILL_USAGE_REMOVED,
            ),
        ] {
            if !present && !unavailable {
                stable_developer_sections.push(removal.to_string());
                if retractable.contains(&removal) {
                    developer_sections.push(removal.to_string());
                }
            }
        }
        let mut contributions = Vec::new();
        let mut unavailable_items = Vec::new();
        let retained_history = self.clone_history().await;
        let disabled_builtin_items = crate::stable_context::disabled_builtin_retractions(
            retained_history.raw_items(),
            turn_context.config.include_permissions_instructions,
            turn_context.config.include_collaboration_mode_instructions,
            turn_context.config.include_environment_context,
            self.features.enabled(Feature::Personality),
        );
        let fallback_context = if represented_digests.is_none() {
            let state = self.state.lock().await;
            state
                .pending_context_baseline()
                .map(|candidate| candidate.turn_context_item)
                .or_else(|| state.reference_context_item())
        } else {
            None
        };
        let retention_digests = represented_digests.or_else(|| {
            fallback_context
                .as_ref()
                .and_then(|context| context.context_provenance.as_ref())
                .map(|provenance| provenance.fragment_digests.as_slice())
        });
        let mut retained_contributions = Vec::new();
        let (thread, turn) = if estimate {
            // Estimates are read-only by contract; poll both deadlines together.
            // Contribution methods can share mutable extension state and retain
            // their historical thread-before-turn ordering below.
            tokio::join!(
                self.poll_thread_context_contributors(true),
                self.poll_turn_context_contributors(turn_context, true),
            )
        } else {
            let thread = self.poll_thread_context_contributors(false).await;
            let turn = self.poll_turn_context_contributors(turn_context, false).await;
            (thread, turn)
        };
        for (scope, batches) in [("thread", thread), ("turn", turn)] {
            for (producer, batch) in batches.into_iter().enumerate() {
                let prefix = format!("turn-contributor:{scope}:{producer}:");
                let Some(batch) = batch else {
                    let retained_before = retained_contributions.len();
                    for digest in retention_digests
                        .into_iter()
                        .flatten()
                        .filter(|digest| digest.key.starts_with(&prefix))
                    {
                        let Some(index) = digest
                            .key
                            .rsplit(':')
                            .next()
                            .and_then(|index| index.parse::<usize>().ok())
                        else {
                            continue;
                        };
                        let marker = format!("<turn_context_contribution index=\"{index}\">");
                        let retained = retained_history
                            .raw_items()
                            .iter()
                            .rev()
                            .filter(|item| {
                                crate::stable_context::is_trusted_stable_context_item(item)
                                    && (scope == "thread"
                                        || item.turn_id() == Some(turn_context.sub_id.as_str()))
                            })
                            .find_map(|item| {
                                let ResponseItem::Message { role, content, .. } = item else {
                                    return None;
                                };
                                let slot = match role.as_str() {
                                    "developer" => PromptSlot::SeparateDeveloper,
                                    "user" => PromptSlot::ContextualUser,
                                    _ => return None,
                                };
                                content.iter().find_map(|part| match part {
                                    codex_protocol::models::ContentItem::InputText { text }
                                        if text.starts_with(&marker)
                                            && digest_context_fragments(
                                                &Self::build_context_contribution_items_from_rendered_fragments(
                                                    vec![(slot, text.clone())],
                                                ),
                                            )[0].digest == digest.digest =>
                                    {
                                        Some((slot, text.clone()))
                                    }
                                    _ => None,
                                })
                            });
                        let (slot, text) = retained.unwrap_or_else(|| (PromptSlot::SeparateDeveloper,
                            crate::stable_context::turn_contribution_text(index,
                                "Source temporarily unavailable and its prior body is no longer retained. This is not successful deletion; do not infer the missing instructions.")));
                        retained_contributions.push((digest.key.clone(), slot, text));
                    }
                    if retained_contributions.len() == retained_before {
                        unavailable_items.push((PromptSlot::SeparateDeveloper, format!(
                            "Context contributor {scope}:{producer} is temporarily unavailable, not authoritatively empty. No accepted same-scope contribution is retained."
                        )));
                    }
                    continue;
                };
                for (fragment_index, fragment) in batch.into_iter().enumerate() {
                    // Pair registration and source-local positions before admission.
                    let index =
                        context_contribution_index(producer, fragment_index, scope == "thread");
                    contributions.push((format!("{prefix}{index}"), index, fragment));
                }
            }
        }
        contributions
            .sort_by_key(|(_, _, fragment)| fragment.slot() != PromptSlot::DeveloperPolicy);
        for (_, _, text) in &retained_contributions {
            if !extension_context_budget.try_take(text) {
                extension_context_budget = ModelContextBudget::new(0);
            }
        }
        let mut rendered_turn_context_fragments = retained_contributions;
        for (key, index, fragment) in contributions {
            let policy = fragment.slot() == PromptSlot::DeveloperPolicy;
            let admitted = take_prompt_fragment_with_identity(
                fragment.clone(),
                &mut extension_context_budget,
                &turn_context.sub_id,
                Some(index),
            );
            let admitted = if admitted.is_none()
                && (policy
                    || retention_digests
                        .into_iter()
                        .flatten()
                        .any(|digest| digest.key == key))
            {
                let recovery = if estimate {
                    "Exact recovery is established at admission.".to_string()
                } else {
                    match self.retain_context_source(if policy { "extension_policy" } else { "extension_context" }, serde_json::json!([
                        {"role":if fragment.slot() == PromptSlot::ContextualUser { "user" } else { "developer" },
                         "producer":key, "text":fragment.text()}
                    ])).await {
                        Ok(recovery) => recovery.to_string(),
                        Err(error) => format!("Recovery unavailable: {error}"),
                    }
                };
                let notice = if policy {
                    format!(
                        "Required policy INCOMPLETE: no policy excerpt was admitted. Do not proceed with actions requiring it until recovered in full. {recovery}"
                    )
                } else {
                    format!(
                        "Context contribution INCOMPLETE: the producer still returned this source, but its body exceeded the admission budget. This is not a source removal. Recover it before relying on its contents. {recovery}"
                    )
                };
                Some((
                    if fragment.slot() == PromptSlot::ContextualUser {
                        PromptSlot::ContextualUser
                    } else {
                        PromptSlot::SeparateDeveloper
                    },
                    crate::stable_context::turn_contribution_text(index, &notice),
                ))
            } else {
                admitted
            };
            let Some((slot, text)) = admitted else {
                continue;
            };
            rendered_turn_context_fragments.push((key, slot, text));
        }
        // Opaque extension contributions never share a message with typed stable
        // permissions/capabilities. Keep conservative stable classification intact.
        let multi_agent_v2_usage_hint_text = (turn::agent_surface_stage(self, turn_context)
            != crate::tools::exposure::AgentSurfaceStage::Prohibited)
            .then(|| multi_agents::usage_hint_text(turn_context, &session_source))
            .flatten();
        let multi_agent_v2_usage_hint_sections =
            (turn_context.multi_agent_version == MultiAgentVersion::V2).then(|| {
                crate::stable_context::multi_agent_usage_hint_sections(
                    multi_agent_v2_usage_hint_text,
                )
            });

        let mut stable_items = Vec::new();
        stable_items.extend(
            Self::build_context_contribution_items_from_rendered_fragments(
                unavailable_items.clone(),
            ),
        );
        if let Some(message) =
            crate::context_manager::updates::build_developer_update_item(stable_developer_sections)
        {
            stable_items.push(message);
        }
        stable_items.extend(
            stable_separate_developer_sections
                .into_iter()
                .filter_map(|section| {
                    crate::context_manager::updates::build_developer_update_item(vec![section])
                }),
        );

        if let Some(usage_hint_sections) = multi_agent_v2_usage_hint_sections.clone()
            && let Some(usage_hint_message) =
                crate::context_manager::updates::build_developer_update_item(usage_hint_sections)
        {
            stable_items.push(usage_hint_message);
        }
        if let Some(message) = crate::context_manager::updates::build_contextual_user_message(
            stable_contextual_user_sections,
        ) {
            stable_items.push(message);
        }
        // Model catalog approval wording can change without a model slug/settings
        // change. The typed TurnContextItem cannot reconstruct that prior text, so
        // retain the rendered permission identity in the existing fragment receipt.
        let rendered_permissions =
            turn_context
                .config
                .include_permissions_instructions
                .then(|| {
                    PermissionsInstructions::from_permission_profile(
                        &turn_context.permission_profile,
                        turn_context.approval_policy.value(),
                        ApprovalPromptContext::new(
                            turn_context
                                .model_info
                                .model_messages
                                .as_ref()
                                .and_then(|messages| messages.approvals.as_ref()),
                        ),
                        self.services.exec_policy.current().as_ref(),
                        turn_context.cwd(),
                        turn_context
                            .config
                            .features
                            .enabled(Feature::ExecPermissionApprovals),
                        turn_context
                            .config
                            .features
                            .enabled(Feature::RequestPermissionsTool),
                    )
                    .render()
                });
        let mut fragment_digests = digest_context_fragments(&stable_items);
        fragment_digests.push(ContextFragmentDigest {
            key: "builtin-inclusion".into(),
            digest: hex::encode(Sha256::digest([
                u8::from(turn_context.config.include_permissions_instructions),
                u8::from(turn_context.config.include_collaboration_mode_instructions),
                u8::from(turn_context.config.include_environment_context),
            ])),
        });
        if let Some(permissions) = rendered_permissions.as_ref()
            && let Some(item) = crate::context_manager::updates::build_developer_update_item(vec![
                permissions.clone(),
            ])
        {
            let mut digest = digest_context_fragments(&[item]).remove(0);
            digest.key = "rendered-permissions".to_string();
            fragment_digests.push(digest);
        }
        fragment_digests.extend(
            rendered_turn_context_fragments
                .iter()
                .map(|(key, slot, text)| {
                    let item = Self::build_context_contribution_items_from_rendered_fragments(
                        vec![(*slot, text.clone())],
                    );
                    let mut digest = digest_context_fragments(&item).remove(0);
                    digest.key = key.clone();
                    digest
                }),
        );
        // A timeout is not an authoritative empty snapshot. Keep the exact
        // earlier source identities so it cannot revoke or rename its neighbors.
        // Only exact same-scope retained bodies count as delivered; a digest alone
        // must never assert that an absent instruction survived replacement.
        fragment_digests.sort_by(|a, b| a.key.cmp(&b.key));
        let mut turn_context_items = Self::build_context_contribution_items_from_rendered_fragments(
            rendered_turn_context_fragments
                .into_iter()
                .map(|(_, slot, text)| (slot, text))
                .chain(unavailable_items)
                .collect(),
        );
        if let Some(retractions) = crate::context_manager::updates::build_developer_update_item(
            crate::stable_context::turn_contribution_authority_retractions(
                retained_history.raw_items(),
                &turn_context_items,
            ),
        ) {
            // Keep the replacement last for latest-value projection while the
            // old developer authority explicitly releases its prior policy.
            turn_context_items.insert(0, retractions);
        }
        let full_context_required = !disabled_builtin_items.is_empty()
            || represented_context.is_none()
            || represented_digests.is_some_and(|represented| {
                !represented
                    .iter()
                    .filter(|digest| !digest.key.starts_with("turn-contributor:"))
                    .eq(fragment_digests
                        .iter()
                        .filter(|digest| !digest.key.starts_with("turn-contributor:")))
            });
        if !full_context_required {
            // Typed settings and world state have their own delta paths. Poll live
            // contributors above, but avoid rendering a full candidate that those
            // paths would immediately discard. The snapshot is only used for full
            // candidates; the delta path derives its accepted snapshot from history.
            return InitialContextBuild {
                full_context_required,
                items: Vec::new(),
                turn_context_items,
                world_state_snapshot: WorldStateSnapshot::default(),
                fragment_digests,
            };
        }
        if let Some(permissions) = rendered_permissions {
            developer_sections.insert(permissions_index, permissions);
        }

        let (world_state_fragments, delivered_world_state_snapshot) =
            world_state.render_full_with_snapshot();
        for fragment in world_state_fragments {
            let item = fragment.into_boxed_response_item();
            if let ResponseItem::Message { role, content, .. } = item {
                for part in content {
                    if let codex_protocol::models::ContentItem::InputText { text } = part {
                        match role.as_str() {
                            "developer" => developer_sections.push(text),
                            "user" => contextual_user_sections.push(text),
                            _ => {}
                        }
                    }
                }
            }
        }

        let mut items = Vec::with_capacity(4);
        if let Some(developer_message) =
            crate::context_manager::updates::build_developer_update_item(developer_sections)
        {
            items.push(developer_message);
        }
        for section in separate_developer_sections {
            if let Some(developer_message) =
                crate::context_manager::updates::build_developer_update_item(vec![section])
            {
                items.push(developer_message);
            }
        }
        if let Some(usage_hint_sections) = multi_agent_v2_usage_hint_sections
            && !crate::stable_context::is_redundant_retraction(&usage_hint_sections, &retractable)
            && let Some(usage_hint_message) =
                crate::context_manager::updates::build_developer_update_item(usage_hint_sections)
        {
            items.push(usage_hint_message);
        }
        if let Some(multi_agent_mode) = multi_agents::effective_multi_agent_mode(turn_context) {
            items.push(ContextualUserFragment::into(
                MultiAgentModeInstructions::new(multi_agent_mode),
            ));
        }
        if let Some(contextual_user_message) =
            crate::context_manager::updates::build_contextual_user_message(contextual_user_sections)
        {
            items.push(contextual_user_message);
        }

        // Compaction installs these items directly into replacement history.
        items.extend(disabled_builtin_items);
        items.extend(turn_context_items.iter().cloned());
        for item in &mut items {
            crate::stable_context::mark_trusted_stable_context_item(item);
            item.set_turn_id_if_missing(&turn_context.sub_id);
        }
        InitialContextBuild {
            full_context_required,
            items,
            turn_context_items,
            world_state_snapshot: delivered_world_state_snapshot,
            fragment_digests,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(item_count = items.len()))]
    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        if let Some(live_thread) = self.live_thread() {
            // Batch ordinary turn records behind the terminal durability barrier. This preserves
            // stream order while preventing each event from forcing disk work onto the next
            // request-preparation path.
            let result = live_thread.append_items_ordered(items).await;
            if let Err(e) = result {
                error!("failed to record rollout items: {e:#}");
            }
        }
    }

    /// Append rollout items and wait for the configured persistence barrier. Unlike the
    /// best-effort event path, an enabled writer must fail closed. Ephemeral sessions
    /// deliberately have no writer: accepting their in-memory history never creates a rollout.
    pub(crate) async fn persist_rollout_items_durable(
        &self,
        items: &[RolloutItem],
    ) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items_ordered(items)
                .await
                .map_err(std::io::Error::other)?;
            // Settings persistence may hold the session-state lock. Flush this
            // event's writer directly; the terminal barrier syncs tool artifacts.
            live_thread.flush().await.map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    /// Append rollout items through the authoritative writer without forcing an fsync-style
    /// barrier. The terminal finalizer owns the single durability barrier for the turn.
    pub(crate) async fn persist_rollout_items_ordered(
        &self,
        items: &[RolloutItem],
    ) -> std::io::Result<()> {
        if let Some(live_thread) = self.live_thread() {
            live_thread
                .append_items_ordered(items)
                .await
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    pub(crate) async fn bind_context_baseline_candidate(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
    ) -> (bool, bool) {
        let mut state = self.state.lock().await;
        let Some(candidate) = state.pending_context_baseline_mut() else {
            // An unchanged prompt reuses the accepted baseline; it does not
            // introduce an unresolved candidate that should clear it on replay.
            return (false, !state.has_reference_context_item());
        };
        candidate.bound_sampling_request_id = Some(sampling_request_id.to_string());
        candidate.bound_physical_attempt_id = Some(physical_attempt_id.to_string());
        (true, true)
    }

    pub(crate) async fn commit_context_baseline_candidate(
        &self,
        sampling_request_id: &str,
        physical_attempt_id: &str,
    ) -> std::io::Result<bool> {
        let (mut authoritative, world_state_snapshot, world_state_item, fragment_digests) = {
            let state = self.state.lock().await;
            let Some(candidate) = state.pending_context_baseline_ref() else {
                return Ok(false);
            };
            if candidate.bound_sampling_request_id.as_deref() != Some(sampling_request_id)
                || candidate.bound_physical_attempt_id.as_deref() != Some(physical_attempt_id)
            {
                return Ok(false);
            }
            (
                candidate.turn_context_item.clone(),
                candidate.world_state_snapshot.clone(),
                candidate.world_state_item.clone(),
                candidate.fragment_digests.clone(),
            )
        };

        authoritative.context_provenance = Some(TurnContextProvenance {
            accepted_attempt: AcceptedAttemptProvenance {
                sampling_request_id: sampling_request_id.to_string(),
                physical_attempt_id: physical_attempt_id.to_string(),
            },
            fragment_digests,
        });
        let mut rollout_items = Vec::with_capacity(2);
        if let Some(world_state_item) = world_state_item {
            rollout_items.push(RolloutItem::WorldState(world_state_item));
        }
        rollout_items.push(RolloutItem::TurnContext(authoritative.clone()));
        // The accepted context must be ordered before any later response item, but it does not
        // need its own durability barrier. Terminal finalization flushes the complete ordered
        // turn prefix before delivery, avoiding one fsync-style stall on the first response event
        // of every model generation.
        self.persist_rollout_items_ordered(&rollout_items).await?;

        let mut state = self.state.lock().await;
        state.history.set_world_state_baseline(world_state_snapshot);
        state.set_reference_context_item(Some(authoritative));
        if state
            .pending_context_baseline_ref()
            .is_some_and(|live_candidate| {
                live_candidate.bound_sampling_request_id.as_deref() == Some(sampling_request_id)
                    && live_candidate.bound_physical_attempt_id.as_deref()
                        == Some(physical_attempt_id)
            })
        {
            state.clear_pending_context_baseline();
        }
        Ok(true)
    }

    pub(crate) async fn mark_context_baseline_unknown(&self) {
        let mut state = self.state.lock().await;
        state.history.mark_realized_context_unknown();
        state.clear_pending_context_baseline();
    }



    pub(crate) async fn clone_history(&self) -> ContextManager {
        self.state.lock().await.clone_history()
    }

    /// Scope retirement alone needs input provenance. Hold the existing history
    /// gate through the plan commit so steering/rollback cannot replace the
    /// authorization boundary between checking it and publishing the revision.
    pub(crate) async fn plan_scope_inputs(
        &self,
    ) -> std::io::Result<(tokio::sync::SemaphorePermit<'_>, crate::plan_store::PlanScopeInputs)> {
        let guard = self.durable_history_commit_gate.acquire().await
            .map_err(|_| std::io::Error::other("history commit gate is closed"))?;
        let inputs = crate::plan_store::PlanScopeInputs::from_history(
            self.state.lock().await.history.raw_items(),
        );
        Ok((guard, inputs))
    }

    pub(crate) async fn reserved_tool_call_ids(&self) -> std::collections::HashSet<String> {
        self.state.lock().await.history.reserved_tool_call_ids()
    }

    pub(crate) async fn dedupe_existing_developer_contexts(
        &self,
        candidates: Vec<ResponseItem>,
    ) -> Vec<ResponseItem> {
        let state = self.state.lock().await;
        crate::hook_runtime::dedupe_existing_developer_contexts(
            state.history.raw_items(),
            candidates,
        )
    }

    async fn acquire_tool_history_io_permit(
        &self,
    ) -> CodexResult<(
        tokio::sync::SemaphorePermit<'_>,
        tokio::sync::SemaphorePermit<'_>,
    )> {
        let reconciliation_permit = self
            .tool_history_reconciliation_gate
            .acquire()
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "completed-tool history reconciliation gate closed: {error}"
                ))
            })?;
        self.tool_history_persistence
            .drain()
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("completed-tool history is not durable: {error}"))
            })?;
        let io_permit = self.tool_history_io_gate.acquire().await.map_err(|error| {
            CodexErr::Fatal(format!("completed-tool history I/O gate closed: {error}"))
        })?;
        Ok((reconciliation_permit, io_permit))
    }

    pub(crate) fn check_tool_history_persistence(&self) -> CodexResult<()> {
        self.tool_history_persistence
            .check_failure()
            .map_err(|error| {
                CodexErr::Fatal(format!("completed-tool history is not durable: {error}"))
            })
    }

    pub(crate) async fn finalize_initial_tool_history_state(
        &self,
        codex_home: &std::path::Path,
        fork_source_thread_id: Option<ThreadId>,
    ) -> CodexResult<()> {
        let _tool_history_io_permit = self.acquire_tool_history_io_permit().await?;
        let mut snapshot = self.state.lock().await.tool_history_state();
        if let Some(source_thread_id) = fork_source_thread_id {
            let (reminted, dropped_candidates) =
                crate::tool_history::remint_tool_history_state_for_fork(
                    codex_home,
                    &source_thread_id.to_string(),
                    &self.thread_id.to_string(),
                    snapshot,
                )
                .await;
            snapshot = reminted;
            if dropped_candidates != 0 {
                tracing::warn!(
                    dropped_candidates,
                    source_thread_id = %source_thread_id,
                    target_thread_id = %self.thread_id,
                    "forked completed-tool history fell back to bounded canonical outputs"
                );
            }
        }
        // Add/validate the new protections before persistence, but never release
        // prior protections until the replacement ledger has committed.
        let reconciled = crate::tool_history::prepare_tool_history_state(
            codex_home,
            &self.thread_id.to_string(),
            snapshot,
        )
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "completed-tool evidence could not be verified; original history retained: {error}"
            ))
        })?;
        crate::tool_history::persist_tool_history_state(
            codex_home,
            &self.thread_id.to_string(),
            &reconciled,
        )
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "failed to persist initial completed-tool history: {error}"
            ))
        })?;
        self.state
            .lock()
            .await
            .set_tool_history_state(reconciled.clone());
        self.tool_history_persistence
            .replace_mirror(reconciled.clone())
            .await;
        crate::tools::command_output_artifact::prune_active_tool_history_artifact_protection(
            codex_home,
            &self.thread_id.to_string(),
            &reconciled.artifact_references(),
        )
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!(
                "failed to finalize initial tool-history artifact cleanup: {error}"
            ))
        })?;
        Ok(())
    }

    pub(crate) async fn register_tool_artifact_origin(
        &self,
        artifact_id: String,
        call_id: String,
        bytes: u64,
        sha256: String,
    ) {
        if let Err(error) = self
            .try_register_tool_artifact_origin(artifact_id, call_id, bytes, sha256)
            .await
        {
            tracing::warn!(%error, "failed to register artifact provenance");
        }
    }

    /// Required recovery handles must not be published if protection or owner
    /// registration fails. Best-effort tool provenance uses the wrapper above.
    pub(crate) async fn try_register_tool_artifact_origin(
        &self,
        artifact_id: String,
        call_id: String,
        bytes: u64,
        sha256: String,
    ) -> CodexResult<()> {
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        crate::tools::command_output_artifact::protect_active_tool_history_artifact(
            self.codex_home().await.as_path(),
            &self.thread_id.to_string(),
            &artifact_id,
            bytes,
            &sha256,
        )
        .await
        .map_err(|error| {
            CodexErr::Fatal(format!("failed to protect artifact provenance: {error}"))
        })?;
        self.register_protected_tool_artifact_origin(artifact_id, call_id, bytes, sha256)
            .await
    }

    /// Create and register an immutable source without rehashing newly synced bytes.
    /// The creation owner protects its newly synced bytes in the same retention
    /// transaction; only inherited/attached artifacts need a second digest check.
    pub(crate) async fn create_source_snapshot_artifact(
        self: &Arc<Self>,
        call_id: String,
        canonical: Arc<codex_tools::CanonicalToolResult>,
        backing: Option<String>,
    ) -> crate::tools::command_output_artifact::CanonicalOutputArtifact {
        // The native creation owner detaches only after bounded stage admission.
        // Its completion keeps metadata, not the full source, through registration.
        let session = Arc::clone(self);
        let bytes = canonical.exact_bytes;
        let sha256 = canonical.sha256.clone();
        crate::tools::command_output_artifact::create_source_snapshot_artifact_with_completion(
            self.codex_home().await.as_path(), &self.thread_id.to_string(), &canonical, backing,
            move |artifact| async move {
                session.register_created_source_snapshot(call_id, bytes, sha256, artifact).await
            },
        ).await
    }

    async fn register_created_source_snapshot(
        &self,
        call_id: String,
        bytes: u64,
        sha256: String,
        mut artifact: crate::tools::command_output_artifact::CanonicalOutputArtifact,
    ) -> crate::tools::command_output_artifact::CanonicalOutputArtifact {
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        if artifact.complete
            && let Some(id) = artifact.artifact_id()
        {
            let home = self.codex_home().await;
            // Reconciliation may have removed the unregistered marker while
            // staging ran. Fall back to strict protection in that uncommon case;
            // never serialize unrelated source staging behind this session gate.
            let protection = crate::tools::command_output_artifact::created_source_snapshot_is_protected(
                &home, &self.thread_id.to_string(), &id,
            ).await;
            let protected = match protection {
                Ok(true) => Ok(()),
                Ok(false) => crate::tools::command_output_artifact::protect_active_tool_history_artifact(
                    &home, &self.thread_id.to_string(), &id, bytes, &sha256,
                ).await,
                Err(error) => Err(error),
            };
            let registration = match protected {
                Ok(()) => self.register_protected_tool_artifact_origin(
                    id, call_id, bytes, sha256,
                ).await,
                Err(error) => Err(CodexErr::Fatal(error)),
            };
            if let Err(error) = registration {
                artifact.complete = false;
                artifact.error = Some(error.to_string());
            }
        }
        artifact
    }

    // Caller holds tool_history_reconciliation_gate and owns verified protection.
    async fn register_protected_tool_artifact_origin(
        &self,
        artifact_id: String,
        call_id: String,
        bytes: u64,
        sha256: String,
    ) -> CodexResult<()> {
        let mutation = crate::tool_history::ToolHistoryMutation::RegisterArtifactOrigin {
            artifact_id,
            call_id,
            bytes,
            sha256,
        };
        let mut writer = self.tool_history_persistence.writer().await;
        writer
            .enqueue_mutation(mutation.clone(), "internal artifact provenance")
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to enqueue internal artifact provenance: {error}"
                ))
            })?;
        self.state
            .lock()
            .await
            .apply_tool_history_mutation(&mutation);
        Ok(())
    }

    /// Retain exact host-owned context through the existing artifact lifecycle.
    /// A recovery read does not change the original source's authority.
    pub(crate) async fn retain_context_source(
        &self,
        source: &str,
        items: serde_json::Value,
    ) -> CodexResult<serde_json::Value> {
        let canonical = codex_tools::CanonicalToolResult::json(serde_json::json!({
            "kind": "context_source", "source": source, "items": items,
            "instruction": "Exact historical source. Preserve each entry's original role/authority; recovery is not current verification."
        }));
        let artifact_id = if let Some(id) = self
            .reusable_tool_artifact(
                &format!("context:{source}"),
                canonical.exact_bytes,
                &canonical.sha256,
            )
            .await
        {
            id
        } else {
            let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(
                self.codex_home().await.as_path(),
                &self.thread_id().to_string(),
                &canonical,
            )
            .await;
            if !artifact.complete {
                let reason = artifact
                    .error
                    .as_deref()
                    .unwrap_or("artifact retention byte budget exhausted");
                return Err(CodexErr::Fatal(format!(
                    "Could not retain complete {source}; context was not admitted. Retained {} of {} bytes: {reason}",
                    artifact.retained_bytes, canonical.exact_bytes,
                )));
            }
            let id = artifact
                .artifact_id()
                .ok_or_else(|| CodexErr::Fatal(format!("Missing {source} recovery artifact")))?;
            self.try_register_tool_artifact_origin(
                id.clone(),
                format!("context:{source}"),
                canonical.exact_bytes,
                canonical.sha256.clone(),
            )
            .await?;
            id
        };
        Ok(serde_json::json!({
            "artifact_id": artifact_id, "bytes": canonical.exact_bytes, "sha256": canonical.sha256,
            "recovery_tool": "read_tool_output", "selector": {"kind":"json_pointer", "pointer":"/items"}
        }))
    }

    pub(crate) async fn compaction_artifact_pins(
        &self,
        state: &crate::tool_history::ToolHistoryState,
        items: &[ResponseItem],
    ) -> CodexResult<Option<String>> {
        let Some(prepared) = state.prepare_artifact_pin_payload_for_items(items) else {
            return Ok(None);
        };
        let mut payload = prepared.payload;
        let mut rendered_candidates = prepared.rendered_candidates;
        if payload["omitted_artifact_count"].as_u64().unwrap_or(0) != 0
            || payload["omitted_detail_count"].as_u64().unwrap_or(0) != 0
        {
            let mut retained = state.clone();
            retained.retain_for_history(items);
            let pins = retained.artifact_recovery_directory_reusing(&mut rendered_candidates);
            drop(rendered_candidates);
            let members = pins
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pin| pin["artifact_id"].as_str().map(str::to_string))
                .collect();
            let directory = self
                .retain_context_source("artifact_directory", pins)
                .await?;
            #[expect(
                clippy::expect_used,
                reason = "successful retain_context_source always constructs an envelope with a string artifact_id"
            )]
            let artifact_id = directory["artifact_id"]
                .as_str()
                .expect("retained directory ID")
                .to_string();
            let mutation = crate::tool_history::ToolHistoryMutation::RegisterArtifactDirectory {
                artifact_id,
                members,
            };
            let _permit = self
                .tool_history_reconciliation_gate
                .acquire()
                .await
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            let mut writer = self.tool_history_persistence.writer().await;
            writer
                .enqueue_mutation(mutation.clone(), "artifact directory membership")
                .map_err(|error| CodexErr::Fatal(error.to_string()))?;
            self.state
                .lock()
                .await
                .apply_tool_history_mutation(&mutation);
            payload["directory"] = directory;
            payload["instruction"] = "Inline pins list the newest retained artifacts. Older artifacts, exact identities and recovered selectors are retained in directory; read its /items with read_tool_output only when a needed artifact is not listed here. Saved outputs and their provenance remain retained.".into();
            while codex_utils_string::approx_token_count(&payload.to_string())
                > crate::tool_history::COMPACTION_ARTIFACT_PIN_TOKEN_BUDGET
            {
                if payload["artifacts"]
                    .as_array_mut()
                    .and_then(Vec::pop)
                    .is_none()
                {
                    break;
                }
                payload["omitted_artifact_count"] =
                    (payload["omitted_artifact_count"].as_u64().unwrap_or(0) + 1).into();
            }
        }
        Ok(Some(payload.to_string()))
    }

    pub(crate) async fn recoverable_additional_context(
        &self,
        mut values: IndexMap<String, AdditionalContextEntry>,
    ) -> CodexResult<IndexMap<String, AdditionalContextEntry>> {
        crate::state::AdditionalContextStore::validate_application_context(&values)
            .map_err(CodexErr::InvalidRequest)?;
        let recovery_sources = crate::state::AdditionalContextStore::recovery_sources(&values);
        for source in recovery_sources {
            #[expect(
                clippy::expect_used,
                reason = "recovery_sources selects keys from this owned map and the loop never removes entries"
            )]
            let entry = values
                .get_mut(&source)
                .expect("source selected from this snapshot");
            let recovery = self
                .retain_context_source(
                    "additional_context",
                    serde_json::json!([
                        {"source": source, "kind": "untrusted", "value": entry.value}
                    ]),
                )
                .await?;
            entry.value = format!(
                "INCOMPLETE INLINE CONTEXT: the full supplied value is retained, not discarded. Recover /items/0/value with read_tool_output before relying on this source. Its original application/untrusted authority is unchanged; recovery does not promote untrusted text. {recovery}"
            );
        }
        Ok(values)
    }

    pub(crate) async fn reusable_tool_artifact(
        &self,
        call_id: &str,
        bytes: u64,
        sha256: &str,
    ) -> Option<String> {
        self.state
            .lock()
            .await
            .tool_history_state_ref()
            .reusable_artifact_for_origin(call_id, bytes, sha256)
    }

    pub(crate) async fn source_snapshot_backing(&self, bytes: u64, sha256: &str) -> Option<String> {
        self.state.lock().await.tool_history_state_ref().source_snapshot_backing(bytes, sha256)
    }

    pub(crate) async fn bind_source_inspection_origin(
        &self,
        artifact_id: String,
        call_id: String,
        bytes: u64,
        sha256: String,
        source: crate::tool_history::SourceInspectionOrigin,
    ) -> CodexResult<()> {
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        let mutation = crate::tool_history::ToolHistoryMutation::BindSourceInspectionOrigin {
            artifact_id,
            call_id,
            bytes,
            sha256,
            source,
        };
        let mut writer = self.tool_history_persistence.writer().await;
        writer
            .enqueue_mutation(mutation.clone(), "source inspection origin")
            .map_err(|error| {
                CodexErr::Fatal(format!(
                    "failed to enqueue source inspection origin: {error}"
                ))
            })?;
        self.state
            .lock()
            .await
            .apply_tool_history_mutation(&mutation);
        Ok(())
    }

    pub(crate) async fn completed_source_inspection(
        &self,
        artifact_id: &str,
        bytes: u64,
        sha256: &str,
        attempt_id: codex_agent_task_store::AttemptId,
    ) -> Option<(String, crate::tool_history::SourceInspectionOrigin)> {
        self.state
            .lock()
            .await
            .tool_history_state_ref()
            .completed_source_inspection(artifact_id, bytes, sha256, attempt_id)
    }

    pub(crate) async fn register_tool_history_candidate(
        &self,
        candidate: crate::tool_history::ToolHistoryCandidate,
    ) {
        let Ok(_reconciliation_permit) = self.tool_history_reconciliation_gate.acquire().await
        else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        let mutation = crate::tool_history::ToolHistoryMutation::RegisterCandidate {
            candidate: Box::new(candidate),
        };
        let mut persistence_writer = self.tool_history_persistence.writer().await;
        let mut state = self.state.lock().await;
        state.apply_tool_history_mutation(&mutation);
        if let Err(err) =
            persistence_writer.enqueue_mutation(mutation, "completed-tool history metadata")
        {
            tracing::warn!(
                "failed to enqueue completed-tool history metadata; in-memory state is not durable: {err}"
            );
        }
    }

    pub(crate) async fn record_tool_history_recovery(
        &self,
        artifact_id: String,
        recovery_call_id: String,
        selectors: Vec<serde_json::Value>,
    ) {
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        let mutation = crate::tool_history::ToolHistoryMutation::RecordArtifactRecovery {
            artifact_id: artifact_id.clone(),
            recovery_call_id,
            selectors,
        };
        let mut writer = self.tool_history_persistence.writer().await;
        let mut state = self.state.lock().await;
        if !state.apply_tool_history_mutation(&mutation) {
            return;
        }
        if let Err(err) = writer.enqueue_mutation(mutation, "artifact reuse priority") {
            tracing::warn!("failed to enqueue artifact reuse priority: {err}");
        }
    }

    pub(crate) async fn prepare_workspace_evidence_item(
        &self,
        item: &ResponseItem,
        policy: codex_utils_output_truncation::TruncationPolicy,
    ) -> ResponseItem {
        self.state.lock().await.history.process_item(item, policy)
    }

    pub(crate) async fn register_workspace_evidence<WorkspaceGateGuard: Send>(
        &self,
        observation: crate::tool_history::WorkspaceEvidenceObservation,
        workspace_gate_guard: WorkspaceGateGuard,
    ) {
        drop(workspace_gate_guard);
        self.register_workspace_evidence_batch(vec![observation]).await;
    }

    pub(crate) async fn register_workspace_evidence_batch(
        &self,
        observations: Vec<crate::tool_history::WorkspaceEvidenceObservation>,
    ) {
        if observations.is_empty() { return; }
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        for observation in observations {
            let mutation = crate::tool_history::ToolHistoryMutation::RegisterWorkspaceEvidence { observation };
            let mut writer = self.tool_history_persistence.writer().await;
            let mut state = self.state.lock().await;
            state.apply_tool_history_mutation(&mutation);
            if let Err(err) = writer.enqueue_mutation(mutation, "native source acquisition metadata") {
                tracing::warn!("failed to enqueue native source acquisition metadata: {err}");
            }
        }
    }

    pub(crate) async fn register_code_mode_nested_evidence(
        &self,
        parent_call_id: String,
        call_id: String,
        output: String,
    ) {
        let Ok(_permit) = self.tool_history_reconciliation_gate.acquire().await else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        let mutation = crate::tool_history::ToolHistoryMutation::RegisterCodeModeNestedEvidence {
            parent_call_id,
            call_id,
            output,
        };
        let mut persistence_writer = self.tool_history_persistence.writer().await;
        let mut state = self.state.lock().await;
        if !state.apply_tool_history_mutation(&mutation) {
            return;
        }
        if let Err(err) = persistence_writer.enqueue_mutation(mutation, "nested workspace evidence")
        {
            tracing::warn!("failed to enqueue nested workspace evidence: {err}");
        }
    }

    pub(crate) async fn register_non_workspace_code_mode_call(&self, call_id: String) {
        let Ok(_reconciliation_permit) = self.tool_history_reconciliation_gate.acquire().await
        else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        let mutation =
            crate::tool_history::ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall { call_id };
        let mut persistence_writer = self.tool_history_persistence.writer().await;
        let mut state = self.state.lock().await;
        state.apply_tool_history_mutation(&mutation);
        if let Err(err) = persistence_writer
            .enqueue_mutation(mutation, "code-mode workspace classification metadata")
        {
            tracing::warn!(
                "failed to enqueue code-mode workspace classification metadata; in-memory state is not durable: {err}"
            );
        }
    }

    pub(crate) async fn flush_tool_history_persistence(&self) -> CodexResult<()> {
        self.tool_history_persistence
            .checkpoint()
            .await
            .map_err(|error| {
                CodexErr::Fatal(format!("completed-tool history checkpoint failed: {error}"))
            })
    }

    pub(crate) async fn invalidate_tool_history_source_dependencies(
        &self,
        affected_paths: Option<&std::collections::BTreeSet<std::path::PathBuf>>,
        current_workspace_identity: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
    ) -> CodexResult<()> {
        self.invalidate_tool_history_source_dependencies_excluding_call_ids(
            affected_paths,
            current_workspace_identity,
            &std::collections::BTreeSet::new(),
        )
        .await
    }

    pub(crate) async fn invalidate_tool_history_source_dependencies_excluding_call_ids(
        &self,
        _affected_paths: Option<&std::collections::BTreeSet<std::path::PathBuf>>,
        _current_workspace_identity: Option<&crate::git_workspace::WorkspaceEvidenceIdentity>,
        _excluded_call_ids: &std::collections::BTreeSet<String>,
    ) -> CodexResult<()> {
        Ok(())
    }

    pub(crate) async fn mark_tool_history_consumed(
        &self,
        turn_context: &TurnContext,
        input: &[ResponseItem],
        generation: crate::tool_history::ModelGenerationId,
    ) {
        if !turn_context.config.completed_tool_history_projection {
            return;
        }
        let Ok(reconciliation_permit) = self.tool_history_reconciliation_gate.acquire().await
        else {
            unreachable!("session-owned tool-history reconciliation semaphore is never closed");
        };
        {
            let mut persistence_writer = self.tool_history_persistence.writer().await;
            let mut state = self.state.lock().await;
            let call_ids = state.mark_tool_history_consumed_with_delta(input, generation.clone());
            if call_ids.is_empty() {
                return;
            }
            if let Err(err) = persistence_writer.enqueue_mutation(
                crate::tool_history::ToolHistoryMutation::MarkConsumed {
                    call_ids,
                    generation,
                    exposed_representations: std::collections::BTreeMap::new(),
                },
                "completed-tool consumption state",
            ) {
                tracing::warn!(
                    "failed to enqueue completed-tool consumption state; in-memory state is not durable: {err}"
                );
            }
        }
        drop(reconciliation_permit);
    }

    pub(crate) async fn current_window_id(&self) -> String {
        let state = self.state.lock().await;
        let thread_id = self.thread_id;
        let window_number = state.auto_compact_window_number();
        format!("{thread_id}:{window_number}")
    }


    pub(crate) async fn reference_context_item(&self) -> Option<TurnContextItem> {
        let state = self.state.lock().await;
        state.reference_context_item()
    }

    pub(crate) async fn has_reference_context_item(&self) -> bool {
        let state = self.state.lock().await;
        state.has_reference_context_item()
    }

    async fn prepare_context_update_inner(
        &self,
        step_context: &StepContext,
        estimate: bool,
    ) -> PreparedContextUpdate {
        let turn_context = Arc::clone(&step_context.turn);
        let (reference_context_item, pending_context_baseline) = {
            let state = self.state.lock().await;
            (
                state.reference_context_item(),
                state.pending_context_baseline(),
            )
        };
        // A staged baseline is not provider-realized yet, but its context is already present in
        // history. Use it for local diffing so the first post-compaction plan does not append a
        // second full startup block. Provider inheritance continues to use only the realized
        // reference item.
        let represented_context_item = pending_context_baseline
            .as_ref()
            .map(|candidate| &candidate.turn_context_item)
            .or(reference_context_item.as_ref());
        let turn_context_item = turn_context.to_turn_context_item_async().await;
        let turn_context_changed =
            !context_snapshot_matches(represented_context_item, &turn_context_item);
        let world_state = Arc::new(if estimate {
            self.estimate_world_state_for_step(step_context).await
        } else {
            self.build_world_state_for_step(step_context).await
        });
        let represented_fragment_digests = pending_context_baseline
            .as_ref()
            .map(|candidate| candidate.fragment_digests.as_slice())
            .or_else(|| {
                reference_context_item
                    .as_ref()
                    .and_then(|item| item.context_provenance.as_ref())
                    .map(|provenance| provenance.fragment_digests.as_slice())
            });

        let complete_context = self
            .build_context_candidate(
                turn_context.as_ref(),
                world_state.as_ref(),
                estimate,
                represented_context_item,
                represented_fragment_digests,
            )
            .await;
        let InitialContextBuild {
            full_context_required: should_inject_full_context,
            items: complete_items,
            turn_context_items,
            world_state_snapshot: complete_world_state_snapshot,
            fragment_digests,
        } = complete_context;

        let turn_contributions_changed = represented_fragment_digests.is_some_and(|represented| {
            !represented
                .iter()
                .filter(|digest| digest.key.starts_with("turn-contributor:"))
                .eq(fragment_digests
                    .iter()
                    .filter(|digest| digest.key.starts_with("turn-contributor:")))
        });
        let (mut context_items, world_state_item, world_state_snapshot) =
            if should_inject_full_context {
                (
                    complete_items,
                    Some(WorldStateItem::full(
                        complete_world_state_snapshot.clone().into_value(),
                    )),
                    complete_world_state_snapshot,
                )
            } else {
                let mut context_items = self
                    .build_settings_update_items(represented_context_item, turn_context.as_ref())
                    .await;
                let (world_state_items, world_state_item, world_state_snapshot) = {
                    let mut history = self.clone_history().await;
                    if let Some(candidate) = pending_context_baseline.as_ref() {
                        history.set_world_state_baseline(candidate.world_state_snapshot.clone());
                    }
                    let (fragments, rollout_item) =
                        history.update_world_state(world_state.as_ref());
                    let world_state_snapshot = history.world_state_baseline().unwrap_or_default();
                    // Nothing represented by a staged baseline has been durably accepted yet.
                    // Preserve a full receipt across local diffs so accepting the eventual
                    // request can commit the complete latest world state in one transaction.
                    let rollout_item = if pending_context_baseline.is_some() {
                        Some(WorldStateItem::full(
                            world_state_snapshot.clone().into_value(),
                        ))
                    } else {
                        rollout_item
                    };
                    (
                        crate::context_manager::updates::merge_contextual_fragments(fragments),
                        rollout_item,
                        world_state_snapshot,
                    )
                };
                context_items.extend(world_state_items);
                (context_items, world_state_item, world_state_snapshot)
            };
        if !should_inject_full_context && (turn_context_changed || turn_contributions_changed) {
            // These are the exact turn-contributor fragments captured while building the full
            // candidate above. Reusing them avoids a second contributor poll in the diff path.
            context_items.extend(turn_context_items);
        }
        // Explicit tombstones replace contributions that disappeared, even when
        // another stable change also selects the full startup context.
        let removed_indices = represented_fragment_digests
            .into_iter()
            .flatten()
            .filter(|digest| {
                digest.key.starts_with("turn-contributor:")
                    && !fragment_digests
                        .iter()
                        .any(|current| current.key == digest.key)
            })
            .filter_map(|digest| digest.key.rsplit(':').next()?.parse::<usize>().ok());
        if let Some(removals) = crate::context_manager::updates::build_developer_update_item(
            removed_indices
                .map(crate::stable_context::turn_contribution_removal)
                .collect(),
        ) {
            context_items.push(removals);
        }
        if !estimate {
            self.services.session_telemetry.counter(
                "codex.context_update",
                1,
                &[
                    (
                        "result",
                        if context_items.is_empty() {
                            "empty"
                        } else {
                            "emitted"
                        },
                    ),
                    (
                        "projection",
                        if should_inject_full_context {
                            "full"
                        } else {
                            "delta"
                        },
                    ),
                ],
            );
        }
        PreparedContextUpdate {
            turn_context,
            world_state,
            context_items,
            baseline: ContextBaselineCandidate {
                turn_context_item,
                world_state_snapshot,
                world_state_item,
                fragment_digests,
                bound_sampling_request_id: None,
                bound_physical_attempt_id: None,
            },
        }
    }

    /// Builds the immutable model-visible context candidate used by both planning and commit.
    pub(crate) async fn prepare_context_update(
        &self,
        step_context: &StepContext,
    ) -> PreparedContextUpdate {
        self.prepare_context_update_inner(step_context, false).await
    }


    /// Persist the latest turn context snapshot for the first real user turn and for
    /// steady-state turns that emit model-visible context updates.
    ///
    /// When the reference snapshot is missing, this injects full initial context. Otherwise, it
    /// emits only context diffs.
    ///
    /// If full context is injected and a model switch occurred, this prepends the compact
    /// `<model_switch>` compatibility note while preserving the session base instructions.
    ///
    /// This is the normal runtime path that establishes a new `reference_context_item`.
    /// Mid-turn compaction is the other path that can re-establish that reference when it
    /// reinjects full initial context into replacement history. Live world-state changes may
    /// independently advance their in-memory baseline within a turn.
    #[instrument(level = "trace", skip_all)]
    pub(crate) async fn record_context_updates_and_set_reference_context_item(
        self: &Arc<Self>,
        step_context: &StepContext,
    ) -> std::io::Result<Arc<WorldState>> {
        let prepared = self.prepare_context_update(step_context).await;
        self.commit_prepared_context_update(prepared, None)
            .await?
            .ok_or_else(|| std::io::Error::other("unconditional context commit was stale"))
    }

    /// Compares a pending plan's generation and begins its persistence while
    /// holding the session-state owner. A stale plan leaves history, the
    /// pending context baseline, and rollout storage untouched.
    pub(crate) async fn compare_and_record_context_updates(
        self: &Arc<Self>,
        prepared: PreparedContextUpdate,
        expected_planning_generation: u64,
    ) -> std::io::Result<Option<Arc<WorldState>>> {
        self.commit_prepared_context_update(prepared, Some(expected_planning_generation))
            .await
    }

    fn compare_and_commit_planning_state<R>(
        &self,
        state_owner: &mut SessionState,
        expected_planning_generation: Option<u64>,
        commit: impl FnOnce(&mut SessionState) -> R,
    ) -> Option<R> {
        if expected_planning_generation
            .is_some_and(|expected| self.services.planning_generation() != expected)
        {
            return None;
        }
        Some(commit(state_owner))
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "Keep planning generation validation atomic with durable context publication"
    )]
    async fn commit_prepared_context_update(
        self: &Arc<Self>,
        prepared: PreparedContextUpdate,
        expected_planning_generation: Option<u64>,
    ) -> std::io::Result<Option<Arc<WorldState>>> {
        let session = Arc::clone(self);
        let in_flight = self.retain_tool_dispatch_commit();
        let commit = tokio::spawn(async move {
            let _in_flight = in_flight;
            let _permit = session
                .durable_history_commit_gate
                .acquire()
                .await
                .map_err(|_| std::io::Error::other("history commit gate is closed"))?;
            let PreparedContextUpdate {
                turn_context,
                world_state,
                context_items,
                baseline,
            } = prepared;
            let context_items = Self::prepare_owned_conversation_items_for_history(
                &turn_context.sub_id,
                context_items,
            );
            let persistence_timing_guard = turn_context
                .turn_timing_state
                .begin_local_phase(TurnLocalPhase::Persistence);
            // Invalidation also owns this lock. Keep its generation check and
            // append acceptance indivisible with respect to publication.
            let mut state = session.state.lock().await;
            if session
                .compare_and_commit_planning_state(&mut state, expected_planning_generation, |_| ())
                .is_none()
            {
                return Ok(None);
            }
            // One changed fragment selects the full startup context. Append only
            // sections whose latest value in history differs, as turn injections do.
            let context_items = crate::stable_context::filter_unchanged_stable_context_items(
                state.history.raw_items(),
                context_items,
            );
            let rollout_items = context_items
                .iter()
                .cloned()
                .map(RolloutItem::ResponseItem)
                .collect::<Vec<_>>();
            session
                .persist_rollout_items_ordered(&rollout_items)
                .await?;
            state
                .current_time_reminder
                .note_recorded_items(&context_items);
            state.record_items(
                context_items.iter(),
                turn_context.model_info.truncation_policy.into(),
            );
            state.stage_context_baseline(baseline);
            drop(state);
            drop(_permit);
            drop(persistence_timing_guard);
            session
                .send_raw_response_items(&turn_context, &context_items)
                .await;
            Ok(Some(world_state))
        });
        commit.await.map_err(|error| {
            std::io::Error::other(format!("prepared context commit task failed: {error}"))
        })?
    }

    pub(crate) async fn record_failed_attempt_usage(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
        identity: Option<&crate::client_common::ResponseAttemptIdentity>,
    ) -> CodexResult<()> {
        let result = self
            .record_token_usage_with_context(turn_context, token_usage, false, identity)
            .await;
        self.send_token_count_event(turn_context).await;
        result
    }

    pub(crate) async fn calibrated_prompt_tokens(
        &self,
        turn_context: &TurnContext,
        local: i64,
    ) -> i64 {
        self.state.lock().await.prompt_token_calibration.estimate(
            &turn_context.config.model_provider_id,
            &turn_context.model_info.slug,
            local,
        )
    }

    pub(crate) async fn record_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        self.record_token_usage_with_context(turn_context, token_usage, true, None)
            .await
    }

    pub(crate) async fn record_compaction_token_usage(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) -> CodexResult<()> {
        let result = self
            .record_token_usage_with_context(turn_context, token_usage, false, None)
            .await;
        self.send_token_count_event(turn_context).await;
        result
    }

    pub(crate) async fn settle_observed_model_usage(&self, turn_context: &TurnContext) -> u64 {
        let mut state = self.state.lock().await;
        let receipts = turn_context.turn_timing_state.claim_unsettled_model_usage();
        let count = receipts.len() as u64;
        if count == 0 { return 0; }
        let mut info = state.token_info().unwrap_or_else(|| TokenUsageInfo {
            total_token_usage: TokenUsage::empty_ledger(),
            last_token_usage: TokenUsage::default(),
            estimated_context_tokens: None,
            model_context_window: turn_context.model_context_window(),
        });
        for usage in receipts {
            turn_context.record_current_turn_usage(&usage);
            info.total_token_usage.add_assign(&usage);
            self.services.agent_control.task_coordinator().record_task_usage_for_source(
                &turn_context.session_source,
                crate::agent::task_metrics::TokenClassTotals::from_usage(&usage), 1,
            );
        }
        // Billable abandoned/compactor usage cannot replace active context.
        // There is no await between claiming receipts and committing totals.
        state.set_token_info(Some(info));
        turn_context.turn_timing_state.mark_model_responses_accounted(count);
        count
    }

    pub(crate) async fn record_missing_model_usage(&self, turn_context: &TurnContext, count: u64) {
        if count == 0 {
            return;
        }
        let mut missing = TokenUsage::missing_receipt();
        missing.coverage = Some(codex_protocol::protocol::TokenUsageCoverage {
            receipts: count,
            reported_receipts: Some(0),
            ..Default::default()
        });
        turn_context.record_current_turn_usage(&missing);
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or_else(|| TokenUsageInfo {
                total_token_usage: TokenUsage::empty_ledger(),
                last_token_usage: TokenUsage::default(),
                estimated_context_tokens: None,
                model_context_window: turn_context.model_context_window(),
            });
            info.total_token_usage.add_assign(&missing);
            // Preserve the context estimate independently, but do not present an
            // earlier receipt as the latest attempt's observed consumption.
            info.estimated_context_tokens = Some(info.context_tokens());
            info.last_token_usage = missing.clone();
            state.set_token_info(Some(info));
        }
        self.services
            .agent_control
            .task_coordinator()
            .record_task_usage_for_source(
                &turn_context.session_source,
                crate::agent::task_metrics::TokenClassTotals::from_usage(&missing),
                count,
            );
        turn_context
            .turn_timing_state
            .mark_model_responses_accounted(count);
    }

    async fn record_token_usage_with_context(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
        update_context: bool,
        attempt_identity: Option<&crate::client_common::ResponseAttemptIdentity>,
    ) -> CodexResult<()> {
        if let Some(token_usage) = token_usage {
            // Release the timing lock before acquiring session state.
            let prepared_basis = turn_context.extension_data
                .get::<crate::client_common::PromptCalibrationBasis>();
            let valid_anchor = token_usage.known_total().is_some();
            let calibration = prepared_basis.as_ref().filter(|_| valid_anchor).and_then(|basis| {
                turn_context.turn_timing_state.prompt_token_calibration_with_estimator(
                    &turn_context.model_info.slug, token_usage, basis.local_tokens,
                )
            });
            let token_info = {
                let mut state = self.state.lock().await;
                // Claim only after acquiring the accounting owner lock.
                let delivered = update_context.then(|| turn_context.extension_data
                    .get::<crate::client_common::ResponseAttemptIdentity>()).flatten();
                if !turn_context.turn_timing_state.claim_current_model_usage(token_usage, attempt_identity.or(delivered.as_deref())) {
                    return Ok(());
                }
                turn_context.record_current_turn_usage(token_usage);
                if update_context {
                    if !valid_anchor {
                        state.prompt_token_calibration = Default::default();
                    } else if let Some(basis) = turn_context
                        .extension_data
                        .get::<crate::client_common::PromptCalibrationBasis>()
                    {
                        state.prompt_token_calibration.observe_prompt(
                            &turn_context.config.model_provider_id,
                            &turn_context.model_info.slug,
                            basis.as_ref().clone(),
                            calibration.map(|(_, measured)| measured),
                            token_usage.known_input().map(|tokens| tokens as u64),
                        );
                    } else {
                        state.prompt_token_calibration.observe(
                            &turn_context.config.model_provider_id,
                            &turn_context.model_info.slug,
                            calibration,
                        );
                    }
                    state.update_token_info_from_usage(
                        token_usage,
                        turn_context.model_context_window(),
                    );
                    if !valid_anchor {
                        let estimated = state.history.estimate_prepared_token_count_with_base_instructions(
                            &turn_context.model_info.input_modalities,
                            &turn_context.base_instructions,
                        ).unwrap_or(0);
                        if let Some(mut info) = state.token_info() {
                            info.estimated_context_tokens = Some(estimated.max(0));
                            state.set_token_info(Some(info));
                        }
                    }
                    if valid_anchor && token_usage.known_input().is_some()
                        && matches!(
                            turn_context.config.model_auto_compact_token_limit_scope,
                            AutoCompactTokenLimitScope::BodyAfterPrefix
                        )
                    {
                        state.ensure_auto_compact_window_server_prefill_from_usage(token_usage);
                    }
                } else {
                    // Compactor consumption is billable, but its prompt/output
                    // does not describe the still-active conversation history.
                    let mut info = state.token_info().unwrap_or_else(|| TokenUsageInfo {
                        total_token_usage: TokenUsage::empty_ledger(),
                        last_token_usage: TokenUsage::default(),
                        estimated_context_tokens: None,
                        model_context_window: turn_context.model_context_window(),
                    });
                    info.total_token_usage.add_assign(token_usage);
                    state.set_token_info(Some(info));
                    // It is still the latest request: publish it as such so
                    // per-request readers do not repeat the prior receipt.
                    state.record_compaction_receipt(token_usage);
                }
                state.published_token_info()
            };
            // Settle counts before extension callbacks can suspend or be cancelled.
            self.services
                .agent_control
                .task_coordinator()
                .record_task_usage_for_source(
                    &turn_context.session_source,
                    crate::agent::task_metrics::TokenClassTotals::from_usage(token_usage),
                    1,
                );
            turn_context
                .turn_timing_state
                .mark_model_responses_accounted(1);
            if let Some(token_info) = token_info.as_ref() {
                for contributor in self.services.extensions.token_usage_contributors() {
                    contributor
                        .on_token_usage(
                            &self.services.session_extension_data,
                            &self.services.thread_extension_data,
                            turn_context.extension_data.as_ref(),
                            token_info,
                        )
                        .await;
                }
            }
        } else {
            self.record_missing_model_usage(turn_context, 1).await;
            if update_context {
                // Successful generation can lack usable provider accounting. Keep
                // budget protection using the existing prepared-history estimate.
                self.state.lock().await.prompt_token_calibration.observe(
                    &turn_context.config.model_provider_id,
                    &turn_context.model_info.slug,
                    None,
                );
                self.recompute_token_usage(turn_context).await;
            }
        }
        if update_context && token_usage.is_some_and(|usage| usage.known_total().is_none()) {
            self.recompute_token_usage(turn_context).await;
        }
        let coordinator = self.services.agent_control.task_coordinator();
        if let Some(binding) = coordinator.binding_for_turn(turn_context) {
            match coordinator
                .heartbeat_typed_actor_binding(&binding, /*progress*/ true)
                .await
            {
                Ok(true) => {}
                Ok(false) => warn!(
                    attempt_id = %binding.attempt_id,
                    "typed actor heartbeat was rejected after model progress"
                ),
                Err(error) => warn!(
                    attempt_id = %binding.attempt_id,
                    %error,
                    "typed actor heartbeat failed after model progress"
                ),
            }
        }
        Ok(())
    }

    /// Outputs retired to recovery pins leave every later prompt, but the
    /// provider's last usage still counts them until the next request. Remove
    /// the estimated saving so compaction pressure reflects the next prompt.
    pub(crate) async fn discount_retired_context_tokens(&self, retired_tokens: i64) {
        let mut state = self.state.lock().await;
        let Some(mut info) = state.token_info() else {
            return;
        };
        info.estimated_context_tokens = Some(
            info.context_tokens()
                .saturating_sub(retired_tokens.max(0))
                .max(0),
        );
        state.set_token_info(Some(info));
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        if let Some(event) = self.recompute_token_usage_state(turn_context).await {
            self.send_token_count(turn_context, event).await;
        }
    }

    /// Update all token state and capture its notification before releasing admission.
    async fn recompute_token_usage_state(
        &self,
        turn_context: &TurnContext,
    ) -> Option<TokenCountEvent> {
        let history = self.state.lock().await.history.clone();
        let base_instructions = &turn_context.base_instructions;
        let estimated_total_tokens = history.estimate_prepared_token_count_with_base_instructions(
            &turn_context.model_info.input_modalities,
            base_instructions,
        )?;
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                estimated_context_tokens: None,
                total_token_usage: TokenUsage::empty_ledger(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            // This notification describes the captured snapshot. Budget readers
            // prepare the current history whenever a local override is present,
            // so concurrent appends cannot disappear behind a newer model anchor.
            info.estimated_context_tokens = Some(estimated_total_tokens.max(0));

            info.model_context_window = turn_context.model_context_window();

            state.set_token_info(Some(info));
        }
        self.set_auto_compact_window_estimated_prefill_for_scope(
            turn_context,
            estimated_total_tokens,
        )
        .await;
        let (info, _) = self.state.lock().await.token_info_and_rate_limits();
        Some(TokenCountEvent { info, rate_limits: None })
    }

    pub(crate) async fn bind_rate_limit_account(
        &self,
        account: Option<crate::responses_retry::AccountChangeIdentity>,
    ) {
        self.state.lock().await.bind_rate_limit_account(account);
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        turn_context
            .turn_timing_state
            .observe_credits(new_rate_limits.credits.as_ref());
        // Only this fresh provider observation may publish account quota. Usage
        // updates must not replay this session's older account-wide snapshot.
        let (mut info, rate_limits) = {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
            state.token_info_and_rate_limits()
        };
        if let Some(info) = info.as_mut() {
            info.model_context_window = turn_context.model_context_window();
        }
        self.send_token_count(turn_context, TokenCountEvent { info, rate_limits }).await;
    }

    pub(crate) async fn mcp_dependency_prompted(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.mcp_dependency_prompted()
    }

    pub(crate) async fn record_mcp_dependency_prompted<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut state = self.state.lock().await;
        state.record_mcp_dependency_prompted(names);
    }

    pub(crate) async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (mut info, _) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        if let Some(info) = info.as_mut() {
            // Settings can change without a new usage receipt. Advertise the
            // active model's capacity, including an explicitly unknown value.
            info.model_context_window = turn_context.model_context_window();
        }
        self.send_token_count(turn_context, TokenCountEvent { info, rate_limits: None }).await;
    }

    /// Publishes usage and rate limits unless they exactly repeat the last
    /// publication. Unchanged rate-limit refreshes, transport fallbacks, and
    /// lifecycle steps that change neither would otherwise persist duplicate
    /// records that per-request readers cannot tell from new requests.
    async fn send_token_count(&self, turn_context: &TurnContext, event: TokenCountEvent) {
        if self.state.lock().await.repeats_published_token_count(&event) {
            return;
        }
        let published = event.clone();
        self.send_event(turn_context, EventMsg::TokenCount(event)).await;
        // Record only after dispatch, so a cancelled send cannot suppress a
        // later identical publication.
        self.state.lock().await.record_published_token_count(&published);
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) -> std::io::Result<()> {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await?;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
        Ok(())
    }

    pub(crate) async fn record_user_prompt_and_emit_turn_item(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        input: &[UserInput],
        client_id: Option<String>,
    ) -> std::io::Result<()> {
        let receipt = self.pending_receipt_for_input(input, client_id.as_deref()).await?;
        turn_context.turn_timing_state.record_user_input();
        let mut user_message_item = UserMessageItem::new(input);
        let response_item_id = match receipt.as_ref() {
            Some((_, suffix)) => ResponseItemId::with_suffix(crate::event_mapping::USER_INPUT_ITEM_ID_BASE, suffix),
            None => ResponseItemId::with_suffix(
                crate::event_mapping::USER_INPUT_ITEM_ID_BASE, &user_message_item.id,
            ),
        };
        user_message_item.id = response_item_id.to_string();
        user_message_item.client_id = client_id;
        let mut response_item = self.response_item_from_user_input(input.to_vec()).await?;
        response_item.set_id(Some(response_item_id));
        // Persist the user message to history, but emit the turn item from `UserInput` so
        // UI-only `text_elements` are preserved. `ResponseItem::Message` does not carry
        // those spans, and `record_response_item_and_emit_turn_item` would drop them.
        self.record_conversation_items_ordered(turn_context, std::slice::from_ref(&response_item))
            .await?;
        let turn_item = TurnItem::UserMessage(user_message_item);
        self.emit_turn_item_started(turn_context, &turn_item).await;
        self.emit_turn_item_completed(turn_context, turn_item).await;
        if let Some((receipt, suffix)) = receipt {
            self.complete_input_receipt(&receipt, &suffix).await?;
        }
        // Keep the accepted input ordered in the rollout, then let the sampling or terminal
        // barrier materialize the complete prefix once. Materializing here would force an extra
        // disk transaction immediately before every initial model request.
        Ok(())
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        // This event announces an active retry, not a terminal failure. Keep
        // model-switch advice for the final error after recovery is exhausted.
        let additional_details = if matches!(&codex_error, CodexErr::ServerOverloaded { .. }) {
            "Selected model is temporarily at capacity. Retrying automatically; your turn is still active."
                .to_string()
        } else {
            codex_error.to_string()
        };
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    /// Inject additional user input into the currently active turn.
    ///
    /// Returns the active turn id when accepted.
    pub async fn steer_input(
        self: &Arc<Self>,
        input: Vec<UserInput>,
        additional_context: IndexMap<String, AdditionalContextEntry>,
        expected_turn_id: Option<&str>,
        client_user_message_id: Option<String>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
    ) -> Result<String, SteerInputError> {
        if client_user_message_id.is_none() {
            return self.steer_input_inner(input, additional_context, expected_turn_id,
                client_user_message_id, responsesapi_client_metadata).await;
        }
        // Acceptance outlives an RPC disconnect: once its durable write begins,
        // the same owner must publish pending input or retain it for recovery.
        let session = Arc::clone(self);
        let expected_turn_id = expected_turn_id.map(str::to_owned);
        self.terminal_tasks.spawn(async move {
            session.steer_input_inner(input, additional_context, expected_turn_id.as_deref(),
                client_user_message_id, responsesapi_client_metadata).await
        }).await.map_err(|error| SteerInputError::ReceiptUnavailable {
            message: format!("steering admission task failed: {error}"),
        })?
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn steer_input_inner(
        &self,
        input: Vec<UserInput>,
        additional_context: IndexMap<String, AdditionalContextEntry>,
        expected_turn_id: Option<&str>,
        client_user_message_id: Option<String>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
    ) -> Result<String, SteerInputError> {
        let _receipt_gate = if client_user_message_id.is_some() { Some(self.input_receipt_gate.lock().await) } else { None };
        if client_user_message_id.is_some() && self.shutting_down.load(Ordering::Acquire) {
            return Err(SteerInputError::ReceiptUnavailable { message: "the thread is shutting down".into() });
        }
        let fingerprint = if client_user_message_id.is_some() {
            input_receipts::fingerprint(&(&input, &additional_context, &responsesapi_client_metadata))
                .map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?
        } else { String::new() };
        let db = if client_user_message_id.is_some() {
            Some(self.receipt_db().map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?)
        } else { None };
        if let (Some(db), Some(client_id), Some(scope)) = (&db, &client_user_message_id, expected_turn_id)
            && let Some(receipt) = db.input_receipt(self.thread_id, "steer", scope, client_id).await
                .map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?
        {
            if receipt.fingerprint != fingerprint { return Err(SteerInputError::IdempotencyConflict); }
            return Ok(receipt.turn_id);
        }
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };

        let Some(active_task) = active_turn.task.as_ref() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };
        let active_turn_context = Arc::clone(&active_task.turn_context);
        let active_turn_id = active_turn_context.sub_id.clone();

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id
        {
            return Err(SteerInputError::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id.clone(),
            });
        }

        match active_task.kind {
            crate::state::TaskKind::Regular => {}
            crate::state::TaskKind::Review => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Review,
                });
            }
            crate::state::TaskKind::Compact => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Compact,
                });
            }
        }

        if input.is_empty() {
            return Err(SteerInputError::EmptyInput);
        }
        let input_for_telemetry = input.clone();
        let additional_context = self
            .recoverable_additional_context(additional_context)
            .await
            .map_err(|error| SteerInputError::AdditionalContextUnavailable {
                message: error.to_string(),
            })?;
        let receipt = match client_user_message_id.as_ref() {
            Some(client_id) => {
                let mut receipt = codex_state::InputReceipt { kind: "steer".into(), scope: active_turn_id.clone(),
                    client_id: client_id.clone(), fingerprint, turn_id: active_turn_id.clone(), payload: None };
                receipt.payload = Some(self.receipt_payload(&receipt, &input, &additional_context)
                    .map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?);
                Some(receipt)
            }
            None => None,
        };
        let receipt_suffix = receipt.as_ref().map(|receipt| {
            input_receipts::fingerprint(&(self.thread_id.to_string(), &receipt.kind, &receipt.scope, &receipt.client_id))
        }).transpose().map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?;
        // Keep the map with accepted input until its ordered recording commits.
        let mut pending_input = vec![TurnInput::AdditionalContext { values: additional_context, start_fence: None }];
        pending_input.push(TurnInput::UserInput {
            content: input,
            client_id: client_user_message_id,
            start_fence: None,
        });
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                &pending_input,
                async || {
                    if let (Some(db), Some(receipt)) = (&db, &receipt) {
                        let accepted = db.reserve_input_receipt(self.thread_id, receipt).await
                            .map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?;
                        if !accepted {
                            let original = db.input_receipt(self.thread_id, "steer", &receipt.scope, &receipt.client_id).await
                                .map_err(|error| SteerInputError::ReceiptUnavailable { message: error.to_string() })?;
                            if !original.is_some_and(|original| original.fingerprint == receipt.fingerprint) {
                                return Err(SteerInputError::IdempotencyConflict);
                            }
                        }
                        return Ok(accepted);
                    }
                    Ok(true)
                },
                || {
                    if let Some(suffix) = &receipt_suffix {
                        self.input_queue.remember_input_receipt(suffix);
                    }
                    if let Some(metadata) = responsesapi_client_metadata {
                        active_turn_context
                            .turn_metadata_state
                            .set_responsesapi_client_metadata(metadata);
                    }
                },
            )
            .await?;
        drop(active);
        active_turn_context
            .session_telemetry
            .user_prompt(&input_for_telemetry);
        Ok(active_turn_id)
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let had_active_turn = self.active_turn.lock().await.is_some();
        self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        if !had_active_turn {
            self.cancel_mcp_startup().await;
        }
    }

    pub(crate) fn hooks(&self) -> Arc<Hooks> {
        self.services.hooks.load_full()
    }

    /// Session-scoped attachments shared by tools and extensions.
    pub(crate) fn session_extension_data(&self) -> &codex_extension_api::ExtensionData {
        &self.services.session_extension_data
    }

    pub(crate) async fn shell_for_environment(
        &self,
        environment: &TurnEnvironment,
    ) -> anyhow::Result<Arc<shell::Shell>> {
        match &environment.shell {
            Some(shell) => Ok(Arc::new(shell.clone())),
            None => self.services.user_shell.resolve().await,
        }
    }


    pub(crate) async fn current_rollout_path(&self) -> anyhow::Result<Option<PathBuf>> {
        let Some(live_thread) = self.live_thread() else {
            return Ok(None);
        };
        live_thread.local_rollout_path().await.map_err(Into::into)
    }

    pub(crate) async fn hook_transcript_path(&self) -> Option<PathBuf> {
        self.ensure_rollout_materialized().await;
        match self.current_rollout_path().await {
            Ok(path) => path,
            Err(err) => {
                warn!("{err}");
                None
            }
        }
    }

    pub(crate) async fn take_pending_session_start_source(
        &self,
    ) -> Option<codex_hooks::SessionStartSource> {
        let mut state = self.state.lock().await;
        state.take_pending_session_start_source()
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }
}

struct Sha256Writer(Sha256);

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Digest::update(&mut self.0, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn durable_history_commit_key(turn_id: &str, items: &[ResponseItem]) -> std::io::Result<String> {
    let mut writer = Sha256Writer(Sha256::new());
    serde_json::to_writer(&mut writer, items).map_err(std::io::Error::other)?;
    Ok(format!("{turn_id}:{}", hex::encode(writer.0.finalize())))
}

const HISTORY_IDENTITY_OFFLOAD_MIN_TEXT_BYTES: usize = 48 * 1024;

fn conversation_items_need_blocking_history_identity(items: &[ResponseItem]) -> bool {
    items.iter().any(|item| match item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output
            .text_content()
            .is_none_or(|text| text.len() >= HISTORY_IDENTITY_OFFLOAD_MIN_TEXT_BYTES),
        _ => true,
    })
}

pub(crate) fn emit_subagent_session_started(
    analytics_events_client: &AnalyticsEventsClient,
    client_metadata: AppServerClientMetadata,
    session_id: SessionId,
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    thread_config: ThreadConfigSnapshot,
    subagent_source: SubAgentSource,
) {
    let AppServerClientMetadata {
        client_name,
        client_version,
    } = client_metadata;
    let (Some(client_name), Some(client_version)) = (client_name, client_version) else {
        tracing::warn!("skipping subagent thread analytics: missing inherited client metadata");
        return;
    };
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    analytics_events_client.track_subagent_thread_started(SubAgentThreadStartedInput {
        session_id: session_id.to_string(),
        thread_id: thread_id.to_string(),
        parent_thread_id: parent_thread_id.map(|thread_id| thread_id.to_string()),
        forked_from_thread_id: thread_config
            .forked_from_thread_id
            .map(|thread_id| thread_id.to_string()),
        product_client_id: thread_config.originator.clone(),
        client_name,
        client_version,
        model: thread_config.model,
        ephemeral: thread_config.ephemeral,
        subagent_source,
        created_at,
    });
}

// Explicit opt-in is needed even when a log database enables INFO globally.
pub(crate) fn plugin_refresh_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CODEX_PLUGIN_REFRESH_TRACE")
        .is_some_and(|value| value == "1"))
}

/// Builds the hook engine for one config snapshot, including any enabled plugin hooks.
#[expect(
    clippy::expect_used,
    reason = "Hook discovery failure must not silently disable configured hooks"
)]
async fn build_hooks_for_config(
    config: &Config,
    plugins_manager: &PluginsManager,
    environment: Option<&TurnEnvironment>,
) -> Hooks {
    let (hook_shell_program, hook_shell_argv) = environment
        .and_then(|environment| environment.shell.as_ref())
        .and_then(|shell| {
            let mut argv = shell.derive_exec_args("", /*use_login_shell*/ false).ok()?;
            let program = argv.remove(0);
            let _ = argv.pop();
            Some((Some(program), argv))
        })
        .unwrap_or_default();
    let plugins_input = config.plugins_config_input();
    let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
    let plugin_hook_sources = plugin_outcome.effective_plugin_hook_sources();
    let plugin_hook_load_warnings = plugin_outcome.effective_plugin_hook_warnings();
    let hooks_config = HooksConfig {
        legacy_notify_argv: config.notify.clone(),
        mutating_finalizer: matches!(
            config.after_agent_policy,
            codex_config::config_toml::AfterAgentPolicy::MutatingFinalizer
        ),
        feature_enabled: config.features.enabled(Feature::CodexHooks),
        bypass_hook_trust: config.bypass_hook_trust,
        config_layer_stack: Some(config.config_layer_stack.as_ref().clone()),
        plugin_hook_sources,
        plugin_hook_load_warnings,
        shell_program: hook_shell_program,
        shell_args: hook_shell_argv,
    };
    tokio::task::spawn_blocking(move || Hooks::new(hooks_config))
        .await
        .expect("hook discovery worker should complete")
}



