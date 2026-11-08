use super::decode_jsonrpc_params;
use crate::JSONRPCRequest;
use crate::RequestId;
use crate::protocol::v1;
use crate::protocol::v2;
use crate::schema_support::RawSchemaEntry;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

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
            /// Content-free registered wire method, without serializing params.
            pub fn method(&self) -> &'static str {
                match self { $(Self::$variant { .. } => $wire,)* }
            }

            /// Whether this registry recognizes a method, independent of parameter validity.
            pub fn is_known_method(method: &str) -> bool {
                matches!(method, $($wire)|*)
            }

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

        #[doc(hidden)]
        pub const EXPERIMENTAL_SERVER_METHODS: &[&str] = &[
            $(
                experimental_method_entry!($(#[experimental($reason)])? => $wire),
            )*
        ];
        #[doc(hidden)]
        pub const EXPERIMENTAL_SERVER_METHOD_PARAM_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $params),
            )*
        ];
        #[doc(hidden)]
        pub const EXPERIMENTAL_SERVER_METHOD_RESPONSE_TYPES: &[&str] = &[
            $(
                experimental_type_entry!($(#[experimental($reason)])? $response),
            )*
        ];

        pub fn export_server_responses(
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
        pub fn export_server_response_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(crate::schema_support::generate_raw_json_schema::<$response>(
                    concat!(stringify!($variant), "Response"),
                )?);
            )*
            Ok(schemas)
        }

        #[allow(clippy::vec_init_then_push)]
        #[doc(hidden)]
        pub fn export_server_param_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let mut schemas = Vec::new();
            $(
                schemas.push(crate::schema_support::generate_raw_json_schema::<$params>(
                    concat!(stringify!($variant), "Params"),
                )?);
            )*
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

    /// Request input from the user for a tool call.
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
