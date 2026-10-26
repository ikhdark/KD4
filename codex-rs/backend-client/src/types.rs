pub use codex_backend_openapi_models::models::ConfigBundleResponse;
pub use codex_backend_openapi_models::models::CreditStatusDetails;
pub use codex_backend_openapi_models::models::DeliveredConfigToml;
pub use codex_backend_openapi_models::models::DeliveredManagedLayers;
pub use codex_backend_openapi_models::models::DeliveredRequirementsToml;
pub use codex_backend_openapi_models::models::DeliveredTomlFragment;
pub use codex_backend_openapi_models::models::PaginatedListTaskListItem;
pub use codex_backend_openapi_models::models::PlanType;
pub use codex_backend_openapi_models::models::RateLimitReachedKind;
pub use codex_backend_openapi_models::models::RateLimitStatusDetails;
pub use codex_backend_openapi_models::models::RateLimitStatusPayload;
pub use codex_backend_openapi_models::models::RateLimitWindowSnapshot;
pub use codex_backend_openapi_models::models::SpendControlLimitDetails;
pub use codex_backend_openapi_models::models::TaskListItem;

use codex_protocol::protocol::RateLimitSnapshot;
use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RateLimitResetCreditsSummary {
    pub available_count: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RateLimitResetCreditsDetails {
    pub credits: Vec<RateLimitResetCreditDetails>,
    pub available_count: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RateLimitResetCreditDetails {
    pub id: String,
    pub reset_type: String,
    pub status: String,
    pub granted_at: String,
    pub expires_at: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RateLimitsWithResetCredits {
    pub rate_limits: Vec<RateLimitSnapshot>,
    /// Backend decision for ordinary included usage; absence is not permission to recover.
    pub ordinary_usage_allowed: Option<bool>,
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSummary>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    pub rate_limit_upsell: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct RateLimitStatusWithResetCredits {
    #[serde(flatten)]
    pub rate_limits: RateLimitStatusPayload,
    // Capture the new optional metadata alongside the generated quota fields until the
    // backend OpenAPI export includes it. This field owns additional_rate_limits on the wire.
    pub additional_rate_limits: Option<Vec<AdditionalRateLimitWithNormalModel>>,
    pub rate_limit_reset_credits: Option<RateLimitResetCreditsSummary>,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    // Preserve the backend-owned banner contract without making optional UI data break usage.
    pub rate_limit_upsell: Option<Value>,
}

/// Additional quota details plus the normal model whose picker metadata should be used.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct AdditionalRateLimitWithNormalModel {
    #[serde(flatten)]
    pub details: codex_backend_openapi_models::models::AdditionalRateLimitDetails,
    pub normal_model_slug: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CodexWorkspaceMessagesResponse {
    #[serde(default)]
    pub messages: Vec<CodexWorkspaceMessage>,
}

/// Authenticated Codex user settings used by CLI runtime policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct CodexUserSettingsResponse {
    /// Server-computed effective commit-attribution policy.
    ///
    /// Older backend responses omit this field, which safely defaults to disabled.
    #[serde(default)]
    pub commit_attribution_enabled: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CodexWorkspaceMessage {
    pub message_id: String,
    pub message_type: CodexWorkspaceMessageType,
    pub message_body: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConsumeRateLimitResetCreditCode {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ConsumeRateLimitResetCreditResponse {
    pub code: ConsumeRateLimitResetCreditCode,
    #[serde(default)]
    pub windows_reset: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CodexWorkspaceMessageType {
    Headline,
    Announcement,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug)]
pub struct AccountsCheckResponse {
    pub accounts: Vec<AccountEntry>,
    pub account_ordering: Vec<String>,
    pub default_account_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AccountEntry {
    pub id: String,
    /// Current subscription reported by the accounts endpoint, independent of token claims.
    #[serde(default)]
    pub plan_type: Option<codex_protocol::account::PlanType>,
    pub workspace_backend_origin: Option<String>,
    pub account_routing_override: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub profile_picture_url: Option<String>,
    #[serde(default)]
    pub structure: String,
}

#[derive(Deserialize)]
struct RawAccountsCheckResponse {
    #[serde(default)]
    accounts: RawAccounts,
    #[serde(default)]
    account_ordering: Vec<String>,
    #[serde(default)]
    default_account_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAccounts {
    List(Vec<AccountEntry>),
    Map(HashMap<String, ChatGptAccountEntry>),
}

impl Default for RawAccounts {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

#[derive(Deserialize)]
struct ChatGptAccountEntry {
    account: ChatGptAccountInfo,
}

#[derive(Deserialize)]
struct ChatGptAccountInfo {
    account_id: Option<String>,
    #[serde(default)]
    plan_type: Option<codex_protocol::account::PlanType>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile_picture_url: Option<String>,
    #[serde(default)]
    structure: String,
}

impl<'de> Deserialize<'de> for AccountsCheckResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawAccountsCheckResponse::deserialize(deserializer)?;
        let accounts = match raw.accounts {
            RawAccounts::List(accounts) => accounts,
            RawAccounts::Map(mut accounts) => raw
                .account_ordering
                .iter()
                .filter_map(|account_id| {
                    let account = accounts.remove(account_id)?.account;
                    Some(AccountEntry {
                        id: account.account_id?,
                        plan_type: account.plan_type,
                        workspace_backend_origin: None,
                        account_routing_override: None,
                        name: account.name,
                        profile_picture_url: account.profile_picture_url,
                        structure: account.structure,
                    })
                })
                .collect(),
        };
        Ok(Self {
            accounts,
            account_ordering: raw.account_ordering,
            default_account_id: raw.default_account_id,
        })
    }
}

/// Hand-rolled models for the Cloud Tasks task-details response.
/// The generated OpenAPI models are pretty bad. This is a half-step
/// towards hand-rolling them.
#[derive(Clone, Debug, Deserialize)]
pub struct CodeTaskDetailsResponse {
    #[serde(default)]
    pub task: Option<CodeTaskIdentity>,
    #[serde(default)]
    pub current_user_turn: Option<Turn>,
    #[serde(default)]
    pub current_assistant_turn: Option<Turn>,
    #[serde(default)]
    pub current_diff_task_turn: Option<Turn>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CodeTaskIdentity {
    pub id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudEnvironment {
    pub id: String,
    pub label: Option<String>,
    pub is_pinned: Option<bool>,
    pub task_count: Option<i64>,
}

#[derive(Debug)]
pub struct TaskIdentityMismatch {
    pub requested: String,
    pub observed: String,
}

impl std::fmt::Display for TaskIdentityMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task identity mismatch: requested {}, received {}", self.requested, self.observed)
    }
}

impl std::error::Error for TaskIdentityMismatch {}

impl CodeTaskDetailsResponse {
    /// Missing legacy identity is unavailable, not a contradictory identity.
    pub fn validate_task_id(&self, requested: &str) -> Result<(), TaskIdentityMismatch> {
        if let Some(observed) = self.task.as_ref().and_then(|task| task.id.as_deref())
            && observed != requested
        {
            return Err(TaskIdentityMismatch { requested: requested.into(), observed: observed.into() });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub attempt_placement: Option<i64>,
    #[serde(default, rename = "turn_status")]
    pub turn_status: Option<String>,
    #[serde(default)]
    pub sibling_turn_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_vec")]
    pub input_items: Vec<TurnItem>,
    #[serde(default, deserialize_with = "deserialize_vec")]
    pub output_items: Vec<TurnItem>,
    #[serde(default)]
    pub worklog: Option<Worklog>,
    #[serde(default)]
    pub error: Option<TurnError>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TurnItem {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, deserialize_with = "deserialize_vec")]
    pub content: Vec<ContentFragment>,
    #[serde(default)]
    pub diff: Option<String>,
    #[serde(default)]
    pub output_diff: Option<DiffPayload>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ContentFragment {
    Structured(StructuredContent),
    Text(String),
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StructuredContent {
    #[serde(rename = "content_type", default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DiffPayload {
    #[serde(default)]
    pub diff: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Worklog {
    #[serde(default, deserialize_with = "deserialize_vec")]
    pub messages: Vec<WorklogMessage>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorklogMessage {
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub content: Option<WorklogContent>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Author {
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorklogContent {
    #[serde(default)]
    pub parts: Vec<ContentFragment>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TurnError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl ContentFragment {
    fn text(&self) -> Option<&str> {
        match self {
            ContentFragment::Structured(inner) => {
                if inner
                    .content_type
                    .as_deref()
                    .map(|ct| ct.eq_ignore_ascii_case("text"))
                    .unwrap_or(false)
                {
                    inner.text.as_deref().filter(|s| !s.is_empty())
                } else {
                    None
                }
            }
            ContentFragment::Text(raw) => {
                if raw.trim().is_empty() {
                    None
                } else {
                    Some(raw.as_str())
                }
            }
        }
    }
}

impl TurnItem {
    fn text_values(&self) -> Vec<String> {
        self.content
            .iter()
            .filter_map(|fragment| fragment.text().map(str::to_string))
            .collect()
    }

    fn diff_text(&self) -> Option<String> {
        if self.kind == "output_diff" {
            if let Some(diff) = &self.diff
                && !diff.is_empty()
            {
                return Some(diff.clone());
            }
        } else if self.kind == "pr"
            && let Some(payload) = &self.output_diff
            && let Some(diff) = &payload.diff
            && !diff.is_empty()
        {
            return Some(diff.clone());
        }
        None
    }
}

impl Turn {
    pub fn unified_diff(&self) -> Option<String> {
        self.output_items.iter().find_map(TurnItem::diff_text)
    }

    pub fn message_texts(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .output_items
            .iter()
            .filter(|item| item.kind == "message")
            .filter(|item| item.role.as_deref().is_none_or(|role| role == "assistant"))
            .flat_map(TurnItem::text_values)
            .collect();

        if let Some(log) = &self.worklog {
            for message in &log.messages {
                if message.is_assistant() {
                    out.extend(message.text_values());
                }
            }
        }

        out
    }

    fn user_prompt(&self) -> Option<String> {
        let parts: Vec<String> = self
            .input_items
            .iter()
            .filter(|item| item.kind == "message")
            .filter(|item| {
                item.role
                    .as_deref()
                    .map(|r| r.eq_ignore_ascii_case("user"))
                    .unwrap_or(true)
            })
            .flat_map(TurnItem::text_values)
            .collect();

        if parts.is_empty() {
            None
        } else {
            Some(parts.join(
                "

",
            ))
        }
    }

    pub fn error_summary(&self) -> Option<String> {
        self.error.as_ref().and_then(TurnError::summary)
    }
}

impl WorklogMessage {
    fn is_assistant(&self) -> bool {
        self.author
            .as_ref()
            .and_then(|a| a.role.as_deref())
            .map(|role| role.eq_ignore_ascii_case("assistant"))
            .unwrap_or(false)
    }

    fn text_values(&self) -> Vec<String> {
        self.content
            .as_ref()
            .map(|content| {
                content
                    .parts
                    .iter()
                    .filter_map(|fragment| fragment.text().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl TurnError {
    fn summary(&self) -> Option<String> {
        let code = self.code.as_deref().unwrap_or("");
        let message = self.message.as_deref().unwrap_or("");
        match (code.is_empty(), message.is_empty()) {
            (true, true) => None,
            (false, true) => Some(code.to_string()),
            (true, false) => Some(message.to_string()),
            (false, false) => Some(format!("{code}: {message}")),
        }
    }
}

pub trait CodeTaskDetailsResponseExt {
    /// Attempt to extract a unified diff string from the assistant or diff turn.
    fn unified_diff(&self) -> Option<String>;
    /// Extract assistant text output messages (no diff) from current turns.
    fn assistant_text_messages(&self) -> Vec<String>;
    /// Extract the user's prompt text from the current user turn, when present.
    fn user_text_prompt(&self) -> Option<String>;
    /// Extract an assistant error message (if the turn failed and provided one).
    fn assistant_error_message(&self) -> Option<String>;
}

impl CodeTaskDetailsResponseExt for CodeTaskDetailsResponse {
    fn unified_diff(&self) -> Option<String> {
        [
            self.current_diff_task_turn.as_ref(),
            self.current_assistant_turn.as_ref(),
        ]
        .into_iter()
        .flatten()
        .find_map(Turn::unified_diff)
    }

    fn assistant_text_messages(&self) -> Vec<String> {
        if let (Some(diff), Some(assistant)) = (&self.current_diff_task_turn, &self.current_assistant_turn)
            && diff.id.as_deref().is_some_and(|id| !id.is_empty()) && diff.id == assistant.id
        {
            let messages = assistant.message_texts();
            return if messages.is_empty() { diff.message_texts() } else { messages };
        }
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for turn in [
            self.current_diff_task_turn.as_ref(),
            self.current_assistant_turn.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(id) = turn.id.as_deref().filter(|id| !id.is_empty())
                && !seen.insert(id)
            {
                continue;
            }
            out.extend(turn.message_texts());
        }
        out
    }

    fn user_text_prompt(&self) -> Option<String> {
        self.current_user_turn.as_ref().and_then(Turn::user_prompt)
    }

    fn assistant_error_message(&self) -> Option<String> {
        self.current_assistant_turn
            .as_ref()
            .and_then(Turn::error_summary)
    }
}

fn deserialize_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Clone, Debug, Deserialize)]
pub struct TurnAttemptsSiblingTurnsResponse {
    #[serde(default)]
    pub sibling_turns: Vec<HashMap<String, Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TokenUsageProfile {
    pub stats: TokenUsageProfileStats,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TokenUsageProfileStats {
    pub lifetime_tokens: Option<i64>,
    pub peak_daily_tokens: Option<i64>,
    pub longest_running_turn_sec: Option<i64>,
    pub current_streak_days: Option<i64>,
    pub longest_streak_days: Option<i64>,
    pub daily_usage_buckets: Option<Vec<TokenUsageProfileDailyBucket>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct TokenUsageProfileDailyBucket {
    pub start_date: String,
    pub tokens: i64,
}

