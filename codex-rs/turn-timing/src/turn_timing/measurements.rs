//! Bounded evidence attached to the existing terminal timing snapshot.
//! Facts are recorded at transport/result ownership boundaries. Semantic work
//! labels are explicit assertions, never inferred from a failed call or edit.
use super::*;
use serde_json::{Value, json};

const LIMIT: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Capture {
    pub(super) observation_scope: String,
    pub(super) app_server: super::app_server::Capture,
    pub(super) performance: super::performance::Capture,
    pub(super) operations: super::operations::Capture,
    enabled: bool,
    actual_attempts: u64,
    actual_logical_requests: u64,
    actual_outputs: u64,
    attempts: BTreeMap<String, Value>,
    outputs: BTreeMap<String, Value>,
    annotations: BTreeMap<String, Value>,
    dependency_annotations: BTreeMap<String, Value>,
    nested_dependencies: BTreeMap<String, Value>,
    causal_agents: Vec<Value>,
    dropped_causal_records: u64,
    recovery_evidence: Option<Value>,
    dropped_attempts: u64,
    dropped_outputs: u64,
    actual_nested_results: u64,
    nested_results: BTreeMap<String, Value>,
    dropped_nested_results: u64,
    actual_artifact_recoveries: u64,
    artifact_recoveries: BTreeMap<String, Value>,
    dropped_artifact_recoveries: u64,
    actual_context_retirements: u64,
    context_retirements: BTreeMap<String, Value>,
    dropped_context_retirements: u64,
    actual_input_prefixes: u64,
    input_prefixes: BTreeMap<String, Value>,
    dropped_input_prefixes: u64,
    output_exposures: BTreeMap<String, Value>,
    output_exposure_edges: usize,
    dropped_output_exposures: u64,
    final_candidates: BTreeMap<String, Value>,
    pub(super) request_owner_ends: BTreeMap<String, Value>,
    pub(super) request_owner_end_count: u64,
}

fn digest(value: &impl serde::Serialize) -> Option<String> {
    serde_json::to_vec(value).ok().map(|bytes| hex::encode(Sha256::digest(bytes)))
}

fn text(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|value| !value.trim().is_empty() && value.len() <= 256)
}

impl Capture {
    pub(super) fn input_prefix(&mut self, attempt: &str, evidence: Value) {
        self.enabled = true;
        if self.input_prefixes.contains_key(attempt) { return; }
        self.actual_input_prefixes = self.actual_input_prefixes.saturating_add(1);
        if self.input_prefixes.len() >= LIMIT {
            self.dropped_input_prefixes = self.dropped_input_prefixes.saturating_add(1);
            return;
        }
        self.input_prefixes.insert(attempt.to_owned(), evidence);
    }

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
            "dependencyNodes":self.dependency_annotations.values().collect::<Vec<_>>(),
            "nestedDependencies":{"complete":self.dropped_causal_records == 0,
                "rows":self.nested_dependencies.values().collect::<Vec<_>>()},
            "causalAgents":{"complete":self.dropped_causal_records == 0,
                "rows":self.causal_agents},
            "requests":{"complete":annotations.len() as u64 == self.actual_logical_requests,
                        "actualCount":self.actual_logical_requests, "rows":annotations},
            "outputs":{"complete":self.dropped_outputs == 0, "actualCount":self.actual_outputs,
                       "rows":self.outputs.values().collect::<Vec<_>>()},
            "nestedResults":{"complete":self.dropped_nested_results == 0 && self.actual_nested_results != u64::MAX,
                "actualCount":self.actual_nested_results,
                "rows":self.nested_results.values().collect::<Vec<_>>(),
                "scope":"settled_native_code_mode_results_before_script_and_outer_display_projection"},
            "requestOwnerClosures":{"schemaVersion":1,
                "complete":self.request_owner_ends.len() as u64 == self.request_owner_end_count,
                "actualCount":self.request_owner_end_count,
                "rows":self.request_owner_ends.values().collect::<Vec<_>>()},
            "artifactRecoveries":{"complete":self.dropped_artifact_recoveries == 0 && self.actual_artifact_recoveries != u64::MAX,
                "actualCount":self.actual_artifact_recoveries,
                "rows":self.artifact_recoveries.values().collect::<Vec<_>>(),
                "scope":"read_tool_output_attempts; audit_only excluded; not exhaustive recovery"},
            "contextRetirements":{"complete":self.dropped_context_retirements == 0 && self.actual_context_retirements != u64::MAX,
                "actualCount":self.actual_context_retirements,
                "rows":self.context_retirements.values().collect::<Vec<_>>(),
                "scope":"runtime pressure/proactive selection; explicit phase checkpoints and compaction are separate owners"},
            "requestInputPrefixes":{"complete":self.dropped_input_prefixes == 0 && self.actual_input_prefixes != u64::MAX,
                "actualCount":self.actual_input_prefixes,
                "rows":self.input_prefixes.values().collect::<Vec<_>>(),
                "scope":"optional actual transport input comparison; websocket deltas are not full logical prompts"},
            "requestOutputExposures":{"complete":self.dropped_output_exposures == 0,
                "rows":self.output_exposures.values().collect::<Vec<_>>(),
                "droppedRequests":self.dropped_output_exposures,
                "scope":"optional effective logical input of dispatched attempts; missing rows are unknown"},
            "capture":{"scope":"tool_runtime_results_before_caller_display", "droppedOutputs":self.dropped_outputs,
                "semanticLabels":"explicit_referenced_assertions_not_automatic_waste_verdicts"}
        })), Some(recovery))
    }
}

