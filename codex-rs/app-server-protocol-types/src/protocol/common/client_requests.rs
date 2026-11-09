use std::path::PathBuf;

use super::FuzzyFileSearchParams;
use super::FuzzyFileSearchResponse;
use super::FuzzyFileSearchSessionStartParams;
use super::FuzzyFileSearchSessionStartResponse;
use super::FuzzyFileSearchSessionStopParams;
use super::FuzzyFileSearchSessionStopResponse;
use super::FuzzyFileSearchSessionUpdateParams;
use super::FuzzyFileSearchSessionUpdateResponse;
use super::decode_jsonrpc_params;
use crate::JSONRPCRequest;
use crate::RequestId;
use crate::protocol::v1;
use crate::protocol::v2;
use crate::schema_support::RawSchemaEntry;
use crate::schema_support::generate_raw_json_schema;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

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

/// Resource admission is independent of semantic serialization: process cleanup
/// still follows process registration, but must not need an ordinary work slot.
pub fn client_request_is_control(method: &str) -> bool {
    matches!(
        method,
        "turn/interrupt" | "command/exec/terminate" | "process/kill"
            | "thread/backgroundTerminals/clean" | "thread/backgroundTerminals/terminate"
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

        #[doc(hidden)]
        pub const EXPERIMENTAL_CLIENT_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? => $wire),
            )*
        ];
        #[doc(hidden)]
        pub const EXPERIMENTAL_CLIENT_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        #[doc(hidden)]
        pub const EXPERIMENTAL_CLIENT_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

        pub fn export_client_responses(
            out_dir: &::std::path::Path,
        ) -> ::std::result::Result<(), ::ts_rs::ExportError> {
            let config = ::ts_rs::Config::from_env().with_out_dir(out_dir).with_large_int("number");
            $(
                <$response as ::ts_rs::TS>::export_all(&config)?;
            )*
            Ok(())
        }

        #[allow(clippy::vec_init_then_push)]
        #[doc(hidden)]
        pub fn export_client_response_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(generate_raw_json_schema::<$response>(stringify!($response))?);
            )*
            Ok(schemas)
        }

        #[allow(clippy::vec_init_then_push)]
        #[doc(hidden)]
        pub fn export_client_param_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(generate_raw_json_schema::<$params>(stringify!($params))?);
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
    FsReadFileRange => "fs/readFileRange" {
        params: v2::FsReadFileRangeParams,
        serialization: None,
        response: v2::FsReadFileRangeResponse,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional = nullable)] #[serde(skip_serializing_if = "Option::is_none")] v2::NullableRemoteControlEnableParams,
        serialization: global("remote-control"),
        response: v2::RemoteControlEnableResponse,
    },
    #[experimental("remoteControl/disable")]
    RemoteControlDisable => "remoteControl/disable" {
        params: #[ts(optional = nullable)] #[serde(skip_serializing_if = "Option::is_none")] v2::NullableRemoteControlDisableParams,
        serialization: global("remote-control"),
        response: v2::RemoteControlDisableResponse,
    },
    #[experimental("remoteControl/status/read")]
    RemoteControlStatusRead => "remoteControl/status/read" {
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: global("account-auth"),
        response: v2::LogoutAccountResponse,
    },

    GetAccountRateLimits => "account/rateLimits/read" {
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountRateLimitsResponse,
    },

    ConsumeAccountRateLimitResetCredit => "account/rateLimitResetCredit/consume" {
        params: v2::ConsumeAccountRateLimitResetCreditParams,
        serialization: global("account-auth"),
        response: v2::ConsumeAccountRateLimitResetCreditResponse,
    },

    GetAccountTokenUsage => "account/usage/read" {
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
        serialization: None,
        response: v2::GetAccountTokenUsageResponse,
    },

    GetWorkspaceMessages => "account/workspaceMessages/read" {
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
        params: #[ts(optional, type = "null | Record<string, never>")] #[schemars(default, schema_with = "crate::schema_support::no_params_schema")] #[serde(skip_serializing_if = "Option::is_none")] Option<()>,
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
