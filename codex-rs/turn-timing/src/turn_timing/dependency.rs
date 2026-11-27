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
const RETENTION_EXCEEDED: &str = "dependency_graph_retention_exceeded";

#[derive(Default)]
struct RetainedNodes {
    nodes: BTreeMap<String, Node>,
    edge_count: usize,
}

impl RetainedNodes {
    fn replaced_edges(&self, id: &str) -> usize {
        self.nodes.get(id).map_or(0, |node| node.depends_on.len())
    }

    fn fits(&self, id: &str, edges: usize) -> bool {
        (self.nodes.len() < MAX_NODES || self.nodes.contains_key(id))
            && edges <= MAX_EDGES - (self.edge_count - self.replaced_edges(id))
    }

    fn insert(&mut self, row: Node) -> Option<()> {
        let edges = row.depends_on.len();
        if !self.fits(&row.id, edges) { return None; }
        self.edge_count = self.edge_count - self.replaced_edges(&row.id) + edges;
        self.nodes.insert(row.id.clone(), row);
        Some(())
    }
}

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
pub(super) fn request_end(timing: &TurnTiming, request: &codex_protocol::protocol::TurnTimingModelRequest) -> Option<u64> {
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
        || row["dispatchMs"].as_u64() != request.dispatch_ms
        || !row["physicalAttemptIds"].as_array().is_some_and(|ids| ids.iter()
            .map(serde_json::Value::as_str)
            .eq(request.physical_attempt_ids.iter().map(|id| Some(id.as_str()))))
    { return None; }
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
    /// The end is the tool owner's typed cessation, not a delivery/release.
    owner_end: bool,
}

struct Attempt<'a> {
    start: u64,
    end: u64,
    request: &'a str,
    owner_end: bool,
}

// Build per-attempt dispatch markers and exact work spans within a retried
// generation. A retry gap is a causal handoff, not model work. Timestamp
// rounding can put a whole attempt at one cut (including the final cut).
fn retry_segments(
    nodes: &mut RetainedNodes, id: &str, lane: &Lane,
    attempts: &[Attempt<'_>], mut previous: String,
) -> Option<String> {
    let mut cuts = lane.cuts.iter().copied().peekable();
    let mut dispatches = attempts.iter().enumerate().peekable();
    let mut current = &attempts[0];
    let mut segments = 0;
    let mut push_work = |nodes: &mut RetainedNodes, previous: String,
                         start, end, attempt: &Attempt<'_>| {
        let key = if segments == 0 { id.to_owned() } else { format!("{id}:segment-{segments}") };
        segments += 1;
        let mut work = node(key.clone(), Kind::Model, start, end, vec![previous]);
        work.sampling_request_id = Some(attempt.request.to_owned());
        if attempt.owner_end {
            work.evidence_ref = Some(format!("requestOwnerClosures:{}", attempt.request));
        }
        nodes.insert(work)?;
        Some(key)
    };
    while let Some(at) = cuts.next() {
        while let Some((index, attempt)) = dispatches.next_if(|(_, attempt)| attempt.start == at) {
            current = attempt;
            if index != 0 {
                let key = format!("{id}:attempt-{index}:dispatch");
                let mut dispatch = node(key.clone(), Kind::Boundary, at, at, vec![previous]);
                dispatch.sampling_request_id = Some(attempt.request.to_owned());
                nodes.insert(dispatch)?;
                previous = key;
            }
            if current.start == current.end {
                previous = push_work(nodes, previous, at, at, current)?;
            }
        }
        if let Some(&end) = cuts.peek() {
            let anchor = format!("{id}:at-{end}");
            if at < current.end {
                previous = push_work(nodes, previous, at, end, current)?;
            } else {
                // Keep :at-{end} an instantaneous release; gap time belongs to
                // a separate boundary span rather than the release itself.
                let gap = format!("{id}:retry-handoff-{at}");
                nodes.insert(node(gap.clone(), Kind::Boundary, at, end, vec![previous]))?;
                previous = gap;
            }
            nodes.insert(node(anchor.clone(), Kind::Boundary, end, end, vec![previous]))?;
            previous = anchor;
        }
    }
    Some(previous)
}

