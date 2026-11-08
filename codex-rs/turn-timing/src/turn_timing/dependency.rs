//! Segmented owner-recorded dependencies. A streamed prefix releases a tool,
//! not the rest of its model request. This projection never changes scheduling.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use codex_protocol::protocol::{TurnTiming, TurnTimingToolCallSource};
use codex_protocol::timing::{
    TurnTimingDependencyGraph, TurnTimingDependencyNode as Node,
    TurnTimingDependencyNodeKind as Kind,
};

const MAX_NODES: usize = 8192;
const MAX_EDGES: usize = 32768;

fn node(id: String, kind: Kind, start: u64, end: u64, deps: Vec<String>) -> Node {
    Node {
        id, kind, start_ns: Some(start), end_ns: Some(end), depends_on: deps,
        sampling_request_id: None, repeat_of: None, work_identity: None,
        input_identity: None, necessity: None, necessity_evidence: Vec::new(),
        evidence_ref: None,
    }
}

fn ns(ms: Option<u64>) -> Option<u64> {
    ms.and_then(|ms| ms.checked_mul(1_000_000))
}

// A stopped local sampling owner has an exact endpoint even when the provider
// never completed. Keep that evidence reference; do not populate completedMs.
fn request_end(timing: &TurnTiming, request: &codex_protocol::protocol::TurnTimingModelRequest) -> Option<u64> {
    if request.completed_ms.is_some() { return request.completed_ms; }
    let packet = timing.measurement_evidence.as_ref()?.get("requestOwnerClosures")?;
    if packet["complete"] != true || packet["schemaVersion"] != 1 { return None; }
    let rows = packet["rows"].as_array()?;
    if packet["actualCount"].as_u64()? != rows.len() as u64 { return None; }
    let matches = rows.iter().filter(|row| row["samplingRequestId"].as_str() == request.sampling_request_id.as_deref())
        .collect::<Vec<_>>();
    if matches.len() != 1 { return None; }
    let row = matches[0];
    if row["basis"] != "sampling_future_scope_exit" || row["providerCompleted"] != false
        || row["dispatchMs"].as_u64() != request.dispatch_ms { return None; }
    row["ownerEndMs"].as_u64()
}

struct Lane {
    kind: Kind,
    start: u64,
    end: u64,
    request: Option<String>,
    cuts: BTreeSet<u64>,
    prerequisites: Vec<String>,
    joins: Vec<String>,
}

impl Lane {
    fn release(&mut self, id: &str, at: u64) -> Option<String> {
        if !(self.start..=self.end).contains(&at) { return None; }
        self.cuts.insert(at);
        Some(format!("{id}:at-{at}"))
    }
}

fn release_ms(timing: &TurnTiming, call: &codex_protocol::protocol::TurnTimingToolCall) -> Option<u64> {
    if call.source == TurnTimingToolCallSource::Direct { return call.delivered_at_ms; }
    timing.measurement_evidence.as_ref()?.get("nestedDependencies")?.get("rows")?.as_array()?
        .iter().find(|row| row["cellId"].as_str() == call.parent_cell_id.as_deref()
            && row["runtimeCallId"].as_str() == call.runtime_tool_call_id.as_deref())?
        .get("releasedAtMs")?.as_u64()
}

