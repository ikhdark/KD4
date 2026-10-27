//! Defines the protocol for a Codex session between a client and an agent.
//!
//! Uses a SQ (Submission Queue) / EQ (Event Queue) pattern to asynchronously communicate
//! between user and agent.

use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::ops::Mul;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use strum_macros::EnumIter;

use crate::AgentPath;
use crate::ResponseItemId;
use crate::SessionId;
use crate::ThreadId;
use crate::approvals::ElicitationRequestEvent;
use crate::capabilities::SelectedCapabilityRoot;
use crate::config_types::CollaborationMode;
use crate::config_types::ModeKind;
use crate::config_types::MultiAgentMode;
use crate::config_types::Personality;
use crate::config_types::ReasoningSummary as ReasoningSummaryConfig;
use crate::config_types::WindowsSandboxLevel;
use crate::dynamic_tools::DynamicToolCallOutputContentItem;
use crate::dynamic_tools::DynamicToolCallRequest;
use crate::dynamic_tools::DynamicToolResponse;
use crate::dynamic_tools::DynamicToolSpec;
use crate::items::TurnItem;
use crate::mcp::CallToolResult;
use crate::mcp::RequestId;
use crate::models::ActivePermissionProfile;
use crate::models::AgentMessageInputContent;
use crate::models::BaseInstructions;
use crate::models::ContentItem;
use crate::models::ImageDetail;
use crate::models::InternalChatMessageMetadataPassthrough;
use crate::models::MessagePhase;
use crate::models::PermissionProfile;
use crate::models::ResponseInputItem;
use crate::models::ResponseItem;
use crate::models::SandboxEnforcement;
use crate::models::WebSearchAction;
use crate::num_format::format_with_separators;
use crate::openai_models::ReasoningEffort as ReasoningEffortConfig;
use crate::parse_command::ParsedCommand;
use crate::plan_tool::UpdatePlanArgs;
use crate::request_permissions::RequestPermissionsEvent;
use crate::request_permissions::RequestPermissionsResponse;
use crate::request_user_input::RequestUserInputResponse;
use crate::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::Error as _;
use serde_json::Value;
use serde_with::serde_as;
use strum_macros::Display;
use tracing::error;
use ts_rs::TS;

fn deserialize_present_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

pub use crate::approvals::ApplyPatchApprovalRequestEvent;
pub use crate::approvals::ElicitationAction;
pub use crate::approvals::ExecApprovalRequestEvent;
pub use crate::approvals::ExecPolicyAmendment;
pub use crate::approvals::NetworkApprovalContext;
pub use crate::approvals::NetworkApprovalProtocol;
pub use crate::approvals::NetworkPolicyAmendment;
pub use crate::approvals::NetworkPolicyRuleAction;
pub use crate::legacy_events::HasLegacyEvent;
pub use crate::permissions::FileSystemAccessMode;
pub use crate::permissions::FileSystemPath;
pub use crate::permissions::FileSystemSandboxEntry;
pub use crate::permissions::FileSystemSandboxKind;
pub use crate::permissions::FileSystemSandboxPolicy;
pub use crate::permissions::FileSystemSpecialPath;
pub use crate::permissions::NetworkSandboxPolicy;
use crate::permissions::default_read_only_subpaths_for_writable_root;
pub use crate::request_permissions::RequestPermissionsArgs;
pub use crate::request_user_input::RequestUserInputEvent;

/// Open/close tags for special context blocks. Used across crates to avoid duplicated hardcoded
/// strings.
pub const USER_INSTRUCTIONS_OPEN_TAG: &str = "<user_instructions>";
pub const USER_INSTRUCTIONS_CLOSE_TAG: &str = "</user_instructions>";
pub const ENVIRONMENT_CONTEXT_OPEN_TAG: &str = "<environment_context>";
pub const ENVIRONMENT_CONTEXT_CLOSE_TAG: &str = "</environment_context>";
pub const APPS_INSTRUCTIONS_OPEN_TAG: &str = "<apps_instructions>";
pub const APPS_INSTRUCTIONS_CLOSE_TAG: &str = "</apps_instructions>";
pub const SKILLS_INSTRUCTIONS_OPEN_TAG: &str = "<skills_instructions>";
pub const SKILLS_INSTRUCTIONS_CLOSE_TAG: &str = "</skills_instructions>";
pub const EXTENSION_SKILLS_INSTRUCTIONS_OPEN_TAG: &str = "<extension_skills_instructions>";
pub const EXTENSION_SKILLS_INSTRUCTIONS_CLOSE_TAG: &str = "</extension_skills_instructions>";
pub const ENVIRONMENT_SKILLS_INSTRUCTIONS_OPEN_TAG: &str = "<environment_skills_instructions>";
pub const ENVIRONMENT_SKILLS_INSTRUCTIONS_CLOSE_TAG: &str = "</environment_skills_instructions>";
pub const PLUGINS_INSTRUCTIONS_OPEN_TAG: &str = "<plugins_instructions>";
pub const PLUGINS_INSTRUCTIONS_CLOSE_TAG: &str = "</plugins_instructions>";
pub const COLLABORATION_MODE_OPEN_TAG: &str = "<collaboration_mode>";
pub const COLLABORATION_MODE_CLOSE_TAG: &str = "</collaboration_mode>";
pub const MULTI_AGENT_MODE_OPEN_TAG: &str = "<multi_agent_mode>";
pub const MULTI_AGENT_MODE_CLOSE_TAG: &str = "</multi_agent_mode>";
pub const USER_MESSAGE_BEGIN: &str = "## My request for Codex:";

/// Removes the model-context prefix from a user message before displaying it.
pub fn strip_user_message_prefix(text: &str) -> &str {
    let text = text.trim();
    if let Some(request) = text.strip_prefix(USER_MESSAGE_BEGIN) {
        return request.trim();
    }
    // Desktop/IDE wrappers put the request header on its own line after a
    // recognized context preamble. A quoted marker in ordinary prose is data.
    if text.starts_with("# Context from my IDE setup:")
        || text.starts_with("# Files mentioned by the user:")
    {
        for (offset, _) in text.match_indices(USER_MESSAGE_BEGIN) {
            if text[..offset].ends_with('\n')
                && let Some(request) = text[offset..].strip_prefix(USER_MESSAGE_BEGIN)
            {
                return request.trim();
            }
        }
    }
    text
}

// TODO(anp): Replace `TurnEnvironmentSelection` with `PathUri` once path URIs carry environment
// identifiers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct TurnEnvironmentSelection {
    pub environment_id: String,
    pub cwd: PathUri,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
pub struct TurnEnvironmentSelections {
    pub legacy_fallback_cwd: AbsolutePathBuf,
    pub environments: Vec<TurnEnvironmentSelection>,
}

impl TurnEnvironmentSelections {
    pub fn new(
        legacy_fallback_cwd: AbsolutePathBuf,
        environments: Vec<TurnEnvironmentSelection>,
    ) -> Self {
        Self {
            legacy_fallback_cwd,
            environments,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
#[serde(transparent)]
#[ts(type = "string")]
pub struct GitSha(pub String);

impl GitSha {
    pub fn new(sha: &str) -> Self {
        Self(sha.to_string())
    }
}

/// Submission Queue Entry - requests from user
#[derive(Debug, Clone)]
pub struct Submission {
    /// Unique id for this Submission to correlate with Events
    pub id: String,
    /// Payload
    pub op: Op,
    /// Client-provided id for the user message represented by `Op::UserInput`.
    pub client_user_message_id: Option<String>,
    /// Optional W3C trace carrier propagated across async submission handoffs.
    pub trace: Option<W3cTraceContext>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct W3cTraceContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tracestate: Option<String>,
}

/// Config payload for refreshing MCP servers.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerRefreshConfig {
    /// Complete runtime server map after source and thread-scoped resolution.
    pub mcp_servers: Value,
    /// OAuth credential store mode to use with this server snapshot.
    pub mcp_oauth_credentials_store_mode: Value,
    pub auth_keyring_backend_kind: Value,
}

/// Persistent thread-settings overrides that can be applied before user input or
/// on their own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThreadSettingsOverrides {
    /// Updated fallback `cwd` and environments supplied together as a complete pair.
    pub environments: Option<TurnEnvironmentSelections>,

    /// Updated runtime workspace roots used to materialize symbolic
    /// `:workspace_roots` filesystem permissions.
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,

    /// Updated profile-defined workspace roots for status summaries and
    /// per-turn config reconstruction.
    pub profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,

    /// Updated command approval policy.
    pub approval_policy: Option<AskForApproval>,

    /// Updated sandbox policy for tool calls.
    pub sandbox_policy: Option<SandboxPolicy>,

    /// Updated permissions profile for tool calls.
    pub permission_profile: Option<PermissionProfile>,

    /// Named or built-in profile that produced `permission_profile`, if the
    /// update selected a profile rather than supplying raw permissions.
    pub active_permission_profile: Option<ActivePermissionProfile>,

    /// Updated Windows sandbox mode for tool execution.
    pub windows_sandbox_level: Option<WindowsSandboxLevel>,

    /// Updated model slug. When set, the model info is derived automatically.
    pub model: Option<String>,

    /// Updated reasoning effort (honored only for reasoning-capable models).
    ///
    /// Use `Some(Some(_))` to set a specific effort, `Some(None)` to clear the
    /// effort, or `None` to leave the existing value unchanged.
    pub effort: Option<Option<ReasoningEffortConfig>>,

    /// Updated reasoning summary preference (honored only for reasoning-capable models).
    pub summary: Option<ReasoningSummaryConfig>,

    /// Updated service tier preference for future turns.
    ///
    /// Use `Some(Some(_))` to set a specific tier, `Some(None)` to clear the
    /// preference, or `None` to leave the existing value unchanged.
    pub service_tier: Option<Option<String>>,

    /// EXPERIMENTAL - set a pre-set collaboration mode.
    /// Takes precedence over model, effort, and developer instructions if set.
    pub collaboration_mode: Option<CollaborationMode>,

    /// Updated personality preference.
    pub personality: Option<Personality>,
}

/// Source classification for client-supplied context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdditionalContextKind {
    Untrusted,
    Application,
}

/// Client-supplied context keyed by an opaque source identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdditionalContextEntry {
    pub value: String,
    pub kind: AdditionalContextKind,
}

/// Submission operation
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum Op {
    /// Abort current task without terminating background terminal processes.
    /// This server sends [`EventMsg::TurnAborted`] in response.
    Interrupt,

    /// Terminate all running background terminal processes for this thread.
    /// Use this when callers intentionally want to stop long-lived background shells.
    CleanBackgroundTerminals,

    /// User input, optionally with thread-settings overrides applied first.
    UserInput {
        /// User input items, see `InputItem`
        items: Vec<UserInput>,
        /// Optional JSON Schema used to constrain the final assistant message for this turn.
        final_output_json_schema: Option<Value>,
        /// Explicit permission for an exactly empty completed final answer on this input only.
        /// Does not waive tools, pending input, completion hooks, or an output schema.
        allow_no_prose: bool,
        /// Optional turn-scoped Responses API `client_metadata`.
        responsesapi_client_metadata: Option<HashMap<String, String>>,
        /// Client-supplied context fragments keyed by an opaque source identifier.
        additional_context: IndexMap<String, AdditionalContextEntry>,

        /// Persistent thread-settings overrides to apply before the input.
        thread_settings: ThreadSettingsOverrides,
    },

    /// Apply persistent thread-settings overrides without starting a turn.
    ///
    /// This uses the same submission queue as turn starts so app-server can
    /// preserve caller order between both kinds of mutation.
    ThreadSettings {
        /// Persistent thread-settings overrides to apply.
        thread_settings: ThreadSettingsOverrides,
    },

    /// Inter-agent communication that should be recorded as agent-message history
    /// while still using the normal thread submission lifecycle.
    InterAgentCommunication {
        communication: InterAgentCommunication,
    },

    /// Approve a command execution
    ExecApproval {
        /// The id of the submission we are approving
        id: String,
        /// Turn id associated with the approval event, when available.
        turn_id: Option<String>,
        /// The user's decision in response to the request.
        decision: ReviewDecision,
    },

    /// Approve a code patch
    PatchApproval {
        /// The id of the submission we are approving
        id: String,
        /// The user's decision in response to the request.
        decision: ReviewDecision,
    },

    /// Resolve an MCP elicitation request.
    ResolveElicitation {
        /// Name of the MCP server that issued the request.
        server_name: String,
        /// Request identifier from the MCP server.
        request_id: RequestId,
        /// User's decision for the request.
        decision: ElicitationAction,
        /// Structured user input supplied for accepted elicitations.
        content: Option<Value>,
        /// Optional client metadata associated with the elicitation response.
        meta: Option<Value>,
    },

    /// Resolve a request_user_input tool call.
    UserInputAnswer {
        /// Turn id for the in-flight request.
        id: String,
        /// Request item identity. Legacy turn-only answers are accepted only
        /// while the turn has issued exactly one request.
        call_id: Option<String>,
        /// User-provided answers.
        response: RequestUserInputResponse,
    },

    /// Resolve a request_permissions tool call.
    RequestPermissionsResponse {
        /// Call id for the in-flight request.
        id: String,
        /// User-granted permissions.
        response: RequestPermissionsResponse,
    },

    /// Resolve a dynamic tool call request.
    DynamicToolResponse {
        /// Originating turn, required to prevent replies crossing turn boundaries.
        turn_id: String,
        /// Call id for the in-flight request.
        id: String,
        /// Tool output payload.
        response: DynamicToolResponse,
    },

    /// Request MCP servers to reinitialize and refresh cached tool lists.
    RefreshMcpServers { config: McpServerRefreshConfig },

    /// Reload user config layer overrides for the active session.
    ///
    /// This updates runtime config-derived behavior (for example app
    /// enable/disable state) without restarting the thread.
    ReloadUserConfig,

    /// Request the agent to summarize the current conversation context.
    /// The agent will use its existing context (either conversation history or previous response id)
    /// to generate a summary which will be returned as an AgentMessage event.
    Compact,

    /// Request Codex to drop the last N user turns from in-memory context.
    ///
    /// This does not attempt to revert local filesystem changes. Clients are
    /// responsible for undoing any edits on disk.
    ThreadRollback { num_turns: u32 },

    /// Request a code review from the agent.
    Review { review_request: ReviewRequest },

    /// Request to shut down codex instance.
    Shutdown,

    /// Execute a user-initiated one-off shell command (triggered by "!cmd").
    ///
    /// The command string is executed using the user's default shell and may
    /// include shell syntax (pipes, redirects, etc.). Output is streamed via
    /// `ExecCommand*` events and the UI regains control upon `TurnComplete`.
    RunUserShellCommand {
        /// The raw command string after '!'
        command: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase", export_to = "v2/")]
pub enum ThreadHistoryMode {
    #[default]
    Legacy,
    Paginated,
}

impl ThreadHistoryMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Paginated => "paginated",
        }
    }
}

impl FromStr for ThreadHistoryMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "paginated" => Ok(Self::Paginated),
            _ => Err(format!("unknown thread history mode `{value}`")),
        }
    }
}

/// The timestamp used to order thread listings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadSortKey {
    #[default]
    #[serde(rename = "CreatedAt")]
    CreatedAt,
    #[serde(rename = "UpdatedAt")]
    UpdatedAt,
    #[serde(rename = "RecencyAt")]
    RecencyAt,
}

/// The direction used to order thread listings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDirection {
    #[serde(rename = "Asc")]
    Asc,
    #[default]
    #[serde(rename = "Desc")]
    Desc,
}

/// Spawn-graph relationship used to filter thread listings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThreadRelationFilter {
    #[serde(rename = "DirectChildrenOf")]
    DirectChildrenOf(ThreadId),
    #[serde(rename = "DescendantsOf")]
    DescendantsOf(ThreadId),
}

impl From<Vec<UserInput>> for Op {
    fn from(value: Vec<UserInput>) -> Self {
        Op::UserInput {
            items: value,
            final_output_json_schema: None,
            allow_no_prose: false,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema, TS)]
pub struct InterAgentCommunication {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub id: Option<ResponseItemId>,
    pub author: AgentPath,
    pub recipient: AgentPath,
    #[serde(default)]
    pub other_recipients: Vec<AgentPath>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub internal_chat_message_metadata_passthrough: Option<InternalChatMessageMetadataPassthrough>,
    pub trigger_turn: bool,
}

impl InterAgentCommunication {
    pub fn new(
        author: AgentPath,
        recipient: AgentPath,
        other_recipients: Vec<AgentPath>,
        content: String,
        trigger_turn: bool,
    ) -> Self {
        Self {
            id: None,
            author,
            recipient,
            other_recipients,
            content,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
            trigger_turn,
        }
    }

    pub fn new_encrypted(
        author: AgentPath,
        recipient: AgentPath,
        other_recipients: Vec<AgentPath>,
        encrypted_content: String,
        trigger_turn: bool,
    ) -> Self {
        Self {
            id: None,
            author,
            recipient,
            other_recipients,
            content: String::new(),
            encrypted_content: Some(encrypted_content),
            internal_chat_message_metadata_passthrough: None,
            trigger_turn,
        }
    }

    pub fn set_turn_id_if_missing(&mut self, turn_id: &str) {
        InternalChatMessageMetadataPassthrough::set_turn_id_if_missing(
            &mut self.internal_chat_message_metadata_passthrough,
            turn_id,
        );
    }

    pub fn to_response_input_item(&self) -> ResponseInputItem {
        let mut communication = self.clone();
        communication.id = None;
        communication.internal_chat_message_metadata_passthrough = None;
        ResponseInputItem::Message {
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: serde_json::to_string(&communication).unwrap_or_default(),
            }],
            phase: Some(MessagePhase::Commentary),
        }
    }

    pub fn to_model_input_item(&self) -> ResponseItem {
        let content = match &self.encrypted_content {
            Some(encrypted_content) => {
                let message_type = if self.trigger_turn {
                    "NEW_TASK"
                } else {
                    "MESSAGE"
                };
                vec![
                    AgentMessageInputContent::InputText {
                        text: format!(
                            "Message Type: {message_type}\nTask name: {}\nSender: {}\nPayload:\n",
                            self.recipient, self.author
                        ),
                    },
                    AgentMessageInputContent::EncryptedContent {
                        encrypted_content: encrypted_content.clone(),
                    },
                ]
            }
            None => vec![AgentMessageInputContent::InputText {
                text: self.content.clone(),
            }],
        };
        ResponseItem::AgentMessage {
            id: self.id.clone(),
            author: self.author.to_string(),
            recipient: self.recipient.to_string(),
            content,
            internal_chat_message_metadata_passthrough: self
                .internal_chat_message_metadata_passthrough
                .clone(),
        }
    }

    pub fn is_message_content(content: &[ContentItem]) -> bool {
        Self::from_message_content(content).is_some()
    }

    pub fn from_message_content(content: &[ContentItem]) -> Option<Self> {
        match content {
            [ContentItem::InputText { text }] | [ContentItem::OutputText { text }] => {
                serde_json::from_str(text).ok()
            }
            _ => None,
        }
    }
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::CleanBackgroundTerminals => "clean_background_terminals",
            Self::UserInput { .. } => "user_input",
            Self::ThreadSettings { .. } => "thread_settings",
            Self::InterAgentCommunication { .. } => "inter_agent_communication",
            Self::ExecApproval { .. } => "exec_approval",
            Self::PatchApproval { .. } => "patch_approval",
            Self::ResolveElicitation { .. } => "resolve_elicitation",
            Self::UserInputAnswer { .. } => "user_input_answer",
            Self::RequestPermissionsResponse { .. } => "request_permissions_response",
            Self::DynamicToolResponse { .. } => "dynamic_tool_response",
            Self::RefreshMcpServers { .. } => "refresh_mcp_servers",
            Self::ReloadUserConfig => "reload_user_config",
            Self::Compact => "compact",
            Self::ThreadRollback { .. } => "thread_rollback",
            Self::Review { .. } => "review",
            Self::Shutdown => "shutdown",
            Self::RunUserShellCommand { .. } => "run_user_shell_command",
        }
    }
}

