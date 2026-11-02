//! Bounded evidence attached to the existing terminal timing snapshot.
//! Facts are recorded at transport/result ownership boundaries. Semantic work
//! labels are explicit assertions, never inferred from a failed call or edit.
use super::*;
use serde_json::{Value, json};

const LIMIT: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Capture {
    enabled: bool,
    actual_attempts: u64,
    actual_logical_requests: u64,
    actual_outputs: u64,
    attempts: BTreeMap<String, Value>,
    outputs: BTreeMap<String, Value>,
    annotations: BTreeMap<String, Value>,
    recovery_evidence: Option<Value>,
    dropped_attempts: u64,
    dropped_outputs: u64,
}

fn digest(value: &impl serde::Serialize) -> Option<String> {
    serde_json::to_vec(value).ok().map(|bytes| hex::encode(Sha256::digest(bytes)))
}

fn text(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| !value.trim().is_empty() && value.len() <= 256)
}

impl Capture {
    pub(super) fn logical_request(&mut self) {
        self.enabled = true;
        self.actual_logical_requests = self.actual_logical_requests.saturating_add(1);
    }

    pub(super) fn dispatch(&mut self, request: &str, attempt: &str, at_ms: u64, retry: bool) {
        self.enabled = true;
        self.actual_attempts = self.actual_attempts.saturating_add(1);
        if self.annotations.get("next").is_some_and(|row| row["afterRequestId"].as_str() != Some(request))
            && let Some(mut annotation) = self.annotations.remove("next") {
            annotation["samplingRequestId"] = json!(request);
            if let Some(fields) = annotation.as_object_mut() { fields.remove("afterRequestId"); }
            self.annotations.insert(request.to_owned(), annotation);
        }
        if self.attempts.len() >= LIMIT {
            self.dropped_attempts = self.dropped_attempts.saturating_add(1);
            return;
        }
        self.attempts.insert(attempt.to_owned(), json!({
            "request_id":request, "attempt_id":attempt, "parent_request_id":null,
            "start_time":at_ms as f64 / 1000.0, "first_token_time":null,
            "end_time":null, "outcome":null, "retention_status":"incomplete",
            "recovery_reason":if retry { Some("native_model_retry") } else { None },
            "recovery_induced":retry,
            "recovery_event_id":if retry { Some(format!("retry:{attempt}")) } else { None },
            "recovery_evidence_ref":if retry { Some(format!("native:dispatch:{attempt}")) } else { None }
        }));
    }

    pub(super) fn finish(&mut self, attempt: &str, at_ms: u64, outcome: &str, progress: bool, usage: Option<&TokenUsage>) {
        if let Some(row) = self.attempts.get_mut(attempt) {
            row["end_time"] = json!(at_ms as f64 / 1000.0);
            row["outcome"] = json!(outcome);
            row["native_actionable_output_observed"] = json!(progress);
            row["retention_status"] = json!("complete");
            if let Some(usage) = usage {
                row["input_tokens"] = json!(usage.known_input());
                row["cached_tokens"] = json!(usage.known_cached_input());
                row["output_tokens"] = json!(usage.known_output());
                row["reasoning_tokens"] = json!(usage.known_reasoning_output());
                if usage.known_output() == Some(0) {
                    row["no_output"] = json!(true);
                }
            }
        }
    }

    pub(super) fn output(&mut self, execution: &str, row: Value) {
        self.enabled = true;
        // This is called once by the result-return owner, before retention.
        // Repeated identities are not a second successful observation.
        self.actual_outputs = self.actual_outputs.saturating_add(1);
        if !row.is_object() || self.outputs.len() >= LIMIT || self.outputs.contains_key(execution) {
            self.dropped_outputs = self.dropped_outputs.saturating_add(1);
            return;
        }
        self.outputs.insert(execution.to_owned(), row);
    }

