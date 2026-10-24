use codex_utils_path_uri::PathUri;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;



#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EnvironmentInfoParams {
    pub environment_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EnvironmentInfoResponse {
    pub shell: EnvironmentShellInfo,
    /// Default working directory reported by the environment, as a canonical file URI.
    pub cwd: Option<PathUri>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct EnvironmentShellInfo {
    /// Stable shell name, for example `zsh`, `bash`, `powershell`, `sh`, or `cmd`.
    pub name: String,
    /// Target-native shell executable path or command name.
    pub path: String,
}
