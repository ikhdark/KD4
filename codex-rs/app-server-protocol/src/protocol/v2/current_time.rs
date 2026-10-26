use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use ts_rs::TS;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CurrentTimeReadParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct CurrentTimeReadResponse {
    /// Current time as whole Unix seconds.
    #[ts(type = "number")]
    pub current_time_at: i64,
}

impl TryFrom<SystemTime> for CurrentTimeReadResponse {
    type Error = String;

    fn try_from(now: SystemTime) -> Result<Self, Self::Error> {
        let seconds = now
            .duration_since(UNIX_EPOCH)
            .map_err(|err| format!("system time is before the Unix epoch: {err}"))?
            .as_secs();
        let current_time_at = i64::try_from(seconds)
            .map_err(|_| "current Unix time does not fit in an i64".to_string())?;
        Ok(Self { current_time_at })
    }
}

