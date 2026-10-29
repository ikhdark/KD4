use std::path::PathBuf;

use crate::JSONRPCNotification;
use crate::JSONRPCRequest;
use crate::RequestId;
use crate::export::GeneratedSchema;
use crate::export::generate_json_schema;
use crate::protocol::v1;
use crate::protocol::v2;
use codex_experimental_api_macros::ExperimentalApi;
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
    ApiKey,
    /// ChatGPT OAuth managed by Codex (tokens persisted and refreshed by Codex).
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

macro_rules! experimental_reason_expr {
    // If a request variant is explicitly marked experimental, that reason wins.
    (variant $variant:ident, #[experimental($reason:expr)] $params:ident $(, $inspect_params:tt)?) => {
        Some($reason)
    };
    // `inspect_params: true` is used when a method is mostly stable but needs
    // field-level gating from its params type (for example, ThreadStart).
    (variant $variant:ident, $params:ident, true) => {
        crate::experimental_api::ExperimentalApi::experimental_reason($params)
    };
    (variant $variant:ident, $params:ident $(, $inspect_params:tt)?) => {
        None
    };
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

/// Resource admission is independent of semantic serialization: process cleanup
/// still follows process registration, but must not need an ordinary work slot.
pub fn client_request_is_control(method: &str) -> bool {
    matches!(
        method,
        "turn/interrupt" | "command/exec/terminate" | "process/kill"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequestSerializationScope {
    Global(&'static str),
    GlobalSharedRead(&'static str),
    Thread {
        thread_id: String,
    },
    /// Out-of-band control traffic for a thread, such as `turn/interrupt`.
    ///
    /// Shares the thread's queue key so control work is still serialized against that
    /// thread, but runs in a reserved lane ahead of queued mutations. Control requests
    /// cannot reorder two mutations relative to each other.
    ThreadControl {
        thread_id: String,
    },
    ThreadPath {
        path: PathBuf,
    },
    CommandExecProcess {
        process_id: String,
    },
    Process {
        process_handle: String,
    },
    FuzzyFileSearchSession {
        session_id: String,
    },
    FsWatch {
        watch_id: String,
    },
    McpOauth {
        server_name: String,
    },
}

macro_rules! serialization_scope_expr {
    ($actual_params:ident, None) => {
        None
    };
    ($actual_params:ident, global($key:literal)) => {
        Some(ClientRequestSerializationScope::Global($key))
    };
    ($actual_params:ident, global_shared_read($key:literal)) => {
        Some(ClientRequestSerializationScope::GlobalSharedRead($key))
    };
    ($actual_params:ident, thread_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::Thread {
            thread_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, thread_control_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::ThreadControl {
            thread_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, optional_thread_id($params:ident . $field:ident)) => {
        $actual_params
            .$field
            .clone()
            .map(|thread_id| ClientRequestSerializationScope::Thread { thread_id })
    };
    ($actual_params:ident, thread_or_path($params:ident . $thread_field:ident, $params2:ident . $path_field:ident)) => {
        if !$actual_params.$thread_field.is_empty() {
            Some(ClientRequestSerializationScope::Thread {
                thread_id: $actual_params.$thread_field.clone(),
            })
        } else if let Some(path) = $actual_params.$path_field.clone() {
            Some(ClientRequestSerializationScope::ThreadPath { path })
        } else {
            Some(ClientRequestSerializationScope::Thread {
                thread_id: $actual_params.$thread_field.clone(),
            })
        }
    };
    ($actual_params:ident, optional_command_process_id($params:ident . $field:ident)) => {
        $actual_params
            .$field
            .clone()
            .map(|process_id| ClientRequestSerializationScope::CommandExecProcess { process_id })
    };
    ($actual_params:ident, command_process_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::CommandExecProcess {
            process_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, process_handle($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::Process {
            process_handle: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, fuzzy_session_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::FuzzyFileSearchSession {
            session_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, fs_watch_id($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::FsWatch {
            watch_id: $actual_params.$field.clone(),
        })
    };
    ($actual_params:ident, mcp_oauth_server($params:ident . $field:ident)) => {
        Some(ClientRequestSerializationScope::McpOauth {
            server_name: $actual_params.$field.clone(),
        })
    };
}

/// Generates an `enum ClientRequest` where each variant is a request that the
/// client can send to the server. Each variant has associated `params` and
/// `response` types. Also generates a `export_client_responses()` function to
/// export all response types to TypeScript.
macro_rules! encode_client_request_params {
    ($params:ident;) => {
        serde_json::to_value($params).map(Some)
    };
    (
        $params:ident;
        #[serde(skip_serializing_if = "Option::is_none")]
        $($remaining:tt)*
    ) => {
        if $params.is_none() {
            Ok(None)
        } else {
            serde_json::to_value($params).map(Some)
        }
    };
    ($params:ident; #[$other:meta] $($remaining:tt)*) => {
        encode_client_request_params!($params; $($remaining)*)
    };
}

/// Decode failures after the wire envelope has already been validated.
#[derive(Debug, thiserror::Error)]
pub enum ClientRequestDecodeError {
    #[error("Unknown method: {0}")]
    UnknownMethod(String),
    #[error("Invalid parameters for {method}: {source}")]
    InvalidParams {
        method: &'static str,
        source: serde_json::Error,
    },
}

macro_rules! client_request_definitions {
    (
        $(
            $(#[experimental($reason:expr)])?
            $(#[doc = $variant_doc:literal])*
            $variant:ident => $wire:literal {
                $(aliases: [$($alias:literal),* $(,)?],)?
                params: $(#[$($params_meta:tt)*])* $params:ty,
                $(inspect_params: $inspect_params:tt,)?
                serialization: $serialization:ident $( ( $($serialization_args:tt)* ) )?,
                $(manual_payload_conversion: $manual_payload_conversion:ident,)?
                response: $response:ty,
            }
        ),* $(,)?
    ) => {
        /// Request from the client to the server.
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ClientRequest {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                $($(
                    #[serde(alias = $alias)]
                )*)?
                #[ts(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    $(#[$($params_meta)*])*
                    params: $params,
                },
            )*
        }

        impl ClientRequest {
            /// Keep method membership separate from parameter decoding without
            /// interpreting serde's diagnostic text. The legacy TryFrom API is unchanged.
            pub fn decode_jsonrpc(request: JSONRPCRequest) -> Result<Self, ClientRequestDecodeError> {
                match request.method.as_str() {
                    $(
                        $wire $( $(| $alias)* )? => Self::try_from(request).map_err(|source| {
                            ClientRequestDecodeError::InvalidParams { method: $wire, source }
                        }),
                    )*
                    _ => Err(ClientRequestDecodeError::UnknownMethod(request.method)),
                }
            }

            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn method_name(&self) -> &'static str {
                match self {
                    $(Self::$variant { .. } => $wire,)*
                }
            }

            pub fn method(&self) -> String {
                self.method_name().to_string()
            }

            pub fn is_control(&self) -> bool {
                client_request_is_control(self.method_name())
            }

            pub fn serialization_scope(&self) -> Option<ClientRequestSerializationScope> {
                match self {
                    $(
                        Self::$variant { params, .. } => {
                            let _ = params;
                            serialization_scope_expr!(
                                params, $serialization $( ( $($serialization_args)* ) )?
                            )
                        }
                    )*
                }
            }
        }

        impl TryFrom<JSONRPCRequest> for ClientRequest {
            type Error = serde_json::Error;

            fn try_from(value: JSONRPCRequest) -> Result<Self, Self::Error> {
                let JSONRPCRequest {
                    id: request_id,
                    method,
                    params,
                    trace: _,
                } = value;

                match method.as_str() {
                    $(
                        $wire $( $(| $alias)* )? => Ok(Self::$variant {
                            request_id,
                            params: decode_jsonrpc_params(params)?,
                        }),
                    )*
                    _ => Err(<serde_json::Error as serde::de::Error>::unknown_variant(
                        &method,
                        &[$($wire),*],
                    )),
                }
            }
        }

        impl TryFrom<ClientRequest> for JSONRPCRequest {
            type Error = serde_json::Error;

            fn try_from(value: ClientRequest) -> Result<Self, Self::Error> {
                match value {
                    $(
                        ClientRequest::$variant { request_id, params } => Ok(Self {
                            id: request_id,
                            method: $wire.to_string(),
                            params: encode_client_request_params!(
                                params;
                                $(#[$($params_meta)*])*
                            )?,
                            trace: None,
                        }),
                    )*
                }
            }
        }

        /// Typed response from the server to the client.
        #[derive(Serialize, Deserialize, Debug, Clone)]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ClientResponse {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    response: $response,
                },
            )*
        }

        impl ClientResponse {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn method_name(&self) -> &'static str {
                match self {
                    $(Self::$variant { .. } => $wire,)*
                }
            }

            pub fn method(&self) -> String {
                self.method_name().to_string()
            }

            pub fn into_jsonrpc_parts(
                self,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                match self {
                    $(
                        Self::$variant { request_id, response } => {
                            serde_json::to_value(response).map(|result| (request_id, result))
                        }
                    )*
                }
            }
        }

        #[derive(Debug, Clone)]
        #[allow(clippy::large_enum_variant)]
        pub enum ClientResponsePayload {
            $( $variant($response), )*
        }

        impl ClientResponsePayload {
            pub fn into_jsonrpc_parts_and_payload(
                self,
                request_id: RequestId,
            ) -> std::result::Result<
                (RequestId, crate::Result, Option<ClientResponsePayload>),
                serde_json::Error,
            > {
                match self {
                    $(
                        Self::$variant(response) => {
                            let result = serde_json::to_value(&response)?;
                            Ok((request_id, result, Some(Self::$variant(response))))
                        }
                    )*
                }
            }

            pub fn into_client_response(self, request_id: RequestId) -> Option<ClientResponse> {
                match self {
                    $(
                        Self::$variant(response) => {
                            Some(ClientResponse::$variant {
                                request_id,
                                response,
                            })
                        }
                    )*
                }
            }

            pub fn into_jsonrpc_parts(
                self,
                request_id: RequestId,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                self.to_jsonrpc_parts(request_id)
            }

            pub fn to_jsonrpc_parts(
                &self,
                request_id: RequestId,
            ) -> std::result::Result<(RequestId, crate::Result), serde_json::Error> {
                match self {
                    $(
                        Self::$variant(response) => {
                            serde_json::to_value(response).map(|result| (request_id, result))
                        }
                    )*
                }
            }
        }

        $(
            client_response_payload_from_impl!(
                $variant,
                $response
                $(, $manual_payload_conversion)?
            );
        )*

        impl crate::experimental_api::ExperimentalApi for ClientRequest {
            fn experimental_reason(&self) -> Option<&'static str> {
                match self {
                    $(
                        Self::$variant { params: _params, .. } => {
                            experimental_reason_expr!(
                                variant $variant,
                                $(#[experimental($reason)])?
                                _params
                                $(, $inspect_params)?
                            )
                        }
                    )*
                }
            }
        }

        pub(crate) const EXPERIMENTAL_CLIENT_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? => $wire),
            )*
        ];
        pub(crate) const EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        pub(crate) const EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

        pub fn export_client_responses(
            out_dir: &::std::path::Path,
        ) -> ::std::result::Result<(), ::ts_rs::ExportError> {
            $(
                <$response as ::ts_rs::TS>::export_all_to(out_dir)?;
            )*
            Ok(())
        }

        #[allow(clippy::vec_init_then_push)]
        pub(crate) fn export_client_response_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(generate_json_schema::<$response>(stringify!($response))?);
            )*
            Ok(schemas)
        }

        #[allow(clippy::vec_init_then_push)]
        pub(crate) fn export_client_param_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(generate_json_schema::<$params>(stringify!($params))?);
            )*
            Ok(schemas)
        }
    };
}

macro_rules! client_response_payload_from_impl {
    ($variant:ident, $response:ty) => {
        impl From<$response> for ClientResponsePayload {
            fn from(response: $response) -> Self {
                Self::$variant(response)
            }
        }
    };
    ($variant:ident, $response:ty, manual) => {};
}

client_request_definitions! {
    Initialize => "initialize" {
        params: v1::InitializeParams,
        serialization: None,
        response: v1::InitializeResponse,
    },

    /// NEW APIs
    // Thread lifecycle
    // Uses `inspect_params` because only some fields are experimental.
    ThreadStart => "thread/start" {
        params: v2::ThreadStartParams,
        inspect_params: true,
        serialization: None,
        response: v2::ThreadStartResponse,
    },
    ThreadResume => "thread/resume" {
        params: v2::ThreadResumeParams,
        inspect_params: true,
        serialization: thread_or_path(params.thread_id, params.path),
        response: v2::ThreadResumeResponse,
    },
    ThreadFork => "thread/fork" {
        params: v2::ThreadForkParams,
        inspect_params: true,
        serialization: thread_or_path(params.thread_id, params.path),
        response: v2::ThreadForkResponse,
    },
    ThreadArchive => "thread/archive" {
        params: v2::ThreadArchiveParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadArchiveResponse,
    },
    ThreadDelete => "thread/delete" {
        params: v2::ThreadDeleteParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadDeleteResponse,
    },
    ThreadUnsubscribe => "thread/unsubscribe" {
        params: v2::ThreadUnsubscribeParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadUnsubscribeResponse,
    },
    #[experimental("thread/incrementElicitation")]
    /// Acquire a connection-owned out-of-band elicitation lease for the thread.
    ///
    /// This is used by external helpers to pause timeout accounting while a user
    /// approval or other elicitation is pending outside the app-server request flow. The
    /// response token can only be released by this connection.
    ThreadIncrementElicitation => "thread/incrementElicitation" {
        aliases: ["thread/increment_elicitation"],
        params: v2::ThreadIncrementElicitationParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadIncrementElicitationResponse,
    },
    #[experimental("thread/decrementElicitation")]
    /// Idempotently release a connection-owned out-of-band elicitation lease.
    ///
    /// The caller must provide the server-issued lease token. When the count reaches zero,
    /// timeout accounting resumes for the thread.
    ThreadDecrementElicitation => "thread/decrementElicitation" {
        aliases: ["thread/decrement_elicitation"],
        params: v2::ThreadDecrementElicitationParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadDecrementElicitationResponse,
    },
    ThreadSetName => "thread/name/set" {
        params: v2::ThreadSetNameParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadSetNameResponse,
    },
    ThreadMetadataUpdate => "thread/metadata/update" {
        params: v2::ThreadMetadataUpdateParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadMetadataUpdateResponse,
    },
    #[experimental("thread/settings/update")]
    ThreadSettingsUpdate => "thread/settings/update" {
        params: v2::ThreadSettingsUpdateParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadSettingsUpdateResponse,
    },
    ThreadUnarchive => "thread/unarchive" {
        params: v2::ThreadUnarchiveParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadUnarchiveResponse,
    },
    ThreadCompactStart => "thread/compact/start" {
        params: v2::ThreadCompactStartParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadCompactStartResponse,
    },
    ThreadShellCommand => "thread/shellCommand" {
        params: v2::ThreadShellCommandParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadShellCommandResponse,
    },
    #[experimental("thread/backgroundTerminals/clean")]
    ThreadBackgroundTerminalsClean => "thread/backgroundTerminals/clean" {
        params: v2::ThreadBackgroundTerminalsCleanParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsCleanResponse,
    },
    #[experimental("thread/backgroundTerminals/list")]
    ThreadBackgroundTerminalsList => "thread/backgroundTerminals/list" {
        params: v2::ThreadBackgroundTerminalsListParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsListResponse,
    },
    #[experimental("thread/backgroundTerminals/terminate")]
    ThreadBackgroundTerminalsTerminate => "thread/backgroundTerminals/terminate" {
        params: v2::ThreadBackgroundTerminalsTerminateParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadBackgroundTerminalsTerminateResponse,
    },
    ThreadRollback => "thread/rollback" {
        params: v2::ThreadRollbackParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadRollbackResponse,
    },
    ThreadQueueList => "thread/queue/list" {
        params: v2::ThreadQueueListParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueListResponse,
    },
    ThreadQueueAdd => "thread/queue/add" {
        params: v2::ThreadQueueAddParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueAddResponse,
    },
    ThreadQueueUpdate => "thread/queue/update" {
        params: v2::ThreadQueueUpdateParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueUpdateResponse,
    },
    ThreadQueueDelete => "thread/queue/delete" {
        params: v2::ThreadQueueDeleteParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueDeleteResponse,
    },
    ThreadQueueReorder => "thread/queue/reorder" {
        params: v2::ThreadQueueReorderParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueReorderResponse,
    },
    ThreadQueueStart => "thread/queue/start" {
        params: v2::ThreadQueueStartParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadQueueStartResponse,
    },
    ThreadList => "thread/list" {
        params: v2::ThreadListParams,
        inspect_params: true,
        serialization: None,
        response: v2::ThreadListResponse,
    },
    #[experimental("project/list")]
    ProjectList => "project/list" {
        params: v2::ProjectListParams,
        serialization: global_shared_read("projects"),
        response: v2::ProjectListResponse,
    },
    #[experimental("project/read")]
    ProjectRead => "project/read" {
        params: v2::ProjectReadParams,
        serialization: global_shared_read("projects"),
        response: v2::ProjectReadResponse,
    },
    #[experimental("project/create")]
    ProjectCreate => "project/create" {
        params: v2::ProjectCreateParams,
        serialization: global("projects"),
        response: v2::ProjectCreateResponse,
    },
    #[experimental("project/import")]
    ProjectImport => "project/import" {
        params: v2::ProjectImportParams,
        serialization: global("projects"),
        response: v2::ProjectImportResponse,
    },
    #[experimental("project/update")]
    ProjectUpdate => "project/update" {
        params: v2::ProjectUpdateParams,
        serialization: global("projects"),
        response: v2::ProjectUpdateResponse,
    },
    #[experimental("project/move")]
    ProjectMove => "project/move" {
        params: v2::ProjectMoveParams,
        serialization: global("projects"),
        response: v2::ProjectMoveResponse,
    },
    #[experimental("project/delete")]
    ProjectDelete => "project/delete" {
        params: v2::ProjectDeleteParams,
        serialization: global("projects"),
        response: v2::ProjectDeleteResponse,
    },
    #[experimental("thread/search")]
    ThreadSearch => "thread/search" {
        params: v2::ThreadSearchParams,
        serialization: None,
        response: v2::ThreadSearchResponse,
    },
    ThreadLoadedList => "thread/loaded/list" {
        params: v2::ThreadLoadedListParams,
        serialization: None,
        response: v2::ThreadLoadedListResponse,
    },
    ThreadRead => "thread/read" {
        params: v2::ThreadReadParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadReadResponse,
    },
    #[experimental("thread/turns/list")]
    ThreadTurnsList => "thread/turns/list" {
        params: v2::ThreadTurnsListParams,
        // Explicitly concurrent: this primarily reads append-only rollout storage.
        serialization: None,
        response: v2::ThreadTurnsListResponse,
    },
    #[experimental("thread/items/list")]
    ThreadItemsList => "thread/items/list" {
        params: v2::ThreadItemsListParams,
        // Explicitly concurrent: this primarily reads append-only rollout storage.
        serialization: None,
        response: v2::ThreadItemsListResponse,
    },
    /// Append raw Responses API items to the thread history without starting a user turn.
    ThreadInjectItems => "thread/injectItems" {
        aliases: ["thread/inject_items"],
        params: v2::ThreadInjectItemsParams,
        serialization: thread_id(params.thread_id),
        response: v2::ThreadInjectItemsResponse,
    },
    SkillsList => "skills/list" {
        params: v2::SkillsListParams,
        serialization: global_shared_read("config"),
        response: v2::SkillsListResponse,
    },
    SkillsExtraRootsSet => "skills/extraRoots/set" {
        params: v2::SkillsExtraRootsSetParams,
        serialization: global("config"),
        response: v2::SkillsExtraRootsSetResponse,
    },
    HooksList => "hooks/list" {
        params: v2::HooksListParams,
        serialization: global("config"),
        response: v2::HooksListResponse,
    },
    MarketplaceAdd => "marketplace/add" {
        params: v2::MarketplaceAddParams,
        serialization: global("config"),
        response: v2::MarketplaceAddResponse,
    },
    MarketplaceRemove => "marketplace/remove" {
        params: v2::MarketplaceRemoveParams,
        serialization: global("config"),
        response: v2::MarketplaceRemoveResponse,
    },
    MarketplaceUpgrade => "marketplace/upgrade" {
        params: v2::MarketplaceUpgradeParams,
        serialization: global("config"),
        response: v2::MarketplaceUpgradeResponse,
    },
    PluginList => "plugin/list" {
        params: v2::PluginListParams,
        serialization: None,
        response: v2::PluginListResponse,
    },
    PluginInstalled => "plugin/installed" {
        params: v2::PluginInstalledParams,
        serialization: None,
        response: v2::PluginInstalledResponse,
    },
    PluginRead => "plugin/read" {
        params: v2::PluginReadParams,
        serialization: None,
        response: v2::PluginReadResponse,
    },
    PluginSkillRead => "plugin/skill/read" {
        params: v2::PluginSkillReadParams,
        serialization: global("config"),
        response: v2::PluginSkillReadResponse,
    },
    PluginShareSave => "plugin/share/save" {
        params: v2::PluginShareSaveParams,
        serialization: global("config"),
        response: v2::PluginShareSaveResponse,
    },
    PluginShareUpdateTargets => "plugin/share/updateTargets" {
        params: v2::PluginShareUpdateTargetsParams,
        serialization: global("config"),
        response: v2::PluginShareUpdateTargetsResponse,
    },
    PluginShareList => "plugin/share/list" {
        params: v2::PluginShareListParams,
        serialization: global("config"),
        response: v2::PluginShareListResponse,
    },
    PluginShareCheckout => "plugin/share/checkout" {
        params: v2::PluginShareCheckoutParams,
        serialization: global("config"),
        response: v2::PluginShareCheckoutResponse,
    },
    PluginShareDelete => "plugin/share/delete" {
        params: v2::PluginShareDeleteParams,
        serialization: global("config"),
        response: v2::PluginShareDeleteResponse,
    },
    AppsList => "app/list" {
        params: v2::AppsListParams,
        serialization: None,
        response: v2::AppsListResponse,
    },
    AppsRead => "app/read" {
        params: v2::AppsReadParams,
        serialization: None,
        response: v2::AppsReadResponse,
    },
    AppsInstalled => "app/installed" {
        params: v2::AppsInstalledParams,
        serialization: None,
        response: v2::AppsInstalledResponse,
    },
    // File system requests are intentionally concurrent. Desktop already treats local
    // file system operations as concurrent, and app-server remote fs mirrors that model.
    FsReadFile => "fs/readFile" {
        params: v2::FsReadFileParams,
        serialization: None,
        response: v2::FsReadFileResponse,
    },
    FsWriteFile => "fs/writeFile" {
        params: v2::FsWriteFileParams,
        serialization: None,
        response: v2::FsWriteFileResponse,
    },
    FsCreateDirectory => "fs/createDirectory" {
        params: v2::FsCreateDirectoryParams,
        serialization: None,
        response: v2::FsCreateDirectoryResponse,
    },
    FsGetMetadata => "fs/getMetadata" {
        params: v2::FsGetMetadataParams,
        serialization: None,
        response: v2::FsGetMetadataResponse,
    },
    FsReadDirectory => "fs/readDirectory" {
        params: v2::FsReadDirectoryParams,
        serialization: None,
        response: v2::FsReadDirectoryResponse,
    },
    FsRemove => "fs/remove" {
        params: v2::FsRemoveParams,
        serialization: None,
        response: v2::FsRemoveResponse,
    },
    FsCopy => "fs/copy" {
        params: v2::FsCopyParams,
        serialization: None,
        response: v2::FsCopyResponse,
    },
    FsWatch => "fs/watch" {
        params: v2::FsWatchParams,
        serialization: fs_watch_id(params.watch_id),
        response: v2::FsWatchResponse,
    },
    FsUnwatch => "fs/unwatch" {
        params: v2::FsUnwatchParams,
        serialization: fs_watch_id(params.watch_id),
        response: v2::FsUnwatchResponse,
    },
    SkillsConfigWrite => "skills/config/write" {
        params: v2::SkillsConfigWriteParams,
        serialization: global("config"),
        response: v2::SkillsConfigWriteResponse,
    },
    PluginInstall => "plugin/install" {
        params: v2::PluginInstallParams,
        serialization: global("config"),
        response: v2::PluginInstallResponse,
    },
    PluginUninstall => "plugin/uninstall" {
        params: v2::PluginUninstallParams,
        serialization: global("config"),
        response: v2::PluginUninstallResponse,
    },
    TurnStart => "turn/start" {
        params: v2::TurnStartParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::TurnStartResponse,
    },
    TurnSteer => "turn/steer" {
        params: v2::TurnSteerParams,
        inspect_params: true,
        serialization: thread_id(params.thread_id),
        response: v2::TurnSteerResponse,
    },
    TurnInterrupt => "turn/interrupt" {
        params: v2::TurnInterruptParams,
        // Interrupts observe and cancel in-flight turn state rather than extending the
        // ordered mutation sequence, so they run in the thread's control lane.
        serialization: thread_control_id(params.thread_id),
        response: v2::TurnInterruptResponse,
    },
    BugCreate => "bug/create" {
        params: v2::BugCreateParams,
        serialization: None,
        response: v2::BugCreateResponse,
    },

    ModelList => "model/list" {
        params: v2::ModelListParams,
        serialization: None,
        response: v2::ModelListResponse,
    },
    GatewayOAuthRead => "account/gatewayOAuth/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GatewayOAuthReadResponse,
    },
    ModelProviderCapabilitiesRead => "modelProvider/capabilities/read" {
        params: v2::ModelProviderCapabilitiesReadParams,
        serialization: None,
        response: v2::ModelProviderCapabilitiesReadResponse,
    },
    ExperimentalFeatureList => "experimentalFeature/list" {
        params: v2::ExperimentalFeatureListParams,
        serialization: global("config"),
        response: v2::ExperimentalFeatureListResponse,
    },
    PermissionProfileList => "permissionProfile/list" {
        params: v2::PermissionProfileListParams,
        serialization: global_shared_read("config"),
        response: v2::PermissionProfileListResponse,
    },
    ExperimentalFeatureEnablementSet => "experimentalFeature/enablement/set" {
        params: v2::ExperimentalFeatureEnablementSetParams,
        serialization: global("config"),
        response: v2::ExperimentalFeatureEnablementSetResponse,
    },
    #[experimental("remoteControl/enable")]
    RemoteControlEnable => "remoteControl/enable" {
        params: #[serde(skip_serializing_if = "Option::is_none")] v2::NullableRemoteControlEnableParams,
        serialization: global("remote-control"),
        response: v2::RemoteControlEnableResponse,
    },
    #[experimental("remoteControl/disable")]
    RemoteControlDisable => "remoteControl/disable" {
        params: #[serde(skip_serializing_if = "Option::is_none")] v2::NullableRemoteControlDisableParams,
        serialization: global("remote-control"),
        response: v2::RemoteControlDisableResponse,
    },
    #[experimental("remoteControl/status/read")]
    RemoteControlStatusRead => "remoteControl/status/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global_shared_read("remote-control"),
        response: v2::RemoteControlStatusReadResponse,
    },
    #[experimental("remoteControl/pairing/start")]
    RemoteControlPairingStart => "remoteControl/pairing/start" {
        params: v2::RemoteControlPairingStartParams,
        serialization: global("remote-control-pairing"),
        response: v2::RemoteControlPairingStartResponse,
    },
    #[experimental("remoteControl/pairing/status")]
    RemoteControlPairingStatus => "remoteControl/pairing/status" {
        params: v2::RemoteControlPairingStatusParams,
        serialization: global_shared_read("remote-control-pairing"),
        response: v2::RemoteControlPairingStatusResponse,
    },
    #[experimental("remoteControl/client/list")]
    RemoteControlClientsList => "remoteControl/client/list" {
        params: v2::RemoteControlClientsListParams,
        serialization: global_shared_read("remote-control-clients"),
        response: v2::RemoteControlClientsListResponse,
    },
    #[experimental("remoteControl/client/revoke")]
    RemoteControlClientsRevoke => "remoteControl/client/revoke" {
        params: v2::RemoteControlClientsRevokeParams,
        serialization: global("remote-control-clients"),
        response: v2::RemoteControlClientsRevokeResponse,
    },
    #[experimental("collaborationMode/list")]
    /// Lists collaboration mode presets.
    CollaborationModeList => "collaborationMode/list" {
        params: v2::CollaborationModeListParams,
        serialization: None,
        response: v2::CollaborationModeListResponse,
    },
    #[experimental("mock/experimentalMethod")]
    /// Test-only method used to validate experimental gating.
    MockExperimentalMethod => "mock/experimentalMethod" {
        params: v2::MockExperimentalMethodParams,
        serialization: None,
        response: v2::MockExperimentalMethodResponse,
    },
    #[experimental("environment/info")]
    /// Reads information from a configured execution environment.
    EnvironmentInfo => "environment/info" {
        params: v2::EnvironmentInfoParams,
        serialization: global_shared_read("environment"),
        response: v2::EnvironmentInfoResponse,
    },

    McpServerOauthLogin => "mcpServer/oauth/login" {
        params: v2::McpServerOauthLoginParams,
        serialization: mcp_oauth_server(params.name),
        response: v2::McpServerOauthLoginResponse,
    },

    McpServerRefresh => "config/mcpServer/reload" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("mcp-registry"),
        response: v2::McpServerRefreshResponse,
    },

    McpServerStatusList => "mcpServerStatus/list" {
        params: v2::ListMcpServerStatusParams,
        serialization: global("mcp-registry"),
        response: v2::ListMcpServerStatusResponse,
    },

    McpResourceRead => "mcpServer/resource/read" {
        params: v2::McpResourceReadParams,
        serialization: optional_thread_id(params.thread_id),
        response: v2::McpResourceReadResponse,
    },

    McpServerToolCall => "mcpServer/tool/call" {
        params: v2::McpServerToolCallParams,
        serialization: thread_id(params.thread_id),
        response: v2::McpServerToolCallResponse,
    },

    WindowsSandboxSetupStart => "windowsSandbox/setupStart" {
        params: v2::WindowsSandboxSetupStartParams,
        serialization: global("windows-sandbox-setup"),
        response: v2::WindowsSandboxSetupStartResponse,
    },
    WindowsSandboxReadiness => "windowsSandbox/readiness" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("config"),
        response: v2::WindowsSandboxReadinessResponse,
    },
    WindowsSandboxGrantReadRoot => "windowsSandbox/grantReadRoot" {
        params: v2::WindowsSandboxGrantReadRootParams,
        serialization: global("windows-sandbox-setup"),
        response: v2::WindowsSandboxGrantReadRootResponse,
    },

    LoginAccount => "account/login/start" {
        params: v2::LoginAccountParams,
        inspect_params: true,
        serialization: global("account-auth"),
        response: v2::LoginAccountResponse,
    },

    CancelLoginAccount => "account/login/cancel" {
        params: v2::CancelLoginAccountParams,
        serialization: global("account-auth"),
        response: v2::CancelLoginAccountResponse,
    },

    LogoutAccount => "account/logout" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("account-auth"),
        response: v2::LogoutAccountResponse,
    },

    GetAccountRateLimits => "account/rateLimits/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountRateLimitsResponse,
    },

    ConsumeAccountRateLimitResetCredit => "account/rateLimitResetCredit/consume" {
        params: v2::ConsumeAccountRateLimitResetCreditParams,
        serialization: global("account-auth"),
        response: v2::ConsumeAccountRateLimitResetCreditResponse,
    },

    GetAccountTokenUsage => "account/usage/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountTokenUsageResponse,
    },

    GetWorkspaceMessages => "account/workspaceMessages/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetWorkspaceMessagesResponse,
    },

    SendAddCreditsNudgeEmail => "account/sendAddCreditsNudgeEmail" {
        params: v2::SendAddCreditsNudgeEmailParams,
        serialization: global("account-auth"),
        response: v2::SendAddCreditsNudgeEmailResponse,
    },

    FeedbackUpload => "feedback/upload" {
        params: v2::FeedbackUploadParams,
        serialization: None,
        response: v2::FeedbackUploadResponse,
    },

    /// Execute a standalone command (argv vector) under the server's sandbox.
    OneOffCommandExec => "command/exec" {
        params: v2::CommandExecParams,
        inspect_params: true,
        serialization: optional_command_process_id(params.process_id),
        response: v2::CommandExecResponse,
    },
    /// Write stdin bytes to a running `command/exec` session or close stdin.
    CommandExecWrite => "command/exec/write" {
        params: v2::CommandExecWriteParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecWriteResponse,
    },
    /// Terminate a running `command/exec` session by client-supplied `processId`.
    CommandExecTerminate => "command/exec/terminate" {
        params: v2::CommandExecTerminateParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecTerminateResponse,
    },
    /// Resize a running PTY-backed `command/exec` session by client-supplied `processId`.
    CommandExecResize => "command/exec/resize" {
        params: v2::CommandExecResizeParams,
        serialization: command_process_id(params.process_id),
        response: v2::CommandExecResizeResponse,
    },
    #[experimental("process/spawn")]
    /// Spawn a standalone process (argv vector) without a Codex sandbox.
    ProcessSpawn => "process/spawn" {
        params: v2::ProcessSpawnParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessSpawnResponse,
    },
    #[experimental("process/writeStdin")]
    /// Write stdin bytes to a running `process/spawn` session or close stdin.
    ProcessWriteStdin => "process/writeStdin" {
        params: v2::ProcessWriteStdinParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessWriteStdinResponse,
    },
    #[experimental("process/kill")]
    /// Terminate a running `process/spawn` session by client-supplied `processHandle`.
    ProcessKill => "process/kill" {
        params: v2::ProcessKillParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessKillResponse,
    },
    #[experimental("process/resizePty")]
    /// Resize a running PTY-backed `process/spawn` session by client-supplied `processHandle`.
    ProcessResizePty => "process/resizePty" {
        params: v2::ProcessResizePtyParams,
        serialization: process_handle(params.process_handle),
        response: v2::ProcessResizePtyResponse,
    },

    ConfigRead => "config/read" {
        params: v2::ConfigReadParams,
        serialization: global_shared_read("config"),
        response: v2::ConfigReadResponse,
    },
    ExternalAgentConfigDetect => "externalAgentConfig/detect" {
        params: v2::ExternalAgentConfigDetectParams,
        serialization: global("config"),
        response: v2::ExternalAgentConfigDetectResponse,
    },
    ExternalAgentConfigImport => "externalAgentConfig/import" {
        params: v2::ExternalAgentConfigImportParams,
        serialization: global("config"),
        response: v2::ExternalAgentConfigImportResponse,
    },
    ExternalAgentConfigImportHistoriesRead => "externalAgentConfig/import/readHistories" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global_shared_read("config"),
        response: v2::ExternalAgentConfigImportHistoriesReadResponse,
    },
    ConfigValueWrite => "config/value/write" {
        params: v2::ConfigValueWriteParams,
        serialization: global("config"),
        manual_payload_conversion: manual,
        response: v2::ConfigWriteResponse,
    },
    ConfigBatchWrite => "config/batchWrite" {
        params: v2::ConfigBatchWriteParams,
        serialization: global("config"),
        manual_payload_conversion: manual,
        response: v2::ConfigWriteResponse,
    },

    ConfigRequirementsRead => "configRequirements/read" {
        params: #[ts(type = "undefined")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("config"),
        response: v2::ConfigRequirementsReadResponse,
    },

    GetAccount => "account/read" {
        params: v2::GetAccountParams,
        serialization: global("account-auth"),
        response: v2::GetAccountResponse,
    },

    /// DEPRECATED APIs below
    GetConversationSummary => "getConversationSummary" {
        params: v1::GetConversationSummaryParams,
        serialization: None,
        response: v1::GetConversationSummaryResponse,
    },
    GitDiffToRemote => "gitDiffToRemote" {
        params: v1::GitDiffToRemoteParams,
        serialization: None,
        response: v1::GitDiffToRemoteResponse,
    },
    /// DEPRECATED in favor of GetAccount
    GetAuthStatus => "getAuthStatus" {
        params: v1::GetAuthStatusParams,
        serialization: global("account-auth"),
        response: v1::GetAuthStatusResponse,
    },
    // Legacy fuzzy search cancellation is intentionally concurrent: clients reuse a
    // cancellation token so a newer request can cancel an older in-flight search.
    FuzzyFileSearch => "fuzzyFileSearch" {
        params: FuzzyFileSearchParams,
        serialization: None,
        response: FuzzyFileSearchResponse,
    },
    #[experimental("fuzzyFileSearch/sessionStart")]
    FuzzyFileSearchSessionStart => "fuzzyFileSearch/sessionStart" {
        params: FuzzyFileSearchSessionStartParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionStartResponse,
    },
    #[experimental("fuzzyFileSearch/sessionUpdate")]
    FuzzyFileSearchSessionUpdate => "fuzzyFileSearch/sessionUpdate" {
        params: FuzzyFileSearchSessionUpdateParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionUpdateResponse,
    },
    #[experimental("fuzzyFileSearch/sessionStop")]
    FuzzyFileSearchSessionStop => "fuzzyFileSearch/sessionStop" {
        params: FuzzyFileSearchSessionStopParams,
        serialization: fuzzy_session_id(params.session_id),
        response: FuzzyFileSearchSessionStopResponse,
    },
}