    pub(super) fn snapshot(&self, requests: &[TurnTimingModelRequest], duration_ms: u64,
                          clock: Option<&str>, scope: &str, retries: &Value) -> (Option<Value>, Option<Value>) {
        if !self.enabled { return (None, None); }
        let mut annotations = Vec::new();
        let direct_context = requests.iter().any(|request| request.logical_context_sha256.is_some());
        let mut previous: Option<(&TurnTimingModelRequest, String)> = None;
        for request in requests {
            let Some(id) = &request.sampling_request_id else { continue; };
            let mut row = self.annotations.get(id).cloned().unwrap_or_else(|| json!({"samplingRequestId":id}));
            // These are full logical-prompt provenance hashes, never WS delta
            // hashes or token-volume guesses. Ordinary history growth counts.
            let context = if direct_context { request.logical_context_sha256.clone() } else {
                request.request_token_categories.as_ref()
                .filter(|_| request.request_diagnostics_status == TurnTimingRequestDiagnosticsStatus::Complete)
                .filter(|categories| !categories.prompt_section_sha256.is_empty())
                .and_then(|categories| digest(&categories.prompt_section_sha256))
            };
            if let Some(context) = context {
                row["contextSha256"] = json!(context);
                row["contextBasis"] = json!(if direct_context { "full_logical_prompt_v1" }
                    else { "full_logical_prompt_section_hashes_v1" });
                if let Some((prior, before)) = &previous {
                    if prior.completed_ms.zip(request.dispatch_ms).is_some_and(|(end, start)| end <= start) {
                        row["contextChange"] = if before == &context { Value::Null } else { json!({
                            "previousRequestId":prior.sampling_request_id, "beforeSha256":before,
                            "afterSha256":context, "reason":"logical_prompt_components_changed",
                            "evidenceRef":format!("native:prompt_section_sha256:{id}")
                        }) };
                    }
                } else if annotations.is_empty() {
                    row["contextChange"] = Value::Null;
                }
                previous = Some((request, context));
            } else {
                previous = None;
            }
            annotations.push(row);
        }
        let mut attempts = self.attempts.values().cloned().collect::<Vec<_>>();
        let mut events = Vec::new();
        for attempt in &mut attempts {
            if let Some(annotation) = attempt["request_id"].as_str().and_then(|id| self.annotations.get(id)) {
                for (from, to) in [("unnecessary","unnecessary"),("necessityEvidenceRef","necessity_evidence_ref"),
                                  ("operationId","operation_id"),("duplicateOf","duplicate_of"),
                                  ("modelTimeAttribution","model_time_attribution")] {
                    if let Some(value) = annotation.get(from) { attempt[to] = value.clone(); }
                }
                if let Some(mut packet) = annotation.get("modelTimeAttribution").cloned()
                    && let Some(mut segment) = packet.as_object_mut().and_then(|packet| packet.remove("requestSpan")) {
                    segment["start_time"] = attempt["start_time"].clone();
                    segment["end_time"] = attempt["end_time"].clone();
                    packet["segments"] = json!([segment]);
                    attempt["model_time_attribution"] = packet;
                }
            }
            if attempt["recovery_induced"] == true {
                let mut event = json!({
                    "event_id":attempt["recovery_event_id"], "mechanism":"retry",
                    "recovery_reason":"native_model_retry", "start_time":attempt["start_time"],
                    "end_time":attempt["end_time"], "outcome":null,
                    "evidence_ref":attempt["recovery_evidence_ref"]
                });
                // Operational recovery success: a completed retry produced
                // actionable output. This is not reviewed task correctness.
                if attempt["outcome"] == "success" {
                    if attempt["native_actionable_output_observed"] == true {
                        event["outcome"] = json!("progress_restored");
                        event["progress_evidence_ref"] = json!(format!("native:actionable_output:{}", attempt["attempt_id"].as_str().unwrap_or_default()));
                    } else {
                        event["outcome"] = json!("no_progress");
                    }
                } else if attempt["outcome"] == "failed" || attempt["outcome"] == "cancelled" {
                    event["outcome"] = json!("failed");
                }
                events.push(event);
            }
        }
        if let Some(decisions) = retries.get("rows").and_then(Value::as_array) {
            for decision in decisions {
                let target = decision["executed_physical_attempt_id"].as_str();
                let matching = target.and_then(|target| events.iter_mut()
                    .find(|event| event["event_id"].as_str() == Some(format!("retry:{target}").as_str())));
                if let Some(event) = matching {
                    event["start_time"] = decision["start_time"].clone();
                    event["recovery_reason"] = decision["trigger"].clone();
                    event["setup_end_time"] = decision["end_time"].clone();
                    event["backoff_ns"] = decision["actual_backoff_ns"].clone();
                } else {
                    events.push(json!({
                        "event_id":format!("retry-decision:{}", decision["decision_id"]),
                        "mechanism":"retry", "recovery_reason":decision["trigger"],
                        "start_time":decision["start_time"], "end_time":decision["end_time"],
                        "outcome":if decision["phase"] == "not_executed" { Some("no_progress") } else { None },
                        "evidence_ref":format!("native:retry-decision:{}", decision["decision_id"]),
                        "backoff_ns":decision["actual_backoff_ns"]
                    }));
                }
            }
        }
        let mut recovery = json!({
            "schema_version":1, "scope_id":scope, "clock_id":clock,
            "start_time":0, "end_time":duration_ms as f64 / 1000.0,
            "scope_complete":self.dropped_attempts == 0,
            "actual_request_count":self.actual_attempts,
            "counter_basis":"independent_physical_attempt_counter",
            "counter_saturated":self.actual_attempts == u64::MAX,
            "counter_evidence_ref":"native:unique_physical_transport_entry",
            "attempts":attempts, "events_complete":self.dropped_attempts == 0 && retries["complete"] == true, "events":events,
            "event_inventory_scope":"physical_model_transport_retries",
            "network_complete":false, "network_events":[], "necessity_complete":false,
            "nativeCapture":{"droppedAttempts":self.dropped_attempts,
                "retryDecisions":retries,
                "eventScope":"physical_model_retry_activity; other recovery mechanisms and causal delay require referenced evidence"}
        });
        if let Some(evidence) = &self.recovery_evidence {
            if let Some(labels) = evidence.get("events").and_then(Value::as_array) {
                for label in labels {
                    if let Some(event) = recovery["events"].as_array_mut().and_then(|events| events.iter_mut()
                        .find(|event| event["event_id"] == label["event_id"])) {
                        if let (Some(target), Some(source)) = (event.as_object_mut(), label.as_object()) {
                            for key in ["outcome","progress_evidence_ref","necessity","unnecessary","necessity_evidence_ref",
                                        "operation_id","duplicate_of"] {
                                if let Some(value) = source.get(key) { target.insert(key.to_owned(), value.clone()); }
                            }
                        }
                    }
                }
            }
            for key in ["critical_path", "necessity_complete", "model_time_attribution_complete",
                        "model_time_components_complete"] {
                if let Some(value) = evidence.get(key) { recovery[key] = value.clone(); }
            }
        }
        if recovery["attempts"].as_array().is_some_and(|rows| rows.iter().all(|row| row["unnecessary"].is_boolean()))
            && recovery["events"].as_array().is_some_and(|rows| rows.iter().all(|row| row["unnecessary"].is_boolean())) {
            recovery["necessity_complete"] = json!(true);
        }
        if recovery["attempts"].as_array().is_some_and(|rows| rows.iter()
            .all(|row| row["model_time_attribution"]["complete"] == true)) {
            recovery["model_time_attribution_complete"] = json!(true);
        }
        (Some(json!({
            "schemaVersion":1,
            "requests":{"complete":annotations.len() as u64 == self.actual_logical_requests,
                        "actualCount":self.actual_logical_requests, "rows":annotations},
            "outputs":{"complete":self.dropped_outputs == 0, "actualCount":self.actual_outputs,
                       "rows":self.outputs.values().collect::<Vec<_>>()},
            "capture":{"scope":"tool_runtime_results_before_caller_display", "droppedOutputs":self.dropped_outputs,
                "semanticLabels":"explicit_referenced_assertions_not_automatic_waste_verdicts"}
        })), Some(recovery))
    }
}