impl Lane {
    fn release(&mut self, id: &str, at: u64) -> Option<String> {
        if !(self.start..=self.end).contains(&at) { return None; }
        self.cuts.insert(at);
        Some(format!("{id}:at-{at}"))
    }
}

fn delivered_release_ms(timing: &TurnTiming, call: &codex_protocol::protocol::TurnTimingToolCall) -> Option<u64> {
    if call.source == TurnTimingToolCallSource::Direct { return call.delivered_at_ms; }
    timing.measurement_evidence.as_ref()?.get("nestedDependencies")?.get("rows")?.as_array()?
        .iter().find(|row| row["cellId"].as_str() == call.parent_cell_id.as_deref()
            && row["runtimeCallId"].as_str() == call.runtime_tool_call_id.as_deref())?
        .get("releasedAtMs")?.as_u64()
}

// A cancelled/interrupted tool whose dispatch owner exited without any
// delivery/release boundary has an exact typed cessation. It never replaces a
// recorded delivery or release, and it is not tool success or output delivery.
pub(super) fn tool_owner_end(timing: &TurnTiming, call: &codex_protocol::protocol::TurnTimingToolCall) -> Option<u64> {
    let outcome = call.outcome.as_deref()?;
    let typed_owner_cut = match call.source {
        TurnTimingToolCallSource::Direct => matches!(outcome, "cancelled" | "interrupted"),
        // Code Mode cancellation can also come from a deadline or the
        // transport. Only the explicit interrupted dispatch permits a cut.
        TurnTimingToolCallSource::CodeMode => outcome == "interrupted",
    };
    if !typed_owner_cut
        || call.delivered_at_ms.is_some() || call.output_model_visible_at_ms.is_some()
    {
        return None;
    }
    let packet = timing.measurement_evidence.as_ref()?.get("toolOwnerClosures")?;
    if packet["complete"] != true || packet["schemaVersion"] != 1 { return None; }
    let rows = packet["rows"].as_array()?;
    if packet["actualCount"].as_u64()? != rows.len() as u64 { return None; }
    let matches = rows.iter().filter(|row| row["executionId"].as_str() == Some(call.execution_id.0.as_str()))
        .collect::<Vec<_>>();
    if matches.len() != 1 { return None; }
    let row = matches[0];
    if row["basis"] != "tool_dispatch_owner_exit" || row["deliveredAtOwnerExit"] != false
        || row["acceptedMs"].as_u64() != call.accepted_at_ms
        || row["outcome"].as_str() != Some(outcome) { return None; }
    row["ownerEndMs"].as_u64()
}

#[derive(Default)]
struct LaneDependencies<'a> {
    emitted: BTreeSet<&'a str>,
    edge_count: usize,
    exceeded: bool,
}

impl<'a> LaneDependencies<'a> {
    fn retain_unique(&mut self, edges: &mut Vec<String>, edge: String) {
        // A raw observation can be duplicated or unbound. Count only distinct
        // inputs to each projected dispatch/end node. Native generation and
        // tool-execution IDs are disjoint and contain no colon suffixes, so
        // these edges survive projection; a handoff adds, never removes, an edge.
        if self.exceeded || edges.contains(&edge) { return; }
        if self.edge_count == MAX_EDGES {
            self.exceeded = true;
        } else {
            self.edge_count += 1;
            edges.push(edge);
        }
    }

