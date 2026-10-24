use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

// Standalone image-generation item owned by the image extension. This is also
// the field-level representation exposed by app-server; core and rollout
// persistence only carry it inside an ExtensionItem envelope.
#[derive(Debug, Clone, Deserialize, Serialize, TS, JsonSchema, PartialEq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ImageGenerationItem {
    pub id: String,
    pub status: String,
    pub revised_prompt: Option<String>,
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub saved_path: Option<AbsolutePathBuf>,
    /// Separate image-model receipt; never add these tokens to text inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub provider_usage: Option<ImageGenerationUsage>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ImageGenerationUsage {
    pub model: Option<String>,
    /// False means a response was observed but supplied no usable usage object.
    pub reported: bool,
    #[ts(type = "number | null")]
    pub input_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub output_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub total_tokens: Option<u64>,
    pub input_tokens_details: Option<ImageTokenDetails>,
    pub output_tokens_details: Option<ImageTokenDetails>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, TS, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ImageTokenDetails {
    #[ts(type = "number | null")]
    pub image_tokens: Option<u64>,
    #[ts(type = "number | null")]
    pub text_tokens: Option<u64>,
}