/// Determines the conditions under which the user is consulted to approve
/// running the command proposed by Codex.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Display,
    JsonSchema,
    TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum AskForApproval {
    /// Under this policy, only "known safe" commands—as determined by
    /// `is_safe_command()`—that **only read files** are auto‑approved.
    /// Everything else will ask the user to approve.
    #[serde(rename = "untrusted")]
    #[strum(serialize = "untrusted")]
    UnlessTrusted,

    /// The model decides when to ask the user for approval.
    #[serde(alias = "on-failure")]
    #[default]
    OnRequest,

    /// Fine-grained controls for individual approval flows.
    ///
    /// When a field is `true`, commands in that category are allowed. When it
    /// is `false`, those requests are automatically rejected instead of shown
    /// to the user.
    #[strum(serialize = "granular")]
    Granular(GranularApprovalConfig),

    /// Never ask the user to approve commands. Failures are immediately returned
    /// to the model, and never escalated to the user for approval.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
pub struct GranularApprovalConfig {
    /// Whether to allow shell command approval requests, including inline
    /// `with_additional_permissions` and `require_escalated` requests.
    pub sandbox_approval: bool,
    /// Whether to allow prompts triggered by execpolicy `prompt` rules.
    pub rules: bool,
    /// Whether to allow approval prompts triggered by skill script execution.
    #[serde(default)]
    pub skill_approval: bool,
    /// Whether to allow prompts triggered by the `request_permissions` tool.
    #[serde(default)]
    pub request_permissions: bool,
    /// Whether to allow MCP elicitation prompts.
    pub mcp_elicitations: bool,
}

impl GranularApprovalConfig {
    pub const fn allows_sandbox_approval(self) -> bool {
        self.sandbox_approval
    }

    pub const fn allows_rules_approval(self) -> bool {
        self.rules
    }

    pub const fn allows_skill_approval(self) -> bool {
        self.skill_approval
    }

    pub const fn allows_request_permissions(self) -> bool {
        self.request_permissions
    }

    pub const fn allows_mcp_elicitations(self) -> bool {
        self.mcp_elicitations
    }
}

/// Represents whether outbound network access is available to the agent.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, Default, JsonSchema, TS,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}

impl NetworkAccess {
    pub fn is_enabled(self) -> bool {
        matches!(self, NetworkAccess::Enabled)
    }
}

/// Determines execution restrictions for model shell commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Display, JsonSchema, TS)]
#[strum(serialize_all = "kebab-case")]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SandboxPolicy {
    /// No restrictions whatsoever. Use with caution.
    #[serde(rename = "danger-full-access")]
    DangerFullAccess,

    /// Read-only access configuration.
    #[serde(rename = "read-only")]
    ReadOnly {
        /// When set to `true`, outbound network access is allowed. `false` by
        /// default.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        network_access: bool,
    },

    /// Indicates the process is already in an external sandbox. Allows full
    /// disk access while honoring the provided network setting.
    #[serde(rename = "external-sandbox")]
    ExternalSandbox {
        /// Whether the external sandbox permits outbound network traffic.
        #[serde(default)]
        network_access: NetworkAccess,
    },

    /// Same as `ReadOnly` but additionally grants write access to the current
    /// working directory ("workspace").
    #[serde(rename = "workspace-write")]
    WorkspaceWrite {
        /// Additional folders (beyond cwd and possibly TMPDIR) that should be
        /// writable from within the sandbox.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        writable_roots: Vec<AbsolutePathBuf>,

        /// When set to `true`, outbound network access is allowed. `false` by
        /// default.
        #[serde(default)]
        network_access: bool,

        /// When set to `true`, will NOT include the per-user `TMPDIR`
        /// environment variable among the default writable roots. Defaults to
        /// `false`.
        #[serde(default)]
        exclude_tmpdir_env_var: bool,

        /// Legacy persisted-data field for the retired `/tmp` writable root.
        /// It is accepted but has no effect on Windows. Defaults to `false`.
        #[serde(default)]
        exclude_slash_tmp: bool,
    },
}

/// A writable root path accompanied by a list of subpaths that should remain
/// read‑only even when the root is writable. This is primarily used to ensure
/// that folders containing files that could be modified to escalate the
/// privileges of the agent (e.g. `.codex`, `.git`, notably `.git/hooks`) under
/// a writable root are not modified by the agent.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct WritableRoot {
    pub root: AbsolutePathBuf,

    /// By construction, these subpaths are all under `root`.
    pub read_only_subpaths: Vec<AbsolutePathBuf>,

    /// Workspace metadata path names that must not be created or replaced under
    /// `root` unless the policy grants an explicit write rule for that metadata
    /// path.
    pub protected_metadata_names: Vec<String>,
}

impl WritableRoot {
    pub fn is_path_writable(&self, path: &Path) -> bool {
        // Check if the path is under the root.
        if !path.starts_with(&self.root) {
            return false;
        }

        // Check if the path is under any of the read-only subpaths.
        for subpath in &self.read_only_subpaths {
            if crate::permissions::path_is_within_denied_root(path, subpath.as_path()) {
                return false;
            }
        }

        if self.path_contains_protected_metadata_name(path) {
            return false;
        }

        true
    }

    fn path_contains_protected_metadata_name(&self, path: &Path) -> bool {
        let Ok(relative_path) = path.strip_prefix(&self.root) else {
            return false;
        };

        let Some(first_component) = relative_path.components().next() else {
            return false;
        };

        self.protected_metadata_names.iter().any(|name| {
            crate::permissions::path_component_matches(
                first_component.as_os_str(),
                std::ffi::OsStr::new(name),
            )
        })
    }
}

impl FromStr for SandboxPolicy {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl FromStr for FileSystemSandboxPolicy {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl FromStr for NetworkSandboxPolicy {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

impl SandboxPolicy {
    /// Returns a policy with read-only disk access and no network.
    pub fn new_read_only_policy() -> Self {
        SandboxPolicy::ReadOnly {
            network_access: false,
        }
    }

    /// Returns a policy that can read the entire disk, but can only write to
    /// the current working directory and configured Windows temporary roots.
    /// It does not allow network access.
    pub fn new_workspace_write_policy() -> Self {
        SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        }
    }

    pub fn has_full_disk_read_access(&self) -> bool {
        true
    }

    pub fn has_full_disk_write_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { .. } => true,
            SandboxPolicy::ReadOnly { .. } => false,
            SandboxPolicy::WorkspaceWrite { .. } => false,
        }
    }

    pub fn has_full_network_access(&self) -> bool {
        match self {
            SandboxPolicy::DangerFullAccess => true,
            SandboxPolicy::ExternalSandbox { network_access } => network_access.is_enabled(),
            SandboxPolicy::ReadOnly { network_access, .. } => *network_access,
            SandboxPolicy::WorkspaceWrite { network_access, .. } => *network_access,
        }
    }

    /// Returns the list of writable roots (tailored to the current working
    /// directory) together with subpaths that should remain read‑only under
    /// each writable root.
    pub fn get_writable_roots_with_cwd(&self, cwd: &Path) -> Vec<WritableRoot> {
        match self {
            SandboxPolicy::DangerFullAccess => Vec::new(),
            SandboxPolicy::ExternalSandbox { .. } => Vec::new(),
            SandboxPolicy::ReadOnly { .. } => Vec::new(),
            SandboxPolicy::WorkspaceWrite {
                writable_roots,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                network_access: _,
            } => {
                // Start from explicitly configured writable roots.
                let mut roots: Vec<AbsolutePathBuf> = writable_roots.clone();

                // Always include the Windows working directory. The legacy
                // exclusion fields remain readable for persisted policy data.
                // TODO(mbolin): cwd param should be AbsolutePathBuf.
                let cwd_absolute = AbsolutePathBuf::from_absolute_path(cwd);
                match cwd_absolute {
                    Ok(cwd) => {
                        roots.push(cwd);
                    }
                    Err(e) => {
                        error!(
                            "Ignoring invalid cwd {:?} for sandbox writable root: {}",
                            cwd, e
                        );
                    }
                }

                let _ = exclude_slash_tmp;

                // Include the configured temporary directory unless explicitly excluded.
                // TMPDIR is not guaranteed to be defined on Windows, but
                // supporting it gives users a configurable temporary root.
                if !exclude_tmpdir_env_var
                    && let Some(tmpdir) = std::env::var_os("TMPDIR")
                    && !tmpdir.is_empty()
                {
                    match AbsolutePathBuf::from_absolute_path(PathBuf::from(&tmpdir)) {
                        Ok(tmpdir_path) => {
                            roots.push(tmpdir_path);
                        }
                        Err(e) => {
                            error!(
                                "Ignoring invalid TMPDIR value {tmpdir:?} for sandbox writable root: {e}",
                            );
                        }
                    }
                }

                // For each root, compute subpaths that should remain read-only.
                let cwd_root = AbsolutePathBuf::from_absolute_path(cwd).ok();
                roots
                    .into_iter()
                    .map(|writable_root| {
                        let protect_missing_dot_codex = cwd_root
                            .as_ref()
                            .is_some_and(|cwd_root| cwd_root == &writable_root);
                        WritableRoot {
                            read_only_subpaths: default_read_only_subpaths_for_writable_root(
                                &writable_root,
                                protect_missing_dot_codex,
                            ),
                            protected_metadata_names: Vec::new(),
                            root: writable_root,
                        }
                    })
                    .collect()
            }
        }
    }
}

/// Event Queue Entry - events from agent
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    /// Submission `id` that this event is correlated with.
    pub id: String,
    /// Payload
    pub msg: EventMsg,
}

#[derive(Debug, Clone, Deserialize, Serialize, Display, JsonSchema, TS)]
// This public wire enum has hundreds of construction and match sites. Boxing
// TurnComplete would be an API-wide migration for a layout-only optimization.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
#[strum(serialize_all = "snake_case")]
pub enum EventMsg {
    /// Error while executing a submission
    Error(ErrorEvent),

    /// Warning issued while processing a submission. Unlike `Error`, this
    /// indicates the turn continued but the user should still be notified.
    Warning(WarningEvent),

    /// Model routing changed from the requested model to a different model.
    ModelReroute(ModelRerouteEvent),

    /// Backend recommends additional account verification for this turn.
    ModelVerification(ModelVerificationEvent),

    /// Backend moderation metadata intended for first-party turn presentation.
    TurnModerationMetadata(TurnModerationMetadataEvent),

    /// Backend indicates that response output is waiting on a safety review.
    SafetyBuffering(SafetyBufferingEvent),

    /// Conversation history was compacted (either automatically or manually).
    ContextCompacted(ContextCompactedEvent),

    /// Conversation history was rolled back by dropping the last N user turns.
    ThreadRolledBack(ThreadRolledBackEvent),

    /// Agent has started a turn.
    /// v1 wire format uses `task_started`; accept `turn_started` for v2 interop.
    #[serde(rename = "task_started", alias = "turn_started")]
    TurnStarted(TurnStartedEvent),

    /// Current owner-authored harness waits; transient, never reconstructed from timing.
    TurnPhaseChanged(TurnPhaseChangedEvent),

    /// Persistent thread-settings overrides from the correlated submission have
    /// been applied to the session configuration.
    ThreadSettingsApplied(ThreadSettingsAppliedEvent),

    /// Agent has completed all actions.
    /// v1 wire format uses `task_complete`; accept `turn_complete` for v2 interop.
    #[serde(rename = "task_complete", alias = "turn_complete")]
    TurnComplete(TurnCompleteEvent),

    /// Closes imported historical context without asserting source completion.
    /// Completion timestamps, errors, and native execution timings are absent.
    ImportedTurnClosed(TurnCompleteEvent),

    /// Usage update for the current session, including totals and last turn.
    /// Optional means unknown — UIs should not display when `None`.
    TokenCount(TokenCountEvent),

    /// Agent text output message
    AgentMessage(AgentMessageEvent),

    /// User/system input message (what was sent to the model)
    UserMessage(UserMessageEvent),

    /// Reasoning event from agent.
    AgentReasoning(AgentReasoningEvent),

    /// Raw chain-of-thought from agent.
    AgentReasoningRawContent(AgentReasoningRawContentEvent),

    /// Signaled when the model begins a new reasoning summary section (e.g., a new titled block).
    AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent),

    /// Ack the client's configure message.
    SessionConfigured(SessionConfiguredEvent),

    /// Legacy goal metadata, retained only to decode existing rollouts.
    ThreadGoalUpdated(ThreadGoalUpdatedEvent),

    /// Incremental MCP startup progress updates.
    McpStartupUpdate(McpStartupUpdateEvent),

    /// Aggregate MCP startup completion summary.
    McpStartupComplete(McpStartupCompleteEvent),

    McpToolCallBegin(McpToolCallBeginEvent),

    McpToolCallEnd(McpToolCallEndEvent),

    McpToolCallProgress(McpToolCallProgressEvent),

    WebSearchBegin(WebSearchBeginEvent),

    WebSearchEnd(WebSearchEndEvent),

    ImageGenerationBegin(ImageGenerationBeginEvent),

    ImageGenerationEnd(ImageGenerationEndEvent),

    /// Notification that the server is about to execute a command.
    ExecCommandBegin(ExecCommandBeginEvent),

    /// Incremental chunk of output from a running command.
    ExecCommandOutputDelta(ExecCommandOutputDeltaEvent),

    /// Terminal interaction for an in-progress command (stdin sent and stdout observed).
    TerminalInteraction(TerminalInteractionEvent),

    ExecCommandEnd(ExecCommandEndEvent),

    /// Notification that the agent attached a local image via the view_image tool.
    ViewImageToolCall(ViewImageToolCallEvent),

    ExecApprovalRequest(ExecApprovalRequestEvent),

    RequestPermissions(RequestPermissionsEvent),

    RequestUserInput(RequestUserInputEvent),

    DynamicToolCallRequest(DynamicToolCallRequest),

    DynamicToolCallResponse(DynamicToolCallResponseEvent),

    ElicitationRequest(ElicitationRequestEvent),

    ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent),

    /// Notification advising the user that something they are using has been
    /// deprecated and should be phased out.
    DeprecationNotice(DeprecationNoticeEvent),

    /// Notification that a model stream experienced an error or disconnect
    /// and the system is handling it (e.g., retrying with backoff).
    StreamError(StreamErrorEvent),

    /// Notification that the agent is about to apply a code patch. Mirrors
    /// `ExecCommandBegin` so front‑ends can show progress indicators.
    PatchApplyBegin(PatchApplyBeginEvent),

    /// Latest model-generated structured changes for an `apply_patch` call.
    PatchApplyUpdated(PatchApplyUpdatedEvent),

    /// Notification that a patch application has finished.
    PatchApplyEnd(PatchApplyEndEvent),

    TurnDiff(TurnDiffEvent),

    PlanUpdate(UpdatePlanArgs),

    TurnAborted(TurnAbortedEvent),

    /// Notification that the agent is shutting down.
    ShutdownComplete,

    /// Entered review mode.
    EnteredReviewMode(EnteredReviewModeEvent),

    /// Exited review mode with an optional final result to apply.
    ExitedReviewMode(ExitedReviewModeEvent),

    RawResponseItem(RawResponseItemEvent),

    ItemStarted(ItemStartedEvent),
    ItemCompleted(ItemCompletedEvent),
    HookStarted(HookStartedEvent),
    HookCompleted(HookCompletedEvent),

    AgentMessageContentDelta(AgentMessageContentDeltaEvent),
    PlanDelta(PlanDeltaEvent),
    ReasoningContentDelta(ReasoningContentDeltaEvent),
    ReasoningRawContentDelta(ReasoningRawContentDeltaEvent),

    /// Collab interaction: agent spawn begin.
    CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent),
    /// Collab interaction: agent spawn end.
    CollabAgentSpawnEnd(CollabAgentSpawnEndEvent),
    /// Collab interaction: agent interaction begin.
    CollabAgentInteractionBegin(CollabAgentInteractionBeginEvent),
    /// Collab interaction: agent interaction end.
    CollabAgentInteractionEnd(CollabAgentInteractionEndEvent),
    /// Collab interaction: waiting begin.
    CollabWaitingBegin(CollabWaitingBeginEvent),
    /// Collab interaction: waiting end.
    CollabWaitingEnd(CollabWaitingEndEvent),
    /// Collab interaction: close begin.
    CollabCloseBegin(CollabCloseBeginEvent),
    /// Collab interaction: close end.
    CollabCloseEnd(CollabCloseEndEvent),
    /// Collab interaction: resume begin.
    CollabResumeBegin(CollabResumeBeginEvent),
    /// Collab interaction: resume end.
    CollabResumeEnd(CollabResumeEndEvent),

    /// Path-based v2 sub-agent activity.
    SubAgentActivity(SubAgentActivityEvent),
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS, EnumIter)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    UserPromptSubmit,
    SubagentStart,
    SubagentStop,
    Stop,
    Interrupt,
}

impl HookEventName {
    /// Every supported event, in declaration order.
    pub fn all() -> impl Iterator<Item = Self> {
        <Self as strum::IntoEnumIterator>::iter()
    }

