use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// Exact decoded report text. The server accepts no attachments or turn data.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct BugCreateParams {
    pub thread_id: String,
    pub raw_text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct BugCreateResponse {
    pub id: i64,
    pub display_id: String,
    /// Always `pending` when returned; persistence has committed already.
    pub status: String,
    pub durable_save_result: bool,
}