    fn bind(
        &mut self, lanes: &mut BTreeMap<String, Lane>, reasons: &mut BTreeSet<&'static str>,
        dependent: &'a str, prerequisite: &str, relation: &str,
    ) {
        let release = match relation {
            "emitted_by" | "nested_emitted_by" => {
                let at = lanes.get(dependent).map(|lane| lane.start);
                let point = at.and_then(|at| lanes.get_mut(prerequisite)
                    .and_then(|lane| lane.release(prerequisite, at)));
                if point.is_none() { reasons.insert("missing_prefix_release_endpoint"); }
                if relation == "nested_emitted_by" {
                    let child_end = lanes.get(dependent).map(|lane| lane.end);
                    if let Some(parent) = lanes.get_mut(prerequisite) {
                        if child_end.is_some_and(|end| end <= parent.end) {
                            self.retain_unique(&mut parent.joins, format!("{dependent}:end"));
                        } else { reasons.insert("nested_work_outside_parent_envelope"); }
                    }
                }
                self.emitted.insert(dependent);
                point
            }
            "consumes_tool_result" | "nested_consumes_tool_result" => {
                // Owner cessation can close a work envelope, not release a
                // result to the continuation. The consumer stays incomplete.
                let input = lanes.get(prerequisite).filter(|lane| lane.kind == Kind::Tool && !lane.owner_end);
                input.zip(lanes.get(dependent)).and_then(|(input, consumer)| {
                    if input.end > consumer.start {
                        // A cell can resume on a local deadline before the
                        // native result is released by the core adapter.
                        reasons.insert("dependency_precedes_prerequisite_completion");
                        return None;
                    }
                    Some(format!("{prerequisite}:end"))
                })
            }
            "continues_generation" => {
                lanes.get(prerequisite).map(|_| format!("{prerequisite}:end"))
            }
            _ => return,
        };
        if let (Some(release), Some(lane)) = (release, lanes.get_mut(dependent)) {
            self.retain_unique(&mut lane.prerequisites, release);
        } else { reasons.insert("unbound_dependency_identity"); }
    }
}

fn project_lanes(
    retained: &mut RetainedNodes, lanes: &mut BTreeMap<String, Lane>,
    retry_attempts: &BTreeMap<String, Vec<Attempt<'_>>>, timing: &TurnTiming, duration: u64,
) -> Option<()> {
    // Keep both endpoints even when only a prefix of the lanes can be retained.
    retained.insert(node("start".into(), Kind::Boundary, 0, 0, Vec::new()))?;
    let ends = lanes.keys().map(|id| format!("{id}:end")).collect::<Vec<_>>();
    retained.insert(node("finish".into(), Kind::Boundary, duration, duration,
        if ends.is_empty() { vec!["start".into()] } else { ends }))?;
    for (id, lane) in lanes {
        let mut previous = format!("{id}:at-{}", lane.start);
        let mut dispatch = node(previous.clone(), Kind::Boundary,
            lane.start, lane.start, std::mem::take(&mut lane.prerequisites));
        dispatch.sampling_request_id = lane.request.clone();
        retained.insert(dispatch)?;
        if let Some(attempts) = retry_attempts.get(id) {
            previous = retry_segments(retained, id, lane, attempts, previous)?;
        } else {
            // Even a zero-duration request has one identified model segment.
            let zero_length = (lane.cuts.len() == 1).then_some(lane.start);
            let ends = zero_length.into_iter().chain(lane.cuts.iter().copied().skip(1));
            let mut start = lane.start;
            for (index, end) in ends.enumerate() {
                let segment = if index == 0 { id.clone() } else { format!("{id}:segment-{index}") };
                let mut work = node(segment.clone(), lane.kind, start, end, vec![previous.clone()]);
                work.sampling_request_id = lane.request.clone();
                if lane.kind == Kind::Model && timing.model_requests.iter().any(|request|
                    request.sampling_request_id == lane.request && request.completed_ms.is_none()) {
                    work.evidence_ref = lane.request.as_ref().map(|id| format!("requestOwnerClosures:{id}"));
                }
                if lane.kind == Kind::Tool && lane.owner_end {
                    work.evidence_ref = Some(format!("toolOwnerClosures:{id}"));
                }
                retained.insert(work)?;
                let boundary = format!("{id}:at-{end}");
                if boundary == previous {
                    // A zero-length lane must not introduce a boundary/self cycle.
                    previous = segment;
                } else {
                    retained.insert(node(boundary.clone(), Kind::Boundary,
                        end, end, vec![segment]))?;
                    previous = boundary;
                }
                start = end;
            }
        }
        let mut deps = vec![previous];
        deps.append(&mut lane.joins);
        deps.sort(); deps.dedup();
        let end = format!("{id}:end");
        retained.insert(node(end, Kind::Boundary, lane.end, lane.end, deps))?;
    }
    Some(())
}