impl TurnTimingState {
    pub fn record_model_context_sha256(&self, hash: String) {
        if let Some(request) = self.state().model_requests.last_mut() {
            request.logical_context_sha256 = Some(hash);
        }
    }

    /// Validate the optional annotations before the plan owner performs effects.
    pub fn validate_measurement_annotations(&self, packet: &Value) -> Result<Value, String> {
        if serde_json::to_vec(packet).map_err(|error| error.to_string())?.len() > 32 * 1024 {
            return Err("measurement annotations exceed 32 KiB".into());
        }
        let object = packet.as_object().ok_or("measurements must be an object")?;
        if object.keys().any(|key| !matches!(key.as_str(), "requests" | "recovery")) {
            return Err("measurements accepts only requests and recovery".into());
        }
        let state = self.state();
        let mut resolved = packet.clone();
        if let Some(rows) = resolved.get_mut("requests") {
            let rows = rows.as_array_mut().filter(|rows| rows.len() <= 128)
                .ok_or("measurements.requests must contain at most 128 annotations")?;
            let mut seen = BTreeSet::new();
            for row in rows {
                let id = row.get("samplingRequestId").and_then(Value::as_str).ok_or("samplingRequestId is required")?;
                let next = id == "next";
                let id = if id == "current" || next {
                    state.model_requests.last().and_then(|request| request.sampling_request_id.as_deref())
                } else { Some(id) }.ok_or("current sampling request is unavailable")?.to_owned();
                let request = state.model_requests.iter().find(|request| request.sampling_request_id.as_deref() == Some(&id))
                    .ok_or("measurement request is not retained in this turn")?;
                if !seen.insert(if next { "next".to_owned() } else { id.clone() }) {
                    return Err("duplicate measurement request identity".into());
                }
                if !state.measurements.annotations.contains_key(&id)
                    && state.measurements.annotations.len() + seen.len() > LIMIT {
                    return Err("request annotation retention is full".into());
                }
                if let Some(kind) = row.get("workKind")
                    && !matches!(kind.as_str(), Some("implementation" | "rework" | "other")) {
                    return Err("workKind must be implementation, rework or other".into());
                }
                if row.get("workKind").and_then(Value::as_str) == Some("rework") {
                    let revision = row.get("revision").ok_or("rework requires revision evidence")?;
                    let hash = revision.get("artifactSha256").and_then(Value::as_str).unwrap_or_default();
                    let at = revision.get("implementedAtMs").and_then(Value::as_u64);
                    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                        || !text(revision.get("evidenceRef")) || !at.zip(request.dispatch_ns)
                            .is_some_and(|(at, dispatch)| u128::from(at) * 1_000_000 <= dispatch) {
                        return Err("rework requires an implemented artifact SHA256, referenced attribution and a pre-dispatch implementation milestone".into());
                    }
                }
                if let Some(unnecessary) = row.get("unnecessary") {
                    if !unnecessary.is_boolean() || !text(row.get("necessityEvidenceRef")) {
                        return Err("necessity labels require a boolean and necessityEvidenceRef".into());
                    }
                }
                row["samplingRequestId"] = json!(if next { "next" } else { &id });
                if next { row["afterRequestId"] = json!(id); }
            }
        }
        if let Some(recovery) = resolved.get("recovery") {
            if !recovery.is_object() { return Err("measurements.recovery must be an object".into()); }
            if let Some(events) = recovery.get("events") {
                let events = events.as_array().filter(|events| events.len() <= 128)
                    .ok_or("recovery.events must contain at most 128 labels")?;
                if events.iter().any(|event| !text(event.get("event_id"))) {
                    return Err("recovery event labels require an event_id".into());
                }
                let mut known = state.measurements.attempts.values()
                    .filter(|attempt| attempt["recovery_induced"] == true)
                    .filter_map(|attempt| attempt["recovery_event_id"].as_str().map(str::to_owned))
                    .collect::<BTreeSet<_>>();
                let decisions = state.retry_decisions.measurement_snapshot(
                    state.started_sample.map(|sample| sample.time.monotonic_ns));
                if let Some(decisions) = decisions["rows"].as_array() {
                    for decision in decisions {
                        let bound = decision["executed_physical_attempt_id"].as_str()
                            .is_some_and(|id| known.contains(&format!("retry:{id}")));
                        if !bound { known.insert(format!("retry-decision:{}", decision["decision_id"])); }
                    }
                }
                let mut unique = BTreeSet::new();
                for event in events {
                    let id = event["event_id"].as_str().unwrap_or_default();
                    if !known.contains(id) || !unique.insert(id) {
                        return Err("recovery labels require unique retained native event IDs".into());
                    }
                    if event["outcome"] == "progress_restored" && !text(event.get("progress_evidence_ref")) {
                        return Err("restored progress requires progress_evidence_ref".into());
                    }
                }
            }
        }
        Ok(resolved)
    }

