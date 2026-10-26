use serde_json::Value as JsonValue;

use crate::pull::PullEvent;

// Convert a single JSON object representing a pull update into one or more events.
pub(crate) fn pull_events_from_value(value: &JsonValue) -> Vec<PullEvent> {
    let mut events = Vec::new();
    if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
        events.push(PullEvent::Status(status.to_string()));
        if status == "success" {
            events.push(PullEvent::Success);
        }
    }
    let total = value.get("total").and_then(JsonValue::as_u64);
    let completed = value.get("completed").and_then(JsonValue::as_u64);
    if total.is_some() || completed.is_some() {
        events.push(PullEvent::ChunkProgress {
            digest: value.get("digest").and_then(JsonValue::as_str)
                .unwrap_or("").to_string(),
            total,
            completed,
        });
    }
    events
}