fn insert_handoffs(retained: &mut RetainedNodes, reasons: &mut BTreeSet<&'static str>) -> Option<()> {
    // Handoffs cover only measured elapsed gaps after explicit prerequisites.
    // They never assert CPU time, resource ownership or an avoidable delay.
    for id in retained.nodes.keys().cloned().collect::<Vec<_>>() {
        if id == "start" { continue; }
        let row = &retained.nodes[&id];
        if row.depends_on.is_empty() { reasons.insert("unbound_task_span"); continue; }
        let previous = row.depends_on.iter().try_fold(0, |latest, dep| {
            Some(retained.nodes.get(dep)?.end_ns?.max(latest))
        });
        let Some(previous) = previous else {
            reasons.insert("missing_prerequisite_endpoint"); continue;
        };
        let start = row.start_ns.unwrap_or(0);
        if previous > start { reasons.insert("dependency_precedes_prerequisite_completion"); }
        if previous < start {
            let handoff = format!("{id}:handoff");
            // The bridge takes over the incoming dependencies: only the
            // edge from the original node to the bridge is new.
            if !retained.fits(&handoff, 1) { return None; }
            let incoming = std::mem::replace(
                &mut retained.nodes.get_mut(&id)?.depends_on, vec![handoff.clone()],
            );
            retained.edge_count = retained.edge_count - incoming.len() + 1;
            retained.insert(node(handoff, Kind::Boundary, previous, start, incoming))?;
        }
    }
    Some(())
}