/// Logical sampling-future ownership, not HTTP headers or provider completion.
#[must_use]
pub struct ModelRequestOwnerGuard {
    timing: Arc<TurnTimingState>,
    generation: Option<u32>,
}

impl Drop for ModelRequestOwnerGuard {
    fn drop(&mut self) {
        self.timing.record_sampling_future_end(self.generation);
    }
}

impl TurnTimingState {
    pub fn begin_model_request_owner(self: &Arc<Self>) -> ModelRequestOwnerGuard {
        let generation = self.state().current_generation_index;
        ModelRequestOwnerGuard { timing: Arc::clone(self), generation }
    }

    fn record_sampling_future_end(&self, generation: Option<u32>) {
        let sample = self.clock.sample();
        let mut state = self.state();
        let end_ms = state
            .elapsed_since_start(sample.time.monotonic_ns)
            .map(u128_to_u64_ms);
        state.measurements.request_owner_end_count = state.measurements.request_owner_end_count.saturating_add(1);
        let closure = state.model_requests.last()
            .filter(|request| Some(request.generation_index) == generation)
            .and_then(|request| Some((request.sampling_request_id.clone()?, json!({
                "samplingRequestId":request.sampling_request_id,
                "physicalAttemptIds":request.physical_attempt_ids,
                "dispatchMs":request.dispatch_ns.map(u128_to_u64_ms), "ownerEndMs":end_ms,
                "providerCompleted":request.completed_ns.is_some(),
                "basis":"sampling_future_scope_exit"}))));
        if let Some((id, closure)) = closure {
            if state.measurements.request_owner_ends.len() < LIMIT {
                state.measurements.request_owner_ends.insert(id.clone(), closure.clone());
            }
            drop(state);
            self.observe_runtime("model_owner_finished", &id, closure);
        } else {
            drop(state);
            self.observe_runtime("model_owner_unidentified", "", json!({"generation":generation}));
        }
    }

