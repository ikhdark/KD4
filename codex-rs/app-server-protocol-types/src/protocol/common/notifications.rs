use super::FuzzyFileSearchSessionCompletedNotification;
use super::FuzzyFileSearchSessionUpdatedNotification;
use super::decode_jsonrpc_params;
use crate::JSONRPCNotification;
use crate::protocol::v2;
use crate::schema_support::RawSchemaEntry;
use codex_experimental_api_macros::ExperimentalApi;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use strum_macros::Display;
use ts_rs::TS;

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
#[doc(hidden)]
pub struct ServerNotificationMetadata {
    pub method: &'static str,
    pub json_export_excluded: bool,
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

        #[doc(hidden)]
        pub const SERVER_NOTIFICATION_METADATA: &[ServerNotificationMetadata] = &[
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
        #[doc(hidden)]
        pub fn export_server_notification_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let mut schemas = Vec::new();
            $(schemas.push(crate::schema_support::generate_raw_json_schema::<$payload>(stringify!($payload))?);)*
            Ok(schemas)
        }
    };
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

        #[doc(hidden)]
        pub fn export_client_notification_schemas(
        ) -> ::anyhow::Result<Vec<RawSchemaEntry>> {
            let schemas = Vec::new();
            $( $(schemas.push(crate::schema_support::generate_raw_json_schema::<$payload>(stringify!($payload))?);)? )*
            Ok(schemas)
        }
    };
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
    /// Proposed plan streaming deltas for plan items.
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
