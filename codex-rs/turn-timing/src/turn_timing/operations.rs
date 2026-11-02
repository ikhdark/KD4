//! Result-owner inventory. Exact immutable byte reads and observed command
//! revisions are deliberately different qualifications; the latter cannot
//! certify arbitrary external process/environment/workspace state.
use super::*;
use serde_json::{Value, json};

const LIMIT: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Capture {
    actual: u64,
    rows: BTreeMap<String, Value>,
    dropped: u64,
}

impl Capture {
    pub(super) fn snapshot(&self, calls: &[TurnTimingToolCall]) -> Value {
        let mut complete = self.dropped == 0 && self.actual != u64::MAX;
        let mut rows = Vec::new();
        for (execution, source) in &self.rows {
            let mut row = source.clone();
            let call = calls.iter().find(|call| call.execution_id.0 == *execution);
            row["startMs"] = json!(call.and_then(|call| call.accepted_at_ms));
            row["endMs"] = json!(call.and_then(|call| call.delivered_at_ms));
            row["outcome"] = json!(call.and_then(|call| call.outcome.as_deref()).and_then(
                |outcome| match outcome {
                    "success" => Some("success"),
                    "failed" | "failure" | "error" => Some("failed"),
                    "aborted" | "cancelled" => Some("cancelled"),
                    _ => None,
                }
            ));
            complete &= row["inputStateSha256"].is_string()
                && row["offsetBytes"].is_u64()
                && row["lengthBytes"].is_u64()
                && row["startMs"]
                    .as_u64()
                    .zip(row["endMs"].as_u64())
                    .is_some_and(|(start, end)| start <= end)
                && row["outcome"].is_string();
            rows.push(row);
        }
        json!({"complete":complete && self.actual == rows.len() as u64,
            "actualCount":self.actual, "rows":rows, "droppedRows":self.dropped,
            "scope":"native_command_launch_calls_and_immutable_snapshot_read_deliveries; not process child census",
            "unavailableReason":if complete { None } else { Some("exact_command_state_or_complete_immutable_read_extent_not_recorded") }})
    }
}

impl TurnTimingState {
    pub fn record_native_operation(
        &self,
        execution: &str,
        call: &str,
        kind: &str,
        receipt: Option<Value>,
        history: Option<Value>,
    ) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() {
            return;
        }
        let capture = &mut state.measurements.operations;
        capture.actual = capture.actual.saturating_add(1);
        if capture.rows.len() >= LIMIT || capture.rows.contains_key(execution) {
            capture.dropped = capture.dropped.saturating_add(1);
            return;
        }
        let mut row = json!({"executionId":execution, "operationId":execution, "callId":call,
            "kind":kind, "retryOf":null});
        if let Some(receipt) = receipt.filter(|_| kind == "read") {
            for key in [
                "resourceId",
                "contentSha256",
                "contentBytes",
                "ranges",
                "basis",
            ] {
                if let Some(value) = receipt.get(key) {
                    row[key] = value.clone();
                }
            }
            let identity = json!([row["resourceId"], row["contentSha256"], row["contentBytes"]]);
            row["inputStateSha256"] =
                json!(hex::encode(Sha256::digest(identity.to_string().as_bytes())));
            if let Some(ranges) = receipt["ranges"]
                .as_array()
                .filter(|ranges| ranges.len() == 1)
                && let (Some(start), Some(end)) = (ranges[0][0].as_u64(), ranges[0][1].as_u64())
                && let Some(length) = end.checked_sub(start)
            {
                row["offsetBytes"] = json!(start);
                row["lengthBytes"] = json!(length);
            }
        }
        if let Some(history) = history {
            let instance = &history["instance"];
            row["observedInputSha256"] = instance["input_revision"].clone();
            row["observedStateSha256"] = instance["observed_state_revision"].clone();
            row["configurationSha256"] = instance["configuration_revision"].clone();
            row["stateCoverage"] = history["state_coverage"].clone();
        }
        capture.rows.insert(execution.to_owned(), row);
    }
}