    /// Stable display and telemetry label for this hook event.
    pub const fn as_pascal_case_label(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
            Self::Interrupt => "Interrupt",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookHandlerType {
    Command,
    Prompt,
    Agent,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookExecutionMode {
    Sync,
    Async,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookScope {
    Thread,
    Turn,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookSource {
    System,
    User,
    Project,
    Mdm,
    SessionFlags,
    Plugin,
    CloudRequirements,
    CloudManagedConfig,
    LegacyManagedConfigFile,
    LegacyManagedConfigMdm,
    #[default]
    Unknown,
}

impl HookSource {
    /// Stable configuration and telemetry label for this hook source.
    pub const fn as_snake_case_label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Project => "project",
            Self::Mdm => "mdm",
            Self::SessionFlags => "session_flags",
            Self::Plugin => "plugin",
            Self::CloudRequirements => "cloud_requirements",
            Self::CloudManagedConfig => "cloud_managed_config",
            Self::LegacyManagedConfigFile => "legacy_managed_config_file",
            Self::LegacyManagedConfigMdm => "legacy_managed_config_mdm",
            Self::Unknown => "unknown",
        }
    }
}


#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookTrustStatus {
    Managed,
    Untrusted,
    Trusted,
    Modified,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookRunStatus {
    Running,
    Completed,
    Failed,
    Blocked,
    Stopped,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum HookOutputEntryKind {
    Warning,
    Stop,
    Feedback,
    Context,
    Error,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub struct HookOutputEntry {
    pub kind: HookOutputEntryKind,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub struct HookRunSummary {
    pub id: String,
    pub event_name: HookEventName,
    pub handler_type: HookHandlerType,
    pub execution_mode: HookExecutionMode,
    pub scope: HookScope,
    pub source_path: AbsolutePathBuf,
    #[serde(default)]
    pub source: HookSource,
    pub display_order: i64,
    pub status: HookRunStatus,
    pub status_message: Option<String>,
    #[ts(type = "number")]
    pub started_at: i64,
    #[ts(type = "number | null")]
    pub completed_at: Option<i64>,
    #[ts(type = "number | null")]
    pub duration_ms: Option<i64>,
    pub entries: Vec<HookOutputEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub struct HookStartedEvent {
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub struct HookCompletedEvent {
    pub turn_id: Option<String>,
    pub run: HookRunSummary,
}

impl From<CollabAgentSpawnBeginEvent> for EventMsg {
    fn from(event: CollabAgentSpawnBeginEvent) -> Self {
        EventMsg::CollabAgentSpawnBegin(event)
    }
}

impl From<CollabAgentSpawnEndEvent> for EventMsg {
    fn from(event: CollabAgentSpawnEndEvent) -> Self {
        EventMsg::CollabAgentSpawnEnd(event)
    }
}

impl From<CollabAgentInteractionBeginEvent> for EventMsg {
    fn from(event: CollabAgentInteractionBeginEvent) -> Self {
        EventMsg::CollabAgentInteractionBegin(event)
    }
}

impl From<CollabAgentInteractionEndEvent> for EventMsg {
    fn from(event: CollabAgentInteractionEndEvent) -> Self {
        EventMsg::CollabAgentInteractionEnd(event)
    }
}

impl From<CollabWaitingBeginEvent> for EventMsg {
    fn from(event: CollabWaitingBeginEvent) -> Self {
        EventMsg::CollabWaitingBegin(event)
    }
}

impl From<CollabWaitingEndEvent> for EventMsg {
    fn from(event: CollabWaitingEndEvent) -> Self {
        EventMsg::CollabWaitingEnd(event)
    }
}

impl From<CollabCloseBeginEvent> for EventMsg {
    fn from(event: CollabCloseBeginEvent) -> Self {
        EventMsg::CollabCloseBegin(event)
    }
}

impl From<CollabCloseEndEvent> for EventMsg {
    fn from(event: CollabCloseEndEvent) -> Self {
        EventMsg::CollabCloseEnd(event)
    }
}

impl From<CollabResumeBeginEvent> for EventMsg {
    fn from(event: CollabResumeBeginEvent) -> Self {
        EventMsg::CollabResumeBegin(event)
    }
}

impl From<CollabResumeEndEvent> for EventMsg {
    fn from(event: CollabResumeEndEvent) -> Self {
        EventMsg::CollabResumeEnd(event)
    }
}

impl From<SubAgentActivityEvent> for EventMsg {
    fn from(event: SubAgentActivityEvent) -> Self {
        EventMsg::SubAgentActivity(event)
    }
}

/// Agent lifecycle status, derived from emitted events.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS, Default)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent is waiting for initialization.
    #[default]
    PendingInit,
    /// Agent is currently running.
    Running,
    /// Agent's current turn was interrupted and it may receive more input.
    Interrupted,
    /// Agent is done. Contains the final assistant message.
    Completed(Option<String>),
    /// Agent completed by surfacing an authoritative typed tool result.
    CompletedWithSurface {
        last_agent_message: Option<String>,
        surfaced_result: SurfacedToolResult,
    },
    /// Execution stopped at a capacity boundary. Acceptance is independently
    /// owned by task receipts; preserve the report even without acceptance.
    Stopped {
        last_agent_message: Option<String>,
        stop_cause: TurnExecutionStopCause,
    },
    /// Agent encountered an error.
    Errored(String),
    /// Agent has been shutdown.
    Shutdown,
    /// Agent is not found.
    NotFound,
}

/// Turn kinds that reject same-turn steering.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum NonSteerableTurnKind {
    Review,
    Compact,
}

/// Codex errors that we expose to clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum CodexErrorInfo {
    ContextWindowExceeded,
    UsageLimitExceeded,
    ServerOverloaded,
    CyberPolicy,
    HttpConnectionFailed {
        http_status_code: Option<u16>,
    },
    /// Failed to connect to the response SSE stream.
    ResponseStreamConnectionFailed {
        http_status_code: Option<u16>,
    },
    InternalServerError,
    Unauthorized,
    BadRequest,
    SandboxError,
    /// The response SSE stream disconnected in the middle of a turnbefore completion.
    ResponseStreamDisconnected {
        http_status_code: Option<u16>,
    },
    /// Reached the retry limit for responses.
    ResponseTooManyFailedAttempts {
        http_status_code: Option<u16>,
    },
    /// Returned when `turn/start` or `turn/steer` is submitted while the current active turn
    /// cannot accept same-turn steering, for example `/review` or manual `/compact`.
    ActiveTurnNotSteerable {
        turn_kind: NonSteerableTurnKind,
    },
    ThreadRollbackFailed,
    #[serde(alias = "session_budget_exceeded")]
    Other,
}

impl CodexErrorInfo {
    /// Whether this error should mark the current turn as failed when replaying history.
    pub fn affects_turn_status(&self) -> bool {
        match self {
            Self::ThreadRollbackFailed | Self::ActiveTurnNotSteerable { .. } => false,
            Self::ContextWindowExceeded
            | Self::UsageLimitExceeded
            | Self::ServerOverloaded
            | Self::CyberPolicy
            | Self::HttpConnectionFailed { .. }
            | Self::ResponseStreamConnectionFailed { .. }
            | Self::InternalServerError
            | Self::Unauthorized
            | Self::BadRequest
            | Self::SandboxError
            | Self::ResponseStreamDisconnected { .. }
            | Self::ResponseTooManyFailedAttempts { .. }
            | Self::Other => true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct RawResponseItemEvent {
    pub item: ResponseItem,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ItemStartedEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub item: TurnItem,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ItemCompletedEvent {
    pub thread_id: ThreadId,
    pub turn_id: String,
    pub item: TurnItem,
    // Old rollout files may contain ItemCompleted events for PlanItem without
    // this field. Default to 0 so those persisted rollouts still deserialize
    // after tightening the core event contract.
    #[serde(default = "default_item_completed_at_ms")]
    pub completed_at_ms: i64,
}

const fn default_item_completed_at_ms() -> i64 {
    0
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct AgentMessageContentDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct PlanDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ReasoningContentDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
    // load with default value so it's backward compatible with the old format.
    #[serde(default)]
    pub summary_index: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema)]
pub struct ReasoningRawContentDeltaEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
    // load with default value so it's backward compatible with the old format.
    #[serde(default)]
    pub content_index: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct EnteredReviewModeEvent {
    pub target: ReviewTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub user_facing_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub item_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ExitedReviewModeEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub item_id: Option<String>,
    pub review_output: Option<ReviewOutputEvent>,
}

// Individual event payload types matching each `EventMsg` variant.

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct ErrorEvent {
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<CodexErrorInfo>,
}

impl ErrorEvent {
    /// Whether this error should mark the current turn as failed when replaying history.
    pub fn affects_turn_status(&self) -> bool {
        self.codex_error_info
            .as_ref()
            .is_none_or(CodexErrorInfo::affects_turn_status)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct WarningEvent {
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ModelRerouteReason {
    HighRiskCyberActivity,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct ModelRerouteEvent {
    pub from_model: String,
    pub to_model: String,
    pub reason: ModelRerouteReason,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ModelVerification {
    TrustedAccessForCyber,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct ModelVerificationEvent {
    pub verifications: Vec<ModelVerification>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct TurnModerationMetadataEvent {
    pub metadata: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct SafetyBufferingEvent {
    pub model: String,
    pub use_cases: Vec<String>,
    pub reasons: Vec<String>,
    pub show_buffering_ui: bool,
    pub faster_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ContextCompactedEvent;

/// Accepted user-facing answer from one completion attempt. Kept separately
/// from last-visible-message evidence, which may include commentary or errors.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnFinalAnswer {
    pub message: Option<AcceptedFinalMessage>,
    /// Absent in older records. Final-specific observations, not task acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tail: Option<TurnFinalTail>,
}

/// Version 1 counts Unicode scalar values, UTF-8 bytes and nonempty Unicode
/// whitespace-separated words in the canonical final, once. No token estimate
/// is inferred. Times are monotonic offsets from this core turn's start.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnFinalTail {
    pub measurement_version: u16,
    pub item_id: Option<String>,
    pub canonical_sha256: Option<String>,
    #[ts(type = "number | null")]
    pub text_codepoints: Option<u64>,
    #[ts(type = "number | null")]
    pub text_utf8_bytes: Option<u64>,
    #[ts(type = "number | null")]
    pub whitespace_words: Option<u64>,
    #[ts(type = "number | null")]
    pub first_content_host_ms: Option<u64>,
    #[ts(type = "number | null")]
    pub full_content_host_ms: Option<u64>,
    /// Host observation of the admitted final candidate, not proof that every
    /// user obligation or external evidence source was reconciled.
    #[ts(type = "number | null")]
    pub reconciled_host_ms: Option<u64>,
    /// Unknown unless an authoritative task oracle supplies required evidence;
    /// never substitute the most recent tool, status call, or plan completion.
    pub required_evidence: Option<TurnRequiredEvidenceFrontier>,
    /// Requires a capable native client. Server writer receipts are NOT render
    /// acknowledgments and cannot populate this field.
    pub native_render_ack: Option<TurnFinalRenderAck>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnRequiredEvidenceFrontier {
    pub provenance: String,
    pub source_snapshot: String,
    /// Oracle-owned references; not final-answer prose.
    pub result_ids: Vec<String>,
    #[ts(type = "number")]
    pub available_host_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnFinalRenderAck {
    pub client_id: String,
    pub canonical_sha256: String,
    pub clock_domain: String,
    #[ts(type = "number")]
    pub rendered_at_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AcceptedFinalMessage {
    pub item_id: String,
    pub text: String,
    pub origin: AcceptedFinalMessageOrigin,
    pub completion_attempt_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum AcceptedFinalMessageOrigin {
    AssistantFinal,
    LegacyUnphased,
    Plan,
}

/// Immutable wall-clock profile captured at the core terminal boundary.
///
/// `exclusive` is the canonical elapsed-time partition. Values in `unions`
/// and `local` may overlap and must not be added to that partition.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTiming {
    /// Scheduling cause, not an acceptance verdict. Absent on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stop_cause: Option<TurnExecutionStopCause>,
    /// Bounded occurrence intervals in a named process clock domain. Missing
    /// evidence must not be reconstructed from scalar totals or wall epochs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub activity_timeline: Option<crate::timing::TurnTimingTimeline>,
    /// Accepted answer identity frozen after completion hooks and pending-input
    /// admission. Some with no message explicitly means no accepted answer;
    /// absence is legacy unknown. This is not the last visible/partial message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub final_answer: Option<TurnFinalAnswer>,
    /// Completion diagnostics captured at the terminal boundary. These are
    /// independent of execution status and timing validity; empty diagnostics
    /// are not proof that the user's entire task was verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub completion_assessment: Option<TurnCompletionAssessment>,
    /// Observed starting minus ending account credit balance, not an isolated
    /// invoice: concurrent turns, replenishments, or delayed reports may affect it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub credit_delta: Option<String>,
    /// Content identity of tracked and non-ignored untracked files captured
    /// before this turn's tools ran. Absent means capture was unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub checkout_snapshot_sha256: Option<String>,
    pub schema_version: u16,
    pub profile_valid: bool,
    pub classification_complete: bool,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub started_at_unix_ms: Option<i64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub completed_at_unix_ms: Option<i64>,
    pub inclusive_duration_ns: u64,
    pub inclusive_duration_ms: u64,
    /// Inclusive duration minus only the interactive-only wait partition.
    pub machine_duration_ns: u64,
    pub machine_duration_ms: u64,
    pub exclusive: TurnTimingExclusive,
    pub unions: TurnTimingUnions,
    pub local: TurnTimingLocal,
    pub milestones: TurnTimingMilestones,
    pub counters: TurnTimingCounters,
    /// Legacy timings from the retired turn-finalization pipeline.
    /// Absent or null means not measured; current runtimes omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub terminalization: Option<TurnTimingTerminalization>,
    /// Per-sampling-request milestones, ordered by request. Times are offsets
    /// from turn start and are diagnostic only; they are not additive buckets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_requests: Vec<TurnTimingModelRequest>,
    /// True qualifies all history-derived purpose, failure, wait-signature and
    /// nonprogress aggregates as retained-tail lower bounds. None is legacy unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub model_request_history_truncated: Option<bool>,
    /// Frozen-core duration excludes subsequent detail export and event enqueue;
    /// it is not a client-delivery or terminal-publication latency measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub terminal_publication_duration_excluded: Option<bool>,
    /// Bounded per-call relay timings captured at delivery. Offsets are from
    /// turn start; phase durations are diagnostics and are not additive with
    /// the canonical `exclusive` partition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TurnTimingToolCall>,
    /// Optional full per-call lifecycle detail in the thread's tool-output
    /// artifact store. Subject to that store's retention policy; aggregate
    /// timings and per-call summary fields remain usable without the artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_call_details_artifact_id: Option<String>,
    #[serde(default)]
    pub tool_call_timing_overflow: u32,
    /// Exact accepted-call closure ledger. A terminal event is publishable only
    /// when every accepted direct and nested execution has timing, a terminal
    /// outcome, and exactly one persisted projection.
    #[serde(default = "TurnTimingToolClosure::unmeasured")]
    pub tool_closure: TurnTimingToolClosure,
    /// Provider usage observed on generations with no recorded progress kinds
    /// and unchanged relevant state. A changed next action alone is not progress
    /// under this predicate; useful inference or diagnosis may still occur.
    /// Final-answer publication is excluded. This observational diagnostic is
    /// neither measured avoidable savings nor authority to stop a turn.
    #[serde(default)]
    pub observational_nonprogress_tokens: TurnTimingDiagnosticTokenAggregate,
    /// Latency observed on the same no-recorded-progress, unchanged-state
    /// generations as `observational_nonprogress_tokens`. Model-stream wait
    /// remains a model-side wall-clock measurement; `decision_latency_ns`
    /// narrows that to dispatch-to-first-actionable-output where observable.
    #[serde(default)]
    pub observational_nonprogress_latency: TurnTimingDiagnosticLatencyAggregate,
    /// Bounded, aggregated host-managed continuation receipts. Resource
    /// identities are hashes; raw paths and payloads are never included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deterministic_continuation_receipts: Vec<TurnTimingDeterministicContinuationReceipt>,
    #[serde(default)]
    pub deterministic_continuation_receipt_overflow: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pre_first_model_output: Option<TurnTimingPreFirstModelOutput>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export_to = "v2/")]
pub enum TurnExecutionStopCause {
    SubagentContextBudget,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnCompletionAssessment {
    /// Versioned declared-scope verdict, independent of diagnostic severity.
    /// Missing on older records means unavailable, never inferred complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub task_disposition: Option<TurnTaskDisposition>,
    /// Bounded unresolved questions for resume. Never passing proof or live handles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub validation_uncertainty: Option<TurnValidationUncertainty>,
    pub failed_checks: Vec<String>,
    pub verification_gaps: Vec<String>,
    pub advisories: Vec<String>,
}

/// Host assessment of a finite declared contract, not all natural-language intent.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[ts(export_to = "v2/")]
pub struct TurnTaskDisposition {
    pub schema_version: u32,
    pub plan_revision: Option<String>,
    pub input_revision: Option<String>,
    pub mutation_revision: u64,
    pub request_state: String,
    pub artifact_state: String,
    pub terminal_action: String,
    pub unmet_obligations: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub authority: String,
}

impl TurnCompletionAssessment {
    /// Material diagnostics only; advisories are not failed task requirements.
    pub fn verification_warning(&self) -> Option<String> {
        (!self.failed_checks.is_empty() || !self.verification_gaps.is_empty()).then(|| {
            format!(
                "{} This turn is not a verified completion of that scope.",
                self.failed_checks
                    .iter()
                    .chain(&self.verification_gaps)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnValidationUncertainty {
    pub version: u8,
    pub failed_checks: Vec<String>,
    pub pending_checks: Vec<String>,
    pub capture_incomplete: bool,
}

impl TurnCompletionAssessment {
    /// Bounded host observations, never acceptance or execution authority.
    pub fn bounded_summary(&self, turn_id: &str) -> Option<String> {
        if self.failed_checks.is_empty()
            && self.verification_gaps.is_empty()
            && self.advisories.is_empty()
        {
            return None;
        }
        let mut result = format!(
            "Host completion observations for turn {turn_id} (not acceptance or test authorization). Missing attribution does not require tests; follow applicable user instructions, including intentional omission. Input attribution is not proof that changed behavior was exercised. Saved process identities are not live or callable handles."
        );
        for (label, entries) in [
            ("failed checks", &self.failed_checks),
            ("verification gaps", &self.verification_gaps),
            ("advisories", &self.advisories),
        ] {
            if entries.is_empty() {
                continue;
            }
            result.push_str(&format!("\n{label} ({}):", entries.len()));
            for entry in entries.iter().take(4) {
                let text = entry.chars().take(192).collect::<String>();
                result.push_str(&format!("\n- {text}"));
                if text.len() < entry.len() {
                    result.push_str(" [truncated]");
                }
            }
            if entries.len() > 4 {
                result.push_str("\n[additional observations omitted; see the turn timing record]");
            }
        }
        Some(result)
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingToolCallSource {
    #[default]
    Direct,
    CodeMode,
}

/// Opaque identity for one concrete execution of a tool call.
#[derive(
    Debug,
    Clone,
    Default,
    Deserialize,
    Serialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    JsonSchema,
    TS,
)]
#[serde(transparent)]
#[ts(type = "string")]
pub struct ToolExecutionId(pub String);

/// Payload-free identity for one accepted tool execution in the terminal
/// closure ledger.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingToolCallIdentity {
    pub call_id: String,
    pub execution_id: ToolExecutionId,
    #[serde(default)]
    pub source: TurnTimingToolCallSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent_call_id: Option<String>,
    #[serde(default)]
    pub sampling_generation_id: SamplingGenerationId,
}

/// Reconciliation summary for all accepted tool executions in a turn.
///
/// `complete` is true for a turn with no accepted calls. Otherwise it requires
/// every accepted execution to have a timing record, terminal outcome, and
/// exactly-once persisted projection, with no orphan or duplicate lifecycle
/// records and no ledger overflow.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingToolClosure {
    /// False when a legacy record omitted the entire closure ledger. Its zero
    /// counts and incomplete verdict are unavailable evidence, not observations.
    #[serde(default = "closure_measurement_available")]
    pub measurement_available: bool,
    #[serde(default)]
    pub accepted_count: u32,
    #[serde(default)]
    pub timing_paired_count: u32,
    #[serde(default)]
    pub terminal_count: u32,
    #[serde(default)]
    pub persisted_count: u32,
    #[serde(default)]
    pub duplicate_call_id_count: u32,
    #[serde(default)]
    pub duplicate_acceptance_count: u32,
    #[serde(default)]
    pub duplicate_timing_count: u32,
    #[serde(default)]
    pub duplicate_persistence_count: u32,
    #[serde(default)]
    pub orphan_timing_count: u32,
    #[serde(default)]
    pub orphan_persistence_count: u32,
    #[serde(default)]
    pub overflow_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_calls: Vec<TurnTimingToolCallIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphan_calls: Vec<TurnTimingToolCallIdentity>,
    #[serde(default)]
    pub complete: bool,
}

impl Default for TurnTimingToolClosure {
    fn default() -> Self {
        Self {
            measurement_available: true,
            accepted_count: 0,
            timing_paired_count: 0,
            terminal_count: 0,
            persisted_count: 0,
            duplicate_call_id_count: 0,
            duplicate_acceptance_count: 0,
            duplicate_timing_count: 0,
            duplicate_persistence_count: 0,
            orphan_timing_count: 0,
            orphan_persistence_count: 0,
            overflow_count: 0,
            unresolved_calls: Vec::new(),
            orphan_calls: Vec::new(),
            complete: true,
        }
    }
}

fn closure_measurement_available() -> bool {
    true
}

impl TurnTimingToolClosure {
    fn unmeasured() -> Self {
        Self {
            measurement_available: false,
            complete: false,
            ..Self::default()
        }
    }
}


/// Opaque identity for the sampling generation that emitted a tool call.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema, TS)]
#[serde(transparent)]
#[ts(type = "string")]
pub struct SamplingGenerationId(pub String);

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ToolLifecycleBoundary {
    #[default]
    RequestCreated,
    ResourceResolutionStart,
    ResourceResolutionEnd,
    DiffTrackerWaitStart,
    DiffTrackerWaitEnd,
    WorkspaceGateWaitStart,
    WorkspaceGateWaitEnd,
    EvidenceTrackerWaitStart,
    EvidenceTrackerWaitEnd,
    Admitted,
    HandlerStart,
    ProcessSpawn,
    ProcessExit,
    HandlerReturn,
    RelayEnqueue,
    RelayDelivery,
    NextModelSampleStart,
}

/// Queue and waiter state captured at a lifecycle boundary.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ToolLifecycleContext {
    #[serde(default)]
    pub relay_queue_depth: u32,
    #[serde(default)]
    pub parallel_gate_waiter_count: u32,
    #[serde(default)]
    pub sampling_gate_waiter_count: u32,
    #[serde(default)]
    pub process_output_waiter_count: u32,
    #[serde(default)]
    pub active_tool_count: u32,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum NextSampleBlockReason {
    WaitingForTool,
    WaitingForDelivery,
    WaitingForGate,
    WaitingForProcessCleanup,
    #[default]
    ReadyToSample,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum HarnessPhase {
    ResourceResolution,
    DiffTracker,
    WorkspaceGate,
    EvidenceTracker,
    Delivery,
    ProcessCleanup,
    ReadyToSample,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TurnPhaseChangedEvent {
    pub turn_id: String,
    pub phases: Vec<HarnessPhase>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ToolLifecycleWakeReason {
    #[default]
    Completed,
    Timeout,
    Cancelled,
    Retry,
}

/// Attribution for a bounded or sticky lifecycle wait.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ToolLifecycleTimerWait {
    pub wait_kind: String,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub requested_timeout_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub effective_timeout_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub deadline_at_ms: Option<u64>,
    #[serde(default)]
    pub wake_reason: ToolLifecycleWakeReason,
    #[serde(default)]
    pub sequence: u32,
}

/// Exact event-site timestamp from the turn's single monotonic clock.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TurnTimingToolLifecycleEvent {
    pub boundary: ToolLifecycleBoundary,
    pub at_ms: u64,
    #[serde(default)]
    pub context: ToolLifecycleContext,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub reentry_count: u32,
}

/// Relay and persistence lifecycle for one model-issued or nested tool call.
///
/// All `*_at_ms` values are monotonic offsets from turn start. A missing
/// boundary means the call ended before that boundary was observed. No tool
/// arguments, output, or paths are persisted; nested identifiers are opaque
/// correlation keys only.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingToolCall {
    pub call_id: String,
    /// Model-visible outer tool call that owns this nested call, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent_call_id: Option<String>,
    /// Runtime cell that issued this nested tool request, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent_cell_id: Option<String>,
    /// Code-mode's payload-free per-cell invocation id, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub runtime_tool_call_id: Option<String>,
    pub tool_name: String,
    #[serde(default)]
    pub source: TurnTimingToolCallSource,
    #[serde(default)]
    pub execution_id: ToolExecutionId,
    #[serde(default)]
    pub sampling_generation_id: SamplingGenerationId,
    /// Payload-free terminal classification reported by the tool runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub outcome: Option<String>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub generation_index: Option<u32>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub accepted_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_poll_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub parallel_gate_admitted_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub handler_entry_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub handler_exit_at_ms: Option<u64>,
    /// The tool result has been collected into the host relay, before any
    /// post-handler evidence, projection, persistence, or delivery work.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub output_collected_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub process_spawned_at_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub process_exited_at_ms: Option<u64>,
    /// Historical relay-completion boundary. This predates persistence timing
    /// and therefore does not prove that the output reached conversation
    /// history or the rollout recorder.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub delivered_at_ms: Option<u64>,
    /// The direct tool output was committed to model-visible conversation
    /// history. Durable rollout persistence is tracked independently by the
    /// tool-closure ledger. Nested code-mode calls are represented by the outer
    /// direct tool output and leave this absent.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub output_model_visible_at_ms: Option<u64>,
    /// The next model generation began after this result became model-visible.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub model_resumed_at_ms: Option<u64>,
    /// Monotonic latency from the host proving that sampling is unblocked to
    /// the provider request reaching its transport dispatch boundary.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub ready_to_sample_to_dispatch_ns: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub item_to_first_poll_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub parallel_gate_wait_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub authorization_state_coordination_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub handler_duration_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub workspace_evidence_before_ms: Option<u64>,
    /// Whether the pre-handler workspace identity came from the latest
    /// authoritative capture instead of running a fresh capture.
    #[serde(default)]
    #[ts(type = "boolean | null")]
    pub workspace_evidence_before_cache_hit: Option<bool>,
    /// Git dependencies that crossed the bounded capture deadline during a
    /// fresh pre-handler workspace capture.
    #[serde(default)]
    pub workspace_evidence_before_timed_out_git_dependencies: Vec<String>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub workspace_evidence_after_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub pre_tool_hook_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub post_tool_hook_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub output_projection_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub history_persistence_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub post_handler_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub total_duration_ms: Option<u64>,
    #[serde(default)]
    pub eager: bool,
    #[serde(default)]
    pub process_alive_at_delivery: bool,
    /// The exec handler reached its post-cleanup process-store observation.
    #[serde(default)]
    pub exec_cleanup_state_observed: bool,
    /// A retained running process is expected only when the response returned
    /// a background process identifier for a later `write_stdin` call.
    #[serde(default)]
    pub background_process_expected: bool,
    /// The process manager still retained this call after foreground cleanup.
    #[serde(default)]
    pub running_process_after_cleanup: bool,
    /// Exact event-site lifecycle records. Older timestamp fields remain as
    /// compatibility projections of these boundaries.
    #[serde(default)]
    pub lifecycle_events: Vec<TurnTimingToolLifecycleEvent>,
    #[serde(default)]
    pub timer_waits: Vec<ToolLifecycleTimerWait>,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub reentry_count: u32,
    #[serde(default)]
    pub next_sample_block_reason: NextSampleBlockReason,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingTerminalization {
    #[serde(default)]
    pub final_mutation_to_seal_ns: u64,
    pub validation_process_ns: u64,
    #[serde(default)]
    pub validation_aggregate_ns: u64,
    #[serde(default)]
    pub validation_launch_count: u32,
    #[serde(default)]
    pub validation_aggregate_count: u32,
    #[serde(default)]
    pub terminal_memo_hit_count: u32,
    #[serde(default)]
    pub diff_reuse_count: u32,
    #[serde(default)]
    pub diff_refresh_count: u32,
    #[serde(default)]
    pub preparation_ns: u64,
    #[serde(default)]
    pub hooks_quiescence_ns: u64,
    #[serde(default)]
    pub fence_ns: u64,
    #[serde(default)]
    pub freshness_ns: u64,
    #[serde(default)]
    pub gate_ns: u64,
    #[serde(default)]
    pub durable_commit_ns: u64,
    #[serde(default)]
    pub delivery_attempt_ns: u64,
    #[serde(default)]
    pub interaction_release_ns: u64,
    #[serde(default)]
    pub post_cleanup_ns: u64,
    #[serde(default)]
    pub unclassified_ns: u64,
}

/// Semantic basis used by request token-category accounting.
///
/// Categories describe the complete logical prompt presented to the model.
/// They do not describe only websocket deltas, newly allocated client memory,
/// uncached provider input, or reconciliation residuals.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingTokenCategoryBasis {
    #[default]
    FullLogicalPrompt,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingRequestTokenCategories {
    /// Exact content hashes from prompt provenance, not the state fingerprint.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prompt_section_sha256: BTreeMap<String, String>,
    #[serde(default)]
    pub accounting_basis: TurnTimingTokenCategoryBasis,
    #[serde(default)]
    pub base_instructions: u64,
    #[serde(default)]
    pub tool_schemas: u64,
    #[serde(default)]
    pub conversation_history: u64,
    /// Serialized tool-result bytes in this request's full logical input,
    /// including retained results. Subset of conversation_history; not transport bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_output_serialized_bytes: Option<u64>,
    /// Native tool-result token estimate for this request's full logical input,
    /// including retained results. Subset of conversation_history; not provider usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_output_tokens_estimate: Option<u64>,
    #[serde(default)]
    pub current_input: u64,
    #[serde(default)]
    pub repository_context: u64,
    #[serde(default)]
    pub skills: u64,
    #[serde(default)]
    pub other_injected_context: u64,
    /// Sum of the mutually exclusive category estimates above.
    #[serde(default)]
    pub logical_total: u64,
    /// One-pass local estimate over the complete serialized logical prompt.
    #[serde(default)]
    pub local_input_estimate: u64,
    /// `local_input_estimate - logical_total`. This exposes estimator and JSON
    /// envelope effects instead of silently assigning them to a category.
    #[serde(default)]
    pub local_reconciliation_residual: i64,
    /// Provider-reported full input tokens, when usage was returned.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub provider_input_tokens: Option<u64>,
    /// `provider_input_tokens - logical_total`, when provider usage exists.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub provider_reconciliation_residual: Option<i64>,
    /// Overlapping diagnostic: logical tokens in categories whose exact hash
    /// matched the preceding request. This is not added to `logical_total`.
    #[serde(default)]
    pub repeated_unchanged_context: u64,
    /// Fixed-prefix categories whose exact hash differed from the preceding
    /// request. Non-empty explains why fixed-prefix reuse was ineligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixed_prefix_changed_categories: Vec<String>,
    /// Input items the preceding request sent, from this request's own
    /// comparison. Absent on the first request of a comparison chain, which has
    /// no predecessor to diverge from.
    ///
    /// A whole-history digest changes on every request by design, so it cannot
    /// distinguish appending from rewriting. These three fields can: with
    /// `history_prefix_items_reused == history_items_previous` the request only
    /// appended, and a smaller `history_first_divergent_index` locates where the
    /// prefix was rewritten or truncated instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub history_items_previous: Option<u32>,
    /// Leading input items identical to the preceding request's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub history_prefix_items_reused: Option<u32>,
    /// First input index whose digest differs from the preceding request's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub history_first_divergent_index: Option<u32>,
    /// Tool results the aggregate output budget dropped from the representation
    /// this request actually sent.
    ///
    /// Attributed to one request and one representation: the budget runs over
    /// several candidate projections, and summing them would count drops that
    /// never reached the model.
    #[serde(default)]
    pub tool_output_budget_drop_count: u32,
    /// Tokens those dropped results would have occupied.
    #[serde(default)]
    pub tool_output_budget_dropped_token_count: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingRequestDiagnosticsStatus {
    #[default]
    Pending,
    Complete,
    Unavailable,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingModelRequest {
    /// Versioned content-free JSON delivery summaries by retained physical
    /// attempt. Chunks are decoded deltas, not individual provider tokens.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stream_delivery_by_attempt: BTreeMap<String, String>,
    /// Host queue observations by physical attempt, then api_to_mapper or
    /// mapper_to_turn stage. Missing stages are unknown, not zero. Queue sums
    /// overlap provider work and each other; never subtract them from model wait
    /// to invent provider computation time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response_queue_timing_by_attempt:
        BTreeMap<String, BTreeMap<String, crate::timing::ResponseQueueTiming>>,
    /// Named setup-phase union durations in nanoseconds, frozen at dispatch.
    /// Measured from request preparation (or attempt start for retries), not
    /// cumulative across the turn. Phases can overlap and must not be summed.
    /// Empty for older records or attempts that never dispatched.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub setup_phase_ns: BTreeMap<String, u64>,
    /// SHA-256 of the exact serialized transport body for each dispatched
    /// physical attempt. Includes retry bodies and websocket deltas.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_sha256_by_attempt: BTreeMap<String, String>,
    /// Hashes of serialized top-level transport fields, captured at dispatch
    /// even when optional prompt/token diagnostics have not finished.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_section_sha256_by_attempt: BTreeMap<String, BTreeMap<String, String>>,
    /// Provider response IDs retained so offline replay preserves websocket
    /// continuation requests rather than inventing new response identities.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response_id_by_attempt: BTreeMap<String, String>,
    /// Zero-based logical generation owning this physical provider attempt.
    #[serde(default)]
    pub generation_index: u32,
    #[serde(default)]
    pub generation_reason: TurnTimingGenerationReason,
    /// Stable workflow purpose for the logical generation. Physical retries
    /// inherit this value from their owning generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub generation_purpose: Option<TurnTimingGenerationPurpose>,
    #[serde(default)]
    pub disposition: TurnTimingGenerationDisposition,
    /// A redacted hash of only the structured state relevant to this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub relevant_state_fingerprint: Option<String>,
    /// Stable identity shared by every physical provider attempt for this
    /// logical request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sampling_request_id: Option<String>,
    /// Physical provider attempts observed for this logical request, including
    /// retries and transport fallbacks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub physical_attempt_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub progress_kinds: Vec<TurnTimingProgressKind>,
    /// False only after the next completed continuation is observed choosing
    /// the same complete ordered tool actions (names and arguments). True also
    /// covers an unknown or unobserved next action; this is not a completion test.
    #[serde(default)]
    pub next_structured_action_changed: bool,
    #[serde(default)]
    pub unchanged_relevant_state: bool,
    #[serde(default)]
    pub attempt_kind: TurnTimingAttemptKind,
    pub is_continuation: bool,
    /// Directly observed model-stream wait for this physical attempt.
    #[serde(default)]
    pub model_stream_wait_ns: u64,
    /// Dispatch-to-first-actionable-output latency. This excludes reasoning
    /// deltas and partial tool arguments; missing actionable output stays null.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub decision_latency_ns: Option<u64>,
    /// Unique tool calls accepted for this generation, including unpolled calls.
    /// Timing schemas <=32 counted calls at executor polling instead.
    #[serde(default)]
    pub tool_call_count: u32,
    /// Tool calls parsed from completed model output before relay persistence
    /// or executor polling. Recorded on the generation's primary attempt.
    #[serde(default)]
    pub model_emitted_tool_call_count: u32,
    /// Direct model-issued calls that acquired the executor admission gate.
    #[serde(default)]
    pub executor_admitted_tool_call_count: u32,
    /// Peak direct model-issued calls holding executor admission concurrently.
    #[serde(default)]
    pub executor_max_concurrent_tool_calls: u32,
    /// Unioned tool-active duration attributed to this generation. Parallel
    /// tool calls are counted once per wall-clock interval.
    #[serde(default)]
    pub tool_active_union_ns: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    /// Provider-reported usage for this physical attempt. Missing usage stays
    /// null, including failed attempts for which the provider reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub token_usage: Option<TurnTimingProviderTokenUsage>,
    /// Full logical prompt accounting and section hashes. No prompt text,
    /// repository paths, or tool arguments are persisted here.
    #[serde(default)]
    pub request_token_categories: Option<TurnTimingRequestTokenCategories>,
    /// Optional diagnostics can finish after the turn. Late diagnostic records
    /// use sampling_request_id and physical_attempt_id to update this row.
    #[serde(default)]
    pub request_diagnostics_status: TurnTimingRequestDiagnosticsStatus,
    /// Whether this logical request exactly matched the preceding stable
    /// prompt prefix under the same prompt-cache identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub fixed_prefix_reuse_eligible: Option<bool>,
    /// Stable SHA-256 fingerprint of the prompt cache key sent to the provider.
    /// The raw key is intentionally not persisted in timing diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub prompt_cache_key_fingerprint: Option<String>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub dispatch_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_model_output_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_actionable_output_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub completed_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingGenerationDisposition {
    #[default]
    #[serde(alias = "model_required")]
    Unknown,
    DecisionBearing,
    #[serde(
        alias = "owner_drained",
        alias = "owner_proved_deterministic",
        alias = "suppressed"
    )]
    Deterministic,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingGenerationPurpose {
    #[default]
    #[serde(rename = "initial", alias = "initial_reasoning")]
    #[ts(rename = "initial")]
    InitialReasoning,
    #[serde(rename = "implementation", alias = "implementation_decision")]
    #[ts(rename = "implementation")]
    ImplementationDecision,
    Wait,
    FailureDiagnosis,
    #[serde(alias = "validation_launch", alias = "validation_selection")]
    ValidationInterpretation,
    Repair,
    #[serde(rename = "agent_coordination", alias = "coordination")]
    #[ts(rename = "agent_coordination")]
    Coordination,
    #[serde(
        rename = "tool_result_interpretation",
        alias = "deterministic_tool_continuation",
        alias = "artifact_continuation",
        alias = "source_interpretation",
        alias = "source_evidence_interpretation",
        alias = "source_discovery"
    )]
    #[ts(rename = "tool_result_interpretation")]
    ArtifactContinuation,
    CompactionRecovery,
    #[serde(rename = "terminal", alias = "terminal_completion_reasoning")]
    #[ts(rename = "terminal")]
    TerminalCompletionReasoning,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum DeterministicContinuationClass {
    #[default]
    UnchangedWait,
    #[serde(alias = "source_bundle", alias = "source_coverage")]
    ArtifactRange,
    AgentEventWait,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum DeterministicContinuationHostAction {
    #[default]
    AwaitStateChange,
    #[serde(
        alias = "batch_source_bundle",
        alias = "reuse_covered_span",
        alias = "read_missing_ranges"
    )]
    DrainArtifactRanges,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[schemars(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub struct TurnTimingDeterministicContinuationReceipt {
    pub class: DeterministicContinuationClass,
    pub resource_identity_hash: String,
    pub state_revision: String,
    pub host_action: DeterministicContinuationHostAction,
    pub wire_identity: String,
    /// Internal proof of the owner-specific action bounds. The owning
    /// subsystem retains the exact typed bounds; telemetry identity carries
    /// only this redacted hash. It is intentionally absent from every wire,
    /// schema, and generated TypeScript representation.
    #[schemars(skip)]
    #[ts(skip)]
    pub action_bounds_hash: String,
    pub suppressed_continuation_count: u32,
}

impl TurnTimingDeterministicContinuationReceipt {
    pub fn new(
        class: DeterministicContinuationClass,
        resource_identity_hash: String,
        state_revision: String,
        host_action: DeterministicContinuationHostAction,
        action_bounds_hash: String,
        suppressed_continuation_count: u32,
    ) -> Self {
        let wire_identity = derive_continuation_wire_identity(
            class,
            &resource_identity_hash,
            &state_revision,
            host_action,
        );
        Self {
            class,
            resource_identity_hash,
            state_revision,
            host_action,
            wire_identity,
            action_bounds_hash,
            suppressed_continuation_count,
        }
    }

    pub fn wire_identity(&self) -> String {
        self.expected_wire_identity()
    }

    pub fn runtime_identity(&self) -> Option<String> {
        if self.action_bounds_hash.is_empty() {
            return None;
        }
        Some(format!(
            "{}|bounds:{}:{}",
            self.expected_wire_identity(),
            self.action_bounds_hash.len(),
            self.action_bounds_hash,
        ))
    }

    fn expected_wire_identity(&self) -> String {
        derive_continuation_wire_identity(
            self.class,
            &self.resource_identity_hash,
            &self.state_revision,
            self.host_action,
        )
    }
}

fn derive_continuation_wire_identity(
    class: DeterministicContinuationClass,
    resource_identity_hash: &str,
    state_revision: &str,
    host_action: DeterministicContinuationHostAction,
) -> String {
    fn class_name(class: DeterministicContinuationClass) -> &'static str {
        match class {
            DeterministicContinuationClass::UnchangedWait => "unchanged_wait",
            DeterministicContinuationClass::ArtifactRange => "artifact_range",
            DeterministicContinuationClass::AgentEventWait => "agent_event_wait",
        }
    }
    fn action_name(action: DeterministicContinuationHostAction) -> &'static str {
        match action {
            DeterministicContinuationHostAction::AwaitStateChange => "await_state_change",
            DeterministicContinuationHostAction::DrainArtifactRanges => "drain_artifact_ranges",
        }
    }
    format!(
        "v1|{}|{}:{}|{}:{}|{}",
        class_name(class),
        resource_identity_hash.len(),
        resource_identity_hash,
        state_revision.len(),
        state_revision,
        action_name(host_action),
    )
}

fn legacy_continuation_wire_identity_matches(
    serialized: &str,
    class: DeterministicContinuationClass,
    resource_identity_hash: &str,
    state_revision: &str,
    host_action: DeterministicContinuationHostAction,
) -> bool {
    let class_names: &[&str] = match class {
        DeterministicContinuationClass::ArtifactRange => {
            &["artifact_range", "source_bundle", "source_coverage"]
        }
        _ => return false,
    };
    let action_names: &[&str] = match host_action {
        DeterministicContinuationHostAction::DrainArtifactRanges => &[
            "drain_artifact_ranges",
            "batch_source_bundle",
            "reuse_covered_span",
            "read_missing_ranges",
        ],
        _ => return false,
    };
    class_names.iter().any(|class_name| {
        action_names.iter().any(|action_name| {
            serialized
                == format!(
                    "v1|{class_name}|{}:{}|{}:{}|{action_name}",
                    resource_identity_hash.len(),
                    resource_identity_hash,
                    state_revision.len(),
                    state_revision,
                )
        })
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeterministicContinuationReceiptRef<'a> {
    class: DeterministicContinuationClass,
    resource_identity_hash: &'a str,
    state_revision: &'a str,
    host_action: DeterministicContinuationHostAction,
    wire_identity: String,
    suppressed_continuation_count: u32,
}

impl Serialize for TurnTimingDeterministicContinuationReceipt {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let expected = self.expected_wire_identity();
        DeterministicContinuationReceiptRef {
            class: self.class,
            resource_identity_hash: &self.resource_identity_hash,
            state_revision: &self.state_revision,
            host_action: self.host_action,
            wire_identity: expected,
            suppressed_continuation_count: self.suppressed_continuation_count,
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeterministicContinuationReceiptOwned {
    class: DeterministicContinuationClass,
    resource_identity_hash: String,
    state_revision: String,
    host_action: DeterministicContinuationHostAction,
    #[serde(default)]
    wire_identity: Option<String>,
    suppressed_continuation_count: u32,
}

impl<'de> Deserialize<'de> for TurnTimingDeterministicContinuationReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = DeterministicContinuationReceiptOwned::deserialize(deserializer)?;
        let expected = derive_continuation_wire_identity(
            wire.class,
            &wire.resource_identity_hash,
            &wire.state_revision,
            wire.host_action,
        );
        if let Some(serialized) = wire.wire_identity
            && serialized != expected
            && !legacy_continuation_wire_identity_matches(
                &serialized,
                wire.class,
                &wire.resource_identity_hash,
                &wire.state_revision,
                wire.host_action,
            )
        {
            return Err(serde::de::Error::custom(
                "deterministic continuation wireIdentity does not match its canonical public fields",
            ));
        }
        Ok(Self {
            class: wire.class,
            resource_identity_hash: wire.resource_identity_hash,
            state_revision: wire.state_revision,
            host_action: wire.host_action,
            wire_identity: expected,
            action_bounds_hash: String::new(),
            suppressed_continuation_count: wire.suppressed_continuation_count,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingProviderTokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub coverage: Option<TokenUsageCoverage>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    /// Provider output minus explicitly reported reasoning, not visible text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output_excluding_reported_reasoning_tokens: Option<u64>,
    /// Deprecated compatibility remainder. Not a final-answer text count;
    /// consult coverage and the canonical optional field before interpreting it.
    pub visible_output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingDiagnosticTokenAggregate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub coverage: Option<TokenUsageCoverage>,
    /// Expected physical usage reports; absent in legacy aggregates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expected_usage_count: Option<u64>,
    /// Attributable reports observed. Totals are lower bounds unless equal to expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub observed_usage_count: Option<u64>,
    pub logical_generations: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    /// Same provider remainder as the per-request canonical field. Retained
    /// window only when TurnTiming.model_request_history_truncated is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output_excluding_reported_reasoning_tokens: Option<u64>,
    /// Deprecated compatibility remainder; never add it to the canonical field.
    pub visible_output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingDiagnosticLatencyAggregate {
    pub logical_generations: u32,
    pub physical_attempts: u32,
    pub model_stream_wait_ns: u64,
    pub decision_ready_attempts: u32,
    pub decision_latency_ns: u64,
    pub tool_calls: u32,
    pub tool_active_union_ns: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingProgressKind {
    #[serde(
        alias = "new_input",
        alias = "plan_change",
        alias = "completion_state",
        alias = "structured_action_change"
    )]
    WorkspaceMutation,
    ValidationResult,
    FailureObservation,
    #[serde(alias = "new_named_evidence", alias = "source_closure")]
    NewSourceEvidence,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingGenerationReason {
    Initial,
    ToolContinuation,
    Compaction,
    Subagent,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum TurnTimingAttemptKind {
    #[default]
    Primary,
    Retry,
    Fallback,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingGenerationReasonCounts {
    pub initial: u32,
    pub tool_continuation: u32,
    pub compaction: u32,
    pub subagent: u32,
    pub other: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingGenerationDispositionCounts {
    #[serde(default)]
    pub unknown: u32,
    #[serde(default)]
    pub decision_bearing: u32,
    #[serde(default)]
    pub deterministic: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingGenerationPurposeCounts {
    #[serde(default, alias = "initial")]
    pub initial_reasoning: u32,
    #[serde(default, alias = "implementation")]
    pub implementation_decision: u32,
    pub wait: u32,
    pub failure_diagnosis: u32,
    pub validation_interpretation: u32,
    pub repair: u32,
    #[serde(default, alias = "agentCoordination")]
    pub coordination: u32,
    #[serde(default, alias = "deterministicToolContinuation")]
    pub artifact_continuation: u32,
    pub compaction_recovery: u32,
    #[serde(default, alias = "terminal")]
    pub terminal_completion_reasoning: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingGenerationPurposeAggregate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub coverage: Option<TokenUsageCoverage>,
    /// Expected physical usage reports; absent in legacy aggregates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expected_usage_count: Option<u64>,
    /// Attributable reports observed. Totals are lower bounds unless equal to expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub observed_usage_count: Option<u64>,
    pub purpose: TurnTimingGenerationPurpose,
    pub generations: u32,
    pub model_stream_wait_ns: u64,
    /// Sum of dispatch-to-first-actionable-output latency for requests where
    /// an actionable output was observed.
    #[serde(default)]
    pub decision_latency_ns: u64,
    #[serde(default)]
    pub decision_ready_requests: u32,
    pub tool_calls: u32,
    /// Sum of per-generation unioned tool-active durations. Parallel tools
    /// within one generation are not double-counted.
    #[serde(default)]
    pub tool_active_union_ns: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Provider remainder, not visible/final text. Completeness also requires
    /// TurnTiming.model_request_history_truncated == Some(false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output_excluding_reported_reasoning_tokens: Option<u64>,
    /// Deprecated compatibility remainder; never add it to the canonical field.
    #[serde(default)]
    pub visible_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingAttemptKindCounts {
    pub primary: u32,
    pub retry: u32,
    pub fallback: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingPreFirstModelOutput {
    pub captured_at_ns: u64,
    pub first_request_dispatch_ready_ns: u64,
    pub client_critical_path_ns: u64,
    pub attributed_client_union_ns: u64,
    pub unattributed_pre_output_ns: u64,
    pub history_snapshot_ns: u64,
    pub normalization_ns: u64,
    pub prompt_construction_ns: u64,
    pub request_transformation_ns: u64,
    pub serialization_ns: u64,
    pub transport_readiness_ns: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingExclusive {
    pub model_only_ns: u64,
    pub tool_only_ns: u64,
    pub model_plus_tool_ns: u64,
    pub interactive_only_wait_ns: u64,
    pub interactive_plus_machine_ns: u64,
    pub retry_only_ns: u64,
    pub orchestration_ns: u64,
    pub standalone_work_ns: u64,
    pub finalization_ns: u64,
    pub unclassified_ns: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingUnions {
    pub model_active_union_ns: u64,
    pub model_active_union_ms: u64,
    pub model_request_wait_union_ns: u64,
    pub model_stream_wait_union_ns: u64,
    pub model_stream_processing_union_ns: u64,
    pub tool_active_union_ns: u64,
    pub tool_active_union_ms: u64,
    pub interactive_wait_union_ns: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingLocal {
    pub preparation_union_ns: u64,
    /// Inclusive planning union, including any nested compaction.
    pub planning_union_ns: u64,
    /// Planning union with nested compaction overlap removed.
    #[serde(default)]
    pub planning_exclusive_union_ns: u64,
    /// Time simultaneously attributed to planning and compaction.
    #[serde(default)]
    pub planning_compaction_overlap_union_ns: u64,
    pub compaction_union_ns: u64,
    pub persistence_union_ns: u64,
    pub serialization_union_ns: u64,
    pub router_build_union_ns: u64,
    pub startup_prewarm_wait_union_ns: u64,
    pub executor_readiness_wait_union_ns: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingMilestones {
    /// Worker entry after the task-installation barrier, from turn start.
    /// Absent is unknown; it must not be reconstructed from wall timestamps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "number")]
    pub turn_worker_started_ms: Option<u64>,
    /// First ordinary request-preparation entry before any provider dispatch.
    /// The preceding span includes initial turn setup, not provider work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "number")]
    pub first_request_preparation_started_ms: Option<u64>,
    /// Time from turn start until the first user input begins durable recording.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub user_input_recorded_ms: Option<u64>,
    /// Time from turn start until the first tool call is accepted for dispatch.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_tool_accepted_ms: Option<u64>,
    /// Time from turn start until the first tool call passes the parallel gate.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_tool_gate_admitted_ms: Option<u64>,
    /// Time from turn start until the first authorized tool handler is entered.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_tool_handler_entry_ms: Option<u64>,
    /// Time from turn start until the first domain-work tool is accepted for dispatch.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_useful_tool_accepted_ms: Option<u64>,
    /// Time from turn start until the first domain-work tool passes the parallel gate.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_useful_tool_gate_admitted_ms: Option<u64>,
    /// Time from turn start until the first authorized domain-work tool handler is entered.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_useful_action_ms: Option<u64>,
    /// Time from turn start until the first domain-work tool completes successfully.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_successful_useful_action_ms: Option<u64>,
    /// Time from turn start until the first authorized control/plumbing tool handler is entered.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_infrastructure_action_ms: Option<u64>,
    /// Time from turn start until the first authorized tool-schema discovery handler is entered.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_tool_discovery_action_ms: Option<u64>,
    /// Time from turn start until the first authorized domain-work handler is entered.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_domain_action_ms: Option<u64>,
    /// Time from turn start until the first domain-work tool completes successfully.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_successful_domain_action_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_model_output_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_actionable_output_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_visible_output_ms: Option<u64>,
    #[serde(default)]
    #[ts(type = "number | null")]
    pub first_agent_message_ms: Option<u64>,
    /// First nonempty assistant text accepted by the post-filter host queue;
    /// reasoning, completed messages and native paint are separate milestones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "number")]
    pub first_assistant_text_ms: Option<u64>,
    /// Item identity when supplied by the published event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub first_assistant_text_item_id: Option<String>,
    /// Phase when supplied by the published event; absent is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub first_assistant_text_phase: Option<MessagePhase>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct TurnTimingCounters {
    /// Logical workflow generations. `model_request_count` remains the
    /// authoritative physical transport-attempt aggregate in timing schema 34+:
    /// each identity is counted once at transport entry, including internal auth
    /// retries. This does not prove provider receipt. Older schemas may count
    /// only the first dispatch/readiness in a sampling row.
    #[serde(default)]
    pub logical_generation_count: u32,
    #[serde(default)]
    pub generations_by_reason: TurnTimingGenerationReasonCounts,
    #[serde(default)]
    pub generations_by_purpose: TurnTimingGenerationPurposeCounts,
    #[serde(default)]
    pub generations_by_disposition: TurnTimingGenerationDispositionCounts,
    /// Authoritative number of model boundaries suppressed by accepted
    /// deterministic-continuation receipts. A receipt whose
    /// `suppressed_continuation_count` is N contributes N, not one. Invalid,
    /// rejected, and zero-count receipts contribute nothing. This counter is
    /// independent of `generations_by_disposition`.
    #[serde(default)]
    pub suppressed_deterministic_continuation_count: u32,
    /// Residual deterministic generation requests proved by turn execution.
    /// Null means production execution control does not measure this counter.
    #[serde(default)]
    pub residual_deterministic_generation_count: Option<u32>,
    /// Continuations drained and recorded by their code-mode owner.
    #[serde(default)]
    pub owner_drained_continuation_count: u32,
    /// Validation commands that executed.
    #[serde(default)]
    pub executed_validation_count: u32,
    /// Total wall-clock nanoseconds spent in validation commands that executed.
    #[serde(default)]
    pub executed_validation_duration_ns: u64,
    /// Ready startup prewarms observed by the first model request.
    #[serde(default)]
    pub ready_startup_prewarm_count: u32,
    /// Tool results the aggregate output budget dropped across every request
    /// this turn actually sent. Preparing an unchanged projection again adds
    /// nothing; only a dispatched request contributes.
    #[serde(default)]
    pub tool_output_budget_drop_count: u32,
    /// Tokens those dropped results would have occupied.
    #[serde(default)]
    pub tool_output_budget_dropped_token_count: u64,
    /// Bounded to the stable purpose enum's cardinality.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purpose_aggregates: Vec<TurnTimingGenerationPurposeAggregate>,
    #[serde(default)]
    pub same_purpose_continuation_count: u32,
    #[serde(default)]
    /// Wait generations sharing coarse revisions; this does not establish that
    /// the wait action, cursor, or owner state repeated.
    #[serde(alias = "exactRepeatedWaitCount")]
    pub wait_generations_with_same_revision_count: u32,
    #[serde(default)]
    pub planning_generation_count: u32,
    #[serde(default)]
    pub plan_revision_generation_count: u32,
    #[serde(default)]
    pub planning_fixed_point_iteration_count: u32,
    #[serde(default)]
    pub planning_invalidation_count: u32,
    #[serde(default)]
    pub planning_semantic_effect_count: u32,
    #[serde(default)]
    pub planning_failure_count: u32,
    #[serde(default)]
    pub failure_signature_count: u32,
    #[serde(default)]
    pub failure_diagnosis_count: u32,
    #[serde(default)]
    pub attempts_by_kind: TurnTimingAttemptKindCounts,
    /// Bounded detail loss; diagnostics derived from request rows cover only
    /// retained detail when either eviction counter is nonzero.
    #[serde(default)]
    pub model_request_detail_eviction_count: u32,
    #[serde(default)]
    pub physical_attempt_detail_eviction_count: u32,
    pub model_request_count: u32,
    pub model_retry_count: u32,
    pub model_fallback_count: u32,
    pub tool_call_count: u32,
    pub approval_wait_count: u32,
    pub permission_wait_count: u32,
    pub user_input_wait_count: u32,
    pub mcp_elicitation_wait_count: u32,
    #[serde(default)]
    pub wait_only_generation_count: u32,
    #[serde(default)]
    pub internally_drained_wait_count: u32,
    #[serde(default)]
    pub no_progress_directive_count: u32,
    #[serde(default)]
    pub proven_loop_activation_count: u32,
    #[serde(default)]
    pub tool_router_reuse_count: u32,
    #[serde(default)]
    pub tool_router_rebuild_count: u32,
    #[serde(default)]
    pub projection_source_dependencies_reuse_count: u32,
    #[serde(default)]
    pub projection_source_dependencies_fallback_count: u32,
    #[serde(default)]
    pub tool_output_truncation_count: u32,
    #[serde(default)]
    pub tool_output_projected_token_count: u64,
    #[serde(default)]
    pub tool_output_artifact_reread_count: u32,
    #[serde(default)]
    pub tool_output_canonical_byte_count: u64,
    #[serde(default)]
    pub tool_output_canonical_token_count: u64,
    #[serde(default)]
    pub tool_output_model_byte_count: u64,
    #[serde(default)]
    pub tool_output_model_token_count: u64,
    #[serde(default)]
    pub tool_output_artifact_creation_count: u32,
    #[serde(default)]
    pub tool_output_artifact_reuse_count: u32,
    #[serde(default)]
    pub tool_output_projection_truncation_count: u32,
    #[serde(default)]
    pub tool_output_omitted_section_count: u64,
    #[serde(default)]
    pub tool_output_recovery_call_count: u32,
    /// Recovery transactions performed inside code-mode cells, without a
    /// separate provider tool-result generation.
    #[serde(default)]
    pub tool_output_in_cell_recovery_call_count: u32,
    #[serde(default)]
    pub tool_output_recovery_retruncation_count: u32,
    #[serde(default)]
    pub tool_output_recursive_spill_count: u32,
    /// Tool-result generations immediately following an observed
    /// direct `read_tool_output` recovery transaction. In-cell reads are
    /// counted separately. This is independent of
    /// whether the recovered projection was truncated again.
    #[serde(default)]
    pub attributable_recovery_generation_count: u32,
    /// Tool-result generations immediately following provider-visible output
    /// that actually omitted or truncated canonical tool information.
    /// Temporal succession only: this does not establish an extra generation
    /// caused by truncation. Historical causal spelling is accepted on input.
    #[serde(default, rename = "continuationAfterTruncationCount", alias = "truncationInducedContinuationCount")]
    pub truncation_induced_continuation_count: u32,
    pub invalid_transition_count: u32,
    pub clock_regression_count: u32,
    pub saturation_count: u32,
}

/// Authoritative typed tool result surfaced as the terminal result of a turn.
/// `canonical_message`, when present, is owner-authored and may be copied
/// unchanged into legacy last-message consumers. `value` is never rendered into
/// synthetic assistant prose by the host.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SurfacedToolResult {
    pub adapter: String,
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub canonical_message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TurnCompleteEvent {
    pub turn_id: String,
    pub last_agent_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub surfaced_result: Option<SurfacedToolResult>,
    /// Terminal error details when the turn completed unsuccessfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<ErrorEvent>,
    /// Unix timestamp (in seconds) when the turn completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub completed_at: Option<i64>,
    /// Duration from turn start to the frozen core terminal boundary, if known.
    /// Detail export/event enqueue after that boundary are excluded; not client delivery latency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub duration_ms: Option<i64>,
    /// Duration until the turn consumer observes nonempty model content (text,
    /// exposed reasoning, or tool input), excluding internal compaction. This is
    /// not provider-internal token timing. Timing schemas <=32 measured first
    /// visible output instead; older records must retain that interpretation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub time_to_first_token_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timing: Option<TurnTiming>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TurnStartedEvent {
    pub turn_id: String,
    // Persist for rollout consumers that correlate turns with telemetry traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub trace_id: Option<String>,
    /// Unix timestamp (in seconds) when the turn started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub started_at: Option<i64>,
    // TODO(aibrahim): make this not optional
    pub model_context_window: Option<i64>,
    #[serde(default)]
    pub collaboration_mode_kind: ModeKind,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ThreadSettingsAppliedEvent {
    pub thread_settings: ThreadSettingsSnapshot,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ThreadSettingsSnapshot {
    pub model: String,
    pub model_provider_id: String,
    /// Outer `None` means an older snapshot did not supply this field; an
    /// inner `None` records an explicit effective clear.
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub service_tier: Option<Option<String>>,
    /// Outer `None` means an older snapshot did not supply this field; an
    /// inner `None` records an explicit effective clear.
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub developer_instructions: Option<Option<String>>,
    pub approval_policy: AskForApproval,
    pub permission_profile: PermissionProfile,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub active_permission_profile: Option<Option<ActivePermissionProfile>>,
    /// Complete sticky thread environment selection, including the legacy cwd.
    /// Missing on rollouts written before environment persistence was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environments: Option<TurnEnvironmentSelections>,
    /// Runtime workspace roots used to materialize symbolic workspace permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
    /// Workspace roots declared by the active permission profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub profile_workspace_roots: Option<Vec<AbsolutePathBuf>>,
    /// Legacy sandbox projection retained alongside the canonical permission profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Effective Windows sandbox implementation for future tool execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub windows_sandbox_level: Option<WindowsSandboxLevel>,
    pub cwd: AbsolutePathBuf,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub reasoning_effort: Option<Option<ReasoningEffortConfig>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub reasoning_summary: Option<Option<ReasoningSummaryConfig>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub personality: Option<Option<Personality>>,
    pub collaboration_mode: CollaborationMode,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsage {
    #[ts(type = "number")]
    pub input_tokens: i64,
    #[ts(type = "number")]
    pub cached_input_tokens: i64,
    #[ts(type = "number")]
    pub output_tokens: i64,
    #[ts(type = "number")]
    pub reasoning_output_tokens: i64,
    #[ts(type = "number")]
    pub total_tokens: i64,
    /// Known receipt sums, not a claim that unobserved provider work was free.
    /// Absent on old records: component availability is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub coverage: Option<TokenUsageCoverage>,
}

/// Constant-space receipt/component coverage. Counts add and subtract with the
/// existing usage ledger, so an earlier missing receipt does not taint later turns.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageCoverage {
    #[ts(type = "number")]
    pub receipts: u64,
    /// Whole usage objects received. None preserves legacy coverage uncertainty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub reported_receipts: Option<u64>,
    /// Sum of the explicitly reported cache-write input class, not an estimate.
    /// Interpret with cache_write_receipts; an absent class is never known zero.
    #[serde(default)]
    #[ts(type = "number")]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    #[ts(type = "number")]
    pub cache_write_receipts: u64,
    #[ts(type = "number")]
    pub input_tokens: u64,
    #[ts(type = "number")]
    pub cached_input_tokens: u64,
    #[ts(type = "number")]
    pub output_tokens: u64,
    #[ts(type = "number")]
    pub reasoning_output_tokens: u64,
    #[ts(type = "number")]
    pub total_tokens: u64,
    #[ts(type = "number")]
    pub invalid_receipts: u64,
    pub saturated: bool,
}

impl TokenUsageCoverage {
    pub fn fully_reported() -> Self {
        Self {
            receipts: 1,
            reported_receipts: Some(1),
            input_tokens: 1,
            cached_input_tokens: 1,
            output_tokens: 1,
            reasoning_output_tokens: 1,
            total_tokens: 1,
            ..Default::default()
        }
    }

    pub fn add_assign(&mut self, other: &Self) {
        macro_rules! add { ($($field:ident),*) => { $(
            if self.$field.checked_add(other.$field).is_none() { self.saturated = true; }
            self.$field = self.$field.saturating_add(other.$field);
        )* }; }
        self.reported_receipts =
            self.reported_receipts
                .zip(other.reported_receipts)
                .map(|(left, right)| {
                    self.saturated |= left.checked_add(right).is_none();
                    left.saturating_add(right)
                });
        self.saturated |= self
            .cache_write_input_tokens
            .checked_add(other.cache_write_input_tokens)
            .is_none();
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        add!(
            receipts,
            cache_write_receipts,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
            invalid_receipts
        );
        self.saturated |= other.saturated;
    }

    fn difference(&self, before: &Self) -> Self {
        Self {
            receipts: self.receipts.saturating_sub(before.receipts),
            reported_receipts: self
                .reported_receipts
                .zip(before.reported_receipts)
                .map(|(after, before)| after.saturating_sub(before)),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(before.cache_write_input_tokens)
                .max(0),
            cache_write_receipts: self
                .cache_write_receipts
                .saturating_sub(before.cache_write_receipts),
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(before.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(before.output_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(before.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(before.total_tokens),
            invalid_receipts: self
                .invalid_receipts
                .saturating_sub(before.invalid_receipts),
            saturated: self.saturated || before.saturated,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageInfo {
    /// Cumulative known consumption, including compaction. Missing receipts are
    /// represented by coverage, not by an assertion that the work was free.
    pub total_token_usage: TokenUsage,
    /// Latest published provider receipt, not a ledger of every provider
    /// request. After a compaction it is that compaction's usage, while
    /// `estimated_context_tokens` keeps the conversation's context size; the
    /// next conversation receipt replaces it. Consumption reconstruction must
    /// use cumulative deltas together with coverage, not sum this field per event.
    pub last_token_usage: TokenUsage,
    /// Local active-context estimate, not observed provider consumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub estimated_context_tokens: Option<i64>,
    // TODO(aibrahim): make this not optional
    #[ts(type = "number | null")]
    pub model_context_window: Option<i64>,
}

impl TokenUsageInfo {
    pub fn new_or_append(
        info: &Option<TokenUsageInfo>,
        last: &Option<TokenUsage>,
        model_context_window: Option<i64>,
    ) -> Option<Self> {
        if info.is_none() && last.is_none() {
            return None;
        }

        let mut info = match info {
            Some(info) => info.clone(),
            None => Self {
                total_token_usage: TokenUsage::empty_ledger(),
                last_token_usage: TokenUsage::default(),
                estimated_context_tokens: None,
                model_context_window,
            },
        };
        if let Some(last) = last {
            info.append_last_usage(last);
        }
        info.model_context_window = model_context_window;
        Some(info)
    }

    pub fn append_last_usage(&mut self, last: &TokenUsage) {
        self.total_token_usage.add_assign(last);
        self.last_token_usage = last.clone();
        self.estimated_context_tokens = None;
    }

    pub fn fill_to_context_window(&mut self, context_window: i64) {
        self.model_context_window = Some(context_window);
        self.estimated_context_tokens = Some(context_window.max(0));
    }

    pub fn context_tokens(&self) -> i64 {
        self.estimated_context_tokens
            .unwrap_or(self.last_token_usage.total_tokens)
    }

    /// Compatibility projection for context bars only, never a consumption ledger.
    pub fn context_usage(&self) -> TokenUsage {
        TokenUsage {
            total_tokens: self.context_tokens(),
            ..TokenUsage::default()
        }
    }

    pub fn full_context_window(context_window: i64) -> Self {
        let mut info = Self {
            total_token_usage: TokenUsage::empty_ledger(),
            last_token_usage: TokenUsage::default(),
            estimated_context_tokens: None,
            model_context_window: Some(context_window),
        };
        info.fill_to_context_window(context_window);
        info
    }

    /// A fork inherits context, not the parent's already-consumed provider work.
    pub fn for_fork(&self) -> Self {
        Self {
            total_token_usage: TokenUsage::empty_ledger(),
            last_token_usage: TokenUsage::default(),
            estimated_context_tokens: Some(self.context_tokens()),
            model_context_window: self.model_context_window,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TokenCountEvent {
    pub info: Option<TokenUsageInfo>,
    pub rate_limits: Option<RateLimitSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct RateLimitSnapshot {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    /// Backend-reported spend-control state. `None` is unavailable, not a sparse-update recovery.
    pub spend_control_reached: Option<bool>,
    pub plan_type: Option<crate::account::PlanType>,
    pub rate_limit_reached_type: Option<RateLimitReachedType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RateLimitReachedType {
    RateLimitReached,
    WorkspaceOwnerCreditsDepleted,
    WorkspaceMemberCreditsDepleted,
    WorkspaceOwnerUsageLimitReached,
    WorkspaceMemberUsageLimitReached,
}

impl FromStr for RateLimitReachedType {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "rate_limit_reached" => Ok(Self::RateLimitReached),
            "workspace_owner_credits_depleted" => Ok(Self::WorkspaceOwnerCreditsDepleted),
            "workspace_member_credits_depleted" => Ok(Self::WorkspaceMemberCreditsDepleted),
            "workspace_owner_usage_limit_reached" => Ok(Self::WorkspaceOwnerUsageLimitReached),
            "workspace_member_usage_limit_reached" => Ok(Self::WorkspaceMemberUsageLimitReached),
            other => Err(format!("unknown rate limit reached type: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct RateLimitWindow {
    /// Percentage (0-100) of the window that has been consumed.
    pub used_percent: f64,
    /// Rolling window duration, in minutes.
    #[ts(type = "number | null")]
    pub window_minutes: Option<i64>,
    /// Unix timestamp (seconds since epoch) when the window resets.
    #[ts(type = "number | null")]
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema, TS)]
pub struct SpendControlLimitSnapshot {
    pub limit: String,
    pub used: String,
    pub remaining_percent: i32,
    pub resets_at: i64,
}

// Includes prompts, tools and space to call compact.
const BASELINE_TOKENS: i64 = 12000;

impl TokenUsage {
    pub fn empty_ledger() -> Self {
        Self {
            coverage: Some(TokenUsageCoverage {
                reported_receipts: Some(0),
                ..Default::default()
            }),
            ..Self::default()
        }
    }

    pub fn missing_receipt() -> Self {
        Self {
            coverage: Some(TokenUsageCoverage {
                receipts: 1,
                reported_receipts: Some(0),
                ..Default::default()
            }),
            ..Self::default()
        }
    }

    pub fn known_input(&self) -> Option<i64> {
        self.known(self.input_tokens, |c| c.input_tokens)
    }
    pub fn known_cached_input(&self) -> Option<i64> {
        self.known(self.cached_input_tokens, |c| c.cached_input_tokens)
    }
    pub fn known_output(&self) -> Option<i64> {
        self.known(self.output_tokens, |c| c.output_tokens)
    }
    pub fn known_reasoning_output(&self) -> Option<i64> {
        self.known(self.reasoning_output_tokens, |c| c.reasoning_output_tokens)
    }
    pub fn known_total(&self) -> Option<i64> {
        if self.coverage.as_ref()?.invalid_receipts != 0 {
            return None;
        }
        self.known(self.total_tokens, |c| c.total_tokens)
    }
    pub fn known_visible_output(&self) -> Option<i64> {
        if self.coverage.as_ref()?.invalid_receipts != 0 {
            return None;
        }
        self.known_output()?
            .checked_sub(self.known_reasoning_output()?)
            .filter(|tokens| *tokens >= 0)
    }
    pub fn known_non_cached_input(&self) -> Option<i64> {
        if self.coverage.as_ref()?.invalid_receipts != 0 {
            return None;
        }
        self.known_input()?
            .checked_sub(self.known_cached_input()?)
            .filter(|tokens| *tokens >= 0)
    }

    pub fn known_cache_write_input(&self) -> Option<i64> {
        let coverage = self.coverage.as_ref()?;
        self.known(coverage.cache_write_input_tokens, |c| {
            c.cache_write_receipts
        })
    }

    /// Only valid for providers whose input classes are disjoint read/write/ordinary.
    /// No provider convention is inferred from an absent cache-write field.
    pub fn known_ordinary_input(&self) -> Option<i64> {
        if self.coverage.as_ref()?.invalid_receipts != 0 {
            return None;
        }
        self.known_non_cached_input()?
            .checked_sub(self.known_cache_write_input()?)
            .filter(|tokens| *tokens >= 0)
    }

    fn known(&self, value: i64, reported: impl FnOnce(&TokenUsageCoverage) -> u64) -> Option<i64> {
        let coverage = self.coverage.as_ref()?;
        (!coverage.saturated && reported(coverage) == coverage.receipts && value >= 0)
            .then_some(value)
    }

    pub fn difference(&self, before: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(before.input_tokens).max(0),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(before.cached_input_tokens)
                .max(0),
            output_tokens: self
                .output_tokens
                .saturating_sub(before.output_tokens)
                .max(0),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(before.reasoning_output_tokens)
                .max(0),
            total_tokens: self.total_tokens.saturating_sub(before.total_tokens).max(0),
            coverage: match (&self.coverage, &before.coverage) {
                (Some(after), Some(before)) => Some(after.difference(before)),
                (Some(after), None)
                    if before.input_tokens == 0
                        && before.cached_input_tokens == 0
                        && before.output_tokens == 0
                        && before.reasoning_output_tokens == 0
                        && before.total_tokens == 0 =>
                {
                    Some(*after)
                }
                _ => None,
            },
        }
    }

    pub fn is_zero(&self) -> bool {
        self.total_tokens == 0
    }

    pub fn cached_input(&self) -> i64 {
        self.cached_input_tokens.max(0)
    }

    pub fn non_cached_input(&self) -> i64 {
        self.input_tokens.saturating_sub(self.cached_input()).max(0)
    }

    /// Derived uncached input + output, not the provider's total token count.
    pub fn blended_total(&self) -> i64 {
        self.non_cached_input()
            .saturating_add(self.output_tokens.max(0))
    }

    pub fn tokens_in_context_window(&self) -> i64 {
        self.total_tokens
    }

    /// Estimate the remaining user-controllable percentage of the model's context window.
    ///
    /// `context_window` is the total size of the model's context window.
    /// `BASELINE_TOKENS` should capture tokens that are always present in
    /// the context (e.g., system prompt and fixed tool instructions) so that
    /// the percentage reflects the portion the user can influence.
    ///
    /// This normalizes both the numerator and denominator by subtracting the
    /// baseline, so immediately after the first prompt the UI shows 100% left
    /// and trends toward 0% as the user fills the effective window.
    pub fn percent_of_context_window_remaining(&self, context_window: i64) -> i64 {
        if context_window <= BASELINE_TOKENS {
            return 0;
        }

        let effective_window = context_window - BASELINE_TOKENS;
        let used = self
            .tokens_in_context_window()
            .saturating_sub(BASELINE_TOKENS)
            .max(0);
        let remaining = (effective_window - used).max(0);
        ((remaining as f64 / effective_window as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as i64
    }

    /// In-place nonnegative, saturating element-wise sum, including restored DTOs.
    pub fn add_assign(&mut self, other: &TokenUsage) {
        if let Some(coverage) = self.coverage.as_mut() {
            if let Some(other_coverage) = &other.coverage {
                coverage.add_assign(other_coverage);
                coverage.saturated |= [
                    (self.input_tokens, other.input_tokens),
                    (self.cached_input_tokens, other.cached_input_tokens),
                    (self.output_tokens, other.output_tokens),
                    (self.reasoning_output_tokens, other.reasoning_output_tokens),
                    (self.total_tokens, other.total_tokens),
                ]
                .into_iter()
                .any(|(left, right)| left.max(0).checked_add(right.max(0)).is_none());
            } else {
                self.coverage = None;
            }
        }
        self.input_tokens = self
            .input_tokens
            .max(0)
            .saturating_add(other.input_tokens.max(0));
        self.cached_input_tokens = self
            .cached_input_tokens
            .max(0)
            .saturating_add(other.cached_input_tokens.max(0));
        self.output_tokens = self
            .output_tokens
            .max(0)
            .saturating_add(other.output_tokens.max(0));
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .max(0)
            .saturating_add(other.reasoning_output_tokens.max(0));
        self.total_tokens = self
            .total_tokens
            .max(0)
            .saturating_add(other.total_tokens.max(0));
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FinalOutput {
    pub token_usage: TokenUsage,
}

impl From<TokenUsage> for FinalOutput {
    fn from(token_usage: TokenUsage) -> Self {
        Self { token_usage }
    }
}

impl fmt::Display for FinalOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let token_usage = &self.token_usage;

        let optional = |value: Option<i64>| {
            value
                .map(format_with_separators)
                .unwrap_or_else(|| "unknown".to_string())
        };
        write!(
            f,
            "Token usage (thread, observed subtotal): total={} input={} cached={} uncached={} output={} reasoning={}",
            format_with_separators(token_usage.total_tokens),
            format_with_separators(token_usage.input_tokens),
            optional(token_usage.known_cached_input()),
            optional(token_usage.known_non_cached_input()),
            format_with_separators(token_usage.output_tokens),
            optional(token_usage.known_reasoning_output()),
        )?;
        match token_usage.coverage.as_ref() {
            Some(coverage) if !coverage.saturated => write!(
                f,
                "; receipts input={}/{} cached={}/{} output={}/{} total={}/{} invalid={}",
                coverage.input_tokens,
                coverage.receipts,
                coverage.cached_input_tokens,
                coverage.receipts,
                coverage.output_tokens,
                coverage.receipts,
                coverage.total_tokens,
                coverage.receipts,
                coverage.invalid_receipts,
            ),
            _ => write!(f, "; metering unknown"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct AgentMessageEvent {
    pub message: String,
    #[serde(default)]
    pub phase: Option<MessagePhase>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema, TS)]
pub struct UserMessageEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub message: String,
    /// Image URLs sourced from `UserInput::Image`. These are safe
    /// to replay in legacy UI history events and correspond to images sent to
    /// the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    /// Detail hints for `images`, indexed in parallel. Missing entries imply
    /// default image detail behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_details: Vec<Option<ImageDetail>>,
    /// Local file paths sourced from `UserInput::LocalImage`. These are kept so
    /// the UI can reattach images when editing history. Local image prompts may
    /// include a display form of the path, but these should not be treated as
    /// API-ready URLs.
    #[serde(default)]
    pub local_images: Vec<std::path::PathBuf>,
    /// Detail hints for `local_images`, indexed in parallel. Missing entries
    /// imply default image detail behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_image_details: Vec<Option<ImageDetail>>,
    /// UI-defined spans within `message` used to render or persist special elements.
    #[serde(default)]
    pub text_elements: Vec<crate::user_input::TextElement>,
}

/// Returns the user-facing preview text for a user message.
pub fn user_message_preview(user: &UserMessageEvent) -> Option<String> {
    let message = strip_user_message_prefix(user.message.as_str());
    if !message.is_empty() {
        return Some(message.to_string());
    }
    if user
        .images
        .as_ref()
        .is_some_and(|images| !images.is_empty())
        || !user.local_images.is_empty()
    {
        return Some("[Image]".to_string());
    }
    None
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct AgentReasoningEvent {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct AgentReasoningRawContentEvent {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct AgentReasoningSectionBreakEvent {
    // load with default value so it's backward compatible with the old format.
    #[serde(default)]
    pub item_id: String,
    #[serde(default)]
    pub summary_index: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq)]
pub struct McpInvocation {
    /// Name of the MCP server as defined in the config.
    pub server: String,
    /// Name of the tool as given by the MCP server.
    pub tool: String,
    /// Arguments to the tool call.
    pub arguments: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq)]
pub struct McpToolCallBeginEvent {
    /// Identifier so this can be paired with the McpToolCallEnd event.
    pub call_id: String,
    pub invocation: McpInvocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub connector_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mcp_app_resource_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub link_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub action_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub plugin_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq)]
pub struct McpToolCallProgressEvent {
    /// Identifier of the in-progress MCP tool-call item.
    pub call_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub total: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq)]
pub struct McpToolCallEndEvent {
    /// Identifier for the corresponding McpToolCallBegin that finished.
    pub call_id: String,
    pub invocation: McpInvocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub connector_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mcp_app_resource_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub link_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub action_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub plugin_id: Option<String>,
    #[ts(type = "{ secs: number, nanos: number }")]
    pub duration: Duration,
    /// Result of the tool call. Note this could be an error.
    pub result: Result<CallToolResult, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq)]
pub struct DynamicToolCallResponseEvent {
    /// Identifier for the corresponding DynamicToolCallRequest.
    pub call_id: String,
    /// Turn ID that this dynamic tool call belongs to.
    pub turn_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Dynamic tool namespace, when one was provided.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Dynamic tool name.
    pub tool: String,
    /// Dynamic tool call arguments.
    pub arguments: serde_json::Value,
    /// Dynamic tool response content items.
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    /// Whether the tool call succeeded.
    pub success: bool,
    /// Optional error text when the tool call failed before producing a response.
    pub error: Option<String>,
    /// The duration of the dynamic tool call.
    #[ts(type = "{ secs: number, nanos: number }")]
    pub duration: Duration,
}

impl McpToolCallEndEvent {
    pub fn is_success(&self) -> bool {
        match &self.result {
            Ok(result) => !result.is_error.unwrap_or(false),
            Err(_) => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct WebSearchBeginEvent {
    pub call_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct WebSearchEndEvent {
    pub call_id: String,
    pub query: String,
    pub action: WebSearchAction,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ImageGenerationBeginEvent {
    pub call_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ImageGenerationEndEvent {
    pub call_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub revised_prompt: Option<String>,
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub saved_path: Option<AbsolutePathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ResumedHistory {
    pub conversation_id: ThreadId,
    pub history: Arc<Vec<RolloutItem>>,
    pub rollout_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub enum InitialHistory {
    #[serde(rename = "New")]
    New,
    #[serde(rename = "Cleared")]
    Cleared,
    #[serde(rename = "Resumed")]
    Resumed(ResumedHistory),
    #[serde(rename = "Forked")]
    Forked(Vec<RolloutItem>),
}

impl InitialHistory {
    pub fn scan_rollout_items(&self, mut predicate: impl FnMut(&RolloutItem) -> bool) -> bool {
        match self {
            InitialHistory::New | InitialHistory::Cleared => false,
            InitialHistory::Resumed(resumed) => resumed.history.iter().any(&mut predicate),
            InitialHistory::Forked(items) => items.iter().any(predicate),
        }
    }

    pub fn forked_from_id(&self) -> Option<ThreadId> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => meta_line.meta.forked_from_id,
                    _ => None,
                })
            }
            InitialHistory::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id),
                _ => None,
            }),
        }
    }

    pub fn session_cwd(&self) -> Option<PathBuf> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => session_cwd_from_items(&resumed.history),
            InitialHistory::Forked(items) => session_cwd_from_items(items),
        }
    }

    pub fn get_rollout_items(&self) -> &[RolloutItem] {
        match self {
            InitialHistory::New | InitialHistory::Cleared => &[],
            InitialHistory::Resumed(resumed) => &resumed.history,
            InitialHistory::Forked(items) => items,
        }
    }

    pub fn get_event_msgs(&self) -> Option<Vec<EventMsg>> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => Some(
                resumed
                    .history
                    .iter()
                    .filter_map(|ri| match ri {
                        RolloutItem::EventMsg(ev) => Some(ev.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            InitialHistory::Forked(items) => Some(
                items
                    .iter()
                    .filter_map(|ri| match ri {
                        RolloutItem::EventMsg(ev) => Some(ev.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
        }
    }

    pub fn get_base_instructions(&self) -> Option<BaseInstructions> {
        // TODO: SessionMeta should (in theory) always be first in the history, so we can probably only check the first item?
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                    _ => None,
                })
            }
            InitialHistory::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.base_instructions.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_dynamic_tools(&self) -> Option<Vec<DynamicToolSpec>> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                    _ => None,
                })
            }
            InitialHistory::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => meta_line.meta.dynamic_tools.clone(),
                _ => None,
            }),
        }
    }

    pub fn get_selected_capability_roots(&self) -> Vec<SelectedCapabilityRoot> {
        self.get_session_meta()
            .map(|meta| meta.selected_capability_roots.clone())
            .unwrap_or_default()
    }

    pub fn get_multi_agent_version(&self) -> Option<MultiAgentVersion> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                multi_agent_version_from_items(&resumed.history, Some(resumed.conversation_id))
            }
            InitialHistory::Forked(items) => {
                multi_agent_version_from_items(items, /*thread_id*/ None)
            }
        }.map(MultiAgentVersion::runtime_version)
    }

    pub fn get_history_mode(&self, default_history_mode: ThreadHistoryMode) -> ThreadHistoryMode {
        match self {
            InitialHistory::New | InitialHistory::Cleared => default_history_mode,
            // Forks copy their source rollout items as-is, so they must keep
            // the source format instead of converting to the destination default.
            InitialHistory::Resumed(_) | InitialHistory::Forked(_) => self
                .get_session_meta()
                .map(|meta| meta.history_mode)
                .unwrap_or(default_history_mode),
        }
    }

    pub fn get_latest_effective_multi_agent_mode(&self) -> Option<MultiAgentMode> {
        let items = match self {
            InitialHistory::New | InitialHistory::Cleared => return None,
            InitialHistory::Resumed(resumed) => &resumed.history,
            InitialHistory::Forked(items) => items,
        };
        items
            .iter()
            .rev()
            .find_map(|item| match item {
                RolloutItem::TurnContext(turn_context) => Some(turn_context),
                RolloutItem::SessionMeta(_)
                | RolloutItem::ToolManifest(_)
                | RolloutItem::SamplingBoundary(_)
                | RolloutItem::ResponseItem(_)
                | RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. }
                | RolloutItem::Compacted(_)
                | RolloutItem::WorldState(_)
                | RolloutItem::AdditionalContextState { .. }
                | RolloutItem::EventMsg(_) => None,
            })
            .and_then(|turn_context| turn_context.multi_agent_mode.clone())
    }

    pub fn get_resumed_session_sources(&self) -> Option<(SessionSource, Option<ThreadSource>)> {
        let meta = self.get_resumed_session_meta()?;
        Some((meta.source.clone(), meta.thread_source.clone()))
    }

    pub fn get_resumed_thread_source(&self) -> Option<ThreadSource> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.thread_source.clone())
    }

    pub fn get_session_originator(&self) -> Option<String> {
        self.get_session_meta()
            .map(|meta| meta.originator.clone())
            .filter(|originator| !originator.is_empty())
    }

    pub fn get_resumed_parent_thread_id(&self) -> Option<ThreadId> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.parent_thread_id)
    }

    fn get_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                    _ => None,
                })
            }
            InitialHistory::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                _ => None,
            }),
        }
    }

    fn get_resumed_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
                    _ => None,
                })
            }
        }
    }
}

fn session_cwd_from_items(items: &[RolloutItem]) -> Option<PathBuf> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.cwd.clone()),
        _ => None,
    })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS, Default)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum SessionSource {
    Cli,
    #[default]
    VSCode,
    Exec,
    Mcp,
    Custom(String),
    Internal(InternalSessionSource),
    SubAgent(SubAgentSource),
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
#[ts(export_to = "v2/")]
pub enum ThreadSource {
    User,
    Subagent,
    Feature(String),
    // Legacy rollout origin only; no memory worker is registered.
    MemoryConsolidation,
}

impl JsonSchema for ThreadSource {
    fn schema_name() -> String {
        "ThreadSource".to_string()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        <String>::json_schema(generator)
    }
}

impl ThreadSource {
    pub fn as_str(&self) -> &str {
        match self {
            ThreadSource::User => "user",
            ThreadSource::Subagent => "subagent",
            ThreadSource::Feature(feature) => feature,
            ThreadSource::MemoryConsolidation => "memory_consolidation",
        }
    }
}

impl fmt::Display for ThreadSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for ThreadSource {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ThreadSource> for String {
    fn from(value: ThreadSource) -> Self {
        value.to_string()
    }
}

impl FromStr for ThreadSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(ThreadSource::User),
            "subagent" => Ok(ThreadSource::Subagent),
            "memory_consolidation" => Ok(ThreadSource::MemoryConsolidation),
            other => Ok(ThreadSource::Feature(other.to_string())),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum InternalSessionSource {
    // Legacy rollout origin only; no memory worker is registered.
    MemoryConsolidation,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubAgentSource {
    Review,
    Compact,
    ThreadSpawn {
        parent_thread_id: ThreadId,
        depth: i32,
        #[serde(default)]
        agent_path: Option<AgentPath>,
        #[serde(default)]
        agent_nickname: Option<String>,
        #[serde(default, alias = "agent_type")]
        agent_role: Option<String>,
    },
    // Legacy rollout origin only; no memory worker is registered.
    MemoryConsolidation,
    Other(String),
}

impl fmt::Display for SessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionSource::Cli => f.write_str("cli"),
            SessionSource::VSCode => f.write_str("vscode"),
            SessionSource::Exec => f.write_str("exec"),
            SessionSource::Mcp => f.write_str("mcp"),
            SessionSource::Custom(source) => f.write_str(source),
            SessionSource::Internal(source) => write!(f, "internal_{source}"),
            SessionSource::SubAgent(sub_source) => write!(f, "subagent_{sub_source}"),
            SessionSource::Unknown => f.write_str("unknown"),
        }
    }
}

impl SessionSource {
    /// Shared continuation eligibility; V2 workers receive work from their parent.
    pub fn accepts_direct_user_input(&self, version: Option<MultiAgentVersion>) -> bool {
        !(version == Some(MultiAgentVersion::V2)
            && matches!(self, Self::SubAgent(SubAgentSource::ThreadSpawn { .. })))
    }

    pub fn from_startup_arg(value: &str) -> Result<Self, &'static str> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err("session source must not be empty");
        }

        let normalized = trimmed.to_ascii_lowercase();
        Ok(match normalized.as_str() {
            "cli" => SessionSource::Cli,
            "vscode" => SessionSource::VSCode,
            "exec" => SessionSource::Exec,
            "mcp" | "appserver" | "app-server" | "app_server" => SessionSource::Mcp,
            "unknown" => SessionSource::Unknown,
            _ => SessionSource::Custom(normalized),
        })
    }

    pub fn is_internal(&self) -> bool {
        matches!(self, SessionSource::Internal(_))
    }

    pub fn is_non_root_agent(&self) -> bool {
        matches!(
            self,
            SessionSource::Internal(_) | SessionSource::SubAgent(_)
        )
    }

    pub fn get_nickname(&self) -> Option<String> {
        match self {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_nickname, .. }) => {
                agent_nickname.clone()
            }
            _ => None,
        }
    }

    pub fn get_agent_role(&self) -> Option<String> {
        match self {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) => {
                agent_role.clone()
            }
            _ => None,
        }
    }

    pub fn get_agent_path(&self) -> Option<AgentPath> {
        match self {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
                agent_path.clone()
            }
            _ => None,
        }
    }

    pub fn restriction_product(&self) -> Option<Product> {
        match self {
            SessionSource::Custom(source) => Product::from_session_source_name(source),
            SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Exec
            | SessionSource::Mcp
            | SessionSource::Unknown => Some(Product::Codex),
            SessionSource::Internal(_) | SessionSource::SubAgent(_) => None,
        }
    }

    pub fn matches_product_restriction(&self, products: &[Product]) -> bool {
        products.is_empty()
            || self
                .restriction_product()
                .is_some_and(|product| product.matches_product_restriction(products))
    }

    pub fn parent_thread_id(&self) -> Option<ThreadId> {
        match self {
            SessionSource::SubAgent(subagent_source) => subagent_source.parent_thread_id(),
            SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Exec
            | SessionSource::Mcp
            | SessionSource::Custom(_)
            | SessionSource::Internal(_)
            | SessionSource::Unknown => None,
        }
    }
}

impl fmt::Display for SubAgentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubAgentSource::Review => f.write_str("review"),
            SubAgentSource::Compact => f.write_str("compact"),
            SubAgentSource::MemoryConsolidation => f.write_str("memory_consolidation"),
            SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth,
                ..
            } => {
                write!(f, "thread_spawn_{parent_thread_id}_d{depth}")
            }
            SubAgentSource::Other(other) => f.write_str(other),
        }
    }
}

impl SubAgentSource {
    pub fn kind(&self) -> &str {
        match self {
            SubAgentSource::Review => "review",
            SubAgentSource::Compact => "compact",
            SubAgentSource::ThreadSpawn { .. } => "thread_spawn",
            SubAgentSource::MemoryConsolidation => "memory_consolidation",
            SubAgentSource::Other(other) => other,
        }
    }

    pub fn parent_thread_id(&self) -> Option<ThreadId> {
        match self {
            SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            } => Some(*parent_thread_id),
            SubAgentSource::Review
            | SubAgentSource::Compact
            | SubAgentSource::MemoryConsolidation
            | SubAgentSource::Other(_) => None,
        }
    }
}

impl fmt::Display for InternalSessionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InternalSessionSource::MemoryConsolidation => f.write_str("memory_consolidation"),
        }
    }
}

fn multi_agent_version_from_items(
    items: &[RolloutItem],
    thread_id: Option<ThreadId>,
) -> Option<MultiAgentVersion> {
    let session_meta_version = items.iter().rev().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line)
            if thread_id.is_none_or(|thread_id| meta_line.meta.id == thread_id) =>
        {
            meta_line.meta.multi_agent_version
        }
        _ => None,
    });

    session_meta_version.or_else(|| {
        items.iter().rev().find_map(|item| match item {
            RolloutItem::TurnContext(turn_context) => turn_context.multi_agent_version,
            RolloutItem::SessionMeta(_)
            | RolloutItem::ToolManifest(_)
            | RolloutItem::SamplingBoundary(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::AdditionalContextState { .. }
            | RolloutItem::EventMsg(_) => None,
        })
    })
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum MultiAgentVersion {
    Disabled,
    /// Historical wire value only; live sessions use V2.
    V1,
    V2,
}

impl MultiAgentVersion {
    /// Preserve old rollout decoding without restoring the retired V1 runtime.
    pub fn runtime_version(self) -> Self {
        match self {
            Self::V1 => Self::V2,
            version => version,
        }
    }
}


#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct SessionContextWindow {
    /// UUIDv7 identity of this context window.
    pub window_id: String,
}

impl SessionContextWindow {
    pub fn new(window_id: String) -> Self {
        Self { window_id }
    }
}

/// Identity of the harness binary, independent of the repository it is editing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct SessionBuildInfo {
    pub version: String,
    pub commit: String,
    pub dirty: String,
    pub profile: String,
    pub built: String,
    /// SHA-256 of the running executable; the fields above cannot tell two
    /// dirty builds of one commit apart. Absent in older rollouts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub executable_sha256: Option<String>,
    /// Measurement names bind estimator and representation scope. Missing in
    /// older rollouts means unknown, not compatible with the current estimator.
    /// Compare ratios only within matching estimator AND scope; provider usage
    /// is not a local token estimate.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub token_accounting: BTreeMap<String, SessionTokenMeasurement>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct SessionTokenMeasurement {
    pub estimator: String,
    pub scope: String,
}

/// SessionMeta contains session-level data that doesn't correspond to a specific turn.
///
/// NOTE: There used to be an `instructions` field here, which stored user_instructions, but we
/// now save that on TurnContext. base_instructions stores the base instructions for the session,
/// and should be used when there is no config override.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, TS)]
pub struct SessionMeta {
    pub session_id: SessionId,
    pub id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    pub timestamp: String,
    pub cwd: PathBuf,
    pub originator: String,
    pub cli_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_build: Option<SessionBuildInfo>,
    #[serde(default)]
    pub source: SessionSource,
    /// Optional analytics source classification for this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<ThreadSource>,
    /// Optional random unique nickname assigned to an AgentControl-spawned sub-agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_nickname: Option<String>,
    /// Optional role (agent_role) assigned to an AgentControl-spawned sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Optional canonical agent path assigned to an AgentControl-spawned sub-agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_path: Option<String>,
    pub model_provider: Option<String>,
    /// base_instructions for the session. This *should* always be present when creating a new session,
    /// but may be missing for older sessions. If not present, fall back to rendering the base_instructions
    /// from ModelsManager.
    pub base_instructions: Option<BaseInstructions>,
    #[serde(
        default,
        deserialize_with = "crate::dynamic_tools::deserialize_dynamic_tool_specs",
        skip_serializing_if = "Option::is_none"
    )]
    pub dynamic_tools: Option<Vec<DynamicToolSpec>>,
    /// Capability roots selected for this thread by the hosting platform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_capability_roots: Vec<SelectedCapabilityRoot>,
    #[serde(default)]
    pub history_mode: ThreadHistoryMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_agent_version: Option<MultiAgentVersion>,
    /// Initial context-window identity for consumers that tail rollout JSONL before compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<SessionContextWindow>,
}

impl Default for SessionMeta {
    fn default() -> Self {
        let id = ThreadId::default();
        SessionMeta {
            session_id: id.into(),
            id,
            forked_from_id: None,
            parent_thread_id: None,
            timestamp: String::new(),
            cwd: PathBuf::new(),
            originator: String::new(),
            cli_version: String::new(),
            harness_build: None,
            source: SessionSource::default(),
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            agent_path: None,
            model_provider: None,
            base_instructions: None,
            dynamic_tools: None,
            selected_capability_roots: Vec::new(),

            history_mode: ThreadHistoryMode::default(),
            multi_agent_version: None,
            context_window: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, TS)]
pub struct SessionMetaLine {
    #[serde(flatten)]
    pub meta: SessionMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, TS)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
// This persisted public enum has hundreds of construction and match sites.
// Boxing EventMsg would be an API-wide migration for a layout-only optimization.
#[allow(clippy::large_enum_variant)]
pub enum RolloutItem {
    SessionMeta(SessionMetaLine),
    ToolManifest(ToolManifestItem),
    /// Model-invisible marker for the exact committed history prefix prepared
    /// for one physical provider attempt.
    SamplingBoundary(SamplingBoundaryItem),
    ResponseItem(ResponseItem),
    /// Legacy delivery item reconstructed as a model-visible `agent_message`.
    InterAgentCommunication(InterAgentCommunication),
    /// Local delivery metadata that is not part of the Responses API item.
    InterAgentCommunicationMetadata {
        trigger_turn: bool,
    },
    Compacted(CompactedItem),
    TurnContext(TurnContextItem),
    WorldState(WorldStateItem),
    /// Versioned, model-invisible source ownership co-recorded with its input.
    AdditionalContextState {
        version: u8,
        turn_id: String,
        state: serde_json::Value,
        /// Checkpoint-only companion owned by the AGENTS source manager, not
        /// evidence that the provider accepted a world-state baseline.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_sources: Option<serde_json::Value>,
    },
    EventMsg(EventMsg),
}

/// Persisted before consuming a physical sampling attempt. The marker's
/// position in the rollout is the history prefix transmitted by that attempt.
/// An unresolved marker makes reconstructed context provenance conservative.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct SamplingBoundaryItem {
    pub sampling_request_id: String,
    pub physical_attempt_id: String,
    /// Exact developer freshness notices in the prepared sampling prompt.
    /// Audit-only: never replay these as canonical conversation history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_freshness_notices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub unresolved_context: bool,
    /// Model-invisible observation before dispatch, never a terminal result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing_checkpoint: Option<SamplingTimingCheckpoint>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct SamplingTimingCheckpoint {
    pub observed_at_unix_ms: i64,
    /// All time and effects after this observation are unknown after process loss.
    pub tail_unknown: bool,
    pub timing: TurnTiming,
    /// Versioned SHA-256 identities of captured config layers, effective turn
    /// settings, selected model metadata, and the dispatched tool manifest.
    /// These describe runtime inputs, not the repository being edited. Missing
    /// entries are unavailable observations, never evidence of equality.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime_identity: BTreeMap<String, String>,
    /// Captured from this process even when SessionMeta belongs to an older
    /// process that created the rollout before resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_build: Option<SessionBuildInfo>,
    /// When true, `timing` keeps its cumulative scalar aggregates but carries
    /// only the model requests, tool calls, and continuation receipts that are
    /// new or changed since the previous checkpoint of the same turn. Its
    /// activity timeline keeps its scalars but carries only the intervals from
    /// the previous checkpoint's last interval on, which may have been extended.
    /// Readers rebuild those arrays by merging the turn's checkpoints in order,
    /// replacing intervals that start at or after the first one carried. Older
    /// rollouts hold full snapshots and omit this field.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incremental: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct ContextFragmentDigest {
    pub key: String,
    pub digest: String,
}

/// Identifies the exact accepted physical request that established the
/// authoritative context baseline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct AcceptedAttemptProvenance {
    pub sampling_request_id: String,
    pub physical_attempt_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct TurnContextProvenance {
    pub accepted_attempt: AcceptedAttemptProvenance,
    #[serde(default)]
    pub fragment_digests: Vec<ContextFragmentDigest>,
}

/// Canonical tool surface used for a model request.
///
/// The first occurrence of a hash carries either `manifest` or a delta from
/// `base_hash`. Later occurrences carry only `hash` and act as references.
/// Legacy records with a required `manifest` field remain valid full entries.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
pub struct ToolManifestItem {
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<ToolManifestDeltaEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<ToolManifestDeltaRemoval>,
}

impl ToolManifestItem {
    pub fn full(hash: String, manifest: Value) -> Self {
        Self {
            hash,
            manifest: Some(manifest),
            base_hash: None,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    pub fn reference(hash: String) -> Self {
        Self {
            hash,
            manifest: None,
            base_hash: None,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    pub fn delta(
        hash: String,
        base_hash: String,
        added: Vec<ToolManifestDeltaEntry>,
        removed: Vec<ToolManifestDeltaRemoval>,
    ) -> Self {
        Self {
            hash,
            manifest: None,
            base_hash: Some(base_hash),
            added,
            removed,
        }
    }

    pub fn is_reference(&self) -> bool {
        self.manifest.is_none() && self.base_hash.is_none()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
pub struct ToolManifestDeltaEntry {
    pub collection: String,
    pub name: String,
    pub index: usize,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash, JsonSchema, TS)]
pub struct ToolManifestDeltaRemoval {
    pub collection: String,
    pub name: String,
}

/// Persisted comparison state used to resume model-visible world-state diffing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
pub struct WorldStateItem {
    /// Full snapshots establish a new baseline; patches update the current baseline.
    pub full: bool,
    pub state: Value,
}

impl WorldStateItem {
    pub fn full(state: Value) -> Self {
        Self { full: true, state }
    }

    pub fn patch(state: Value) -> Self {
        Self { full: false, state }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, JsonSchema, TS)]
pub struct CompactedItem {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_history: Option<Vec<ResponseItem>>,
    /// Monotonic position of this context window within the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_number: Option<u64>,
    /// UUIDv7 identity of the first context window in this thread's window chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_window_id: Option<String>,
    /// UUIDv7 identity of the context window immediately before this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_window_id: Option<String>,
    /// UUIDv7 identity of this context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}

impl From<CompactedItem> for ResponseItem {
    fn from(value: CompactedItem) -> Self {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: value.message,
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, TS)]
pub struct TurnContextNetworkItem {
    pub allowed_domains: Vec<String>,
    pub denied_domains: Vec<String>,
}

/// Persist once per real user turn after computing that turn's model-visible
/// context updates, and again after mid-turn compaction when replacement
/// history re-establishes full context, so resume/fork replay can recover the
/// latest durable baseline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
pub struct TurnContextItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub cwd: AbsolutePathBuf,
    /// Effective workspace roots used to materialize symbolic
    /// `:workspace_roots` filesystem permissions in `permission_profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_roots: Option<Vec<AbsolutePathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub approval_policy: AskForApproval,
    pub sandbox_policy: SandboxPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<PermissionProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<TurnContextNetworkItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_system_sandbox_policy: Option<FileSystemSandboxPolicy>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comp_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub personality: Option<Personality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration_mode: Option<CollaborationMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_version: Option<MultiAgentVersion>,
    /// Effective model-visible mode used as the durable context-diff baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_agent_mode: Option<MultiAgentMode>,
    /// Authoritative validation policy. Legacy contexts inherit configured defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_mode: Option<crate::validation::ValidationMode>,
    /// Root thread and turn whose validation attempts this context shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_root_turn: Option<(String, String)>,
    /// Settled admission identities, retained across compaction and recovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_attempts: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ReasoningEffortConfig>,
    /// Durable proof that this baseline was transmitted and accepted by the
    /// exact physical request named here. Legacy items omit this field and are
    /// reconstructed as an unknown baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_provenance: Option<TurnContextProvenance>,
}

impl TurnContextItem {
    pub fn permission_profile(&self) -> PermissionProfile {
        self.permission_profile.clone().unwrap_or_else(|| {
            let file_system_sandbox_policy =
                self.file_system_sandbox_policy.clone().unwrap_or_else(|| {
                    FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
                        &self.sandbox_policy,
                        self.cwd.as_path(),
                    )
                });
            PermissionProfile::from_runtime_permissions_with_enforcement(
                SandboxEnforcement::from_legacy_sandbox_policy(&self.sandbox_policy),
                &file_system_sandbox_policy,
                NetworkSandboxPolicy::from(&self.sandbox_policy),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "mode", content = "limit", rename_all = "snake_case")]
pub enum TruncationPolicy {
    Bytes(usize),
    Tokens(usize),
}

impl From<crate::openai_models::TruncationPolicyConfig> for TruncationPolicy {
    fn from(config: crate::openai_models::TruncationPolicyConfig) -> Self {
        let limit = usize::try_from(config.limit.max(0)).unwrap_or(usize::MAX);
        match config.mode {
            crate::openai_models::TruncationMode::Bytes => Self::Bytes(limit),
            crate::openai_models::TruncationMode::Tokens => Self::Tokens(limit),
        }
    }
}

impl TruncationPolicy {
    pub fn truncate_text(&self, content: &str) -> String {
        match self {
            TruncationPolicy::Bytes(bytes) => {
                codex_utils_string::truncate_middle_chars(content, *bytes)
            }
            TruncationPolicy::Tokens(tokens) => {
                codex_utils_string::truncate_middle_with_token_budget(content, *tokens).0
            }
        }
    }

    pub fn token_budget(&self) -> usize {
        match self {
            TruncationPolicy::Bytes(bytes) => {
                usize::try_from(codex_utils_string::approx_tokens_from_byte_count(*bytes))
                    .unwrap_or(usize::MAX)
            }
            TruncationPolicy::Tokens(tokens) => *tokens,
        }
    }

    pub fn byte_budget(&self) -> usize {
        match self {
            TruncationPolicy::Bytes(bytes) => *bytes,
            TruncationPolicy::Tokens(tokens) => {
                codex_utils_string::approx_bytes_for_tokens(*tokens)
            }
        }
    }
}

impl Mul<f64> for TruncationPolicy {
    type Output = Self;

    fn mul(self, multiplier: f64) -> Self::Output {
        match self {
            TruncationPolicy::Bytes(bytes) => {
                TruncationPolicy::Bytes((bytes as f64 * multiplier).ceil() as usize)
            }
            TruncationPolicy::Tokens(tokens) => {
                TruncationPolicy::Tokens((tokens as f64 * multiplier).ceil() as usize)
            }
        }
    }
}

pub const CURRENT_ROLLOUT_FORMAT_VERSION: u32 = 1;

#[derive(Clone)]
pub struct RolloutLine {
    pub timestamp: String,
    pub item: RolloutItem,
}

#[derive(Serialize, JsonSchema)]
struct RolloutLineSchema<'a> {
    pub timestamp: &'a str,
    pub format_version: u32,
    #[serde(flatten)]
    pub item: &'a RolloutItem,
}

impl JsonSchema for RolloutLine {
    fn schema_name() -> String {
        "RolloutLine".to_string()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::schema::Schema {
        RolloutLineSchema::json_schema(generator)
    }
}

impl Serialize for RolloutLine {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        RolloutLineSchema {
            timestamp: &self.timestamp,
            format_version: CURRENT_ROLLOUT_FORMAT_VERSION,
            item: &self.item,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RolloutLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut value = Value::deserialize(deserializer)?;
        let fields = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("rollout line must be an object"))?;
        let timestamp = fields
            .remove("timestamp")
            .ok_or_else(|| D::Error::missing_field("timestamp"))?;
        let timestamp = serde_json::from_value(timestamp).map_err(D::Error::custom)?;
        let version = fields
            .remove("format_version")
            .map(serde_json::from_value::<u32>)
            .transpose()
            .map_err(D::Error::custom)?
            .unwrap_or(0);

        match version {
            0 => migrate_rollout_v0(fields).map_err(D::Error::custom)?,
            CURRENT_ROLLOUT_FORMAT_VERSION => {
                reject_v0_fields_in_current_rollout(fields).map_err(D::Error::custom)?
            }
            _ => {
                return Err(D::Error::custom(format!(
                    "unsupported rollout format version {version}; this build supports version {CURRENT_ROLLOUT_FORMAT_VERSION}"
                )));
            }
        }

        let item = serde_json::from_value(Value::Object(std::mem::take(fields)))
            .map_err(D::Error::custom)?;
        Ok(Self { timestamp, item })
    }
}

fn migrate_rollout_v0(fields: &mut serde_json::Map<String, Value>) -> Result<(), String> {
    let item_type = fields
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let Some(payload) = fields.get_mut("payload").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    match item_type.as_deref() {
        Some("session_meta") => {
            if !payload.contains_key("session_id") {
                let thread_id = payload
                    .get("id")
                    .cloned()
                    .ok_or_else(|| "v0 session metadata is missing id".to_string())?;
                payload.insert("session_id".to_string(), thread_id);
            }
            if let Some(agent_type) = payload.remove("agent_type") {
                payload
                    .entry("agent_role".to_string())
                    .or_insert(agent_type);
            }
        }
        Some("turn_context") => {
            payload.remove("summary");
        }
        Some("event_msg")
            if payload.get("type").and_then(Value::as_str) == Some("collab_agent_spawn_end") =>
        {
            // Missing historical evidence is unavailable, not a model/effort
            // selection. Keep recorded values, including custom effort names.
            payload.entry("model".to_string()).or_insert(Value::Null);
            payload
                .entry("reasoning_effort".to_string())
                .or_insert(Value::Null);
        }
        _ => {}
    }
    Ok(())
}

fn reject_v0_fields_in_current_rollout(
    fields: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    let item_type = fields.get("type").and_then(Value::as_str);
    let payload = fields.get("payload").and_then(Value::as_object);
    if item_type == Some("event_msg")
        && let Some(payload) = payload
        && matches!(
            payload.get("type").and_then(Value::as_str),
            Some("collab_agent_spawn_begin" | "collab_agent_spawn_end")
        )
    {
        for field in ["model", "reasoning_effort"] {
            if !payload.contains_key(field) {
                return Err(format!("missing field `{field}`"));
            }
        }
    }
    if item_type == Some("turn_context")
        && payload.is_some_and(|value| value.contains_key("summary"))
    {
        return Err("rollout format version 1 does not accept turn_context.summary".to_string());
    }
    if item_type == Some("session_meta")
        && payload.is_some_and(|value| value.contains_key("agent_type"))
    {
        return Err("rollout format version 1 does not accept session_meta.agent_type".to_string());
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, TS)]
pub struct GitInfo {
    /// Current commit hash (SHA)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<GitSha>,
    /// Current branch name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Repository URL (if available from remote)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDelivery {
    Inline,
    Detached,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
pub enum ReviewTarget {
    /// Review the working tree: staged, unstaged, and untracked files.
    UncommittedChanges,

    /// Review changes between the current branch and the given base branch.
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    BaseBranch { branch: String },

    /// Review the changes introduced by a specific commit.
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Commit {
        sha: String,
        /// Optional human-readable label (e.g., commit subject) for UIs.
        title: Option<String>,
    },

    /// Arbitrary instructions provided by the user.
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Custom { instructions: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
/// Review request sent to the review session.
pub struct ReviewRequest {
    pub target: ReviewTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub user_facing_hint: Option<String>,
}

/// Structured review result produced by a child review session.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ReviewOutputEvent {
    pub findings: Vec<ReviewFinding>,
    pub overall_correctness: String,
    pub overall_explanation: String,
    pub overall_confidence_score: f32,
}

impl Default for ReviewOutputEvent {
    fn default() -> Self {
        Self {
            findings: Vec::new(),
            overall_correctness: String::default(),
            overall_explanation: String::default(),
            overall_confidence_score: 0.0,
        }
    }
}

/// A single review finding describing an observed issue or recommendation.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ReviewFinding {
    pub title: String,
    pub body: String,
    pub confidence_score: f32,
    pub priority: i32,
    pub code_location: ReviewCodeLocation,
}

/// Location of the code related to a review finding.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ReviewCodeLocation {
    pub absolute_file_path: PathBuf,
    pub line_range: ReviewLineRange,
}

/// Inclusive line range in a file associated with the finding.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ReviewLineRange {
    pub start: u32,
    pub end: u32,
}

#[derive(
    Debug, Clone, Copy, Display, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecCommandSource {
    #[default]
    Agent,
    UserShell,
    UnifiedExecStartup,
    UnifiedExecInteraction,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ExecCommandStatus {
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ExecCommandBeginEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<crate::items::EnvironmentProvenance>,
    /// Identifier so this can be paired with the ExecCommandEnd event.
    pub call_id: String,
    /// Identifier for the underlying PTY process (when available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub process_id: Option<String>,
    /// Turn ID that this command belongs to.
    pub turn_id: String,
    #[serde(default)]
    pub started_at_ms: i64,
    /// The command to be executed.
    pub command: Vec<String>,
    /// The command's working directory if not the default cwd for the agent.
    pub cwd: PathUri,
    pub parsed_cmd: Vec<ParsedCommand>,
    /// Where the command originated. Defaults to Agent for backward compatibility.
    #[serde(default)]
    pub source: ExecCommandSource,
    /// Raw input sent to a unified exec session (if this is an interaction event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interaction_input: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ExecCommandEndEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<crate::items::EnvironmentProvenance>,
    /// Identifier for the ExecCommandBegin that finished.
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub output_metadata: Option<crate::items::CommandExecutionOutputMetadata>,
    /// Identifier for the underlying PTY process (when available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub process_id: Option<String>,
    /// Turn ID that this command belongs to.
    pub turn_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// The command that was executed.
    pub command: Vec<String>,
    /// The command's working directory if not the default cwd for the agent.
    pub cwd: PathUri,
    pub parsed_cmd: Vec<ParsedCommand>,
    /// Where the command originated. Defaults to Agent for backward compatibility.
    #[serde(default)]
    pub source: ExecCommandSource,
    /// Raw input sent to a unified exec session (if this is an interaction event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interaction_input: Option<String>,

    /// Captured stdout
    pub stdout: String,
    /// Captured stderr
    pub stderr: String,
    /// Captured aggregated output
    #[serde(default)]
    pub aggregated_output: String,
    /// The observed command exit code; null when the platform did not supply one.
    pub exit_code: Option<i32>,
    /// The duration of the command execution.
    #[ts(type = "{ secs: number, nanos: number }")]
    pub duration: Duration,
    /// Formatted output from the command, as seen by the model.
    pub formatted_output: String,
    /// Completion status for this command execution.
    pub status: ExecCommandStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ViewImageToolCallEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<crate::items::EnvironmentProvenance>,
    /// Identifier for the originating tool call.
    pub call_id: String,
    /// Filesystem path resolved for the selected environment.
    ///
    /// This core event is not exposed directly in the app-server API. App-server
    /// converts the path to `LegacyAppPathString` when building its public item.
    pub path: PathUri,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct ExecCommandOutputDeltaEvent {
    /// Identifier for the ExecCommandBegin that produced this chunk.
    pub call_id: String,
    /// Which stream produced this chunk.
    pub stream: ExecOutputStream,
    /// Raw bytes from the stream (may not be valid UTF-8).
    #[serde_as(as = "serde_with::base64::Base64")]
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub chunk: Vec<u8>,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct TerminalInteractionEvent {
    /// Identifier for the ExecCommandBegin that produced this chunk.
    pub call_id: String,
    /// Process id associated with the running command.
    pub process_id: String,
    /// Stdin sent to the running session.
    pub stdin: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct DeprecationNoticeEvent {
    /// Concise summary of what is deprecated.
    pub summary: String,
    /// Optional extra guidance, such as migration steps or rationale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct ThreadRolledBackEvent {
    /// Number of user turns that were removed from context.
    pub num_turns: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct StreamErrorEvent {
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<CodexErrorInfo>,
    /// Optional details about the underlying stream failure (often the same
    /// human-readable message that is surfaced as the terminal error if retries
    /// are exhausted).
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct PatchApplyBeginEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<crate::items::EnvironmentProvenance>,
    /// Identifier so this can be paired with the PatchApplyEnd event.
    pub call_id: String,
    /// Turn ID that this patch belongs to.
    /// Uses `#[serde(default)]` for backwards compatibility.
    #[serde(default)]
    pub turn_id: String,
    /// If true, there was no ApplyPatchApprovalRequest for this patch.
    pub auto_approved: bool,
    /// The changes to be applied.
    pub changes: HashMap<PathBuf, FileChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct PatchApplyUpdatedEvent {
    /// Identifier for the originating `apply_patch` tool call.
    pub call_id: String,
    /// Structured file changes parsed from the model-generated patch input so far.
    pub changes: HashMap<PathBuf, FileChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct PatchApplyEndEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub environment: Option<crate::items::EnvironmentProvenance>,
    /// Identifier for the PatchApplyBegin that finished.
    pub call_id: String,
    /// Turn ID that this patch belongs to.
    /// Uses `#[serde(default)]` for backwards compatibility.
    #[serde(default)]
    pub turn_id: String,
    /// Captured stdout (summary printed by apply_patch).
    pub stdout: String,
    /// Captured stderr (parser errors, IO failures, etc.).
    pub stderr: String,
    /// Whether the patch was applied successfully.
    pub success: bool,
    /// The changes that were applied (mirrors PatchApplyBeginEvent::changes).
    #[serde(default)]
    pub changes: HashMap<PathBuf, FileChange>,
    /// Completion status for this patch application.
    pub status: PatchApplyStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PatchApplyStatus {
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TurnDiffEvent {
    pub unified_diff: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct McpStartupUpdateEvent {
    /// Server name being started.
    pub server: String,
    /// Current startup status.
    pub status: McpStartupStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case", tag = "state")]
#[ts(rename_all = "snake_case", tag = "state")]
pub enum McpStartupStatus {
    Starting,
    Ready,
    Failed {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional = nullable)]
        reason: Option<McpStartupFailureReason>,
    },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum McpStartupFailureReason {
    ReauthenticationRequired,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, Default)]
pub struct McpStartupCompleteEvent {
    pub ready: Vec<String>,
    pub failed: Vec<McpStartupFailure>,
    pub cancelled: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct McpStartupFailure {
    pub server: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum McpAuthStatus {
    Unsupported,
    NotLoggedIn,
    BearerToken,
    OAuth,
}

impl fmt::Display for McpAuthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            McpAuthStatus::Unsupported => "Unsupported",
            McpAuthStatus::NotLoggedIn => "Not logged in",
            McpAuthStatus::BearerToken => "Bearer token",
            McpAuthStatus::OAuth => "OAuth",
        };
        f.write_str(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum Product {
    #[serde(alias = "CHATGPT")]
    Chatgpt,
    #[serde(alias = "CODEX")]
    Codex,
    #[serde(alias = "ATLAS")]
    Atlas,
}
impl Product {
    pub fn to_app_platform(self) -> &'static str {
        match self {
            Self::Chatgpt => "chat",
            Self::Codex => "codex",
            Self::Atlas => "atlas",
        }
    }

    pub fn from_session_source_name(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "chatgpt" => Some(Self::Chatgpt),
            "codex" => Some(Self::Codex),
            "atlas" => Some(Self::Atlas),
            _ => None,
        }
    }

    pub fn matches_product_restriction(&self, products: &[Product]) -> bool {
        products.is_empty() || products.contains(self)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SkillScope {
    User,
    Repo,
    System,
    Admin,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    /// Legacy short_description from SKILL.md. Prefer SKILL.json interface.short_description.
    pub short_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interface: Option<SkillInterface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependencies: Option<SkillDependencies>,
    pub path: AbsolutePathBuf,
    pub scope: SkillScope,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq, Eq)]
pub struct SkillInterface {
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub icon_small: Option<AbsolutePathBuf>,
    pub icon_large: Option<AbsolutePathBuf>,
    pub brand_color: Option<String>,
    pub default_prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq, Eq)]
pub struct SkillDependencies {
    pub tools: Vec<SkillToolDependency>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS, PartialEq, Eq)]
pub struct SkillToolDependency {
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub r#type: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub url: Option<String>,
}


#[derive(Debug, Clone, Serialize, JsonSchema, TS)]
pub struct SessionConfiguredEvent {
    pub session_id: SessionId,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from_id: Option<ThreadId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<ThreadId>,
    /// Optional analytics source classification for this thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_source: Option<ThreadSource>,

    /// Optional user-facing thread name (may be unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub thread_name: Option<String>,

    /// Tell the client what model is being queried.
    pub model: String,

    pub model_provider_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,

    /// When to escalate for approval for execution
    pub approval_policy: AskForApproval,

    /// Canonical effective permissions for commands executed in the session.
    pub permission_profile: PermissionProfile,

    /// Named or implicit built-in profile that produced `permission_profile`,
    /// when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub active_permission_profile: Option<ActivePermissionProfile>,

    /// Working directory that should be treated as the *root* of the
    /// session.
    pub cwd: AbsolutePathBuf,

    /// The effort the model is putting into reasoning about the user's request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortConfig>,

    /// Optional initial messages (as events) for resumed sessions.
    /// When present, UIs can use these to seed the history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_messages: Option<Vec<EventMsg>>,


    /// Path in which the rollout is stored. Can be `None` for ephemeral threads
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<PathBuf>,
}

impl<'de> Deserialize<'de> for SessionConfiguredEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            session_id: SessionId,
            #[serde(default)]
            thread_id: Option<ThreadId>,
            forked_from_id: Option<ThreadId>,
            parent_thread_id: Option<ThreadId>,
            #[serde(default)]
            thread_source: Option<ThreadSource>,
            #[serde(default)]
            thread_name: Option<String>,
            model: String,
            model_provider_id: String,
            service_tier: Option<String>,
            approval_policy: AskForApproval,
            // `SessionConfiguredEvent` is persisted into rollout history. Older
            // rollouts only have `sandbox_policy`, so accept it on deserialize
            // and immediately project it into the canonical `permission_profile`.
            sandbox_policy: Option<SandboxPolicy>,
            permission_profile: Option<PermissionProfile>,
            #[serde(default)]
            active_permission_profile: Option<ActivePermissionProfile>,
            cwd: AbsolutePathBuf,
            reasoning_effort: Option<ReasoningEffortConfig>,
            initial_messages: Option<Vec<EventMsg>>,
            rollout_path: Option<PathBuf>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let permission_profile = match (wire.permission_profile, wire.sandbox_policy) {
            (Some(permission_profile), _) => permission_profile,
            (None, Some(sandbox_policy)) => PermissionProfile::from_legacy_sandbox_policy_for_cwd(
                &sandbox_policy,
                wire.cwd.as_path(),
            ),
            (None, None) => {
                return Err(serde::de::Error::missing_field("permission_profile"));
            }
        };

        Ok(Self {
            session_id: wire.session_id,
            thread_id: wire.thread_id.unwrap_or_else(|| wire.session_id.into()),
            forked_from_id: wire.forked_from_id,
            parent_thread_id: wire.parent_thread_id,
            thread_source: wire.thread_source,
            thread_name: wire.thread_name,
            model: wire.model,
            model_provider_id: wire.model_provider_id,
            service_tier: wire.service_tier,
            approval_policy: wire.approval_policy,
            permission_profile,
            active_permission_profile: wire.active_permission_profile,
            cwd: wire.cwd,
            reasoning_effort: wire.reasoning_effort,
            initial_messages: wire.initial_messages,
            rollout_path: wire.rollout_path,
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}





#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct ThreadGoal {
    pub thread_id: ThreadId,
    pub objective: String,
    pub status: ThreadGoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "protocol/")]
pub struct ThreadGoalUpdatedEvent {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    pub goal: ThreadGoal,
}

/// User's decision in response to an ExecApprovalRequest.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// User has approved this command and the agent should execute it.
    Approved,

    /// User has approved this command and wants to apply the proposed execpolicy
    /// amendment so future matching commands are permitted.
    ApprovedExecpolicyAmendment {
        proposed_execpolicy_amendment: ExecPolicyAmendment,
    },

    /// User has approved this request and wants future prompts in the same
    /// session-scoped approval cache to be automatically approved for the
    /// remainder of the session.
    ApprovedForSession,

    /// User chose to persist a network policy rule (allow/deny) for future
    /// requests to the same host.
    NetworkPolicyAmendment {
        network_policy_amendment: NetworkPolicyAmendment,
    },

    /// User has denied this command and the agent should not execute it, but
    /// it should continue the session and try something else.
    #[default]
    Denied,

    /// User has denied this command and the agent should not do anything until
    /// the user's next command.
    Abort,
}

impl ReviewDecision {
    /// Returns an opaque version of the decision without PII. We can't use an ignored flag
    /// on `serde` because the serialization is required by some surfaces.
    pub fn to_opaque_string(&self) -> &'static str {
        match self {
            ReviewDecision::Approved => "approved",
            ReviewDecision::ApprovedExecpolicyAmendment { .. } => "approved_with_amendment",
            ReviewDecision::ApprovedForSession => "approved_for_session",
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } => match network_policy_amendment.action {
                NetworkPolicyRuleAction::Allow => "approved_with_network_policy_allow",
                NetworkPolicyRuleAction::Deny => "denied_with_network_policy_deny",
            },
            ReviewDecision::Denied => "denied",
            ReviewDecision::Abort => "abort",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(tag = "type")]
pub enum FileChange {
    Add {
        content: String,
    },
    Delete {
        content: String,
    },
    Update {
        unified_diff: String,
        move_path: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct Chunk {
    /// 1-based line index of the first line in the original file
    pub orig_index: u32,
    pub deleted_lines: Vec<String>,
    pub inserted_lines: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, TS)]
pub struct TurnAbortedEvent {
    pub turn_id: Option<String>,
    pub reason: TurnAbortReason,
    /// Unix timestamp (in seconds) when the turn was aborted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub completed_at: Option<i64>,
    /// Duration between turn start and abort in milliseconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null", optional)]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub timing: Option<TurnTiming>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum TurnAbortReason {
    Interrupted,
    Replaced,
    ReviewEnded,
    BudgetLimited,
    InternalError,
    /// Recovery found an open turn after its owning process disappeared.
    ProcessLost,
    /// The session shut down (application exit, thread unload, or delegate
    /// teardown) while the turn was running; the user did not interrupt it.
    SessionShutdown,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabAgentSpawnBeginEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub started_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Initial prompt sent to the agent. Can be empty to prevent CoT leaking at the
    /// beginning.
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffortConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct CollabAgentRef {
    /// Thread ID of the receiver/new agent.
    pub thread_id: ThreadId,
    /// Optional nickname assigned to an AgentControl-spawned sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_nickname: Option<String>,
    /// Optional role (agent_role) assigned to an AgentControl-spawned sub-agent.
    #[serde(default, alias = "agent_type", skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct CollabAgentStatusEntry {
    /// Thread ID of the receiver/new agent.
    pub thread_id: ThreadId,
    /// Optional nickname assigned to an AgentControl-spawned sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_nickname: Option<String>,
    /// Optional role (agent_role) assigned to an AgentControl-spawned sub-agent.
    #[serde(default, alias = "agent_type", skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    /// Last known status of the agent.
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabAgentSpawnEndEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the newly spawned agent, if it was created.
    pub new_thread_id: Option<ThreadId>,
    /// Optional nickname assigned to the new agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_agent_nickname: Option<String>,
    /// Optional role assigned to the new agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_agent_role: Option<String>,
    /// Initial prompt sent to the agent. Can be empty to prevent CoT leaking at the
    /// beginning.
    pub prompt: String,
    /// Effective model used by the spawned agent after inheritance and role overrides.
    pub model: Option<String>,
    /// Effective reasoning effort used by the spawned agent after inheritance and role overrides.
    pub reasoning_effort: Option<ReasoningEffortConfig>,
    /// Last known status of the new agent reported to the sender agent.
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabAgentInteractionBeginEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub started_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
    /// Prompt sent from the sender to the receiver. Can be empty to prevent CoT
    /// leaking at the beginning.
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabAgentInteractionEndEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
    /// Optional nickname assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_nickname: Option<String>,
    /// Optional role assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_role: Option<String>,
    /// Prompt sent from the sender to the receiver. Can be empty to prevent CoT
    /// leaking at the beginning.
    pub prompt: String,
    /// Last known status of the receiver agent reported to the sender agent.
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum SubAgentActivityKind {
    Started,
    Interacted,
    Interrupted,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct SubAgentActivityEvent {
    pub event_id: String,
    #[serde(default)]
    pub occurred_at_ms: i64,
    /// Thread ID of the affected sub-agent.
    pub agent_thread_id: ThreadId,
    /// Canonical v2 path of the affected sub-agent.
    pub agent_path: AgentPath,
    pub kind: SubAgentActivityKind,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabWaitingBeginEvent {
    #[serde(default)]
    pub started_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receivers.
    pub receiver_thread_ids: Vec<ThreadId>,
    /// Optional nicknames/roles for receivers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receiver_agents: Vec<CollabAgentRef>,
    /// ID of the waiting call.
    pub call_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabWaitingEndEvent {
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// ID of the waiting call.
    pub call_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Optional receiver metadata paired with final statuses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_statuses: Vec<CollabAgentStatusEntry>,
    /// Last known status of the receiver agents reported to the sender agent.
    pub statuses: HashMap<ThreadId, AgentStatus>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabCloseBeginEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub started_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabCloseEndEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
    /// Optional nickname assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_nickname: Option<String>,
    /// Optional role assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_role: Option<String>,
    /// Last known status of the receiver agent reported to the sender agent before
    /// the close.
    pub status: AgentStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabResumeBeginEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub started_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
    /// Optional nickname assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_nickname: Option<String>,
    /// Optional role assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_role: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct CollabResumeEndEvent {
    /// Identifier for the collab tool call.
    pub call_id: String,
    #[serde(default)]
    pub completed_at_ms: i64,
    /// Thread ID of the sender.
    pub sender_thread_id: ThreadId,
    /// Thread ID of the receiver.
    pub receiver_thread_id: ThreadId,
    /// Optional nickname assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_nickname: Option<String>,
    /// Optional role assigned to the receiver agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_agent_role: Option<String>,
    /// Last known status of the receiver agent reported to the sender agent after
    /// resume.
    pub status: AgentStatus,
}