    pub fn record_measurement_annotations(&self, packet: Value) {
        let mut state = self.state();
        state.measurements.enabled = true;
        if let Some(rows) = packet.get("requests").and_then(Value::as_array) {
            for row in rows {
                if let Some(id) = row.get("samplingRequestId").and_then(Value::as_str) {
                    state.measurements.annotations.insert(id.to_owned(), row.clone());
                }
            }
        }
        if let Some(recovery) = packet.get("recovery") {
            state.measurements.recovery_evidence = Some(recovery.clone());
        }
    }

    pub fn record_model_attempt_outcome(&self, attempt: &str, outcome: &str, progress: bool, usage: Option<&TokenUsage>) {
        let mut state = self.state();
        if let Some(at_ms) = state.elapsed_since_start(self.clock.sample().time.monotonic_ns) {
            state.measurements.finish(attempt, u128_to_u64_ms(at_ms), outcome, progress, usage);
        }
    }

    pub fn record_tool_output_measurement(&self, call_id: &str, execution_id: &str, row: Value) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        if row.is_null() {
            state.measurements.output(execution_id, row);
            return;
        }
        let mut row = row;
        row["outputId"] = json!(execution_id);
        row["executionId"] = json!(execution_id);
        row["callId"] = json!(call_id);
        row["ordinal"] = json!(0);
        state.measurements.output(execution_id, row);
    }
}

