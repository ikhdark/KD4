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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    #[test]
    fn current_time_response_serializes_whole_unix_seconds() {
        for (millis, seconds) in [(0, 0), (999, 0), (12_345, 12), (98_765, 98)] {
            let response =
                CurrentTimeReadResponse::try_from(UNIX_EPOCH + Duration::from_millis(millis))
                    .expect("post-epoch time should produce a response");
            let value = serde_json::to_value(&response).expect("serialize current time");
            assert_eq!(value, serde_json::json!({ "currentTimeAt": seconds }));
            assert_eq!(
                serde_json::from_value::<CurrentTimeReadResponse>(value)
                    .expect("deserialize current time"),
                response
            );
        }
    }

    #[test]
    fn current_time_response_rejects_time_before_unix_epoch() {
        let error = CurrentTimeReadResponse::try_from(UNIX_EPOCH - Duration::from_millis(1))
            .expect_err("pre-epoch time should be rejected");
        assert!(error.starts_with("system time is before the Unix epoch:"));
    }
}