fn check_connectivity(nodes: &BTreeMap<String, Node>, reasons: &mut BTreeSet<&'static str>) {
    let mut pending = nodes.iter().map(|(id, row)| (id.clone(), row.depends_on.len()))
        .collect::<BTreeMap<_, _>>();
    let mut successors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in nodes.values() { for dep in &row.depends_on {
        successors.entry(dep.clone()).or_default().push(row.id.clone());
    }}
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
    // Stream retries and transport fallbacks record one row per physical
    // attempt under their generation's index. Bind each generation to its rows
    // in record order instead of comparing rows with logical generations.
    let mut generations = BTreeMap::<u32, Vec<&codex_protocol::protocol::TurnTimingModelRequest>>::new();
    for request in &timing.model_requests {
        generations.entry(request.generation_index).or_default().push(request);
    }
    if timing.model_request_history_truncated != Some(false)
        || !generations.keys().copied().eq(0..timing.counters.logical_generation_count)
        || timing.model_requests.windows(2).any(|pair| pair[1].generation_index < pair[0].generation_index)
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
    let mut retry_attempts = BTreeMap::<String, Vec<Attempt<'_>>>::new();
    for (index, (generation, attempts)) in generations.iter().enumerate() {
        let id = format!("generation-{generation}");
        let spans = attempts.iter().map(|attempt| Some((
            ns(attempt.dispatch_ms)?, ns(request_end(timing, attempt))?
        ))).collect::<Option<Vec<_>>>();
        let Some(spans) = spans else {
            reasons.insert("missing_request_endpoint");
            continue;
        };
        if attempts.iter().any(|attempt| attempt.sampling_request_id.as_deref().is_none_or(str::is_empty)) {
            reasons.insert("missing_request_identity");
        }
        if spans.iter().any(|&(start, end)| start > end || end > timeline.duration_ns)
            || spans.windows(2).any(|pair| pair[0].1 > pair[1].0)
        {
            reasons.insert("ambiguous_or_invalid_request_span");
            continue;
        }
        let start = spans[0].0;
        let end = spans[spans.len() - 1].1;
        let cuts = spans.iter().flat_map(|&(start, end)| [start, end]).collect();
        let row = Lane {
            kind: Kind::Model, start, end, request: attempts[0].sampling_request_id.clone(),
            cuts,
            prerequisites: if index == 0 { vec!["start".into()] } else { Vec::new() },
            joins: Vec::new(), owner_end: false,
        };
        if attempts.len() > 1 {
            let physical = spans.into_iter().zip(attempts).map(|((start, end), attempt)| Some(Attempt {
                start, end, request: attempt.sampling_request_id.as_deref()?,
                owner_end: attempt.completed_ms.is_none(),
            })).collect::<Option<Vec<_>>>();
            let Some(physical) = physical else { continue; };
            retry_attempts.insert(id.clone(), physical);
        }
        if lanes.insert(id, row).is_some() {
            reasons.insert("ambiguous_or_invalid_request_span");
        }
    }
    for call in &timing.tool_calls {
        let release = delivered_release_ms(timing, call);
        let endpoint = release.or_else(|| tool_owner_end(timing, call));
        let (Some(start), Some(end)) = (ns(call.accepted_at_ms), ns(endpoint)) else {
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
            owner_end: release.is_none(),
        }).is_some() { reasons.insert("ambiguous_tool_identity"); }
    }

    let mut dependencies = LaneDependencies::default();
    if let Some(blocking) = &timeline.blocking {
        if blocking.dropped_dependencies != 0 || blocking.dropped_intervals != 0 {
            reasons.insert("dropped_dependency_evidence");
        }
        for edge in &blocking.dependencies {
            dependencies.bind(&mut lanes, &mut reasons, &edge.dependent, &edge.prerequisite, &edge.relation);
        }
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
            let child_lane = lanes.get(&call.execution_id.0);
            let mut parents = timing.tool_calls.iter().filter(|parent|
                parent.source == TurnTimingToolCallSource::Direct
                && Some(parent.call_id.as_str()) == call.parent_call_id.as_deref()
                && child_lane.zip(lanes.get(&parent.execution_id.0))
                    .is_some_and(|(child, outer)| outer.start <= child.start && child.end <= outer.end));
            let Some(parent) = parents.next().filter(|_| parents.next().is_none()) else {
                reasons.insert("nested_parent_execution_ambiguous_or_outside_envelope"); continue;
            };
            dependencies.bind(&mut lanes, &mut reasons, &call.execution_id.0,
                &parent.execution_id.0, "nested_emitted_by");
            for prior in predecessors {
                let mut matches = timing.tool_calls.iter().filter(|other|
                    other.parent_cell_id == call.parent_cell_id
                    && other.runtime_tool_call_id.as_deref() == prior.as_str());
                let Some(prior) = matches.next().filter(|_| matches.next().is_none()) else {
                    reasons.insert("nested_result_identity_not_retained"); continue;
                };
                dependencies.bind(&mut lanes, &mut reasons, &call.execution_id.0,
                    &prior.execution_id.0, "nested_consumes_tool_result");
            }
        }
        for call in &timing.tool_calls {
            if !dependencies.emitted.contains(call.execution_id.0.as_str()) {
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
    let mut retained = RetainedNodes::default();
    let lanes_fit = project_lanes(&mut retained, &mut lanes, &retry_attempts, timing, timeline.duration_ns).is_some();
    // Do not derive handoffs or connectivity from a graph missing causal inputs.
    if dependencies.exceeded || !lanes_fit || insert_handoffs(&mut retained, &mut reasons).is_none() {
        reasons.insert(RETENTION_EXCEEDED);
    } else {
        // Traversal may only draw conclusions from the untruncated graph.
        check_connectivity(&retained.nodes, &mut reasons);
    }
    let mut nodes = retained.nodes;

    // Whole-envelope repeat annotations cannot silently become first-fragment
    // attributions. Keep them in measurementEvidence; apply only unsplit lanes.
    if let Some(rows) = timing.measurement_evidence.as_ref()
        .and_then(|evidence| evidence["dependencyNodes"].as_array()) {
        for annotation in rows {
            let Some(id) = annotation["id"].as_str() else { continue; };
            if !retry_attempts.contains_key(id) && lanes.get(id).is_some_and(|lane| lane.cuts.len() <= 2)
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
        nodes: nodes.into_values().collect(),
        unavailable_reasons: reasons.into_iter().map(str::to_owned).collect(),
    })
}