pub(super) fn snapshot(timing: &TurnTiming) -> Option<TurnTimingDependencyGraph> {
    let timeline = timing.activity_timeline.as_ref()?;
    if timeline.tail_unknown { return None; }
    let mut reasons = BTreeSet::new();
    if !timing.profile_valid || !timeline.coverage_complete || timeline.version != 1
        || timeline.duration_ns != timing.inclusive_duration_ns
        || timeline.clock_domain_id.is_none() || timeline.dropped_intervals != 0
        || timeline.open_process_count != 0
    {
        reasons.insert("incomplete_clock_or_activity_coverage");
    }
    if timing.model_request_history_truncated != Some(false)
        || timing.model_requests.len() != timing.counters.logical_generation_count as usize
        || timing.counters.saturation_count != 0
    {
        reasons.insert("incomplete_request_inventory");
    }
    if !timing.tool_closure.measurement_available || !timing.tool_closure.complete
        || timing.tool_call_timing_overflow != 0
        || timing.tool_calls.len() != timing.tool_closure.accepted_count as usize
    {
        reasons.insert("incomplete_tool_inventory");
    }
    let mut lanes = BTreeMap::new();
    for (index, request) in timing.model_requests.iter().enumerate() {
        let id = format!("generation-{}", request.generation_index);
        let (Some(start), Some(end)) = (ns(request.dispatch_ms), ns(request_end(timing, request))) else {
            reasons.insert("missing_request_endpoint");
            continue;
        };
        if request.sampling_request_id.is_none() { reasons.insert("missing_request_identity"); }
        let row = Lane {
            kind: Kind::Model, start, end, request: request.sampling_request_id.clone(),
            cuts: BTreeSet::from([start, end]),
            prerequisites: if index == 0 { vec!["start".into()] } else { Vec::new() },
            joins: Vec::new(),
        };
        if start > end || end > timeline.duration_ns || lanes.insert(id, row).is_some() {
            reasons.insert("ambiguous_or_invalid_request_span");
        }
    }
    for call in &timing.tool_calls {
        let (Some(start), Some(end)) = (ns(call.accepted_at_ms), ns(release_ms(timing, call))) else {
            reasons.insert("missing_tool_endpoint");
            continue;
        };
        let id = call.execution_id.0.clone();
        if id.is_empty() || start > end || end > timeline.duration_ns {
            reasons.insert("ambiguous_or_invalid_tool_span");
            continue;
        }
        if lanes.insert(id, Lane {
            kind: Kind::Tool, start, end, request: None,
            cuts: BTreeSet::from([start, end]), prerequisites: Vec::new(), joins: Vec::new(),
        }).is_some() { reasons.insert("ambiguous_tool_identity"); }
    }

    if let Some(blocking) = &timeline.blocking {
        if blocking.dropped_dependencies != 0 || blocking.dropped_intervals != 0 {
            reasons.insert("dropped_dependency_evidence");
        }
        let mut emitted = BTreeSet::new();
        let mut dependencies = blocking.dependencies.clone();
        let nested = timing.measurement_evidence.as_ref().map(|evidence| &evidence["nestedDependencies"]);
        if nested.is_some_and(|packet| packet["complete"] != true) {
            reasons.insert("nested_dependency_capture_dropped");
        }
        for call in timing.tool_calls.iter().filter(|call| call.source == TurnTimingToolCallSource::CodeMode) {
            let row = nested.and_then(|packet| packet["rows"].as_array()).and_then(|rows| rows.iter().find(|row|
                row["cellId"].as_str() == call.parent_cell_id.as_deref()
                && row["runtimeCallId"].as_str() == call.runtime_tool_call_id.as_deref()));
            let predecessors = row.and_then(|row| row["predecessors"].as_array());
            let Some(predecessors) = predecessors else {
                reasons.insert("nested_async_dependencies_not_recorded"); continue;
            };
            let parents = timing.tool_calls.iter().filter(|parent|
                parent.source == TurnTimingToolCallSource::Direct
                && Some(parent.call_id.as_str()) == call.parent_call_id.as_deref()
                && parent.accepted_at_ms.zip(call.accepted_at_ms).is_some_and(|(a, b)| a <= b)
                && release_ms(timing, call).zip(release_ms(timing, parent)).is_some_and(|(a, b)| a <= b))
                .collect::<Vec<_>>();
            if parents.len() != 1 { reasons.insert("nested_parent_execution_ambiguous_or_outside_envelope"); continue; }
            dependencies.push(codex_protocol::timing::TurnTimingDependencyEdge {
                dependent:call.execution_id.0.clone(), prerequisite:parents[0].execution_id.0.clone(),
                relation:"nested_emitted_by".into(),
            });
            for prior in predecessors {
                let matches = timing.tool_calls.iter().filter(|other|
                    other.parent_cell_id == call.parent_cell_id
                    && other.runtime_tool_call_id.as_deref() == prior.as_str()).collect::<Vec<_>>();
                if matches.len() != 1 { reasons.insert("nested_result_identity_not_retained"); continue; }
                dependencies.push(codex_protocol::timing::TurnTimingDependencyEdge {
                    dependent:call.execution_id.0.clone(), prerequisite:matches[0].execution_id.0.clone(),
                    relation:"nested_consumes_tool_result".into(),
                });
            }
        }
        for edge in &dependencies {
            let dependent = &edge.dependent;
            let prerequisite = &edge.prerequisite;
            let release = match edge.relation.as_str() {
                "emitted_by" | "nested_emitted_by" => {
                    let at = lanes.get(dependent).map(|lane| lane.start);
                    let point = at.and_then(|at| lanes.get_mut(prerequisite)
                        .and_then(|lane| lane.release(prerequisite, at)));
                    if point.is_none() { reasons.insert("missing_prefix_release_endpoint"); }
                    if edge.relation == "nested_emitted_by" {
                        let child_end = lanes.get(dependent).map(|lane| lane.end);
                        if let Some(parent) = lanes.get_mut(prerequisite) {
                            if child_end.is_some_and(|end| end <= parent.end) {
                                parent.joins.push(format!("{dependent}:end"));
                            } else { reasons.insert("nested_work_outside_parent_envelope"); }
                        }
                    }
                    emitted.insert(dependent.clone());
                    point
                }
                "consumes_tool_result" | "continues_generation" | "nested_consumes_tool_result" => {
                    lanes.get(prerequisite).map(|_| format!("{prerequisite}:end"))
                }
                _ => continue,
            };
            if let (Some(release), Some(lane)) = (release, lanes.get_mut(dependent)) {
                if !lane.prerequisites.contains(&release) { lane.prerequisites.push(release); }
            } else { reasons.insert("unbound_dependency_identity"); }
        }
        for call in &timing.tool_calls {
            if !emitted.contains(&call.execution_id.0) {
                reasons.insert(if call.source == TurnTimingToolCallSource::Direct {
                    "model_prefix_release_not_recorded"
                } else { "nested_fork_join_not_recorded" });
            }
        }
    } else { reasons.insert("causal_relations_not_recorded"); }

    if let Some(agents) = timing.measurement_evidence.as_ref().map(|evidence| &evidence["causalAgents"]) {
        if agents["complete"] != true { reasons.insert("agent_causal_capture_dropped"); }
        for link in agents["rows"].as_array().into_iter().flatten().filter(|row| row["kind"] == "spawn") {
            let point = link["executionId"].as_str().zip(link["atMs"].as_u64())
                .and_then(|(id, at)| lanes.get_mut(id).and_then(|lane|
                    at.checked_mul(1_000_000).and_then(|at| lane.release(id, at))));
            if point.is_none() { reasons.insert("agent_spawn_execution_or_endpoint_not_retained"); }
        }
    }
    let mut nodes = BTreeMap::new();
    nodes.insert("start".to_owned(), node("start".into(), Kind::Boundary, 0, 0, Vec::new()));
    for (id, lane) in &lanes {
        let mut previous = format!("{id}:at-{}", lane.start);
        let mut dispatch = node(previous.clone(), Kind::Boundary,
            lane.start, lane.start, lane.prerequisites.clone());
        dispatch.sampling_request_id = lane.request.clone();
        nodes.insert(previous.clone(), dispatch);
        let cuts = lane.cuts.iter().copied().collect::<Vec<_>>();
        // Even a zero-duration request has one identified model segment.
        let spans = if cuts.len() == 1 { vec![(cuts[0], cuts[0])] }
            else { cuts.windows(2).map(|pair| (pair[0], pair[1])).collect() };
        for (index, (start, end)) in spans.into_iter().enumerate() {
            let segment = if index == 0 { id.clone() } else { format!("{id}:segment-{index}") };
            let mut work = node(segment.clone(), lane.kind, start, end, vec![previous.clone()]);
            work.sampling_request_id = lane.request.clone();
            if lane.kind == Kind::Model && timing.model_requests.iter().any(|request|
                request.sampling_request_id == lane.request && request.completed_ms.is_none()) {
                work.evidence_ref = lane.request.as_ref().map(|id| format!("requestOwnerClosures:{id}"));
            }
            nodes.insert(segment.clone(), work);
            let boundary = format!("{id}:at-{end}");
            if boundary == previous {
                // A zero-length lane must not introduce a boundary/self cycle.
                previous = segment;
            } else {
                nodes.insert(boundary.clone(), node(boundary.clone(), Kind::Boundary,
                    end, end, vec![segment]));
                previous = boundary;
            }
        }
        let mut deps = vec![previous];
        deps.extend(lane.joins.iter().cloned());
        deps.sort(); deps.dedup();
        let end = format!("{id}:end");
        nodes.insert(end.clone(), node(end, Kind::Boundary, lane.end, lane.end, deps));
    }
    let ends = lanes.keys().map(|id| format!("{id}:end")).collect::<Vec<_>>();
    nodes.insert("finish".into(), node("finish".into(), Kind::Boundary,
        timeline.duration_ns, timeline.duration_ns,
        if ends.is_empty() { vec!["start".into()] } else { ends }));

    // Handoffs cover only measured elapsed gaps after explicit prerequisites.
    // They never assert CPU time, resource ownership or an avoidable delay.
    for id in nodes.keys().cloned().collect::<Vec<_>>() {
        if id == "start" { continue; }
        let row = &nodes[&id];
        if row.depends_on.is_empty() { reasons.insert("unbound_task_span"); continue; }
        let ends = row.depends_on.iter().map(|dep| nodes.get(dep).and_then(|node| node.end_ns))
            .collect::<Option<Vec<_>>>();
        let Some(previous) = ends.and_then(|ends| ends.into_iter().max()) else {
            reasons.insert("missing_prerequisite_endpoint"); continue;
        };
        let start = row.start_ns.unwrap_or(0);
        if previous > start { reasons.insert("dependency_precedes_prerequisite_completion"); }
        if previous < start {
            let handoff = format!("{id}:handoff");
            let bridge = node(handoff.clone(), Kind::Boundary, previous, start, row.depends_on.clone());
            nodes.get_mut(&id)?.depends_on = vec![handoff.clone()];
            nodes.insert(handoff, bridge);
        }
    }
    let mut pending = nodes.iter().map(|(id, row)| (id.clone(), row.depends_on.len()))
        .collect::<BTreeMap<_, _>>();
    let mut successors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in nodes.values() { for dep in &row.depends_on {
        successors.entry(dep.clone()).or_default().push(row.id.clone());
    }}
    let edge_count: usize = pending.values().sum();
    let mut ready = VecDeque::from(["start".to_owned()]);
    let mut visited = 0;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for child in successors.get(&id).into_iter().flatten() {
            if let Some(count) = pending.get_mut(child) {
                *count = count.saturating_sub(1);
                if *count == 0 { ready.push_back(child.clone()); }
            }
        }
    }
    if visited != nodes.len() { reasons.insert("cyclic_or_disconnected_task_graph"); }
    if nodes.len() > MAX_NODES || edge_count > MAX_EDGES { reasons.insert("dependency_graph_retention_exceeded"); }

    // Whole-envelope repeat annotations cannot silently become first-fragment
    // attributions. Keep them in measurementEvidence; apply only unsplit lanes.
    if let Some(rows) = timing.measurement_evidence.as_ref()
        .and_then(|evidence| evidence["dependencyNodes"].as_array()) {
        for annotation in rows {
            let Some(id) = annotation["id"].as_str() else { continue; };
            if lanes.get(id).is_some_and(|lane| lane.cuts.len() <= 2)
                && let Some(row) = nodes.get_mut(id) {
                row.repeat_of = annotation["repeatOf"].as_str().map(str::to_owned);
                row.work_identity = annotation["workIdentity"].as_str().map(str::to_owned);
                row.input_identity = annotation["inputIdentity"].as_str().map(str::to_owned);
                row.necessity = annotation["necessity"].as_str().map(str::to_owned);
                row.evidence_ref = annotation["evidenceRef"].as_str().map(str::to_owned);
                row.necessity_evidence = annotation["necessityEvidence"].as_array().into_iter().flatten()
                    .filter_map(|reference| reference.as_str().map(str::to_owned)).collect();
            }
        }
    }
    Some(TurnTimingDependencyGraph {
        schema_version: 2, complete: reasons.is_empty(), includes_descendants: false,
        clock_domain_id: timeline.clock_domain_id.clone(), started_at_ns: timeline.started_at_ns,
        duration_ns: timeline.duration_ns, start_node: "start".into(), finish_node: "finish".into(),
        nodes: nodes.into_values().take(MAX_NODES).collect(),
        unavailable_reasons: reasons.into_iter().map(str::to_owned).collect(),
    })
}
