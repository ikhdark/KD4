use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

pub const DESKTOP_CLIENT_NAME: &str = "codex_desktop";
pub const DESKTOP_RUNTIME_RECEIPT_RELATIVE_PATH: &str = "runtime/desktop-app-server-runtime.json";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DesktopRuntimeReceipt {
    pub schema_version: u32,
    pub pid: u32,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub codex_home: PathBuf,
    pub client_name: String,
    pub build_version: String,
    pub build_commit: String,
    pub build_dirty: String,
    pub build_profile: String,
    pub build_built: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerBuildInfo {
    pub version: String,
    pub commit: String,
    pub dirty: String,
    pub profile: String,
    pub built: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerRuntimeInfo {
    pub executable_path: Option<AbsolutePathBuf>,
    pub install_method: String,
    pub package_layout: Option<ServerPackageLayout>,
    pub expected_local_binary_path: Option<AbsolutePathBuf>,
    pub local_binary_match: Option<bool>,
    pub warnings: Vec<ServerRuntimeWarning>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerRuntimeWarning {
    pub code: String,
    pub message: String,
    pub action: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerPackageLayout {
    pub package_dir: AbsolutePathBuf,
    pub bin_dir: AbsolutePathBuf,
    pub resources_dir: Option<AbsolutePathBuf>,
    pub path_dir: Option<AbsolutePathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Effective feature keys enabled for this app-server process.
    pub enabled_features: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ServerLocalWatermark {
    pub version: String,
    pub label: String,
    pub detail: String,
}