    /// Runtime-owned async provenance, independent of model arguments or text.
    pub fn record_nested_dependencies(&self, cell: &str, call: &str, parent: Option<&str>,
                                      predecessors: Option<Vec<String>>) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        let capture = &mut state.measurements;
        capture.enabled = true;
        let key = format!("{cell}:{call}");
        if capture.nested_dependencies.len() >= LIMIT || capture.nested_dependencies.contains_key(&key) {
            capture.dropped_causal_records = capture.dropped_causal_records.saturating_add(1);
            return;
        }
        let predecessors = predecessors.filter(|rows| rows.len() <= 128
            && rows.iter().all(|id| !id.is_empty() && id.len() <= 256));
        capture.nested_dependencies.insert(key, json!({"cellId":cell, "runtimeCallId":call,
            "parentCallId":parent, "predecessors":predecessors}));
    }

    pub fn record_nested_result_release(&self, cell: &str, call: &str) {
        let at = self.monotonic_offset_ms();
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        if let Some(row) = state.measurements.nested_dependencies.get_mut(&format!("{cell}:{call}")) {
            row["releasedAtMs"] = json!(at);
        }
    }

    /// The spawn/wait owner supplies the actual child identity and execution.
    /// `at_ms` is captured before launch for forks, at observed settlement for joins.
    pub fn record_causal_agent(&self, kind: &str, child: &str, execution: &str, at_ms: u64) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        let capture = &mut state.measurements;
        capture.enabled = true;
        if capture.causal_agents.len() >= LIMIT || !matches!(kind, "spawn" | "join") {
            capture.dropped_causal_records = capture.dropped_causal_records.saturating_add(1);
            return;
        }
        capture.causal_agents.push(json!({"kind":kind, "childSession":child,
            "executionId":execution, "atMs":at_ms}));
    }

    pub fn observe_runtime(&self, kind: &str, operation: &str, data: Value) {
        if !codex_protocol::timing::observations::enabled() { return; }
        let scope = self.state().measurements.observation_scope.clone();
        if !scope.is_empty() {
            let _ = codex_protocol::timing::observations::record(kind, "", &scope, operation, data);
        }
    }

    pub fn bind_runtime_observations(&self, thread: &str, turn: &str) {
        if !codex_protocol::timing::observations::enabled() { return; }
        let state = self.state();
        let scope = state.measurements.observation_scope.clone();
        let origin = state.started_sample.map(|sample| sample.time.monotonic_ns);
        drop(state);
        let _ = codex_protocol::timing::observations::record("scope_bound", thread, turn, &scope,
            json!({"origin_ns":origin}));
    }

    /// Bind the output at its originating completion owner, before later
    /// completion hooks/steering can accept or discard it. Never pick the last
    /// request at terminal publication as a substitute for this association.
    pub fn record_final_candidate(&self, completion: &str, item: &str) {
        let Some(identity) = self.latest_model_dispatch_identity() else { return; };
        let mut state = self.state();
        if state.measurements.final_candidates.len() < LIMIT {
            state.measurements.final_candidates.insert(completion.into(), json!({
                "completionAttemptId":completion, "itemId":item,
                "samplingRequestId":identity.sampling_request_id,
                "physicalAttemptId":identity.physical_attempt_id,
                "basis":"native_originating_completion_owner_v1"}));
        }
    }

    pub fn accepted_final_request(&self, message: Option<&codex_protocol::protocol::AcceptedFinalMessage>) -> Value {
        let Some(message) = message else { return json!({"status":"no_accepted_message"}); };
        let state = self.state();
        match state.measurements.final_candidates.get(&message.completion_attempt_id)
            .filter(|row| row["itemId"].as_str() == Some(message.item_id.as_str())) {
            Some(row) => { let mut row = row.clone(); row["status"] = json!("accepted"); row },
            None => json!({"status":"unavailable", "reason":"originating_request_not_recorded"}),
        }
    }

    pub fn record_model_output_exposures(&self, request: &str, attempt: &str, outputs: Vec<Value>, complete: bool) {
        let mut state = self.state();
        let capture = &mut state.measurements;
        capture.enabled = true;
        if capture.output_exposures.contains_key(attempt) { return; }
        if capture.output_exposures.len() >= LIMIT || capture.output_exposure_edges + outputs.len() > 16_384 {
            capture.dropped_output_exposures = capture.dropped_output_exposures.saturating_add(1);
            return;
        }
        capture.output_exposure_edges += outputs.len();
        capture.output_exposures.insert(attempt.to_owned(), json!({
            "request_id":request, "attempt_id":attempt, "outputs":outputs, "complete":complete,
            "basis":"effective_logical_input_for_dispatched_attempt_v1",
        }));
        state.refresh_completed_request_diagnostics();
    }

    pub fn record_context_retirement(&self, event: &str, started: std::time::Instant, facts: Value) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        let start_ns = codex_protocol::timing::process_monotonic_ns_at(started)
            .filter(|_| self.clock.domain_id() == Some(codex_protocol::timing::process_clock_domain_id()))
            .and_then(|at| at.checked_sub(state.started_sample?.time.monotonic_ns));
        let end_ms = state.started_sample.and_then(|start| self.clock.sample().time.monotonic_ns
            .checked_sub(start.time.monotonic_ns)).map(u128_to_u64_ms);
        let preceding = start_ns.and_then(|at| state.model_requests.iter().rev()
            .find(|request| request.dispatch_ns.is_some_and(|dispatch| dispatch <= at)))
            .and_then(|request| request.sampling_request_id.clone());
        let capture = &mut state.measurements;
        capture.enabled = true;
        capture.actual_context_retirements = capture.actual_context_retirements.saturating_add(1);
        if capture.context_retirements.len() >= LIMIT || capture.context_retirements.contains_key(event) {
            capture.dropped_context_retirements = capture.dropped_context_retirements.saturating_add(1);
            return;
        }
        capture.context_retirements.insert(event.to_owned(), json!({
            "event_id":event, "after_request_id":preceding,
            "start_ms":start_ns.map(u128_to_u64_ms), "end_ms":end_ms,
            "clock_id":self.clock.domain_id(), "facts":facts,
        }));
    }

    /// Reuse the byte count from the actual nested-result serializer. This is
    /// not canonical artifact size, caller-visible text, or provider tokens.
    pub fn record_native_nested_result(&self, call: &str, cell: &str, runtime_call: &str,
                                       parent: Option<&str>, bytes: usize) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        let capture = &mut state.measurements;
        capture.enabled = true;
        capture.actual_nested_results = capture.actual_nested_results.saturating_add(1);
        if bytes == 0 || capture.nested_results.len() >= LIMIT || capture.nested_results.contains_key(call) {
            capture.dropped_nested_results = capture.dropped_nested_results.saturating_add(1);
            return;
        }
        capture.nested_results.insert(call.to_owned(), json!({
            "callId":call, "parentCallId":parent, "parentCellId":cell,
            "runtimeToolCallId":runtime_call, "rawToolResultBytes":bytes,
            "byteAccounting":"native_nested_result_json_v1",
            "evidenceRef":format!("native:nested-result:{call}")
        }));
    }

    /// The owner supplies an observed Instant, not a duration subtracted from a
    /// later timestamp. A foreign/pre-turn clock observation stays unavailable.
    pub fn record_artifact_recovery(&self, call: &str, started: std::time::Instant, facts: Value) {
        let mut state = self.state();
        if state.completed_snapshot.is_some() { return; }
        let start_ns = codex_protocol::timing::process_monotonic_ns_at(started)
            .filter(|_| self.clock.domain_id() == Some(codex_protocol::timing::process_clock_domain_id()))
            .and_then(|at| at.checked_sub(state.started_sample?.time.monotonic_ns));
        let end_ms = state.started_sample.and_then(|start| self.clock.sample().time.monotonic_ns
            .checked_sub(start.time.monotonic_ns)).map(u128_to_u64_ms);
        let preceding = start_ns.and_then(|at| state.model_requests.iter().rev()
            .find(|request| request.dispatch_ns.is_some_and(|dispatch| dispatch <= at)))
            .and_then(|request| request.sampling_request_id.clone());
        let capture = &mut state.measurements;
        capture.enabled = true;
        capture.actual_artifact_recoveries = capture.actual_artifact_recoveries.saturating_add(1);
        if capture.artifact_recoveries.len() >= LIMIT || capture.artifact_recoveries.contains_key(call) {
            capture.dropped_artifact_recoveries = capture.dropped_artifact_recoveries.saturating_add(1);
            return;
        }
        capture.artifact_recoveries.insert(call.to_owned(), json!({
            "event_id":format!("artifact:{call}"), "call_id":call,
            "after_request_id":preceding,
            "mechanism":"artifact_retrieval", "start_ms":start_ns.map(u128_to_u64_ms), "end_ms":end_ms,
            "clock_id":self.clock.domain_id(), "facts":facts,
            "evidence_ref":format!("native:artifact-recovery:{call}")
        }));
        drop(state);
        if codex_protocol::timing::observations::enabled() {
            self.observe_runtime("recovery_artifact_attempt", call, json!({
                "started_ms":start_ns.map(u128_to_u64_ms), "ended_ms":end_ms,
                "basis":"read_tool_output_owner_includes_failed_and_abandoned_attempts"}));
        }
    }

    pub fn record_model_context_sha256(&self, hash: String) {
        let mut state = self.state();
        let generation = state.current_generation_index;
        let request = state.model_requests.last_mut()
            .filter(|request| Some(request.generation_index) == generation);
        let identity = if let Some(request) = request {
            request.logical_context_sha256 = Some(hash.clone());
            request.sampling_request_id.clone()
        } else { None };
        drop(state);
        if codex_protocol::timing::observations::enabled() {
            self.observe_runtime("model_logical_identity", identity.as_deref().unwrap_or_default(),
                json!({"logical_context_sha256":hash, "basis":"full_logical_prompt_v1_before_transport_delta"}));
        }
    }

    /// Validate the optional annotations before the plan owner performs effects.
    pub fn validate_measurement_annotations(&self, packet: &Value) -> Result<Value, String> {
        if serde_json::to_vec(packet).map_err(|error| error.to_string())?.len() > 32 * 1024 {
            return Err("measurement annotations exceed 32 KiB".into());
        }
        let object = packet.as_object().ok_or("measurements must be an object")?;
        if object.keys().any(|key| !matches!(key.as_str(), "requests" | "recovery" | "dependencyNodes")) {
            return Err("measurements accepts only requests, recovery and dependencyNodes".into());
        }
        let state = self.state();
        let mut resolved = packet.clone();
        if let Some(rows) = packet.get("dependencyNodes") {
            let rows = rows.as_array().filter(|rows| rows.len() <= 128)
                .ok_or("dependencyNodes must contain at most 128 annotations")?;
            let known = state.model_requests.iter().map(|request| format!("generation-{}", request.generation_index))
                .chain(state.tool_calls.iter().map(|call| call.execution_id.0.clone())).collect::<BTreeSet<_>>();
            let mut seen = BTreeSet::new();
            for row in rows {
                let fields = row.as_object().ok_or("dependency node annotation must be an object")?;
                if fields.keys().any(|key| !matches!(key.as_str(), "id" | "repeatOf" | "workIdentity"
                    | "inputIdentity" | "necessity" | "necessityEvidence" | "evidenceRef")) {
                    return Err("unsupported dependency node annotation field".into());
                }
                let id = row["id"].as_str().filter(|id| known.contains(*id)).ok_or("dependency node is not retained in this turn")?;
                if !seen.insert(id) || !text(row.get("evidenceRef")) {
                    return Err("dependency labels require unique native IDs and evidenceRef".into());
                }
                for field in ["workIdentity", "inputIdentity"] {
                    if let Some(value) = row.get(field) {
                        let hash = value.as_str().unwrap_or_default();
                        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
                            return Err("dependency work/input identities must be lowercase SHA256".into());
                        }
                    }
                }
                if let Some(repeat) = row.get("repeatOf") {
                    if !repeat.as_str().is_some_and(|prior| prior != id && known.contains(prior))
                        || !row["workIdentity"].is_string() || !row["inputIdentity"].is_string() {
                        return Err("repeatOf requires a distinct retained native node and work/input identities".into());
                    }
                }
                if let Some(necessity) = row.get("necessity") {
                    if !matches!(necessity.as_str(), Some("required" | "unnecessary" | "unknown")) {
                        return Err("necessity must be required, unnecessary or unknown".into());
                    }
                    if necessity != "unknown" && !row["necessityEvidence"].as_array().is_some_and(|refs|
                        !refs.is_empty() && refs.len() <= 16 && refs.iter().all(|value| text(Some(value)))) {
                        return Err("necessity assessments require 1..16 evidence references".into());
                    }
                }
            }
            let added = seen.iter().filter(|id| !state.measurements.dependency_annotations.contains_key(**id)).count();
            if state.measurements.dependency_annotations.len() + added > LIMIT {
                return Err("dependency annotation retention is full".into());
            }
        }
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
        if let Some(rows) = packet.get("dependencyNodes").and_then(Value::as_array) {
            for row in rows {
                if let Some(id) = row["id"].as_str() {
                    state.measurements.dependency_annotations.insert(id.to_owned(), row.clone());
                }
            }
        }
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
        state.measurements.app_server.result(execution_id, self.clock.sample().time.monotonic_ns);
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

