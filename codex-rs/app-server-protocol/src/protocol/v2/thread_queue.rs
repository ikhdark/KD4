use super::Turn;
use super::UserInput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

/// A durable follow-up, in queue order. Starting it uses the current thread settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct QueuedSubmission {
    pub id: String,
    pub client_user_message_id: Option<String>,
    pub input: Vec<UserInput>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueListParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueListResponse {
    pub data: Vec<QueuedSubmission>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueAddParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[ts(optional = nullable)]
    pub client_user_message_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueAddResponse {
    pub queued_submission: QueuedSubmission,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueUpdateParams {
    pub thread_id: String,
    pub queued_submission_id: String,
    pub input: Vec<UserInput>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueUpdateResponse {
    pub queued_submission: QueuedSubmission,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueDeleteParams {
    pub thread_id: String,
    pub queued_submission_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueDeleteResponse {
    pub deleted: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueReorderParams {
    pub thread_id: String,
    /// Listed entries move to the front; omitted entries retain their relative order.
    pub queued_submission_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueReorderResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueStartParams {
    pub thread_id: String,
    pub queued_submission_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueStartResponse {
    pub turn: Turn,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadQueueChangedNotification {
    pub thread_id: String,
}

#[cfg(test)]
mod tests {
    use crate::ClientRequest;
    use serde_json::json;

    #[test]
    fn thread_queue_desktop_requests_round_trip() {
        let input = json!([{"type":"text", "text":"next task", "text_elements":[]}]);
        for (method, params) in [
            ("list", json!({"threadId":"thread", "cursor":null})),
            (
                "add",
                json!({"threadId":"thread", "input":input, "clientUserMessageId":"client"}),
            ),
            (
                "update",
                json!({"threadId":"thread", "queuedSubmissionId":"item", "input":input}),
            ),
            (
                "delete",
                json!({"threadId":"thread", "queuedSubmissionId":"item"}),
            ),
            (
                "reorder",
                json!({"threadId":"thread", "queuedSubmissionIds":["item"]}),
            ),
            (
                "start",
                json!({"threadId":"thread", "queuedSubmissionId":"item"}),
            ),
        ] {
            let method = format!("thread/queue/{method}");
            let request: ClientRequest = serde_json::from_value(json!({
                "id":1, "method":method, "params":params,
            }))
            .expect("Desktop queue request");
            let encoded = serde_json::to_value(&request).expect("encode queue request");
            assert_eq!(encoded["method"], method);
            assert_eq!(encoded["params"]["threadId"], "thread");
            assert!(matches!(request.serialization_scope(),
                Some(crate::ClientRequestSerializationScope::Thread { thread_id }) if thread_id == "thread"));
        }
    }
}