/// Generates an `enum ServerRequest` where each variant is a request that the
/// server can send to the client along with the corresponding params and
/// response types. It also generates helper types used by the app/server
/// infrastructure (payload enum, request constructor, and export helpers).
macro_rules! server_request_definitions {
    (
        $(
            $(#[experimental($reason:expr)])?
            $(#[doc = $variant_doc:literal])*
            $variant:ident => $wire:literal {
                params: $params:ty,
                response: $response:ty,
            }
        ),* $(,)?
    ) => {
        /// Request initiated from the server and sent to the client.
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ServerRequest {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                #[ts(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    params: $params,
                },
            )*
        }

        impl ServerRequest {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn response_from_result(
                &self,
                result: crate::Result,
            ) -> serde_json::Result<ServerResponse> {
                match self {
                    $(
                        Self::$variant { request_id, .. } => {
                            let response = serde_json::from_value::<$response>(result)?;
                            Ok(ServerResponse::$variant {
                                request_id: request_id.clone(),
                                response,
                            })
                        }
                    )*
                }
            }
        }

        impl TryFrom<JSONRPCRequest> for ServerRequest {
            type Error = serde_json::Error;

            fn try_from(value: JSONRPCRequest) -> Result<Self, Self::Error> {
                let JSONRPCRequest {
                    id: request_id,
                    method,
                    params,
                    trace: _,
                } = value;

                match method.as_str() {
                    $(
                        $wire => Ok(Self::$variant {
                            request_id,
                            params: decode_jsonrpc_params(params)?,
                        }),
                    )*
                    _ => Err(<serde_json::Error as serde::de::Error>::unknown_variant(
                        &method,
                        &[$($wire),*],
                    )),
                }
            }
        }

        /// Typed response from the client to the server.
        #[derive(Serialize, Deserialize, Debug, Clone)]
        #[serde(tag = "method", rename_all = "camelCase")]
        pub enum ServerResponse {
            $(
                $(#[doc = $variant_doc])*
                #[serde(rename = $wire)]
                $variant {
                    #[serde(rename = "id")]
                    request_id: RequestId,
                    response: $response,
                },
            )*
        }

        impl ServerResponse {
            pub fn id(&self) -> &RequestId {
                match self {
                    $(Self::$variant { request_id, .. } => request_id,)*
                }
            }

            pub fn method_name(&self) -> &'static str {
                match self {
                    $(Self::$variant { .. } => $wire,)*
                }
            }

            pub fn method(&self) -> String {
                self.method_name().to_string()
            }
        }

        #[derive(Debug, Clone, PartialEq, JsonSchema)]
        #[allow(clippy::large_enum_variant)]
        pub enum ServerRequestPayload {
            $( $variant($params), )*
        }

        impl ServerRequestPayload {
            pub fn request_with_id(self, request_id: RequestId) -> ServerRequest {
                match self {
                    $(Self::$variant(params) => ServerRequest::$variant { request_id, params },)*
                }
            }
        }

        pub(crate) const EXPERIMENTAL_SERVER_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? => $wire),
            )*
        ];
        pub(crate) const EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        pub(crate) const EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

        pub fn export_server_responses(
            out_dir: &::std::path::Path,
        ) -> ::std::result::Result<(), ::ts_rs::ExportError> {
            $(
                <$response as ::ts_rs::TS>::export_all_to(out_dir)?;
            )*
            Ok(())
        }

        #[allow(clippy::vec_init_then_push)]
        pub(crate) fn export_server_response_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(crate::export::generate_json_schema::<$response>(
                    concat!(stringify!($variant), "Response"),
                )?);
            )*
            Ok(schemas)
        }

        #[allow(clippy::vec_init_then_push)]
        pub(crate) fn export_server_param_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(crate::export::generate_json_schema::<$params>(
                    concat!(stringify!($variant), "Params"),
                )?);
            )*
            Ok(schemas)
        }
    };
}

