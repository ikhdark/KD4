use crate::models::AdditionalPermissionProfile;
use crate::models::FileSystemPermissions;
use crate::models::NetworkPermissions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantScope {
    #[default]
    Turn,
    Session,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct RequestPermissionProfile {
    pub network: Option<NetworkPermissions>,
    pub file_system: Option<FileSystemPermissions<PathUri>>,
}

impl RequestPermissionProfile {
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}

/// URI-preserving permission overlay granted by `request_permissions`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct UriAdditionalPermissionProfile {
    pub network: Option<NetworkPermissions>,
    pub file_system: Option<FileSystemPermissions<PathUri>>,
}

impl UriAdditionalPermissionProfile {
    pub fn is_empty(&self) -> bool {
        self.network.is_none() && self.file_system.is_none()
    }
}

impl From<RequestPermissionProfile> for UriAdditionalPermissionProfile {
    fn from(value: RequestPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value.file_system,
        }
    }
}

impl From<UriAdditionalPermissionProfile> for RequestPermissionProfile {
    fn from(value: UriAdditionalPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value.file_system,
        }
    }
}

impl From<AdditionalPermissionProfile> for UriAdditionalPermissionProfile {
    fn from(value: AdditionalPermissionProfile) -> Self {
        Self {
            network: value.network,
            file_system: value
                .file_system
                .map(FileSystemPermissions::<PathUri>::from),
        }
    }
}

impl TryFrom<UriAdditionalPermissionProfile> for AdditionalPermissionProfile {
    type Error = io::Error;

    fn try_from(value: UriAdditionalPermissionProfile) -> Result<Self, Self::Error> {
        Ok(Self {
            network: value.network,
            file_system: value
                .file_system
                .map(FileSystemPermissions::<AbsolutePathBuf>::try_from)
                .transpose()?,
        })
    }
}

impl From<AdditionalPermissionProfile> for RequestPermissionProfile {
    fn from(value: AdditionalPermissionProfile) -> Self {
        UriAdditionalPermissionProfile::from(value).into()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsArgs {
    #[serde(
        default,
        rename = "environment_id",
        alias = "environmentId",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    pub environment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsResponse {
    pub permissions: RequestPermissionProfile,
    #[serde(default)]
    pub scope: PermissionGrantScope,
    /// Review every subsequent command in this turn before normal sandboxed execution.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_auto_review: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct RequestPermissionsEvent {
    /// Responses API call id for the associated tool call, if available.
    pub call_id: String,
    /// Turn ID that this request belongs to.
    /// Uses `#[serde(default)]` for backwards compatibility.
    #[serde(default)]
    pub turn_id: String,
    #[serde(
        default,
        rename = "environmentId",
        alias = "environment_id",
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(optional)]
    #[ts(rename = "environmentId")]
    pub environment_id: Option<String>,
    #[ts(type = "number")]
    pub started_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub permissions: RequestPermissionProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cwd: Option<AbsolutePathBuf>,
    /// URI-preserving working directory for the selected environment.
    ///
    /// Older persisted events only contain `cwd`, so this remains optional at
    /// the protocol boundary. New producers always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub cwd_uri: Option<PathUri>,
}

#[cfg(test)]
mod tests {
    use super::RequestPermissionsEvent;
    use serde_json::json;

    #[test]
    fn request_permissions_event_accepts_legacy_payload_without_cwd_uri() {
        let event = serde_json::from_value::<RequestPermissionsEvent>(json!({
            "call_id": "call-1",
            "started_at_ms": 0,
            "permissions": {}
        }))
        .expect("legacy request-permissions event should deserialize");

        assert_eq!(event.cwd_uri, None);
    }
}
