use codex_protocol::auth::AuthMode as CoreAuthMode;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use strum_macros::Display;
use ts_rs::TS;

/// Authentication mode for OpenAI-backed providers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// OpenAI API key provided by the caller and stored by Codex.
    #[strum(serialize = "apikey")]
    ApiKey,
    /// ChatGPT OAuth managed by Codex (tokens persisted and refreshed by Codex).
    #[strum(serialize = "chatgpt")]
    Chatgpt,
    /// [UNSTABLE] FOR OPENAI INTERNAL USE ONLY - DO NOT USE.
    ///
    /// ChatGPT auth tokens are supplied by an external host app and are only
    /// stored in memory. Token refresh must be handled by the external host app.
    #[serde(rename = "chatgptAuthTokens")]
    #[ts(rename = "chatgptAuthTokens")]
    #[strum(serialize = "chatgptAuthTokens")]
    ChatgptAuthTokens,
    /// Backend auth supplied as request headers.
    #[serde(rename = "headers")]
    #[ts(rename = "headers")]
    #[strum(serialize = "headers")]
    Headers,
    /// Programmatic Codex auth backed by a registered Agent Identity.
    #[serde(rename = "agentIdentity")]
    #[ts(rename = "agentIdentity")]
    #[strum(serialize = "agentIdentity")]
    AgentIdentity,
    /// Programmatic Codex auth backed by a personal access token.
    #[serde(rename = "personalAccessToken")]
    #[ts(rename = "personalAccessToken")]
    #[strum(serialize = "personalAccessToken")]
    PersonalAccessToken,
    /// Amazon Bedrock bearer token managed by Codex.
    #[serde(rename = "bedrockApiKey")]
    #[ts(rename = "bedrockApiKey")]
    #[strum(serialize = "bedrockApiKey")]
    BedrockApiKey,
}

impl AuthMode {
    /// Returns whether this mode represents an authenticated human ChatGPT account.
    pub fn has_chatgpt_account(self) -> bool {
        CoreAuthMode::from(self).has_chatgpt_account()
    }

    /// Returns whether this mode is backed by Codex services rather than a direct model API.
    pub fn uses_codex_backend(self) -> bool {
        CoreAuthMode::from(self).uses_codex_backend()
    }
}

impl From<CoreAuthMode> for AuthMode {
    fn from(value: CoreAuthMode) -> Self {
        match value {
            CoreAuthMode::ApiKey => Self::ApiKey,
            CoreAuthMode::Chatgpt => Self::Chatgpt,
            CoreAuthMode::ChatgptAuthTokens => Self::ChatgptAuthTokens,
            CoreAuthMode::Headers => Self::Headers,
            CoreAuthMode::AgentIdentity => Self::AgentIdentity,
            CoreAuthMode::PersonalAccessToken => Self::PersonalAccessToken,
            CoreAuthMode::BedrockApiKey => Self::BedrockApiKey,
        }
    }
}

impl From<AuthMode> for CoreAuthMode {
    fn from(value: AuthMode) -> Self {
        match value {
            AuthMode::ApiKey => Self::ApiKey,
            AuthMode::Chatgpt => Self::Chatgpt,
            AuthMode::ChatgptAuthTokens => Self::ChatgptAuthTokens,
            AuthMode::Headers => Self::Headers,
            AuthMode::AgentIdentity => Self::AgentIdentity,
            AuthMode::PersonalAccessToken => Self::PersonalAccessToken,
            AuthMode::BedrockApiKey => Self::BedrockApiKey,
        }
    }
}

macro_rules! experimental_method_entry {
    (#[experimental($reason:expr)] => $wire:literal) => {
        $wire
    };
    (#[experimental($reason:expr)]) => {
        $reason
    };
    ($($tt:tt)*) => {
        ""
    };
}

macro_rules! experimental_type_entry {
    (#[experimental($reason:expr)] $ty:ty) => {
        stringify!($ty)
    };
    ($ty:ty) => {
        ""
    };
}

mod client_requests;
mod notifications;
mod server_requests;

pub use client_requests::*;
pub use notifications::*;
pub use server_requests::*;

fn decode_jsonrpc_params<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> serde_json::Result<T> {
    // JSON-RPC permits `params` to be omitted. Deserialize an omitted value as
    // JSON null so request definitions using `Option<_>` or `()` can accept
    // their parameterless wire form while required parameter structs still
    // reject it through their normal type validation.
    match params {
        // Clients such as Codex Desktop also send `{}` to parameterless methods,
        // which serde's derived tagged-enum decoding accepts as unit. Keep that
        // form working; parameter structs still decode `{}` or keep its error.
        Some(serde_json::Value::Object(map)) if map.is_empty() => {
            serde_json::from_value(serde_json::Value::Object(map))
                .or_else(|err| serde_json::from_value(serde_json::Value::Null).map_err(|_| err))
        }
        params => serde_json::from_value(params.unwrap_or(serde_json::Value::Null)),
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchParams {
    pub query: String,
    pub roots: Vec<String>,
    // If provided, cancels the previous request with this value on the same connection.
    pub cancellation_token: Option<String>,
}

/// Superset of [`codex_file_search::FileMatch`]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
pub struct FuzzyFileSearchResult {
    pub root: String,
    pub path: String,
    pub match_type: FuzzyFileSearchMatchType,
    pub file_name: String,
    pub score: u32,
    pub indices: Option<Vec<u32>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub enum FuzzyFileSearchMatchType {
    File,
    Directory,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchResponse {
    pub files: Vec<FuzzyFileSearchResult>,
    /// Matches found among scanned entries, before the result limit is applied.
    #[serde(default)]
    pub total_match_count: usize,
    /// Number of indexed files and directories, including entries that did not match.
    #[serde(default)]
    pub scanned_file_count: usize,
    /// False if scanning was skipped or paths were omitted due to limits, cancellation, or errors.
    #[serde(default)]
    pub walk_complete: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionStartParams {
    pub session_id: String,
    pub roots: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionStartResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionUpdateParams {
    pub session_id: String,
    pub query: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionUpdateResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionStopParams {
    pub session_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
pub struct FuzzyFileSearchSessionStopResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionUpdatedNotification {
    pub session_id: String,
    pub query: String,
    pub files: Vec<FuzzyFileSearchResult>,
    /// Matches found among scanned entries, before the result limit is applied.
    /// Zero when the query is empty and results are suppressed.
    #[serde(default)]
    pub total_match_count: usize,
    /// Number of indexed files and directories, including entries that did not match.
    #[serde(default)]
    pub scanned_file_count: usize,
    /// True only after scanning and current-query matching finish without omissions.
    /// False during matching, or when limits, cancellation or errors interrupt search.
    #[serde(default)]
    pub walk_complete: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FuzzyFileSearchSessionCompletedNotification {
    pub session_id: String,
    pub query: String,
    /// Terminal results are delivered even if best-effort progress updates were lost.
    /// Absent only in legacy server payloads; absence does not prove completeness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub results: Option<FuzzyFileSearchResponse>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}