/// Generates `ServerNotification` enum and helpers, including a JSON Schema
/// exporter for each notification.
macro_rules! notification_delivery_required {
    () => {
        false
    };
    (required) => {
        true
    };
}

macro_rules! notification_json_export_excluded {
    () => {
        false
    };
    (excluded) => {
        true
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerNotificationMetadata {
    pub(crate) method: &'static str,
    pub(crate) json_export_excluded: bool,
}

macro_rules! server_notification_definitions {
    (
        $(
            $(#[delivery($delivery:ident)])?
            $(#[json_export($json_export:ident)])?
            $(#[doc = $variant_doc:literal])*
            $(#[experimental($experimental_reason:expr)])?
            $variant:ident => $wire:literal ( $payload:ty )
        ),* $(,)?
    ) => {
        /// Notification sent from the server to the client.
        #[derive(
            Serialize,
            Deserialize,
            Debug,
            Clone,
            JsonSchema,
            TS,
            Display,
            ExperimentalApi,
        )]
        #[allow(clippy::large_enum_variant)]
        #[serde(tag = "method", content = "params", rename_all = "camelCase")]
        #[strum(serialize_all = "camelCase")]
        pub enum ServerNotification {
            $(
                $(#[doc = $variant_doc])*
                $(#[experimental($experimental_reason)])?
                #[serde(rename = $wire)]
                #[ts(rename = $wire)]
                #[strum(serialize = $wire)]
                $variant($payload),
            )*
        }

        pub(crate) const SERVER_NOTIFICATION_METADATA: &[ServerNotificationMetadata] = &[
            $(
                ServerNotificationMetadata {
                    method: $wire,
                    json_export_excluded: notification_json_export_excluded!($($json_export)?),
                },
            )*
        ];

        /// Returns `true` for notifications that must survive transport backpressure.
        ///
        /// These notifications either carry the authoritative assistant transcript or
        /// close a lifecycle/configuration operation that clients may be awaiting.
        pub fn server_notification_requires_delivery(notification: &ServerNotification) -> bool {
            match notification {
                $(
                    ServerNotification::$variant(_) =>
                        notification_delivery_required!($($delivery)?),
                )*
            }
        }

        impl TryFrom<JSONRPCNotification> for ServerNotification {
            type Error = serde_json::Error;

            fn try_from(value: JSONRPCNotification) -> Result<Self, serde_json::Error> {
                let JSONRPCNotification { method, params } = value;

                match method.as_str() {
                    $(
                        $wire => decode_jsonrpc_params(params).map(Self::$variant),
                    )*
                    _ => Err(<serde_json::Error as serde::de::Error>::unknown_variant(
                        &method,
                        &[$($wire),*],
                    )),
                }
            }
        }

        impl ServerNotification {
            /// Recognize registered methods without accepting malformed payloads.
            pub fn is_known_method(method: &str) -> bool {
                matches!(method, $($wire)|*)
            }
        }

        #[allow(clippy::vec_init_then_push)]
        pub(crate) fn export_server_notification_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let mut schemas = Vec::new();
            $(schemas.push(crate::export::generate_json_schema::<$payload>(stringify!($payload))?);)*
            Ok(schemas)
        }
    };
}

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

/// Notifications sent from the client to the server.
macro_rules! client_notification_definitions {
    (
        $(
            $(#[$variant_meta:meta])*
            $variant:ident $( ( $payload:ty ) )?
        ),* $(,)?
    ) => {
        #[derive(Serialize, Deserialize, Debug, Clone, JsonSchema, TS, Display)]
        #[serde(tag = "method", content = "params", rename_all = "camelCase")]
        #[strum(serialize_all = "camelCase")]
        pub enum ClientNotification {
            $(
                $(#[$variant_meta])*
                $variant $( ( $payload ) )?,
            )*
        }

        impl TryFrom<ClientNotification> for JSONRPCNotification {
            type Error = serde_json::Error;

            fn try_from(value: ClientNotification) -> Result<Self, Self::Error> {
                let method = value.to_string();
                let params = match serde_json::to_value(value)? {
                    serde_json::Value::Object(mut value) => value.remove("params"),
                    _ => {
                        return Err(<serde_json::Error as serde::ser::Error>::custom(
                            "client notification must serialize as an object",
                        ));
                    }
                };
                Ok(Self { method, params })
            }
        }

        pub(crate) fn export_client_notification_schemas(
        ) -> ::anyhow::Result<Vec<GeneratedSchema>> {
            let schemas = Vec::new();
            $( $(schemas.push(crate::export::generate_json_schema::<$payload>(stringify!($payload))?);)? )*
            Ok(schemas)
        }
    };
}

server_request_definitions! {
    /// NEW APIs
    /// Sent when approval is requested for a specific command execution.
    /// This request is used for Turns started via turn/start.
    CommandExecutionRequestApproval => "item/commandExecution/requestApproval" {
        params: v2::CommandExecutionRequestApprovalParams,
        response: v2::CommandExecutionRequestApprovalResponse,
    },

    /// Sent when approval is requested for a specific file change.
    /// This request is used for Turns started via turn/start.
    FileChangeRequestApproval => "item/fileChange/requestApproval" {
        params: v2::FileChangeRequestApprovalParams,
        response: v2::FileChangeRequestApprovalResponse,
    },

    /// EXPERIMENTAL - Request input from the user for a tool call.
    ToolRequestUserInput => "item/tool/requestUserInput" {
        params: v2::ToolRequestUserInputParams,
        response: v2::ToolRequestUserInputResponse,
    },

    /// Request input for an MCP server elicitation.
    McpServerElicitationRequest => "mcpServer/elicitation/request" {
        params: v2::McpServerElicitationRequestParams,
        response: v2::McpServerElicitationRequestResponse,
    },

    /// Request approval for additional permissions from the user.
    PermissionsRequestApproval => "item/permissions/requestApproval" {
        params: v2::PermissionsRequestApprovalParams,
        response: v2::PermissionsRequestApprovalResponse,
    },

    /// Execute a dynamic tool call on the client.
    DynamicToolCall => "item/tool/call" {
        params: v2::DynamicToolCallParams,
        response: v2::DynamicToolCallResponse,
    },

    ChatgptAuthTokensRefresh => "account/chatgptAuthTokens/refresh" {
        params: v2::ChatgptAuthTokensRefreshParams,
        response: v2::ChatgptAuthTokensRefreshResponse,
    },

    /// Generate a fresh upstream attestation result on demand.
    AttestationGenerate => "attestation/generate" {
        params: v2::AttestationGenerateParams,
        response: v2::AttestationGenerateResponse,
    },

    #[experimental("currentTime/read")]
    /// Read the current time from an external clock owned by the client.
    CurrentTimeRead => "currentTime/read" {
        params: v2::CurrentTimeReadParams,
        response: v2::CurrentTimeReadResponse,
    },

    /// DEPRECATED APIs below
    /// Request to approve a patch.
    /// This request is used for Turns started via the legacy APIs (i.e. SendUserTurn, SendUserMessage).
    ApplyPatchApproval => "applyPatchApproval" {
        params: v1::ApplyPatchApprovalParams,
        response: v1::ApplyPatchApprovalResponse,
    },
    /// Legacy compatibility request to approve a command.
    /// The app-server retains this wire shape for decoding but does not emit it; live command
    /// approvals use `item/commandExecution/requestApproval`.
    ExecCommandApproval => "execCommandApproval" {
        params: v1::ExecCommandApprovalParams,
        response: v1::ExecCommandApprovalResponse,
    },
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

server_notification_definitions! {
    /// NEW NOTIFICATIONS
    Error => "error" (v2::ErrorNotification),
    ThreadStarted => "thread/started" (v2::ThreadStartedNotification),
    ThreadStatusChanged => "thread/status/changed" (v2::ThreadStatusChangedNotification),
    ThreadQueueChanged => "thread/queue/changed" (v2::ThreadQueueChangedNotification),
    ThreadArchived => "thread/archived" (v2::ThreadArchivedNotification),
    ThreadDeleted => "thread/deleted" (v2::ThreadDeletedNotification),
    ThreadUnarchived => "thread/unarchived" (v2::ThreadUnarchivedNotification),
    #[delivery(required)]
    ThreadClosed => "thread/closed" (v2::ThreadClosedNotification),
    #[delivery(required)]
    SkillsChanged => "skills/changed" (v2::SkillsChangedNotification),
    #[delivery(required)]
    ThreadNameUpdated => "thread/name/updated" (v2::ThreadNameUpdatedNotification),
    #[experimental("project/changed")]
    ProjectChanged => "project/changed" (v2::ProjectChangedNotification),
    #[experimental("thread/project/updated")]
    ThreadProjectUpdated => "thread/project/updated" (v2::ThreadProjectUpdatedNotification),
    #[delivery(required)]
    #[experimental("thread/settings/updated")]
    ThreadSettingsUpdated => "thread/settings/updated" (v2::ThreadSettingsUpdatedNotification),
    #[delivery(required)]
    ThreadTokenUsageUpdated => "thread/tokenUsage/updated" (v2::ThreadTokenUsageUpdatedNotification),
    TurnStarted => "turn/started" (v2::TurnStartedNotification),
    HookStarted => "hook/started" (v2::HookStartedNotification),
    #[delivery(required)]
    TurnCompleted => "turn/completed" (v2::TurnCompletedNotification),
    #[delivery(required)]
    HookCompleted => "hook/completed" (v2::HookCompletedNotification),
    TurnDiffUpdated => "turn/diff/updated" (v2::TurnDiffUpdatedNotification),
    TurnPlanUpdated => "turn/plan/updated" (v2::TurnPlanUpdatedNotification),
    ItemStarted => "item/started" (v2::ItemStartedNotification),
    #[delivery(required)]
    ItemCompleted => "item/completed" (v2::ItemCompletedNotification),
    #[json_export(excluded)]
    /// This event is internal-only. Used by Codex Cloud.
    RawResponseItemCompleted => "rawResponseItem/completed" (v2::RawResponseItemCompletedNotification),
    #[delivery(required)]
    AgentMessageDelta => "item/agentMessage/delta" (v2::AgentMessageDeltaNotification),
    #[delivery(required)]
    /// EXPERIMENTAL - proposed plan streaming deltas for plan items.
    PlanDelta => "item/plan/delta" (v2::PlanDeltaNotification),
    #[delivery(required)]
    /// Stream base64-encoded stdout/stderr chunks for a running `command/exec` session.
    CommandExecOutputDelta => "command/exec/outputDelta" (v2::CommandExecOutputDeltaNotification),
    #[delivery(required)]
    /// Stream base64-encoded stdout/stderr chunks for a running `process/spawn` session.
    #[experimental("process/outputDelta")]
    ProcessOutputDelta => "process/outputDelta" (v2::ProcessOutputDeltaNotification),
    #[delivery(required)]
    /// Final exit notification for a `process/spawn` session.
    #[experimental("process/exited")]
    ProcessExited => "process/exited" (v2::ProcessExitedNotification),
    CommandExecutionOutputDelta => "item/commandExecution/outputDelta" (v2::CommandExecutionOutputDeltaNotification),
    TerminalInteraction => "item/commandExecution/terminalInteraction" (v2::TerminalInteractionNotification),
    FileChangePatchUpdated => "item/fileChange/patchUpdated" (v2::FileChangePatchUpdatedNotification),
    #[delivery(required)]
    ServerRequestResolved => "serverRequest/resolved" (v2::ServerRequestResolvedNotification),
    McpToolCallProgress => "item/mcpToolCall/progress" (v2::McpToolCallProgressNotification),
    #[delivery(required)]
    McpServerOauthLoginCompleted => "mcpServer/oauthLogin/completed" (v2::McpServerOauthLoginCompletedNotification),
    McpServerStatusUpdated => "mcpServer/startupStatus/updated" (v2::McpServerStatusUpdatedNotification),
    #[delivery(required)]
    McpServerStartupCompleted => "mcpServer/startup/completed" (v2::McpServerStartupCompletedNotification),
    AccountUpdated => "account/updated" (v2::AccountUpdatedNotification),
    #[delivery(required)]
    AccountRateLimitsUpdated => "account/rateLimits/updated" (v2::AccountRateLimitsUpdatedNotification),
    AppListUpdated => "app/list/updated" (v2::AppListUpdatedNotification),
    RemoteControlStatusChanged => "remoteControl/status/changed" (v2::RemoteControlStatusChangedNotification),
    ExternalAgentConfigImportProgress => "externalAgentConfig/import/progress" (v2::ExternalAgentConfigImportProgressNotification),
    #[delivery(required)]
    ExternalAgentConfigImportCompleted => "externalAgentConfig/import/completed" (v2::ExternalAgentConfigImportCompletedNotification),
    #[delivery(required)]
    FsChanged => "fs/changed" (v2::FsChangedNotification),
    #[delivery(required)]
    ReasoningSummaryTextDelta => "item/reasoning/summaryTextDelta" (v2::ReasoningSummaryTextDeltaNotification),
    ReasoningSummaryPartAdded => "item/reasoning/summaryPartAdded" (v2::ReasoningSummaryPartAddedNotification),
    #[delivery(required)]
    ReasoningTextDelta => "item/reasoning/textDelta" (v2::ReasoningTextDeltaNotification),
    ModelRerouted => "model/rerouted" (v2::ModelReroutedNotification),
    ModelVerification => "model/verification" (v2::ModelVerificationNotification),
    #[experimental("turn/moderationMetadata")]
    TurnModerationMetadata => "turn/moderationMetadata" (v2::TurnModerationMetadataNotification),
    ModelSafetyBufferingUpdated => "model/safetyBuffering/updated" (v2::ModelSafetyBufferingUpdatedNotification),
    Warning => "warning" (v2::WarningNotification),
    DeprecationNotice => "deprecationNotice" (v2::DeprecationNoticeNotification),
    ConfigWarning => "configWarning" (v2::ConfigWarningNotification),
    FuzzyFileSearchSessionUpdated => "fuzzyFileSearch/sessionUpdated" (FuzzyFileSearchSessionUpdatedNotification),
    #[delivery(required)]
    FuzzyFileSearchSessionCompleted => "fuzzyFileSearch/sessionCompleted" (FuzzyFileSearchSessionCompletedNotification),

    /// Notifies the user of world-writable directories on Windows, which cannot be protected by the sandbox.
    WindowsWorldWritableWarning => "windows/worldWritableWarning" (v2::WindowsWorldWritableWarningNotification),
    #[delivery(required)]
    WindowsSandboxSetupCompleted => "windowsSandbox/setupCompleted" (v2::WindowsSandboxSetupCompletedNotification),

    #[delivery(required)]
    AccountLoginCompleted => "account/login/completed" (v2::AccountLoginCompletedNotification),

}

client_notification_definitions! {
    Initialized,
}


